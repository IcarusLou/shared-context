use std::{
    ffi::OsString,
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, ExitStatus, Stdio},
    sync::mpsc::{self, Receiver, RecvTimeoutError},
    thread,
    time::{Duration, Instant},
};

use sctx_scenario_contract::{AgentFraming, AgentVendor};
use serde_json::{Value, json};

const MAX_PROCESS_OUTPUT_BYTES: usize = 2 * 1024 * 1024;
const MAX_MCP_FRAME_BYTES: usize = 8 * 1024 * 1024;
const MAX_MCP_HEADER_BYTES: usize = 8 * 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(5);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProcessErrorKind {
    Start,
    Write,
    Exit,
    Timeout,
    Output,
    Protocol,
    Observer,
    Crashed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ProcessError {
    pub kind: ProcessErrorKind,
    pub message: &'static str,
}

impl ProcessError {
    const fn new(kind: ProcessErrorKind, message: &'static str) -> Self {
        Self { kind, message }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct CommandContext {
    pub binary: PathBuf,
    pub home: PathBuf,
    pub temporary: PathBuf,
    pub workspace: PathBuf,
    pub search_path: OsString,
}

impl CommandContext {
    fn command(&self) -> Command {
        let mut command = Command::new(&self.binary);
        command
            .env_clear()
            .env("HOME", &self.home)
            .env("TMPDIR", &self.temporary)
            .env("PATH", &self.search_path)
            .env("SCTX_SCENARIO_NETWORK", "disabled")
            .env("CARGO_NET_OFFLINE", "true")
            .env("GIT_ALLOW_PROTOCOL", "file")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("HTTP_PROXY", "http://127.0.0.1:9")
            .env("HTTPS_PROXY", "http://127.0.0.1:9")
            .env("ALL_PROXY", "http://127.0.0.1:9")
            .env("NO_PROXY", "")
            .env("http_proxy", "http://127.0.0.1:9")
            .env("https_proxy", "http://127.0.0.1:9")
            .env("all_proxy", "http://127.0.0.1:9")
            .env("no_proxy", "")
            .env("CODEX_HOME", self.home.join("model-execution-disabled"))
            .current_dir(&self.workspace);
        command
    }
}

pub(crate) struct OneShotOutput {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
}

pub(crate) fn run_one_shot(
    context: &CommandContext,
    arguments: &[String],
    input: Option<&[u8]>,
    timeout: Duration,
) -> Result<OneShotOutput, ProcessError> {
    let mut child = context
        .command()
        .args(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| ProcessError::new(ProcessErrorKind::Start, "sctx child could not start"))?;
    if let Some(input) = input {
        child
            .stdin
            .as_mut()
            .ok_or_else(|| ProcessError::new(ProcessErrorKind::Write, "sctx stdin is unavailable"))?
            .write_all(input)
            .map_err(|_| ProcessError::new(ProcessErrorKind::Write, "sctx stdin write failed"))?;
    }
    drop(child.stdin.take());
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| ProcessError::new(ProcessErrorKind::Output, "sctx stdout is unavailable"))?;
    let reader = thread::spawn(move || drain_bounded(stdout));
    let status = wait_with_timeout(&mut child, timeout)?;
    let stdout = reader
        .join()
        .map_err(|_| ProcessError::new(ProcessErrorKind::Output, "sctx stdout reader failed"))??;
    Ok(OneShotOutput { status, stdout })
}

fn drain_bounded(mut reader: impl Read) -> Result<Vec<u8>, ProcessError> {
    let mut output = Vec::new();
    let mut buffer = [0_u8; 16 * 1024];
    let mut over_capacity = false;
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|_| ProcessError::new(ProcessErrorKind::Output, "sctx stdout read failed"))?;
        if read == 0 {
            break;
        }
        let remaining = MAX_PROCESS_OUTPUT_BYTES.saturating_sub(output.len());
        output.extend_from_slice(&buffer[..read.min(remaining)]);
        over_capacity |= read > remaining;
    }
    if over_capacity {
        Err(ProcessError::new(
            ProcessErrorKind::Output,
            "sctx stdout exceeded the runner limit",
        ))
    } else {
        Ok(output)
    }
}

