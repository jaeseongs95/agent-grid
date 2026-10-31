//! 예약 실행을 소유하지 않는 일회 TUI 제어 어댑터. 기존 실행기의 작성자 하나만 통로를 만진다.
#![cfg(windows)]

use super::{cron::{bounded_cron_snapshot, CronRequest}, cron_wire as wire};
use crate::{owned_tui_control::{OwnedTuiPipe, PipeCancellation}, process_job::{OwnedProcessIdentity, ProcessIdentity}};
use portable_pty::CommandBuilder;
use serde_json::{json, Value};
use std::{collections::VecDeque, path::Path, sync::{atomic::{AtomicBool, Ordering}, mpsc, Arc, Condvar, Mutex}, time::{Duration, Instant}};
use tokio::sync::Notify;

const MAX_REQUESTS: usize = 32;
const MAX_BYTES: usize = 2 * 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const FRAME_TIMEOUT: Duration = Duration::from_secs(5);
type VerifyPeer = Arc<dyn Fn(&mut OwnedTuiPipe) -> Result<ProcessIdentity, String> + Send + Sync>;

pub(crate) struct CronPreparation {
    pipe: OwnedTuiPipe,
    bootstrap: wire::Bootstrap,
    token: wire::SecretToken,
    cancelled: Arc<AtomicBool>,
}

/// 정상적인 전역 도움말의 옵션 선언과 그 항목의 가능한 값만 검사한다.
fn help_supports_control(bytes: &[u8]) -> Result<bool, String> {
    let help = std::str::from_utf8(bytes).map_err(|_| "예약 제어 도움말이 UTF-8이 아닙니다.")?;
    if !help.lines().any(|line| line.trim_start().starts_with("Usage:") || line.trim_start().starts_with("사용법:")) {
        return Err("예약 제어의 전역 도움말 형식을 확인할 수 없습니다.".into());
    }
    let options: Vec<_> = help.lines().skip_while(|line| line.trim() != "Options:").skip(1)
        .take_while(|line| line.is_empty() || line.starts_with(char::is_whitespace)).collect();
    let column = options.iter().filter_map(|line| {
        if !line.trim_start().starts_with('-') { return None; }
        let position = line.find("--help")?;
        (line[position..].split_ascii_whitespace().next()?.trim_end_matches(',') == "--help").then_some(position)
    }).min().ok_or("예약 제어 도움말의 옵션 선언 열을 확인할 수 없습니다.")?;
    // 실제 옵션 열보다 더 들여쓴 설명문의 유사 옵션은 선언이 아니다.
    let declaration = options.iter().position(|line| line.trim_start().starts_with('-')
        && line.get(column..).is_some_and(|tail| tail.starts_with("--")
            && tail.split_ascii_whitespace().next() == Some("--desktop-cron-control")));
    let Some(index) = declaration else { return Ok(false); };
    let item = options[index..].iter().enumerate().take_while(|(offset,line)| *offset == 0
        || !(line.trim_start().starts_with('-') && line.get(column..).is_some_and(|tail| tail.starts_with("--"))))
        .map(|(_,line)| *line).collect::<Vec<_>>().join("\n");
    let Some(values) = item.split("[possible values:").nth(1).and_then(|tail| tail.split(']').next()) else {
        return Err("예약 제어 옵션의 정확한 허용 값을 확인할 수 없습니다.".into());
    };
    Ok(values.split(',').map(str::trim).eq(["pipe-v1"]))
}

impl CronPreparation {
    pub(crate) fn prepare(command: &mut CommandBuilder, session_id: &str, generation: u64,
        cancelled: Arc<AtomicBool>) -> Result<Option<Self>, String> {
        // 구형 실행 파일에도 부모의 이전 bootstrap이 유출되지 않도록 먼저 제거한다.
        let inherited: Vec<_> = std::env::vars_os().map(|(key, _)| key).collect();
        let configured: Vec<_> = command.iter_full_env_as_str().map(|(key, _)| std::ffi::OsString::from(key)).collect();
        for key in inherited.into_iter().chain(configured) {
            if key.to_string_lossy().eq_ignore_ascii_case(wire::BOOTSTRAP_ENV)
                || key.to_string_lossy().eq_ignore_ascii_case(wire::TOKEN_ENV) { command.env_remove(key); }
        }
        command.env_remove(wire::BOOTSTRAP_ENV);
        command.env_remove(wire::TOKEN_ENV);
        let exe = command.get_argv().first().ok_or("예약 제어 대상 실행 파일이 없습니다.")?.clone();
        let cwd = command.get_cwd().map(Path::new).unwrap_or_else(|| Path::new("."));
        let output = super::extensions::command_stdout_until(&exe, cwd, &["--help".into()],
            Instant::now() + FRAME_TIMEOUT, Some(&cancelled))
            .map_err(|_| "선택한 CLI의 예약 제어 도움말 실행이 실패했습니다. 미지원으로 간주하지 않았습니다.")?;
        if !help_supports_control(&output)? { return Ok(None); }
        if cancelled.load(Ordering::Acquire) { return Err("예약 제어 준비가 취소됐습니다.".into()); }
        let pipe = tauri::async_runtime::block_on(async { OwnedTuiPipe::bind() })
            .map_err(|error| format!("예약 제어 통로를 만들지 못했습니다: {error}"))?;
        let host = OwnedProcessIdentity::current().map_err(|error| format!("예약 제어 호스트를 확인하지 못했습니다: {error}"))?.identity();
        let bootstrap = wire::Bootstrap::new(pipe.name(), host.pid, host.created_filetime, session_id, generation)?;
        let token = wire::SecretToken::generate()?;
        let encoded = serde_json::to_string(&bootstrap).map_err(|_| "예약 제어 초기 자료를 직렬화하지 못했습니다.")?;
        command.env(wire::BOOTSTRAP_ENV, encoded);
        command.env(wire::TOKEN_ENV, token.expose_hex().as_str());
        command.arg("--desktop-cron-control=pipe-v1");
        Ok(Some(Self { pipe, bootstrap, token, cancelled }))
    }

