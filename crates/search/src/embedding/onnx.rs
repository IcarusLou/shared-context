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
//! * **The family is declared, not sniffed.** Two encoder families are supported and they disagree
//!   about every step between text and vector: which token carries the sentence, whether a query is
//!   prefixed with an instruction, whether the graph wants `position_ids`, and what cosine floor
//!   means anything. None of those disagreements is detectable from the vector -- each combination
//!   produces a unit-length vector of the right width -- so the family is read from the model
//!   directory's `config.json` and a directory that has one this provider cannot recognise fails to
//!   load. The failure is the feature: a bad install has to surface where `sctx doctor` looks, not
//!   as a channel that answers every query with a confidently wrong ranking.
//! * **The one session is rationed, not merely locked.** `Session::run` needs `&mut`, so every
//!   encode in the process is serialised through one mutex, and the corpus backfill is a loop of
//!   the most expensive encodes there are. A plain mutex lets that loop barge: it re-acquires the
//!   moment it releases, and a query that arrives during the backfill window waits behind a run of
//!   corpus texts rather than one, which is how a real query burned its whole encode budget and
//!   degraded to `embedding_unavailable`. [`SessionGate`] admits by priority and aborts the corpus
//!   run in flight when a query arrives, which is what makes one session enough for both.

use std::{
    borrow::Cow,
    path::Path,
    sync::{Condvar, Mutex, MutexGuard, OnceLock, PoisonError},
    time::{Duration, Instant},
};

use ort::{
    session::{NoSelectedOutputs, RunOptions, Session},
    value::TensorRef,
};
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

/// How long the corpus backfill holds off after an interactive encode finishes.
///
/// Preemption makes a query fast; this makes the backfill stop wasting work to keep it fast. A
/// Pack issues its Working Intent query and its task-association query back to back, so a corpus
/// text started in the gap between them would be aborted seconds later with nothing to show for
/// it. Waiting out the gap costs the backfill this much idle time per burst, and saves it a whole
/// discarded encode.
const INTERACTIVE_QUIET_PERIOD: Duration = Duration::from_millis(150);

/// Ceiling on how long one corpus text may be deferred for politeness.
///
/// Without it, queries arriving faster than [`INTERACTIVE_QUIET_PERIOD`] would stall the backfill
/// indefinitely, and a backfill that never finishes is a *permanent* recall regression -- strictly
/// worse than the transient latency it was deferring for. Past this bound the corpus text goes
/// ahead as soon as no query is actually waiting.
const MAX_BULK_DEFERRAL: Duration = Duration::from_millis(500);

/// How long the backfill may go without completing a corpus text before it stops being
/// preemptible.
///
/// Preemption is what keeps a query fast, and taken alone it is also a way for a busy server to
/// leave its corpus permanently unembedded: every attempt aborted, no vector ever written. Past
/// this bound one corpus text runs to completion whatever arrives, so the backfill always finishes
/// -- at the cost of at most one query per bound waiting a full corpus encode. At thirty seconds
/// that is roughly one query in a hundred on a server under constant load, and none at all on one
/// that ever goes quiet.
const BULK_PROGRESS_DEADLINE: Duration = Duration::from_secs(30);

/// What an encode is for, which is what decides who waits for whom.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Priority {
    /// A user is blocked on this encode and a budget is running against it.
    Interactive,
    /// Corpus work with no reader waiting. It may always be made to wait, and may be abandoned
    /// part-way and redone later.
    Bulk,
}

impl Priority {
    /// What text encoded at this priority is.
    ///
    /// The two facts coincide by construction rather than by coincidence: the only thing a user
    /// ever blocks on is a query, and the only thing the backfill ever encodes is corpus text. Both
    /// entry points on the trait already carry the distinction, so an asymmetric model needs no new
    /// plumbing to be told which side of itself to be -- and cannot be told wrongly without also
    /// scheduling the encode wrongly, which is loud.
    const fn role(self) -> TextRole {
        match self {
            Self::Interactive => TextRole::Query,
            Self::Bulk => TextRole::Document,
        }
    }
}

