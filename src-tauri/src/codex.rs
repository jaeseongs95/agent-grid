//! Official Codex app-server protocol adapter. User config/auth remain owned by Codex.
use portable_pty::CommandBuilder;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    env, ffi::OsString,
    io::{self, Read, Write},
    net::{Shutdown, TcpListener, TcpStream},
    path::Path,
    process::{Child, ChildStdin, Command, Stdio},
    sync::{atomic::{AtomicBool, Ordering}, mpsc, Arc, Mutex},
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tungstenite::{client::IntoClientRequest, Message, WebSocket};
use uuid::Uuid;
use crate::process_job::{LaunchGate, ProcessJob};

const TOKEN_ENV: &str = "AGENT_GRID_REMOTE_TOKEN";
const RPC_TIMEOUT: Duration = Duration::from_secs(20);
const HELP_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_FRAME: usize = 8 * 1024 * 1024;
const CATALOG_PAGE_SIZE: usize = 50;
const PREVIEW_TEXT_BYTES: usize = 64 * 1024;
static CATALOG_REQUEST: Mutex<()> = Mutex::new(());

#[path = "codex_catalog.rs"]
mod catalog;
pub use catalog::{catalog_list, catalog_list_with_executable, catalog_preview, catalog_preview_with_executable, CatalogPage, CatalogPreview, CatalogQuery};

#[path = "codex_extensions.rs"]
mod extensions;
pub use extensions::{ExtensionMutation, ExtensionsSnapshot};
pub(crate) use extensions::{extension_admission, extensions_list_with_executable, extensions_mutate_with_executable, ExtensionPermit};

#[path = "codex_mobile.rs"]
mod mobile;
pub use mobile::{MobileAction, MobileConnection, MobileReply, MobileStatus, MobilePairing};

#[path = "codex_usage.rs"]
mod usage;
pub use usage::{weekly_usage_with_executable, WeeklyUsageSnapshot};

#[derive(Debug)]
pub enum CodexEvent { ThreadId(String), Status(String) }

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExternalSession {
    pub id: String,
    pub title: String,
    pub cwd: String,
    pub status: String,
    /// Unix milliseconds, represented as a string to preserve precision in clients.
    pub observed_at: String,
}

pub struct CodexSession {
    pub command: CommandBuilder,
    server: Child,
    job: Option<Arc<ProcessJob>>,
    _launch_gate: LaunchGate,
    stopped: Arc<AtomicBool>,
    socket: Option<TcpStream>,
    observer: Option<JoinHandle<()>>,
    mobile: Option<MobileConnection>,
}

impl CodexSession {
    pub fn mobile_connection(&self) -> MobileConnection {
        self.mobile.as_ref().expect("connected sessions have a mobile management handle").clone()
    }
}

impl Drop for CodexSession {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        if let Some(mobile) = &self.mobile { mobile.cancel(); }
        if let Some(socket) = self.socket.take() { let _ = socket.shutdown(Shutdown::Both); }
        if let Some(job) = self.job.take() { let _ = job.terminate(); }
        // kill may report that the owned process already exited; wait still reaps it.
        let _ = self.server.kill();
        let _ = self.server.wait();
        if let Some(observer) = self.observer.take() { let _ = observer.join(); }
    }
}

fn executable() -> Result<OsString, String> {
    crate::codex_settings::resolve(None).executable()
}

fn hidden(command: &mut Command) {
    #[cfg(windows)] {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000); // CREATE_NO_WINDOW, background protocol process only.
    }
    #[cfg(not(windows))] let _ = command;
}

fn server_args(endpoint: &str, verifier: &str, no_remote_control: bool, remote_client_name: Option<&str>) -> Vec<String> {
    let mut args: Vec<String> = ["app-server", "--listen", endpoint, "--ws-auth", "capability-token", "--ws-token-sha256", verifier]
        .into_iter().map(str::to_owned).collect();
    if no_remote_control { args.push("--no-remote-control".into()); }
    if let Some(name) = remote_client_name { args.extend(["--remote-control-client-name".into(), name.into()]); }
    args
}