    /// 호출자는 제어 상태 설치 후, 자식 실행 문을 열기 전에 시작한다.
    pub(crate) fn start(self, verify: VerifyPeer) -> Result<CronConnection, String> {
        if self.cancelled.load(Ordering::Acquire) { return Err("예약 제어 시작이 취소됐습니다.".into()); }
        let shared = Arc::new(Shared::new(self.cancelled.clone(), self.pipe.cancellation()));
        let lifetime = Arc::new(ConnectionLifetime(shared.clone()));
        let worker = shared.clone();
        tauri::async_runtime::spawn(async move {
            let completion = Completion(worker.clone());
            let Self { pipe, bootstrap, token, .. } = self;
            run(pipe, bootstrap, token, verify, worker).await;
            drop(completion);
        });
        Ok(CronConnection { shared, _lifetime: lifetime })
    }
}

#[derive(Clone)]
pub(crate) struct CronConnection { shared: Arc<Shared>, _lifetime: Arc<ConnectionLifetime> }
struct ConnectionLifetime(Arc<Shared>);
impl Drop for ConnectionLifetime { fn drop(&mut self) { self.0.cancel(); } }
struct Shared { state: Mutex<State>, wake: Notify, done: Condvar, stopped: Arc<AtomicBool>, pipe_cancel: PipeCancellation }
struct State {
    queue: VecDeque<Arc<Work>>, active: Option<Arc<Work>>, count: usize, bytes: usize,
    owner: Option<wire::UuidText>, scope: Option<wire::Scope>, pending: Option<Pending>,
    initial_list_reserved: bool, supports_interrupt: bool, closed: bool, finished: bool,
    failure: Option<String>, cleanup_failure: Option<String>, worker_returned: bool,
}
struct Pending { id: wire::UuidText, scope: wire::Scope, body: Arc<Value>, admission: Arc<()>, bytes: usize }
struct Work {
    request: wire::Request, frame: Value, admission: Arc<()>, bytes: usize, mutation: bool, deadline: Instant,
    stage: Arc<Mutex<Stage>>, reply: Mutex<Option<mpsc::Sender<Result<Value, String>>>>,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage { Queued, Sending, Expired, Done }
impl Shared {
    fn new(stopped: Arc<AtomicBool>, pipe_cancel: PipeCancellation) -> Self {
        Self { state: Mutex::new(State { queue: VecDeque::new(), active: None, count: 0, bytes: 0, owner: None, scope: None,
            pending: None, initial_list_reserved: false, supports_interrupt: false, closed: false, finished: false, failure: None, cleanup_failure: None, worker_returned: false }),
            wake: Notify::new(), done: Condvar::new(), stopped, pipe_cancel }
    }
    fn cancel(&self) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        // 같은 잠금으로 송신 시도 표시와 취소를 선형화한다.
        state.closed = true;
        state.supports_interrupt = false;
        drop(state);
        self.pipe_cancel.cancel();
        self.wake.notify_one();
    }
}
struct Completion(Arc<Shared>);
impl Drop for Completion {
    fn drop(&mut self) {
        let mut state = self.0.state.lock().unwrap_or_else(|error| error.into_inner());
        state.closed = true;
        state.supports_interrupt = false;
        if !state.worker_returned { state.cleanup_failure = Some("예약 제어 작업이 정상 반환 전에 종료됐습니다.".into()); }
        if let Some(work) = state.active.take() {
            let sent = *work.stage.lock().unwrap_or_else(|error| error.into_inner()) == Stage::Sending;
            let result = if sent && work.mutation { Ok(unknown()) } else { rejection(work.mutation, "connectionClosed") };
            finish_locked(&mut state, &work, result, !sent);
        }
        while let Some(work) = state.queue.pop_front() {
            finish_locked(&mut state, &work, rejection(work.mutation, "connectionClosed"), true);
        }
        state.finished = true;
        self.0.done.notify_all();
    }
}
fn rejection(mutation: bool, reason: &str) -> Result<Value, String> {
    if mutation { Ok(json!({"status":"notApplied","reason":reason})) }
    else { Err(format!("예약 조회를 송신하지 못했습니다: {reason}")) }
}
fn unknown() -> Value { json!({"status":"unknown","reason":"resultUnknown"}) }
fn disconnected(state: &State) -> Value {
    let mut result = json!({"supported":true,"connected":false,"scope":"cliLifetime","jobs":[],"hasMore":false,
        "reason":if state.closed {"connectionClosed"} else {"connectionNotReady"}});
    if state.pending.is_some() { result["mutationState"] = json!("unknown"); }
    result
}
fn clear_pending(state: &mut State) {
    if let Some(pending) = state.pending.take() { state.bytes -= pending.bytes; }
}
fn finish_locked(state: &mut State, work: &Work, result: Result<Value, String>, definitive: bool) {
    if work.mutation && definitive && state.pending.as_ref().is_some_and(|pending| Arc::ptr_eq(&pending.admission, &work.admission)) { clear_pending(state); }
    if work.request.scope.is_none() { state.initial_list_reserved = false; }
    *work.stage.lock().unwrap_or_else(|error| error.into_inner()) = Stage::Done;
    state.active = None;
    state.count -= 1;
    state.bytes -= work.bytes;
    if let Some(reply) = work.reply.lock().unwrap_or_else(|error| error.into_inner()).take() { let _ = reply.send(result); }
}
impl CronConnection {
    pub(crate) fn cancel(&self) { self.shared.cancel(); }
    pub(crate) fn wait_until(&self, deadline: Instant) -> Result<(), String> {
        let mut state = self.shared.state.lock().map_err(|_| "예약 제어 종료 상태 잠금이 손상됐습니다.")?;
        while !state.finished {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() { return Err("예약 제어 작업의 실제 종료를 기한 안에 확인하지 못했습니다.".into()); }
            state = self.shared.done.wait_timeout(state, remaining).map_err(|_| "예약 제어 종료 대기가 손상됐습니다.")?.0;
        }
        match &state.cleanup_failure { Some(error) => Err(error.clone()), None => Ok(()) }
    }
    pub(crate) fn request(&self, thread_id: &str, body: CronRequest) -> Result<Value, String> {
        let deadline = Instant::now() + REQUEST_TIMEOUT;
        body.validate()?;
        let target = wire::UuidText::parse(thread_id)?;
        let mutation = !matches!(body, CronRequest::List {});
        let is_cancel = matches!(body, CronRequest::Cancel { .. });
        let body_value = serde_json::to_value(&body).map_err(|_| "예약 요청 본문을 직렬화하지 못했습니다.")?;
        let mut state = self.shared.state.lock().map_err(|_| "예약 제어 접수 상태 잠금이 손상됐습니다.")?;
        if mutation && state.pending.is_some() { return rejection(true, "unresolvedMutation"); }
        if state.closed || self.shared.stopped.load(Ordering::Acquire) || state.owner.is_none() {
            return if mutation { rejection(true, "connectionNotReady") } else { bounded_cron_snapshot(disconnected(&state)) };
        }
        if is_cancel && !state.supports_interrupt { return Ok(json!({"status":"unsupported","reason":"expectedTurnInterruptUnsupported"})); }
        if state.scope.as_ref().is_some_and(|scope| scope.thread_id != target) { return rejection(mutation, "staleTarget"); }
        if mutation && state.scope.is_none() { return rejection(true, "scopeNotReady"); }
        if state.scope.is_none() && state.initial_list_reserved { return rejection(false, "scopeNotReady"); }
        let id = if mutation { wire::UuidText::parse(body_value["requestId"].as_str().ok_or("예약 변경 식별자가 없습니다.")?)? }
            else { wire::UuidText::parse(&uuid::Uuid::new_v4().to_string())? };
        let reconcile = if !mutation {
            state.pending.as_ref().map(|pending| {
                if state.scope.as_ref() != Some(&pending.scope) { return Err("미결 예약 변경의 소유 범위가 바뀌었습니다.".to_owned()); }
                serde_json::from_value((*pending.body).clone()).map(|request| wire::Reconcile { request })
                    .map_err(|_| "보존한 예약 변경 본문이 손상됐습니다.".to_owned())
            }).transpose()?
        } else { None };
        let request = wire::Request { v: wire::VersionOne, kind: wire::RequestType::Value, id: id.clone(),
            owner_instance_id: state.owner.as_ref().unwrap().clone(), target_thread_id: target,
            scope: state.scope.clone(), body, reconcile };
        request.validate()?;
        let frame = serde_json::to_value(&request).map_err(|_| "예약 제어 봉투를 직렬화하지 못했습니다.")?;
        let length = serde_json::to_vec(&frame).map_err(|_| "예약 제어 크기를 확인하지 못했습니다.")?.len();
        if length > crate::owned_tui_control::MAX_FRAME_BYTES { return rejection(mutation, "frameTooLarge"); }
        // typed 요청·송신 Value·미결 원문까지 포함한 보수적인 직렬화 보관 예약이다.
        let bytes = length.checked_mul(2).ok_or("예약 제어 보관 크기를 표현할 수 없습니다.")?;
        let pending_bytes = if mutation { body_value.to_string().len() } else { 0 };
        let reservation = bytes.checked_add(pending_bytes).ok_or("예약 제어 보관 크기를 표현할 수 없습니다.")?;
        if state.count >= MAX_REQUESTS || reservation > MAX_BYTES.saturating_sub(state.bytes) { return rejection(mutation, "queueFull"); }
        if Instant::now() >= deadline { return rejection(mutation, "requestTimeout"); }
        let (sender, receiver) = mpsc::channel();
        let work = Arc::new(Work { request, frame, admission: Arc::new(()), bytes, mutation, deadline, stage: Arc::new(Mutex::new(Stage::Queued)), reply: Mutex::new(Some(sender)) });
        if mutation { state.pending = Some(Pending { id, scope: state.scope.clone().unwrap(), body: Arc::new(body_value), admission: work.admission.clone(), bytes: pending_bytes }); }
        if state.scope.is_none() { state.initial_list_reserved = true; }
        let admission = work.admission.clone();
        let stage = work.stage.clone();
        state.count += 1; state.bytes += reservation; state.queue.push_back(work);
        drop(state);
        self.shared.wake.notify_one();
        match receiver.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(result) => result,
            Err(_) => {
                let mut state = self.shared.state.lock().map_err(|_| "예약 제어 기한 상태 잠금이 손상됐습니다.")?;
                let mut stage = stage.lock().map_err(|_| "예약 제어 송신 상태 잠금이 손상됐습니다.")?;
                match *stage {
                    Stage::Queued => {
                        *stage = Stage::Expired;
                        if mutation && state.pending.as_ref().is_some_and(|pending| Arc::ptr_eq(&pending.admission, &admission)) { clear_pending(&mut state); }
                        rejection(mutation, "requestTimeout")
                    }
                    Stage::Sending if mutation => Ok(unknown()),
                    Stage::Sending => Err("예약 조회 결과를 기한 안에 받지 못했습니다.".into()),
                    Stage::Expired => rejection(mutation, "requestTimeout"),
                    Stage::Done => receiver.try_recv().unwrap_or_else(|_| Err("예약 제어 응답 전달이 종료됐습니다.".into())),
                }
            }
        }
    }
}