/// Admission control in front of the one ONNX session, and the reason one session is enough.
///
/// Two rules, and the second is the one that matters:
///
/// * **A corpus encode never starts while a query is waiting or running.** That alone bounds a
///   query's wait at the corpus text already in flight -- but on this machine a full-length corpus
///   encode is 1062 ms at p95 against a 2000 ms budget, so "one corpus text" is already half of
///   the budget and a real query still misses it. Yielding between texts cannot fix a single text
///   that costs what the whole budget costs.
/// * **A corpus encode in flight is aborted when a query arrives.** ONNX Runtime can be told to
///   halt a run from another thread ([`RunOptions::terminate`]), so the query waits for the
///   backfill to notice rather than to finish. The abandoned text is re-encoded on the next
///   attempt; corpus work is idempotent and nobody is waiting for it.
///
/// The alternative that also removes the wait is a second session, which would double the ~1.2 GB
/// of resident weights for a window that closes on its own. This costs no memory at all: one
/// `OrtRunOptions` handle and six words of bookkeeping.
///
/// [`RunOptions::terminate`]: ort::session::RunOptions::terminate
struct SessionGate {
    state: Mutex<GateState>,
    changed: Condvar,
    /// The handle corpus runs execute under, and the lever that aborts them. Queries deliberately
    /// run under no options at all, so terminating this can never touch one.
    bulk_run: RunOptions<NoSelectedOutputs>,
}

/// Who holds the session, who is waiting for it, and what each side has been able to do lately.
#[derive(Debug, Default)]
struct GateState {
    held_by: Option<Priority>,
    /// Whether the corpus encode in flight, if any, may be aborted. False for the one run per
    /// [`BULK_PROGRESS_DEADLINE`] that is allowed to finish no matter what.
    held_preemptible: bool,
    interactive_waiting: usize,
    /// Corpus encodes inside [`OnnxEmbeddingProvider::encode_bulk`], waiting ones included. This
    /// is what makes an encode sample say whether a backfill was competing with it.
    bulk_pending: usize,
    /// Set when a query aborted the corpus run in flight, so the backfill can tell "a query needed
    /// the session" from "this text cannot be encoded" and retry rather than drop a revision.
    bulk_preempted: bool,
    last_interactive_end: Option<Instant>,
    last_bulk_end: Option<Instant>,
    /// When the backfill last finished a corpus text, or first asked to start one. What
    /// [`BULK_PROGRESS_DEADLINE`] is measured from.
    last_bulk_progress: Option<Instant>,
}

impl GateState {
    /// How much of the post-query quiet period is left, or `None` once it has elapsed.
    fn remaining_quiet(&self) -> Option<Duration> {
        self.last_interactive_end
            .map(|ended| INTERACTIVE_QUIET_PERIOD.saturating_sub(ended.elapsed()))
            .filter(|remaining| !remaining.is_zero())
    }
}

impl SessionGate {
    /// Builds the gate and the run handle corpus encodes will execute under.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::External`] when ONNX Runtime cannot allocate run options, which is the
    /// same class of failure as being unable to create the session itself.
    fn new() -> Result<Self> {
        let bulk_run = RunOptions::new().map_err(|error| {
            Error::new(
                ErrorKind::External,
                format!("create embedding backfill run options: {error}"),
            )
        })?;
        Ok(Self {
            state: Mutex::new(GateState::default()),
            changed: Condvar::new(),
            bulk_run,
        })
    }