fn help_supports_option(bytes: &[u8], option: &str) -> Result<bool, String> {
    let help = std::str::from_utf8(bytes).map_err(|e| format!("Codex app-server help is not valid UTF-8: {e}"))?;
    if !help.lines().any(|line| line.trim_start().strip_prefix("Usage:").or_else(|| line.trim_start().strip_prefix("사용법:")).is_some_and(|usage| usage.split_ascii_whitespace().any(|token| token == "app-server"))) {
        return Err("Codex returned unrecognized app-server help; startup capability is unknown".into());
    }
    let options = help.lines().skip_while(|line| line.trim() != "Options:").skip(1)
        .take_while(|line| line.is_empty() || line.starts_with(char::is_whitespace));
    // Clap의 실제 long-option 열은 --help 선언에서 구한다. 더 들여쓴
    // 설명문이나 다른 섹션에 적힌 이름을 지원 옵션으로 오인하지 않는다.
    let column = options.clone().filter_map(|line| {
        if !line.trim_start().starts_with('-') { return None; }
        let position = line.find("--help")?;
        (line[position..].split_ascii_whitespace().next()?.trim_end_matches(',') == "--help").then_some(position)
    }).min().ok_or("Codex returned app-server help without a recognized options section; startup capability is unknown")?;
    Ok(options.into_iter().any(|line| line.trim_start().starts_with('-')
        && line.get(column..).is_some_and(|tail| tail.starts_with("--")
            && tail.split_ascii_whitespace().next().is_some_and(|token| token.trim_end_matches(',') == option))))
}

#[cfg(test)]
fn help_supports_no_remote_control(bytes: &[u8]) -> Result<bool, String> { help_supports_option(bytes, "--no-remote-control") }

fn server_capabilities(exe: &OsString, cwd: &Path) -> Result<(bool, bool), String> {
    // 실행 파일별 캐시를 두지 않아 같은 경로의 교체나 설정 변경도 다음
    // launch에서 실제 도움말로 판별한다. 실패를 미지원으로 삼키지 않는다.
    let args = vec!["app-server".into(), "--help".into()];
    let output = extensions::command_stdout(exe, cwd, &args, HELP_TIMEOUT)
        .map_err(|error| format!("Cannot inspect app-server startup capabilities: {error}"))?;
    Ok((help_supports_option(&output, "--no-remote-control")?, help_supports_option(&output, "--remote-control-client-name")?))
}

fn tui_command(exe: &OsString, cwd: &Path, endpoint: &str, token: &str, id: Option<&str>) -> CommandBuilder {
    let mut command = CommandBuilder::new(exe);
    if let Some(id) = id {
        command.arg("resume");
        command.arg(id);
    }
    command.arg("--remote");
    command.arg(endpoint);
    command.arg("--remote-auth-token-env");
    command.arg(TOKEN_ENV);
    command.arg("--cd");
    command.arg(cwd.as_os_str());
    command.cwd(cwd);
    command.env(TOKEN_ENV, token);
    // No model override: the official TUI discovers its model catalog dynamically.
    command
}

fn initialize() -> Value {
    json!({"clientInfo":{"name":"agent_grid","title":"OpenDIY Desktop","version":env!("CARGO_PKG_VERSION")}})
}

fn response(message: &Value, id: u64) -> Option<Result<Value, String>> {
    if message.get("id") != Some(&json!(id)) { return None; }
    if let Some(error) = message.get("error") {
        return Some(Err(format!("Codex RPC error {}: {}", error["code"], error["message"])));
    }
    Some(message.get("result").cloned().ok_or_else(|| "Codex returned an invalid RPC response".into()))
}

fn timed_out(error: &tungstenite::Error) -> bool {
    matches!(error, tungstenite::Error::Io(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut))
}

struct Rpc<S: Read + Write> {
    ws: WebSocket<S>,
    next_id: u64,
    cancelled: Option<Arc<AtomicBool>>,
}

fn adopt_root(threads: &[Value], cwd: &Path) -> Result<Option<Value>, String> {
    let mut roots = threads.iter().filter(|thread| {
        thread["parentThreadId"].is_null()
            && thread["forkedFromId"].is_null()
            && matches!(thread["source"].as_str(), Some("cli" | "vscode" | "appServer"))
            && thread["threadSource"].as_str().map_or(true, |s| s == "user")
            && thread["sessionId"].as_str() == thread["id"].as_str()
            && thread["id"].as_str().is_some_and(|id| verified_thread(thread, id, cwd).is_ok())
    });
    let root = roots.next().cloned();
    if roots.next().is_some() { return Err("Dedicated Codex server has multiple root threads; active thread is unknown".into()); }
    Ok(root)
}

