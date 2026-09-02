//! The ONNX Runtime [`EmbeddingProvider`] the shipped installation actually uses.
//!
//! Two things about this file are deliberate and load-bearing.
//!
//! * **Nothing is downloaded, bundled, or linked at build time.** `ort` is compiled with
//!   `load-dynamic`, so the ONNX Runtime shared library is opened at run time from the path
//!   `[retrieval] embedding_runtime_path` names. A build of this workspace on a machine with no
//!   network and no ONNX Runtime installed still succeeds, and a binary that never sees a
//!   `[retrieval]` table never opens a library.
//! * **The graph is inspected, not assumed.** Different bge-m3 exports name their pooled output
//!   differently and some emit only `last_hidden_state`. Guessing wrong yields plausible-looking
//!   vectors that quietly rank nothing correctly, so the session's declared inputs and outputs are
//!   read once at load and the pooling strategy is chosen from what is actually there.

use std::{
    path::Path,
    sync::{Mutex, OnceLock},
};

use ort::{session::Session, value::TensorRef};
use sctx_domain::{Error, ErrorKind, Result};
use tokenizers::Tokenizer;

use super::{EmbeddingProvider, SEMANTIC_MAX_TOKENS, normalize};

/// The ONNX Runtime library is process-global: `ort::init_from` may only take effect once, before
/// any session exists. A second `[retrieval]` path in the same process would be ignored rather
/// than honoured, so the first one wins and the fact is recorded here.
static RUNTIME: OnceLock<std::result::Result<(), String>> = OnceLock::new();

/// Loads the ONNX Runtime shared library once per process.
///
/// # Errors
///
/// Returns [`ErrorKind::InvalidInput`] when the library cannot be opened, and reports the same
/// failure to every later caller rather than retrying a load that cannot succeed twice.
pub fn initialize_runtime(runtime_library: &Path) -> Result<()> {
    let outcome = RUNTIME.get_or_init(|| {
        let builder = ort::init_from(runtime_library).map_err(|error| {
            format!(
                "load ONNX Runtime from {}: {error}",
                runtime_library.display()
            )
        })?;
        if builder.commit() {
            Ok(())
        } else {
            Err("ONNX Runtime environment was already committed".to_owned())
        }
    });
    match outcome {
        Ok(()) => Ok(()),
        Err(detail) => Err(Error::new(ErrorKind::InvalidInput, detail.clone())),
    }
}

/// How the token sequence is reduced to one vector.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Pooling {
    /// The graph already emits one vector per sequence.
    Pooled,
    /// Take the `[CLS]` token's hidden state, which is what bge-m3 trains its dense retrieval
    /// head on. Mean pooling over the sequence is a different embedding space and scores
    /// differently against a corpus built with CLS pooling.
    ClassToken,
}

/// A loaded bge-m3-shaped ONNX encoder.
pub struct OnnxEmbeddingProvider {
    // `Session::run` needs `&mut`, and the provider is shared across the backfill thread and the
    // query path. Encodes are short and the corpus backfill is the only sustained user, so a plain
    // mutex costs less than the session pool it would take to remove it.
    session: Mutex<Session>,
    tokenizer: Tokenizer,
    input_names: Vec<String>,
    output_name: String,
    pooling: Pooling,
    dimensions: usize,
}

