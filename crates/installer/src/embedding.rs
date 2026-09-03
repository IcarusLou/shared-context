//! One-command provisioning of the optional embedding recall channel (ADR-0004).
//!
//! ADR-0004 decided that the model is *not* distributed with the package: `[retrieval]` points at
//! files the operator fetched themselves. That decision stands -- nothing here is bundled, nothing
//! is downloaded at build time, and an installation that never runs `sctx embedding install` is
//! byte-identical to one that never heard of this module. What it removes is the six-step manual
//! errand the decision implied: fetch 2.1 GB of weights from one host, an ONNX Runtime tarball from
//! another, unpack the right dynamic library out of it, hand-edit `config.toml`, and then discover
//! at `serve` time whether any of it actually loads.
//!
//! ## Why `curl` instead of an HTTP client
//!
//! A 2.2 GB transfer wants conditional resume, redirect following, and retry -- which is either a
//! new HTTP stack (and, in practice, a TLS stack and an async runtime) in the dependency graph of
//! a tool whose entire value proposition is that it stays out of the way, or a `curl` that every
//! supported platform already ships. The download is a rare, operator-initiated, foreground errand,
//! so the cost of a subprocess is irrelevant and the cost of the dependency is permanent. `curl`
//! wins. It is run through the same bounded-child shape as every other subprocess in this
//! workspace: piped output to a file, wall-clock deadline, kill and reap on overrun.
//!
//! ## Why the hashes are pinned in the binary
//!
//! `--model-url` exists so a team can serve the same files from an internal mirror, which is
//! exactly the situation where "the bytes came from somewhere else" must not mean "the bytes are
//! different". So the digests below are properties of the *files*, not of the host: a mirror is
//! checked against the same constants as the default source, and a mismatch is a hard failure that
//! leaves nothing behind. They were measured on the artifacts this repository's own T5b acceptance
//! run used; see [`MODEL_FILES`] and [`RUNTIME_ARM64`].
//!
//! ## Why the config write comes last
//!
//! A configured `[retrieval]` that cannot load is strictly worse than an unconfigured one: the
//! former costs a 9--12 second load attempt and an `embedding_unavailable` omission on every
//! Pack, the latter costs nothing. So the model is loaded and asked to encode one real sentence
//! *before* `config.toml` is touched, and a failure at that point leaves an installation that is
//! merely as it was.

use std::{
    ffi::OsString,
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use sctx_domain::{Error, ErrorKind, Result};
use sctx_local_state::UserConfigStore;
use sctx_search::{
    SemanticCacheKey, SemanticVectorCache, load_onnx_provider, model_fingerprint,
    semantic_cache_path,
};
use serde::Serialize;
use sha2::{Digest, Sha256};

use super::{ensure_private_directory, io_error};

/// Default mirror for the bge-m3 ONNX export.
///
/// The mirror leads because the operators this command exists for are the ones for whom the
/// canonical host is slow or unreachable; [`HUGGINGFACE_MODEL_BASE`] is tried next, and both are
/// checked against the same digests.
pub const MIRROR_MODEL_BASE: &str = "https://hf-mirror.com/BAAI/bge-m3/resolve/main/onnx";

/// Canonical upstream for the bge-m3 ONNX export, used when the mirror does not answer.
pub const HUGGINGFACE_MODEL_BASE: &str = "https://huggingface.co/BAAI/bge-m3/resolve/main/onnx";

/// The ONNX Runtime release this command provisions.
pub const RUNTIME_VERSION: &str = "1.28.1";

/// Directory name under the installation root that holds everything this command writes.
const EMBEDDING_DIRECTORY: &str = "embedding";

/// The sentence the self-check encodes. Any non-empty text proves the same thing; a real one
/// keeps the failure message readable when a tokenizer is subtly wrong.
const SELF_CHECK_TEXT: &str = "shared context embedding channel self check";

/// Wall-clock budget for one `model.onnx_data` transfer.
///
/// 2.2 GB at a pessimistic 1.2 MB/s is roughly half an hour, which is the point of the generous
/// budget: this bound exists to stop a wedged connection from hanging forever, not to enforce a
/// throughput expectation. Resume makes a timeout cheap -- the next run continues where this one
/// stopped.
const LARGE_DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// Wall-clock budget for the small transfers (tokenizer, graph, runtime tarball).
const SMALL_DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// Wall-clock budget for `curl --version` and for unpacking the runtime tarball.
const TOOL_TIMEOUT: Duration = Duration::from_secs(2 * 60);

/// How often a bounded child is polled for exit.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// How much of a failed child's stderr is quoted back. Enough for `curl`'s one-line diagnosis,
/// bounded so a runaway process cannot turn an error message into a memory problem.
const MAX_CHILD_STDERR_BYTES: usize = 8 * 1024;

/// How many stderr lines of a failed child are quoted back.
const MAX_CHILD_STDERR_LINES: usize = 12;

/// How much is read at a time when digesting a multi-gigabyte file.
const HASH_CHUNK_BYTES: usize = 1024 * 1024;

/// One file this command fetches, with the integrity it must have afterwards.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct RemoteFile {
    /// Name at the source and on disk. The two are always equal, which is what makes
    /// `--model-url` a base directory rather than a per-file mapping.
    pub name: &'static str,
    /// Lowercase hex SHA-256 of the complete file.
    pub sha256: &'static str,
    /// Exact byte length, checked before the digest so a truncated transfer fails in constant time.
    pub bytes: u64,
}

/// The bge-m3 ONNX export, as measured on the artifacts used for the ADR-0004 acceptance run.
///
/// These three files are what [`sctx_search::load_onnx_provider`] opens: `model.onnx` is the graph,
/// `model.onnx_data` its external weights (the export is split, so the graph alone cannot load),
/// and `tokenizer.json` the vocabulary. The remaining files in the upstream directory --
/// `config.json`, `tokenizer_config.json`, `special_tokens_map.json` -- are not read by the
/// provider and are deliberately not fetched.
pub const MODEL_FILES: [RemoteFile; 3] = [
    RemoteFile {
        name: "model.onnx",
        sha256: "f84251230831afb359ab26d9fd37d5936d4d9bb5d1d5410e66442f630f24435b",
        bytes: 724_923,
    },
    RemoteFile {
        name: "model.onnx_data",
        sha256: "1eebfb28493f67bba03ce0ef64bfdc7fc5a3bd9d7493f818bb1d78cd798416b4",
        bytes: 2_266_820_608,
    },
    RemoteFile {
        name: "tokenizer.json",
        sha256: "6710678b12670bc442b99edc952c4d996ae309a7020c1fa0096dd245c2faf790",
        bytes: 17_082_821,
    },
];

/// The macOS/arm64 ONNX Runtime release tarball.
///
/// Measured from the published asset, which is the same build the acceptance run loaded.
pub const RUNTIME_ARM64: RemoteFile = RemoteFile {
    name: "onnxruntime-osx-arm64-1.28.1.tgz",
    sha256: "18c853e5c5deba90e244e1b953a65121f23747ba2c04e6743885caa6ce1ea12f",
    bytes: 31_484_009,
};

/// A platform this command can provision without help.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Platform {
    /// Apple silicon macOS, the only target with a pinned upstream runtime build.
    DarwinArm64,
    /// Intel macOS. The model is identical; the runtime has to be supplied, see
    /// [`Platform::default_runtime`].
    DarwinX64,
}