fn read_state(rpc: &mut Rpc<TcpStream>, selected: Option<&str>, cwd: &Path) -> Result<Option<Value>, String> {
    if let Some(id) = selected {
        let result = rpc.call("thread/read", json!({"threadId":id,"includeTurns":false}))?;
        verified_thread(&result["thread"], id, cwd)?;
        return Ok(Some(result["thread"].clone()));
    }
    let loaded = rpc.call("thread/loaded/list", json!({}))?;
    let ids = loaded["data"].as_array().ok_or("Codex loaded thread list has no data array")?;
    if ids.len() > 100 { return Err("Dedicated Codex server has too many loaded threads to identify its root".into()); }
    let mut threads = Vec::new();
    for id in ids {
        let id = id.as_str().ok_or("Invalid Codex loaded thread ID")?;
        let result = rpc.call("thread/read", json!({"threadId":id,"includeTurns":false}))?;
        threads.push(result["thread"].clone());
    }
    adopt_root(&threads, cwd)
}
impl<S: Read + Write> Rpc<S> {
    fn send(&mut self, value: Value) -> Result<(), String> {
        check_cancelled(self.cancelled.as_ref())?;
        self.ws.send(Message::Text(value.to_string().into())).map_err(|e| format!("Codex transport write failed: {e}"))
    }
    fn read(&mut self) -> Result<Option<Value>, String> {
        match self.ws.read() {
            Ok(Message::Text(text)) => {
                if text.len() > MAX_FRAME { return Err("Codex protocol frame exceeded limit".into()); }
                serde_json::from_str(&text).map(Some).map_err(|e| format!("Invalid Codex JSON: {e}"))
            }
            Ok(Message::Close(_)) => Err("Codex app-server disconnected".into()),
            Ok(_) => Ok(None),
            Err(error) if timed_out(&error) => Ok(None),
            Err(error) => Err(format!("Codex transport read failed: {error}")),
        }
    }
    fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        check_cancelled(self.cancelled.as_ref())?;
        self.next_id += 1;
        let id = self.next_id;
        self.send(json!({"id":id,"method":method,"params":params}))?;
        let deadline = Instant::now() + RPC_TIMEOUT;
        while Instant::now() < deadline {
            check_cancelled(self.cancelled.as_ref())?;
            if let Some(message) = self.read()? {
                if let Some(result) = response(&message, id) { return result; }
            }
        }
        // A lost mutation reply is an unknown result, never a reason to create a fresh thread.
        Err(format!("Codex {method} timed out; its result is unknown"))
    }
}

fn check_cancelled(cancelled: Option<&Arc<AtomicBool>>) -> Result<(), String> {
    if cancelled.is_some_and(|flag| flag.load(Ordering::Acquire)) { Err("Session startup was cancelled".into()) } else { Ok(()) }
}

fn connect(endpoint: &str, token: &str, server: &mut Child, job: &ProcessJob, cancelled: Option<Arc<AtomicBool>>) -> Result<Rpc<TcpStream>, String> {
    let address = endpoint.trim_start_matches("ws://").parse().map_err(|e| format!("Invalid loopback endpoint: {e}"))?;
    let deadline = Instant::now() + Duration::from_secs(12);
    loop {
        check_cancelled(cancelled.as_ref())?;
        if let Some(status) = server.try_wait().map_err(|e| format!("Cannot inspect Codex server: {e}"))? {
            return Err(format!("Codex app-server exited before connecting ({status}); verify the CLI supports authenticated WebSockets"));
        }
        if let Ok(stream) = TcpStream::connect_timeout(&address, Duration::from_millis(300)) {
            // Authenticate the OS peer before constructing or sending a bearer credential.
            // A released ephemeral port may have been claimed by an unrelated listener.
            job.verify_tcp_peer(&stream)
                .map_err(|error| format!("Codex app-server TCP peer ownership verification failed: {error}"))?;
            let mut rpc = authenticated_socket(endpoint, token, stream)?;
            rpc.cancelled = cancelled;
            return Ok(rpc);
        }
        if Instant::now() >= deadline { return Err("Codex app-server did not become ready within 12 seconds".into()); }
        thread::sleep(Duration::from_millis(80));
    }
}

// The caller must verify the connected peer before this sends a credential.
fn authenticated_socket(endpoint: &str, token: &str, stream: TcpStream) -> Result<Rpc<TcpStream>, String> {
    stream.set_read_timeout(Some(Duration::from_millis(500))).map_err(|_| "Cannot set Codex read timeout")?;
    stream.set_write_timeout(Some(Duration::from_secs(5))).map_err(|_| "Cannot set Codex write timeout")?;
    let mut request = endpoint.into_client_request().map_err(|_| "Invalid Codex endpoint")?;
    request.headers_mut().insert("Authorization", format!("Bearer {token}").parse().map_err(|_| "Invalid capability token")?);
    let mut config = tungstenite::protocol::WebSocketConfig::default();
    config.max_message_size = Some(MAX_FRAME);
    config.max_frame_size = Some(MAX_FRAME);
    // Handshake errors can contain the authorization header; never return them.
    let (ws, _) = tungstenite::client::client_with_config(request, stream, Some(config))
        .map_err(|_| "Codex authenticated WebSocket handshake failed".to_string())?;
    Ok(Rpc {ws, next_id:0, cancelled:None})
}

