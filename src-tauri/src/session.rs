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
    #[cfg(windows)]
    cron: Mutex<Option<codex::cron_control::CronConnection>>,
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
    #[cfg(windows)]
    fn cancel_cron(&self) {
        if let Some(connection) = self.cron.lock().unwrap_or_else(|e|e.into_inner()).as_ref() { connection.cancel(); }
    }

    #[cfg(windows)]
    fn finish_cron(&self,deadline: Instant) -> Result<(),String> {
        let connection=self.cron.lock().map_err(|_|"예약 제어 종료 상태 오류")?.take();
        if let Some(connection)=connection {connection.cancel();connection.wait_until(deadline)?;}
        Ok(())
    }

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
        #[cfg(windows)] self.cancel_cron();
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
        #[cfg(windows)] self.finish_cron(deadline)?;
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
    // 프로세스가 정리돼도 같은 세대의 결과 불명을 확정 미적용으로 바꾸지 않는다.
    // 복원 메타데이터(None)는 이전 실행의 영수증 유무를 증명하지 않는다.
    subagent_receipts: Mutex<Option<HashMap<String, SubagentReceipt>>>,
    flow: OutputFlow,
    done: (Mutex<bool>, Condvar),
    sink: EventSink,
    store: Arc<Mutex<Store>>,
    attachments: crate::conversation_attachments::ConversationAttachments,
}
struct SubagentReceipt { digest: [u8;32], status: &'static str }
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
        drop(done);
        let id = self.info()?.id;
        result.and(self.attachments.cleanup_session(&id))
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

#[derive(Default)]
struct RuntimeSelection {
    generation: u64,
    preparation: u64,
    executable: Option<std::ffi::OsString>,
    ready: bool,
    preparing: bool,
    changing: bool,
}

#[derive(Clone, Debug)]
struct RuntimeSnapshot { generation: u64, executable: std::ffi::OsString }
#[derive(Debug)]
struct PreparedRuntime { selection: RuntimeSnapshot, preparation: u64 }

struct EngineInner {
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    store: Arc<Mutex<Store>>,
    sink: EventSink,
    exiting: Arc<AtomicBool>,
    attachments: crate::conversation_attachments::ConversationAttachments,
    runtime: Mutex<RuntimeSelection>,
}

#[derive(Clone)]
pub struct Engine { inner: Arc<EngineInner> }

#[path = "session_exit.rs"]
mod exit;

impl Engine {
    pub fn new(database: &Path, sink: EventSink) -> Result<Self, String> {
        let store = Arc::new(Mutex::new(Store::open(database)?));
        let saved = store.lock().map_err(|_| "metadata store lock poisoned")?.sessions()?;
        let attachments = crate::conversation_attachments::ConversationAttachments::default();
        let mut sessions = HashMap::new();
        for info in saved {
            sessions.insert(info.id.clone(), Arc::new(Session { info: Mutex::new(info), phase: AtomicU8::new(3), stop_requested: Arc::new(AtomicBool::new(false)), cleanup_failed:AtomicBool::new(false), process: Mutex::new(None), subagent_receipts: Mutex::new(None), flow: OutputFlow::default(), done: (Mutex::new(true), Condvar::new()), sink: sink.clone(), store: store.clone(), attachments:attachments.clone() }));
        }
        Ok(Self { inner: Arc::new(EngineInner { sessions: Mutex::new(sessions), store, sink, exiting: Arc::new(AtomicBool::new(false)), attachments, runtime: Mutex::new(RuntimeSelection::default()) }) })
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
            if let Some(id) = sessions.iter().find(|(_, session)| session.phase.load(Ordering::Acquire) == 3 && !session.cleanup_failed.load(Ordering::Acquire)).map(|(id, _)| id.clone()) { Some(id) }
            else { return Err("session metadata limit reached".into()); }
        } else { None };
        let info = SessionInfo { id: Uuid::new_v4().to_string(), generation: 1, title: request.title.clone(), cwd: cwd.to_string_lossy().into_owned(), project_id: request.project_id.clone(), project_assigned: request.project_assigned || request.project_id.is_some(), project_revision: 0, thread_id: request.thread_id.clone(), state: "starting".into(), active: true };
        {
            let store = self.inner.store.lock().map_err(|_| "metadata store lock poisoned")?;
            store.save_session(&info)?;
            // 완료된 원격 중단 불명도 보존한다. 상한 정리는 사용자 동의가 아니다.
            let mut live_ids: Vec<_> = sessions.iter().filter(|(_, session)| session.phase.load(Ordering::Acquire) < 3 || session.cleanup_failed.load(Ordering::Acquire)).map(|(id, _)| id.clone()).collect();
            live_ids.push(info.id.clone());
            store.prune_sessions(&live_ids)?;
        }
        let session = Arc::new(Session { info: Mutex::new(info.clone()), phase: AtomicU8::new(0), stop_requested: Arc::new(AtomicBool::new(false)), cleanup_failed:AtomicBool::new(false), process: Mutex::new(None), subagent_receipts: Mutex::new(Some(HashMap::new())), flow: OutputFlow::default(), done: (Mutex::new(false), Condvar::new()), sink: self.inner.sink.clone(), store: self.inner.store.clone(),attachments:self.inner.attachments.clone() });
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
        let adapter = match self.runtime_snapshot().and_then(|selected| self.prepare_start_with(selected, &session.stop_requested,
            |exe| codex::prepare_runtime_with_executable(exe, &session.stop_requested),
            |exe| codex::prepare_with_cancellation(exe, Path::new(&info.cwd), info.thread_id.as_deref(), on_event, session.stop_requested.clone()))) {
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
            #[cfg(windows)]
            let cron = if adapter.is_some() {
                let info=session.info()?;
                codex::cron_control::CronPreparation::prepare(&mut command,&info.id,info.generation,session.stop_requested.clone())?
            } else {None};
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
            let killer = child.child.clone_killer();
            let process = Arc::new(ProcessControl { master: Mutex::new(Some(pair.master)), writer: Mutex::new(Some(writer)), killer: Mutex::new(killer), job: Mutex::new(Some(job)), adapter: Mutex::new(adapter),
                #[cfg(windows)] cron:Mutex::new(None),
                termination:Mutex::new(()),shared_cleanup_failed:AtomicBool::new(false),shared_stop_attempted:AtomicBool::new(false), _gate: gate });
            *session.process.lock().map_err(|_| "session process lock poisoned")? = Some(process.clone());
            #[cfg(windows)]
            if let Some(cron)=cron {
                let weak=Arc::downgrade(&process);
                let cancelled=session.stop_requested.clone();
                let verify=Arc::new(move |pipe: &mut crate::owned_tui_control::OwnedTuiPipe| {
                    if cancelled.load(Ordering::Acquire) {return Err("종료 중인 세션의 제어 연결을 허용하지 않았습니다.".into());}
                    let process=weak.upgrade().ok_or("예약 대상 소유 프로세스가 종료됐습니다.")?;
                    let job=process.job.lock().map_err(|_|"예약 대상 소유 Job 상태 오류")?;
                    pipe.verify_peer(job.as_ref().ok_or("예약 대상 소유 Job이 종료됐습니다.")?).map_err(|_|"예약 제어 상대의 소유 신원을 확인하지 못했습니다.".into())
                });
                *process.cron.lock().map_err(|_|"예약 제어 설치 상태 오류")?=Some(cron.start(verify)?);
            }
            // 소유 Job·제어 연결·정리 경로를 모두 설치한 뒤 도우미를 실행한다.
            process._gate.release()?;
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
            #[cfg(windows)] {
                if process.finish_cron(Instant::now()+Duration::from_secs(5)).is_err() {waiting.cleanup_failed.store(true,Ordering::Release);}
            }
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
        let conversation = process.adapter.lock().map_err(|_| "session adapter lock poisoned")?.as_ref().and_then(CodexSession::conversation_connection);
        let write = || {
            let mut writer = process.writer.lock().map_err(|_| "PTY writer lock poisoned")?;
            if session.phase.load(Ordering::Acquire) != 1 { return Err("session is stopping".into()); }
            writer.as_mut().ok_or("session PTY has exited")?.write_all(data.as_bytes()).map_err(|e| format!("write owned PTY: {e}"))
        };
        if let Some(conversation) = conversation { conversation.terminal_write(write) } else { write() }
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
        self.change_runtime_selection_with(|| self.inner.store.lock().map_err(|_| "metadata store lock poisoned")?.set_codex_executable(selected.as_deref()))?;
        // Reset is successful even when its environment/PATH fallback is absent.
        // Return that resolution failure as status, never as a failed save.
        Ok(crate::codex_settings::resolve(selected.as_deref()))
    }

