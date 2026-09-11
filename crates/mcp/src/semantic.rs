//! Serve-time loading and backfill of the optional embedding recall channel (ADR-0004).
//!
//! The whole point of this file is that retrieval never waits for it. A bge-m3 session takes 9--12
//! seconds to load and the corpus backfill takes longer still, so both happen on a background
//! thread that publishes into a [`SemanticChannelHandle`] the request path already holds. Until
//! that publish lands, every automatic Pack reports `embedding_unavailable` and answers from the
//! lexical channels exactly as it would on an installation that never configured a model.

use std::{path::Path, sync::Arc, time::Duration};

use sctx_index::{ProjectionIndex, SEARCH_RANKING_VERSION};
use sctx_local_state::{RetrievalSettings, UserConfigStore};
use sctx_search::{
    EmbeddingSemanticChannel, EncodeSampleRecorder, SearchEngine, SemanticCacheKey,
    SemanticChannelHandle, SemanticVectorCache, load_onnx_provider, model_fingerprint,
};

/// Reads `[retrieval]` without letting a broken config file break `serve`.
///
/// A configuration this process cannot read is not a reason to refuse to serve; it is a reason to
/// have no embedding channel, which `sctx doctor` reports in full.
pub(crate) fn retrieval_settings(root: &Path) -> RetrievalSettings {
    UserConfigStore::open_existing(root)
        .and_then(|config| config.retrieval_settings())
        .unwrap_or_default()
}

/// Starts the background loader when, and only when, `[retrieval]` names both halves.
///
/// Returns `None` for every installation that configured nothing, which is what keeps the
/// unconfigured path byte-identical: no handle means [`SearchEngine`] is never given a channel,
/// so no channel feature and no omission can be produced.
pub(crate) fn spawn_semantic_loader(root: &Path) -> Option<SemanticChannelHandle> {
    let settings = retrieval_settings(root);
    let model_path = settings.embedding_model_path.clone()?;
    let runtime_path = settings.embedding_runtime_path.clone()?;
    let budget = settings.encode_budget();
    let handle = SemanticChannelHandle::new();
    let background = handle.clone();
    let root = root.to_path_buf();
    if std::thread::Builder::new()
        .name("sctx-embedding-loader".to_owned())
        .spawn(move || {
            if let Err(error) =
                load_and_backfill(&root, &model_path, &runtime_path, budget, &background)
            {
                // stderr is the MCP server's advisory channel; stdout carries the protocol. A
                // failed model load degrades retrieval, it never fails a request.
                eprintln!("sctx: embedding channel unavailable: {error}");
            }
        })
        .is_err()
    {
        return Some(handle);
    }
    Some(handle)
}

/// Loads the model, publishes whatever is already cached, then fills in what is missing.
///
/// The two publishes are deliberate. The first makes a restarted server useful within milliseconds
/// of the model load on a corpus it has already embedded once; the second widens the snapshot to
/// include revisions accepted since. Without the first, every restart would spend the whole
/// backfill answering `embedding_unavailable` over vectors it already had on disk.
fn load_and_backfill(
    root: &Path,
    model_path: &Path,
    runtime_path: &Path,
    budget: Option<Duration>,
    handle: &SemanticChannelHandle,
) -> sctx_search::Result<()> {
    let provider = load_onnx_provider(model_path, runtime_path)?;
    let key = SemanticCacheKey::new(model_fingerprint(model_path)?, SEARCH_RANKING_VERSION);
    let cache = Arc::new(SemanticVectorCache::open_at_root(root)?);
    // Vectors from a superseded model or corpus generation can never be compared against the
    // current ones, so they are disk cost with no possible reader.
    let _pruned = cache.prune_superseded(&key)?;

    // Read the corpus before the first publish, not after, so both prunes land before any snapshot
    // is loaded. This is one local SQLite query against a projection that is already synchronized,
    // against a model load that has just cost 9--12 seconds: it does not measurably delay the
    // publish, and doing it here means the first snapshot a restarted server answers from is
    // already free of vectors for revisions the projection has since retired.
    let engine = SearchEngine::new(open_index(root));
    let embeddable = engine.embeddable_revisions()?;
    let _retired = cache.retain_revisions(
        &key,
        &embeddable
            .iter()
            .map(|(revision_id, _)| *revision_id)
            .collect(),
    )?;

    // Both publishes carry the same budget and the same recorder, and share one query vector cache
    // through `from_cache`, so the second one inherits the first one's warmth instead of resetting
    // a session back to a cold encode.
    let build = |provider: Arc<dyn sctx_search::EmbeddingProvider>| {
        let channel = EmbeddingSemanticChannel::from_cache(provider, &cache, &key)?
            .with_encode_recorder(Arc::clone(&cache) as Arc<dyn EncodeSampleRecorder>);
        Ok::<_, sctx_search::Error>(match budget {
            Some(budget) => channel.with_budget(budget),
            None => channel,
        })
    };

    handle.publish(Arc::new(build(Arc::clone(&provider))?));

    let cached = cache.cached_revisions(&key)?;
    let mut wrote = false;
    for (revision_id, text) in embeddable {
        if cached.contains(&revision_id) {
            continue;
        }
        // One failed encode is one missing candidate, not a failed backfill: the remaining corpus
        // is still worth having.
        //
        // `encode_bulk`, not `encode`, and that is the whole point of the publish-then-backfill
        // order being safe. The channel above is already answering, so from here to the end of the
        // loop every query shares one ONNX session with the most expensive encodes in the system.
        // Asking for the corpus at background priority means a query waits for the corpus text
        // already in flight and never for the next one.
        let Ok(vector) = provider.encode_bulk(&text) else {
            continue;
        };
        if cache.store(&key, revision_id, &vector).is_ok() {
            wrote = true;
        }
    }
    if wrote {
        handle.publish(Arc::new(build(provider)?));
    }
    Ok(())
}