fn wait_with_timeout(child: &mut Child, timeout: Duration) -> Result<ExitStatus, ProcessError> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|_| ProcessError::new(ProcessErrorKind::Exit, "sctx child status failed"))?
        {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(ProcessError::new(
                ProcessErrorKind::Timeout,
                "sctx child exceeded the step timeout",
            ));
        }
        thread::sleep(POLL_INTERVAL);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum McpFraming {
    Newline,
    ContentLength,
}

impl McpFraming {
    fn from_agent(framing: AgentFraming) -> Result<Self, ProcessError> {
        match framing {
            AgentFraming::NewlineDelimitedJson => Ok(Self::Newline),
            AgentFraming::ContentLength => Ok(Self::ContentLength),
            AgentFraming::CliJson => Err(ProcessError::new(
                ProcessErrorKind::Protocol,
                "CLI framing cannot carry MCP actions",
            )),
        }
    }
}

pub(crate) enum McpManager {
    NeverStarted {
        context: CommandContext,
        vendor: AgentVendor,
        framing: AgentFraming,
        timeout: Duration,
    },
    Running(McpChild),
    Crashed {
        context: CommandContext,
        vendor: AgentVendor,
        framing: AgentFraming,
        timeout: Duration,
    },
}

impl McpManager {
    pub fn new(
        context: CommandContext,
        vendor: AgentVendor,
        framing: AgentFraming,
        timeout: Duration,
    ) -> Self {
        Self::NeverStarted {
            context,
            vendor,
            framing,
            timeout,
        }
    }

    pub fn call(&mut self, tool: &str, arguments: &Value) -> Result<Value, ProcessError> {
        self.ensure_started()?;
        match self {
            Self::Running(child) => child.call_tool(tool, arguments),
            Self::NeverStarted { .. } => unreachable!(),
            Self::Crashed { .. } => Err(ProcessError::new(
                ProcessErrorKind::Crashed,
                "MCP child requires an explicit restart",
            )),
        }
    }

    pub fn restart(&mut self) -> Result<(), ProcessError> {
        let (context, vendor, framing, timeout) = self.settings();
        if let Self::Running(child) = self {
            child.stop();
        }
        *self = Self::Running(McpChild::spawn(context, vendor, framing, timeout)?);
        Ok(())
    }

    pub fn crash(&mut self) {
        let (context, vendor, framing, timeout) = self.settings();
        if let Self::Running(child) = self {
            child.stop();
        }
        *self = Self::Crashed {
            context,
            vendor,
            framing,
            timeout,
        };
    }

    fn ensure_started(&mut self) -> Result<(), ProcessError> {
        let Self::NeverStarted {
            context,
            vendor,
            framing,
            timeout,
        } = self
        else {
            return Ok(());
        };
        let child = McpChild::spawn(context.clone(), *vendor, *framing, *timeout)?;
        *self = Self::Running(child);
        Ok(())
    }

    fn settings(&self) -> (CommandContext, AgentVendor, AgentFraming, Duration) {
        match self {
            Self::NeverStarted {
                context,
                vendor,
                framing,
                timeout,
            }
            | Self::Crashed {
                context,
                vendor,
                framing,
                timeout,
            } => (context.clone(), *vendor, *framing, *timeout),
            Self::Running(child) => (
                child.context.clone(),
                child.vendor,
                child.public_framing,
                child.timeout,
            ),
        }
    }
}

pub(crate) struct McpChild {
    child: Child,
    stdin: ChildStdin,
    responses: Receiver<Result<Value, ProcessError>>,
    context: CommandContext,
    vendor: AgentVendor,
    public_framing: AgentFraming,
    framing: McpFraming,
    timeout: Duration,
    next_id: u64,
}