    fn codex_executable(&self) -> Result<std::ffi::OsString, String> { self.get_codex_settings()?.executable() }

    fn change_runtime_selection_with(&self, save: impl FnOnce() -> Result<(), String>) -> Result<(), String> {
        {
            let mut runtime = self.inner.runtime.lock().map_err(|_| "실행파일 선택 잠금이 손상되었습니다.")?;
            if runtime.changing { return Err("실행파일 설정을 저장 중입니다. 잠시 후 다시 시도하세요.".into()); }
            if runtime.generation == u64::MAX { return Err("실행파일 선택 세대 한도에 도달했습니다.".into()); }
            runtime.changing = true;
        }
        // 저장소 쓰기와 파일 확인은 선택 잠금 밖에서 수행한다. 실패한 저장은 캐시를 보존한다.
        let result = save();
        let mut runtime = self.inner.runtime.lock().map_err(|_| "실행파일 선택 잠금이 손상되었습니다.")?;
        if result.is_ok() {
            runtime.generation += 1;
            runtime.executable = None;
            runtime.ready = false;
            runtime.preparing = false;
        }
        runtime.changing = false;
        result
    }

    fn runtime_snapshot(&self) -> Result<RuntimeSnapshot, String> { self.runtime_snapshot_with(|| self.codex_executable()) }

    fn runtime_snapshot_with(&self, resolve: impl FnOnce() -> Result<std::ffi::OsString, String>) -> Result<RuntimeSnapshot, String> {
        let generation = {
            let runtime = self.inner.runtime.lock().map_err(|_| "실행파일 선택 잠금이 손상되었습니다.")?;
            if runtime.changing { return Err("실행파일 설정을 저장 중입니다. 잠시 후 다시 시도하세요.".into()); }
            runtime.generation
        };
        let executable = resolve()?;
        let mut runtime = self.inner.runtime.lock().map_err(|_| "실행파일 선택 잠금이 손상되었습니다.")?;
        if runtime.changing || runtime.generation != generation { return Err("실행파일 선택이 바뀌었습니다. 다시 시도하세요.".into()); }
        if runtime.executable.as_ref() != Some(&executable) {
            runtime.generation = runtime.generation.checked_add(1).ok_or("실행파일 선택 세대 한도에 도달했습니다.")?;
            runtime.executable = Some(executable.clone());
            runtime.ready = false;
            runtime.preparing = false;
        }
        Ok(RuntimeSnapshot { generation: runtime.generation, executable })
    }

    fn verify_runtime_selection(&self, selected: &RuntimeSnapshot, runtime: &RuntimeSelection, cancelled: &AtomicBool) -> Result<(), String> {
        if self.inner.exiting.load(Ordering::Acquire) || cancelled.load(Ordering::Acquire) { return Err("종료 또는 중단 요청으로 실행파일 준비 결과를 사용하지 않았습니다.".into()); }
        if runtime.changing || runtime.generation != selected.generation || runtime.executable.as_ref() != Some(&selected.executable) { return Err("실행파일 선택이 바뀌었습니다. 다시 준비하세요.".into()); }
        Ok(())
    }