#[cfg(windows)]
pub(crate) fn verify_connect_peer_fixture(endpoint: &str, server: &mut Child, job: &ProcessJob) -> Result<(), String> {
    // Exercise the real connection path: the unrelated fixture listener must receive zero HTTP bytes.
    connect(endpoint, "agent-grid-port-preemption-fixture", server, job, None).map(|_| ())
}

fn verified_thread(thread: &Value, requested: &str, cwd: &Path) -> Result<(), String> {
    if thread["id"].as_str() != Some(requested) { return Err("Codex returned a different thread ID; refusing to resume".into()); }
    let stored = thread["cwd"].as_str().ok_or("Codex thread has no verified working directory")?;
    let stored_path = Path::new(stored).canonicalize().map_err(|e| format!("Cannot verify saved thread directory: {e}"))?;
    if stored_path != cwd { return Err("Saved thread directory differs from the selected directory; refusing to resume".into()); }
    Ok(())
}

fn status(value: &Value) -> String {
    match value["type"].as_str() {
        Some("active") => {
            let flags = value["activeFlags"].as_array();
            if flags.is_some_and(|f| f.iter().any(|v| v == "waitingOnApproval" || v == "waitingOnUserInput")) { "waiting".into() }
            else { "running".into() }
        }
        Some("idle") => "idle".into(),
        Some("systemError") => "error".into(),
        Some("notLoaded") => "notLoaded".into(),
        _ => "unknown".into(),
    }
}

fn event_status(message: &Value, selected: &str) -> Option<String> {
    let params = &message["params"];
    let id = params["threadId"].as_str().or_else(|| params["thread"]["id"].as_str());
    if id != Some(selected) { return None; }
    match message["method"].as_str()? {
        "thread/status/changed" => Some(status(&params["status"])),
        "turn/started" => Some("running".into()),
        "turn/completed" => Some(match params["turn"]["status"].as_str() {
            Some("failed") => "error", Some("completed" | "interrupted") => "idle", _ => "unknown",
        }.into()),
        "thread/closed" => Some("notLoaded".into()),
        _ => None,
    }
}

fn publish_status(previous: &mut Option<String>, state: String, on_event: &dyn Fn(CodexEvent)) {
    if previous.as_ref() != Some(&state) {
        *previous = Some(state.clone());
        on_event(CodexEvent::Status(state));
    }
}

/// Both terminal sessions and short-lived catalog requests use the same owned,
/// authenticated server. Browsing never creates/resumes a conversation.
fn launch_server(exe: &OsString, cwd: &Path, thread_id: Option<&str>) -> Result<(CodexSession, Rpc<TcpStream>), String> {
    launch_server_with_cancellation(exe, cwd, thread_id, None)
}

fn launch_server_with_cancellation(exe: &OsString, cwd: &Path, thread_id: Option<&str>, cancelled: Option<Arc<AtomicBool>>) -> Result<(CodexSession, Rpc<TcpStream>), String> {
    check_cancelled(cancelled.as_ref())?;
    let (no_remote_control, namespaced_remote) = server_capabilities(exe, cwd)?;
    check_cancelled(cancelled.as_ref())?;
    // Opt-in namespace isolates this owned process from all existing servers'
    // enrollment records. Older CLIs remain usable with mobile mutations gated.
    let remote_client_name = namespaced_remote.then(|| format!("opendiy-desktop-{}", Uuid::new_v4().simple()));
    let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
    let endpoint = format!("ws://{}", listener.local_addr().map_err(|e| e.to_string())?);
    drop(listener);
    // Two independent OS-random UUIDs provide 244 unpredictable bits; raw token stays in memory/child environment.
    let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    let verifier = format!("{:x}", Sha256::digest(token.as_bytes()));
    let (mut process, launch_gate) = LaunchGate::wrap_std(exe.as_os_str())?;
    process.args(server_args(&endpoint, &verifier, no_remote_control, remote_client_name.as_deref())).current_dir(&cwd)
        .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    hidden(&mut process);
    let server = process.spawn().map_err(|e| format!("Cannot start Codex app-server: {e}"))?;
    let mut session = CodexSession {
        command: tui_command(exe, cwd, &endpoint, &token, thread_id), server, job:None, _launch_gate:launch_gate,
        stopped:Arc::new(AtomicBool::new(false)), socket:None, observer:None, mobile:None,
    };
    session.job = Some(Arc::new(ProcessJob::assign_std(&session.server)?));
    session.mobile = Some(MobileConnection::new(endpoint.clone(), token.clone(), session.job.as_ref().unwrap(), session.stopped.clone(), namespaced_remote));
    session._launch_gate.release()?;
    let job = session.job.as_ref().ok_or("Codex server has no owned process job")?;
    let mut rpc = connect(&endpoint, &token, &mut session.server, job, cancelled)?;
    rpc.call("initialize", initialize())?;
    rpc.send(json!({"method":"initialized","params":{}}))?;
    Ok((session, rpc))
}