    /// Locks the bookkeeping, recovering from poison.
    ///
    /// Nothing between this lock and its release can panic -- the guarded state is a handful of
    /// counters, flags and timestamps -- so a poisoned gate can only have been poisoned by a panic
    /// elsewhere, which cannot have left it inconsistent. Refusing to recover would wedge the
    /// encoder for the life of the process over a panic that never touched it.
    fn lock(&self) -> MutexGuard<'_, GateState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Waits until this priority may run, and returns the pass that releases the session again.
    fn admit(&self, priority: Priority) -> SessionPass<'_> {
        let mut state = self.lock();
        match priority {
            Priority::Interactive => {
                // Registered as waiting *before* the wait, so a corpus text that finishes while
                // this query sleeps sees it and stands down instead of taking another turn.
                state.interactive_waiting += 1;
                while state.held_by.is_some() {
                    if state.held_by == Some(Priority::Bulk) && state.held_preemptible {
                        // Best effort by design. If ONNX Runtime refuses the signal the query
                        // simply waits the corpus encode out, which is where it started.
                        if self.bulk_run.terminate().is_ok() {
                            state.bulk_preempted = true;
                        }
                        // Whether or not it took, do not ask again for this holder: the flag is
                        // already set and re-terminating buys nothing.
                        state.held_preemptible = false;
                    }
                    state = self
                        .changed
                        .wait(state)
                        .unwrap_or_else(PoisonError::into_inner);
                }
                state.interactive_waiting -= 1;
            }
            Priority::Bulk => {
                let deferred_since = Instant::now();
                loop {
                    if state.held_by.is_some() || state.interactive_waiting > 0 {
                        state = self
                            .changed
                            .wait(state)
                            .unwrap_or_else(PoisonError::into_inner);
                        continue;
                    }
                    if deferred_since.elapsed() >= MAX_BULK_DEFERRAL {
                        break;
                    }
                    let Some(quiet) = state.remaining_quiet() else {
                        break;
                    };
                    state = self
                        .changed
                        .wait_timeout(state, quiet)
                        .unwrap_or_else(PoisonError::into_inner)
                        .0;
                }
                // Clearing the abort flag under the same lock that sets it is what makes the
                // handshake sound: no query can slip a `terminate` in between this reset and the
                // moment this run becomes visible as the preemptible holder.
                state.bulk_preempted = false;
                let _ignored = self.bulk_run.unterminate();
                state.held_preemptible = state
                    .last_bulk_progress
                    .is_some_and(|progress| progress.elapsed() < BULK_PROGRESS_DEADLINE);
            }
        }
        state.held_by = Some(priority);
        drop(state);
        SessionPass {
            gate: self,
            priority,
        }
    }

    /// Marks a corpus encode as in progress for as long as the returned scope lives.
    fn bulk_scope(&self) -> BulkScope<'_> {
        let mut state = self.lock();
        state.bulk_pending += 1;
        // Starts the progress clock on the first corpus text of the process, so the deadline is
        // measured from when the backfill began rather than from an epoch it never had.
        state.last_bulk_progress.get_or_insert_with(Instant::now);
        drop(state);
        BulkScope { gate: self }
    }

    /// Takes the "a query aborted your run" flag, if it is set.
    fn take_preempted(&self) -> bool {
        std::mem::take(&mut self.lock().bulk_preempted)
    }

    /// Records that a corpus text was encoded whole, restarting the progress deadline.
    fn note_bulk_progress(&self) {
        self.lock().last_bulk_progress = Some(Instant::now());
    }
}

/// The right to run one encode on the session, released on drop so a failed encode cannot wedge
/// the gate.
struct SessionPass<'gate> {
    gate: &'gate SessionGate,
    priority: Priority,
}

impl Drop for SessionPass<'_> {
    fn drop(&mut self) {
        let mut state = self.gate.lock();
        state.held_by = None;
        state.held_preemptible = false;
        match self.priority {
            Priority::Interactive => state.last_interactive_end = Some(Instant::now()),
            Priority::Bulk => state.last_bulk_end = Some(Instant::now()),
        }
        drop(state);
        // Every waiter has a different admission rule, so waking one arbitrary thread can wake the
        // one thread that must keep waiting and leave a runnable one asleep.
        self.gate.changed.notify_all();
    }
}

/// One corpus encode's presence, from the moment it is asked for to the moment it is answered.
struct BulkScope<'gate> {
    gate: &'gate SessionGate,
}

impl Drop for BulkScope<'_> {
    fn drop(&mut self) {
        let mut state = self.gate.lock();
        state.bulk_pending -= 1;
        state.last_bulk_end = Some(Instant::now());
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
    /// Take the final position's hidden state, which is where a causal decoder accumulates the
    /// whole sequence. Only the last position has attended to everything before it, so for this
    /// family it is the only position that carries a sentence at all.
    LastToken,
}

/// Which encoder family the loaded model belongs to.
///
/// This is the one fact the rest of the file branches on, and it exists because the two families
/// share nothing but the file names. It is deliberately not inferred from the graph: an encoder and
/// a decoder can declare identical inputs and an identically shaped output and still need opposite
/// treatment at every step.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelFamily {
    /// bge-m3: XLM-R tokenizer, `[CLS]` pooling, queries and documents encoded identically.
    ///
    /// This is what a model directory with no `config.json` is taken to be, because that is what
    /// every directory the installer has written so far contains.
    BgeM3,
    /// F2LLM over a Qwen3-0.6B decoder: last-token pooling, an explicit `position_ids` input, and
    /// an instruction prefix that goes in front of queries and never in front of corpus text.
    Qwen3,
}