impl Platform {
    /// Identifies the running platform.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::Unsupported`] elsewhere, with the manual `[retrieval]` route, because
    /// the channel itself is portable -- only this convenience is not.
    pub fn detect() -> Result<Self> {
        Self::from_target(std::env::consts::OS, std::env::consts::ARCH)
    }

    /// Identifies a platform from an OS/architecture pair, so the mapping is testable without
    /// cross-compiling.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::Unsupported`] for every pair this command has no plan for.
    pub fn from_target(os: &str, arch: &str) -> Result<Self> {
        match (os, arch) {
            ("macos", "aarch64") => Ok(Self::DarwinArm64),
            ("macos", "x86_64") => Ok(Self::DarwinX64),
            _ => Err(Error::new(
                ErrorKind::Unsupported,
                format!(
                    "`sctx embedding install` supports macOS on arm64 and x86_64; this host is \
                     {os}/{arch}. The embedding channel itself is not limited to those: download \
                     a bge-m3 ONNX export (`model.onnx`, `model.onnx_data`, `tokenizer.json`) and \
                     an ONNX Runtime {RUNTIME_VERSION} shared library for this platform, then set \
                     `[retrieval] embedding_model_path` to the model directory and `[retrieval] \
                     embedding_runtime_path` to the library in `config.toml`. `sctx doctor` \
                     reports whether the result loads."
                ),
            )),
        }
    }

    /// The stable name used in reports and error messages.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DarwinArm64 => "darwin-arm64",
            Self::DarwinX64 => "darwin-x64",
        }
    }

    /// The runtime tarball this platform downloads by default, when one is published.
    ///
    /// `None` for [`Platform::DarwinX64`]: ONNX Runtime {`RUNTIME_VERSION`} publishes no macOS
    /// `x86_64` asset at all, so there is no URL to default to and no digest that could have been
    /// measured. Rather than guess a build, that platform requires `--runtime-url` together with
    /// `--expected-sha256`, which keeps the "verified before it is used" invariant intact while
    /// being honest that the operator, not this binary, is vouching for the bytes.
    #[must_use]
    pub const fn default_runtime(self) -> Option<(&'static str, RemoteFile)> {
        match self {
            Self::DarwinArm64 => Some((
                "https://github.com/microsoft/onnxruntime/releases/download/v1.28.1/onnxruntime-osx-arm64-1.28.1.tgz",
                RUNTIME_ARM64,
            )),
            Self::DarwinX64 => None,
        }
    }

    /// The dynamic library name this platform loads.
    #[must_use]
    pub const fn runtime_library_name(self) -> &'static str {
        "libonnxruntime.dylib"
    }
}

/// What the operator asked `sctx embedding install` to do.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct InstallOptions {
    /// Base URL holding the three model files under their upstream names. A team mirror goes
    /// here; the pinned digests still apply.
    pub model_url: Option<String>,
    /// Full URL of an ONNX Runtime release tarball.
    pub runtime_url: Option<String>,
    /// Digest the runtime tarball must have, for the platform or mirror this binary has not
    /// measured.
    pub expected_runtime_sha256: Option<String>,
}

/// What one file cost, and where it ended up.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct FileOutcome {
    pub name: String,
    pub path: PathBuf,
    pub bytes: u64,
    /// True when the file was already present and verified, so nothing was transferred.
    pub reused: bool,
    /// The URL the bytes came from, absent when the file was reused.
    pub source: Option<String>,
}

/// What `sctx embedding install` did.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct InstallReport {
    pub root: PathBuf,
    pub platform: &'static str,
    pub model_path: PathBuf,
    pub runtime_path: PathBuf,
    pub files: Vec<FileOutcome>,
    /// Width of the vector the self-check encode produced, which is also the proof it ran.
    pub self_check_dimensions: usize,
    pub configured: bool,
    /// Revisions embedded by the warm-up this command performed.
    pub embedded: usize,
    /// Revisions the vector cache holds afterwards.
    pub cached: usize,
    /// Why the warm-up did not complete, when it did not. The channel is configured and working
    /// either way; the cache simply fills at `serve` instead.
    pub warm_error: Option<String>,
}

/// One file's contribution to `sctx embedding status`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct FileStatus {
    pub name: String,
    pub path: PathBuf,
    pub present: bool,
    pub expected_bytes: Option<u64>,
    pub actual_bytes: Option<u64>,
    /// True when the file is present and, where a size is pinned, exactly that size.
    ///
    /// Sizes are checked and digests are not: re-hashing 2.2 GB on every `status` would turn a
    /// glance into a ten-second errand, and the failure mode status exists to catch -- a
    /// half-written or deleted file -- changes the size. `--verify` is the answer when the
    /// question is whether the bytes are *right* rather than whether they are *there*.
    pub ok: bool,
}

/// What `sctx embedding status` observed.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct StatusReport {
    pub root: PathBuf,
    pub configured: bool,
    pub model_path: Option<PathBuf>,
    pub runtime_path: Option<PathBuf>,
    pub files: Vec<FileStatus>,
    pub model_fingerprint: Option<String>,
    pub embedded_revisions: usize,
    /// What the last [`sctx_search::SEMANTIC_ENCODE_SAMPLE_HISTORY`] query encodes cost, and how
    /// many of them overran the budget.
    ///
    /// A timeout degrades one retrieval to `embedding_unavailable`, which reads in the Pack
    /// exactly like a model that never loaded. That indistinguishability is what let a budget no
    /// real query could meet look like a working channel for an entire release. This is where the
    /// difference is visible: all-zero samples mean the channel has not been asked anything yet, a
    /// low timeout count means it is working, and a high one means the budget does not fit this
    /// machine.
    pub encode_latency: sctx_search::EncodeLatencySummary,
    /// The encode budget in force, in milliseconds, and whether `[retrieval]` set it.
    pub encode_budget_ms: u64,
    pub encode_budget_configured: bool,
    /// `Some` only when `--verify` asked for a real load; `None` means the question was not put.
    pub loads: Option<bool>,
    pub load_error: Option<String>,
    /// True when `[retrieval]` names both halves and every file it names is present at its
    /// expected size.
    pub ready: bool,
}

/// What `sctx embedding remove` deleted.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RemoveReport {
    pub root: PathBuf,
    pub config_cleared: bool,
    pub removed: Vec<PathBuf>,
    pub absent: Vec<PathBuf>,
}

/// Where a step-by-step account goes while a long command runs.
///
/// The CLI's stdout carries one JSON envelope and nothing else, so progress is an advisory stream
/// the caller owns -- stderr in the CLI, a capture buffer in tests.
pub type Progress<'a> = &'a mut dyn FnMut(&str);

/// The model directory this command installs into.
#[must_use]
pub fn model_directory(root: &Path) -> PathBuf {
    root.join(EMBEDDING_DIRECTORY).join("model")
}

/// The runtime directory this command installs into.
#[must_use]
pub fn runtime_directory(root: &Path) -> PathBuf {
    root.join(EMBEDDING_DIRECTORY).join("runtime")
}

/// Where release tarballs are kept between runs, so a re-run does not re-download them.
#[must_use]
fn download_directory(root: &Path) -> PathBuf {
    root.join(EMBEDDING_DIRECTORY).join("downloads")
}

