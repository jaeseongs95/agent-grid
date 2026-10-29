use crate::{codex::{self, CodexEvent, CodexSession}, flow::{OutputFlow, Utf8Decoder}, process_job::{LaunchGate, ProcessJob}, storage::Store};
use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, io::{Read, Write}, path::{Path, PathBuf}, sync::{atomic::{AtomicBool, AtomicU8, Ordering}, Arc, Condvar, Mutex}, thread, time::{Duration, Instant}};
use uuid::Uuid;

pub(crate) const MAX_ACTIVE_SESSIONS: usize = 32;
const MAX_METADATA_SESSIONS: usize = 512;
const MAX_WRITE_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfo {
    pub id: String,
    pub generation: u64,
    pub title: String,
    pub cwd: String,
    #[serde(default)]
    pub project_id: Option<String>,
    #[serde(default)]
    pub project_assigned: bool,
    #[serde(default)]
    pub project_revision: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    pub state: String,
    #[serde(default)]
    pub active: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StartRequest {
    pub cwd: String,
    pub title: String,
    #[serde(default)]
    pub project_id: Option<String>,
    #[serde(default)]
    pub project_assigned: bool,
    pub thread_id: Option<String>,
    pub cols: u16,
    pub rows: u16,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalOutput { pub id: String, pub generation: u64, pub seq: u64, pub data: String }

#[derive(Clone, Debug)]
pub enum BackendEvent { Output(TerminalOutput), Changed(SessionInfo) }
pub type EventSink = Arc<dyn Fn(BackendEvent) -> Result<(), String> + Send + Sync>;

struct ProcessControl {
    // Closing the master must never wait for a writer blocked in the input
    // pipe. ClosePseudoConsole tears down that pipe and wakes the writer.
    master: Mutex<Option<Box<dyn MasterPty + Send>>>,
    writer: Mutex<Option<Box<dyn Write + Send>>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    job: Mutex<Option<ProcessJob>>,
    adapter: Mutex<Option<CodexSession>>,
    termination: Mutex<()>,
    shared_cleanup_failed: AtomicBool,
    shared_stop_attempted: AtomicBool,
    _gate: LaunchGate,
}

/// Child's library handle does not terminate on Drop. Every failed thread spawn
/// must still kill and reap the process rather than detaching it accidentally.
struct OwnedChild { child: Box<dyn portable_pty::Child + Send + Sync>, reaped: bool }
impl OwnedChild {
    fn wait(&mut self) -> std::io::Result<portable_pty::ExitStatus> {
        let result = self.child.wait();
        self.reaped = result.is_ok();
        result
    }
}
impl Drop for OwnedChild {
    fn drop(&mut self) {
        if !self.reaped { let _ = self.child.kill(); let _ = self.child.wait(); }
    }
}

struct LaunchGuard { session: Arc<Session>, armed: bool }
impl Drop for LaunchGuard {
    fn drop(&mut self) {
        if !self.armed { return; }
        // 실패 정리는 사용자 취소 요청과 구분한다. 관측은 정리 중 억제한다.
        let _ = self.session.phase.fetch_update(Ordering::AcqRel, Ordering::Acquire, |phase| if phase < 3 { Some(2) } else { None });
        self.session.flow.cancel();
        let process = self.session.process.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(process) = process {
            let shared_cleanup_failed = process.shared_cleanup_failed.load(Ordering::Acquire);
            let _ = process.terminate();
            self.session.cleanup_failed.fetch_or(shared_cleanup_failed || process.shared_cleanup_failed.load(Ordering::Acquire), Ordering::AcqRel);
            process.close_io();
            let adapter = process.adapter.lock().unwrap_or_else(|e| e.into_inner()).take();
            drop(adapter);
        }
        let _ = self.session.finish("error");
    }
}
impl ProcessControl {
    fn close_io(&self) {
        let master = self.master.lock().unwrap_or_else(|e| e.into_inner()).take();
        drop(master); // Reader keeps draining during ClosePseudoConsole.
        let writer = self.writer.lock().unwrap_or_else(|e| e.into_inner()).take();
        drop(writer);
    }

    fn terminate(&self) -> Result<(), String> {
        self.terminate_until(Instant::now() + Duration::from_secs(8))
    }
    fn terminate_until(&self, deadline: Instant) -> Result<(), String> {
        let _termination = self.termination.lock().map_err(|_| "session termination lock poisoned")?;
        // adapter를 회수하는 PTY reaper와 경합하므로 먼저 해당 세션의 제어 핸들만
        // 복제한다. 외부 공유 서버의 프로세스 수명은 이 핸들이 소유하지 않는다.
        let shared = self.adapter.lock().map_err(|_| "session adapter lock poisoned")?.as_ref().and_then(CodexSession::shared_control);
        let stop = if let Some(shared) = shared {
            if self.shared_stop_attempted.swap(true, Ordering::AcqRel) {
                if self.shared_cleanup_failed.load(Ordering::Acquire) {
                    return Err("공유 작업 중단 결과를 확인할 수 없습니다. 중단 요청을 재전송하지 않았습니다.".into());
                }
                return Ok(());
            }
            self.shared_cleanup_failed.store(true, Ordering::Release);
            shared.capture_stop(deadline).map(Some)
        } else { Ok(None) };
        let job = self.job.lock().map_err(|_| "owned process job lock poisoned")?;
        let job_result = job.as_ref().map(ProcessJob::terminate).unwrap_or(Ok(()));
        let kill_result = self.killer.lock().map_err(|_| "owned process killer lock poisoned")?.kill().map_err(|e| format!("stop owned PTY: {e}"));
        drop(job);
        #[cfg(windows)] let terminated = { let _ = kill_result; job_result };
        #[cfg(not(windows))] let terminated = { let _ = job_result; kill_result };
        terminated?;
        // 원래 turn을 고정한 뒤 자기 TUI를 정리하고 정확한 ID만 중단한다.
        let result = stop.and_then(|stop| stop.map(|stop| stop.finish()).unwrap_or(Ok(())));
        self.shared_cleanup_failed.store(result.is_err(), Ordering::Release);
        result
    }
}

struct Session {
    info: Mutex<SessionInfo>,
    // phase 0: preparing, 1: live, 2: stopping, 3: reaped/restored.
    // Codex status observations cannot turn a reaped process back into a live one.
    phase: AtomicU8,
    stop_requested: Arc<AtomicBool>,
    cleanup_failed: AtomicBool,
    process: Mutex<Option<Arc<ProcessControl>>>,
    flow: OutputFlow,
    done: (Mutex<bool>, Condvar),
    sink: EventSink,
    store: Arc<Mutex<Store>>,
}
impl Session {
    fn info(&self) -> Result<SessionInfo, String> {
        let guard = self.info.lock().map_err(|_| "session metadata lock poisoned")?;
        let mut info = guard.clone();
        if self.phase.load(Ordering::Acquire) == 3 { info.active = false; }
        Ok(info)
    }

    fn update(&self, change: impl FnOnce(&mut SessionInfo), status_only: bool) -> Result<(), String> {
        let snapshot = {
            let mut info = self.info.lock().map_err(|_| "session metadata lock poisoned")?;
            if status_only && self.phase.load(Ordering::Acquire) >= 2 { return Ok(()); }
            change(&mut info);
            self.store.lock().map_err(|_| "metadata store lock poisoned")?.save_session(&info)?;
            info.clone()
        };
        (self.sink)(BackendEvent::Changed(snapshot))
    }

    fn observe(&self, event: CodexEvent) {
        let result = self.update(|info| match event {
            CodexEvent::ThreadId(id) => info.thread_id = Some(id),
            CodexEvent::Status(state) => info.state = state,
        }, true);
        if result.is_err() {
            // Persistence/delivery failure is surfaced as an error state, never
            // reported as successfully persisted metadata. No token or output logged.
            let snapshot = if let Ok(mut info) = self.info.lock() {
                if self.phase.load(Ordering::Acquire) < 2 { info.state = "error".into(); Some(info.clone()) } else { None }
            } else { None };
            if let Some(info) = snapshot { let _ = (self.sink)(BackendEvent::Changed(info)); }
        }
    }

    fn fail_reader(&self) {
        self.flow.cancel();
        let process = self.process.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some(process) = process { let _ = process.terminate(); }
    }

    fn output(&self, data: String) -> Result<bool, String> {
        if data.is_empty() { return Ok(true); }
        let Some(seq) = self.flow.reserve(data.len())? else { return Ok(false); };
        let info = self.info()?;
        (self.sink)(BackendEvent::Output(TerminalOutput { id: info.id, generation: info.generation, seq, data }))?;
        Ok(true)
    }

    fn finish(&self, state: &str) -> Result<(), String> {
        self.finish_reaped(state, false)
    }

    fn finish_reaped(&self, state: &str, shared_unconfirmed: bool) -> Result<(), String> {
        let result = self.update(|info| {
            // adapter 인계 뒤의 취소도 완료와 같은 metadata 락에서 판정한다.
            // TUI 연결 회수는 공유 turn의 중단 확인이 아니다.
            if shared_unconfirmed && self.stop_requested.load(Ordering::Acquire) {
                self.cleanup_failed.store(true, Ordering::Release);
            }
            // Publish process ownership and completion together under info's lock.
            self.phase.store(3, Ordering::Release);
            info.active = false;
            info.state = if self.cleanup_failed.load(Ordering::Acquire) { "unknown" }
                else if self.stop_requested.load(Ordering::Acquire) { "stopped" } else { state }.into();
        }, false);
        let mut done = self.done.0.lock().map_err(|_| "session completion lock poisoned")?;
        *done = true;
        self.done.1.notify_all();
        result
    }

    fn request_stop(&self) -> Result<(), String> {
        if self.phase.load(Ordering::Acquire) == 3 { return Ok(()); }
        let result = self.update(|info| {
            // 완료와 취소의 승자를 메타데이터 락 하나로 정한다.
            if self.phase.load(Ordering::Acquire) < 3 {
                self.stop_requested.store(true, Ordering::Release);
                self.phase.store(2, Ordering::Release);
                info.state = "stopping".into();
            }
        }, false);
        self.flow.cancel();
        result
    }

    fn stop(&self) -> Result<(), String> {
        self.stop_until(Instant::now() + Duration::from_secs(43))
    }
    fn stop_until(&self, deadline: Instant) -> Result<(), String> {
        if self.phase.load(Ordering::Acquire) == 3 {
            return if self.cleanup_failed.load(Ordering::Acquire) { Err("공유 작업 중단 결과가 확인되지 않았습니다. 중단 요청을 재전송하지 않았습니다.".into()) } else { Ok(()) };
        }
        let status_result = self.request_stop();
        let process = self.process.lock().map_err(|_| "session process lock poisoned")?.clone();
        let termination_result = if let Some(process) = process { process.terminate_until(deadline) } else { Ok(()) };
        let done = self.done.0.lock().map_err(|_| "session completion lock poisoned")?;
        let remaining = deadline.saturating_duration_since(Instant::now()).min(Duration::from_secs(35));
        let (_done_guard, timeout) = self.done.1.wait_timeout_while(done, remaining, |done| !*done).map_err(|_| "session completion lock poisoned")?;
        if timeout.timed_out() { return Err("owned session cleanup has not completed within 35 seconds".into()); }
        if self.cleanup_failed.load(Ordering::Acquire) { return Err("공유 작업 중단 결과가 확인되지 않았습니다. 중단 요청을 재전송하지 않았습니다.".into()); }
        termination_result.and(status_result)
    }
}

struct EngineInner {
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    store: Arc<Mutex<Store>>,
    sink: EventSink,
    exiting: AtomicBool,
}

#[derive(Clone)]
pub struct Engine { inner: Arc<EngineInner> }

#[path = "session_exit.rs"]
mod exit;

impl Engine {
    pub fn new(database: &Path, sink: EventSink) -> Result<Self, String> {
        let store = Arc::new(Mutex::new(Store::open(database)?));
        let saved = store.lock().map_err(|_| "metadata store lock poisoned")?.sessions()?;
        let mut sessions = HashMap::new();
        for info in saved {
            sessions.insert(info.id.clone(), Arc::new(Session { info: Mutex::new(info), phase: AtomicU8::new(3), stop_requested: Arc::new(AtomicBool::new(false)), cleanup_failed:AtomicBool::new(false), process: Mutex::new(None), flow: OutputFlow::default(), done: (Mutex::new(true), Condvar::new()), sink: sink.clone(), store: store.clone() }));
        }
        Ok(Self { inner: Arc::new(EngineInner { sessions: Mutex::new(sessions), store, sink, exiting: AtomicBool::new(false) }) })
    }

    fn allocate(&self, request: &StartRequest) -> Result<Arc<Session>, String> {
        validate_size(request.cols, request.rows)?;
        if request.title.len() > 512 || request.title.chars().any(char::is_control) { return Err("title must be at most 512 bytes without control characters".into()); }
        let cwd = PathBuf::from(&request.cwd).canonicalize().map_err(|e| format!("open working directory: {e}"))?;
        if !cwd.is_dir() { return Err("working directory must be a directory".into()); }
        if self.inner.exiting.load(Ordering::Acquire) { return Err("application is exiting".into()); }
        let mut sessions = self.inner.sessions.lock().map_err(|_| "session registry lock poisoned")?;
        if self.inner.exiting.load(Ordering::Acquire) { return Err("application is exiting".into()); }
        if let Some(thread_id) = request.thread_id.as_deref() {
            for session in sessions.values() {
                if session.phase.load(Ordering::Acquire) < 3 && session.info()?.thread_id.as_deref() == Some(thread_id) {
                    return Err("Saved thread already has an active session; wait for its cleanup before resuming".into());
                }
            }
        }
        if sessions.values().filter(|session| session.phase.load(Ordering::Acquire) < 3).count() >= MAX_ACTIVE_SESSIONS { return Err("32 concurrent sessions limit reached".into()); }
        // Bound metadata in memory as well as output. Remove oldest terminal
        // entries only from the live registry; persisted restore metadata survives.
        let retired_id = if sessions.len() >= MAX_METADATA_SESSIONS {
            if let Some(id) = sessions.iter().find(|(_, session)| session.phase.load(Ordering::Acquire) == 3).map(|(id, _)| id.clone()) { Some(id) }
            else { return Err("session metadata limit reached".into()); }
        } else { None };
        let info = SessionInfo { id: Uuid::new_v4().to_string(), generation: 1, title: request.title.clone(), cwd: cwd.to_string_lossy().into_owned(), project_id: request.project_id.clone(), project_assigned: request.project_assigned || request.project_id.is_some(), project_revision: 0, thread_id: request.thread_id.clone(), state: "starting".into(), active: true };
        {
            let store = self.inner.store.lock().map_err(|_| "metadata store lock poisoned")?;
            store.save_session(&info)?;
            let mut live_ids: Vec<_> = sessions.iter().filter(|(_, session)| session.phase.load(Ordering::Acquire) < 3).map(|(id, _)| id.clone()).collect();
            live_ids.push(info.id.clone());
            store.prune_sessions(&live_ids)?;
        }
        let session = Arc::new(Session { info: Mutex::new(info.clone()), phase: AtomicU8::new(0), stop_requested: Arc::new(AtomicBool::new(false)), cleanup_failed:AtomicBool::new(false), process: Mutex::new(None), flow: OutputFlow::default(), done: (Mutex::new(false), Condvar::new()), sink: self.inner.sink.clone(), store: self.inner.store.clone() });
        if let Some(id) = retired_id { sessions.remove(&id); }
        sessions.insert(info.id.clone(), session.clone());
        drop(sessions);
        if let Err(error) = (self.inner.sink)(BackendEvent::Changed(info)) {
            let _ = session.finish("error");
            return Err(error);
        }
        Ok(session)
    }

    pub fn start(&self, request: StartRequest) -> Result<SessionInfo, String> {
        let session = self.allocate(&request)?;
        let weak = Arc::downgrade(&session);
        let on_event = Arc::new(move |event| { if let Some(session) = weak.upgrade() { session.observe(event); } });
        let info = session.info()?;
        let adapter = match self.codex_executable().and_then(|exe| codex::prepare_with_cancellation(&exe, Path::new(&info.cwd), info.thread_id.as_deref(), on_event, session.stop_requested.clone())) {
            Ok(adapter) => adapter,
            Err(error) => { session.finish("error")?; return Err(error); }
        };
        let command = adapter.command.clone();
        self.launch(session, command, Some(adapter), request.cols, request.rows)
    }

    /// Headless fixtures use this same allocation, PTY, flow and cleanup path.
    pub(crate) fn start_fixture(&self, request: StartRequest, command: CommandBuilder) -> Result<SessionInfo, String> {
        let session = self.allocate(&request)?;
        self.launch(session, command, None, request.cols, request.rows)
    }

    fn launch(&self, session: Arc<Session>, mut command: CommandBuilder, adapter: Option<CodexSession>, cols: u16, rows: u16) -> Result<SessionInfo, String> {
        // This PTY is rendered by xterm, even if the GUI's parent inherited
        // TERM=dumb. Scope capability declarations to this child environment.
        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");
        let mut guard = LaunchGuard { session: session.clone(), armed: true };
        let launch = || -> Result<_, String> {
            if session.stop_requested.load(Ordering::Acquire) { return Err("session was stopped before launch".into()); }
            let pair = native_pty_system().openpty(size(cols, rows)).map_err(|e| format!("open native PTY: {e}"))?;
            let reader = pair.master.try_clone_reader().map_err(|e| format!("open PTY reader: {e}"))?;
            let writer = pair.master.take_writer().map_err(|e| format!("open PTY writer: {e}"))?;
            let (command, gate) = LaunchGate::wrap_pty(command)?;
            let raw_child = pair.slave.spawn_command(command).map_err(|e| format!("spawn owned PTY process: {e}"))?;
            let child = OwnedChild { child: raw_child, reaped: false };
            drop(pair.slave);
            let job = match ProcessJob::assign_pty(child.child.as_ref()) {
                Ok(job) => job,
                Err(error) => return Err(error),
            };
            gate.release()?;
            let killer = child.child.clone_killer();
            let process = Arc::new(ProcessControl { master: Mutex::new(Some(pair.master)), writer: Mutex::new(Some(writer)), killer: Mutex::new(killer), job: Mutex::new(Some(job)), adapter: Mutex::new(adapter), termination:Mutex::new(()),shared_cleanup_failed:AtomicBool::new(false),shared_stop_attempted:AtomicBool::new(false), _gate: gate });
            *session.process.lock().map_err(|_| "session process lock poisoned")? = Some(process.clone());
            Ok((child, reader, process))
        };
        let (mut child, mut reader, process) = match launch() {
            Ok(parts) => parts,
            Err(error) => { session.finish(if session.stop_requested.load(Ordering::Acquire) { "stopped" } else { "error" })?; return Err(error); }
        };
        // Installation and stop serialize through the process slot. A stop that
        // won during prepare/openpty must also terminate the newly installed child.
        if session.stop_requested.load(Ordering::Acquire) { process.terminate()?; }
        else {
            session.phase.compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire).ok();
            // Process liveness does not prove the vendor is running a turn.
            // Only an adapter observation may report idle/running/waiting.
            session.update(|info| { if info.state == "starting" { info.state = "unknown".into(); } }, true)?;
        }
        let reading = session.clone();
        let reader_thread = thread::Builder::new().name("pty-output".into()).spawn(move || -> Result<(), String> {
            let mut decoder = Utf8Decoder::default();
            let mut buffer = [0u8; 8192];
            let mut delivering = true;
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(count) => {
                        if delivering {
                            let text = decoder.decode(&buffer[..count], false);
                            match reading.output(text) {
                                Ok(active) => delivering = active,
                                Err(error) => { reading.fail_reader(); return Err(error); }
                            }
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    // Windows ConPTY reports BrokenPipe at normal process exit.
                    Err(error) if matches!(error.kind(), std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::UnexpectedEof) => break,
                    Err(error) => { reading.fail_reader(); return Err(format!("read owned PTY: {error}")); }
                }
            }
            if delivering { if let Err(error) = reading.output(decoder.decode(&[], true)) { reading.fail_reader(); return Err(error); } }
            Ok(())
        }).map_err(|e| format!("start PTY reader: {e}"))?;
        let waiting = session.clone();
        thread::Builder::new().name("pty-reaper".into()).spawn(move || {
            let waited = child.wait();
            // 공유 turn의 중단 판정 전에 PTY 종료를 세션 중단 완료로 발표하지 않는다.
            let termination = process.termination.lock().unwrap_or_else(|e| e.into_inner());
            waiting.cleanup_failed.fetch_or(process.shared_cleanup_failed.load(Ordering::Acquire), Ordering::AcqRel);
            let adapter = process.adapter.lock().unwrap_or_else(|e| e.into_inner()).take();
            let shared = adapter.as_ref().and_then(CodexSession::shared_control).is_some();
            drop(termination);
            // Kill any descendants retaining the terminal after their leader exits.
            let job = process.job.lock().unwrap_or_else(|e| e.into_inner()).take();
            drop(job);
            process.close_io();
            drop(adapter); // No registry/metadata lock is held across observer join.
            let read_result = reader_thread.join();
            let termination = process.termination.lock().unwrap_or_else(|e| e.into_inner());
            waiting.cleanup_failed.fetch_or(process.shared_cleanup_failed.load(Ordering::Acquire), Ordering::AcqRel);
            waiting.process.lock().unwrap_or_else(|e| e.into_inner()).take();
            let stopped = waiting.stop_requested.load(Ordering::Acquire);
            let failed = !matches!(waited, Ok(ref status) if status.success()) || !matches!(read_result, Ok(Ok(())));
            let state = if stopped { "stopped" } else if failed { "error" } else { "exited" };
            let shared_unconfirmed = shared && !process.shared_stop_attempted.load(Ordering::Acquire);
            drop(termination);
            let _ = waiting.finish_reaped(state, shared_unconfirmed);
        }).map_err(|e| format!("start PTY reaper: {e}"))?;
        guard.armed = false;
        session.info()
    }

    fn lookup(&self, id: &str, generation: u64) -> Result<Arc<Session>, String> {
        let session = self.inner.sessions.lock().map_err(|_| "session registry lock poisoned")?.get(id).cloned().ok_or("session not found")?;
        if session.info()?.generation != generation { return Err("stale session generation".into()); }
        Ok(session)
    }

    pub fn list(&self) -> Result<Vec<SessionInfo>, String> {
        let sessions: Vec<_> = self.inner.sessions.lock().map_err(|_| "session registry lock poisoned")?.values().cloned().collect();
        let mut infos = sessions.into_iter().map(|s| s.info()).collect::<Result<Vec<_>, _>>()?;
        infos.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(infos)
    }

    pub fn write(&self, id: &str, generation: u64, data: &str) -> Result<(), String> {
        if data.len() > MAX_WRITE_BYTES { return Err("terminal write exceeds 64 KiB limit".into()); }
        let session = self.lookup(id, generation)?;
        if session.phase.load(Ordering::Acquire) != 1 { return Err("session is not live".into()); }
        let process = session.process.lock().map_err(|_| "session process lock poisoned")?.clone().ok_or("session has no PTY process")?;
        let mut writer = process.writer.lock().map_err(|_| "PTY writer lock poisoned")?;
        if session.phase.load(Ordering::Acquire) != 1 { return Err("session is stopping".into()); }
        writer.as_mut().ok_or("session PTY has exited")?.write_all(data.as_bytes()).map_err(|e| format!("write owned PTY: {e}"))
    }

    pub fn resize(&self, id: &str, generation: u64, cols: u16, rows: u16) -> Result<(), String> {
        validate_size(cols, rows)?;
        let session = self.lookup(id, generation)?;
        if session.phase.load(Ordering::Acquire) != 1 { return Err("session is not live".into()); }
        let process = session.process.lock().map_err(|_| "session process lock poisoned")?.clone().ok_or("session has no PTY process")?;
        let master = process.master.lock().map_err(|_| "PTY master lock poisoned")?;
        master.as_ref().ok_or("session PTY has exited")?.resize(size(cols, rows)).map_err(|e| format!("resize owned PTY: {e}"))
    }

    pub fn stop(&self, id: &str, generation: u64) -> Result<(), String> { self.lookup(id, generation)?.stop() }
    pub fn acknowledge(&self, id: &str, generation: u64, seq: u64) -> Result<(), String> { self.lookup(id, generation)?.flow.acknowledge(seq) }
    pub fn load_layout(&self) -> Result<serde_json::Value, String> { self.inner.store.lock().map_err(|_| "metadata store lock poisoned")?.load_layout() }
    pub fn save_layout(&self, layout: &serde_json::Value) -> Result<(), String> { self.inner.store.lock().map_err(|_| "metadata store lock poisoned")?.save_layout(layout) }

    pub fn list_projects(&self) -> Result<Vec<crate::project::ProjectInfo>, String> {
        self.inner.store.lock().map_err(|_| "metadata store lock poisoned")?.projects()
    }

    pub fn import_project(&self, cwd: &str) -> Result<crate::project::ProjectInfo, String> {
        // Canonicalization may touch a network filesystem. Do it before taking
        // the metadata lock so terminal lifecycle updates remain independent.
        let directory = crate::project::directory(cwd)?;
        let store = self.inner.store.lock().map_err(|_| "metadata store lock poisoned")?;
        self.check_project_admission()?;
        store.import_project(&directory)
    }

    fn check_project_admission(&self) -> Result<(), String> {
        if self.inner.exiting.load(Ordering::Acquire) { return Err("application is exiting".into()); }
        Ok(())
    }

    pub fn create_project(&self, name: &str, cwd: Option<&str>) -> Result<crate::project::ProjectInfo, String> {
        self.check_project_admission()?;
        let directory = cwd.map(crate::project::directory).transpose()?;
        let store = self.inner.store.lock().map_err(|_| "metadata store lock poisoned")?;
        self.check_project_admission()?;
        store.create_project(name, directory.as_ref())
    }

    pub fn rename_project(&self, id: &str, name: &str) -> Result<crate::project::ProjectInfo, String> {
        let store = self.inner.store.lock().map_err(|_| "metadata store lock poisoned")?;
        self.check_project_admission()?;
        store.rename_project(id, name)
    }

    pub fn set_project_favorite(&self, id: &str, favorite: bool) -> Result<crate::project::ProjectInfo, String> {
        let store = self.inner.store.lock().map_err(|_| "metadata store lock poisoned")?;
        self.check_project_admission()?;
        store.set_project_favorite(id, favorite)
    }

    pub fn move_session_to_project(&self, id: &str, generation: u64, project_id: Option<&str>) -> Result<SessionInfo, String> {
        let sessions = self.inner.sessions.lock().map_err(|_| "session registry lock poisoned")?;
        self.check_project_admission()?;
        let session = sessions.get(id).ok_or("session not found")?;
        let mut info = session.info.lock().map_err(|_| "session metadata lock poisoned")?;
        if info.generation != generation { return Err("stale session generation".into()); }
        let mut next = info.clone();
        next.project_id = project_id.map(str::to_owned);
        next.project_assigned = true;
        next.project_revision = crate::project::next_revision(next.project_revision)?;
        self.inner.store.lock().map_err(|_| "metadata store lock poisoned")?.save_session(&next)?;
        *info = next.clone();
        drop(info);
        drop(sessions);
        (self.inner.sink)(BackendEvent::Changed(next.clone())).map_err(|e| format!("Session project saved; notification failed: {e}"))?;
        Ok(next)
    }

    pub fn delete_project(&self, id: &str) -> Result<(), String> {
        // 기존 update 경로와 같은 info→store 순서를 따른다. registry와
        // 모든 info를 잡은 동안 시작·이동·상태 저장이 삭제를 추월하지 못한다.
        let sessions = self.inner.sessions.lock().map_err(|_| "session registry lock poisoned")?;
        self.check_project_admission()?;
        let mut infos = sessions.values().map(|session| session.info.lock().map_err(|_| "session metadata lock poisoned"))
            .collect::<Result<Vec<_>, _>>()?;
        let detached = self.inner.store.lock().map_err(|_| "metadata store lock poisoned")?.delete_project(id)?;
        let mut changed = Vec::new();
        for info in &mut infos {
            if let Some((_, revision)) = detached.iter().find(|(id, _)| id == &info.id) {
                info.project_id = None;
                info.project_assigned = true;
                info.project_revision = *revision;
                changed.push((**info).clone());
            }
        }
        drop(infos);
        drop(sessions);
        let mut failures = Vec::new();
        for info in changed { if let Err(error) = (self.inner.sink)(BackendEvent::Changed(info)) { failures.push(error); } }
        if failures.is_empty() { Ok(()) } else { Err(format!("Project removed; session notifications failed: {}", failures.join("; "))) }
    }

    pub fn get_ui_language(&self) -> Result<crate::ui_language::UiLanguage, String> {
        self.inner.store.lock().map_err(|_| "metadata store lock poisoned")?.ui_language()
    }

    pub fn set_ui_language(&self, language: crate::ui_language::UiLanguage) -> Result<(), String> {
        self.inner.store.lock().map_err(|_| "metadata store lock poisoned")?.set_ui_language(language)
    }

    pub fn get_codex_settings(&self) -> Result<crate::codex_settings::CodexSettings, String> {
        // Read one setting snapshot, then release SQLite before filesystem I/O.
        let saved = self.inner.store.lock().map_err(|_| "metadata store lock poisoned")?.codex_executable()?;
        Ok(crate::codex_settings::resolve(saved.as_deref()))
    }

    pub fn set_codex_executable(&self, path: Option<&str>) -> Result<crate::codex_settings::CodexSettings, String> {
        // Validate before changing the existing value. Cancel and invalid saves
        // cannot discard a working configuration or restart live sessions.
        let selected = path.map(|path| crate::codex_settings::validate(Path::new(path))).transpose()?;
        self.inner.store.lock().map_err(|_| "metadata store lock poisoned")?.set_codex_executable(selected.as_deref())?;
        // Reset is successful even when its environment/PATH fallback is absent.
        // Return that resolution failure as status, never as a failed save.
        Ok(crate::codex_settings::resolve(selected.as_deref()))
    }

    fn codex_executable(&self) -> Result<std::ffi::OsString, String> { self.get_codex_settings()?.executable() }

    pub fn catalog_list(&self, request: codex::CatalogQuery) -> Result<codex::CatalogPage, String> {
        codex::catalog_list_with_executable(&self.codex_executable()?, request)
    }

    pub fn catalog_preview(&self, thread_id: &str, cursor: Option<String>) -> Result<codex::CatalogPreview, String> {
        codex::catalog_preview_with_executable(&self.codex_executable()?, thread_id, cursor)
    }

    fn with_extension_operation<T>(&self, operation: impl FnOnce(&std::ffi::OsString, &codex::ExtensionPermit) -> Result<T, String>) -> Result<T, String> {
        if self.inner.exiting.load(Ordering::Acquire) { return Err("application is exiting; no extension operation was started".into()); }
        // Resolve one executable snapshot without holding the extension permit
        // or a metadata/session lock across admission or vendor I/O.
        let exe = self.codex_executable()?;
        let permit = codex::extension_admission()?;
        // This check and shutdown's exiting.store share the same permit. A
        // pre-admission flag check alone would allow a late mutation to race exit.
        if self.inner.exiting.load(Ordering::Acquire) { return Err("application is exiting; no extension operation was started".into()); }
        operation(&exe, &permit)
    }

    pub fn extensions_list(&self, cwd: Option<String>) -> Result<codex::ExtensionsSnapshot, String> {
        self.with_extension_operation(|exe, permit| codex::extensions_list_with_executable(exe, cwd, permit))
    }

    pub fn extensions_mutate(&self, cwd: Option<String>, request: codex::ExtensionMutation) -> Result<codex::ExtensionsSnapshot, String> {
        self.with_extension_operation(|exe, permit| codex::extensions_mutate_with_executable(exe, cwd, request, permit))
    }

    pub fn weekly_usage(&self) -> Result<codex::WeeklyUsageSnapshot, String> {
        self.with_extension_operation(|exe, _permit| codex::weekly_usage_with_executable(exe))
    }

    pub fn mobile_control(&self, id: &str, generation: u64, action: codex::MobileAction) -> Result<codex::MobileReply, String> {
        // The exit permit serializes management admission, but metadata and PTY
        // locks are released before connecting or waiting for a remote reply.
        let _permit = codex::extension_admission()?;
        if self.inner.exiting.load(Ordering::Acquire) { return Err("application is exiting; mobile management was not started".into()); }
        let session = self.lookup(id, generation)?;
        if session.phase.load(Ordering::Acquire) != 1 || session.stop_requested.load(Ordering::Acquire) {
            return Err("mobile management requires a running session".into());
        }
        let process = session.process.lock().map_err(|_| "session process lock poisoned")?.clone().ok_or("session process is unavailable")?;
        let connection = process.adapter.lock().map_err(|_| "session adapter lock poisoned")?.as_ref().ok_or("session server is unavailable")?.mobile_connection();
        drop(process);
        connection.request(action)
    }

    pub fn list_external(&self) -> Result<Vec<codex::ExternalSession>, String> {
        codex::list_external_with_executable(&self.codex_executable()?)
    }

    pub fn shutdown(&self) -> Result<(), String> {
        // Never interrupt an extension install/config write/import. Refuse exit
        // before changing exiting or stopping PTYs if management still owns the
        // permit. All shutdown callees below only drain terminal resources and
        // must not reacquire this permit. It is released after their cleanup.
        let _extensions = codex::extension_admission().map_err(|error| format!("Cannot exit while extension management is busy: {error}"))?;
        let was_exiting = self.inner.exiting.swap(true, Ordering::AcqRel);
        let result = (|| {
            let sessions: Vec<_> = self.inner.sessions.lock().map_err(|_| "session registry lock poisoned")?.values().cloned().collect();
            // Send every cancellation before waiting, so one slow session cannot
            // prevent the others from receiving their stop signal.
            let mut failures = Vec::new();
            let deadline = Instant::now() + Duration::from_secs(43);
            for session in &sessions {
                if session.phase.load(Ordering::Acquire) < 3 {
                    if let Err(error) = session.request_stop() { failures.push(error); }
                }
            }
            // 활성 세션은 기존 상한 32개뿐이다. 별도 대기열 없이 같은 전체 기한으로
            // 각 소유 TUI와 공유 작업을 정리하여 느린 RPC가 다음 세션의 취소를 막지 않는다.
            thread::scope(|scope| {
                let mut workers = Vec::new();
                for session in sessions {
                    // 완료 세션의 불명도 합산한다. 완료 분기는 원격 요청을 재전송하지 않는다.
                    if session.phase.load(Ordering::Acquire) == 3 {
                        if let Err(error) = session.stop_until(deadline) { failures.push(error); }
                        continue;
                    }
                    match thread::Builder::new().name("session-shutdown".into()).spawn_scoped(scope, move || session.stop_until(deadline)) {
                        Ok(worker) => workers.push(worker),
                        Err(error) => failures.push(format!("세션 정리 작업자를 시작할 수 없습니다: {error}")),
                    }
                }
                for worker in workers {
                    match worker.join() {
                        Ok(Ok(())) => {},
                        Ok(Err(error)) => failures.push(error),
                        Err(_) => failures.push("세션 정리 작업자가 비정상 종료되었습니다.".into()),
                    }
                }
            });
            if failures.is_empty() { Ok(()) } else { Err(failures.join("; ")) }
        })();
        // 같은 admission permit 안에서 복원하므로 다른 shutdown의 성공을
        // 실패한 호출이 덮거나 새 세션이 정리 목록에서 빠질 수 없다.
        if result.is_err() { self.inner.exiting.store(was_exiting, Ordering::Release); }
        result
    }

    pub(crate) fn wait_for_exit(&self, id: &str, generation: u64, timeout: Duration) -> Result<SessionInfo, String> {
        let session = self.lookup(id, generation)?;
        let done = session.done.0.lock().map_err(|_| "session completion lock poisoned")?;
        let (_done_guard, result) = session.done.1.wait_timeout_while(done, timeout, |done| !*done).map_err(|_| "session completion lock poisoned")?;
        if result.timed_out() { return Err("fixture session did not exit before timeout".into()); }
        session.info()
    }
}

fn size(cols: u16, rows: u16) -> PtySize { PtySize { cols, rows, pixel_width: 0, pixel_height: 0 } }
fn validate_size(cols: u16, rows: u16) -> Result<(), String> {
    if !(2..=1000).contains(&cols) || !(1..=1000).contains(&rows) { Err("terminal size must be 2..1000 columns and 1..1000 rows".into()) } else { Ok(()) }
}

#[cfg(all(test, windows))]
#[path = "session_launch_guard_tests.rs"]
mod launch_guard_tests;

#[cfg(all(test, windows))]
#[path = "session_shutdown_race_tests.rs"]
mod shutdown_race_tests;

#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    #[test]
    fn unconfirmed_shared_stop_finishes_as_unknown_and_a_retry_never_reports_success() {
        let engine = Engine::new(Path::new(":memory:"), Arc::new(|_| Ok(()))).unwrap();
        let session = engine.allocate(&request()).unwrap();
        session.request_stop().unwrap();
        session.cleanup_failed.store(true, Ordering::Release);
        session.finish("stopped").unwrap();
        let info = session.info().unwrap();
        assert!(!info.active);
        assert_eq!(info.state, "unknown", "PTY 정리만으로 공유 작업 중단 완료를 보고하면 안 된다");
        assert!(session.stop().unwrap_err().contains("재전송하지 않았습니다"));
    }
    // 서로 다른 Engine도 같은 전역 확장 관리 입장권을 사용한다.
    pub(super) static EXTENSION_ADMISSION_TEST: Mutex<()> = Mutex::new(());

    fn request() -> StartRequest {
        StartRequest { cwd: std::env::current_dir().unwrap().to_string_lossy().into_owned(), title: "lifetime fixture".into(), project_id: None, project_assigned: false, thread_id: None, cols: 80, rows: 24 }
    }

    #[test]
    fn resume_admission_has_one_owner_through_error_and_stopping() {
        let engine = Engine::new(Path::new(":memory:"), Arc::new(|_| Ok(()))).unwrap();
        let mut request = request();
        request.thread_id = Some(Uuid::new_v4().to_string());
        let request = Arc::new(request);
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let workers: Vec<_> = (0..2).map(|_| {
            let engine = engine.clone(); let request = request.clone(); let barrier = barrier.clone();
            thread::spawn(move || { barrier.wait(); engine.allocate(&request) })
        }).collect();
        barrier.wait();
        let results: Vec<_> = workers.into_iter().map(|worker| worker.join().unwrap()).collect();
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1, "one saved thread must have one native owner");
        let owner = results.into_iter().find_map(Result::ok).unwrap();
        owner.phase.store(1, Ordering::Release);
        owner.observe(CodexEvent::Status("error".into()));
        assert!(engine.allocate(&request).is_err(), "observation error does not release ownership");
        owner.phase.store(2, Ordering::Release);
        assert!(engine.allocate(&request).is_err(), "cleanup must finish before another resume");
        let info = owner.info().unwrap();
        assert!(engine.stop(&info.id, info.generation + 1).unwrap_err().contains("stale"));
        assert!(!owner.stop_requested.load(Ordering::Acquire));
        owner.finish("stopped").unwrap();
        let next = engine.allocate(&request).unwrap();
        assert_ne!(next.info().unwrap().id, info.id);
        next.finish("stopped").unwrap();
    }

    #[test]
    fn resume_cancel_during_prepare_preserves_stopped_after_start_failure() {
        let (entered, entry) = std::sync::mpsc::sync_channel(0);
        let release = Arc::new(std::sync::Barrier::new(2));
        let sink_release = release.clone();
        let engine = Engine::new(Path::new(":memory:"), Arc::new(move |event| {
            if let BackendEvent::Changed(info) = event {
                if info.state == "starting" { entered.send(info).unwrap(); sink_release.wait(); }
            }
            Ok(())
        })).unwrap();
        let missing = std::env::temp_dir().join(format!("missing-opendiy-resume-{}.exe", Uuid::new_v4()));
        engine.inner.store.lock().unwrap().set_codex_executable(Some(missing.to_str().unwrap())).unwrap();
        let starting = engine.clone();
        let worker = thread::spawn(move || starting.start(request()));
        let info = entry.recv_timeout(Duration::from_secs(5)).unwrap();
        let session = engine.lookup(&info.id, info.generation).unwrap();
        let stopping = engine.clone();
        let stop = thread::spawn(move || stopping.stop(&info.id, info.generation));
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !session.stop_requested.load(Ordering::Acquire) {
            assert!(std::time::Instant::now() < deadline);
            thread::yield_now();
        }
        release.wait();
        assert!(worker.join().unwrap().is_err());
        stop.join().unwrap().unwrap();
        assert_eq!(session.info().unwrap().state, "stopped");
        assert!(!session.info().unwrap().active);
        assert!(session.process.lock().unwrap().is_none());
    }

    #[cfg(windows)]
    #[test]
    fn resume_observation_error_keeps_real_pty_stoppable_until_reaped() {
        let engine = Engine::new(Path::new(":memory:"), Arc::new(|_| Ok(()))).unwrap();
        let shell = PathBuf::from(std::env::var_os("SystemRoot").unwrap()).join("System32/WindowsPowerShell/v1.0/powershell.exe");
        let mut command = CommandBuilder::new(shell);
        command.args(["-NoLogo", "-NoProfile", "-NonInteractive", "-Command", "Start-Sleep -Seconds 30"]);
        let started = engine.start_fixture(request(), command).unwrap();
        let session = engine.lookup(&started.id, started.generation).unwrap();
        session.observe(CodexEvent::Status("error".into()));
        assert!(session.info().unwrap().active);
        assert!(session.process.lock().unwrap().is_some());
        let start = std::time::Instant::now();
        engine.stop(&started.id, started.generation).unwrap();
        assert!(start.elapsed() < Duration::from_secs(5), "stop must wake the real PTY reader and reap its process");
        let finished = session.info().unwrap();
        assert_eq!(finished.state, "stopped");
        assert!(!finished.active);
        assert!(session.process.lock().unwrap().is_none());
        assert!(*session.done.0.lock().unwrap());
        engine.stop(&started.id, started.generation).unwrap();
    }

    #[test]
    fn resume_launch_guard_failure_reports_error_without_user_cancel() {
        let engine = Engine::new(Path::new(":memory:"), Arc::new(|_| Ok(()))).unwrap();
        let allocated = engine.allocate(&request()).unwrap();
        drop(LaunchGuard { session: allocated, armed: true });
        let failed = engine.list().unwrap().pop().unwrap();
        assert_eq!(failed.state, "error");
        assert!(!failed.active);
        let session = engine.lookup(&failed.id, failed.generation).unwrap();
        assert!(!session.stop_requested.load(Ordering::Acquire));
        assert!(session.process.lock().unwrap().is_none());
        assert!(*session.done.0.lock().unwrap());
    }

    #[test]
    fn resume_cancel_and_finish_share_the_same_metadata_lock() {
        let engine = Engine::new(Path::new(":memory:"), Arc::new(|_| Ok(()))).unwrap();
        let session = engine.allocate(&request()).unwrap();
        let metadata = session.info.lock().unwrap();
        let (entered, entry) = std::sync::mpsc::sync_channel(0);
        let stopping = session.clone();
        let worker = thread::spawn(move || { entered.send(()).unwrap(); stopping.request_stop() });
        entry.recv_timeout(Duration::from_secs(5)).unwrap();
        // 완료가 이 락을 소유한 동안 취소가 phase/flag를 먼저 발행하면 안 된다.
        let deadline = std::time::Instant::now() + Duration::from_millis(200);
        while !session.stop_requested.load(Ordering::Acquire) && std::time::Instant::now() < deadline { thread::yield_now(); }
        assert_eq!(session.phase.load(Ordering::Acquire), 0);
        assert!(!session.stop_requested.load(Ordering::Acquire));
        drop(metadata);
        worker.join().unwrap().unwrap();
        assert_eq!(session.phase.load(Ordering::Acquire), 2);
        session.finish("error").unwrap();
        assert_eq!(session.info().unwrap().state, "stopped");
        let finished = engine.allocate(&request()).unwrap();
        finished.finish("exited").unwrap();
        finished.request_stop().unwrap();
        assert_eq!(finished.info().unwrap().state, "exited");
        assert!(!finished.stop_requested.load(Ordering::Acquire));
    }

    #[test]
    fn project_moves_check_generation_and_deletion_preserves_live_session_ownership() {
        let (sender, receiver) = std::sync::mpsc::channel();
        let engine = Engine::new(Path::new(":memory:"), Arc::new(move |event| sender.send(event).map_err(|e| e.to_string()))).unwrap();
        let project = engine.create_project("명시 소속", None).unwrap();
        let mut start = request();
        start.project_id = Some(project.id.clone());
        let session = engine.allocate(&start).unwrap();
        let before = session.info().unwrap();
        assert_eq!(before.project_id.as_deref(), Some(project.id.as_str()));
        assert!(before.project_assigned && before.active);
        assert!(engine.move_session_to_project(&before.id, before.generation + 1, None).unwrap_err().contains("stale"));
        assert!(engine.move_session_to_project(&before.id, before.generation, Some("missing")).is_err());
        assert_eq!(session.info().unwrap().project_id, before.project_id);
        assert_eq!(engine.inner.store.lock().unwrap().sessions().unwrap()[0].project_id, before.project_id);
        let unassigned = engine.move_session_to_project(&before.id, before.generation, None).unwrap();
        assert!(unassigned.project_assigned && unassigned.project_id.is_none());
        engine.move_session_to_project(&before.id, before.generation, Some(&project.id)).unwrap();
        engine.delete_project(&project.id).unwrap();
        let after = session.info().unwrap();
        assert!(after.project_assigned && after.project_id.is_none() && after.active);
        assert_eq!(after.cwd, before.cwd);
        assert_eq!(after.generation, before.generation);
        assert_eq!(after.state, before.state);
        assert_eq!(session.phase.load(Ordering::Acquire), 0);
        assert!(!session.stop_requested.load(Ordering::Acquire));
        assert!(!*session.done.0.lock().unwrap());
        assert!(engine.list_projects().unwrap().is_empty());
        assert!(receiver.try_iter().any(|event| matches!(event, BackendEvent::Changed(info) if info.project_assigned && info.project_id.is_none())));
        assert!(engine.allocate(&start).is_err());
        assert_eq!(engine.list().unwrap().len(), 1);
        session.finish("exited").unwrap();
    }

    #[test]
    fn deleting_automatic_folder_assignment_does_not_reclassify_an_explicit_unassigned_session() {
        let _admission_test = EXTENSION_ADMISSION_TEST.lock().unwrap();
        let engine = Engine::new(Path::new(":memory:"), Arc::new(|_| Ok(()))).unwrap();
        let project = engine.import_project(&request().cwd).unwrap();
        let first = engine.allocate(&request()).unwrap();
        assert!(!first.info().unwrap().project_assigned);
        engine.delete_project(&project.id).unwrap();
        let detached = first.info().unwrap();
        assert!(detached.project_assigned && detached.project_id.is_none());
        let reimported = engine.import_project(&request().cwd).unwrap();
        assert_ne!(reimported.id, project.id);
        assert!(first.info().unwrap().project_assigned);
        assert!(first.info().unwrap().project_id.is_none());
        let persisted = engine.inner.store.lock().unwrap().sessions().unwrap();
        assert!(persisted[0].project_assigned && persisted[0].project_id.is_none());
        first.finish("exited").unwrap();
        engine.shutdown().unwrap();
        assert!(engine.create_project("종료 뒤 생성", None).is_err());
        assert!(engine.rename_project(&reimported.id, "종료 뒤 변경").is_err());
        assert!(engine.set_project_favorite(&reimported.id, true).is_err());
        assert!(engine.delete_project(&reimported.id).is_err());
        assert!(engine.move_session_to_project(&detached.id, detached.generation, None).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn project_changes_keep_the_same_running_pty_and_input_until_its_normal_exit() {
        let (sender, receiver) = std::sync::mpsc::channel();
        let engine = Engine::new(Path::new(":memory:"), Arc::new(move |event| sender.send(event).map_err(|e| e.to_string()))).unwrap();
        let project = engine.create_project("실제 터미널 시험", None).unwrap();
        let mut start = request();
        start.project_id = Some(project.id.clone());
        let shell = std::path::PathBuf::from(std::env::var_os("SystemRoot").unwrap()).join("System32/WindowsPowerShell/v1.0/powershell.exe");
        let mut command = CommandBuilder::new(shell);
        command.args(["-NoLogo", "-NoProfile", "-NonInteractive", "-Command", "[Console]::WriteLine('PROJECT_MOVE_READY'); $line = [Console]::ReadLine(); if ($line -ne 'keep-running') { exit 3 }; [Console]::WriteLine('PROJECT_MOVE_DONE')"]);
        let started = engine.start_fixture(start, command).unwrap();
        struct Cleanup(Engine, String, u64);
        impl Drop for Cleanup { fn drop(&mut self) { let _ = self.0.stop(&self.1, self.2); } }
        let _cleanup = Cleanup(engine.clone(), started.id.clone(), started.generation);
        let session = engine.lookup(&started.id, started.generation).unwrap();
        let process = session.process.lock().unwrap().clone().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut output = String::new();
        let mut answered_queries = 0;
        while !output.contains("PROJECT_MOVE_READY") {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            match receiver.recv_timeout(remaining).unwrap_or_else(|error| panic!("readiness receive failed: {error}; state={:?}; output={output:?}", session.info())) {
                BackendEvent::Output(chunk) => {
                    engine.acknowledge(&chunk.id, chunk.generation, chunk.seq).unwrap();
                    output.push_str(&chunk.data);
                    // 실제 xterm처럼 ConPTY의 커서 위치 질의에 응답한다.
                    // 누적 문자열로 검사해 프레임 경계에 걸친 질의도 놓치지 않는다.
                    let queries = output.matches("\x1b[6n").count();
                    while answered_queries < queries {
                        engine.write(&started.id, started.generation, "\x1b[1;1R").unwrap();
                        answered_queries += 1;
                    }
                }
                BackendEvent::Changed(info) => assert!(info.active, "fixture ended before readiness: {info:?}; output={output:?}"),
            }
        }
        let moved = engine.move_session_to_project(&started.id, started.generation, None).unwrap();
        assert!(moved.active && moved.project_assigned && moved.project_id.is_none());
        engine.move_session_to_project(&started.id, started.generation, Some(&project.id)).unwrap();
        engine.delete_project(&project.id).unwrap();
        assert!(Arc::ptr_eq(&process, session.process.lock().unwrap().as_ref().unwrap()));
        assert_eq!(session.phase.load(Ordering::Acquire), 1);
        assert!(!session.stop_requested.load(Ordering::Acquire));
        engine.write(&started.id, started.generation, "keep-running\r").unwrap();
        let exited = engine.wait_for_exit(&started.id, started.generation, Duration::from_secs(10)).unwrap();
        assert_eq!(exited.state, "exited");
        for event in receiver.try_iter() { if let BackendEvent::Output(chunk) = event { output.push_str(&chunk.data); } }
        assert!(output.contains("PROJECT_MOVE_DONE"), "stdin fixture did not complete: {output:?}");
        assert!(!exited.active);
        assert_eq!(exited.cwd, started.cwd);
        assert!(exited.project_assigned && exited.project_id.is_none());
    }

    #[test]
    fn extension_work_and_complete_exit_share_one_admission_boundary() {
        let _admission_test = EXTENSION_ADMISSION_TEST.lock().unwrap();
        let engine = Engine::new(Path::new(":memory:"), Arc::new(|_| Ok(()))).unwrap();
        engine.set_codex_executable(Some(std::env::current_exe().unwrap().to_str().unwrap())).unwrap();
        let root = std::env::temp_dir().join(format!("agent-grid-extension-exit-test-{}", Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let marker = root.join("committed.txt");
        let (entered, entry) = std::sync::mpsc::sync_channel(0);
        let (release, released) = std::sync::mpsc::sync_channel(0);
        let worker_engine = engine.clone();
        let worker_marker = marker.clone();
        let worker = thread::spawn(move || worker_engine.with_extension_operation(|_, _permit| {
            entered.send(()).map_err(|e| e.to_string())?;
            released.recv_timeout(Duration::from_secs(5)).map_err(|e| e.to_string())?;
            // The same production entry wrapper covers CLI commands, staging
            // cleanup and readback. Keep it held through this visible commit.
            std::fs::write(worker_marker, "mutation committed").map_err(|e| e.to_string())
        }));
        entry.recv_timeout(Duration::from_secs(5)).unwrap();
        let started = std::time::Instant::now();
        let error = engine.shutdown().unwrap_err();
        assert!(error.contains("extension management is busy"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(2), "exit must refuse instead of waiting on a mutation");
        assert!(!engine.inner.exiting.load(Ordering::Acquire), "refused exit must not shut the engine down");
        assert!(!marker.exists());
        assert!(engine.extensions_list(None).unwrap_err().contains("in progress"), "second entry must not accumulate a worker queue");
        release.send(()).unwrap();
        worker.join().unwrap().unwrap();
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), "mutation committed");

        // Hold a terminal registry lock to observe shutdown after it has won
        // admission but before its cleanup returns. Late extension work must
        // never enter, either while the permit is held or after it is released.
        let registry = engine.inner.sessions.lock().unwrap();
        let closing = engine.clone();
        let shutdown = thread::spawn(move || closing.shutdown());
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !engine.inner.exiting.load(Ordering::Acquire) {
            assert!(std::time::Instant::now() < deadline, "shutdown must win admission after the mutation settles");
            thread::sleep(Duration::from_millis(2));
        }
        assert!(engine.extensions_mutate(None, codex::ExtensionMutation::RemovePlugin { plugin_id: "sample@fixture".into() }).unwrap_err().contains("application is exiting"));
        assert!(codex::extension_admission().err().unwrap().contains("in progress"), "shutdown must own admission through terminal cleanup");
        drop(registry);
        shutdown.join().unwrap().unwrap();
        assert!(engine.extensions_list(None).unwrap_err().contains("application is exiting"));
        engine.shutdown().unwrap(); // A retry does not recursively reacquire the permit.
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn active_ownership_survives_observation_errors_and_stopping_until_finish() {
        let (sender, receiver) = std::sync::mpsc::channel();
        let engine = Engine::new(Path::new(":memory:"), Arc::new(move |event| { sender.send(event).map_err(|e| e.to_string()) })).unwrap();
        let session = engine.allocate(&request()).unwrap();
        assert_eq!(session.phase.load(Ordering::Acquire), 0);
        assert!(session.info().unwrap().active);
        assert_eq!(session.info().unwrap().state, "starting");
        session.phase.store(1, Ordering::Release); // Adapter observation with a live owner.
        session.observe(CodexEvent::Status("error".into()));
        assert_eq!(session.phase.load(Ordering::Acquire), 1);
        assert!(session.info().unwrap().active);
        assert_eq!(session.info().unwrap().state, "error");

        let stopping = session.clone();
        let stopper = thread::spawn(move || stopping.stop());
        loop {
            if let BackendEvent::Changed(info) = receiver.recv_timeout(Duration::from_secs(5)).unwrap() {
                if info.state == "stopping" { assert!(info.active); break; }
            }
        }
        assert_eq!(session.phase.load(Ordering::Acquire), 2);
        assert!(session.info().unwrap().active);
        session.observe(CodexEvent::Status("idle".into()));
        assert_eq!(session.info().unwrap().state, "stopping");
        session.finish("stopped").unwrap();
        stopper.join().unwrap().unwrap();
        assert_eq!(session.phase.load(Ordering::Acquire), 3);
        assert!(!session.info().unwrap().active);
        session.observe(CodexEvent::Status("error".into()));
        assert_eq!(session.info().unwrap().state, "stopped");
        assert!(!engine.list().unwrap()[0].active);
    }

    #[test]
    fn failed_start_releases_ownership_and_reaped_projection_is_inactive() {
        let engine = Engine::new(Path::new(":memory:"), Arc::new(|_| Ok(()))).unwrap();
        let missing = std::env::temp_dir().join(format!("missing-agent-grid-{}.exe", Uuid::new_v4()));
        engine.inner.store.lock().unwrap().set_codex_executable(Some(missing.to_str().unwrap())).unwrap();
        assert!(engine.start(request()).is_err());
        let failed = engine.list().unwrap().pop().unwrap();
        assert_eq!(failed.state, "error");
        assert!(!failed.active);
        assert_eq!(engine.lookup(&failed.id, failed.generation).unwrap().phase.load(Ordering::Acquire), 3);
        let preparing = engine.allocate(&request()).unwrap();
        assert!(preparing.info().unwrap().active);
        preparing.phase.store(3, Ordering::Release);
        assert!(!preparing.info().unwrap().active);
        preparing.finish("error").unwrap();
    }

    #[test]
    fn persisted_active_metadata_restores_with_no_process_ownership() {
        let directory = std::env::temp_dir().join(format!("agent-grid-lifetime-{}", Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let database = directory.join("workspace.sqlite3");
        let id = {
            let engine = Engine::new(&database, Arc::new(|_| Ok(()))).unwrap();
            let preparing = engine.allocate(&request()).unwrap();
            assert!(preparing.info().unwrap().active);
            preparing.info().unwrap().id
        };
        let restored = Engine::new(&database, Arc::new(|_| Ok(()))).unwrap();
        let session = restored.lookup(&id, 1).unwrap();
        assert_eq!(session.phase.load(Ordering::Acquire), 3);
        assert_eq!(session.info().unwrap().state, "restored");
        assert!(!session.info().unwrap().active);
        assert!(session.process.lock().unwrap().is_none());
        assert!(*session.done.0.lock().unwrap());
        drop(session);
        drop(restored);
        for file in ["workspace.sqlite3-wal", "workspace.sqlite3-shm", "workspace.sqlite3"] {
            let path = directory.join(file);
            if path.exists() { std::fs::remove_file(path).unwrap(); }
        }
        std::fs::remove_dir(directory).unwrap();
    }
}

#[cfg(all(test, windows))]
mod settings_tests {
    use super::*;
    #[test]
    fn invalid_executable_save_preserves_value_and_reset_reports_status() {
        let directory = std::env::temp_dir().join(format!("agent-grid-setting-engine-{}", Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let database = directory.join("workspace.sqlite3");
        let engine = Engine::new(&database, Arc::new(|_| Ok(()))).unwrap();
        let executable = std::env::current_exe().unwrap();
        let saved = engine.set_codex_executable(Some(executable.to_str().unwrap())).unwrap();
        assert_eq!(saved.source, "saved");
        let admitted = engine.codex_executable().unwrap();
        assert!(engine.set_codex_executable(Some("relative.cmd")).is_err());
        assert_eq!(engine.get_codex_settings().unwrap().saved_path, saved.saved_path);
        assert_eq!(engine.codex_executable().unwrap(), admitted);
        let reset = engine.set_codex_executable(None).unwrap();
        assert!(reset.saved_path.is_none());
        // A missing fallback is status data; resetting a key still succeeded.
        if reset.resolved_path.is_none() { assert!(reset.error.is_some()); }
        assert_eq!(admitted, saved.executable().unwrap());
        drop(engine);
        let reopened = Engine::new(&database, Arc::new(|_| Ok(()))).unwrap();
        assert!(reopened.get_codex_settings().unwrap().saved_path.is_none());
        drop(reopened);
        for file in ["workspace.sqlite3-wal", "workspace.sqlite3-shm", "workspace.sqlite3"] {
            let path = directory.join(file);
            if path.exists() { std::fs::remove_file(path).unwrap(); }
        }
        std::fs::remove_dir(directory).unwrap();
    }
}