    fn prepare_selected_runtime_with(&self, selected: RuntimeSnapshot, force: bool, cancelled: &AtomicBool, prepare: impl FnOnce(&std::ffi::OsString) -> Result<(), String>) -> Result<PreparedRuntime, String> {
        let preparation = {
            let mut runtime = self.inner.runtime.lock().map_err(|_| "실행파일 선택 잠금이 손상되었습니다.")?;
            self.verify_runtime_selection(&selected, &runtime, cancelled)?;
            if !force && runtime.ready { return Ok(PreparedRuntime { selection: selected, preparation: runtime.preparation }); }
            if !force && runtime.preparing { return Err("선택한 실행파일을 준비 중입니다. 잠시 후 다시 시도하세요.".into()); }
            runtime.preparation = runtime.preparation.checked_add(1).ok_or("실행파일 준비 세대 한도에 도달했습니다.")?;
            runtime.ready = false;
            runtime.preparing = true;
            runtime.preparation
        };
        let result = prepare(&selected.executable);
        let mut runtime = self.inner.runtime.lock().map_err(|_| "실행파일 선택 잠금이 손상되었습니다.")?;
        if runtime.generation == selected.generation && runtime.preparation == preparation { runtime.preparing = false; }
        self.verify_runtime_selection(&selected, &runtime, cancelled)?;
        if runtime.preparation != preparation { return Err("더 최근 실행파일 준비 요청이 있습니다. 이전 결과를 사용하지 않았습니다.".into()); }
        result?;
        runtime.ready = true;
        Ok(PreparedRuntime { selection: selected, preparation })
    }

    fn verify_prepared_runtime(&self, prepared: &PreparedRuntime, cancelled: &AtomicBool) -> Result<(), String> {
        let runtime = self.inner.runtime.lock().map_err(|_| "실행파일 선택 잠금이 손상되었습니다.")?;
        self.verify_runtime_selection(&prepared.selection, &runtime, cancelled)?;
        if !runtime.ready || runtime.preparation != prepared.preparation { return Err("선택한 실행파일의 준비가 완료되지 않았습니다. 다시 준비하세요.".into()); }
        Ok(())
    }

    fn prepare_start_with<T>(&self, selected: RuntimeSnapshot, cancelled: &AtomicBool, prepare: impl FnOnce(&std::ffi::OsString) -> Result<(), String>, connect: impl FnOnce(&std::ffi::OsString) -> Result<T, String>) -> Result<T, String> {
        let prepared = self.prepare_selected_runtime_with(selected, false, cancelled, prepare)?;
        let connection = connect(&prepared.selection.executable)?;
        // 연결을 기다리는 사이 설정 또는 강제 준비가 바뀌었으면 연결을 해제하고 PTY를 만들지 않는다.
        self.verify_prepared_runtime(&prepared, cancelled)?;
        Ok(connection)
    }

    pub fn prepare_runtime(&self) -> Result<(), String> {
        if self.inner.exiting.load(Ordering::Acquire) { return Err("앱이 종료 중이어서 공유 서버를 시작하지 않았습니다.".into()); }
        self.prepare_selected_runtime_with(self.runtime_snapshot()?, true, &self.inner.exiting, |exe| codex::prepare_runtime_with_executable(exe, &self.inner.exiting)).map(|_| ())
    }

    pub fn catalog_list(&self, request: codex::CatalogQuery) -> Result<codex::CatalogPage, String> {
        codex::catalog_list_with_executable(&self.codex_executable()?, request)
    }

    pub fn catalog_preview(&self, thread_id: &str, cursor: Option<String>) -> Result<codex::CatalogPreview, String> {
        codex::catalog_preview_with_executable(&self.codex_executable()?, thread_id, cursor)
    }

    fn with_extension_operation<T>(&self, operation: impl FnOnce(&std::ffi::OsString, &codex::ExtensionPermit) -> Result<T, String>) -> Result<T, String> {
        self.with_extension_operation_ready(false, operation)
    }

    fn with_extension_operation_ready<T>(&self, require_ready: bool, operation: impl FnOnce(&std::ffi::OsString, &codex::ExtensionPermit) -> Result<T, String>) -> Result<T, String> {
        if self.inner.exiting.load(Ordering::Acquire) { return Err("application is exiting; no extension operation was started".into()); }
        // Resolve one executable snapshot without holding the extension permit
        // or a metadata/session lock across admission or vendor I/O.
        let selected = self.runtime_snapshot()?;
        let permit = codex::extension_admission()?;
        // This check and shutdown's exiting.store share the same permit. A
        // pre-admission flag check alone would allow a late mutation to race exit.
        if self.inner.exiting.load(Ordering::Acquire) { return Err("application is exiting; no extension operation was started".into()); }
        {
            let runtime = self.inner.runtime.lock().map_err(|_| "실행파일 선택 잠금이 손상되었습니다.")?;
            self.verify_runtime_selection(&selected, &runtime, &self.inner.exiting)?;
            if require_ready && !runtime.ready { return Err("선택한 실행파일의 준비가 완료되지 않아 사용량을 조회하지 않았습니다.".into()); }
        }
        operation(&selected.executable, &permit)
    }

    pub fn extensions_list(&self, cwd: Option<String>) -> Result<codex::ExtensionsSnapshot, String> {
        self.with_extension_operation(|exe, permit| codex::extensions_list_with_executable(exe, cwd, permit))
    }

    pub fn extensions_mutate(&self, cwd: Option<String>, request: codex::ExtensionMutation) -> Result<codex::ExtensionsSnapshot, String> {
        self.with_extension_operation(|exe, permit| codex::extensions_mutate_with_executable(exe, cwd, request, permit))
    }