pub fn prepare(cwd: &Path, thread_id: Option<&str>, on_event: Arc<dyn Fn(CodexEvent) + Send + Sync>) -> Result<CodexSession, String> {
    prepare_with_executable(&executable()?, cwd, thread_id, on_event)
}

pub fn prepare_with_executable(exe: &OsString, cwd: &Path, thread_id: Option<&str>, on_event: Arc<dyn Fn(CodexEvent) + Send + Sync>) -> Result<CodexSession, String> {
    prepare_with_cancellation(exe, cwd, thread_id, on_event, Arc::new(AtomicBool::new(false)))
}

pub(crate) fn prepare_with_cancellation(exe: &OsString, cwd: &Path, thread_id: Option<&str>, on_event: Arc<dyn Fn(CodexEvent) + Send + Sync>, cancelled: Arc<AtomicBool>) -> Result<CodexSession, String> {
    let cwd = cwd.canonicalize().map_err(|e| format!("Cannot open working directory: {e}"))?;
    if !cwd.is_dir() { return Err("Working directory must be a directory".into()); }
    if let Some(id) = thread_id { Uuid::parse_str(id).map_err(|_| "Saved Codex thread ID is not a UUID; refusing ambiguous name-based resume")?; }
    let (mut session, mut rpc) = launch_server_with_cancellation(exe, &cwd, thread_id, Some(cancelled))?;
    if let Some(id) = thread_id {
        let existing = rpc.call("thread/read", json!({"threadId":id,"includeTurns":false}))?;
        verified_thread(&existing["thread"], id, &cwd)?;
        on_event(CodexEvent::ThreadId(id.to_owned()));
        on_event(CodexEvent::Status(status(&existing["thread"]["status"])));
    }
    // Fresh TUI owns thread/start; an empty pre-created thread may not have a resumable rollout.
    // The observer uses only read RPCs and never claims the TUI's writer/approval ownership.
    session.socket = Some(rpc.ws.get_ref().try_clone().map_err(|e| e.to_string())?);
    let stopped = session.stopped.clone();
    let mut selected = thread_id.map(str::to_owned);
    session.observer = Some(thread::spawn(move || {
        let mut next_poll = Instant::now();
        let mut last_state: Option<String> = None;
        let mut failures = 0;
        while !stopped.load(Ordering::Acquire) {
            if Instant::now() >= next_poll {
                let snapshot = read_state(&mut rpc, selected.as_deref(), &cwd);
                if stopped.load(Ordering::Acquire) { break; }
                match snapshot {
                    Ok(Some(thread)) => {
                        failures = 0;
                        if selected.is_none() {
                            let Some(id) = thread["id"].as_str().filter(|id| Uuid::parse_str(id).is_ok()) else {
                                publish_status(&mut last_state, "unknown".into(), on_event.as_ref());
                                break;
                            };
                            selected = Some(id.to_owned());
                            on_event(CodexEvent::ThreadId(id.to_owned()));
                        }
                        publish_status(&mut last_state, status(&thread["status"]), on_event.as_ref());
                    }
                    Ok(None) => { failures = 0; }
                    Err(_) => {
                        failures += 1;
                        publish_status(&mut last_state, "unknown".into(), on_event.as_ref());
                        // Only bounded read-only retries; never resume/start from the observer.
                        if failures >= 3 { break; }
                    }
                }
                next_poll = Instant::now() + Duration::from_secs(1);
            }
            let incoming = rpc.read();
            if stopped.load(Ordering::Acquire) { break; }
            match incoming {
                Ok(Some(message)) => if let Some(id) = selected.as_deref() {
                    if let Some(state) = event_status(&message, id) { publish_status(&mut last_state, state, on_event.as_ref()); }
                },
                Ok(None) => {},
                Err(_) => {
                    if !stopped.load(Ordering::Acquire) { publish_status(&mut last_state, "unknown".into(), on_event.as_ref()); }
                    break;
                }
            }
        }
    }));
    Ok(session)
}

