use crate::{codex::{self, CodexEvent, CodexSession}, flow::{OutputFlow, Utf8Decoder}, process_job::{LaunchGate, ProcessJob}, storage::Store};
use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, io::{Read, Write}, path::{Path, PathBuf}, sync::{atomic::{AtomicBool, AtomicU8, Ordering}, Arc, Condvar, Mutex}, thread, time::Duration};
use uuid::Uuid;

const MAX_ACTIVE_SESSIONS: usize = 32;
const MAX_METADATA_SESSIONS: usize = 512;
const MAX_WRITE_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfo {
    pub id: String,
    pub generation: u64,
    pub title: String,
    pub cwd: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    pub state: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StartRequest {
    pub cwd: String,
    pub title: String,
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
        self.session.stop_requested.store(true, Ordering::Release);
        self.session.flow.cancel();
        let process = self.session.process.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(process) = process {
            let _ = process.terminate();
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
        let job = self.job.lock().map_err(|_| "owned process job lock poisoned")?;
        let job_result = job.as_ref().map(ProcessJob::terminate).unwrap_or(Ok(()));
        let kill_result = self.killer.lock().map_err(|_| "owned process killer lock poisoned")?.kill().map_err(|e| format!("stop owned PTY: {e}"));
        #[cfg(windows)] { let _ = kill_result; job_result }
        #[cfg(not(windows))] { let _ = job_result; kill_result }
    }
}

struct Session {
    info: Mutex<SessionInfo>,
    // phase 0: preparing, 1: live, 2: stopping, 3: reaped/restored.
    // Codex status observations cannot turn a reaped process back into a live one.
    phase: AtomicU8,
    stop_requested: AtomicBool,
    process: Mutex<Option<Arc<ProcessControl>>>,
    flow: OutputFlow,
    done: (Mutex<bool>, Condvar),
    sink: EventSink,
    store: Arc<Mutex<Store>>,
}
impl Session {
    fn info(&self) -> Result<SessionInfo, String> { self.info.lock().map(|i| i.clone()).map_err(|_| "session metadata lock poisoned".into()) }

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
            let snapshot = if let Ok(mut info) = self.info.lock() { info.state = "error".into(); Some(info.clone()) } else { None };
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
        self.phase.store(3, Ordering::Release);
        let result = self.update(|info| info.state = state.into(), false);
        let mut done = self.done.0.lock().map_err(|_| "session completion lock poisoned")?;
        *done = true;
        self.done.1.notify_all();
        result
    }

    fn stop(&self) -> Result<(), String> {
        if self.phase.fetch_update(Ordering::AcqRel, Ordering::Acquire, |phase| if phase < 3 { Some(2) } else { None }).is_err() { return Ok(()); }
        self.stop_requested.store(true, Ordering::Release);
        self.flow.cancel();
        let status_result = self.update(|info| { if self.phase.load(Ordering::Acquire) == 2 { info.state = "stopping".into(); } }, false);
        let process = self.process.lock().map_err(|_| "session process lock poisoned")?.clone();
        if let Some(process) = process { process.terminate()?; }
        let done = self.done.0.lock().map_err(|_| "session completion lock poisoned")?;
        let (_done_guard, timeout) = self.done.1.wait_timeout_while(done, Duration::from_secs(35), |done| !*done).map_err(|_| "session completion lock poisoned")?;
        if timeout.timed_out() { return Err("owned session cleanup has not completed within 35 seconds".into()); }
        status_result
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

impl Engine {
    pub fn new(database: &Path, sink: EventSink) -> Result<Self, String> {
        let store = Arc::new(Mutex::new(Store::open(database)?));
        let saved = store.lock().map_err(|_| "metadata store lock poisoned")?.sessions()?;
        let mut sessions = HashMap::new();
        for info in saved {
            sessions.insert(info.id.clone(), Arc::new(Session { info: Mutex::new(info), phase: AtomicU8::new(3), stop_requested: AtomicBool::new(false), process: Mutex::new(None), flow: OutputFlow::default(), done: (Mutex::new(true), Condvar::new()), sink: sink.clone(), store: store.clone() }));
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
        if sessions.values().filter(|session| session.phase.load(Ordering::Acquire) < 3).count() >= MAX_ACTIVE_SESSIONS { return Err("32 concurrent sessions limit reached".into()); }
        // Bound metadata in memory as well as output. Remove oldest terminal
        // entries only from the live registry; persisted restore metadata survives.
        if sessions.len() >= MAX_METADATA_SESSIONS {
            if let Some(id) = sessions.iter().find(|(_, session)| session.phase.load(Ordering::Acquire) == 3).map(|(id, _)| id.clone()) { sessions.remove(&id); }
            else { return Err("session metadata limit reached".into()); }
        }
        let info = SessionInfo { id: Uuid::new_v4().to_string(), generation: 1, title: request.title.clone(), cwd: cwd.to_string_lossy().into_owned(), thread_id: request.thread_id.clone(), state: "starting".into() };
        {
            let store = self.inner.store.lock().map_err(|_| "metadata store lock poisoned")?;
            store.save_session(&info)?;
            let mut live_ids: Vec<_> = sessions.iter().filter(|(_, session)| session.phase.load(Ordering::Acquire) < 3).map(|(id, _)| id.clone()).collect();
            live_ids.push(info.id.clone());
            store.prune_sessions(&live_ids)?;
        }
        let session = Arc::new(Session { info: Mutex::new(info.clone()), phase: AtomicU8::new(0), stop_requested: AtomicBool::new(false), process: Mutex::new(None), flow: OutputFlow::default(), done: (Mutex::new(false), Condvar::new()), sink: self.inner.sink.clone(), store: self.inner.store.clone() });
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
        let adapter = match codex::prepare(Path::new(&info.cwd), info.thread_id.as_deref(), on_event) {
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
            let process = Arc::new(ProcessControl { master: Mutex::new(Some(pair.master)), writer: Mutex::new(Some(writer)), killer: Mutex::new(killer), job: Mutex::new(Some(job)), adapter: Mutex::new(adapter), _gate: gate });
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
            // Kill any descendants retaining the terminal after their leader exits.
            let job = process.job.lock().unwrap_or_else(|e| e.into_inner()).take();
            drop(job);
            process.close_io();
            let adapter = process.adapter.lock().unwrap_or_else(|e| e.into_inner()).take();
            drop(adapter); // No registry/metadata lock is held across observer join.
            let read_result = reader_thread.join();
            waiting.process.lock().unwrap_or_else(|e| e.into_inner()).take();
            let stopped = waiting.stop_requested.load(Ordering::Acquire);
            let failed = !matches!(waited, Ok(ref status) if status.success()) || !matches!(read_result, Ok(Ok(())));
            let state = if stopped { "stopped" } else if failed { "error" } else { "exited" };
            let _ = waiting.finish(state);
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
        self.inner.store.lock().map_err(|_| "metadata store lock poisoned")?.import_project(&directory)
    }

    pub fn shutdown(&self) -> Result<(), String> {
        self.inner.exiting.store(true, Ordering::Release);
        let sessions: Vec<_> = self.inner.sessions.lock().map_err(|_| "session registry lock poisoned")?.values().cloned().collect();
        // Send every cancellation before waiting, so one slow session cannot
        // prevent the others from receiving their stop signal.
        let mut failures = Vec::new();
        for session in &sessions {
            if session.phase.load(Ordering::Acquire) < 3 {
                session.stop_requested.store(true, Ordering::Release);
                let _ = session.phase.fetch_update(Ordering::AcqRel, Ordering::Acquire, |phase| if phase < 3 { Some(2) } else { None });
                session.flow.cancel();
                let process = match session.process.lock() { Ok(lock) => lock.clone(), Err(_) => { failures.push("session process lock poisoned".into()); continue; } };
                if let Some(process) = process { if let Err(error) = process.terminate() { failures.push(error); } }
            }
        }
        for session in sessions { if let Err(error) = session.stop() { failures.push(error); } }
        if failures.is_empty() { Ok(()) } else { Err(failures.join("; ")) }
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