    pub fn weekly_usage(&self, manual: bool) -> Result<codex::WeeklyUsageSnapshot, String> {
        self.with_extension_operation_ready(true, |exe, _permit| codex::weekly_usage_with_executable(exe, manual))
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

    pub fn conversation_request(&self, id: &str, generation: u64, request: codex::ConversationRequest) -> Result<serde_json::Value, String> {
        request.validate()?;
        if self.inner.exiting.load(Ordering::Acquire) { return Err("앱이 종료 중이어서 대화 요청을 시작하지 않았습니다.".into()); }
        let session = self.lookup(id, generation)?;
        if session.phase.load(Ordering::Acquire) != 1 || session.stop_requested.load(Ordering::Acquire) { return Err("실행 중인 세션에서만 대화 요청을 사용할 수 있습니다.".into()); }
        let info = session.info.lock().map_err(|_| "session metadata lock poisoned")?.clone();
        let thread_id = info.thread_id.ok_or("원래 대화 식별자가 아직 확인되지 않았습니다. 잠시 후 다시 시도하세요.")?;
        let process = session.process.lock().map_err(|_| "session process lock poisoned")?.clone().ok_or("session process is unavailable")?;
        let connection = process.adapter.lock().map_err(|_| "session adapter lock poisoned")?.as_ref().and_then(CodexSession::conversation_connection).ok_or("이 세션은 구조화 대화 연결을 지원하지 않습니다.")?;
        drop(process);
        connection.request_for_session(&thread_id, Path::new(&info.cwd), request,id,self.inner.attachments.clone())
    }

    fn with_subagent_receipt(&self,id: &str,generation: u64,request: codex::SubagentRequest,execute: impl FnOnce(Arc<Session>,codex::SubagentRequest)->Result<serde_json::Value,String>) -> Result<serde_json::Value,String> {
        // 찾을 수 없거나 교체된 세대의 영수증을 현재 세대의 미송신으로 단정하지 않는다.
        let session=self.lookup(id,generation)?;
        let Some((request_id,digest))=request.receipt_identity()? else {return execute(session,request);};
        {
            let mut receipts=session.subagent_receipts.lock().map_err(|_|"하위 에이전트 영수증 상태 오류")?;
            let receipts=receipts.as_mut().ok_or("복원된 세션의 이전 하위 에이전트 요청 결과를 확인할 수 없습니다. 확정 미적용으로 처리하지 않았습니다.")?;
            if let Some(receipt)=receipts.get(&request_id) {
                if receipt.digest!=digest {return Err("동일 요청 식별자에 다른 하위 에이전트 변경을 지정할 수 없습니다.".into());}
                return Ok(serde_json::json!({"status":receipt.status}));
            }
            if receipts.len()>=1024 {return Ok(serde_json::json!({"status":"notApplied","reason":"세션의 하위 에이전트 영수증 상한에 도달해 새 요청을 보내지 않았습니다."}));}
            // 새 요청만 수락 검사로 들어간다. 동시 재조회는 이 진행 중 기록을 본다.
            receipts.insert(request_id.clone(),SubagentReceipt {digest,status:"unknown"});
        }
        let result=execute(session.clone(),request);
        let status=match result.as_ref().ok().and_then(|value|value["status"].as_str()) {
            Some("applied")=>"applied",Some("notApplied")=>"notApplied",Some("unsupported")=>"unsupported",_=>"unknown",
        };
        let mut receipts=session.subagent_receipts.lock().map_err(|_|"하위 에이전트 영수증 상태 오류")?;
        if let Some(receipt)=receipts.as_mut().and_then(|receipts|receipts.get_mut(&request_id)) {receipt.status=status;}
        result
    }

    pub fn subagents_request(&self, id: &str, generation: u64, request: codex::SubagentRequest) -> Result<serde_json::Value, String> {
        self.with_subagent_receipt(id,generation,request,|session,request| {
        let mutation = !matches!(&request, codex::SubagentRequest::List { .. });
        // 기존·진행 중 영수증 조회 뒤의 새 요청에만 확정 미송신 결과를 돌려준다.
        let admitted = (|| {
            request.validate()?;
            if self.inner.exiting.load(Ordering::Acquire) { return Err("앱이 종료 중이어서 하위 에이전트 요청을 시작하지 않았습니다.".to_owned()); }
            if session.phase.load(Ordering::Acquire) != 1 || session.stop_requested.load(Ordering::Acquire) { return Err("실행 중인 세션에서만 하위 에이전트를 관리할 수 있습니다.".into()); }
            let info = session.info.lock().map_err(|_| "하위 에이전트 부모 세션 상태 오류")?.clone();
            let thread_id = info.thread_id.ok_or("부모 대화 식별자가 아직 확인되지 않았습니다.")?;
            let process = session.process.lock().map_err(|_| "하위 에이전트 부모 프로세스 상태 오류")?.clone().ok_or("부모 프로세스를 확인할 수 없습니다.")?;
            let connection = process.adapter.lock().map_err(|_| "하위 에이전트 연결 상태 오류")?.as_ref().and_then(CodexSession::conversation_connection).ok_or("이 세션은 하위 에이전트 관리 연결을 지원하지 않습니다.")?;
            drop(process);
            if self.inner.exiting.load(Ordering::Acquire) || session.phase.load(Ordering::Acquire) != 1 || session.stop_requested.load(Ordering::Acquire) { return Err("세션이 종료 중이어서 하위 에이전트 요청을 시작하지 않았습니다.".into()); }
            Ok((connection, thread_id, info.cwd))
        })();
        let (connection, thread_id, cwd) = match admitted {
            Ok(value) => value,
            Err(reason) if mutation => return Ok(serde_json::json!({"status":"notApplied","reason":reason})),
            Err(reason) => return Err(reason),
        };
        connection.subagents_request(&thread_id, Path::new(&cwd), request)
        })
    }

    pub fn cron_request(&self,id: &str,generation: u64,request: codex::CronRequest) -> Result<serde_json::Value,String> {
        request.validate()?;
        if self.inner.exiting.load(Ordering::Acquire) {return Err("앱이 종료 중이어서 예약 요청을 시작하지 않았습니다.".into());}
        let session=self.lookup(id,generation)?;
        if session.phase.load(Ordering::Acquire)!=1 || session.stop_requested.load(Ordering::Acquire) {return Err("실행 중인 세션에서만 예약 요청을 사용할 수 있습니다.".into());}
        let info=session.info.lock().map_err(|_| "예약 대상 세션 상태 오류")?.clone();
        let thread_id=info.thread_id.ok_or("예약 대상 대화 식별자가 아직 확인되지 않았습니다.")?;
        let process=session.process.lock().map_err(|_| "예약 대상 프로세스 상태 오류")?.clone().ok_or("예약 대상 프로세스를 확인할 수 없습니다.")?;
        let adapter=process.adapter.lock().map_err(|_| "예약 제어 연결 상태 오류")?;
        adapter.as_ref().ok_or("예약 대상 CLI를 확인할 수 없습니다.")?.cron_target_ready(&thread_id)?;
        if self.inner.exiting.load(Ordering::Acquire) || session.phase.load(Ordering::Acquire)!=1 || session.stop_requested.load(Ordering::Acquire) {return Err("세션이 종료 중이어서 예약 요청을 시작하지 않았습니다.".into());}
        #[cfg(windows)]
        let connection=process.cron.lock().map_err(|_|"예약 제어 상태 오류")?.clone();
        #[cfg(windows)]
        if let Some(connection)=connection {
            drop(adapter);
            drop(process);
            return connection.request(&thread_id,request);
        }
        adapter.as_ref().ok_or("예약 대상 CLI를 확인할 수 없습니다.")?.cron_request(&thread_id,request)
    }

    pub fn conversation_attachments(&self) -> crate::conversation_attachments::ConversationAttachments { self.inner.attachments.clone() }

    pub fn stage_conversation_image(&self,id: &str,generation: u64,data_url: &str) -> Result<crate::conversation_attachments::ConversationAttachment,String> {
        let session = self.lookup(id,generation)?;
        if self.inner.exiting.load(Ordering::Acquire) || session.phase.load(Ordering::Acquire) != 1 {return Err("실행 중인 세션에만 이미지를 첨부할 수 있습니다.".into());}
        let staged = self.inner.attachments.stage_image(id,data_url)?;
        if session.phase.load(Ordering::Acquire) != 1 {
            self.inner.attachments.cleanup_staged(id,&staged.path)?;
            return Err("이미지 준비 중 세션이 종료되어 미전송 이미지를 정리했습니다.".into());
        }
        Ok(staged)
    }

    pub fn cleanup_conversation_attachment(&self,id: &str,generation: u64,path: &str) -> Result<(),String> {
        self.lookup(id,generation)?;
        self.inner.attachments.cleanup_staged(id,path)
    }

    pub fn list_pending_questions(&self) -> Result<serde_json::Value,String> {
        let sessions: Vec<_> = self.inner.sessions.lock().map_err(|_| "session registry lock poisoned")?.values().cloned().collect();
        let mut items = Vec::new();
        for session in sessions {
            let info = session.info()?;
            let process = session.process.lock().map_err(|_| "session process lock poisoned")?.clone();
            let connection = process.as_ref().map(|process|process.adapter.lock().map_err(|_| "session adapter lock poisoned").map(|adapter|adapter.as_ref().and_then(CodexSession::conversation_connection))).transpose()?.flatten();
            if connection.as_ref().is_some_and(|connection|connection.is_deleted()) { continue; }
            let (owned,connected,pending) = connection.as_ref().map(|connection|connection.pending_snapshot()).transpose()?.unwrap_or((false,false,Vec::new()));
            for request in &pending {
                let method = request["method"].as_str().unwrap_or("");
                let kind = match method {"item/tool/requestUserInput" => "question","item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => "approval",_ => "unsupported"};
                let request_id = request["requestId"].as_str().ok_or("대기 요청 식별자가 없습니다.")?;
                let mut item = serde_json::json!({"id":format!("{}:{}:{}:{}",info.id,info.thread_id.as_deref().unwrap_or(""),info.generation,request_id),"threadId":info.thread_id,"sessionId":info.id,"generation":info.generation,"requestId":request_id,"kind":kind,"sessionTitle":info.title,"title":if kind=="question" {"사용자 답변 대기"} else if kind=="approval" {"사용자 승인 대기"} else {"터미널에서 확인할 요청"},"message":request["params"]["reason"].as_str().or_else(||request["params"]["command"].as_str()).unwrap_or(""),"canRespond":owned&&connected&&request["guiOwned"]==true&&kind!="unsupported"&&request["responded"]!=true,"responded":request["responded"]==true});
                if kind=="question" {item["questions"]=request["params"]["questions"].clone();}
                item["metadataAvailable"]=request["metadataAvailable"].clone();
                if kind=="approval" {
                    let options=codex::ConversationConnection::approval_decisions(&request["params"]).into_iter().map(|decision|serde_json::json!({"id":decision,"label":match decision {"accept"=>"승인","acceptForSession"=>"이 세션에서 승인","decline"=>"거절",_=>"취소"}})).collect::<Vec<_>>();
                    if options.is_empty() {item["canRespond"]=serde_json::json!(false);}
                    item["approval"]=serde_json::json!({"options":options});
                }
                items.push(item);
            }
            if pending.is_empty() && info.active && info.state=="waiting" {
                items.push(serde_json::json!({"id":format!("{}:{}:{}:terminal",info.id,info.thread_id.as_deref().unwrap_or(""),info.generation),"threadId":info.thread_id,"sessionId":info.id,"generation":info.generation,"kind":"unsupported","title":"터미널에서 답변할 요청","message":"현재 연결에서 질문 원문을 확인할 수 없습니다. 원래 터미널에서 확인하세요.","sessionTitle":info.title,"canRespond":false}));
            }
        }
        Ok(serde_json::json!({"items":items}))
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
mod runtime_preparation_tests {
    use super::*;

    fn engine() -> Engine { Engine::new(Path::new(":memory:"), Arc::new(|_| Ok(()))).unwrap() }
    fn select(engine: &Engine, executable: &str) -> RuntimeSnapshot {
        engine.runtime_snapshot_with(|| Ok(executable.into())).unwrap()
    }
    fn unlocked(engine: &Engine) {
        assert!(engine.inner.runtime.try_lock().is_ok(), "외부 작업 중 선택 잠금이 유지됨");
        assert!(engine.inner.store.try_lock().is_ok(), "외부 작업 중 저장소 잠금이 유지됨");
        assert!(engine.inner.sessions.try_lock().is_ok(), "외부 작업 중 세션 잠금이 유지됨");
    }

    #[test]
    fn readiness_reuses_only_success_and_forced_failure_discards_the_old_ticket() {
        let engine = engine();
        let selected = select(&engine, "runtime-a.exe");
        let cancelled = AtomicBool::new(false);
        let first = engine.prepare_selected_runtime_with(selected.clone(), false, &cancelled, |_| { unlocked(&engine); Ok(()) }).unwrap();
        let reused = engine.prepare_selected_runtime_with(selected.clone(), false, &cancelled, |_| panic!("성공 캐시를 재사용하지 않음")).unwrap();
        assert_eq!(first.preparation, reused.preparation);
        engine.verify_prepared_runtime(&first, &cancelled).unwrap();
        assert_eq!(engine.prepare_selected_runtime_with(selected.clone(), true, &cancelled, |_| Err("준비 실패".into())).unwrap_err(), "준비 실패");
        assert!(!engine.inner.runtime.lock().unwrap().ready);
        assert!(engine.verify_prepared_runtime(&first, &cancelled).is_err());
        let retried = engine.prepare_selected_runtime_with(selected, false, &cancelled, |_| Ok(())).unwrap();
        assert!(retried.preparation > first.preparation);
        engine.verify_prepared_runtime(&retried, &cancelled).unwrap();
    }

    #[test]
    fn late_preparation_cannot_commit_over_a_new_selection_or_forced_request() {
        for change_selection in [true, false] {
            let engine = engine();
            let selected = select(&engine, "runtime-a.exe");
            let (entered, entry) = std::sync::mpsc::sync_channel(0);
            let (release, released) = std::sync::mpsc::sync_channel(0);
            let worker_engine = engine.clone();
            let worker = thread::spawn(move || worker_engine.prepare_selected_runtime_with(selected, true, &AtomicBool::new(false), |_| {
                unlocked(&worker_engine);
                entered.send(()).unwrap();
                released.recv_timeout(Duration::from_secs(5)).unwrap();
                Ok(())
            }));
            entry.recv_timeout(Duration::from_secs(5)).unwrap();
            let competing = select(&engine, "runtime-a.exe");
            assert!(engine.prepare_selected_runtime_with(competing, false, &AtomicBool::new(false), |_| panic!("동시 시작이 준비 작업을 중복 실행함")).is_err());
            if change_selection { engine.change_runtime_selection_with(|| { unlocked(&engine); Ok(()) }).unwrap(); }
            let selected = select(&engine, if change_selection { "runtime-b.exe" } else { "runtime-a.exe" });
            let latest = engine.prepare_selected_runtime_with(selected, true, &AtomicBool::new(false), |_| Ok(())).unwrap();
            release.send(()).unwrap();
            assert!(worker.join().unwrap().is_err(), "오래된 준비 결과가 확정됨");
            engine.verify_prepared_runtime(&latest, &AtomicBool::new(false)).unwrap();
            assert_eq!(engine.inner.runtime.lock().unwrap().preparation, latest.preparation);
        }
    }

    #[test]
    fn selection_save_and_resolution_are_fenced_without_holding_io_locks() {
        let engine = engine();
        let selected = select(&engine, "runtime-a.exe");
        let prepared = engine.prepare_selected_runtime_with(selected, false, &AtomicBool::new(false), |_| Ok(())).unwrap();
        assert_eq!(engine.change_runtime_selection_with(|| { unlocked(&engine); Err("저장 실패".into()) }).unwrap_err(), "저장 실패");
        engine.verify_prepared_runtime(&prepared, &AtomicBool::new(false)).unwrap();
        engine.change_runtime_selection_with(|| {
            unlocked(&engine);
            assert!(engine.runtime_snapshot_with(|| panic!("저장 중 실행파일을 해석함")).is_err());
            assert!(engine.verify_prepared_runtime(&prepared, &AtomicBool::new(false)).is_err());
            Ok(())
        }).unwrap();
        assert!(!engine.inner.runtime.lock().unwrap().ready);
        assert!(engine.verify_prepared_runtime(&prepared, &AtomicBool::new(false)).is_err());
        let resolved = engine.runtime_snapshot_with(|| {
            unlocked(&engine);
            engine.change_runtime_selection_with(|| Ok(())).unwrap();
            Ok("runtime-b.exe".into())
        });
        assert!(resolved.is_err(), "해석 중 선택이 바뀐 결과를 채택함");
        assert!(engine.inner.runtime.lock().unwrap().executable.is_none());
    }

    #[test]
    fn start_rejects_selection_changes_during_preparation_or_connection_and_drops_the_connection() {
        let engine = engine();
        let selected = select(&engine, "runtime-a.exe");
        let cancelled = AtomicBool::new(false);
        assert!(engine.prepare_start_with::<()>(selected, &cancelled, |_| {
            engine.change_runtime_selection_with(|| Ok(())).unwrap();
            Ok(())
        }, |_| panic!("선택이 바뀐 준비 결과로 연결을 시작함")).is_err());
        let selected = select(&engine, "runtime-b.exe");
        struct Connection(Arc<AtomicBool>);
        impl Drop for Connection { fn drop(&mut self) { self.0.store(true, Ordering::Release); } }
        let dropped = Arc::new(AtomicBool::new(false));
        assert!(engine.prepare_start_with(selected, &cancelled, |exe| { assert_eq!(exe, &std::ffi::OsString::from("runtime-b.exe")); Ok(()) }, |exe| {
            unlocked(&engine);
            assert_eq!(exe, &std::ffi::OsString::from("runtime-b.exe"));
            engine.change_runtime_selection_with(|| Ok(())).unwrap();
            Ok(Connection(dropped.clone()))
        }).is_err());
        assert!(dropped.load(Ordering::Acquire), "거절한 연결의 소유권이 해제되지 않음");
    }

    #[test]
    fn stop_and_exit_prevent_preparation_admission_and_result_commit() {
        let engine = engine();
        let selected = select(&engine, "runtime-a.exe");
        let stopped = AtomicBool::new(true);
        assert!(engine.prepare_selected_runtime_with(selected.clone(), false, &stopped, |_| panic!("중단 이후 준비가 시작됨")).is_err());
        stopped.store(false, Ordering::Release);
        assert!(engine.prepare_selected_runtime_with(selected.clone(), false, &stopped, |_| { stopped.store(true, Ordering::Release); Ok(()) }).is_err());
        assert!(!engine.inner.runtime.lock().unwrap().ready);
        stopped.store(false, Ordering::Release);
        let prepared = engine.prepare_selected_runtime_with(selected.clone(), false, &stopped, |_| Ok(())).unwrap();
        stopped.store(true, Ordering::Release);
        assert!(engine.verify_prepared_runtime(&prepared, &stopped).is_err());
        stopped.store(false, Ordering::Release);
        assert!(engine.prepare_selected_runtime_with(selected.clone(), true, &stopped, |_| {
            unlocked(&engine);
            engine.inner.exiting.store(true, Ordering::Release);
            Ok(())
        }).is_err());
        assert!(!engine.inner.runtime.lock().unwrap().ready);
        assert!(engine.prepare_selected_runtime_with(selected, true, &stopped, |_| panic!("종료 이후 준비가 시작됨")).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn invalid_save_preserves_readiness_but_successful_save_and_reset_require_preparation() {
        let _admission = super::lifecycle_tests::EXTENSION_ADMISSION_TEST.lock().unwrap();
        let engine = engine();
        let executable = std::env::current_exe().unwrap();
        engine.set_codex_executable(Some(executable.to_str().unwrap())).unwrap();
        let selected = engine.runtime_snapshot().unwrap();
        let prepared = engine.prepare_selected_runtime_with(selected, false, &AtomicBool::new(false), |_| Ok(())).unwrap();
        assert!(engine.set_codex_executable(Some("relative.cmd")).is_err());
        engine.verify_prepared_runtime(&prepared, &AtomicBool::new(false)).unwrap();
        engine.with_extension_operation_ready(true, |exe, _| { assert_eq!(exe, &prepared.selection.executable); unlocked(&engine); Ok(()) }).unwrap();
        engine.set_codex_executable(Some(executable.to_str().unwrap())).unwrap();
        assert!(engine.with_extension_operation_ready::<()>(true, |_, _| panic!("미준비 실행파일로 사용량을 조회함")).is_err());
        let selected = engine.runtime_snapshot().unwrap();
        let selected = engine.prepare_selected_runtime_with(selected, false, &AtomicBool::new(false), |_| Ok(())).unwrap();
        engine.set_codex_executable(None).unwrap();
        assert!(engine.verify_prepared_runtime(&selected, &AtomicBool::new(false)).is_err());
        assert!(!engine.inner.runtime.lock().unwrap().ready);
    }
}

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
    fn conversation_and_attachment_admission_reject_stale_generations_without_a_transport() {
        let engine = Engine::new(Path::new(":memory:"),Arc::new(|_|Ok(()))).unwrap();
        let session = engine.allocate(&request()).unwrap(); let info = session.info().unwrap();
        assert!(engine.conversation_request(&info.id,info.generation+1,codex::ConversationRequest::Attach).unwrap_err().contains("stale"));
        assert!(engine.stage_conversation_image(&info.id,info.generation+1,"data:image/png;base64,").unwrap_err().contains("stale"));
        assert!(engine.cleanup_conversation_attachment(&info.id,info.generation+1,"external-file").unwrap_err().contains("stale"));
        assert!(session.process.lock().unwrap().is_none());
        session.finish("stopped").unwrap();
    }

    #[test]
    fn subagent_admission_rejects_stale_stopped_or_unowned_parent_before_transport() {
        let engine = Engine::new(Path::new(":memory:"), Arc::new(|_| Ok(()))).unwrap();
        let session = engine.allocate(&request()).unwrap(); let info = session.info().unwrap();
        let list = || codex::SubagentRequest::List { cursor: None };
        assert!(engine.subagents_request(&info.id, info.generation + 1, list()).unwrap_err().contains("stale"));
        assert!(engine.subagents_request(&info.id, info.generation, list()).unwrap_err().contains("실행 중"));
        session.phase.store(1, Ordering::Release);
        assert!(engine.subagents_request(&info.id, info.generation, list()).unwrap_err().contains("대화 식별자"));
        session.info.lock().unwrap().thread_id = Some(Uuid::new_v4().to_string());
        assert!(engine.subagents_request(&info.id, info.generation, list()).unwrap_err().contains("프로세스"));
        session.stop_requested.store(true, Ordering::Release);
        assert!(engine.subagents_request(&info.id, info.generation, list()).unwrap_err().contains("실행 중"));
        engine.inner.exiting.store(true, Ordering::Release);
        assert!(engine.subagents_request(&info.id, info.generation, list()).unwrap_err().contains("종료 중"));
        assert!(session.process.lock().unwrap().is_none()); session.finish("stopped").unwrap();
    }

    #[test]
    fn subagent_mutation_admission_is_known_not_applied_before_transport() {
        let engine = Engine::new(Path::new(":memory:"), Arc::new(|_| Ok(()))).unwrap();
        let session = engine.allocate(&request()).unwrap(); let info = session.info().unwrap();
        let send = || codex::SubagentRequest::Send {
            agent_id: Uuid::new_v4().to_string(), request_id: Uuid::new_v4().to_string(), text: "합성 지시".into(),
            expected_parent_runtime_id: Uuid::new_v4().to_string(), expected_runtime_id: Some(Uuid::new_v4().to_string()),
        };
        for stage in 0..5 {
            match stage {
                1 => { session.phase.store(1, Ordering::Release); },
                2 => { session.info.lock().unwrap().thread_id = Some(Uuid::new_v4().to_string()); },
                3 => { session.stop_requested.store(true, Ordering::Release); },
                4 => { engine.inner.exiting.store(true, Ordering::Release); },
                _ => {},
            }
            let result = engine.subagents_request(&info.id, info.generation, send()).unwrap();
            assert_eq!(result["status"], "notApplied"); assert!(result["reason"].is_string());
        }
        assert!(engine.subagents_request(&info.id, info.generation + 1, send()).unwrap_err().contains("stale"));
        assert!(session.process.lock().unwrap().is_none()); session.finish("stopped").unwrap();
    }

    fn subagent_send_value() -> serde_json::Value {
        serde_json::json!({"type":"send","agentId":Uuid::new_v4().to_string(),"requestId":Uuid::new_v4().to_string(),"text":"합성 지시",
            "expectedParentRuntimeId":Uuid::new_v4().to_string(),"expectedRuntimeId":Uuid::new_v4().to_string()})
    }

    #[test]
    fn subagent_engine_preserves_sent_unknown_across_stop_exit_phase_process_and_stale_lookup() {
        for boundary in 0..4 {
            let engine=Engine::new(Path::new(":memory:"),Arc::new(|_|Ok(()))).unwrap();
            let session=engine.allocate(&request()).unwrap();let info=session.info().unwrap();let parent=Uuid::new_v4().to_string();
            session.phase.store(1,Ordering::Release);session.info.lock().unwrap().thread_id=Some(parent.clone());
            let value=subagent_send_value();let send=||serde_json::from_value::<codex::SubagentRequest>(value.clone()).unwrap();
            let connection=codex::ConversationConnection::fixture_subagent_connection();
            let first=engine.with_subagent_receipt(&info.id,info.generation,send(),|_,request|connection.fixture_subagent_lost_reply(&parent,request)).unwrap();
            assert_eq!(first["status"],"unknown","실제 CONTROL 송신 후 응답 유실을 먼저 확인합니다.");
            match boundary {
                0=>session.stop_requested.store(true,Ordering::Release),
                1=>engine.inner.exiting.store(true,Ordering::Release),
                2=>session.phase.store(2,Ordering::Release),
                _=>{assert!(session.process.lock().unwrap().is_none());},
            }
            let repeated=engine.subagents_request(&info.id,info.generation,send()).unwrap();
            assert_eq!(repeated["status"],"unknown","종료·프로세스 소실 뒤 기존 요청을 미적용으로 바꾸면 안 됩니다.");
            let fresh=serde_json::from_value(subagent_send_value()).unwrap();
            assert_eq!(engine.subagents_request(&info.id,info.generation,fresh).unwrap()["status"],"notApplied");
            assert!(engine.subagents_request(&info.id,info.generation+1,send()).unwrap_err().contains("stale"));
            assert_eq!(engine.subagents_request(&info.id,info.generation,send()).unwrap()["status"],"unknown");
            let mut different=value.clone();different["text"]=serde_json::json!("다른 내용");
            assert!(engine.subagents_request(&info.id,info.generation,serde_json::from_value(different).unwrap()).is_err());
            assert!(session.process.lock().unwrap().is_none(),"추가 송신을 위한 프로세스·통로를 만들지 않습니다.");
            session.finish("stopped").unwrap();
        }
    }

    #[test]
    fn subagent_engine_reads_inflight_receipt_before_shutdown_admission() {
        let engine=Engine::new(Path::new(":memory:"),Arc::new(|_|Ok(()))).unwrap();let session=engine.allocate(&request()).unwrap();let info=session.info().unwrap();
        let value=subagent_send_value();let send=||serde_json::from_value::<codex::SubagentRequest>(value.clone()).unwrap();
        let result=engine.with_subagent_receipt(&info.id,info.generation,send(),|session,_| {
            session.stop_requested.store(true,Ordering::Release);engine.inner.exiting.store(true,Ordering::Release);
            assert_eq!(engine.subagents_request(&info.id,info.generation,send()).unwrap()["status"],"unknown");
            let fresh=serde_json::from_value(subagent_send_value()).unwrap();
            assert_eq!(engine.subagents_request(&info.id,info.generation,fresh).unwrap()["status"],"notApplied");
            Err("진행 중 요청의 최종 결과 미확인".into())
        });
        assert!(result.is_err());assert_eq!(engine.subagents_request(&info.id,info.generation,send()).unwrap()["status"],"unknown");session.finish("stopped").unwrap();
    }

    #[test]
    fn subagent_engine_receipts_bound_new_admission_and_restored_state_is_unconfirmed() {
        let engine=Engine::new(Path::new(":memory:"),Arc::new(|_|Ok(()))).unwrap();let session=engine.allocate(&request()).unwrap();let info=session.info().unwrap();
        let value=subagent_send_value();let send=||serde_json::from_value::<codex::SubagentRequest>(value.clone()).unwrap();
        let (id,digest)=send().receipt_identity().unwrap().unwrap();
        {
            let mut ledger=session.subagent_receipts.lock().unwrap();let ledger=ledger.as_mut().unwrap();
            ledger.insert(id,SubagentReceipt {digest,status:"unknown"});
            for _ in 1..1024 {ledger.insert(Uuid::new_v4().to_string(),SubagentReceipt {digest:[0;32],status:"applied"});}
        }
        assert_eq!(engine.subagents_request(&info.id,info.generation,send()).unwrap()["status"],"unknown");
        let fresh=serde_json::from_value(subagent_send_value()).unwrap();
        assert_eq!(engine.subagents_request(&info.id,info.generation,fresh).unwrap()["status"],"notApplied");
        *session.subagent_receipts.lock().unwrap()=None;
        assert!(engine.subagents_request(&info.id,info.generation,send()).unwrap_err().contains("이전 하위 에이전트 요청 결과를 확인할 수 없습니다"));
        assert!(engine.subagents_request("확인할 수 없는 세션",info.generation,send()).is_err());session.finish("stopped").unwrap();
    }

    #[test]
    fn cron_admission_checks_generation_lifetime_and_current_thread_before_transport() {
        let engine=Engine::new(Path::new(":memory:"),Arc::new(|_|Ok(()))).unwrap();
        let session=engine.allocate(&request()).unwrap();let info=session.info().unwrap();
        assert!(engine.cron_request(&info.id,info.generation+1,codex::CronRequest::List{}).unwrap_err().contains("stale"));
        assert!(engine.cron_request(&info.id,info.generation,codex::CronRequest::List{}).unwrap_err().contains("실행 중"));
        session.phase.store(1,Ordering::Release);
        assert!(engine.cron_request(&info.id,info.generation,codex::CronRequest::List{}).unwrap_err().contains("대화 식별자"));
        session.info.lock().unwrap().thread_id=Some(Uuid::new_v4().to_string());
        assert!(engine.cron_request(&info.id,info.generation,codex::CronRequest::List{}).unwrap_err().contains("프로세스"));
        session.stop_requested.store(true,Ordering::Release);
        assert!(engine.cron_request(&info.id,info.generation,codex::CronRequest::List{}).unwrap_err().contains("실행 중"));
        assert!(session.process.lock().unwrap().is_none());session.finish("stopped").unwrap();
    }

    #[test]
    fn waiting_terminal_inbox_is_a_read_only_fallback_without_inventing_question_metadata() {
        let engine = Engine::new(Path::new(":memory:"),Arc::new(|_|Ok(()))).unwrap();
        let session = engine.allocate(&request()).unwrap();
        session.update(|info|info.state="waiting".into(),true).unwrap();
        let inbox = engine.list_pending_questions().unwrap(); let items = inbox["items"].as_array().unwrap();
        assert_eq!(items.len(),1); assert_eq!(items[0]["kind"],"unsupported"); assert_eq!(items[0]["canRespond"],false);
        assert!(items[0].get("requestId").is_none()); assert!(items[0].get("questions").is_none());
        assert!(session.process.lock().unwrap().is_none(),"질문함 조회가 서버나 연결을 만들면 안 된다");
        session.finish("stopped").unwrap(); assert!(engine.list_pending_questions().unwrap()["items"].as_array().unwrap().is_empty());
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