/// `app-server proxy` relays raw bytes, including the control socket's WebSocket handshake.
/// A bounded reader bridges the blocking pipe to synchronous tungstenite with read deadlines.
struct ProxyStream {
    input: ChildStdin,
    receiver: Option<mpsc::Receiver<io::Result<Vec<u8>>>>,
    pending: Vec<u8>,
    offset: usize,
    timeout: Duration,
}
impl Read for ProxyStream {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() { return Ok(0); }
        if self.offset == self.pending.len() {
            self.pending = match self.receiver.as_ref().ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "proxy closed"))?.recv_timeout(self.timeout) {
                Ok(result) => result?,
                Err(mpsc::RecvTimeoutError::Timeout) => return Err(io::Error::new(io::ErrorKind::TimedOut, "proxy read timed out")),
                Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(0),
            };
            self.offset = 0;
        }
        let count = output.len().min(self.pending.len() - self.offset);
        output[..count].copy_from_slice(&self.pending[self.offset..self.offset + count]);
        self.offset += count;
        Ok(count)
    }
}
impl Write for ProxyStream {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> { self.input.write(bytes) }
    fn flush(&mut self) -> io::Result<()> { self.input.flush() }
}

/// A short-lived connection to an *existing* official daemon. Never starts/stops it.
struct Proxy {
    child: Child,
    job: Option<ProcessJob>,
    _launch_gate: LaunchGate,
    rpc: Option<Rpc<ProxyStream>>,
    reader: Option<JoinHandle<()>>,
}
impl Drop for Proxy {
    fn drop(&mut self) {
        self.rpc.take(); // Drop stdin/receiver, unblocking a reader waiting to send.
        if let Some(job) = self.job.take() { let _ = job.terminate(); }
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() { let _ = reader.join(); }
    }
}
impl Proxy {
    fn open(exe: &OsString) -> Result<Self, String> {
        let (mut command, launch_gate) = LaunchGate::wrap_std(exe.as_os_str())?;
        command.args(["app-server", "proxy"]).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
        hidden(&mut command);
        let child = command.spawn().map_err(|e| format!("Cannot open existing Codex daemon proxy: {e}"))?;
        let mut proxy = Self {child,job:None,_launch_gate:launch_gate,rpc:None,reader:None};
        proxy.job = Some(ProcessJob::assign_std(&proxy.child)?);
        proxy._launch_gate.release()?;
        let input = proxy.child.stdin.take().expect("piped stdin");
        let mut output = proxy.child.stdout.take().expect("piped stdout");
        let (sender, receiver) = mpsc::sync_channel(32);
        let reader = thread::spawn(move || {
            loop {
                let mut bytes = vec![0; 16 * 1024];
                let value = match output.read(&mut bytes) {
                    Ok(0) => break,
                    Ok(count) => { bytes.truncate(count); Ok(bytes) },
                    Err(error) => Err(error),
                };
                let failed = value.is_err();
                if sender.send(value).is_err() || failed { break; }
            }
        });
        proxy.reader = Some(reader);
        let stream = ProxyStream {input,receiver:Some(receiver),pending:Vec::new(),offset:0,timeout:Duration::from_secs(8)};
        let mut config = tungstenite::protocol::WebSocketConfig::default();
        config.max_message_size = Some(MAX_FRAME);
        config.max_frame_size = Some(MAX_FRAME);
        // The URL supplies HTTP Upgrade metadata only; bytes travel through the official proxy.
        let handshake = tungstenite::client::client_with_config("ws://localhost/", stream, Some(config));
        match handshake {
            Ok((mut ws, _)) => {
                ws.get_mut().timeout = Duration::from_millis(500);
                proxy.rpc = Some(Rpc {ws,next_id:0,cancelled:None});
                Ok(proxy)
            }
            Err(error) => {
                // Drop the interrupted stream/receiver before Proxy joins its reader.
                drop(error);
                Err("Existing Codex daemon WebSocket handshake failed; no daemon was started".into())
            }
        }
    }
    fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.rpc.as_mut().unwrap().call(method, params)
            .map_err(|e| format!("Existing Codex daemon request failed: {e}; no daemon was started"))
    }
}