/// One planned transfer: the file that must exist, and the sources to try in order.
///
/// This owns its digest rather than borrowing a [`RemoteFile`], because an operator-supplied
/// `--expected-sha256` is as valid a plan as a pinned one and neither should be more awkward to
/// express than the other.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlannedDownload {
    pub name: String,
    /// Lowercase hex SHA-256 the completed file must have. Never empty: a plan without a digest is
    /// not a plan this command will execute.
    pub sha256: String,
    /// Expected byte length, or 0 when the size was never measured and only the digest decides.
    pub bytes: u64,
    pub destination: PathBuf,
    /// Tried in order until one succeeds. More than one entry only for the default model source,
    /// where a mirror leads and the canonical host backs it up.
    pub sources: Vec<String>,
    pub timeout: Duration,
}

/// Everything `install` will fetch, before it fetches any of it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DownloadPlan {
    pub platform: Platform,
    pub model_directory: PathBuf,
    pub runtime_directory: PathBuf,
    pub model_files: Vec<PlannedDownload>,
    pub runtime_archive: PlannedDownload,
}

impl DownloadPlan {
    /// The library path `[retrieval] embedding_runtime_path` will name.
    #[must_use]
    pub fn runtime_library(&self) -> PathBuf {
        self.runtime_directory
            .join(self.platform.runtime_library_name())
    }
}

/// Builds the complete transfer plan without touching the network or the filesystem.
///
/// Separated from execution so the part with all the policy in it -- which host leads, which
/// digest applies to a mirrored file, what a platform without a published runtime requires -- is
/// checkable in a unit test rather than only observable by downloading two gigabytes.
///
/// # Errors
///
/// Returns [`ErrorKind::InvalidInput`] when the platform has no default runtime and the operator
/// supplied neither `--runtime-url` nor `--expected-sha256`, and when a supplied digest is not a
/// SHA-256.
pub fn plan_downloads(
    root: &Path,
    platform: Platform,
    options: &InstallOptions,
) -> Result<DownloadPlan> {
    let model_directory = model_directory(root);
    let runtime_directory = runtime_directory(root);
    let model_bases = match options.model_url.as_deref() {
        // An explicit source replaces both defaults rather than joining them. Falling back from a
        // team mirror to the public internet would silently undo the reason the mirror was named.
        Some(base) => vec![base.trim_end_matches('/').to_owned()],
        None => vec![
            MIRROR_MODEL_BASE.to_owned(),
            HUGGINGFACE_MODEL_BASE.to_owned(),
        ],
    };
    let model_files = MODEL_FILES
        .iter()
        .map(|file| PlannedDownload {
            name: file.name.to_owned(),
            sha256: file.sha256.to_owned(),
            bytes: file.bytes,
            destination: model_directory.join(file.name),
            sources: model_bases
                .iter()
                .map(|base| format!("{base}/{}", file.name))
                .collect(),
            timeout: if file.bytes > 512 * 1024 * 1024 {
                LARGE_DOWNLOAD_TIMEOUT
            } else {
                SMALL_DOWNLOAD_TIMEOUT
            },
        })
        .collect::<Vec<_>>();
    let runtime_archive = plan_runtime(root, platform, options)?;
    Ok(DownloadPlan {
        platform,
        model_directory,
        runtime_directory,
        model_files,
        runtime_archive,
    })
}

fn plan_runtime(
    root: &Path,
    platform: Platform,
    options: &InstallOptions,
) -> Result<PlannedDownload> {
    let default = platform.default_runtime();
    let (url, pinned) = match (options.runtime_url.as_deref(), default) {
        // A custom URL for a platform this binary has measured is still checked against the
        // measured digest, unless the operator overrides it: same file, different host.
        (Some(url), None) => (url.to_owned(), None),
        (Some(url), Some((_, file))) | (None, Some((url, file))) => (url.to_owned(), Some(file)),
        (None, None) => {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                format!(
                    "ONNX Runtime {RUNTIME_VERSION} publishes no macOS x86_64 release asset, so \
                     there is nothing for `sctx embedding install` to download by default on \
                     {}. Supply a runtime tarball you trust with `--runtime-url <URL> \
                     --expected-sha256 <SHA256>`; the download is rejected unless it hashes to \
                     exactly that. `shasum -a 256 <file>` prints the digest of a tarball you \
                     already have.",
                    platform.as_str()
                ),
            ));
        }
    };
    let (sha256, bytes) = match options.expected_runtime_sha256.as_deref() {
        // An operator vouching for a tarball also invalidates the pinned size: the file they name
        // is by definition not the one that was measured.
        Some(supplied) => (validated_sha256(supplied)?, 0),
        None => match pinned {
            Some(file) => (file.sha256.to_owned(), file.bytes),
            None => {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    format!(
                        "--runtime-url on {} requires --expected-sha256, because this binary has \
                         no measured digest for a macOS x86_64 ONNX Runtime build to check it \
                         against. `shasum -a 256 <file>` prints the digest of a tarball you \
                         already have.",
                        platform.as_str()
                    ),
                ));
            }
        },
    };
    let name = pinned.map_or("onnxruntime.tgz", |file| file.name);
    Ok(PlannedDownload {
        name: name.to_owned(),
        sha256,
        bytes,
        destination: download_directory(root).join(name),
        sources: vec![url],
        timeout: SMALL_DOWNLOAD_TIMEOUT,
    })
}

fn validated_sha256(value: &str) -> Result<String> {
    let normalized = value.trim().to_ascii_lowercase();
    if normalized.len() != 64 || !normalized.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            format!("--expected-sha256 must be 64 hexadecimal characters, got {value:?}"),
        ));
    }
    Ok(normalized)
}