/// What a text is being encoded *as*.
///
/// For bge-m3 the distinction is invisible -- the same text produces the same vector either way. For
/// an asymmetric model it changes the text before it is ever tokenized, which is why it has to be
/// carried this far down rather than decided by the caller.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TextRole {
    /// A retrieval query, which is what a user is waiting on.
    Query,
    /// Corpus text being embedded for later comparison.
    Document,
}

/// The instruction F2LLM was trained to see in front of a query and never in front of a document.
///
/// The asymmetry is the model's, not a choice made here: dropping it costs recall on every query,
/// and adding it to corpus text moves the whole corpus into the query region of the space, which
/// costs more. Both mistakes are silent -- the vectors stay unit-length and the right width -- so
/// the two sides are separated by [`TextRole`] at the one place that can still tell them apart.
const QWEN3_QUERY_INSTRUCTION: &str =
    "Instruct: Given a question, retrieve passages that can help answer the question.\nQuery: ";

/// `<|im_end|>`, the token this tokenizer's post-processor appends to every sequence.
///
/// Last-token pooling reads whatever token ends up last, so this one is not decoration: truncation
/// cutting it off leaves the vector meaning "the 511th token of the text" instead of "the text",
/// with nothing in the output to say so. [`OnnxEmbeddingProvider::input_ids`] puts it back.
const QWEN3_EOS_TOKEN: i64 = 151_645;

/// Reads the model family out of the directory `[retrieval]` names.
///
/// Three outcomes, and the third is the point. No `config.json` is bge-m3, because that is exactly
/// what the installations that predate this function look like and a working install must not start
/// failing on an upgrade. A `config.json` naming a family this provider implements is that family.
/// Anything else -- unreadable, unparseable, or naming a `model_type` nobody here has calibrated --
/// is a load failure, because the alternative is picking a tokenization and a pooling by coin flip
/// and reporting the result as retrieval.
fn detect_family(model_directory: &Path) -> Result<ModelFamily> {
    let config_path = model_directory.join("config.json");
    let raw = match std::fs::read(&config_path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ModelFamily::BgeM3);
        }
        Err(error) => {
            return Err(Error::new(
                ErrorKind::Io,
                format!("read model config {}: {error}", config_path.display()),
            ));
        }
    };
    let config: serde_json::Value = serde_json::from_slice(&raw).map_err(|error| {
        Error::new(
            ErrorKind::InvalidInput,
            format!("parse model config {}: {error}", config_path.display()),
        )
    })?;
    match config.get("model_type").and_then(serde_json::Value::as_str) {
        Some("qwen3") => Ok(ModelFamily::Qwen3),
        other => Err(Error::new(
            ErrorKind::InvalidInput,
            format!(
                "model config {} declares model_type {:?}, which this build has no encoder \
                 contract for",
                config_path.display(),
                other.unwrap_or("<missing>")
            ),
        )),
    }
}

/// A loaded ONNX encoder, of one of the families [`ModelFamily`] names.
pub struct OnnxEmbeddingProvider {
    // `Session::run` needs `&mut`, and the provider is shared across the backfill thread and the
    // query path. One session is the memory-honest choice -- a second would double the ~1.2 GB
    // resident weights -- so the sharing is arbitrated rather than removed: `gate` decides who
    // runs next and `session` is uncontended by the time anyone reaches it.
    gate: SessionGate,
    session: Mutex<Session>,
    tokenizer: Tokenizer,
    input_names: Vec<String>,
    output_name: String,
    family: ModelFamily,
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
    /// [`ErrorKind::InvalidInput`] when the model cannot be loaded, declares a family this build
    /// has no contract for, or has no output this provider knows how to pool.
    pub fn load(model_directory: &Path) -> Result<Self> {
        let family = detect_family(model_directory)?;
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
        let (output_name, pooling) = select_output(family, &output_names)?;

        Ok(Self {
            gate: SessionGate::new()?,
            session: Mutex::new(session),
            tokenizer,
            input_names,
            output_name,
            family,
            pooling,
            // Both families are 1024-wide, but the width is confirmed from the first encode rather
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
        let probe = provider.encode_inner("shared context", Priority::Interactive)?;
        if probe.is_empty() {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "embedding model produced an empty vector",
            ));
        }
        provider.dimensions = probe.len();
        Ok(provider)
    }