impl McpChild {
    fn spawn(
        context: CommandContext,
        vendor: AgentVendor,
        public_framing: AgentFraming,
        timeout: Duration,
    ) -> Result<Self, ProcessError> {
        let framing = McpFraming::from_agent(public_framing)?;
        let client = match vendor {
            AgentVendor::Cursor => "cursor",
            AgentVendor::Codex => "codex",
        };
        let mut child = context
            .command()
            .args(["mcp", "serve", "--client", client])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|_| ProcessError::new(ProcessErrorKind::Start, "MCP child could not start"))?;
        let stdin = child.stdin.take().ok_or_else(|| {
            ProcessError::new(ProcessErrorKind::Write, "MCP stdin is unavailable")
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            ProcessError::new(ProcessErrorKind::Output, "MCP stdout is unavailable")
        })?;
        let (sender, responses) = mpsc::sync_channel(16);
        thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                match read_frame(&mut reader, framing) {
                    Ok(Some(value)) => {
                        if sender.send(Ok(value)).is_err() {
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(error) => {
                        let _ = sender.send(Err(error));
                        break;
                    }
                }
            }
        });
        let mut process = Self {
            child,
            stdin,
            responses,
            context,
            vendor,
            public_framing,
            framing,
            timeout,
            next_id: 1,
        };
        let initialize = json!({
            "jsonrpc": "2.0",
            "id": process.next_id,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "sctx-scenario-runner", "version": "1"}
            }
        });
        let response = process.request(&initialize)?;
        if response
            .get("result")
            .and_then(|result| result.get("protocolVersion"))
            != Some(&Value::String("2024-11-05".to_owned()))
        {
            process.stop();
            return Err(ProcessError::new(
                ProcessErrorKind::Protocol,
                "MCP initialize response is invalid",
            ));
        }
        Ok(process)
    }

    fn call_tool(&mut self, tool: &str, arguments: &Value) -> Result<Value, ProcessError> {
        self.next_id = self.next_id.checked_add(1).ok_or_else(|| {
            ProcessError::new(
                ProcessErrorKind::Protocol,
                "MCP request identity overflowed",
            )
        })?;
        let request = json!({
            "jsonrpc": "2.0",
            "id": self.next_id,
            "method": "tools/call",
            "params": {"name": tool, "arguments": arguments}
        });
        let response = self.request(&request)?;
        if response.get("error").is_some() {
            return Err(ProcessError::new(
                ProcessErrorKind::Protocol,
                "MCP request returned a typed error",
            ));
        }
        let result = response.get("result").ok_or_else(|| {
            ProcessError::new(ProcessErrorKind::Protocol, "MCP result is missing")
        })?;
        if result.get("isError").and_then(Value::as_bool) == Some(true) {
            return Err(ProcessError::new(
                ProcessErrorKind::Protocol,
                "MCP tool returned a typed failure",
            ));
        }
        result.get("structuredContent").cloned().ok_or_else(|| {
            ProcessError::new(
                ProcessErrorKind::Protocol,
                "MCP structured result is missing",
            )
        })
    }

    fn request(&mut self, request: &Value) -> Result<Value, ProcessError> {
        write_frame(&mut self.stdin, request, self.framing)?;
        let response = match self.responses.recv_timeout(self.timeout) {
            Ok(result) => result?,
            Err(RecvTimeoutError::Timeout) => {
                self.stop();
                return Err(ProcessError::new(
                    ProcessErrorKind::Timeout,
                    "MCP response exceeded the step timeout",
                ));
            }
            Err(RecvTimeoutError::Disconnected) => {
                return Err(ProcessError::new(
                    ProcessErrorKind::Exit,
                    "MCP child exited before responding",
                ));
            }
        };
        if response.get("id") != request.get("id") {
            return Err(ProcessError::new(
                ProcessErrorKind::Protocol,
                "MCP response identity does not match its request",
            ));
        }
        Ok(response)
    }

    fn stop(&mut self) {
        let _ = self.stdin.flush();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for McpChild {
    fn drop(&mut self) {
        self.stop();
    }
}

fn write_frame(
    writer: &mut impl Write,
    value: &Value,
    framing: McpFraming,
) -> Result<(), ProcessError> {
    let body = serde_json::to_vec(value).map_err(|_| {
        ProcessError::new(
            ProcessErrorKind::Protocol,
            "MCP request serialization failed",
        )
    })?;
    match framing {
        McpFraming::Newline => {
            writer.write_all(&body).map_err(write_failed)?;
            writer.write_all(b"\n").map_err(write_failed)?;
        }
        McpFraming::ContentLength => {
            writer
                .write_all(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes())
                .map_err(write_failed)?;
            writer.write_all(&body).map_err(write_failed)?;
        }
    }
    writer.flush().map_err(write_failed)
}

fn read_frame(
    reader: &mut impl BufRead,
    framing: McpFraming,
) -> Result<Option<Value>, ProcessError> {
    let body = match framing {
        McpFraming::Newline => {
            let mut body = Vec::new();
            let read = reader
                .take((MAX_MCP_FRAME_BYTES + 1) as u64)
                .read_until(b'\n', &mut body)
                .map_err(read_failed)?;
            if read == 0 {
                return Ok(None);
            }
            if body.len() > MAX_MCP_FRAME_BYTES {
                return Err(frame_too_large());
            }
            if body.last() == Some(&b'\n') {
                body.pop();
                if body.last() == Some(&b'\r') {
                    body.pop();
                }
            }
            body
        }
        McpFraming::ContentLength => {
            let mut header = Vec::new();
            let read = reader
                .take((MAX_MCP_HEADER_BYTES + 1) as u64)
                .read_until(b'\n', &mut header)
                .map_err(read_failed)?;
            if read == 0 {
                return Ok(None);
            }
            if header.len() > MAX_MCP_HEADER_BYTES {
                return Err(frame_too_large());
            }
            let header = std::str::from_utf8(&header).map_err(|_| invalid_frame())?;
            let length = header
                .trim()
                .strip_prefix("Content-Length:")
                .and_then(|value| value.trim().parse::<usize>().ok())
                .filter(|length| *length <= MAX_MCP_FRAME_BYTES)
                .ok_or_else(invalid_frame)?;
            loop {
                let mut line = Vec::new();
                reader
                    .take((MAX_MCP_HEADER_BYTES + 1) as u64)
                    .read_until(b'\n', &mut line)
                    .map_err(read_failed)?;
                if line == b"\r\n" || line == b"\n" {
                    break;
                }
                if line.is_empty() || line.len() > MAX_MCP_HEADER_BYTES {
                    return Err(invalid_frame());
                }
            }
            let mut body = vec![0_u8; length];
            reader.read_exact(&mut body).map_err(read_failed)?;
            body
        }
    };
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(|_| invalid_frame())
}

fn write_failed(_: std::io::Error) -> ProcessError {
    ProcessError::new(ProcessErrorKind::Write, "MCP frame write failed")
}

fn read_failed(_: std::io::Error) -> ProcessError {
    ProcessError::new(ProcessErrorKind::Output, "MCP frame read failed")
}

fn invalid_frame() -> ProcessError {
    ProcessError::new(ProcessErrorKind::Protocol, "MCP response frame is invalid")
}

fn frame_too_large() -> ProcessError {
    ProcessError::new(
        ProcessErrorKind::Output,
        "MCP response frame exceeded the runner limit",
    )
}

pub(crate) fn parse_json_output(output: &OneShotOutput) -> Result<Value, ProcessError> {
    if !output.status.success() {
        return Err(ProcessError::new(
            ProcessErrorKind::Exit,
            "sctx child exited unsuccessfully",
        ));
    }
    serde_json::from_slice(&output.stdout).map_err(|_| {
        ProcessError::new(
            ProcessErrorKind::Protocol,
            "sctx child returned invalid JSON",
        )
    })
}

pub(crate) fn binary_basename(path: &Path) -> Option<String> {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(str::to_ascii_lowercase)
}