fn io_failure(context: &str, error: std::io::Error) -> String { format!("{context}: {error}") }
fn frame_deadline(request_deadline: Instant) -> tokio::time::Instant {
    std::cmp::min(request_deadline, Instant::now() + FRAME_TIMEOUT).into()
}
async fn authenticate(pipe: &mut OwnedTuiPipe, bootstrap: &wire::Bootstrap, token: wire::SecretToken,
    verify: &VerifyPeer) -> Result<wire::UuidText, String> {
    // 사용자 로그인 동안의 무트래픽 대기는 인증 제한시간에 포함하지 않는다.
    let waiting = tokio::time::Instant::now() + Duration::from_secs(100 * 365 * 24 * 60 * 60);
    pipe.accept(waiting).await.map_err(|error| io_failure("예약 제어 연결 대기 실패", error))?;
    let until = tokio::time::Instant::now() + FRAME_TIMEOUT;
    let identity = verify(pipe)?;
    let hello = pipe.read_frame::<wire::Hello>(until).await.map_err(|error| io_failure("예약 제어 인사 수신 실패", error))?;
    let (handshake, challenge) = wire::ServerHandshake::begin(token, bootstrap, hello, identity.pid, identity.created_filetime)?;
    let challenge = serde_json::to_value(challenge).map_err(|_| "예약 제어 인증 응답 변환 실패")?;
    pipe.write_frame(&challenge, until).await.map_err(|error| io_failure("예약 제어 인증 응답 송신 실패", error))?;
    let auth = pipe.read_frame::<wire::Auth>(until).await.map_err(|error| io_failure("예약 제어 증명 수신 실패", error))?;
    let ready = handshake.finish(auth)?;
    let owner = ready.owner_instance_id.clone();
    pipe.write_frame(&serde_json::to_value(ready).map_err(|_| "예약 제어 준비 응답 변환 실패")?, until)
        .await.map_err(|error| io_failure("예약 제어 준비 응답 송신 실패", error))?;
    Ok(owner)
}
async fn run(mut pipe: OwnedTuiPipe, bootstrap: wire::Bootstrap, token: wire::SecretToken,
    verify: VerifyPeer, shared: Arc<Shared>) {
    let result = async {
        let owner = authenticate(&mut pipe, &bootstrap, token, &verify).await?;
        {
            let mut state = shared.state.lock().map_err(|_| "예약 제어 소유 상태 잠금 손상")?;
            if state.closed || shared.stopped.load(Ordering::Acquire) { return Ok(()); }
            state.owner = Some(owner);
        }
        loop {
            // 알림 등록 뒤 큐를 검사하여 접수와 idle 진입 사이의 깨우기를 잃지 않는다.
            let notified = shared.wake.notified();
            let work = {
                let mut state = shared.state.lock().map_err(|_| "예약 제어 큐 잠금 손상")?;
                if state.closed || shared.stopped.load(Ordering::Acquire) { break; }
                let work = state.queue.pop_front();
                state.active = work.clone();
                work
            };
            let Some(work) = work else { notified.await; continue; };
            {
                let mut state = shared.state.lock().map_err(|_| "예약 제어 송신 잠금 손상")?;
                let expired = *work.stage.lock().map_err(|_| "예약 제어 작업 잠금 손상")? == Stage::Expired;
                if state.closed || shared.stopped.load(Ordering::Acquire) || expired || Instant::now() >= work.deadline {
                    finish_locked(&mut state, &work, rejection(work.mutation, "requestTimeout"), true);
                    continue;
                }
                *work.stage.lock().map_err(|_| "예약 제어 작업 잠금 손상")? = Stage::Sending;
            }
            // 이 표시 뒤의 실패는 바이트 0을 입증할 수 없으므로 변경은 unknown이다.
            let reply = async {
                pipe.write_frame(&work.frame, frame_deadline(work.deadline)).await
                    .map_err(|error| io_failure("예약 제어 요청 송신 실패", error))?;
                let response = pipe.read_frame::<wire::Response>(frame_deadline(work.deadline)).await
                    .map_err(|error| io_failure("예약 제어 결과 수신 실패", error))?;
                response.validate_for(&work.request)?;
                Ok::<_, String>(response)
            }.await;
            match reply {
                Err(error) => {
                    let mut state = shared.state.lock().map_err(|_| "예약 제어 실패 상태 잠금 손상")?;
                    finish_locked(&mut state, &work, if work.mutation { Ok(unknown()) } else { Err(error.clone()) }, false);
                    return Err(error);
                }
                Ok(response) => {
                    let mut state = shared.state.lock().map_err(|_| "예약 제어 결과 상태 잠금 손상")?;
                    let result = if let Some(error) = response.error {
                        match error.code {
                            wire::ErrorCode::InvalidRequest | wire::ErrorCode::StaleTarget => rejection(work.mutation, "staleTarget"),
                            wire::ErrorCode::RequestTimeout | wire::ErrorCode::Internal => {
                                if work.mutation { Ok(unknown()) } else { Err("TUI가 예약 조회를 확정하지 못했습니다.".into()) }
                            }
                        }
                    } else {
                        let result = response.result.ok_or("예약 제어 응답에 결과가 없습니다.")?;
                        if let wire::ResultBody::List(list) = &result {
                            let scope = response.scope.ok_or("예약 조회가 실제 소유 범위를 반환하지 않았습니다.")?;
                            if let Some(pending) = &state.pending {
                                if pending.scope != scope { return Err("미결 예약 변경의 실제 범위가 바뀌었습니다.".into()); }
                                if matches!(list.mutation_state, Some(wire::MutationState::Clear))
                                    && list.reconciled_request_id.as_ref() == Some(&pending.id) { clear_pending(&mut state); }
                            }
                            state.scope = Some(scope);
                            state.supports_interrupt = list.connected && list.supported && list.supports_expected_turn_interrupt == Some(true);
                        }
                        let value = result.into_value()?;
                        if work.mutation { Ok(value) } else { bounded_cron_snapshot(value) }
                    };
                    let definitive = work.mutation && result.as_ref().is_ok_and(|value| value["status"] != "unknown");
                    finish_locked(&mut state, &work, result, definitive);
                }
            }
        }
        Ok::<(), String>(())
    }.await;
    let close = pipe.close().map_err(|error| io_failure("예약 제어 통로 정리 실패", error));
    let mut state = shared.state.lock().unwrap_or_else(|error| error.into_inner());
    let cancelled = state.closed;
    state.worker_returned = true;
    state.closed = true; state.supports_interrupt = false;
    // 인증·통신 기능 실패와 OS 자원 회수 실패는 다른 종료 결과다.
    // 정상 회수한 기능 실패가 상위 세션의 종료 완료를 막으면 안 된다.
    state.cleanup_failure = close.err();
    state.failure = if cancelled || shared.stopped.load(Ordering::Acquire) { None } else { result.err() };
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cron_control_help_requires_real_option_and_exact_possible_value() {
        let help = |item: &str| format!("Usage: codex [OPTIONS]\n\nOptions:\n      --help  도움말\n{item}\n");
        assert!(help_supports_control(help("      --desktop-cron-control <MODE>\n          [possible values: pipe-v1]").as_bytes()).unwrap());
        assert!(help_supports_control(b"Usage: codex [OPTIONS]

Options:
  -h, --help  help
      --desktop-cron-control <MODE>
          [possible values: pipe-v1]
").unwrap());
        for item in ["      --other <MODE>\n          --desktop-cron-control [possible values: pipe-v1]",
            "      --desktop-cron-control <MODE>\n          [possible values: pipe-v10]",
            "      --desktop-cron-control-fake <MODE>\n          [possible values: pipe-v1]",
            "      --other <MODE>
       --desktop-cron-control [possible values: pipe-v1]",
            "      --other <MODE>
          --desktop-cron-control <MODE>
              [possible values: pipe-v1]"] {
            assert!(!help_supports_control(help(item).as_bytes()).unwrap());
        }
        assert!(help_supports_control(help("      --desktop-cron-control <MODE>\n          설명 pipe-v1").as_bytes()).is_err());
        assert!(help_supports_control(b"failed").is_err());
    }
    fn fixture() -> CronConnection {
        tauri::async_runtime::block_on(async {
            let pipe = OwnedTuiPipe::bind().unwrap();
            let shared = Arc::new(Shared::new(Arc::new(AtomicBool::new(false)), pipe.cancellation()));
            let owner = wire::UuidText::parse("10000000-0000-0000-0000-000000000001").unwrap();
            let scope = wire::Scope {
                backend_id: wire::UuidText::parse("20000000-0000-0000-0000-000000000002").unwrap(),
                backend_epoch: wire::DecimalU64::new(1),
                thread_id: wire::UuidText::parse("30000000-0000-0000-0000-000000000003").unwrap(),
            };
            let mut state = shared.state.lock().unwrap();
            state.owner = Some(owner); state.scope = Some(scope); state.worker_returned = true;
            drop(state);
            CronConnection { shared: shared.clone(), _lifetime: Arc::new(ConnectionLifetime(shared)) }
        })
    }
    const THREAD: &str = "30000000-0000-0000-0000-000000000003";
    fn add() -> CronRequest {
        CronRequest::Add { request_id: uuid::Uuid::new_v4().to_string(),
            schedule: super::super::cron::CronSchedule::Every { seconds: 10 }, prompt: "보존할 원문
 ".into() }
    }
    fn await_count(connection: &CronConnection, count: usize) {
        let until = Instant::now() + Duration::from_secs(3);
        while connection.shared.state.lock().unwrap().count != count {
            assert!(Instant::now() < until, "예약 접수 시험의 대기 시간이 초과됐습니다.");
            std::thread::yield_now();
        }
    }
    #[test]
    fn cron_control_admission_is_bounded_and_cancel_waits_for_actual_completion() {
        let connection = fixture();
        std::thread::scope(|threads| {
            let requests: Vec<_> = (0..MAX_REQUESTS).map(|_| threads.spawn(|| connection.request(THREAD, CronRequest::List {}))).collect();
            await_count(&connection, MAX_REQUESTS);
            assert!(connection.request(THREAD, CronRequest::List {}).unwrap_err().contains("queueFull"));
            connection.cancel();
            assert!(connection.wait_until(Instant::now()).is_err(), "취소 신호만으로 완료를 보고하면 안 됩니다.");
            drop(Completion(connection.shared.clone()));
            connection.wait_until(Instant::now() + Duration::from_secs(1)).unwrap();
            for request in requests { assert!(request.join().unwrap().is_err()); }
        });
        let state = connection.shared.state.lock().unwrap();
        assert_eq!((state.count, state.bytes), (0, 0));
    }
    #[test]
    fn cron_control_cancel_cap_and_queued_cancellation_send_no_mutation() {
        let connection = fixture();
        let cancel = CronRequest::Cancel { request_id: uuid::Uuid::new_v4().to_string(), job_id: "1".into() };
        assert_eq!(connection.request(THREAD, cancel).unwrap(), json!({"status":"unsupported","reason":"expectedTurnInterruptUnsupported"}));
        assert_eq!(connection.shared.state.lock().unwrap().count, 0);
        std::thread::scope(|threads| {
            let request = threads.spawn(|| connection.request(THREAD, add()));
            await_count(&connection, 1);
            assert_eq!(connection.request(THREAD, add()).unwrap()["reason"], "unresolvedMutation");
            connection.cancel();
            drop(Completion(connection.shared.clone()));
            assert_eq!(request.join().unwrap().unwrap()["status"], "notApplied");
        });
        assert!(connection.shared.state.lock().unwrap().pending.is_none());
    }
    #[test]
    fn cron_control_sent_unknown_keeps_exact_body_and_reconcile_only() {
        let connection = fixture();
        std::thread::scope(|threads| {
            let request = threads.spawn(|| connection.request(THREAD, add()));
            await_count(&connection, 1);
            let expected = {
                let mut state = connection.shared.state.lock().unwrap();
                let work = state.queue.pop_front().unwrap();
                *work.stage.lock().unwrap() = Stage::Sending;
                let expected = state.pending.as_ref().unwrap().body.clone();
                finish_locked(&mut state, &work, Ok(unknown()), false);
                expected
            };
            assert_eq!(request.join().unwrap().unwrap()["status"], "unknown");
            assert_eq!(connection.request(THREAD, add()).unwrap()["reason"], "unresolvedMutation");
            let list = threads.spawn(|| connection.request(THREAD, CronRequest::List {}));
            await_count(&connection, 1);
            {
                let state = connection.shared.state.lock().unwrap();
                let work = state.queue.front().unwrap();
                assert_eq!(&work.frame["reconcile"]["request"], expected.as_ref());
                assert_eq!(work.request.scope, Some(state.pending.as_ref().unwrap().scope.clone()));
                assert_eq!(state.bytes, work.bytes + state.pending.as_ref().unwrap().bytes);
            }
            connection.cancel();
            drop(Completion(connection.shared.clone()));
            assert!(list.join().unwrap().is_err());
        });
        let snapshot = connection.request(THREAD, CronRequest::List {}).unwrap();
        assert_eq!(snapshot["mutationState"], "unknown");
        assert!(connection.shared.state.lock().unwrap().pending.is_some());
    }
    #[test]
    fn cron_control_serialized_byte_reservation_includes_retained_pending_body() {
        let connection = fixture();
        {
            let mut state = connection.shared.state.lock().unwrap();
            state.bytes = MAX_BYTES;
        }
        assert!(connection.request(THREAD, CronRequest::List {}).unwrap_err().contains("queueFull"));
        assert_eq!(connection.shared.state.lock().unwrap().count, 0);
    }

    // 이 결합 시험의 신원 callback은 같은 프로세스 신원만 반환한다.
    // 실제 PTY Job 소속·PID 재사용 검증은 owned_tui_control의 별도 OS 시험 책임이다.
    use serde::de::DeserializeOwned;
    use tokio::{io::{AsyncReadExt, AsyncWriteExt}, net::windows::named_pipe::{ClientOptions, NamedPipeClient}};

    struct LiveFixture {
        connection: CronConnection,
        client: Option<NamedPipeClient>,
        name: String,
        owner: String,
        scope: Value,
    }
    async fn fixture_write(client: &mut NamedPipeClient, value: &Value) {
        let bytes = serde_json::to_vec(value).unwrap();
        assert!((1..=crate::owned_tui_control::MAX_FRAME_BYTES).contains(&bytes.len()));
        tokio::time::timeout(FRAME_TIMEOUT, async {
            // 실제 byte pipe에서 헤더와 본문이 여러 쓰기로 나뉘는 경계도 지난다.
            let prefix = (bytes.len() as u32).to_le_bytes();
            client.write_all(&prefix[..2]).await.unwrap();
            client.write_all(&prefix[2..]).await.unwrap();
            for chunk in bytes.chunks(7) { client.write_all(chunk).await.unwrap(); }
        }).await.expect("시험 통로 송신 기한");
    }
    async fn fixture_read<T: DeserializeOwned>(client: &mut NamedPipeClient) -> Result<T, String> {
        tokio::time::timeout(FRAME_TIMEOUT, async {
            let mut prefix = [0; 4];
            client.read_exact(&mut prefix).await.map_err(|_| "시험 통로의 헤더 수신 실패".to_owned())?;
            let length = u32::from_le_bytes(prefix) as usize;
            if !(1..=crate::owned_tui_control::MAX_FRAME_BYTES).contains(&length) { return Err("시험 통로의 프레임 크기 오류".into()); }
            let mut bytes = vec![0; length];
            client.read_exact(&mut bytes).await.map_err(|_| "시험 통로의 본문 수신 실패".to_owned())?;
            wire::from_json::<T>(&bytes)
        }).await.map_err(|_| "시험 통로 수신 기한 초과".to_owned())?
    }
    fn begin_live_request(connection: &CronConnection, body: CronRequest) -> tokio::task::JoinHandle<Result<Value, String>> {
        let connection = connection.clone();
        // 동기 공개 호출이 입출력 실행기의 진행을 막지 않도록 시험 호출자만 blocking pool에서 실행한다.
        tokio::task::spawn_blocking(move || connection.request(THREAD, body))
    }
    impl LiveFixture {
        async fn connect(wrong_proof: bool) -> Self {
            let pipe = OwnedTuiPipe::bind().unwrap();
            let name = pipe.name().to_owned();
            let identity = OwnedProcessIdentity::current().unwrap().identity();
            let session = uuid::Uuid::new_v4().to_string();
            let owner = uuid::Uuid::new_v4().to_string();
            let bootstrap = wire::Bootstrap::new(&name, identity.pid, identity.created_filetime, &session, 1).unwrap();
            let token = wire::SecretToken::generate().unwrap();
            let token_hex = token.expose_hex();
            let preparation = CronPreparation { pipe, bootstrap: bootstrap.clone(), token, cancelled: Arc::new(AtomicBool::new(false)) };
            let connection = preparation.start(Arc::new(move |_| Ok(identity))).unwrap();
            let client = ClientOptions::new().open(&name).unwrap();
            let mut fixture = Self { connection, client: Some(client), name, owner: owner.clone(), scope: json!({
                "backendId":uuid::Uuid::new_v4().to_string(),"backendEpoch":"1","threadId":THREAD
            }) };
            let hello_value = json!({"v":1,"type":"hello","sessionId":session,"sessionGeneration":"1",
                "ownerInstanceId":owner,"clientPid":identity.pid,"clientCreatedFiletime":identity.created_filetime.to_string(),
                "clientNonce":"0123456789abcdef0123456789abcdef"});
            let hello: wire::Hello = serde_json::from_value(hello_value.clone()).unwrap();
            fixture_write(fixture.client.as_mut().unwrap(), &hello_value).await;
            let challenge: wire::Challenge = fixture_read(fixture.client.as_mut().unwrap()).await.unwrap();
            let auth = wire::fixture_client_auth(token_hex.as_str(), &bootstrap, &hello, &challenge).unwrap();
            let mut auth_value = serde_json::to_value(auth).unwrap();
            if wrong_proof {
                let proof = auth_value["proof"].as_str().unwrap();
                let replacement = if proof.starts_with('0') { "1" } else { "0" };
                auth_value["proof"] = json!(format!("{replacement}{}", &proof[1..]));
            }
            fixture_write(fixture.client.as_mut().unwrap(), &auth_value).await;
            if wrong_proof {
                assert!(fixture_read::<wire::Ready>(fixture.client.as_mut().unwrap()).await.is_err());
                fixture.await_finished().await;
            } else {
                let ready: wire::Ready = fixture_read(fixture.client.as_mut().unwrap()).await.unwrap();
                assert_eq!(ready.owner_instance_id.as_str(), fixture.owner);
                let until = Instant::now() + FRAME_TIMEOUT;
                while fixture.connection.shared.state.lock().unwrap().owner.is_none() {
                    assert!(Instant::now() < until, "인증 뒤 소유 상태 공개 기한 초과");
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            }
            fixture
        }
        async fn read_request(&mut self) -> Value {
            fixture_read::<Value>(self.client.as_mut().unwrap()).await.unwrap()
        }
        fn envelope(&self, request: &Value, result: Value) -> Value {
            json!({"v":1,"type":"response","id":request["id"],"ownerInstanceId":self.owner,"scope":self.scope,"result":result})
        }
        async fn respond(&mut self, request: &Value, result: Value) {
            let envelope = self.envelope(request, result);
            fixture_write(self.client.as_mut().unwrap(), &envelope).await;
        }
        fn list_result(&self, supports_interrupt: bool) -> Value {
            json!({"supported":true,"connected":true,"scope":"cliLifetime","jobs":[],"hasMore":false,
                "supportsExpectedTurnInterrupt":supports_interrupt})
        }
        async fn bind_scope(&mut self, supports_interrupt: bool) {
            let request = begin_live_request(&self.connection, CronRequest::List {});
            let frame = self.read_request().await;
            assert_eq!(frame["type"], "request");
            assert_eq!(frame["targetThreadId"], THREAD);
            assert!(frame["scope"].is_null());
            self.respond(&frame, self.list_result(supports_interrupt)).await;
            assert_eq!(request.await.unwrap().unwrap()["connected"], true);
        }
        async fn no_request_bytes(&mut self) {
            let mut byte = [0];
            let result = tokio::time::timeout(Duration::from_millis(30), self.client.as_mut().unwrap().read(&mut byte)).await;
            assert!(result.is_err(), "로컬 거절 또는 확정 뒤 재송신이 통로 바이트를 만들었습니다.");
        }
        async fn await_finished(&self) {
            let until = Instant::now() + FRAME_TIMEOUT;
            while !self.connection.shared.state.lock().unwrap().finished {
                assert!(Instant::now() < until, "실제 제어 작업 종료 기한 초과");
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            self.connection.wait_until(Instant::now()).expect("기능 실패 뒤에도 정상 회수한 작업의 종료는 성공해야 합니다.");
        }
    }
    impl Drop for LiveFixture {
        fn drop(&mut self) {
            self.connection.cancel();
            drop(self.client.take());
            // 통신 기능의 성공 여부와 별도로 실제 작업·핸들의 회수를 기다린다.
            let _ = self.connection.wait_until(Instant::now() + FRAME_TIMEOUT);
            if !std::thread::panicking() {
                assert!(self.connection.shared.state.lock().unwrap().finished, "시험 제어 작업을 회수하지 못했습니다.");
            }
        }
    }
    fn live_add() -> (CronRequest, Value) {
        let value = json!({"type":"add","requestId":uuid::Uuid::new_v4().to_string(),
            "schedule":{"type":"every","seconds":10},"prompt":"  원문\n두 번째 줄  \t"});
        (serde_json::from_value(value.clone()).unwrap(), value)
    }
    #[test]
    fn cron_control_real_pipe_auth_list_cancel_gate_and_exact_add_round_trip() {
        tauri::async_runtime::block_on(async {
            let mut fixture = LiveFixture::connect(false).await;
            fixture.bind_scope(false).await;
            let cancel = CronRequest::Cancel { request_id: uuid::Uuid::new_v4().to_string(), job_id: "1".into() };
            assert_eq!(fixture.connection.request(THREAD, cancel).unwrap()["status"], "unsupported");
            fixture.no_request_bytes().await;
            let (body, expected) = live_add();
            let request = begin_live_request(&fixture.connection, body);
            let frame = fixture.read_request().await;
            assert_eq!(frame["body"], expected);
            assert_eq!(frame["id"], expected["requestId"]);
            assert_eq!(frame["scope"], fixture.scope);
            fixture.respond(&frame, json!({"status":"applied","jobId":"1"})).await;
            assert_eq!(request.await.unwrap().unwrap()["status"], "applied");
            assert!(fixture.connection.shared.state.lock().unwrap().pending.is_none());
            fixture.no_request_bytes().await;
        });
    }
    #[test]
    fn cron_control_real_pipe_disconnect_keeps_unknown_without_reconnection() {
        tauri::async_runtime::block_on(async {
            let mut fixture = LiveFixture::connect(false).await;
            fixture.bind_scope(false).await;
            let (body, expected) = live_add();
            let request = begin_live_request(&fixture.connection, body);
            let frame = fixture.read_request().await;
            assert_eq!(frame["body"], expected);
            drop(fixture.client.take());
            assert_eq!(request.await.unwrap().unwrap()["status"], "unknown");
            fixture.await_finished().await;
            assert_eq!(fixture.connection.request(THREAD, add()).unwrap()["reason"], "unresolvedMutation");
            assert_eq!(fixture.connection.request(THREAD, CronRequest::List {}).unwrap()["mutationState"], "unknown");
            assert_eq!(fixture.connection.shared.state.lock().unwrap().pending.as_ref().unwrap().body.as_ref(), &expected);
            assert!(ClientOptions::new().open(&fixture.name).is_err(), "종료된 일회 통로를 다시 열면 안 됩니다.");
        });
    }
    #[test]
    fn cron_control_real_pipe_unknown_resolves_only_with_exact_explicit_receipt() {
        tauri::async_runtime::block_on(async {
            let mut fixture = LiveFixture::connect(false).await;
            fixture.bind_scope(false).await;
            let (body, expected) = live_add();
            let request = begin_live_request(&fixture.connection, body);
            let frame = fixture.read_request().await;
            fixture.respond(&frame, json!({"status":"unknown"})).await;
            assert_eq!(request.await.unwrap().unwrap()["status"], "unknown");
            assert_eq!(fixture.connection.request(THREAD, add()).unwrap()["reason"], "unresolvedMutation");
            for clear in [false, true] {
                let list = begin_live_request(&fixture.connection, CronRequest::List {});
                let frame = fixture.read_request().await;
                assert_eq!(frame["scope"], fixture.scope);
                assert_eq!(frame["reconcile"]["request"], expected);
                let mut result = fixture.list_result(false);
                result["mutationState"] = json!(if clear { "clear" } else { "unknown" });
                if clear { result["reconciledRequestId"] = expected["requestId"].clone(); }
                fixture.respond(&frame, result).await;
                assert_eq!(list.await.unwrap().unwrap()["mutationState"], if clear { "clear" } else { "unknown" });
                assert_eq!(fixture.connection.shared.state.lock().unwrap().pending.is_none(), clear);
            }
            let (body, _) = live_add();
            let request = begin_live_request(&fixture.connection, body);
            let frame = fixture.read_request().await;
            fixture.respond(&frame, json!({"status":"applied","jobId":"2"})).await;
            assert_eq!(request.await.unwrap().unwrap()["status"], "applied");
        });
    }
    #[test]
    fn cron_control_real_pipe_wrong_response_binding_never_confirms_mutation() {
        tauri::async_runtime::block_on(async {
            for field in ["ownerInstanceId", "scope", "id"] {
                let mut fixture = LiveFixture::connect(false).await;
                fixture.bind_scope(false).await;
                let (body, _) = live_add();
                let request = begin_live_request(&fixture.connection, body);
                let frame = fixture.read_request().await;
                let mut response = fixture.envelope(&frame, json!({"status":"applied","jobId":"1"}));
                if field == "scope" { response["scope"]["backendEpoch"] = json!("2"); }
                else { response[field] = json!(uuid::Uuid::new_v4().to_string()); }
                fixture_write(fixture.client.as_mut().unwrap(), &response).await;
                assert_eq!(request.await.unwrap().unwrap()["status"], "unknown");
                fixture.await_finished().await;
                assert!(fixture.connection.shared.state.lock().unwrap().pending.is_some());
                assert_eq!(fixture.connection.request(THREAD, add()).unwrap()["reason"], "unresolvedMutation");
            }
        });
    }
    #[test]
    fn cron_control_real_pipe_wrong_auth_proof_never_enables_requests() {
        tauri::async_runtime::block_on(async {
            let fixture = LiveFixture::connect(true).await;
            assert!(fixture.connection.shared.state.lock().unwrap().owner.is_none());
            assert_eq!(fixture.connection.request(THREAD, add()).unwrap()["status"], "notApplied");
            assert_eq!(fixture.connection.request(THREAD, CronRequest::List {}).unwrap()["connected"], false);
            assert!(fixture.connection.shared.state.lock().unwrap().pending.is_none());
        });
    }

    #[test]
    fn cron_control_real_pipe_unrelated_receipt_cannot_clear_unknown() {
        tauri::async_runtime::block_on(async {
            let mut fixture = LiveFixture::connect(false).await;
            fixture.bind_scope(false).await;
            let (body, expected) = live_add();
            let request = begin_live_request(&fixture.connection, body);
            let frame = fixture.read_request().await;
            fixture.respond(&frame, json!({"status":"unknown"})).await;
            assert_eq!(request.await.unwrap().unwrap()["status"], "unknown");
            let list = begin_live_request(&fixture.connection, CronRequest::List {});
            let frame = fixture.read_request().await;
            assert_eq!(frame["reconcile"]["request"], expected);
            let mut result = fixture.list_result(false);
            result["mutationState"] = json!("clear");
            result["reconciledRequestId"] = json!(uuid::Uuid::new_v4().to_string());
            fixture.respond(&frame, result).await;
            assert!(list.await.unwrap().is_err());
            fixture.await_finished().await;
            assert!(fixture.connection.shared.state.lock().unwrap().pending.is_some());
            assert_eq!(fixture.connection.request(THREAD, add()).unwrap()["reason"], "unresolvedMutation");
        });
    }

    #[test]
    fn cron_control_wait_distinguishes_feature_failure_cleanup_failure_and_incomplete_worker() {
        let connection = fixture();
        assert!(connection.wait_until(Instant::now()).is_err());
        {
            let mut state = connection.shared.state.lock().unwrap();
            state.finished = true;
            state.failure = Some("시험 인증 실패".into());
        }
        connection.wait_until(Instant::now()).expect("정상 회수한 기능 실패는 세션 종료를 막으면 안 됩니다.");
        {
            let mut state = connection.shared.state.lock().unwrap();
            state.cleanup_failure = Some("시험 통로 회수 실패".into());
        }
        assert_eq!(connection.wait_until(Instant::now()).unwrap_err(), "시험 통로 회수 실패");
    }
    #[test]
    fn cron_control_abnormal_worker_completion_remains_a_cleanup_error() {
        let connection = fixture();
        connection.shared.state.lock().unwrap().worker_returned = false;
        drop(Completion(connection.shared.clone()));
        assert!(connection.shared.state.lock().unwrap().finished);
        assert!(connection.wait_until(Instant::now()).unwrap_err().contains("정상 반환 전에"));
    }

}