    /// Which family this provider loaded, for callers that report what is installed.
    #[must_use]
    pub const fn family(&self) -> ModelFamily {
        self.family
    }

    /// The exact token sequence `text` is encoded as in `role`.
    ///
    /// Public because nothing downstream can check it. Every mistake this function can make -- a
    /// missing query instruction, a truncation that drops the token last-token pooling reads, an
    /// off-by-one anywhere in the sequence -- produces a unit-length vector of the correct width
    /// that simply means something other than the text. There is no assertion available at the
    /// vector, so the assertion has to be available at the tokens, against a reference
    /// implementation's. Nothing on the retrieval path calls this; [`Self::encode_inner`] shares it.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::InvalidInput`] when the tokenizer rejects the text.
    pub fn input_ids(&self, text: &str, role: TextRole) -> Result<Vec<i64>> {
        let text = match (self.family, role) {
            (ModelFamily::Qwen3, TextRole::Query) => {
                Cow::Owned(format!("{QWEN3_QUERY_INSTRUCTION}{text}"))
            }
            // bge-m3 is symmetric, and a document is a document in every family. The prefix is
            // joined here rather than by the caller so the query vector cache stays keyed on the
            // text a session actually asked about.
            _ => Cow::Borrowed(text),
        };
        let mut encoding = self
            .tokenizer
            .encode(text.as_ref(), true)
            .map_err(|error| {
                Error::new(ErrorKind::InvalidInput, format!("tokenize query: {error}"))
            })?;
        encoding.truncate(
            SEMANTIC_MAX_TOKENS,
            0,
            tokenizers::TruncationDirection::Right,
        );
        let mut ids = encoding
            .get_ids()
            .iter()
            .map(|id| i64::from(*id))
            .collect::<Vec<_>>();
        // Truncation cuts from the right, which is precisely where the post-processor put the token
        // this family pools on. Restoring it costs the last token of the text and keeps the vector
        // meaning the text; not restoring it costs the vector its meaning and says nothing.
        if self.family == ModelFamily::Qwen3 && ids.last() != Some(&QWEN3_EOS_TOKEN) {
            if ids.len() >= SEMANTIC_MAX_TOKENS {
                ids.truncate(SEMANTIC_MAX_TOKENS.saturating_sub(1));
            }
            ids.push(QWEN3_EOS_TOKEN);
        }
        if ids.is_empty() {
            ids.push(0);
        }
        Ok(ids)
    }