fn external(thread: &Value, observed_at: &str) -> Result<ExternalSession, String> {
    let id = thread["id"].as_str().ok_or("Codex thread list entry has no ID")?.to_owned();
    let cwd = thread["cwd"].as_str().ok_or("Codex thread list entry has no working directory")?.to_owned();
    let title = thread["name"].as_str().filter(|s| !s.is_empty())
        .or_else(|| thread["preview"].as_str().filter(|s| !s.is_empty())).unwrap_or(&id).to_owned();
    Ok(ExternalSession {id,title,cwd,status:status(&thread["status"]),observed_at:observed_at.into()})
}

pub fn list_external() -> Result<Vec<ExternalSession>, String> {
    list_external_with_executable(&executable()?)
}

pub fn list_external_with_executable(exe: &OsString) -> Result<Vec<ExternalSession>, String> {
    let mut proxy = Proxy::open(exe)?;
    proxy.call("initialize", initialize())?;
    proxy.rpc.as_mut().unwrap().send(json!({"method":"initialized","params":{}}))?;
    let observed = SystemTime::now().duration_since(UNIX_EPOCH).map_err(|e| e.to_string())?.as_millis().to_string();
    let mut sessions = Vec::new();
    let mut cursor: Option<String> = None;
    // Bound discovery work and fail explicitly rather than silently truncating a large history.
    for _ in 0..100 {
        let page = proxy.call("thread/list", json!({"limit":100,"cursor":cursor,"useStateDbOnly":true}))?;
        let data = page["data"].as_array().ok_or("Codex thread/list response has no data array")?;
        for thread in data { sessions.push(external(thread, &observed)?); }
        match page["nextCursor"].as_str() {
            None => return Ok(sessions),
            Some(next) if cursor.as_deref() == Some(next) => return Err("Codex thread/list repeated its pagination cursor".into()),
            Some(next) => cursor = Some(next.to_owned()),
        }
    }
    Err("Codex thread discovery exceeded 100 pages; results were not returned partially".into())
}