impl OnnxEmbeddingProvider {
    /// Loads `model.onnx` and `tokenizer.json` out of one model directory.
    ///
    /// The ONNX Runtime library must already be loaded through [`initialize_runtime`].
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::Io`] when the directory is missing either file and
    /// [`ErrorKind::InvalidInput`] when the model cannot be loaded or has no output this provider
    /// knows how to pool.
    pub fn load(model_directory: &Path) -> Result<Self> {
        let model_path = model_directory.join("model.onnx");
        if !model_path.is_file() {
            return Err(Error::new(
                ErrorKind::Io,
                format!("embedding model {} is missing", model_path.display()),
            ));
        }
        let tokenizer_path = model_directory.join("tokenizer.json");
        let tokenizer = Tokenizer::from_file(&tokenizer_path).map_err(|error| {
            Error::new(
                ErrorKind::Io,
                format!("load tokenizer {}: {error}", tokenizer_path.display()),
            )
        })?;
        let session = Session::builder()
            .and_then(|mut builder| builder.commit_from_file(&model_path))
            .map_err(|error| {
                Error::new(
                    ErrorKind::InvalidInput,
                    format!("load embedding model {}: {error}", model_path.display()),
                )
            })?;

        let input_names = session
            .inputs()
            .iter()
            .map(|outlet| outlet.name().to_owned())
            .collect::<Vec<_>>();
        if !input_names.iter().any(|name| name == "input_ids") {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                format!("embedding model declares no input_ids input, only {input_names:?}"),
            ));
        }
        let output_names = session
            .outputs()
            .iter()
            .map(|outlet| outlet.name().to_owned())
            .collect::<Vec<_>>();
        let (output_name, pooling) = select_output(&output_names)?;

        Ok(Self {
            session: Mutex::new(session),
            tokenizer,
            input_names,
            output_name,
            pooling,
            // bge-m3 dense is 1024-wide, but the width is confirmed from the first encode rather
            // than asserted here: a dimension mismatch has to be a load-time or first-encode
            // failure, never a silently wrong vector.
            dimensions: 0,
        })
    }

    /// Loads the model and confirms its output width with one probe encode.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::load`], plus any failure of the probe encode.
    pub fn load_and_probe(model_directory: &Path) -> Result<Self> {
        let mut provider = Self::load(model_directory)?;
        let probe = provider.encode_inner("shared context")?;
        if probe.is_empty() {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "embedding model produced an empty vector",
            ));
        }
        provider.dimensions = probe.len();
        Ok(provider)
    }

    fn encode_inner(&self, text: &str) -> Result<Vec<f32>> {
        let mut encoding = self.tokenizer.encode(text, true).map_err(|error| {
            Error::new(ErrorKind::InvalidInput, format!("tokenize query: {error}"))
        })?;
        encoding.truncate(
            SEMANTIC_MAX_TOKENS,
            0,
            tokenizers::TruncationDirection::Right,
        );
        let length = encoding.get_ids().len().max(1);
        let ids = encoding
            .get_ids()
            .iter()
            .map(|id| i64::from(*id))
            .collect::<Vec<_>>();
        let ids = if ids.is_empty() { vec![0_i64] } else { ids };
        let mask = vec![1_i64; length];
        let type_ids = vec![0_i64; length];

        let shape = [1_usize, length];
        let mut inputs = Vec::new();
        for name in &self.input_names {
            let values: &[i64] = match name.as_str() {
                "input_ids" => &ids,
                "attention_mask" => &mask,
                "token_type_ids" => &type_ids,
                // An input this provider cannot supply means the graph is not the encoder shape
                // ADR-0004 specified; refusing is the only honest answer.
                other => {
                    return Err(Error::new(
                        ErrorKind::InvalidInput,
                        format!("embedding model requires unsupported input {other}"),
                    ));
                }
            };
            let tensor = TensorRef::from_array_view((shape, values)).map_err(|error| {
                Error::new(
                    ErrorKind::InvalidInput,
                    format!("build {name} tensor: {error}"),
                )
            })?;
            inputs.push((name.clone(), tensor));
        }

        let mut session = self.session.lock().map_err(|_| {
            Error::new(
                ErrorKind::InvariantViolation,
                "embedding session lock was poisoned",
            )
        })?;
        let outputs = session.run(inputs).map_err(|error| {
            Error::new(ErrorKind::External, format!("run embedding model: {error}"))
        })?;
        let output = outputs.get(self.output_name.as_str()).ok_or_else(|| {
            Error::new(
                ErrorKind::External,
                format!("embedding model returned no {} output", self.output_name),
            )
        })?;
        let (shape, values) = output.try_extract_tensor::<f32>().map_err(|error| {
            Error::new(
                ErrorKind::External,
                format!("read embedding model output: {error}"),
            )
        })?;
        let mut vector = pool(self.pooling, shape.as_ref(), values)?;
        if !normalize(&mut vector) {
            return Err(Error::new(
                ErrorKind::External,
                "embedding model produced a zero-length vector",
            ));
        }
        Ok(vector)
    }
}

impl EmbeddingProvider for OnnxEmbeddingProvider {
    fn dimensions(&self) -> usize {
        self.dimensions
    }

    fn encode(&self, text: &str) -> Result<Vec<f32>> {
        let vector = self.encode_inner(text)?;
        if self.dimensions != 0 && vector.len() != self.dimensions {
            return Err(Error::new(
                ErrorKind::External,
                format!(
                    "embedding model returned {} dimensions, expected {}",
                    vector.len(),
                    self.dimensions
                ),
            ));
        }
        Ok(vector)
    }
}

/// Picks the output to pool, preferring one the graph already reduced.
fn select_output(output_names: &[String]) -> Result<(String, Pooling)> {
    for candidate in ["dense_vecs", "sentence_embedding", "text_embeds"] {
        if let Some(name) = output_names.iter().find(|name| name.as_str() == candidate) {
            return Ok((name.clone(), Pooling::Pooled));
        }
    }
    for candidate in ["last_hidden_state", "token_embeddings", "output"] {
        if let Some(name) = output_names.iter().find(|name| name.as_str() == candidate) {
            return Ok((name.clone(), Pooling::ClassToken));
        }
    }
    // Falling back to output 0 with CLS pooling would be a guess, and a wrong guess here is
    // indistinguishable from a working channel that ranks badly.
    Err(Error::new(
        ErrorKind::InvalidInput,
        format!("embedding model has no recognised dense output, only {output_names:?}"),
    ))
}

/// Reduces one batch-of-one model output to a single vector.
fn pool(pooling: Pooling, shape: &[i64], values: &[f32]) -> Result<Vec<f32>> {
    let width = shape
        .last()
        .copied()
        .and_then(|width| usize::try_from(width).ok())
        .filter(|width| *width > 0)
        .ok_or_else(|| {
            Error::new(
                ErrorKind::External,
                format!("embedding output has unusable shape {shape:?}"),
            )
        })?;
    if values.len() < width {
        return Err(Error::new(
            ErrorKind::External,
            "embedding output is shorter than one vector",
        ));
    }
    // Batch is always one and the class token is index zero, so both strategies read the same
    // leading slice; they differ in what the rest of the buffer means, not in where the answer is.
    match pooling {
        Pooling::Pooled | Pooling::ClassToken => Ok(values[..width].to_vec()),
    }
}