/// Downloads, verifies, unpacks, proves, and configures the embedding channel.
///
/// The order is the contract: nothing lands under its real name until it matches its digest, and
/// `config.toml` is not touched until a loaded model has encoded a sentence. Re-running is
/// idempotent -- an already verified file is not transferred again -- which is what makes an
/// interrupted 2.2 GB download a resumable errand rather than a lost afternoon.
///
/// # Errors
///
/// Returns [`ErrorKind::Unsupported`] on a platform with no plan, [`ErrorKind::External`] when
/// `curl` is missing or a transfer fails, [`ErrorKind::InvalidInput`] on a digest mismatch or an
/// unloadable model, and typed filesystem errors otherwise.
pub fn install(
    root: &Path,
    options: &InstallOptions,
    progress: Progress<'_>,
) -> Result<InstallReport> {
    let platform = Platform::detect()?;
    let plan = plan_downloads(root, platform, options)?;
    require_curl()?;
    // Everything that can be known before the first byte moves is checked before the first byte
    // moves. Discovering after a half-hour download that there is no `config.toml` to write into
    // would be a half hour spent to learn something available immediately.
    require_writable_configuration(root)?;
    ensure_private_directory(root)?;
    ensure_private_directory(&root.join(EMBEDDING_DIRECTORY))?;
    ensure_private_directory(&plan.model_directory)?;
    ensure_private_directory(&plan.runtime_directory)?;
    ensure_private_directory(&download_directory(root))?;

    progress(&format!(
        "platform {}, installing into {}",
        platform.as_str(),
        root.join(EMBEDDING_DIRECTORY).display()
    ));

    let mut files = Vec::new();
    for planned in &plan.model_files {
        files.push(fetch_verified(planned, progress)?);
    }
    let archive = fetch_verified(&plan.runtime_archive, progress)?;
    let runtime_library = plan.runtime_library();
    if runtime_library.is_file() {
        progress(&format!(
            "reusing runtime library {}",
            runtime_library.display()
        ));
    } else {
        unpack_runtime(
            &plan.runtime_archive.destination,
            &plan.runtime_directory,
            platform,
            progress,
        )?;
    }
    files.push(FileOutcome {
        name: platform.runtime_library_name().to_owned(),
        path: runtime_library.clone(),
        bytes: file_size(&runtime_library).unwrap_or(0),
        reused: archive.reused,
        source: archive.source.clone(),
    });

    progress("loading the model to prove it works before writing any configuration");
    let started = Instant::now();
    let provider = load_onnx_provider(&plan.model_directory, &runtime_library)?;
    let vector = provider.encode(SELF_CHECK_TEXT)?;
    if vector.is_empty() {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "the model loaded but encoded an empty vector; `[retrieval]` was left unconfigured",
        ));
    }
    progress(&format!(
        "self check passed: {}-dimensional vector in {:.1}s",
        vector.len(),
        started.elapsed().as_secs_f64()
    ));

    UserConfigStore::open_existing(root)?
        .set_retrieval_embedding(&plan.model_directory, &runtime_library)?;
    progress("wrote `[retrieval] embedding_model_path` and `embedding_runtime_path`");

    // The channel is on from here. A backfill that fails leaves an installation whose vector
    // cache is merely empty, which `serve` fills on its own; it is not a reason to undo a
    // configuration that has already proven it loads.
    let (embedded, cached, warm_error) = match sctx_mcp::warm_semantic_cache_at_root(root) {
        Ok(report) => {
            progress(&format!(
                "embedded {} Context revision(s); {} cached in total",
                report.embedded, report.cached
            ));
            (report.embedded, report.cached, None)
        }
        Err(error) => {
            progress(&format!(
                "warning: the vector cache could not be filled now ({error}); `sctx mcp serve` \
                 fills it in the background"
            ));
            (0, 0, Some(error.to_string()))
        }
    };

    Ok(InstallReport {
        root: root.to_path_buf(),
        platform: platform.as_str(),
        model_path: plan.model_directory,
        runtime_path: runtime_library,
        files,
        self_check_dimensions: vector.len(),
        configured: true,
        embedded,
        cached,
        warm_error,
    })
}

/// Reports what is configured, what is on disk, and how much of the corpus is embedded.
///
/// # Errors
///
/// Returns typed configuration errors when `config.toml` cannot be read.
pub fn status(root: &Path, verify: bool) -> Result<StatusReport> {
    let settings = UserConfigStore::open_existing(root)?.retrieval_settings()?;
    let model_path = settings.embedding_model_path.clone();
    let runtime_path = settings.embedding_runtime_path.clone();
    let mut files = Vec::new();
    if let Some(model_path) = model_path.as_deref() {
        for file in &MODEL_FILES {
            let path = model_path.join(file.name);
            let actual = file_size(&path);
            files.push(FileStatus {
                name: file.name.to_owned(),
                path,
                present: actual.is_some(),
                // A model directory the operator assembled by hand is a supported configuration,
                // and its `model.onnx_data` legitimately differs from the export this command
                // installs. The pinned size is only an expectation for the files this command put
                // there, so it is reported and compared, never required.
                expected_bytes: Some(file.bytes),
                actual_bytes: actual,
                ok: actual.is_some(),
            });
        }
    }
    if let Some(runtime_path) = runtime_path.as_deref() {
        let actual = file_size(runtime_path);
        files.push(FileStatus {
            name: runtime_path.file_name().map_or_else(
                || "runtime".to_owned(),
                |name| name.to_string_lossy().into_owned(),
            ),
            path: runtime_path.to_path_buf(),
            present: actual.is_some(),
            expected_bytes: None,
            actual_bytes: actual,
            ok: actual.is_some(),
        });
    }
    let configured = settings.embedding_enabled();
    let ready = configured && files.iter().all(|file| file.ok);
    let fingerprint = model_path
        .as_deref()
        .and_then(|path| model_fingerprint(path).ok());
    let cache = SemanticVectorCache::open(&semantic_cache_path(root));
    let embedded_revisions = fingerprint.as_deref().map_or(0, |fingerprint| {
        cache
            .as_ref()
            .ok()
            .and_then(|cache| {
                cache
                    .cached_revisions(&SemanticCacheKey::new(
                        fingerprint,
                        sctx_index::SEARCH_RANKING_VERSION,
                    ))
                    .ok()
            })
            .map_or(0, |revisions| revisions.len())
    });
    // The history is not keyed by model generation: it describes what this machine can encode in,
    // which does not stop being true when the operator swaps exports.
    let encode_latency = cache
        .as_ref()
        .ok()
        .and_then(|cache| cache.encode_latency_summary().ok())
        .unwrap_or_default();
    let (loads, load_error) = if verify && ready {
        // Only a `--verify` pays the 9--12 second load, and only when the files it needs are
        // there: reporting "it does not load" about a file that is simply missing would name the
        // wrong problem.
        match model_path
            .as_deref()
            .zip(runtime_path.as_deref())
            .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "incomplete `[retrieval]`"))
            .and_then(|(model, runtime)| load_onnx_provider(model, runtime))
            .and_then(|provider| provider.encode(SELF_CHECK_TEXT))
        {
            Ok(vector) if !vector.is_empty() => (Some(true), None),
            Ok(_) => (
                Some(false),
                Some("the model encoded an empty vector".to_owned()),
            ),
            Err(error) => (Some(false), Some(error.to_string())),
        }
    } else {
        (None, None)
    };
    Ok(StatusReport {
        root: root.to_path_buf(),
        configured,
        model_path,
        runtime_path,
        files,
        model_fingerprint: fingerprint,
        embedded_revisions,
        encode_latency,
        encode_budget_ms: u64::try_from(
            settings
                .encode_budget()
                .unwrap_or(sctx_search::SEMANTIC_ENCODE_BUDGET)
                .as_millis(),
        )
        .unwrap_or(u64::MAX),
        encode_budget_configured: settings.embedding_encode_budget_ms.is_some(),
        loads,
        load_error,
        ready,
    })
}

/// Removes the configuration, the downloaded files, and the vector cache.
///
/// # Errors
///
/// Returns [`ErrorKind::InvalidInput`] without `confirmed`, and typed configuration or filesystem
/// errors when a removal cannot be completed.
pub fn remove(root: &Path, confirmed: bool, progress: Progress<'_>) -> Result<RemoveReport> {
    if !confirmed {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "embedding remove deletes the downloaded model, the ONNX Runtime library, and the \
             vector cache; rerun with --yes",
        ));
    }
    let config_cleared = UserConfigStore::open_existing(root)?.clear_retrieval_embedding()?;
    progress(if config_cleared {
        "removed `[retrieval]` from config.toml"
    } else {
        "`[retrieval]` was not configured"
    });
    let mut removed = Vec::new();
    let mut absent = Vec::new();
    let cache = semantic_cache_path(root);
    // The two sidecars are SQLite's, not ours: leaving a `-wal` behind next to a deleted database
    // would leave the next `open` a partial journal to recover from.
    let targets = [
        root.join(EMBEDDING_DIRECTORY),
        cache.clone(),
        sidecar(&cache, "-wal"),
        sidecar(&cache, "-shm"),
    ];
    for target in targets {
        match fs::symlink_metadata(&target) {
            Ok(metadata) => {
                if metadata.is_dir() && !metadata.file_type().is_symlink() {
                    fs::remove_dir_all(&target).map_err(io_error("remove embedding directory"))?;
                } else {
                    fs::remove_file(&target).map_err(io_error("remove embedding file"))?;
                }
                progress(&format!("removed {}", target.display()));
                removed.push(target);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                absent.push(target);
            }
            Err(error) => return Err(io_error("inspect embedding target")(error)),
        }
    }
    Ok(RemoveReport {
        root: root.to_path_buf(),
        config_cleared,
        removed,
        absent,
    })
}

fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut name = OsString::from(path.as_os_str());
    name.push(suffix);
    PathBuf::from(name)
}

/// Ensures one planned file exists and matches its digest, downloading it if it does not.
fn fetch_verified(planned: &PlannedDownload, progress: Progress<'_>) -> Result<FileOutcome> {
    if let Some(actual) = file_size(&planned.destination)
        && size_matches(planned.bytes, actual)
    {
        progress(&format!("verifying existing {}", planned.name));
        if sha256_file(&planned.destination)? == planned.sha256 {
            progress(&format!("{} already present and verified", planned.name));
            return Ok(FileOutcome {
                name: planned.name.clone(),
                path: planned.destination.clone(),
                bytes: actual,
                reused: true,
                source: None,
            });
        }
        // A file with the right size and the wrong content cannot be resumed into correctness.
        fs::remove_file(&planned.destination).map_err(io_error("remove mismatched download"))?;
    }
    let part = sidecar(&planned.destination, ".part");
    // A partial larger than the target can only be junk, and `curl --continue-at -` would ask the
    // server to resume past the end of the file rather than start over.
    if let Some(existing) = file_size(&part)
        && planned.bytes > 0
        && existing > planned.bytes
    {
        fs::remove_file(&part).map_err(io_error("remove oversized partial download"))?;
    }
    let mut failures = Vec::new();
    for source in &planned.sources {
        progress(&format!("downloading {} from {source}", planned.name));
        if let Err(error) = curl(source, &part, planned.timeout) {
            failures.push(format!("{source}: {error}"));
            continue;
        }
        let actual = file_size(&part).unwrap_or(0);
        if !size_matches(planned.bytes, actual) {
            fs::remove_file(&part).map_err(io_error("remove short download"))?;
            failures.push(format!(
                "{source}: expected {} bytes, got {actual}",
                planned.bytes
            ));
            continue;
        }
        progress(&format!("verifying {}", planned.name));
        let digest = sha256_file(&part)?;
        if digest != planned.sha256 {
            // Leaving the partial behind would poison every later resume with bytes already known
            // to be wrong, so the failed transfer is removed and nothing takes the real name.
            fs::remove_file(&part).map_err(io_error("remove corrupted download"))?;
            return Err(Error::new(
                ErrorKind::InvalidInput,
                format!(
                    "{} from {source} has SHA-256 {digest}, expected {}. Nothing was installed \
                     and `[retrieval]` was not configured.",
                    planned.name, planned.sha256
                ),
            ));
        }
        fs::rename(&part, &planned.destination).map_err(io_error("publish verified download"))?;
        progress(&format!("{} verified ({actual} bytes)", planned.name));
        return Ok(FileOutcome {
            name: planned.name.clone(),
            path: planned.destination.clone(),
            bytes: actual,
            reused: false,
            source: Some(source.clone()),
        });
    }
    Err(Error::new(
        ErrorKind::External,
        format!(
            "could not download {}: {}",
            planned.name,
            failures.join("; ")
        ),
    ))
}

const fn size_matches(expected: u64, actual: u64) -> bool {
    // A pinned size of 0 means "not measured" -- an operator-supplied tarball -- and only the
    // digest decides.
    expected == 0 || expected == actual
}

fn file_size(path: &Path) -> Option<u64> {
    fs::metadata(path)
        .ok()
        .filter(std::fs::Metadata::is_file)
        .map(|metadata| metadata.len())
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path).map_err(io_error("open file for hashing"))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; HASH_CHUNK_BYTES];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(io_error("read file for hashing"))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// Confirms there is an installation to configure, before anything is downloaded.
///
/// Reading `[retrieval]` is the cheapest possible proof that `config.toml` exists, parses, and can
/// be reached: it is the same read the write at the end of `install` has to survive.
fn require_writable_configuration(root: &Path) -> Result<()> {
    UserConfigStore::open_existing(root)
        .and_then(|config| config.retrieval_settings())
        .map(|_| ())
        .map_err(|error| {
            Error::new(
                ErrorKind::InvalidInput,
                format!(
                    "`sctx embedding install` configures an existing installation, and {} could \
                     not be read: {error}. Run `sctx setup` first.",
                    root.join("config.toml").display()
                ),
            )
        })
}

fn require_curl() -> Result<()> {
    let mut command = Command::new("curl");
    command.arg("--version");
    if run_bounded(&mut command, TOOL_TIMEOUT).is_ok() {
        return Ok(());
    }
    Err(Error::new(
        ErrorKind::External,
        "`sctx embedding install` downloads through `curl`, which this host does not have. \
         Install curl, or fetch the files by hand -- `model.onnx`, `model.onnx_data` and \
         `tokenizer.json` from https://huggingface.co/BAAI/bge-m3/resolve/main/onnx/ into one \
         directory, and an ONNX Runtime shared library -- then point `[retrieval] \
         embedding_model_path` and `embedding_runtime_path` at them in `config.toml`.",
    ))
}

fn curl(url: &str, destination: &Path, timeout: Duration) -> Result<()> {
    let mut command = Command::new("curl");
    command
        .arg("--fail")
        .arg("--location")
        .arg("--retry")
        .arg("3")
        .arg("--continue-at")
        .arg("-")
        .arg("--no-progress-meter")
        .arg("--show-error")
        .arg("--output")
        .arg(destination)
        .arg(url);
    run_bounded(&mut command, timeout)
}

/// Runs a child with a wall-clock deadline, killing and reaping it on overrun.
///
/// Both streams go to a pipe drained on their own threads, which is the same shape the Git callers
/// in this workspace use and the reason a child that writes more than a pipe buffer cannot wedge
/// the parent. Only the tail of stderr survives into an error message.
fn run_bounded(command: &mut Command, timeout: Duration) -> Result<()> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| {
            Error::new(
                ErrorKind::External,
                format!("start {:?}: {error}", command.get_program()),
            )
        })?;
    let stderr = child.stderr.take();
    let reader = stderr.map(|stream| thread::spawn(move || read_capped(stream)));
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                if let Some(reader) = reader {
                    let _ = reader.join();
                }
                return Err(Error::new(
                    ErrorKind::External,
                    format!(
                        "{:?} exceeded its {}s budget and was stopped; rerun to resume",
                        command.get_program(),
                        timeout.as_secs()
                    ),
                ));
            }
            Ok(None) => thread::sleep(POLL_INTERVAL),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                if let Some(reader) = reader {
                    let _ = reader.join();
                }
                return Err(Error::new(
                    ErrorKind::External,
                    format!("wait for {:?}: {error}", command.get_program()),
                ));
            }
        }
    };
    let detail = reader
        .and_then(|reader| reader.join().ok())
        .unwrap_or_default();
    if status.success() {
        return Ok(());
    }
    Err(Error::new(
        ErrorKind::External,
        format!(
            "{:?} exited with {}{}",
            command.get_program(),
            status
                .code()
                .map_or_else(|| "a signal".to_owned(), |code| code.to_string()),
            if detail.is_empty() {
                String::new()
            } else {
                format!(": {detail}")
            }
        ),
    ))
}

