//! Loads an F2LLM-v2-0.6B provider out of an unmodified Hugging Face snapshot.
//!
//! Shared by the encoder contract and the floor calibration, which both need the same two
//! environment variables and the same reshaping of the snapshot, and would otherwise disagree about
//! what "the model" means the first time one of them was edited:
//!
//! ```text
//! SCTX_PROBE_F2LLM_MODEL=~/.cache/huggingface/hub/models--codefuse-ai--F2LLM-v2-0.6B/snapshots/<sha> \
//! SCTX_PROBE_EMBEDDING_RUNTIME=~/.shared-context/embedding/runtime/libonnxruntime.dylib
//! ```
//!
//! The snapshot is used as it comes off the Hub, `onnx/` subdirectory and all, and the flat
//! directory the provider expects is built out of symlinks. That is not a convenience: copying the
//! external-weights blob to reshape a directory would cost 2.4 GB of disk to say nothing extra, and
//! a symlinked `model.onnx_data` beside a symlinked `model.onnx` is resolved by ONNX Runtime exactly
//! as the real installation layout is. The session is loaded once per test binary and shared,
//! because several tests holding several sessions would mean several copies of the weights resident
//! at once for no additional coverage.

// Not every binary that loads the model also fingerprints its directory.
#![allow(dead_code)]

use std::{
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
};

use sctx_search::embedding::onnx::{OnnxEmbeddingProvider, initialize_runtime};
use tempfile::TempDir;

/// The loaded session and the directory whose symlinks it holds open.
struct Loaded {
    provider: Arc<OnnxEmbeddingProvider>,
    /// Dropped when the process exits, never before: ONNX Runtime keeps `model.onnx_data` open for
    /// the life of the session, and that path runs through these symlinks.
    _flat: TempDir,
}

static LOADED: OnceLock<Loaded> = OnceLock::new();

fn probe_paths() -> Option<(PathBuf, PathBuf)> {
    let model = std::env::var_os("SCTX_PROBE_F2LLM_MODEL")?;
    let runtime = std::env::var_os("SCTX_PROBE_EMBEDDING_RUNTIME")?;
    Some((PathBuf::from(model), PathBuf::from(runtime)))
}

/// Builds the flat model directory the provider loads from, out of a Hugging Face snapshot.
///
/// `model.onnx_data` is optional because an export small enough to fit one file has none. The other
/// three are the provider's entire contract with a model directory, and their absence means the
/// environment variable names something that is not an F2LLM snapshot.
fn flatten_snapshot(snapshot: &Path, into: &Path) {
    std::fs::create_dir_all(into).expect("create the flattened model directory");
    for (name, required) in [
        ("model.onnx", true),
        ("model.onnx_data", false),
        ("tokenizer.json", true),
        ("config.json", true),
    ] {
        let source = [snapshot.join("onnx").join(name), snapshot.join(name)]
            .into_iter()
            .find(|candidate| candidate.exists());
        match source {
            Some(source) => {
                // Resolved through the Hub's own blob symlinks first, so the link this test writes
                // points at a file rather than at another link into a cache that may be pruned.
                let source = std::fs::canonicalize(&source).expect("resolve the snapshot file");
                std::os::unix::fs::symlink(source, into.join(name))
                    .expect("link the snapshot file into the flat directory");
            }
            None => assert!(
                !required,
                "{} holds no {name}; SCTX_PROBE_F2LLM_MODEL must name an F2LLM snapshot",
                snapshot.display()
            ),
        }
    }
}

/// The one F2LLM provider this test binary uses, loading it on first call.
///
/// # Panics
///
/// Panics when the two environment variables are unset, which is how an `--ignored` run that forgot
/// them fails loudly instead of measuring nothing.
pub fn provider() -> &'static Arc<OnnxEmbeddingProvider> {
    &loaded().provider
}

fn loaded() -> &'static Loaded {
    LOADED.get_or_init(|| {
        let Some((model, runtime)) = probe_paths() else {
            panic!(
                "set SCTX_PROBE_F2LLM_MODEL and SCTX_PROBE_EMBEDDING_RUNTIME; see the module \
                     docs"
            );
        };
        initialize_runtime(&runtime).expect("load ONNX Runtime from SCTX_PROBE_EMBEDDING_RUNTIME");
        let flat = tempfile::tempdir().expect("a temporary directory for the flattened model");
        let directory = flat.path().join("model");
        flatten_snapshot(&model, &directory);
        Loaded {
            provider: Arc::new(
                OnnxEmbeddingProvider::load_and_probe(&directory).expect("load the F2LLM export"),
            ),
            _flat: flat,
        }
    })
}