    /// Encodes one text, waiting for the session on `priority`'s terms.
    ///
    /// Tokenisation happens before admission on purpose: it needs no session, and doing it inside
    /// the gate would lengthen the window a waiting query has to sit through for no gain.
    fn encode_inner(&self, text: &str, priority: Priority) -> Result<Vec<f32>> {
        let ids = self.input_ids(text, priority.role())?;
        let length = ids.len();
        let mask = vec![1_i64; length];
        let type_ids = vec![0_i64; length];
        // Built only when the graph asks for it. bge-m3 derives its positions internally and
        // declares no such input, so handing one over unasked would be an input the session has no
        // slot for; the decoder export declares it and gets a plain `arange`, which is what a
        // batch of one with no padding means.
        let positions = if self.input_names.iter().any(|name| name == "position_ids") {
            (0..length)
                .map(|index| i64::try_from(index).unwrap_or(i64::MAX))
                .collect()
        } else {
            Vec::new()
        };

        let shape = [1_usize, length];
        let mut inputs = Vec::new();
        for name in &self.input_names {
            let values: &[i64] = match name.as_str() {
                "input_ids" => &ids,
                "attention_mask" => &mask,
                "token_type_ids" => &type_ids,
                "position_ids" => &positions,
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

        let _pass = self.gate.admit(priority);
        let mut session = self.session.lock().map_err(|_| {
            Error::new(
                ErrorKind::InvariantViolation,
                "embedding session lock was poisoned",
            )
        })?;
        // Corpus runs carry the gate's abort handle; queries deliberately carry none, so nothing
        // the gate does to the backfill can reach the encode a user is waiting on.
        let run = match priority {
            Priority::Interactive => session.run(inputs),
            Priority::Bulk => session.run_with_options(inputs, &self.gate.bulk_run),
        };
        let outputs = run.map_err(|error| {
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
        self.encode_checked(text, Priority::Interactive)
    }

    fn encode_bulk(&self, text: &str) -> Result<Vec<f32>> {
        // The scope is taken before admission so a corpus text that is still queuing already
        // counts as backfill pressure: a query encoded while it waits is a contended encode, and
        // the sample must say so. It covers the retries too, which is right -- an aborted attempt
        // is backfill pressure like any other.
        let _scope = self.gate.bulk_scope();
        loop {
            match self.encode_checked(text, Priority::Bulk) {
                Ok(vector) => {
                    self.gate.note_bulk_progress();
                    return Ok(vector);
                }
                // A run a query aborted failed for a reason that has nothing to do with this text.
                // Reporting it would cost the corpus a revision permanently, on the strength of an
                // interruption; the retry queues behind the query that caused it and tries again.
                // The loop terminates because a backfill starved past `BULK_PROGRESS_DEADLINE`
                // stops being preemptible.
                Err(error) => {
                    if !self.gate.take_preempted() {
                        return Err(error);
                    }
                }
            }
        }
    }
}

impl OnnxEmbeddingProvider {
    /// Encodes and confirms the width the probe established.
    fn encode_checked(&self, text: &str, priority: Priority) -> Result<Vec<f32>> {
        let vector = self.encode_inner(text, priority)?;
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
///
/// The family decides only the second half. Whether the graph pooled for us is a fact about the
/// graph and reads the same either way; *which* position carries the sentence when it did not is a
/// fact about the model, and reading a decoder's first position is how one gets a plausible vector
/// of the start-of-sequence token instead of the text.
fn select_output(family: ModelFamily, output_names: &[String]) -> Result<(String, Pooling)> {
    for candidate in ["dense_vecs", "sentence_embedding", "text_embeds"] {
        if let Some(name) = output_names.iter().find(|name| name.as_str() == candidate) {
            return Ok((name.clone(), Pooling::Pooled));
        }
    }
    let sequence_pooling = match family {
        ModelFamily::BgeM3 => Pooling::ClassToken,
        ModelFamily::Qwen3 => Pooling::LastToken,
    };
    for candidate in ["last_hidden_state", "token_embeddings", "output"] {
        if let Some(name) = output_names.iter().find(|name| name.as_str() == candidate) {
            return Ok((name.clone(), sequence_pooling));
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
    // Batch is always one and the class token is index zero, so the first two strategies read the
    // same leading slice; they differ in what the rest of the buffer means, not in where the answer
    // is. The third is the one that has to look.
    match pooling {
        Pooling::Pooled | Pooling::ClassToken => Ok(values[..width].to_vec()),
        Pooling::LastToken => {
            // Unlike the other two, this answer is not at the front of the buffer, so the sequence
            // length is load-bearing and the shape is checked rather than trusted. A rank-2 output
            // would be a graph that already pooled, and its "last row" is the tail of one vector,
            // not the vector -- a mistake that still returns 1024 finite floats.
            let [batch, sequence, _] = shape else {
                return Err(Error::new(
                    ErrorKind::External,
                    format!(
                        "last-token pooling needs a [batch, sequence, hidden] output, got {shape:?}"
                    ),
                ));
            };
            if *batch != 1 {
                return Err(Error::new(
                    ErrorKind::External,
                    format!("embedding output has batch {batch}, expected one sequence"),
                ));
            }
            let sequence = usize::try_from(*sequence)
                .ok()
                .filter(|sequence| *sequence > 0)
                .ok_or_else(|| {
                    Error::new(
                        ErrorKind::External,
                        format!("embedding output has unusable shape {shape:?}"),
                    )
                })?;
            let end = sequence.checked_mul(width).ok_or_else(|| {
                Error::new(
                    ErrorKind::External,
                    format!("embedding output has unusable shape {shape:?}"),
                )
            })?;
            if values.len() < end {
                return Err(Error::new(
                    ErrorKind::External,
                    "embedding output is shorter than the shape it declares",
                ));
            }
            Ok(values[end - width..end].to_vec())
        }
    }
}
