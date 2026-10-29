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
        let result = self.update(|info| {
            // Publish process ownership and completion together under info's lock.
            self.phase.store(3, Ordering::Release);
            info.active = false;
            info.state = state.into();
        }, false);
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
        let session = Arc::new(Session { info: Mutex::new(info.clone()), phase: AtomicU8::new(0), stop_requested: AtomicBool::new(false), process: Mutex::new(None), flow: OutputFlow::default(), done: (Mutex::new(false), Condvar::new()), sink: self.inner.sink.clone(), store: self.inner.store.clone() });
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
        let adapter = match self.codex_executable().and_then(|exe| codex::prepare_with_executable(&exe, Path::new(&info.cwd), info.thread_id.as_deref(), on_event)) {
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

#[cfg(test)]
mod lifecycle_tests {
    use super::*;

    fn request() -> StartRequest {
        StartRequest { cwd: std::env::current_dir().unwrap().to_string_lossy().into_owned(), title: "lifetime fixture".into(), project_id: None, project_assigned: false, thread_id: None, cols: 80, rows: 24 }
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
        let mut command = CommandBuilder::new("cmd.exe");
        command.args(["/d", "/q", "/c", "echo PROJECT_MOVE_READY & set /p line= & echo PROJECT_MOVE_DONE"]);
        let started = engine.start_fixture(start, command).unwrap();
        struct Cleanup(Engine, String, u64);
        impl Drop for Cleanup { fn drop(&mut self) { let _ = self.0.stop(&self.1, self.2); } }
        let _cleanup = Cleanup(engine.clone(), started.id.clone(), started.generation);
        let session = engine.lookup(&started.id, started.generation).unwrap();
        let process = session.process.lock().unwrap().clone().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut output = String::new();
        while !output.contains("PROJECT_MOVE_READY") {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            match receiver.recv_timeout(remaining).unwrap() { BackendEvent::Output(chunk) => output.push_str(&chunk.data), _ => {} }
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
        assert!(!exited.active);
        assert_eq!(exited.cwd, started.cwd);
        assert!(exited.project_assigned && exited.project_id.is_none());
    }

    #[test]
    fn extension_work_and_complete_exit_share_one_admission_boundary() {
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