fn read_capped(mut stream: impl Read) -> String {
    let mut buffer = Vec::new();
    let _ = stream
        .by_ref()
        .take(MAX_CHILD_STDERR_BYTES as u64)
        .read_to_end(&mut buffer);
    // Drain whatever is left so the child is never blocked writing into a full pipe, without
    // keeping any of it.
    let _ = std::io::copy(&mut stream, &mut std::io::sink());
    String::from_utf8_lossy(&buffer)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .take(MAX_CHILD_STDERR_LINES)
        .collect::<Vec<_>>()
        .join("; ")
}

/// Unpacks the release tarball and keeps only the dynamic library out of it.
///
/// A release tarball is ~120 MB unpacked, almost all of it headers, `CMake` files and a debug symbol
/// bundle that nothing here loads. It is extracted to a staging directory, the one library is
/// copied out, and the staging directory is deleted -- which also means a tarball whose layout is
/// not what this function expects fails loudly instead of leaving a runtime directory that looks
/// installed.
fn unpack_runtime(
    archive: &Path,
    runtime_directory: &Path,
    platform: Platform,
    progress: Progress<'_>,
) -> Result<()> {
    let staging = runtime_directory.join(".unpack");
    if staging.exists() {
        fs::remove_dir_all(&staging).map_err(io_error("clear runtime staging directory"))?;
    }
    ensure_private_directory(&staging)?;
    progress("unpacking the ONNX Runtime release");
    let mut command = Command::new("tar");
    command.arg("-xzf").arg(archive).arg("-C").arg(&staging);
    let extracted = run_bounded(&mut command, TOOL_TIMEOUT);
    let located =
        extracted.and_then(|()| find_runtime_library(&staging, platform.runtime_library_name()));
    let outcome = match located {
        Ok(source) => {
            let destination = runtime_directory.join(platform.runtime_library_name());
            fs::copy(&source, &destination)
                .map_err(io_error("install ONNX Runtime library"))
                .map(|_| destination)
        }
        Err(error) => Err(error),
    };
    // The staging tree is removed whether or not the copy worked; on failure it would otherwise be
    // 120 MB of evidence for a problem the error message already explains.
    let _ = fs::remove_dir_all(&staging);
    let destination = outcome?;
    progress(&format!("installed {}", destination.display()));
    Ok(())
}

