//! Serve-time loading and backfill of the optional embedding recall channel (ADR-0004).
//!
//! The whole point of this file is that retrieval never waits for it. A bge-m3 session takes 9--12
//! seconds to load and the corpus backfill takes longer still, so both happen on a background
//! thread that publishes into a [`SemanticChannelHandle`] the request path already holds. Until
//! that publish lands, every automatic Pack reports `embedding_unavailable` and answers from the
//! lexical channels exactly as it would on an installation that never configured a model.

use std::{path::Path, sync::Arc};

use sctx_index::{ProjectionIndex, SEARCH_RANKING_VERSION};
use sctx_local_state::{RetrievalSettings, UserConfigStore};
use sctx_search::{
    EmbeddingSemanticChannel, SearchEngine, SemanticCacheKey, SemanticChannelHandle,
    SemanticVectorCache, load_onnx_provider, model_fingerprint,
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
    let handle = SemanticChannelHandle::new();
    let background = handle.clone();
    let root = root.to_path_buf();
    if std::thread::Builder::new()
        .name("sctx-embedding-loader".to_owned())
        .spawn(move || {
            if let Err(error) = load_and_backfill(&root, &model_path, &runtime_path, &background) {
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
    handle: &SemanticChannelHandle,
) -> sctx_search::Result<()> {
    let provider = load_onnx_provider(model_path, runtime_path)?;
    let key = SemanticCacheKey::new(model_fingerprint(model_path)?, SEARCH_RANKING_VERSION);
    let cache = SemanticVectorCache::open_at_root(root)?;
    // Vectors from a superseded model or ranking version can never be compared against the current
    // ones, so they are disk cost with no possible reader.
    let _pruned = cache.prune_superseded(&key)?;

    handle.publish(Arc::new(EmbeddingSemanticChannel::from_cache(
        Arc::clone(&provider),
        &cache,
        &key,
    )?));

    let engine = SearchEngine::new(open_index(root));
    let embeddable = engine.embeddable_revisions()?;
    let cached = cache.cached_revisions(&key)?;
    let mut wrote = false;
    for (revision_id, text) in embeddable {
        if cached.contains(&revision_id) {
            continue;
        }
        // One failed encode is one missing candidate, not a failed backfill: the remaining corpus
        // is still worth having.
        let Ok(vector) = provider.encode(&text) else {
            continue;
        };
        if cache.store(&key, revision_id, &vector).is_ok() {
            wrote = true;
        }
    }
    if wrote {
        handle.publish(Arc::new(EmbeddingSemanticChannel::from_cache(
            provider, &cache, &key,
        )?));
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
    let cached = cache.cached_revisions(&key)?;
    let mut embedded = 0;
    for (revision_id, text) in SearchEngine::new(open_index(root)).embeddable_revisions()? {
        if cached.contains(&revision_id) {
            continue;
        }
        let Ok(vector) = provider.encode(&text) else {
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