fn open_index(root: &Path) -> ProjectionIndex {
    ProjectionIndex::new(root.join("repository"), root.join("state"))
}

/// What one synchronous cache warm-up did.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Serialize)]
pub struct SemanticWarmReport {
    /// Whether `[retrieval]` named both a model and a runtime.
    pub configured: bool,
    /// Revisions embedded by this call.
    pub embedded: usize,
    /// Revisions the cache holds under the current key afterwards.
    pub cached: usize,
}

/// Fills the vector cache synchronously, for callers that are not a long-lived `serve`.
///
/// The background loader in [`spawn_semantic_loader`] is right for an MCP process that stays up
/// for a whole session, and useless for one that answers a single request and exits before the
/// 9--12 second load finishes -- which is what a CLI invocation and a scripted harness both are.
/// `sctx doctor --fix` is the supported way to pay that cost on purpose, once, rather than never.
///
/// # Errors
///
/// Returns a typed error when the model cannot be loaded or the cache cannot be written.
pub fn warm_semantic_cache_at_root(root: &Path) -> sctx_search::Result<SemanticWarmReport> {
    let settings = retrieval_settings(root);
    let (Some(model_path), Some(runtime_path)) = (
        settings.embedding_model_path.clone(),
        settings.embedding_runtime_path.clone(),
    ) else {
        return Ok(SemanticWarmReport::default());
    };
    let provider = load_onnx_provider(&model_path, &runtime_path)?;
    let key = SemanticCacheKey::new(model_fingerprint(&model_path)?, SEARCH_RANKING_VERSION);
    let cache = SemanticVectorCache::open_at_root(root)?;
    let _pruned = cache.prune_superseded(&key)?;
    let embeddable = SearchEngine::new(open_index(root)).embeddable_revisions()?;
    // Same reclaim as the background loader, for the same reason: a vector whose revision left the
    // accepted set is filed under an unchanged key and nothing else will ever drop it.
    let _retired = cache.retain_revisions(
        &key,
        &embeddable
            .iter()
            .map(|(revision_id, _)| *revision_id)
            .collect(),
    )?;
    let cached = cache.cached_revisions(&key)?;
    let mut embedded = 0;
    for (revision_id, text) in embeddable {
        if cached.contains(&revision_id) {
            continue;
        }
        // Corpus priority here too, for the same reason it is corpus priority in the background
        // loader: nothing is waiting on these vectors. This process serves no queries, so the
        // priority never costs anything -- it only keeps the two backfill paths saying the same
        // thing about what corpus work is.
        let Ok(vector) = provider.encode_bulk(&text) else {
            continue;
        };
        if cache.store(&key, revision_id, &vector).is_ok() {
            embedded += 1;
        }
    }
    Ok(SemanticWarmReport {
        configured: true,
        embedded,
        cached: cache.cached_revisions(&key)?.len(),
    })
}