#[cfg(test)]
#[path = "codex_startup_tests.rs"]
mod startup_tests;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn resume_rpc_cancellation_interrupts_a_pending_reply_and_blocks_late_send() {
        use std::sync::atomic::AtomicUsize;
        struct PendingReply { cancelled: Arc<AtomicBool>, reads: Arc<AtomicUsize>, writes: Arc<AtomicUsize> }
        impl Read for PendingReply {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                self.reads.fetch_add(1, Ordering::Relaxed);
                self.cancelled.store(true, Ordering::Release);
                Err(io::ErrorKind::WouldBlock.into())
            }
        }
        impl Write for PendingReply {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> { self.writes.fetch_add(1, Ordering::Relaxed); Ok(bytes.len()) }
            fn flush(&mut self) -> io::Result<()> { Ok(()) }
        }
        let cancelled = Arc::new(AtomicBool::new(false));
        let reads = Arc::new(AtomicUsize::new(0));
        let writes = Arc::new(AtomicUsize::new(0));
        let stream = PendingReply { cancelled: cancelled.clone(), reads: reads.clone(), writes: writes.clone() };
        let mut rpc = Rpc { ws: WebSocket::from_raw_socket(stream, tungstenite::protocol::Role::Client, None), next_id:0, cancelled:Some(cancelled) };
        let started = Instant::now();
        assert!(rpc.call("thread/read", json!({"threadId":"fixture"})).unwrap_err().contains("cancelled"));
        assert!(started.elapsed() < Duration::from_secs(1), "cancel must not wait for the 20-second RPC deadline");
        assert_eq!(reads.load(Ordering::Relaxed), 1);
        let sent = writes.load(Ordering::Relaxed);
        assert!(rpc.call("initialize", json!({})).unwrap_err().contains("cancelled"));
        assert_eq!(writes.load(Ordering::Relaxed), sent, "a cancelled startup must not send the next RPC");
    }
    #[test]
    fn rpc_replies_require_exact_numeric_id_and_preserve_errors() {
        assert!(response(&json!({"id":2,"result":{}}), 1).is_none());
        assert!(response(&json!({"id":"1","result":{}}), 1).is_none());
        assert!(response(&json!({"method":"thread/started","params":{}}), 1).is_none());
        assert!(response(&json!({"id":1,"error":{"code":-32602,"message":"missing thread"}}), 1).unwrap().unwrap_err().contains("missing thread"));
    }
    #[test]
    fn notifications_cannot_change_another_threads_status() {
        let event = json!({"method":"thread/status/changed","params":{"threadId":"a","status":{"type":"active","activeFlags":["waitingOnApproval"]}}});
        assert_eq!(event_status(&event, "a").as_deref(), Some("waiting"));
        assert_eq!(event_status(&event, "b"), None);
        assert_eq!(status(&json!({})), "unknown");
        assert_eq!(status(&json!({"type":"notLoaded"})), "notLoaded");
    }
    #[test]
    fn resume_validates_exact_id_and_existing_canonical_directory() {
        let cwd = env::current_dir().unwrap().canonicalize().unwrap();
        let value = json!({"id":"a","cwd":cwd});
        assert!(verified_thread(&value, "a", &cwd).is_ok());
        assert!(verified_thread(&value, "b", &cwd).is_err());
        assert!(verified_thread(&json!({"id":"a"}), "a", &cwd).is_err());
        assert!(verified_thread(&json!({"id":"a","cwd":cwd.join("missing-grid-test-directory")}), "a", &cwd).is_err());
    }
    #[test]
    fn command_keeps_bearer_out_of_argv_and_preserves_exact_resume() {
        let token = "secret-capability";
        let digest = format!("{:x}", Sha256::digest(token.as_bytes()));
        let args = server_args("ws://127.0.0.1:1234", &digest, false, None);
        assert!(!args.iter().any(|a| a.contains(token)));
        assert!(args.contains(&digest));
        assert!(!args.iter().any(|arg| arg == "--no-remote-control"));
        let isolated = server_args("ws://127.0.0.1:1234", &digest, true, None);
        assert_eq!(&isolated[..args.len()], args.as_slice());
        assert_eq!(isolated.last().unwrap(), "--no-remote-control");
        assert!(!isolated.iter().any(|arg| arg.contains(token)));
        let namespaced = server_args("ws://127.0.0.1:1234", &digest, true, Some("owned-process-uuid"));
        assert_eq!(&namespaced[namespaced.len()-2..], ["--remote-control-client-name", "owned-process-uuid"]);
        let cmd = tui_command(&OsString::from("codex"), Path::new("."), "ws://127.0.0.1:1234", token, Some("exact-id"));
        let args: Vec<_> = cmd.get_argv().iter().map(|s| s.to_string_lossy().into_owned()).collect();
        assert_eq!(&args[1..3], ["resume", "exact-id"]);
        assert!(!args.iter().any(|a| a.contains(token)));
        assert!(args.contains(&TOKEN_ENV.into()));
    }
    #[test]
    fn fresh_tui_creates_thread_and_observer_adopts_only_unambiguous_root() {
        let cwd = env::current_dir().unwrap().canonicalize().unwrap();
        let cmd = tui_command(&OsString::from("codex"), &cwd, "ws://127.0.0.1:1234", "token", None);
        assert!(!cmd.get_argv().iter().any(|arg| arg == "resume"));
        // Keys and enum serialization match official v2/thread_data.rs, not the core's snake_case metadata.
        let root = json!({"id":"root","sessionId":"root","cwd":cwd,"source":"vscode","threadSource":null,"parentThreadId":null,"forkedFromId":null});
        let child = json!({"id":"child","sessionId":"root","cwd":cwd,"parentThreadId":"root","source":{"subAgent":"compact"},"threadSource":"subagent"});
        assert_eq!(adopt_root(&[root.clone(), child], &cwd).unwrap().unwrap()["id"], "root");
        assert!(adopt_root(&[], &cwd).unwrap().is_none());
        assert!(adopt_root(&[root.clone(), json!({"id":"other","sessionId":"other","source":"cli","cwd":cwd})], &cwd).is_err());
        assert!(adopt_root(&[json!({"id":"unknown","sessionId":"unknown","cwd":cwd,"source":"unknown"})], &cwd).unwrap().is_none());
        assert!(adopt_root(&[json!({"id":"missing-source","sessionId":"missing-source","cwd":cwd})], &cwd).unwrap().is_none());
    }
    #[test]
    fn status_recovers_after_unknown_observation_and_deduplicates_only_current_state() {
        let updates = std::cell::RefCell::new(Vec::new());
        let sink = |event| if let CodexEvent::Status(state) = event { updates.borrow_mut().push(state); };
        let mut previous = None;
        for state in ["running", "running", "unknown", "running", "idle"] {
            publish_status(&mut previous, state.into(), &sink);
        }
        assert_eq!(*updates.borrow(), ["running", "unknown", "running", "idle"]);
    }
    #[test]
    fn external_status_is_observed_and_missing_status_stays_unknown() {
        let entry = external(&json!({"id":"a","cwd":"/repo","preview":"hello"}), "123").unwrap();
        assert_eq!(entry.status, "unknown");
        assert_eq!(entry.title, "hello");
        assert_eq!(entry.observed_at, "123");
        let serialized = serde_json::to_value(entry).unwrap();
        assert_eq!(serialized["observedAt"], "123");
    }
}