fn find_runtime_library(directory: &Path, name: &str) -> Result<PathBuf> {
    let mut pending = vec![directory.to_path_buf()];
    while let Some(current) = pending.pop() {
        let Ok(listing) = fs::read_dir(&current) else {
            continue;
        };
        for entry in listing.flatten() {
            let path = entry.path();
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            if metadata.is_dir() {
                // The debug symbol bundle is a directory whose leaf has the library's name; it is
                // not a loadable library and must not be mistaken for one.
                if path
                    .extension()
                    .is_some_and(|extension| extension == "dSYM")
                {
                    continue;
                }
                pending.push(path);
            } else if metadata.is_file() && path.file_name().is_some_and(|leaf| leaf == name) {
                return Ok(path);
            }
        }
    }
    Err(Error::new(
        ErrorKind::InvalidInput,
        format!(
            "the ONNX Runtime archive contains no {name}; it is not an ONNX Runtime release \
             tarball for this platform"
        ),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(options: &InstallOptions) -> Result<DownloadPlan> {
        plan_downloads(Path::new("/root"), Platform::DarwinArm64, options)
    }

    #[test]
    fn default_model_sources_lead_with_the_mirror_and_fall_back_to_upstream() {
        let plan = plan(&InstallOptions::default()).unwrap();
        assert_eq!(plan.model_files.len(), 3);
        let onnx = &plan.model_files[0];
        assert_eq!(onnx.name, "model.onnx");
        assert_eq!(
            onnx.sources,
            vec![
                "https://hf-mirror.com/BAAI/bge-m3/resolve/main/onnx/model.onnx".to_owned(),
                "https://huggingface.co/BAAI/bge-m3/resolve/main/onnx/model.onnx".to_owned(),
            ]
        );
        assert_eq!(
            onnx.destination,
            Path::new("/root/embedding/model/model.onnx")
        );
    }

    #[test]
    fn a_custom_model_base_replaces_both_defaults_and_keeps_the_pinned_digests() {
        let plan = plan(&InstallOptions {
            model_url: Some("https://mirror.internal/bge-m3/".to_owned()),
            ..InstallOptions::default()
        })
        .unwrap();
        for (planned, pinned) in plan.model_files.iter().zip(MODEL_FILES.iter()) {
            assert_eq!(
                planned.sources,
                vec![format!("https://mirror.internal/bge-m3/{}", pinned.name)],
                "a named mirror must not silently fall back to the public internet"
            );
            assert_eq!(planned.sha256, pinned.sha256);
            assert_eq!(planned.bytes, pinned.bytes);
        }
    }

    #[test]
    fn the_weights_file_gets_the_long_budget_and_the_rest_do_not() {
        let plan = plan(&InstallOptions::default()).unwrap();
        let weights = plan
            .model_files
            .iter()
            .find(|planned| planned.name == "model.onnx_data")
            .unwrap();
        assert_eq!(weights.timeout, LARGE_DOWNLOAD_TIMEOUT);
        let tokenizer = plan
            .model_files
            .iter()
            .find(|planned| planned.name == "tokenizer.json")
            .unwrap();
        assert_eq!(tokenizer.timeout, SMALL_DOWNLOAD_TIMEOUT);
    }

    #[test]
    fn arm64_downloads_the_pinned_runtime_release() {
        let plan = plan(&InstallOptions::default()).unwrap();
        assert_eq!(plan.runtime_archive.sha256, RUNTIME_ARM64.sha256);
        assert_eq!(plan.runtime_archive.bytes, RUNTIME_ARM64.bytes);
        assert_eq!(
            plan.runtime_archive.sources,
            vec![
                "https://github.com/microsoft/onnxruntime/releases/download/v1.28.1/onnxruntime-osx-arm64-1.28.1.tgz"
                    .to_owned()
            ]
        );
        assert_eq!(
            plan.runtime_library(),
            Path::new("/root/embedding/runtime/libonnxruntime.dylib")
        );
    }

    #[test]
    fn a_mirrored_runtime_for_a_measured_platform_keeps_the_measured_digest() {
        let plan = plan(&InstallOptions {
            runtime_url: Some("file:///tmp/ort.tgz".to_owned()),
            ..InstallOptions::default()
        })
        .unwrap();
        assert_eq!(plan.runtime_archive.sources, vec!["file:///tmp/ort.tgz"]);
        assert_eq!(plan.runtime_archive.sha256, RUNTIME_ARM64.sha256);
    }

    #[test]
    fn an_explicit_digest_overrides_the_pinned_one_and_stops_checking_the_size() {
        let expected = "a".repeat(64);
        let plan = plan(&InstallOptions {
            runtime_url: Some("file:///tmp/ort.tgz".to_owned()),
            expected_runtime_sha256: Some(expected.to_uppercase()),
            ..InstallOptions::default()
        })
        .unwrap();
        assert_eq!(plan.runtime_archive.sha256, expected);
        assert_eq!(plan.runtime_archive.bytes, 0);
    }

    #[test]
    fn x64_has_no_published_runtime_and_says_what_to_supply() {
        let error = plan_downloads(
            Path::new("/root"),
            Platform::DarwinX64,
            &InstallOptions::default(),
        )
        .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        let message = error.to_string();
        assert!(message.contains("--runtime-url"), "{message}");
        assert!(message.contains("--expected-sha256"), "{message}");
    }

    #[test]
    fn x64_installs_from_an_operator_supplied_and_vouched_for_tarball() {
        let expected = "b".repeat(64);
        let plan = plan_downloads(
            Path::new("/root"),
            Platform::DarwinX64,
            &InstallOptions {
                runtime_url: Some("https://mirror.internal/ort-x64.tgz".to_owned()),
                expected_runtime_sha256: Some(expected.clone()),
                ..InstallOptions::default()
            },
        )
        .unwrap();
        assert_eq!(plan.runtime_archive.sha256, expected);
        // The model half is identical on both architectures: only the runtime is per-platform.
        assert_eq!(plan.model_files.len(), 3);
        assert_eq!(plan.model_files[0].sha256, MODEL_FILES[0].sha256);
    }

    #[test]
    fn x64_without_a_digest_is_refused_even_with_a_url() {
        let error = plan_downloads(
            Path::new("/root"),
            Platform::DarwinX64,
            &InstallOptions {
                runtime_url: Some("https://mirror.internal/ort-x64.tgz".to_owned()),
                ..InstallOptions::default()
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("--expected-sha256"));
    }

    #[test]
    fn a_malformed_digest_is_rejected_before_anything_is_downloaded() {
        for supplied in ["", "abc", &"g".repeat(64), &"a".repeat(63)] {
            let error = plan(&InstallOptions {
                runtime_url: Some("file:///tmp/ort.tgz".to_owned()),
                expected_runtime_sha256: Some(supplied.to_owned()),
                ..InstallOptions::default()
            })
            .unwrap_err();
            assert_eq!(error.kind(), ErrorKind::InvalidInput, "{supplied:?}");
            assert!(error.to_string().contains("64 hexadecimal"), "{supplied:?}");
        }
    }

    #[test]
    fn unsupported_platforms_are_told_how_to_configure_retrieval_by_hand() {
        let error = Platform::from_target("linux", "x86_64").unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported);
        let message = error.to_string();
        assert!(message.contains("linux/x86_64"), "{message}");
        assert!(message.contains("embedding_model_path"), "{message}");
        assert!(message.contains("embedding_runtime_path"), "{message}");
    }

    #[test]
    fn supported_platforms_map_to_their_stable_names() {
        assert_eq!(
            Platform::from_target("macos", "aarch64").unwrap(),
            Platform::DarwinArm64
        );
        assert_eq!(
            Platform::from_target("macos", "x86_64").unwrap(),
            Platform::DarwinX64
        );
        assert_eq!(Platform::DarwinArm64.as_str(), "darwin-arm64");
        assert_eq!(Platform::DarwinX64.as_str(), "darwin-x64");
    }

    #[test]
    fn an_unmeasured_size_never_blocks_a_download_but_zero_still_matches_zero() {
        assert!(size_matches(0, 12_345));
        assert!(size_matches(10, 10));
        assert!(!size_matches(10, 11));
    }

    // The tests below exercise the real transfer path against `file://` sources. `curl` treats a
    // local file as an ordinary transfer -- redirects, retries and `--continue-at -` all behave --
    // so the verification, publication and idempotency rules can be proven on kilobytes instead of
    // gigabytes, through exactly the code that will later move the gigabytes.

    fn silent() -> impl FnMut(&str) {
        |_line: &str| {}
    }

    fn source_file(directory: &Path, name: &str, contents: &[u8]) -> (String, String) {
        let path = directory.join(name);
        fs::write(&path, contents).unwrap();
        let digest = format!("{:x}", Sha256::digest(contents));
        (format!("file://{}", path.display()), digest)
    }

    fn planned(
        destination: PathBuf,
        sources: Vec<String>,
        sha256: String,
        bytes: u64,
    ) -> PlannedDownload {
        PlannedDownload {
            name: destination
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            sha256,
            bytes,
            destination,
            sources,
            timeout: TOOL_TIMEOUT,
        }
    }

    #[test]
    fn a_verified_download_lands_under_its_real_name_and_leaves_no_partial() {
        let temporary = tempfile::tempdir().unwrap();
        let (url, digest) = source_file(temporary.path(), "source.bin", b"embedding channel");
        let destination = temporary.path().join("installed.bin");
        let plan = planned(destination.clone(), vec![url.clone()], digest, 17);

        let outcome = fetch_verified(&plan, &mut silent()).unwrap();

        assert!(!outcome.reused);
        assert_eq!(outcome.source.as_deref(), Some(url.as_str()));
        assert_eq!(fs::read(&destination).unwrap(), b"embedding channel");
        assert!(
            !sidecar(&destination, ".part").exists(),
            "a published download must not leave its partial behind"
        );
    }

    #[test]
    fn a_digest_mismatch_publishes_nothing_and_removes_the_partial() {
        let temporary = tempfile::tempdir().unwrap();
        let (url, _) = source_file(temporary.path(), "source.bin", b"the wrong bytes entirely");
        let destination = temporary.path().join("installed.bin");
        let plan = planned(destination.clone(), vec![url], "c".repeat(64), 0);

        let error = fetch_verified(&plan, &mut silent()).unwrap_err();

        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert!(error.to_string().contains("SHA-256"), "{error}");
        assert!(
            !destination.exists(),
            "a file that failed verification must never take the real name"
        );
        assert!(
            !sidecar(&destination, ".part").exists(),
            "a partial known to be wrong would poison every later resume"
        );
    }

    #[test]
    fn a_short_download_is_rejected_on_size_before_its_digest_is_computed() {
        let temporary = tempfile::tempdir().unwrap();
        let (url, digest) = source_file(temporary.path(), "source.bin", b"four");
        let destination = temporary.path().join("installed.bin");
        // The digest is the file's own, so only the pinned size can reject this.
        let plan = planned(destination.clone(), vec![url], digest, 999);

        let error = fetch_verified(&plan, &mut silent()).unwrap_err();

        assert_eq!(error.kind(), ErrorKind::External);
        assert!(error.to_string().contains("expected 999 bytes"), "{error}");
        assert!(!destination.exists());
    }

    #[test]
    fn an_already_verified_file_is_reused_rather_than_downloaded_again() {
        let temporary = tempfile::tempdir().unwrap();
        let (url, digest) = source_file(temporary.path(), "source.bin", b"embedding channel");
        let destination = temporary.path().join("installed.bin");
        let plan = planned(destination.clone(), vec![url], digest, 17);

        let first = fetch_verified(&plan, &mut silent()).unwrap();
        // Removing the source proves the second call never reaches for it.
        fs::remove_file(temporary.path().join("source.bin")).unwrap();
        let second = fetch_verified(&plan, &mut silent()).unwrap();

        assert!(!first.reused);
        assert!(
            second.reused,
            "a verified file must not be transferred twice"
        );
        assert_eq!(second.source, None);
        assert_eq!(second.bytes, 17);
    }

    #[test]
    fn a_present_file_with_the_right_size_and_wrong_content_is_replaced() {
        let temporary = tempfile::tempdir().unwrap();
        let (url, digest) = source_file(temporary.path(), "source.bin", b"embedding channel");
        let destination = temporary.path().join("installed.bin");
        // Same length, different bytes: only the digest can tell these apart.
        fs::write(&destination, b"EMBEDDING CHANNEL").unwrap();
        let plan = planned(destination.clone(), vec![url], digest, 17);

        let outcome = fetch_verified(&plan, &mut silent()).unwrap();

        assert!(!outcome.reused);
        assert_eq!(fs::read(&destination).unwrap(), b"embedding channel");
    }

    #[test]
    fn a_partial_larger_than_the_target_is_discarded_instead_of_resumed() {
        let temporary = tempfile::tempdir().unwrap();
        let (url, digest) = source_file(temporary.path(), "source.bin", b"embedding channel");
        let destination = temporary.path().join("installed.bin");
        let part = sidecar(&destination, ".part");
        fs::write(&part, vec![b'x'; 4096]).unwrap();
        let plan = planned(destination.clone(), vec![url], digest, 17);

        let outcome = fetch_verified(&plan, &mut silent()).unwrap();

        assert!(!outcome.reused);
        assert_eq!(fs::read(&destination).unwrap(), b"embedding channel");
    }

    #[test]
    fn a_dead_first_source_falls_through_to_the_next_one() {
        let temporary = tempfile::tempdir().unwrap();
        let (url, digest) = source_file(temporary.path(), "source.bin", b"embedding channel");
        let destination = temporary.path().join("installed.bin");
        let plan = planned(
            destination.clone(),
            vec![
                format!("file://{}/absent.bin", temporary.path().display()),
                url.clone(),
            ],
            digest,
            17,
        );

        let outcome = fetch_verified(&plan, &mut silent()).unwrap();

        assert_eq!(outcome.source.as_deref(), Some(url.as_str()));
        assert_eq!(fs::read(&destination).unwrap(), b"embedding channel");
    }

    #[test]
    fn every_source_failing_reports_all_of_them() {
        let temporary = tempfile::tempdir().unwrap();
        let destination = temporary.path().join("installed.bin");
        let plan = planned(
            destination.clone(),
            vec![
                format!("file://{}/absent-one.bin", temporary.path().display()),
                format!("file://{}/absent-two.bin", temporary.path().display()),
            ],
            "d".repeat(64),
            0,
        );

        let error = fetch_verified(&plan, &mut silent()).unwrap_err();

        assert_eq!(error.kind(), ErrorKind::External);
        let message = error.to_string();
        assert!(message.contains("absent-one.bin"), "{message}");
        assert!(message.contains("absent-two.bin"), "{message}");
    }

    #[test]
    fn an_archive_without_a_runtime_library_is_refused() {
        let temporary = tempfile::tempdir().unwrap();
        let error = find_runtime_library(temporary.path(), "libonnxruntime.dylib").unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert!(error.to_string().contains("libonnxruntime.dylib"));
    }

    #[test]
    fn the_debug_symbol_bundle_is_never_mistaken_for_the_library() {
        let temporary = tempfile::tempdir().unwrap();
        let bundle = temporary
            .path()
            .join("lib")
            .join("libonnxruntime.dylib.dSYM")
            .join("Contents")
            .join("Resources")
            .join("DWARF");
        fs::create_dir_all(&bundle).unwrap();
        fs::write(bundle.join("libonnxruntime.dylib"), b"debug symbols").unwrap();
        let real = temporary.path().join("lib").join("libonnxruntime.dylib");
        fs::write(&real, b"the library").unwrap();

        let found = find_runtime_library(temporary.path(), "libonnxruntime.dylib").unwrap();

        assert_eq!(found, real);
    }

    #[test]
    fn remove_refuses_without_confirmation_and_touches_nothing() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("root");
        UserConfigStore::initialize(&root).unwrap();
        let embedding = root.join("embedding");
        fs::create_dir_all(embedding.join("model")).unwrap();
        fs::write(embedding.join("model").join("model.onnx"), b"graph").unwrap();

        let error = remove(&root, false, &mut silent()).unwrap_err();

        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert!(error.to_string().contains("--yes"), "{error}");
        assert!(embedding.exists(), "a refused removal must delete nothing");
    }

    #[test]
    fn remove_clears_the_configuration_the_files_and_the_vector_cache() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("root");
        let config = UserConfigStore::initialize(&root).unwrap();
        let model = model_directory(&root);
        let runtime = runtime_directory(&root).join("libonnxruntime.dylib");
        fs::create_dir_all(&model).unwrap();
        fs::create_dir_all(runtime.parent().unwrap()).unwrap();
        fs::write(model.join("model.onnx"), b"graph").unwrap();
        fs::write(&runtime, b"runtime").unwrap();
        config.set_retrieval_embedding(&model, &runtime).unwrap();
        let cache = semantic_cache_path(&root);
        fs::create_dir_all(cache.parent().unwrap()).unwrap();
        fs::write(&cache, b"vectors").unwrap();
        fs::write(sidecar(&cache, "-wal"), b"journal").unwrap();

        let report = remove(&root, true, &mut silent()).unwrap();

        assert!(report.config_cleared);
        assert_eq!(
            UserConfigStore::open_existing(&root)
                .unwrap()
                .retrieval_settings()
                .unwrap(),
            sctx_local_state::RetrievalSettings::default()
        );
        assert!(!root.join("embedding").exists());
        assert!(!cache.exists());
        assert!(!sidecar(&cache, "-wal").exists());
        assert!(report.removed.contains(&root.join("embedding")));
        assert!(report.removed.contains(&cache));
        // The `-shm` sidecar was never created, so it is reported as absent rather than removed.
        assert!(report.absent.contains(&sidecar(&cache, "-shm")));
    }

    #[test]
    fn remove_is_idempotent_on_an_installation_that_never_configured_anything() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("root");
        UserConfigStore::initialize(&root).unwrap();

        let report = remove(&root, true, &mut silent()).unwrap();

        assert!(!report.config_cleared);
        assert!(report.removed.is_empty());
        assert_eq!(report.absent.len(), 4);
    }

    #[test]
    fn status_on_an_unconfigured_installation_is_off_rather_than_broken() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("root");
        UserConfigStore::initialize(&root).unwrap();

        let report = status(&root, false).unwrap();

        assert!(!report.configured);
        assert!(!report.ready);
        assert!(report.files.is_empty());
        assert_eq!(report.embedded_revisions, 0);
        assert_eq!(report.loads, None, "--verify was not asked for");
    }

    #[test]
    fn status_reports_a_configured_channel_whose_files_went_missing() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("root");
        let config = UserConfigStore::initialize(&root).unwrap();
        let model = model_directory(&root);
        let runtime = runtime_directory(&root).join("libonnxruntime.dylib");
        fs::create_dir_all(&model).unwrap();
        fs::create_dir_all(runtime.parent().unwrap()).unwrap();
        fs::write(model.join("model.onnx"), b"graph").unwrap();
        fs::write(&runtime, b"runtime").unwrap();
        config.set_retrieval_embedding(&model, &runtime).unwrap();

        // `tokenizer.json` and `model.onnx_data` were never written.
        let report = status(&root, false).unwrap();

        assert!(report.configured);
        assert!(!report.ready);
        let missing = report
            .files
            .iter()
            .filter(|file| !file.present)
            .map(|file| file.name.clone())
            .collect::<Vec<_>>();
        assert_eq!(missing, vec!["model.onnx_data", "tokenizer.json"]);
        let graph = report
            .files
            .iter()
            .find(|file| file.name == "model.onnx")
            .unwrap();
        assert_eq!(graph.actual_bytes, Some(5));
        assert_eq!(graph.expected_bytes, Some(MODEL_FILES[0].bytes));
    }
}
