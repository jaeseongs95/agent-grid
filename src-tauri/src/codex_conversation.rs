//! 기존 서버·대화의 구조화 연결. 입력과 응답은 한 작업자가 소유하며 재전송하지 않는다.
use super::*;

#[path = "codex_subagents.rs"]
mod subagents;
pub use subagents::SubagentRequest;
use std::{collections::{HashMap, HashSet, VecDeque}, path::PathBuf, sync::Weak};

const EVENT_COUNT: usize = 128;
const EVENT_BYTES: usize = 1024 * 1024;
const REQUEST_COUNT: usize = 32;
const TEXT_PREVIEW: usize = 4096;
const DETAIL_BYTES: usize = 16 * 1024;
const MAX_CONTEXT_INTEGER: i64 = 9_007_199_254_740_991;
const PASSIVE_SUBSCRIBE: &str = "thread/subscribePassive";
const START_USER_IF_IDLE: &str = "turn/startUserIfIdle";
const COMPACT_IF_IDLE: &str = "thread/compact/startIfIdle";
const HISTORY_UNAVAILABLE: &str = "대화 이력 원문을 아직 조회할 수 없습니다. 빈 이력으로 처리하지 않았습니다.";

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum ConversationRequest {
    Attach,
    Detach,
    HandoffToTerminal,
    Poll,
    ContextStatus,
    Compact,
    Page { cursor: Option<String> },
    Items { turn_id: String, cursor: Option<String> },
    Detail { turn_id: String, item_id: String, offset: Option<usize>, snapshot_id: Option<String> },
    Models { cursor: Option<String> },
    Send { request_id: String, text: String, #[serde(default)] attachments: Vec<Attachment>, model: Option<String>, effort: Option<String> },
    Interrupt { turn_id: String },
    Respond { request_id: String, result: Value },
}
impl ConversationRequest {
    pub(crate) fn validate(&self) -> Result<(), String> { validate_request(self) }
}
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Attachment { pub r#type: String, pub path: String }

#[derive(Clone)]
pub(super) enum Endpoint {
    Shared(shared::SharedEndpoint),
    Owned { address: String, token: String, job: Weak<ProcessJob> },
}

#[derive(Clone)]
pub struct ConversationConnection(Arc<Control>);
struct Control {
    endpoint: Endpoint,
    stopped: Arc<AtomicBool>,
    // 동일 잠금 아래 입력 주체를 바꾸고 PTY 쓰기를 시작한다.
    input: Mutex<bool>,
    owned: AtomicBool,
    worker: Mutex<Option<Worker>>,
    events: Mutex<Events>,
    sent: Mutex<HashSet<String>>,
    unknown: AtomicBool,
    deleted: AtomicBool,
    detail: Mutex<Option<DetailSnapshot>>,
    subagents: Mutex<subagents::State>,
}
struct DetailSnapshot { id: String, turn_id: String, item_id: String, text: Arc<str> }
struct Worker {
    sender: mpsc::SyncSender<Work>,
    cancelled: Arc<AtomicBool>,
    transport: Arc<Mutex<Transport>>,
    join: Option<JoinHandle<()>>,
    joined: bool,
    closing: Option<Uuid>,
}
impl Worker {
    fn stop(&mut self) -> Result<(),String> {
        if self.joined {return Ok(());}
        self.cancelled.store(true,Ordering::Release);
        let transport=self.transport.lock().map_err(|_| "대화 연결 정리 상태 오류")?;
        if let Some(socket)=&transport.socket {let _=socket.shutdown(Shutdown::Both);}
        if let Some(job)=transport.proxy.as_ref().and_then(Weak::upgrade) {let _=job.terminate();}
        drop(transport);
        self.join.take().ok_or("대화 작업자의 종료 결과를 다시 확인할 수 없습니다.")?.join().map_err(|_| "대화 작업자가 비정상 종료되었습니다.".to_owned())?;
        self.joined=true;Ok(())
    }
}
#[derive(Default)]
struct Transport { socket: Option<TcpStream>, proxy: Option<Weak<ProcessJob>> }
struct DeadlineWatch { done: mpsc::Sender<()>, join: Option<JoinHandle<()>> }
impl DeadlineWatch {
    fn start(deadline: Arc<Mutex<Option<Instant>>>, transport: Arc<Mutex<Transport>>, cancelled: Arc<AtomicBool>, stopped: Arc<AtomicBool>) -> Result<Self,String> {
        let (done,receiver) = mpsc::channel();
        let join = thread::Builder::new().name("codex-conversation-deadline".into()).spawn(move || {
            loop {
                let expired = deadline.lock().unwrap_or_else(|e| e.into_inner()).is_some_and(|value| Instant::now() >= value);
                if expired || cancelled.load(Ordering::Acquire) || stopped.load(Ordering::Acquire) {
                    cancelled.store(true,Ordering::Release);
                    let transport = transport.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(socket) = &transport.socket { let _ = socket.shutdown(Shutdown::Both); }
                    if let Some(job) = transport.proxy.as_ref().and_then(Weak::upgrade) { let _ = job.terminate(); }
                    break;
                }
                match receiver.recv_timeout(Duration::from_millis(100)) {Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,Err(mpsc::RecvTimeoutError::Timeout) => {}}
            }
        }).map_err(|e| format!("대화 연결 기한 감시자를 시작할 수 없습니다: {e}"))?;
        Ok(Self{done,join:Some(join)})
    }
}
impl Drop for DeadlineWatch {
    fn drop(&mut self) { let _ = self.done.send(()); if let Some(join) = self.join.take() { let _ = join.join(); } }
}
struct SendContext { session_id: String, store: crate::conversation_attachments::ConversationAttachments }
struct Work { request: ConversationRequest, reply: mpsc::SyncSender<Result<Value, String>>, deadline: Instant, send_context: Option<SendContext> }
#[derive(Default)]
struct CompactProgress { turn: Option<(String,bool)> }
impl CompactProgress {
    fn observe(&mut self,thread_id: &str,message: &Value) {
        let params=&message["params"];if params["threadId"].as_str()!=Some(thread_id) {return;}
        let Some(turn)=params["turn"]["id"].as_str().or_else(||params["turnId"].as_str()).filter(|id|valid_id(id).is_ok()) else {return;};
        match message["method"].as_str() {
            Some("turn/started")=>{if self.turn.as_ref().is_some_and(|(id,done)|id==turn&&*done){return;}self.turn=Some((turn.into(),false));},
            Some("turn/completed")=>{if self.turn.as_ref().is_none_or(|(id,_)|id==turn){self.turn=Some((turn.into(),true));}},
            Some("item/started" | "item/completed") if params["item"]["type"]=="contextCompaction"=>{
                if self.turn.as_ref().is_some_and(|(id,done)|id==turn&&*done){return;}
                if params["item"]["id"].as_str().is_some_and(|id|valid_id(id).is_ok()) {self.turn=Some((turn.into(),false));}
            },
            _=>{},
        }
    }
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
struct ContextTokenUsage {
    total: ContextTokenBreakdown,
    last: ContextTokenBreakdown,
    model_context_window: Option<i64>,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
struct ContextTokenBreakdown {
    total_tokens: i64, input_tokens: i64, cached_input_tokens: i64,
    #[serde(default)] cache_write_input_tokens: i64,
    output_tokens: i64, reasoning_output_tokens: i64,
}
impl ContextTokenBreakdown {
    fn valid(&self) -> bool {
        [self.total_tokens,self.input_tokens,self.cached_input_tokens,self.cache_write_input_tokens,self.output_tokens,self.reasoning_output_tokens].into_iter().all(|value|(0..=MAX_CONTEXT_INTEGER).contains(&value))
    }
}
#[derive(Default)]
struct Events {
    queue: VecDeque<(Value, usize)>, bytes: usize, seq: u64, gap: bool,
    pending: HashMap<String, Value>, connected: bool, error: Option<String>,
    answered: HashSet<String>,
    observed_pending: HashMap<String,Value>, observer_connected: bool,
    requests_unavailable: bool,
    token_usage: Option<ContextTokenUsage>, context_active: Option<bool>, token_usage_fresh: bool,
    context_compaction: Option<(String,String)>, context_revision: u64,
    context_activity_revision: u64, context_turn: Option<String>, gui_compact_supported: bool, observer_compact_supported: bool,
    gui_interrupt_supported: bool, interrupt_diagnostic: Option<&'static str>,
}
impl Events {
    fn blocks_attach(&self,owned: bool,unknown: bool) -> bool {
        owned || unknown || self.blocks_release(true) || self.pending.values().any(|request|request["guiOwned"]!=false)
    }
    fn blocks_release(&self,handoff: bool) -> bool {
        self.requests_unavailable || self.pending.iter().chain(self.observed_pending.iter()).any(|(id,request)| {
            !handoff || request["responded"]==true || self.answered.contains(id)
                || !matches!(request["method"].as_str(),Some("item/commandExecution/requestApproval" | "item/fileChange/requestApproval" | "item/tool/requestUserInput" | "item/permissions/requestApproval"))
        })
    }
    fn push(&mut self, mut event: Value) {
        self.seq += 1;
        event["seq"] = json!(self.seq);
        let bytes = event.to_string().len();
        if bytes > EVENT_BYTES { self.gap = true; return; }
        while self.queue.len() >= EVENT_COUNT || self.bytes + bytes > EVENT_BYTES {
            if let Some((_, size)) = self.queue.pop_front() { self.bytes -= size; self.gap = true; }
            else { break; }
        }
        self.bytes += bytes;
        self.queue.push_back((event, bytes));
    }
    fn poll(&mut self) -> Value {
        let events: Vec<_> = self.queue.drain(..).map(|(value, _)| value).collect();
        self.bytes = 0;
        let gap = std::mem::take(&mut self.gap);
        json!({"events":events,"gap":gap,"pendingRequests":self.pending.values().collect::<Vec<_>>(),"connected":self.connected,"error":self.error})
    }
}
impl ConversationConnection {
    pub(crate) fn approval_decisions(params: &Value) -> Vec<&'static str> {
        match params.get("availableDecisions") {
            None=>vec!["accept","decline","cancel"],
            Some(Value::Array(advertised))=>["accept","acceptForSession","decline","cancel"].into_iter()
                .filter(|choice|advertised.iter().any(|value|value.as_str()==Some(*choice))).collect(),
            Some(_)=>Vec::new(),
        }
    }
    pub(super) fn new(endpoint: Endpoint, stopped: Arc<AtomicBool>) -> Self {
        Self(Arc::new(Control {endpoint,stopped,input:Mutex::new(false),owned:AtomicBool::new(false),worker:Mutex::new(None),events:Mutex::new(Events::default()),sent:Mutex::new(HashSet::new()),unknown:AtomicBool::new(false),deleted:AtomicBool::new(false),detail:Mutex::new(None),subagents:Mutex::new(subagents::State::default())}))
    }
    pub(crate) fn terminal_write<T>(&self, write: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
        let gui = self.0.input.lock().map_err(|_| "대화 입력 소유 상태를 확인할 수 없습니다.")?;
        if *gui { return Err("대화 화면이 입력을 소유하고 있습니다. 터미널로 전환한 뒤 입력하세요.".into()); }
        write()
    }
    pub(crate) fn pending_snapshot(&self) -> Result<(bool,bool,Vec<Value>),String> {
        let owned = self.0.owned.load(Ordering::Acquire);
        let events = self.0.events.lock().map_err(|_| "대화 승인 상태 오류")?;
        let mut requests = events.observed_pending.clone();
        requests.extend(events.pending.clone());
        for (id,request) in &mut requests {request["metadataAvailable"]=json!(!events.requests_unavailable&&((events.connected&&events.pending.contains_key(id))||(events.observer_connected&&events.observed_pending.contains_key(id))));}
        Ok((owned,events.connected,requests.into_values().collect()))
    }
    fn context_status(&self, thread_id: &str) -> Result<Value,String> {
        // 작업자 종료 join을 기다리지 않는 읽기다. 연결·이력·이벤트 큐를 건드리지 않는다.
        let closing=match self.0.worker.try_lock() {
            Ok(worker)=>worker.as_ref().is_some_and(|worker|worker.closing.is_some()),
            Err(std::sync::TryLockError::WouldBlock)=>true,
            Err(std::sync::TryLockError::Poisoned(_))=>return Err("대화 연결 상태 오류".into()),
        } || self.0.stopped.load(Ordering::Acquire);
        let events=self.0.events.lock().map_err(|_| "대화 문맥 상태 오류")?;
        let reason=context_block_reason(&self.0,&events,closing);
        Ok(json!({"threadId":thread_id,"connected":events.connected||events.observer_connected,"guiConnected":events.connected,
            "tokenUsage":events.token_usage,"tokenUsageFresh":events.token_usage_fresh&&events.observer_connected&&!self.0.unknown.load(Ordering::Acquire),"compacting":events.context_compaction.is_some(),"active":events.context_active,
            "canCompact":reason.is_none(),"reason":reason,"revision":events.context_revision}))
    }
    pub(crate) fn is_deleted(&self) -> bool { self.0.deleted.load(Ordering::Acquire) }
    pub(super) fn observer_message(&self,thread_id: &str,message: Value) -> Result<(),String> {record_message(&self.0,thread_id,message,true)}
    pub(super) fn observer_call<S: Read+Write>(&self,rpc: &mut Rpc<S>,thread_id: &str,method: &str,params: Value) -> Result<Value,String> {
        call_observed(&self.0,rpc,thread_id,method,params,self.0.stopped.as_ref(),true,None,None,None)
    }
    // false는 실제 JSON-RPC의 정확한 준비 오류에서만 반환한다. 문자열이나
    // 성공 응답의 유사 필드로 재시도를 지시하지 않는다.
    pub(super) fn observer_subscribe<S: Read+Write>(&self,rpc: &mut Rpc<S>,thread_id: &str,cwd: &Path) -> Result<bool,String> {
        {let mut events=self.0.events.lock().map_err(|_| "질문 관측 상태 오류")?;events.observer_compact_supported=false;events.context_revision+=1;}
        let mut preparing=false;
        let result=match call_observed(&self.0,rpc,thread_id,PASSIVE_SUBSCRIBE,json!({"threadId":thread_id}),self.0.stopped.as_ref(),true,None,Some(&mut preparing),None) {
            Ok(value)=>value,Err(_) if preparing=>return Ok(false),Err(error)=>return Err(error),
        };
        verify_passive(&result,thread_id,cwd)?;
        let mut events=self.0.events.lock().map_err(|_| "질문 관측 상태 오류")?;
        if events.requests_unavailable {return Err("요청 관측을 중단한 세션의 구독을 복구하지 않았습니다.".into());}
        events.context_active=context_active(&result["thread"]["status"]);
        events.context_turn=None;events.context_activity_revision+=1;
        events.observer_compact_supported=result["supportsCompactIfIdle"].as_bool()==Some(true);
        events.token_usage_fresh=false;events.context_compaction=None;
        events.context_revision+=1;
        events.observer_connected=true;
        Ok(true)
    }
    pub(super) fn observer_disconnected(&self) {
        let mut events=self.0.events.lock().unwrap_or_else(|e|e.into_inner());
        events.observer_connected=false;events.token_usage_fresh=false;events.context_revision+=1;
        events.observer_compact_supported=false;
        events.context_active=None;events.context_compaction=None;
        events.context_turn=None;events.context_activity_revision+=1;
    }
    pub(crate) fn cancel(&self) {
        let mut slot = self.0.worker.lock().unwrap_or_else(|e| e.into_inner());
        let worker = slot.take();
        if let Some(mut worker) = worker {
            if let Err(error)=worker.stop() {self.0.events.lock().unwrap_or_else(|e|e.into_inner()).error=Some(error);}
        }
        {let mut events=self.0.events.lock().unwrap_or_else(|e|e.into_inner());events.gui_compact_supported=false;events.gui_interrupt_supported=false;events.interrupt_diagnostic=None;}
        *self.0.input.lock().unwrap_or_else(|e| e.into_inner()) = false;
        self.0.owned.store(false,Ordering::Release);
        self.0.detail.lock().unwrap_or_else(|e|e.into_inner()).take();
    }
    fn release(&self,finished_only: bool,handoff: bool) -> Result<bool,String> {
        let mut slot=self.0.worker.lock().map_err(|_| "대화 연결 상태 오류")?;
        let Some(worker)=slot.as_mut() else {return if self.0.owned.load(Ordering::Acquire) {Err("대화 입력 소유권의 종료 근거가 없습니다.".into())} else {Ok(true)};};
        if finished_only && !worker.joined && !worker.join.as_ref().is_some_and(JoinHandle::is_finished) {return Ok(false);}
        // worker의 종료 콜백도 events를 잠그므로 join 중에는 events/input을 잡지 않는다.
        if let Err(error)=worker.stop() {
            let mut events=self.0.events.lock().map_err(|_| "대화 승인 상태 오류")?;
            events.connected=false;events.gui_compact_supported=false;events.gui_interrupt_supported=false;events.interrupt_diagnostic=None;events.error=Some(error.clone());return Err(error);
        }
        let mut events=self.0.events.lock().map_err(|_| "대화 승인 상태 오류")?;
        if events.blocks_release(handoff) || self.0.unknown.load(Ordering::Acquire) {
            return Err("미확인 요청 또는 승인 대기가 남아 대화 입력 소유권을 유지했습니다.".into());
        }
        // join 뒤 두 맵의 최신 사본을 같은 잠금에서 재검사한다. 명시 인계의
        // 일반 미응답 pending은 해결/삭제하지 않고 원래 TUI의 응답을 기다린다.
        *self.0.input.lock().map_err(|_| "대화 입력 소유 상태 오류")?=false;
        self.0.owned.store(false,Ordering::Release);
        events.gui_compact_supported=false;events.gui_interrupt_supported=false;events.interrupt_diagnostic=None;
        for request in events.pending.values_mut() {request["guiOwned"]=json!(false);}
        slot.take();
        self.0.detail.lock().map_err(|_| "대화 상세 스냅샷 상태 오류")?.take();
        Ok(true)
    }
    #[cfg(test)]
    pub(crate) fn request(&self, thread_id: &str, cwd: &Path, request: ConversationRequest) -> Result<Value, String> {
        self.request_context(thread_id,cwd,request,None)
    }
    pub(crate) fn request_for_session(&self,thread_id: &str,cwd: &Path,request: ConversationRequest,session_id: &str,store: crate::conversation_attachments::ConversationAttachments) -> Result<Value,String> {
        let context=matches!(request,ConversationRequest::Send{..}).then(||SendContext{session_id:session_id.to_owned(),store});
        self.request_context(thread_id,cwd,request,context)
    }
    fn request_context(&self, thread_id: &str, cwd: &Path, request: ConversationRequest, send_context: Option<SendContext>) -> Result<Value, String> {
        validate_request(&request)?;
        if matches!(request, ConversationRequest::ContextStatus) {
            return self.context_status(thread_id);
        }
        if matches!(request, ConversationRequest::Poll) {
            let mut poll = self.0.events.lock().map_err(|_| "대화 이벤트 상태를 확인할 수 없습니다.")?.poll();
            poll["unknownResult"] = json!(self.0.unknown.load(Ordering::Acquire)); return Ok(poll);
        }
        // 해제도 같은 단일 작업자의 요청으로 직렬화한다. 승인/결과 불명 상태에서는
        // 연결과 입력 소유권을 보존해 터미널의 두 번째 응답을 막는다.
        if matches!(request, ConversationRequest::Detach | ConversationRequest::HandoffToTerminal) {
            let handoff=matches!(request,ConversationRequest::HandoffToTerminal);
            if self.release(true,handoff)? {return Ok(json!({}));}
            let (reply, receiver) = mpsc::sync_channel(1);
            let claim=Uuid::new_v4();
            {
                let mut worker = self.0.worker.lock().map_err(|_| "대화 연결 상태 오류")?;
                if let Some(worker) = worker.as_mut() {
                    if worker.closing.is_some() {return Err("대화 입력 인계를 이미 확인 중입니다. 입력 소유권을 유지했습니다.".into());}
                    worker.closing=Some(claim);
                    if worker.sender.try_send(Work {request,reply,deadline:Instant::now()+RPC_TIMEOUT,send_context:None}).is_err() {
                        worker.closing=None;return Err("대화 해제 요청을 전달할 수 없습니다. 입력 소유권을 유지했습니다.".into());
                    }
                } else { return Ok(json!({})); }
            }
            let result = receiver.recv_timeout(RPC_TIMEOUT+Duration::from_secs(2)).map_err(|_| "대화 해제 결과를 확인할 수 없어 입력 소유권을 유지했습니다.")?;
            let result=match result {Ok(value)=>value,Err(error)=>{
                let mut worker=self.0.worker.lock().map_err(|_| "대화 연결 상태 오류")?;
                if let Some(worker)=worker.as_mut() {
                    if worker.closing==Some(claim) && !worker.joined && !worker.cancelled.load(Ordering::Acquire) && worker.join.as_ref().is_some_and(|join|!join.is_finished()) {worker.closing=None;}
                }
                return Err(error);
            }};
            if !self.release(false,handoff)? {return Err("대화 입력 소유권의 해제를 확인할 수 없습니다.".into());}
            return Ok(result);
        }
        if self.0.stopped.load(Ordering::Acquire) { return Err("대화 세션이 종료되었습니다.".into()); }
        if self.0.deleted.load(Ordering::Acquire) { return Err("원래 대화가 삭제되었습니다. 다른 대화를 만들거나 재개하지 않았습니다.".into()); }
        let mut worker = self.0.worker.lock().map_err(|_| "대화 연결 상태를 확인할 수 없습니다.")?;
        if matches!(request, ConversationRequest::Attach) {
            if worker.is_some() { return Err("이미 대화 화면이 연결되어 있습니다.".into()); }
            {
                let mut events = self.0.events.lock().map_err(|_| "대화 이벤트 상태 오류")?;
                if events.blocks_attach(self.0.owned.load(Ordering::Acquire),self.0.unknown.load(Ordering::Acquire)) {return Err("이전 연결의 미확인 요청을 보존하고 있어 새 입력 소유권을 만들지 않았습니다.".into());}
                events.queue.clear(); events.bytes=0; events.gap=false; events.error=None;
                events.gui_compact_supported=false;events.gui_interrupt_supported=false;events.interrupt_diagnostic=None;
                events.context_revision+=1;
            }
            *self.0.input.lock().map_err(|_| "대화 입력 소유 상태를 확인할 수 없습니다.")? = true;
            self.0.owned.store(true,Ordering::Release);
            let (sender, receiver) = mpsc::sync_channel(8);
            let cancelled = Arc::new(AtomicBool::new(false));
            let transport = Arc::new(Mutex::new(Transport::default()));
            let control = self.0.clone();
            let thread_id = thread_id.to_owned(); let cwd = cwd.to_path_buf();
            let worker_cancelled = cancelled.clone(); let worker_transport = transport.clone();
            let join = match thread::Builder::new().name("codex-conversation".into()).spawn(move || {
                let result = run(control.clone(), &thread_id, &cwd, receiver, worker_cancelled, worker_transport);
                let mut events = control.events.lock().unwrap_or_else(|e| e.into_inner());
                events.connected = false;events.gui_compact_supported=false;events.gui_interrupt_supported=false;events.interrupt_diagnostic=None;events.context_revision+=1;
                if let Err(error) = result { events.error = Some(error); }
            }) { Ok(join) => join, Err(error) => { *self.0.input.lock().unwrap_or_else(|e| e.into_inner()) = false; self.0.owned.store(false,Ordering::Release); return Err(format!("대화 연결 작업자를 시작할 수 없습니다: {error}")); } };
            *worker = Some(Worker {sender,cancelled,transport,join:Some(join),joined:false,closing:None});
        }
        if worker.as_ref().is_some_and(|worker|worker.closing.is_some()) {return Err("대화 입력을 터미널로 인계 중이라 새 요청을 받지 않았습니다.".into());}
        if let ConversationRequest::Send {request_id,..} = &request {
            Uuid::parse_str(request_id).map_err(|_| "전송 식별자는 UUID여야 합니다.")?;
            let mut sent = self.0.sent.lock().map_err(|_| "대화 전송 기록을 확인할 수 없습니다.")?;
            if sent.contains(request_id) { return Err("이미 처리한 전송 식별자입니다. 결과가 불명확한 요청을 재전송하지 않았습니다.".into()); }
            // 삭제한 식별자의 재전송을 허용하지 않으므로 상한에 도달하면 명시적으로 거부한다.
            if sent.len() >= 1024 { return Err("이 세션의 전송 식별자 상한에 도달했습니다. 세션을 다시 여세요.".into()); }
            sent.insert(request_id.clone());
        }
        let (reply, receiver) = mpsc::sync_channel(1);
        worker.as_ref().ok_or("대화 화면을 먼저 연결하세요.")?.sender.try_send(Work {request,reply,deadline:Instant::now() + RPC_TIMEOUT,send_context})
            .map_err(|error| match error {mpsc::TrySendError::Full(_) => "대화 요청 대기열이 가득 찼습니다. 요청을 보내지 않았습니다.",mpsc::TrySendError::Disconnected(_) => "대화 연결이 끊겼습니다. 전송 결과는 확인되지 않았습니다."})?;
        drop(worker);
        receiver.recv_timeout(RPC_TIMEOUT + Duration::from_secs(2)).map_err(|_| "대화 요청 응답을 확인할 수 없습니다. 요청을 재전송하지 않았습니다.".to_string())?
    }
}

fn run(control: Arc<Control>, thread_id: &str, cwd: &Path, receiver: mpsc::Receiver<Work>, cancelled: Arc<AtomicBool>, transport: Arc<Mutex<Transport>>) -> Result<(), String> {
    let deadline = Arc::new(Mutex::new(Some(Instant::now()+RPC_TIMEOUT)));
    let _watch = DeadlineWatch::start(deadline.clone(),transport.clone(),cancelled.clone(),control.stopped.clone())?;
    let result = match &control.endpoint {
        Endpoint::Shared(endpoint) => {
            let mut proxy = endpoint.open(Some(cancelled.clone()))?;
            transport.lock().map_err(|_| "대화 연결 정리 상태 오류")?.proxy = proxy.job.as_ref().map(Arc::downgrade);
            // 취소 감시자는 세션 연결 수명 동안 유지해 포화된 파이프 쓰기도 회수한다.
            let mut rpc = proxy.rpc.take().unwrap();
            let result = serve(&control, &mut rpc, thread_id, cwd, receiver, &cancelled, &deadline);
            drop(rpc); drop(proxy); result
        }
        Endpoint::Owned {address,token,job} => {
            let address_socket = address.trim_start_matches("ws://").parse().map_err(|_| "대화 서버 주소가 올바르지 않습니다.")?;
            let stream = TcpStream::connect_timeout(&address_socket, Duration::from_secs(2)).map_err(|e| format!("기존 대화 서버 연결 실패: {e}"))?;
            let job = job.upgrade().ok_or("기존 대화 서버가 종료되었습니다.")?;
            job.verify_tcp_peer(&stream).map_err(|_| "기존 대화 서버의 연결 소유권을 확인할 수 없습니다.")?;
            drop(job);
            transport.lock().map_err(|_| "대화 연결 정리 상태 오류")?.socket = Some(stream.try_clone().map_err(|e| e.to_string())?);
            let mut rpc = authenticated_socket(address, token, stream)?;
            rpc.cancelled = Some(cancelled.clone());
            serve(&control, &mut rpc, thread_id, cwd, receiver, &cancelled, &deadline)
        }
    };
    let mut transport = transport.lock().map_err(|_| "대화 연결 정리 상태 오류")?;
    if let Some(socket) = transport.socket.take() { let _ = socket.shutdown(Shutdown::Both); }
    transport.proxy.take();
    result
}
fn live(control: &Control, cancelled: &AtomicBool) -> Result<(), String> {
    if control.deleted.load(Ordering::Acquire) { Err("원래 대화가 삭제되어 연결과 이력 조회를 종료했습니다.".into()) }
    else if control.stopped.load(Ordering::Acquire) || cancelled.load(Ordering::Acquire) { Err("대화 연결이 종료되었습니다. 진행 중 요청 결과는 확인되지 않았습니다.".into()) }
    else if control.events.lock().map_err(|_| "대화 승인 상태 오류")?.requests_unavailable {Err("해결 기록 상한으로 요청 관측을 중단했습니다. 마지막 확인 내용을 보존했습니다.".into())} else { Ok(()) }
}
fn verify_passive(result: &Value,thread_id: &str,cwd: &Path) -> Result<(),String> {
    verified_thread(&result["thread"],thread_id,cwd)?;
    interrupt_capability(result)?;
    if result.get("supportsCompactIfIdle").is_some_and(|value|!value.is_boolean()) {
        return Err("수동 구독 응답의 압축 지원 정보가 bool이 아니어서 연결을 중단했습니다.".into());
    }
    if !result["thread"]["turns"].as_array().is_some_and(Vec::is_empty)
        || !result["initialTurnsPage"].is_null() || !result["turnsBackwardsCursor"].is_null() || !result["itemsBackwardsCursor"].is_null() {
        return Err("수동 구독 응답에 이력이 포함되었거나 형식이 올바르지 않아 연결을 중단했습니다.".into());
    }
    Ok(())
}
fn interrupt_capability(result: &Value) -> Result<bool,String> {
    match result.get("supportsExpectedTurnInterrupt") {
        None=>Ok(false),Some(Value::Bool(supported))=>Ok(*supported),
        Some(_)=>Err("서버 응답의 안전한 작업 중단 지원 정보 형식이 올바르지 않아 연결을 중단했습니다.".into()),
    }
}
fn call<S: Read + Write>(control: &Control, rpc: &mut Rpc<S>, thread_id: &str, method: &str, params: Value, cancelled: &AtomicBool) -> Result<Value, String> {
    call_observed(control,rpc,thread_id,method,params,cancelled,false,None,None,None)
}
fn call_observed<S: Read+Write>(control: &Control,rpc: &mut Rpc<S>,thread_id: &str,method: &str,params: Value,cancelled: &AtomicBool,metadata_only: bool,send_context: Option<(&SendContext,&[Attachment])>,preparing: Option<&mut bool>,mut compact_progress: Option<&mut CompactProgress>) -> Result<Value,String> {
    live(control, cancelled)?;
    check_cancelled(rpc.cancelled.as_ref())?;
    if rpc.deadline.is_some_and(|deadline|Instant::now()>=deadline) {return Err("대화 요청 기한이 지나 전송하지 않았습니다.".into());}
    let mutation = matches!(method,START_USER_IF_IDLE | COMPACT_IF_IDLE | "turn/interrupt");
    let previous_unknown = if mutation {control.unknown.swap(true,Ordering::AcqRel)} else {false};
    // 입력·작업 상태·큐·취소·기한 검사가 끝난 실제 송신 시도 경계에서만 보존한다.
    // 이 뒤의 실패는 일부 바이트 송신 가능성이 있어 파일과 결과 불명을 보존한다.
    let mut retention=if let Some((context,attachments))=send_context {
        match context.store.retain_for_send(&context.session_id,&attachments.iter().map(|item|item.path.clone()).collect::<Vec<_>>()) {
            Ok(receipt)=>Some((context,receipt)),
            Err(error)=>{control.unknown.store(previous_unknown,Ordering::Release);return Err(error);},
        }
    } else {None};
    rpc.next_id += 1; let id = rpc.next_id;
    // 최종 사전 검사 뒤의 송신 시도다. retain 뒤에 Rpc::send의 사전 검사를
    // 다시 실행해 확정 미전송 오류로 바꾸지 않는다. 진행 기한 감시자는 이
    // 경계 뒤의 포화 쓰기·취소를 소유 transport 종료로 해제한다.
    rpc.ws.send(Message::Text(json!({"id":id,"method":method,"params":params}).to_string().into()))
        .map_err(|error|format!("대화 {method} 송신 시도에 실패했습니다. 요청 결과는 확인되지 않았습니다: {error}"))?;
    let deadline = rpc.deadline.unwrap_or_else(|| Instant::now()+RPC_TIMEOUT).min(Instant::now()+RPC_TIMEOUT);
    while Instant::now() < deadline {
        live(control, cancelled)?;
        if let Some(message) = rpc.read()? {
            if let Some(result) = response(&message, id) {
                // 송신 후 internal 오류는 수락 뒤 실패일 수도 있다. 명시 미시작 또는
                // 메서드/파라미터 검증 거절 외에는 결과 불명과 첨부를 보존한다.
                let rejected=matches!(message["error"]["code"].as_i64(),Some(-32601 | -32602))
                    || (method==START_USER_IF_IDLE && matches!(message["result"]["status"].as_str(),Some("skippedBusy" | "notStarted")))
                    || (method==COMPACT_IF_IDLE && message.get("error").is_none() && (message["result"]["status"]=="skippedBusy" || (message["result"]["status"]=="notStarted"&&message["result"]["reason"].is_string())));
                let accepted=if matches!(method,START_USER_IF_IDLE | COMPACT_IF_IDLE) {
                    (method!=COMPACT_IF_IDLE||message.get("error").is_none()) && message["result"]["status"]=="started" && message["result"]["turn"]["id"].as_str().is_some_and(|id|!id.is_empty()&&(method!=COMPACT_IF_IDLE||valid_id(id).is_ok()))
                } else {message.get("result").is_some()};
                if mutation && (rejected || accepted) { control.unknown.store(previous_unknown,Ordering::Release); }
                if rejected {
                    if let Some((context,receipt))=retention.take() {context.store.rollback_known_rejection(&context.session_id,receipt)?;}
                }
                if method==COMPACT_IF_IDLE && matches!(message["error"]["code"].as_i64(),Some(-32601 | -32602)) {
                    return Ok(if message["error"]["code"]==-32601 {json!({"status":"unsupported","reason":"methodUnsupported"})}else{json!({"status":"notStarted","reason":"invalidParams"})});
                }
                if metadata_only && method==PASSIVE_SUBSCRIBE && message["error"]["code"].as_i64()==Some(-32600)
                    && message["error"]["data"]["kind"].as_str()==Some("threadSubscriptionUnavailable")
                    && matches!(message["error"]["data"]["reason"].as_str(),Some("notLoaded" | "listenerNotReady")) {
                    if let Some(preparing)=preparing {*preparing=true;}
                    return result;
                }
                if message["error"]["code"] == -32601 && matches!(method,PASSIVE_SUBSCRIBE | START_USER_IF_IDLE) {
                    return Err(format!("현재 Codex 서버가 안전한 대화 기능({method})을 지원하지 않습니다. 호환되는 서버가 필요합니다. 기존 재개·전송 방식으로 대체하지 않았습니다."));
                }
                if method==PASSIVE_SUBSCRIBE && message["error"]["code"].as_i64()==Some(-32600) && message["error"]["data"].is_null() {
                    return Err(format!("현재 서버의 안전한 수동 구독을 확인할 수 없습니다. 호환되는 서버가 필요합니다. 재시도하거나 기존 재개 방식으로 대체하지 않았습니다. {}",result.as_ref().err().map(String::as_str).unwrap_or("응답 오류")));
                }
                if message["error"]["code"].as_i64()==Some(-32600) && message["error"]["data"]["kind"].as_str()==Some("historyProjectionUnavailable")
                    && message["error"]["data"]["reason"].as_str()==Some("unmaterialized") {
                    if matches!(method,"thread/turns/list" | "thread/items/list" | "thread/items/read") {
                        return Ok(json!({"historyUnavailable":true,"reason":message["error"]["data"]["reason"]}));
                    }
                    return Err(HISTORY_UNAVAILABLE.into());
                }
                if matches!(method,"thread/turns/list" | "thread/items/list" | "thread/items/read") && message["result"].get("historyUnavailable").is_some() {
                    return Err("서버 성공 응답의 이력 대기 표식을 거부했습니다.".into());
                }
                return result;
            }
            if let Some(progress)=compact_progress.as_deref_mut() {progress.observe(thread_id,&message);}
            record_message(control, thread_id, message, metadata_only)?;
        }
    }
    Err(format!("대화 {method} 응답 시간이 초과되었습니다. 요청 결과는 불명확하며 재전송하지 않았습니다."))
}
fn serve<S: Read + Write>(control: &Control, rpc: &mut Rpc<S>, thread_id: &str, cwd: &Path, receiver: mpsc::Receiver<Work>, cancelled: &AtomicBool, deadline: &Mutex<Option<Instant>>) -> Result<(), String> {
    {let mut events=control.events.lock().map_err(|_| "대화 문맥 상태 오류")?;events.gui_compact_supported=false;events.gui_interrupt_supported=false;events.interrupt_diagnostic=None;}
    rpc.deadline = *deadline.lock().map_err(|_| "대화 요청 기한 상태 오류")?;
    let mut init = initialize(); init["capabilities"] = json!({"experimentalApi":true});
    let initialized=call(control, rpc, thread_id, "initialize", init, cancelled)?;
    let initialize_interrupt=interrupt_capability(&initialized)?;
    rpc.send(json!({"method":"initialized","params":{}}))?;
    // loaded 동일 runtime의 수동 구독만 허용한다. resume는 idle 작업을 실행할 수 있어 대체 경로로 쓰지 않는다.
    let mut resumed = call(control, rpc, thread_id, PASSIVE_SUBSCRIBE, json!({"threadId":thread_id}), cancelled)?;
    verify_passive(&resumed, thread_id, cwd)?;
    // CLI는 응답 뒤 복사된 pending을 replay한다. 기존 Attach 전체기한 안에서
    // 실제 동일 원문 또는 resolved를 기다리며 별도 큐/재송신은 만들지 않는다.
    let replay_deadline=rpc.deadline.unwrap_or_else(||Instant::now()+RPC_TIMEOUT);
    loop {
        live(control,cancelled)?;
        let events=control.events.lock().map_err(|_| "대화 승인 상태 오류")?;
        if events.blocks_release(true) || control.unknown.load(Ordering::Acquire) {return Err("재구독 중 미확인 요청이 확인되어 입력 소유권을 유지했습니다.".into());}
        let waiting=events.pending.values().any(|request|request["guiOwned"]==false);drop(events);
        if !waiting {break;}
        if Instant::now()>=replay_deadline {return Err("기한 안에 인계한 요청의 동일 원문을 확인하지 못했습니다. 이전 요청을 보존했습니다.".into());}
        if let Some(message)=rpc.read()? {observe_message(control,thread_id,message)?;}
    }
    resumed["threadId"] = json!(thread_id); resumed["owner"] = json!("gui");
    let current = call(control,rpc,thread_id,"thread/turns/list",json!({"threadId":thread_id,"limit":1,"sortDirection":"desc","itemsView":"summary"}),cancelled)?;
    if current["historyUnavailable"] == true {
        resumed["historyUnavailable"] = json!(true); resumed["activeTurnKnown"] = json!(false); resumed["activeTurnId"] = Value::Null;
    } else {
        let current = current["data"].as_array().filter(|values| values.len() <= 1).ok_or("현재 대화 작업 목록이 요청 상한을 넘었습니다.")?;
        resumed["activeTurnKnown"] = json!(true);
        resumed["activeTurnId"] = current.first().filter(|turn| turn["status"] == "inProgress").and_then(|turn| turn["id"].as_str()).map_or(Value::Null,|id|json!(id));
    }
    { let mut events = control.events.lock().map_err(|_| "대화 이벤트 상태 오류")?; if events.requests_unavailable {return Err("요청 관측을 중단한 세션의 연결을 복구하지 않았습니다.".into());} events.gui_compact_supported=resumed["supportsCompactIfIdle"].as_bool()==Some(true);
        let passive_interrupt=interrupt_capability(&resumed)?;
        // 이 GUI 연결에서 두 응답을 모두 읽었으므로 양쪽의 명시 지원만 채택한다.
        // 다른 관찰자 연결의 값이나 압축 지원 값은 중단 권한이 아니다.
        events.gui_interrupt_supported=initialize_interrupt&&passive_interrupt;
        events.interrupt_diagnostic=(initialize_interrupt!=passive_interrupt).then_some("현재 연결의 안전한 작업 중단 지원 응답이 서로 일치하지 않아 요청을 보내지 않았습니다.");
        events.connected = true;events.context_revision+=1; events.error = None; }
    rpc.deadline = None; *deadline.lock().map_err(|_| "대화 요청 기한 상태 오류")? = None;
    loop {
        live(control, cancelled)?;
        match receiver.try_recv() {
            Ok(work) => {
                if Instant::now() >= work.deadline { let _ = work.reply.send(Err("대기 중 대화 요청의 기한이 지나 전송하지 않았습니다.".into())); continue; }
                if matches!(work.request,ConversationRequest::Detach | ConversationRequest::HandoffToTerminal) {
                    let handoff=matches!(work.request,ConversationRequest::HandoffToTerminal);
                    let pending = control.events.lock().map_err(|_| "대화 승인 상태 오류")?.blocks_release(handoff);
                    if pending || control.unknown.load(Ordering::Acquire) {
                        let _ = work.reply.send(Err("승인 대기 또는 결과 불명 요청이 있어 대화 입력 소유권을 유지했습니다. 현재 화면에서 요청을 처리하세요.".into()));
                        continue;
                    }
                    let _ = work.reply.send(Ok(json!({}))); return Ok(());
                }
                let result = match work.request {
                    ConversationRequest::Attach => Ok(resumed.clone()),
                    request => {
                        rpc.deadline = Some(work.deadline); *deadline.lock().map_err(|_| "대화 요청 기한 상태 오류")? = Some(work.deadline);
                        let result = execute_context(control, rpc, thread_id, request, cancelled, work.send_context.as_ref());
                        rpc.deadline = None; *deadline.lock().map_err(|_| "대화 요청 기한 상태 오류")? = None;
                        result
                    },
                };
                let _ = work.reply.send(result);
            }
            Err(mpsc::TryRecvError::Disconnected) => return Ok(()),
            Err(mpsc::TryRecvError::Empty) => if let Some(message) = rpc.read()? { observe_message(control, thread_id, message)?; },
        }
    }
}
fn valid_id(value: &str) -> Result<(), String> {
    if value.is_empty() || value.len() > 256 || value.chars().any(char::is_control) { Err("대화 항목 식별자가 올바르지 않습니다.".into()) } else { Ok(()) }
}
fn cursor(value: &Option<String>) -> Result<(), String> {
    if value.as_ref().is_some_and(|v| v.len() > 4096) { Err("대화 페이지 위치가 상한을 넘었습니다.".into()) } else { Ok(()) }
}
fn validate_request(request: &ConversationRequest) -> Result<(), String> {
    match request {
        ConversationRequest::Page {cursor: value} | ConversationRequest::Models {cursor: value} => cursor(value),
        ConversationRequest::Items {turn_id,cursor: value} => {valid_id(turn_id)?;cursor(value)},
        ConversationRequest::Detail {turn_id,item_id,snapshot_id,..} => {valid_id(turn_id)?;valid_id(item_id)?;if let Some(id)=snapshot_id {valid_id(id)?;}Ok(())},
        ConversationRequest::Interrupt {turn_id} => valid_id(turn_id),
        ConversationRequest::Send {request_id,text,attachments,model,effort} => {
            Uuid::parse_str(request_id).map_err(|_| "전송 식별자는 UUID여야 합니다.")?;
            if text.len() > 64*1024 || attachments.len() > 16 || attachments.iter().any(|item| item.path.len() > 32768 || item.r#type.len() > 32)
                || model.as_ref().is_some_and(|v| v.is_empty() || v.len() > 256) || effort.as_ref().is_some_and(|v| v.len() > 64) {
                return Err("대화 전송 요청이 허용 크기를 넘었습니다. 대기열에 넣지 않았습니다.".into());
            }
            Ok(())
        },
        ConversationRequest::Respond {request_id,result} => {
            if request_id.len() > 256 || !result.is_object() || result.to_string().len() > 64*1024 {return Err("대화 응답 요청이 허용 크기를 넘었습니다. 대기열에 넣지 않았습니다.".into());}
            Ok(())
        },
        _ => Ok(()),
    }
}
fn bounded_page(mut value: Value, limit: usize) -> Result<Value, String> {
    if value["historyUnavailable"] == true { return Err(HISTORY_UNAVAILABLE.into()); }
    if value["data"].as_array().is_none_or(|data| data.len() > limit) { return Err("대화 페이지가 요청 상한을 넘었거나 형식이 올바르지 않습니다.".into()); }
    summarize(&mut value);
    Ok(value)
}
fn summarize(value: &mut Value) {
    match value {
        Value::String(text) if text.len() > TEXT_PREVIEW => *value = json!({"__detail":true,"bytes":text.len()}),
        Value::Array(values) => for value in values { summarize(value); },
        Value::Object(values) => for value in values.values_mut() { summarize(value); },
        _ => {},
    }
}
#[cfg(test)]
fn execute<S: Read + Write>(control: &Control, rpc: &mut Rpc<S>, thread_id: &str, request: ConversationRequest, cancelled: &AtomicBool) -> Result<Value, String> {
    execute_context(control,rpc,thread_id,request,cancelled,None)
}
fn execute_context<S: Read + Write>(control: &Control, rpc: &mut Rpc<S>, thread_id: &str, request: ConversationRequest, cancelled: &AtomicBool,send_context: Option<&SendContext>) -> Result<Value, String> {
    match request {
        ConversationRequest::Page {cursor: page_cursor} => {
            cursor(&page_cursor)?;
            let value=call(control,rpc,thread_id,"thread/turns/list",json!({"threadId":thread_id,"cursor":page_cursor,"limit":20,"sortDirection":"desc","itemsView":"summary"}),cancelled)?;
            if page_cursor.is_none() && value["historyUnavailable"]==true && value["reason"]=="unmaterialized" {Ok(value)} else {bounded_page(value,20)}
        },
        ConversationRequest::Items {turn_id,cursor: page_cursor} => { valid_id(&turn_id)?; cursor(&page_cursor)?; bounded_page(call(control,rpc,thread_id,"thread/items/list",json!({"threadId":thread_id,"turnId":turn_id,"cursor":page_cursor,"limit":20,"sortDirection":"desc"}),cancelled)?,20) },
        ConversationRequest::Detail {turn_id,item_id,offset,snapshot_id} => {
            valid_id(&turn_id)?; valid_id(&item_id)?;
            let offset=offset.unwrap_or(0);
            if offset==0 {
                control.detail.lock().map_err(|_| "대화 상세 스냅샷 상태 오류")?.take();
                let result = call(control,rpc,thread_id,"thread/items/read",json!({"threadId":thread_id,"turnId":turn_id,"itemIds":[item_id]}),cancelled)?;
                if result["historyUnavailable"] == true { return Err(HISTORY_UNAVAILABLE.into()); }
                let data = result["data"].as_array().filter(|data| data.len() == 1).ok_or("상세 대화 항목을 찾을 수 없습니다.")?;
                if data[0]["turnId"] != turn_id || data[0]["item"]["id"] != item_id { return Err("다른 대화 항목의 상세 응답을 거부했습니다.".into()); }
                let text=serde_json::to_string(&data[0]["item"]).map_err(|e|e.to_string())?;
                if text.len()>MAX_FRAME {return Err("상세 항목이 8MiB 프로토콜 상한을 넘었습니다. 원문을 잘라내지 않았습니다.".into());}
                *control.detail.lock().map_err(|_| "대화 상세 스냅샷 상태 오류")?=Some(DetailSnapshot{id:Uuid::new_v4().to_string(),turn_id:turn_id.clone(),item_id:item_id.clone(),text:Arc::from(text)});
            }
            let (id,text)={
                let detail=control.detail.lock().map_err(|_| "대화 상세 스냅샷 상태 오류")?;
                let detail=detail.as_ref().filter(|detail| detail.turn_id==turn_id&&detail.item_id==item_id&&(offset==0||snapshot_id.as_ref()==Some(&detail.id))).ok_or("상세 스냅샷이 교체되었거나 만료되었습니다. 처음부터 다시 조회하세요.")?;
                (detail.id.clone(),detail.text.clone())
            };
            let mut page=detail_chunk(&text,offset)?; page["snapshotId"]=json!(id);
            if page["nextOffset"].is_null() {control.detail.lock().map_err(|_| "대화 상세 스냅샷 상태 오류")?.take();}
            Ok(page)
        },
        ConversationRequest::Compact => {
            live(control,cancelled)?;
            let events=control.events.lock().map_err(|_| "대화 문맥 상태 오류")?;
            if let Some(reason)=context_block_reason(control,&events,false) {return Ok(json!({"status":if reason=="unsupported" {"unsupported"}else{"notStarted"},"reason":reason}));}
            let activity=events.context_activity_revision;drop(events);
            let mut progress=CompactProgress::default();
            let result=call_observed(control,rpc,thread_id,COMPACT_IF_IDLE,json!({"threadId":thread_id}),cancelled,false,None,None,Some(&mut progress));
            let mut events=control.events.lock().map_err(|_| "대화 문맥 상태 오류")?;
            let value=match result {Ok(value)=>value,Err(error)=>{events.token_usage_fresh=false;events.context_revision+=1;return Err(error);}};
            match value["status"].as_str() {
                Some("unsupported") if !control.unknown.load(Ordering::Acquire)=>{events.gui_compact_supported=false;events.context_revision+=1;Ok(value)},
                Some("skippedBusy")=>Ok(value),
                Some("notStarted") if value["reason"].is_string()=>Ok(value),
                Some("started") if value["turn"]["id"].as_str().is_some_and(|id|valid_id(id).is_ok())=>{
                    let turn=value["turn"]["id"].as_str().unwrap();
                    // 같은 관찰자의 더 최근 상태를 접수 응답으로 되돌리지 않는다.
                    // GUI에서 먼저 온 알림은 이번 RPC 동안의 작은 요약으로만 대조한다.
                    if events.context_activity_revision==activity {
                        match progress.turn {
                            // 화면에서 먼저 온 알림의 활동/완료를 관찰자 전역 상태로
                            // 승격하지 않는다. 원래 관찰자가 확인할 때까지 미확인이다.
                            Some(_)=>{events.context_active=None;events.context_turn=None;events.context_compaction=None;},
                            None=>{events.context_active=Some(true);events.context_turn=Some(turn.into());events.context_compaction=Some((turn.into(),String::new()));},
                        }
                        events.token_usage_fresh=false;events.context_activity_revision+=1;
                    }
                    events.context_revision+=1;Ok(value)
                },
                _=>{control.unknown.store(true,Ordering::Release);events.token_usage_fresh=false;events.context_revision+=1;Err("압축 접수 결과를 확인할 수 없습니다. 자동으로 다시 요청하지 않았습니다.".into())},
            }
        },
        ConversationRequest::Models {cursor: page_cursor} => { cursor(&page_cursor)?; bounded_page(call(control,rpc,thread_id,"model/list",json!({"cursor":page_cursor,"limit":50,"includeHidden":false}),cancelled)?,50) },
        ConversationRequest::Send {request_id,text,attachments,model,effort} => {
            if control.unknown.load(Ordering::Acquire) { return Err("이전 전송 결과가 불명확하여 새 전송을 차단했습니다. 원래 대화 상태를 먼저 확인하세요.".into()); }
            let input = user_input(&text, &attachments)?;
            if model.as_ref().is_some_and(|v| v.is_empty() || v.len() > 256) || effort.as_ref().is_some_and(|v| v.len() > 64) { return Err("모델 또는 추론 설정이 올바르지 않습니다.".into()); }
            // 서버의 원자적 사용자 입장만 사용한다. 상태 조회와 start 사이의 외부 입력 경쟁을 만들지 않는다.
            let mut params = json!({"threadId":thread_id,"input":input,"clientUserMessageId":request_id});
            if let Some(model) = model { params["model"] = json!(model); }
            if let Some(effort) = effort { params["effort"] = json!(effort); }
            let result=call_observed(control,rpc,thread_id,START_USER_IF_IDLE,params,cancelled,false,send_context.map(|context|(context,attachments.as_slice())),None,None)?;
            match result["status"].as_str() {
                Some("started") if result["turn"]["id"].as_str().is_some_and(|id| !id.is_empty()) => Ok(result),
                Some("skippedBusy") => Err("다른 입력이 먼저 시작되어 새 메시지를 보내지 않았습니다. 초안을 보존했습니다.".into()),
                Some("notStarted") => Err(format!("새 메시지가 수락되지 않았습니다. 초안을 보존했습니다. {}",result["reason"].as_str().unwrap_or("사유 미확인"))),
                _ => {control.unknown.store(true,Ordering::Release);Err("서버의 새 메시지 접수 결과 형식이 올바르지 않아 실행 여부를 확인할 수 없습니다. 재전송하지 않았습니다.".into())},
            }
        },
        ConversationRequest::Interrupt {turn_id} => {
            valid_id(&turn_id)?;live(control,cancelled)?;
            {let events=control.events.lock().map_err(|_| "대화 중단 지원 상태 오류")?;
                if !events.connected || !events.gui_interrupt_supported {return Err(events.interrupt_diagnostic.unwrap_or("현재 연결에서 안전한 작업 중단을 지원하는지 확인되지 않아 요청을 보내지 않았습니다.").into());}
            }
            call(control,rpc,thread_id,"turn/interrupt",json!({"threadId":thread_id,"turnId":turn_id}),cancelled)
        },
        ConversationRequest::Respond {request_id,result} => {
            let mut events = control.events.lock().map_err(|_| "대화 승인 상태 오류")?;
            let request = events.pending.get(&request_id).ok_or("이미 해결되었거나 이 연결이 소유하지 않은 요청입니다.")?;
            if request["responded"] == true { return Err("이미 응답을 보낸 요청입니다. 같은 응답을 다시 보내지 않았습니다.".into()); }
            validate_response(request, &result)?;
            if events.answered.len() >= 1024 { return Err("서버 요청 응답 기록 상한에 도달해 추가 응답을 보내지 않았습니다.".into()); }
            // 전송 전에 소유권을 소비한다. 쓰기 실패·연결 단절 후 같은 응답을 재전송하지 않는다.
            events.pending.get_mut(&request_id).unwrap()["responded"] = json!(true);
            events.answered.insert(request_id.clone()); drop(events);
            let id: Value = serde_json::from_str(&request_id).map_err(|_| "승인 요청 식별자 오류")?;
            rpc.send(json!({"id":id,"result":result})).map_err(|_| "승인 응답 전송 결과를 확인할 수 없습니다. 다시 보내지 않았습니다.".to_string())?;
            Ok(json!({}))
        },
        _ => Err("이 대화 요청은 현재 연결에서 실행할 수 없습니다.".into()),
    }
}
fn user_input(text: &str, attachments: &[Attachment]) -> Result<Vec<Value>, String> {
    if text.len() > 64 * 1024 || attachments.len() > 16 { return Err("메시지 또는 첨부 개수가 상한을 넘었습니다.".into()); }
    let mut input = Vec::new();
    if !text.trim().is_empty() { input.push(json!({"type":"text","text":text,"text_elements":[]})); }
    for attachment in attachments {
        if !matches!(attachment.r#type.as_str(), "localImage" | "file") { return Err("이 첨부 형식은 지원하지 않습니다. 로컬 이미지 또는 파일 참조만 사용할 수 있습니다.".into()); }
        if attachment.path.len() > 32768 || attachment.path.chars().any(char::is_control) { return Err("첨부 경로가 올바르지 않습니다.".into()); }
        let path = PathBuf::from(&attachment.path);
        if !path.is_absolute() || !path.is_file() { return Err("첨부는 존재하는 로컬 파일의 절대 경로여야 합니다.".into()); }
        if attachment.r#type == "localImage" {
            if !path.extension().and_then(|v| v.to_str()).is_some_and(|v| matches!(v.to_ascii_lowercase().as_str(), "png" | "jpg" | "jpeg" | "webp" | "gif")) { return Err("지원하지 않는 로컬 이미지 형식입니다.".into()); }
            input.push(json!({"type":"localImage","path":path}));
        } else {
            input.push(json!({"type":"mention","name":path.file_name().and_then(|s| s.to_str()).unwrap_or("file"),"path":attachment.path}));
        }
    }
    if input.is_empty() { return Err("전송할 메시지 또는 첨부가 없습니다.".into()); }
    Ok(input)
}
fn detail_chunk(text: &str, offset: usize) -> Result<Value, String> {
    if offset > text.len() || !text.is_char_boundary(offset) { return Err("상세 조회 위치가 올바르지 않습니다.".into()); }
    let mut end = (offset + DETAIL_BYTES).min(text.len());
    while !text.is_char_boundary(end) { end -= 1; }
    Ok(json!({"text":&text[offset..end],"nextOffset":if end < text.len() {Some(end)} else {None},"totalBytes":text.len()}))
}
fn context_active(status: &Value) -> Option<bool> {
    match status["type"].as_str() {Some("idle")=>Some(false),Some("active")=>Some(true),_=>None}
}
fn context_block_reason(control: &Control, events: &Events, closing: bool) -> Option<&'static str> {
    if control.deleted.load(Ordering::Acquire) {Some("deleted")}
    else if closing {Some("closing")}
    else if control.unknown.load(Ordering::Acquire) {Some("unknownResult")}
    else if events.requests_unavailable || !events.pending.is_empty() || !events.observed_pending.is_empty() {Some("pendingRequest")}
    else if events.context_compaction.is_some() {Some("compacting")}
    else if !events.connected && !events.observer_connected {Some("disconnected")}
    else if !events.connected {Some(if events.observer_compact_supported {"guiDisconnected"}else{"unsupported"})}
    else if events.context_active==Some(true) {Some("active")}
    else if events.context_active.is_none() {Some("statusUnknown")}
    else if !events.gui_compact_supported {Some("unsupported")}
    else {None}
}
fn observe_message(control: &Control, thread_id: &str, message: Value) -> Result<(), String> {
    record_message(control,thread_id,message,false)
}
fn record_message(control: &Control, thread_id: &str, message: Value, metadata_only: bool) -> Result<(), String> {
    let Some(method) = message["method"].as_str() else { return Ok(()); };
    if metadata_only && message.get("id").is_none() && !matches!(method,"serverRequest/resolved" | "thread/deleted" | "thread/tokenUsage/updated" | "thread/settings/updated" | "model/rerouted" | "thread/status/changed" | "turn/started" | "turn/completed" | "item/started" | "item/completed") {return Ok(());}
    let params = &message["params"];
    let selected = params["threadId"].as_str().or_else(|| params["thread"]["id"].as_str());
    if selected != Some(thread_id) { return Ok(()); }
    let mut events = control.events.lock().map_err(|_| "대화 이벤트 상태 오류")?;
    if method == "thread/deleted" {
        if control.deleted.swap(true,Ordering::AcqRel) { return Err("원래 대화가 이미 삭제되었습니다.".into()); }
        events.pending.clear(); events.observed_pending.clear(); events.connected=false; events.observer_connected=false;events.gui_compact_supported=false;events.observer_compact_supported=false;events.gui_interrupt_supported=false;events.interrupt_diagnostic=None;
        events.token_usage=None;events.context_active=None;events.context_compaction=None;events.context_turn=None;events.context_activity_revision+=1;events.context_revision+=1;
        events.queue.clear(); events.bytes=0;
        events.error=Some("원래 대화가 삭제되었습니다. 이전 이력과 상세 캐시를 사용하지 않습니다.".into());
        events.push(json!({"method":method,"params":params}));
        drop(events);
        control.detail.lock().map_err(|_| "대화 상세 스냅샷 상태 오류")?.take();
        // 요청 결과 불명은 삭제 알림만으로 해결됐다고 가정하지 않는다. 소유권은 worker join 후 해제한다.
        return Err("원래 대화가 삭제되어 수동 구독을 종료했습니다.".into());
    }
    // 사용량과 그 무효화는 기존 관찰 스트림 하나만 환원한다. GUI 복사본을
    // 섞지 않아 서로 다른 연결의 도착 순번을 전역 최신 순서로 오인하지 않는다.
    if metadata_only && message.get("id").is_none() {
        match method {
            "thread/tokenUsage/updated" => {
                let usage=params["turnId"].as_str().filter(|id|valid_id(id).is_ok()).and_then(|_|serde_json::from_value::<ContextTokenUsage>(params["tokenUsage"].clone()).ok())
                    .filter(|usage|usage.total.valid()&&usage.last.valid()&&usage.model_context_window.map_or(true,|window|(1..=MAX_CONTEXT_INTEGER).contains(&window)));
                events.token_usage_fresh=usage.as_ref().is_some_and(|usage|usage.model_context_window.is_some())&&events.context_compaction.is_none();
                events.token_usage=usage;events.context_revision+=1;
            },
            "thread/settings/updated" | "model/rerouted" => {events.token_usage_fresh=false;events.context_revision+=1;},
            "thread/status/changed" => {events.context_active=context_active(&params["status"]);events.context_activity_revision+=1;events.context_revision+=1;},
            "turn/started" => {events.context_active=Some(true);events.context_turn=params["turn"]["id"].as_str().filter(|id|valid_id(id).is_ok()).map(str::to_owned);events.context_activity_revision+=1;events.context_revision+=1;},
            "turn/completed" => {
                let turn=params["turn"]["id"].as_str().or_else(||params["turnId"].as_str());
                if events.context_compaction.as_ref().is_some_and(|(id,_)|Some(id.as_str())==turn) {events.token_usage_fresh=false;events.context_compaction=None;}
                if events.context_compaction.is_none() && events.context_turn.as_deref().is_none_or(|id|Some(id)==turn) {events.context_active=Some(false);}
                events.context_activity_revision+=1;events.context_revision+=1;
            },
            "item/started" | "item/completed" if params["item"]["type"]=="contextCompaction" => {
                if let (Some(turn),Some(item))=(params["turnId"].as_str(),params["item"]["id"].as_str()) {
                    if valid_id(turn).is_ok()&&valid_id(item).is_ok() {
                        let key=(turn.to_owned(),item.to_owned());
                        events.token_usage_fresh=false;
                        if method=="item/started" {events.context_compaction=Some(key);}
                        else if events.context_compaction.as_ref().is_some_and(|(id,old_item)|id==turn&&(old_item.is_empty()||old_item==item)) {events.context_compaction=None;}
                        events.context_activity_revision+=1;events.context_revision+=1;
                    }
                }
            },
            _=>{},
        }
    }
    if metadata_only && message.get("id").is_none() && method!="serverRequest/resolved" {return Ok(());}
    if method == "serverRequest/resolved" {
        if let Some(id) = params.get("requestId") {
            let key=id.to_string();
            if key.len()>256 || (!id.is_string()&&!id.is_i64()&&!id.is_u64()) {return Err("해결 요청 식별자 형식을 확인할 수 없어 관측을 중단했습니다.".into());}
            events.pending.remove(&key);events.observed_pending.remove(&key);
            // replay는 서버가 복사한 뒤 이미 해결됐을 수 있다. 안전한 eviction 근거가
            // 없으므로 해결 ID를 세션 동안 보존하고 상한에서는 명시 중단한다.
            if !events.answered.contains(&key) && events.answered.len()>=1024 {
                let error="해결 요청 기록 상한에 도달해 늦은 사본을 구분할 수 없습니다. 관측을 중단했습니다.";
                events.requests_unavailable=true;events.connected=false;events.observer_connected=false;events.gui_compact_supported=false;events.observer_compact_supported=false;events.gui_interrupt_supported=false;events.interrupt_diagnostic=None;events.error=Some(error.into());return Err(error.into());
            }
            events.answered.insert(key);
        }
    }
    if events.requests_unavailable {return Err("서버 요청 관측을 중단한 세션입니다. 마지막 확인 내용을 보존했습니다.".into());}
    let mut event = json!({"method":method,"params":params});
    if let Some(id) = message.get("id") {
        let key = id.to_string();
        if events.answered.contains(&key) { return Ok(()); }
        if key.len()>256 {return Err("서버 요청 식별자가 허용 크기를 넘었습니다. 응답을 보내지 않았습니다.".into());}
        if !id.is_string() && !id.is_i64() && !id.is_u64() { return Err("서버 요청 식별자 형식이 올바르지 않습니다.".into()); }
        let count = events.pending.keys().chain(events.observed_pending.keys()).collect::<HashSet<_>>().len();
        if !events.pending.contains_key(&key) && !events.observed_pending.contains_key(&key) && count >= REQUEST_COUNT { return Err("처리 대기 서버 요청이 상한을 넘었습니다. 응답을 자동 승인하지 않았습니다.".into()); }
        if event.to_string().len() > 64 * 1024 { return Err("서버 승인 요청이 표시 상한을 넘었습니다. 응답을 보내지 않았습니다.".into()); }
        event["requestId"] = json!(key);
        event["guiOwned"] = json!(!metadata_only);
        if metadata_only {events.observed_pending.entry(key).or_insert(event);return Ok(());}
        if let Some(previous)=events.pending.get_mut(&key) {
            if previous["guiOwned"]==false {
                if previous["method"]!=event["method"] || previous["params"]!=event["params"] {return Err("인계한 요청과 재구독 원문이 달라 이전 요청을 보존했습니다.".into());}
                previous["guiOwned"]=json!(true);
            }
            return Ok(());
        }
        events.pending.insert(key,event.clone());
    } else {
        summarize(&mut event);
    }
    if !metadata_only {events.push(event);}
    Ok(())
}
fn validate_response(request: &Value, result: &Value) -> Result<(), String> {
    if result.to_string().len() > 64 * 1024 || !result.is_object() { return Err("서버 요청 응답 형식 또는 크기가 올바르지 않습니다.".into()); }
    match request["method"].as_str() {
        Some("item/commandExecution/requestApproval" | "item/fileChange/requestApproval") => {
            if result.as_object().is_some_and(|v| v.len() == 1) && result["decision"].as_str().is_some_and(|decision|ConversationConnection::approval_decisions(&request["params"]).contains(&decision)) { Ok(()) }
            else { Err("지원하지 않는 승인 결정입니다. 권한·정책 변경 응답은 보내지 않았습니다.".into()) }
        },
        Some("item/tool/requestUserInput") => {
            let answers = result["answers"].as_object().ok_or("질문 응답 목록이 올바르지 않습니다.")?;
            let questions = request["params"]["questions"].as_array().ok_or("질문 목록이 올바르지 않습니다.")?;
            if answers.len() != questions.len() || questions.iter().any(|question| {
                question["id"].as_str().and_then(|id| answers.get(id)).and_then(|value| value["answers"].as_array())
                    .is_none_or(|values| values.is_empty() || values.iter().any(|value| !value.is_string()))
            }) { return Err("모든 질문에 문자열 답변을 제공해야 합니다.".into()); }
            Ok(())
        },
        _ => Err("현재 화면에서 지원하지 않는 서버 요청입니다. 자동 응답하지 않았습니다.".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (ConversationConnection,Rpc<TcpStream>,TcpListener) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let stream = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        stream.set_read_timeout(Some(Duration::from_millis(100))).unwrap();
        let connection = ConversationConnection::new(Endpoint::Owned{address:String::new(),token:String::new(),job:Weak::new()},Arc::new(AtomicBool::new(false)));
        (connection,Rpc{ws:WebSocket::from_raw_socket(stream,tungstenite::protocol::Role::Client,None),next_id:0,cancelled:None,deadline:None},listener)
    }
    fn read_request(socket: &mut WebSocket<TcpStream>) -> Value {
        loop {if let Message::Text(text) = socket.read().unwrap() {return serde_json::from_str(&text).unwrap();}}
    }
    fn reply(socket: &mut WebSocket<TcpStream>,request: &Value,result: Value) {
        socket.send(Message::Text(json!({"id":request["id"],"result":result}).to_string().into())).unwrap();
    }
    fn assert_closed_without_input(socket: &mut WebSocket<TcpStream>) {
        let result=socket.get_mut().read(&mut[0u8;1]);
        let closed=match &result {Ok(0)=>true,Err(error)=>matches!(error.kind(),io::ErrorKind::ConnectionAborted|io::ErrorKind::ConnectionReset),_=>false};
        assert!(closed,"추가 송신 없이 실제 연결이 닫혀야 한다. 시간 초과는 종료 근거가 아니다: {result:?}");
    }
    #[test]
    fn context_snapshot_uses_existing_observer_without_draining_or_connecting() {
        let(control,mut rpc,listener)=fixture();
        control.0.events.lock().unwrap().observer_connected=true;
        let usage=json!({"total":{"totalTokens":100,"inputTokens":70,"cachedInputTokens":0,"outputTokens":30,"reasoningOutputTokens":0},"last":{"totalTokens":40,"inputTokens":30,"cachedInputTokens":0,"outputTokens":10,"reasoningOutputTokens":0},"modelContextWindow":1000});
        control.observer_message("original",json!({"method":"thread/tokenUsage/updated","params":{"threadId":"foreign","turnId":"turn","tokenUsage":usage}})).unwrap();
        assert!(control.context_status("original").unwrap()["tokenUsage"].is_null());
        control.observer_message("original",json!({"method":"thread/tokenUsage/updated","params":{"threadId":"original","turnId":"turn","tokenUsage":usage}})).unwrap();
        observe_message(&control.0,"original",json!({"method":"test/event","params":{"threadId":"original"}})).unwrap();
        for _ in 0..3 {
            let status=control.request("original",Path::new("."),ConversationRequest::ContextStatus).unwrap();
            assert_eq!(status["connected"],true);assert_eq!(status["guiConnected"],false);
            assert_eq!(status["tokenUsage"]["last"]["totalTokens"],40);assert_eq!(status["tokenUsageFresh"],true);
            assert_eq!(status["reason"],"unsupported");assert_eq!(status["active"],Value::Null);
        }
        assert!(control.0.worker.lock().unwrap().is_none());assert_eq!(control.0.events.lock().unwrap().queue.len(),1);
        {let _worker=control.0.worker.lock().unwrap();assert_eq!(control.context_status("original").unwrap()["reason"],"closing");}
        assert_eq!(rpc.next_id,0);
        let(stream,_)=listener.accept().unwrap();let mut socket=WebSocket::from_raw_socket(stream,tungstenite::protocol::Role::Server,None);
        drop(rpc);assert_closed_without_input(&mut socket);
    }
    #[test]
    fn context_usage_is_unavailable_after_malformed_update_and_stale_after_compaction() {
        let(control,_rpc,_listener)=fixture();control.0.events.lock().unwrap().observer_connected=true;
        let usage=json!({"total":{"totalTokens":100,"inputTokens":70,"cachedInputTokens":0,"outputTokens":30,"reasoningOutputTokens":0},"last":{"totalTokens":40,"inputTokens":30,"cachedInputTokens":0,"outputTokens":10,"reasoningOutputTokens":0},"modelContextWindow":1000});
        let update=|usage:Value|json!({"method":"thread/tokenUsage/updated","params":{"threadId":"original","turnId":"turn","tokenUsage":usage}});
        control.observer_message("original",update(usage.clone())).unwrap();
        control.observer_message("original",json!({"method":"item/started","params":{"threadId":"original","turnId":"compact","item":{"id":"compact-item","type":"contextCompaction"}}})).unwrap();
        control.observer_message("original",update(usage.clone())).unwrap();
        let status=control.context_status("original").unwrap();assert_eq!(status["compacting"],true);assert_eq!(status["tokenUsageFresh"],false);
        control.observer_message("original",json!({"method":"turn/completed","params":{"threadId":"original","turn":{"id":"other","status":"completed"}}})).unwrap();assert_eq!(control.context_status("original").unwrap()["compacting"],true);
        control.observer_message("original",json!({"method":"item/completed","params":{"threadId":"original","turnId":"compact","item":{"id":"compact-item","type":"contextCompaction"}}})).unwrap();
        assert_eq!(control.context_status("original").unwrap()["tokenUsageFresh"],false);
        control.observer_message("original",update(usage.clone())).unwrap();assert_eq!(control.context_status("original").unwrap()["tokenUsageFresh"],true);
        let mut invalid=usage;invalid["last"]["totalTokens"]=json!(-1);control.observer_message("original",update(invalid)).unwrap();
        let status=control.context_status("original").unwrap();assert!(status["tokenUsage"].is_null());assert_eq!(status["tokenUsageFresh"],false);
    }
    #[test]
    fn context_numeric_limits_reject_invalid_usage_without_inventing_zero() {
        let(control,_rpc,_listener)=fixture();control.0.events.lock().unwrap().observer_connected=true;
        let valid=json!({"total":{"totalTokens":400000,"inputTokens":300000,"cachedInputTokens":0,"outputTokens":100000,"reasoningOutputTokens":0},"last":{"totalTokens":56000,"inputTokens":40000,"cachedInputTokens":0,"outputTokens":16000,"reasoningOutputTokens":0},"modelContextWindow":100000});
        for (path,bad) in [
            ("/last/totalTokens",json!(-1)),("/last/totalTokens",json!(1.5)),("/last/totalTokens",json!(MAX_CONTEXT_INTEGER+1)),
            ("/total/totalTokens",json!(MAX_CONTEXT_INTEGER+1)),("/last/inputTokens",json!(-1)),
            ("/last/reasoningOutputTokens",json!(MAX_CONTEXT_INTEGER+1)),("/modelContextWindow",json!(0)),
            ("/modelContextWindow",json!(-1)),("/modelContextWindow",json!(MAX_CONTEXT_INTEGER+1)),
        ] {
            let mut usage=valid.clone();*usage.pointer_mut(path).unwrap()=bad.clone();
            control.observer_message("original",json!({"method":"thread/tokenUsage/updated","params":{"threadId":"original","turnId":"turn","tokenUsage":usage}})).unwrap();
            let snapshot=control.context_status("original").unwrap();assert!(snapshot["tokenUsage"].is_null(),"{path}={bad}");assert_eq!(snapshot["tokenUsageFresh"],false);
        }
        let mut missing=valid.clone();missing["last"].as_object_mut().unwrap().remove("totalTokens");
        control.observer_message("original",json!({"method":"thread/tokenUsage/updated","params":{"threadId":"original","turnId":"turn","tokenUsage":missing}})).unwrap();assert!(control.context_status("original").unwrap()["tokenUsage"].is_null());
        let mut unknown_window=valid.clone();unknown_window["modelContextWindow"]=Value::Null;
        control.observer_message("original",json!({"method":"thread/tokenUsage/updated","params":{"threadId":"original","turnId":"turn","tokenUsage":unknown_window}})).unwrap();
        let snapshot=control.context_status("original").unwrap();assert_eq!(snapshot["tokenUsage"]["last"]["totalTokens"],56000);assert_eq!(snapshot["tokenUsageFresh"],false);
        for used in [0,12000,100000,100001,MAX_CONTEXT_INTEGER] {
            let mut usage=valid.clone();usage["last"]["totalTokens"]=json!(used);
            control.observer_message("original",json!({"method":"thread/tokenUsage/updated","params":{"threadId":"original","turnId":"turn","tokenUsage":usage}})).unwrap();
            let snapshot=control.context_status("original").unwrap();assert_eq!(snapshot["tokenUsage"]["last"]["totalTokens"],used);assert_eq!(snapshot["tokenUsageFresh"],true);
        }
        println!("사용량 경계: 음수·소수·안전정수 초과·누락·분모 0 거부, 분모 null 미확인, 유효 0·큰수 보존");
    }
    #[test]
    fn context_observer_alone_owns_usage_across_gui_handoff_and_disconnect() {
        let(control,_rpc,_listener)=fixture();
        let usage=|used:i64|json!({"method":"thread/tokenUsage/updated","params":{"threadId":"original","turnId":"turn","tokenUsage":{"total":{"totalTokens":400000,"inputTokens":300000,"cachedInputTokens":0,"outputTokens":100000,"reasoningOutputTokens":0},"last":{"totalTokens":used,"inputTokens":0,"cachedInputTokens":0,"outputTokens":used,"reasoningOutputTokens":0},"modelContextWindow":100000}}});
        {let mut events=control.0.events.lock().unwrap();events.connected=true;events.observer_connected=true;}
        control.observer_message("original",usage(56000)).unwrap();
        observe_message(&control.0,"original",usage(99000)).unwrap();assert_eq!(control.context_status("original").unwrap()["tokenUsage"]["last"]["totalTokens"],56000);
        for method in ["thread/settings/updated","model/rerouted"] {
            // GUI의 복사본은 관찰자의 사용량 순서를 바꾸지 않는다.
            observe_message(&control.0,"original",json!({"method":method,"params":{"threadId":"original"}})).unwrap();assert_eq!(control.context_status("original").unwrap()["tokenUsageFresh"],true);
            control.observer_message("original",json!({"method":method,"params":{"threadId":"original"}})).unwrap();assert_eq!(control.context_status("original").unwrap()["tokenUsageFresh"],false);
            control.observer_message("original",usage(12000)).unwrap();assert_eq!(control.context_status("original").unwrap()["tokenUsageFresh"],true);
        }
        // GUI 해제 뒤에도 동일 관찰 스트림에서 감소한 새 사용량을 계속 받는다.
        control.0.events.lock().unwrap().connected=false;control.observer_message("original",usage(8000)).unwrap();
        let snapshot=control.context_status("original").unwrap();assert_eq!(snapshot["tokenUsage"]["last"]["totalTokens"],8000);assert_eq!(snapshot["tokenUsageFresh"],true);
        let revision=snapshot["revision"].as_u64().unwrap();control.observer_disconnected();
        let snapshot=control.context_status("original").unwrap();assert_eq!(snapshot["tokenUsageFresh"],false);assert!(snapshot["revision"].as_u64().unwrap()>revision);
        // 연결 여부만 다시 true로 해도 과거값이 fresh로 부활하지 않는다.
        control.0.events.lock().unwrap().observer_connected=true;assert_eq!(control.context_status("original").unwrap()["tokenUsageFresh"],false);
        println!("수신 순서: observer 56000 → GUI 99000 무시 → observer 모델 변경 미확인 → 12000 감소 수락 → GUI 인계 후 8000 갱신 → 단절/재연결 과거값 미확인");
    }
    #[test]
    fn context_passive_resubscribe_keeps_old_usage_stale_until_a_new_observer_update() {
        let(control,mut rpc,listener)=fixture();let cwd=env::current_dir().unwrap().canonicalize().unwrap();let expected_cwd=cwd.clone();
        control.0.events.lock().unwrap().observer_connected=true;
        let usage=|used:i64|json!({"method":"thread/tokenUsage/updated","params":{"threadId":"original","turnId":"turn","tokenUsage":{"total":{"totalTokens":used,"inputTokens":used,"cachedInputTokens":0,"outputTokens":0,"reasoningOutputTokens":0},"last":{"totalTokens":used,"inputTokens":used,"cachedInputTokens":0,"outputTokens":0,"reasoningOutputTokens":0},"modelContextWindow":100000}}});
        control.observer_message("original",usage(12000)).unwrap();control.observer_disconnected();
        let server=thread::spawn(move|| {
            let(stream,_)=listener.accept().unwrap();stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();let mut socket=WebSocket::from_raw_socket(stream,tungstenite::protocol::Role::Server,None);
            let request=read_request(&mut socket);assert_eq!(request["method"],PASSIVE_SUBSCRIBE);
            reply(&mut socket,&request,json!({"thread":{"id":"original","cwd":expected_cwd,"status":{"type":"idle"},"turns":[]}}));assert_closed_without_input(&mut socket);
        });
        assert!(control.observer_subscribe(&mut rpc,"original",&cwd).unwrap());
        let snapshot=control.context_status("original").unwrap();assert_eq!(snapshot["tokenUsage"]["last"]["totalTokens"],12000);assert_eq!(snapshot["tokenUsageFresh"],false);
        control.observer_message("original",usage(56000)).unwrap();assert_eq!(control.context_status("original").unwrap()["tokenUsageFresh"],true);
        assert_eq!(rpc.next_id,1);drop(rpc);server.join().unwrap();
    }
    #[test]
    fn context_ten_thousand_updates_keep_one_snapshot_and_existing_event_bounds() {
        let(control,_rpc,_listener)=fixture();control.0.events.lock().unwrap().observer_connected=true;
        for metadata_only in [true,false] {
            for index in 0..10000 {
                let message=json!({"method":"thread/tokenUsage/updated","params":{"threadId":"original","turnId":"turn","tokenUsage":{"total":{"totalTokens":index,"inputTokens":index,"cachedInputTokens":0,"outputTokens":0,"reasoningOutputTokens":0},"last":{"totalTokens":index,"inputTokens":index,"cachedInputTokens":0,"outputTokens":0,"reasoningOutputTokens":0},"modelContextWindow":100000}}});
                record_message(&control.0,"original",message,metadata_only).unwrap();
            }
            let events=control.0.events.lock().unwrap();assert!(events.token_usage.is_some());assert_eq!(events.token_usage.as_ref().unwrap().last.total_tokens,9999);
            assert_eq!(events.queue.len(),if metadata_only {0}else{EVENT_COUNT});assert!(events.bytes<=EVENT_BYTES);assert_eq!(events.gap,!metadata_only);
            // gap은 표시 이력 큐의 퇴거다. 환원기는 매 프레임을 처리했으므로 최신 snapshot 자체는 유효하다.
            assert!(events.token_usage_fresh);
            println!("연속 사용량 10000회: 관찰자={metadata_only}, 스냅샷=1, 큐={}, 바이트={}, 표시이력누락={}, 최신값=9999",events.queue.len(),events.bytes,events.gap);
            drop(events);assert!(control.0.worker.lock().unwrap().is_none());
        }
    }
    #[test]
    fn context_unsupported_compact_and_native_guards_never_send_legacy_compact() {
        for state in ["idle","active","statusUnknown","pendingRequest","unknownResult","compacting","deleted","closing"] {
            let(control,mut rpc,listener)=fixture();
            {let mut events=control.0.events.lock().unwrap();events.connected=true;events.context_active=Some(false);events.gui_compact_supported=state!="idle";
                match state {"active"=>events.context_active=Some(true),"statusUnknown"=>events.context_active=None,
                    "pendingRequest"=>{events.observed_pending.insert("1".into(),json!({"method":"item/tool/requestUserInput"}));},
                    "compacting"=>events.context_compaction=Some(("turn".into(),"item".into())),_=>{}}
            }
            control.0.unknown.store(state=="unknownResult",Ordering::Release);control.0.deleted.store(state=="deleted",Ordering::Release);control.0.stopped.store(state=="closing",Ordering::Release);
            let result=execute(&control.0,&mut rpc,"original",ConversationRequest::Compact,&AtomicBool::new(false));
            if matches!(state,"deleted"|"closing") {assert!(result.is_err());}
            else {let value=result.unwrap();assert_eq!(value["status"],if state=="idle" {"unsupported"}else{"notStarted"});assert_eq!(value["reason"],if state=="idle" {"unsupported"}else{state});}
            assert_eq!(rpc.next_id,0);let(stream,_)=listener.accept().unwrap();let mut socket=WebSocket::from_raw_socket(stream,tungstenite::protocol::Role::Server,None);drop(rpc);assert_closed_without_input(&mut socket);
        }
    }
    fn compact_bootstrap(socket: &mut WebSocket<TcpStream>,cwd: &Path,capability: Option<Value>) {
        conversation_bootstrap(socket,cwd,capability,None,None);
    }
    fn conversation_bootstrap(socket: &mut WebSocket<TcpStream>,cwd: &Path,compact: Option<Value>,initialize_interrupt: Option<Value>,passive_interrupt: Option<Value>) {
        let init=read_request(socket);assert_eq!(init["method"],"initialize");let mut initialized=json!({});
        let malformed=initialize_interrupt.as_ref().is_some_and(|value|!value.is_boolean());
        if let Some(capability)=initialize_interrupt {initialized["supportsExpectedTurnInterrupt"]=capability;}reply(socket,&init,initialized);if malformed {return;}
        assert_eq!(read_request(socket)["method"],"initialized");
        let subscribe=read_request(socket);assert_eq!(subscribe["method"],PASSIVE_SUBSCRIBE);assert_eq!(subscribe["params"],json!({"threadId":"original"}));
        let mut value=json!({"thread":{"id":"original","cwd":cwd,"turns":[],"status":{"type":"idle"}}});
        let malformed=compact.as_ref().into_iter().chain(passive_interrupt.as_ref()).any(|value|!value.is_boolean());
        if let Some(capability)=compact {value["supportsCompactIfIdle"]=capability;}
        if let Some(capability)=passive_interrupt {value["supportsExpectedTurnInterrupt"]=capability;}reply(socket,&subscribe,value);
        if malformed {return;}
        let history=read_request(socket);assert_eq!(history["method"],"thread/turns/list");reply(socket,&history,json!({"data":[]}));
    }
    fn compact_gui(control: &ConversationConnection,mut rpc: Rpc<TcpStream>,cwd: PathBuf) {
        control.0.events.lock().unwrap().observer_connected=true;
        control.observer_message("original",json!({"method":"thread/status/changed","params":{"threadId":"original","status":{"type":"idle"}}})).unwrap();
        control.observer_message("original",json!({"method":"thread/tokenUsage/updated","params":{"threadId":"original","turnId":"previous","tokenUsage":{"total":{"totalTokens":100,"inputTokens":70,"cachedInputTokens":0,"outputTokens":30,"reasoningOutputTokens":0},"last":{"totalTokens":40,"inputTokens":30,"cachedInputTokens":0,"outputTokens":10,"reasoningOutputTokens":0},"modelContextWindow":1000}}})).unwrap();
        install_actor(control,move|state,receiver,cancelled|{let _=serve(&state,&mut rpc,"original",&cwd,receiver,&cancelled,&Mutex::new(None));});
        let(reply,attached)=mpsc::sync_channel(1);control.0.worker.lock().unwrap().as_ref().unwrap().sender.try_send(Work{request:ConversationRequest::Attach,reply,deadline:Instant::now()+RPC_TIMEOUT,send_context:None}).ok().unwrap();attached.recv_timeout(Duration::from_secs(3)).unwrap().unwrap();
    }
    #[test]
    fn compact_requires_verified_gui_capability_and_resets_it_on_disconnect() {
        let control=fixture().0;
        for capability in [None,Some(json!(false)),Some(json!(true)),Some(json!(false))] {
            let(_,rpc,listener)=fixture();let cwd=env::current_dir().unwrap().canonicalize().unwrap();let expected_cwd=cwd.clone();let supported=capability==Some(json!(true));
            control.observer_disconnected();
            let server=thread::spawn(move||{
                let(stream,_)=listener.accept().unwrap();stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();let mut socket=WebSocket::from_raw_socket(stream,tungstenite::protocol::Role::Server,None);compact_bootstrap(&mut socket,&expected_cwd,capability);
                if supported {let request=read_request(&mut socket);assert_eq!(request["method"],COMPACT_IF_IDLE);assert_eq!(request["params"],json!({"threadId":"original"}));reply(&mut socket,&request,json!({"status":"started","turn":{"id":"compact-turn"}}));}assert_closed_without_input(&mut socket);
            });
            compact_gui(&control,rpc,cwd);assert_eq!(control.context_status("original").unwrap()["canCompact"],supported);
            let value=control.request("original",Path::new("."),ConversationRequest::Compact).unwrap();assert_eq!(value["status"],if supported{"started"}else{"unsupported"});
            if supported {let status=control.context_status("original").unwrap();assert_eq!(status["compacting"],true);assert_eq!(status["active"],true);assert_eq!(status["canCompact"],false);assert_eq!(control.request("original",Path::new("."),ConversationRequest::Compact).unwrap()["status"],"notStarted");}
            control.request("original",Path::new("."),ConversationRequest::Detach).unwrap();assert!(!control.0.events.lock().unwrap().gui_compact_supported);server.join().unwrap();
        }
        let(control,mut rpc,listener)=fixture();let cwd=env::current_dir().unwrap().canonicalize().unwrap();let expected_cwd=cwd.clone();
        let server=thread::spawn(move||{
            let(stream,_)=listener.accept().unwrap();stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();let mut socket=WebSocket::from_raw_socket(stream,tungstenite::protocol::Role::Server,None);
            for capability in [json!(true),json!(false)] {let request=read_request(&mut socket);assert_eq!(request["method"],PASSIVE_SUBSCRIBE);reply(&mut socket,&request,json!({"thread":{"id":"original","cwd":expected_cwd,"turns":[],"status":{"type":"idle"}},"supportsCompactIfIdle":capability}));}
            let request=read_request(&mut socket);assert_eq!(request["method"],PASSIVE_SUBSCRIBE);socket.send(Message::Text(json!({"id":request["id"],"error":{"code":-32601,"message":"unsupported"}}).to_string().into())).unwrap();assert_closed_without_input(&mut socket);
        });
        assert!(control.observer_subscribe(&mut rpc,"original",&cwd).unwrap());assert!(control.0.events.lock().unwrap().observer_compact_supported);
        // 관찰자의 지원 정보는 화면 연결의 지원 증거로 승격하지 않는다.
        control.0.events.lock().unwrap().connected=true;assert_eq!(execute(&control.0,&mut rpc,"original",ConversationRequest::Compact,&AtomicBool::new(false)).unwrap()["status"],"unsupported");
        control.observer_disconnected();assert!(!control.0.events.lock().unwrap().observer_compact_supported);
        assert!(control.observer_subscribe(&mut rpc,"original",&cwd).unwrap());assert!(!control.0.events.lock().unwrap().observer_compact_supported);
        control.0.events.lock().unwrap().observer_compact_supported=true;assert!(control.observer_subscribe(&mut rpc,"original",&cwd).is_err());assert!(!control.0.events.lock().unwrap().observer_compact_supported);drop(rpc);server.join().unwrap();
    }
    #[test]
    fn compact_rejects_malformed_support_bits_before_history_or_compact() {
        for capability in [Value::Null,json!("true"),json!(1),json!({}),json!([])] {
            for observer in [false,true] {
                let(control,mut rpc,listener)=fixture();let cwd=env::current_dir().unwrap().canonicalize().unwrap();let expected_cwd=cwd.clone();let advertised=capability.clone();
                {let mut events=control.0.events.lock().unwrap();events.gui_compact_supported=true;events.observer_compact_supported=true;}
                let server=thread::spawn(move||{
                    let(stream,_)=listener.accept().unwrap();stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();let mut socket=WebSocket::from_raw_socket(stream,tungstenite::protocol::Role::Server,None);
                    if observer {let subscribe=read_request(&mut socket);assert_eq!(subscribe["method"],PASSIVE_SUBSCRIBE);reply(&mut socket,&subscribe,json!({"thread":{"id":"original","cwd":expected_cwd,"turns":[],"status":{"type":"idle"}},"supportsCompactIfIdle":advertised}));}
                    else {compact_bootstrap(&mut socket,&expected_cwd,Some(advertised));}
                    assert_closed_without_input(&mut socket);
                });
                if observer {assert!(control.observer_subscribe(&mut rpc,"original",&cwd).unwrap_err().contains("bool"));assert!(!control.0.events.lock().unwrap().observer_compact_supported);}
                else {let(_sender,receiver)=mpsc::sync_channel(1);assert!(serve(&control.0,&mut rpc,"original",&cwd,receiver,&AtomicBool::new(false),&Mutex::new(None)).unwrap_err().contains("bool"));assert!(!control.0.events.lock().unwrap().gui_compact_supported);}
                assert!(!control.0.events.lock().unwrap().connected);assert_eq!(rpc.next_id,if observer{1}else{2});drop(rpc);server.join().unwrap();
            }
        }
    }
    #[test]
    fn compact_wire_rejections_preserve_state_and_uncertain_results_block_resend() {
        for outcome in ["skippedBusy","notStarted","unsupported","invalidParams","internal","invalidRequest","lost","badStarted","badNotStarted","forgedUnsupported","hybridError"] {
            let(control,rpc,listener)=fixture();let cwd=env::current_dir().unwrap().canonicalize().unwrap();let expected_cwd=cwd.clone();let state=control.clone();
            let known=matches!(outcome,"skippedBusy"|"notStarted"|"unsupported"|"invalidParams");
            let server=thread::spawn(move||{
                let(stream,_)=listener.accept().unwrap();stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();let mut socket=WebSocket::from_raw_socket(stream,tungstenite::protocol::Role::Server,None);compact_bootstrap(&mut socket,&expected_cwd,Some(json!(true)));
                let request=read_request(&mut socket);assert_eq!(request["method"],COMPACT_IF_IDLE);assert_eq!(request["params"],json!({"threadId":"original"}));
                assert_eq!(state.context_status("original").unwrap()["reason"],"unknownResult","송신 관측 때 불명 보호가 이미 설정되고 worker/events 잠금은 해제돼야 한다");
                if outcome=="lost" {return;}
                let mut value=match outcome {
                    "unsupported"|"invalidParams"|"internal"|"invalidRequest"=>json!({"error":{"code":match outcome{"unsupported"=>-32601,"invalidParams"=>-32602,"invalidRequest"=>-32600,_=>-32603},"message":"서버 판정"}}),
                    "badStarted"=>json!({"result":{"status":"started","turn":{"id":""}}}),
                    "badNotStarted"=>json!({"result":{"status":"notStarted"}}),
                    "forgedUnsupported"=>json!({"result":{"status":"unsupported"}}),
                    "hybridError"=>json!({"error":{"code":-32603,"message":"내부 실패"},"result":{"status":"started","turn":{"id":"compact-turn"}}}),
                    _=>json!({"result":{"status":outcome,"reason":"대기 입력"}}),
                };value["id"]=request["id"].clone();socket.send(Message::Text(value.to_string().into())).unwrap();assert_closed_without_input(&mut socket);
            });
            compact_gui(&control,rpc,cwd);let before=control.context_status("original").unwrap();
            let result=control.request("original",Path::new("."),ConversationRequest::Compact);assert_eq!(result.is_ok(),known,"{outcome}");assert_eq!(control.0.unknown.load(Ordering::Acquire),!known,"{outcome}");
            let status=control.context_status("original").unwrap();assert_eq!(status["tokenUsage"],before["tokenUsage"]);assert_eq!(status["compacting"],false);
            if known {let value=result.unwrap();assert_eq!(value["status"],match outcome{"unsupported"=>"unsupported","invalidParams"=>"notStarted",other=>other});assert_eq!(status["active"],before["active"]);assert_eq!(status["tokenUsageFresh"],before["tokenUsageFresh"]);}
            else {assert!(control.terminal_write(||Ok(1)).is_err());let next=control.request("original",Path::new("."),ConversationRequest::Compact);if let Ok(value)=next {assert_eq!(value["reason"],"unknownResult");}}
            if outcome=="unsupported" {assert_eq!(control.request("original",Path::new("."),ConversationRequest::Compact).unwrap()["status"],"unsupported");}
            control.cancel();server.join().unwrap();
        }
    }
    #[test]
    fn compact_reply_never_resurrects_early_completed_or_newer_observed_turns() {
        for sequence in ["start","itemComplete","complete","nextTurn","observerComplete","observerNext","observerUsage","foreign"] {
            let(control,rpc,listener)=fixture();let cwd=env::current_dir().unwrap().canonicalize().unwrap();let expected_cwd=cwd.clone();let observed=control.clone();
            let server=thread::spawn(move||{
                let(stream,_)=listener.accept().unwrap();stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();let mut socket=WebSocket::from_raw_socket(stream,tungstenite::protocol::Role::Server,None);compact_bootstrap(&mut socket,&expected_cwd,Some(json!(true)));
                let request=read_request(&mut socket);assert_eq!(request["method"],COMPACT_IF_IDLE);
                let thread_id=if sequence=="foreign"{"other"}else{"original"};
                let mut messages=vec![json!({"method":"turn/started","params":{"threadId":thread_id,"turn":{"id":"compact-turn"}}}),json!({"method":"item/started","params":{"threadId":thread_id,"turnId":"compact-turn","item":{"id":"compact-item","type":"contextCompaction"}}})];
                if sequence!="start" {messages.push(json!({"method":"item/completed","params":{"threadId":thread_id,"turnId":"compact-turn","item":{"id":"compact-item","type":"contextCompaction"}}}));}
                if sequence!="start"&&sequence!="itemComplete" {messages.push(json!({"method":"turn/completed","params":{"threadId":thread_id,"turn":{"id":"compact-turn","status":"completed"}}}));}
                if matches!(sequence,"nextTurn"|"observerNext") {messages.push(json!({"method":"turn/started","params":{"threadId":thread_id,"turn":{"id":"newer-turn"}}}));messages.push(json!({"method":"turn/completed","params":{"threadId":thread_id,"turn":{"id":"compact-turn"}}}));}
                for message in messages {if sequence.starts_with("observer") {observed.observer_message("original",message).unwrap();}else{socket.send(Message::Text(message.to_string().into())).unwrap();}}
                if sequence=="observerUsage" {observed.observer_message("original",json!({"method":"thread/tokenUsage/updated","params":{"threadId":"original","turnId":"compact-turn","tokenUsage":{"total":{"totalTokens":100,"inputTokens":100,"cachedInputTokens":0,"outputTokens":0,"reasoningOutputTokens":0},"last":{"totalTokens":10,"inputTokens":10,"cachedInputTokens":0,"outputTokens":0,"reasoningOutputTokens":0},"modelContextWindow":1000}}})).unwrap();}
                reply(&mut socket,&request,json!({"status":"started","turn":{"id":"compact-turn"}}));assert_closed_without_input(&mut socket);
            });
            compact_gui(&control,rpc,cwd);let value=control.request("original",Path::new("."),ConversationRequest::Compact).unwrap();assert_eq!(value["status"],"started");assert_eq!(value["turn"]["id"],"compact-turn");
            let status=control.context_status("original").unwrap();assert_eq!(status["compacting"],sequence=="foreign","{sequence}");
            if !sequence.starts_with("observer")&&sequence!="foreign" {
                assert!(status["active"].is_null(),"화면의 선행 활동을 관찰자 상태로 승격하면 안 된다: {sequence}");assert_eq!(status["reason"],"statusUnknown");
                control.observer_message("original",json!({"method":"turn/started","params":{"threadId":"original","turn":{"id":"compact-turn"}}})).unwrap();assert_eq!(control.context_status("original").unwrap()["active"],true);
                control.observer_message("original",json!({"method":"turn/completed","params":{"threadId":"original","turn":{"id":"compact-turn"}}})).unwrap();assert_eq!(control.context_status("original").unwrap()["active"],false);
            } else {assert_eq!(status["active"],!matches!(sequence,"observerComplete"|"observerUsage"),"{sequence}");}
            if sequence=="observerNext" {assert_eq!(control.0.events.lock().unwrap().context_turn.as_deref(),Some("newer-turn"));}
            if sequence=="observerUsage" {assert_eq!(status["tokenUsageFresh"],true);assert_eq!(status["tokenUsage"]["last"]["totalTokens"],10);}
            assert!(!control.0.unknown.load(Ordering::Acquire));control.cancel();server.join().unwrap();
        }
    }
    #[test]
    fn interrupt_requires_both_gui_capabilities_independently_of_compact_and_resets_on_reconnect() {
        let control=fixture().0;
        for compact in [false,true] {
            for (initialize_interrupt,passive_interrupt) in [(Some(true),Some(true)),(None,None),(Some(false),Some(false)),(Some(false),Some(true)),(Some(true),Some(false)),(Some(true),None),(None,Some(true))] {
                let(_,rpc,listener)=fixture();let cwd=env::current_dir().unwrap().canonicalize().unwrap();let expected_cwd=cwd.clone();let supported=initialize_interrupt==Some(true)&&passive_interrupt==Some(true);
                let server=thread::spawn(move||{
                    let(stream,_)=listener.accept().unwrap();stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();let mut socket=WebSocket::from_raw_socket(stream,tungstenite::protocol::Role::Server,None);
                    conversation_bootstrap(&mut socket,&expected_cwd,Some(json!(compact)),initialize_interrupt.map(Value::Bool),passive_interrupt.map(Value::Bool));
                    let page=read_request(&mut socket);assert_eq!(page["method"],"thread/turns/list");reply(&mut socket,&page,json!({"data":[{"id":"preserved-turn","status":"inProgress"}],"nextCursor":"older"}));
                    if supported {let request=read_request(&mut socket);assert_eq!(request["method"],"turn/interrupt");assert_eq!(request["params"],json!({"threadId":"original","turnId":"requested-turn"}));reply(&mut socket,&request,json!({}));}
                    assert_closed_without_input(&mut socket);
                });
                compact_gui(&control,rpc,cwd);assert_eq!(control.context_status("original").unwrap()["canCompact"],compact);
                let before=control.context_status("original").unwrap();let page=control.request("original",Path::new("."),ConversationRequest::Page{cursor:None}).unwrap();assert_eq!(page["data"][0]["id"],"preserved-turn");assert_eq!(page["nextCursor"],"older");
                let result=control.request("original",Path::new("."),ConversationRequest::Interrupt{turn_id:"requested-turn".into()});assert_eq!(result.is_ok(),supported);
                if let Err(error)=result {assert!(error.contains("요청을 보내지 않았습니다"));assert!(!error.contains("supportsExpectedTurnInterrupt"));}
                assert!(!control.0.unknown.load(Ordering::Acquire));let after=control.context_status("original").unwrap();assert_eq!(after["active"],before["active"]);assert_eq!(after["tokenUsage"],before["tokenUsage"]);
                assert_eq!(control.0.events.lock().unwrap().gui_interrupt_supported,supported);
                assert_eq!(control.0.events.lock().unwrap().interrupt_diagnostic.is_some(),(initialize_interrupt==Some(true)) ^ (passive_interrupt==Some(true)));
                control.request("original",Path::new("."),ConversationRequest::Detach).unwrap();assert!(!control.0.events.lock().unwrap().gui_interrupt_supported);assert!(control.0.events.lock().unwrap().interrupt_diagnostic.is_none());server.join().unwrap();
            }
        }
    }
    #[test]
    fn interrupt_malformed_capabilities_fail_before_further_queries_and_observer_never_grants_gui_support() {
        for capability in [Value::Null,json!("true"),json!(1),json!({}),json!([])] {
            for phase in ["initialize","passive","observer"] {
                let(control,mut rpc,listener)=fixture();let cwd=env::current_dir().unwrap().canonicalize().unwrap();let expected_cwd=cwd.clone();let advertised=capability.clone();
                control.0.events.lock().unwrap().gui_interrupt_supported=phase!="observer";
                let server=thread::spawn(move||{
                    let(stream,_)=listener.accept().unwrap();stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();let mut socket=WebSocket::from_raw_socket(stream,tungstenite::protocol::Role::Server,None);
                    if phase=="observer" {let subscribe=read_request(&mut socket);assert_eq!(subscribe["method"],PASSIVE_SUBSCRIBE);reply(&mut socket,&subscribe,json!({"thread":{"id":"original","cwd":expected_cwd,"turns":[],"status":{"type":"idle"}},"supportsExpectedTurnInterrupt":advertised}));}
                    else {conversation_bootstrap(&mut socket,&expected_cwd,Some(json!(true)),Some(if phase=="initialize"{advertised.clone()}else{json!(true)}),Some(if phase=="passive"{advertised}else{json!(true)}));}
                    assert_closed_without_input(&mut socket);
                });
                let error=if phase=="observer" {control.observer_subscribe(&mut rpc,"original",&cwd).unwrap_err()}
                    else {let(_sender,receiver)=mpsc::sync_channel(1);serve(&control.0,&mut rpc,"original",&cwd,receiver,&AtomicBool::new(false),&Mutex::new(None)).unwrap_err()};
                assert!(error.contains("지원 정보 형식"));assert!(!error.contains("supportsExpectedTurnInterrupt"));assert!(!control.0.events.lock().unwrap().gui_interrupt_supported);assert_eq!(rpc.next_id,if phase=="passive"{2}else{1});drop(rpc);server.join().unwrap();
            }
        }
        let(control,mut rpc,listener)=fixture();let cwd=env::current_dir().unwrap().canonicalize().unwrap();let expected_cwd=cwd.clone();
        let server=thread::spawn(move||{
            let(stream,_)=listener.accept().unwrap();stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();let mut socket=WebSocket::from_raw_socket(stream,tungstenite::protocol::Role::Server,None);
            let subscribe=read_request(&mut socket);assert_eq!(subscribe["method"],PASSIVE_SUBSCRIBE);reply(&mut socket,&subscribe,json!({"thread":{"id":"original","cwd":expected_cwd,"turns":[],"status":{"type":"idle"}},"supportsExpectedTurnInterrupt":true,"supportsCompactIfIdle":true}));assert_closed_without_input(&mut socket);
        });
        assert!(control.observer_subscribe(&mut rpc,"original",&cwd).unwrap());
        {let mut events=control.0.events.lock().unwrap();assert!(!events.gui_interrupt_supported);events.connected=true;}
        assert!(execute(&control.0,&mut rpc,"original",ConversationRequest::Interrupt{turn_id:"requested-turn".into()},&AtomicBool::new(false)).unwrap_err().contains("요청을 보내지 않았습니다"));assert!(!control.0.unknown.load(Ordering::Acquire));assert_eq!(rpc.next_id,1);drop(rpc);server.join().unwrap();
    }
    #[test]
    fn bounded_events_mark_gaps_and_release_drained_bytes() {
        let mut events = Events::default();
        for index in 0..EVENT_COUNT * 3 { events.push(json!({"method":"delta","params":{"delta":index.to_string()}})); }
        assert_eq!(events.queue.len(), EVENT_COUNT); assert!(events.gap); assert!(events.bytes <= EVENT_BYTES);
        let page = events.poll(); assert_eq!(page["events"].as_array().unwrap().len(), EVENT_COUNT); assert_eq!(page["gap"], true);
        assert_eq!(events.bytes, 0); assert!(events.queue.is_empty()); assert_eq!(events.poll()["gap"], false);
    }
    #[test]
    fn detail_chunks_round_trip_unicode_without_losing_content() {
        let text = "가🙂나".repeat(10000); let mut offset = 0; let mut collected = String::new();
        loop { let page = detail_chunk(&text,offset).unwrap(); collected.push_str(page["text"].as_str().unwrap());
            match page["nextOffset"].as_u64() {Some(next) => {assert!(next > offset as u64); offset = next as usize;},None => break}
        }
        assert_eq!(collected,text); assert!(detail_chunk("가",1).is_err());
    }
    #[test]
    fn large_fields_are_explicit_detail_references_and_unknown_attachments_fail() {
        let mut value = json!({"id":"same-item","text":"x".repeat(TEXT_PREVIEW+1)}); summarize(&mut value);
        assert_eq!(value["id"],"same-item"); assert_eq!(value["text"]["__detail"],true); assert_eq!(value["text"]["bytes"],TEXT_PREVIEW+1);
        assert!(user_input("",&[Attachment {r#type:"audio".into(),path:"/fake".into()}]).is_err());
        assert!(user_input("",&[]).is_err()); assert!(user_input(&"a".repeat(65537),&[]).is_err());
    }
    #[test]
    fn requests_belong_to_the_original_thread_and_resolve_only_once() {
        let control = ConversationConnection::new(Endpoint::Owned{address:String::new(),token:String::new(),job:Weak::new()},Arc::new(AtomicBool::new(false)));
        observe_message(&control.0,"original",json!({"id":7,"method":"item/commandExecution/requestApproval","params":{"threadId":"other"}})).unwrap();
        assert!(control.0.events.lock().unwrap().pending.is_empty());
        observe_message(&control.0,"original",json!({"id":7,"method":"item/commandExecution/requestApproval","params":{"threadId":"original"}})).unwrap();
        assert_eq!(control.0.events.lock().unwrap().pending.len(),1);
        observe_message(&control.0,"original",json!({"method":"serverRequest/resolved","params":{"threadId":"original","requestId":7}})).unwrap();
        assert!(control.0.events.lock().unwrap().pending.is_empty());
    }
    #[test]
    fn terminal_input_is_exclusive_and_persistent_policy_responses_fail() {
        let control = ConversationConnection::new(Endpoint::Owned{address:String::new(),token:String::new(),job:Weak::new()},Arc::new(AtomicBool::new(false)));
        assert_eq!(control.terminal_write(|| Ok(42)).unwrap(),42);
        *control.0.input.lock().unwrap() = true; assert!(control.terminal_write(|| Ok(42)).is_err());
        let request = json!({"method":"item/commandExecution/requestApproval"});
        assert!(validate_response(&request,&json!({"decision":"accept"})).is_ok());
        assert!(validate_response(&request,&json!({"decision":{"acceptWithExecpolicyAmendment":{}}})).is_err());
    }
    #[test]
    fn approval_choices_intersect_advertised_decisions_and_reject_unknown_or_empty_lists() {
        let params=json!({"availableDecisions":["decline","acceptForSession",{"acceptWithExecpolicyAmendment":{}},"futureDecision"]});
        assert_eq!(ConversationConnection::approval_decisions(&params),["acceptForSession","decline"]);
        let request=json!({"method":"item/commandExecution/requestApproval","params":params});
        assert!(validate_response(&request,&json!({"decision":"accept"})).is_err());
        assert!(validate_response(&request,&json!({"decision":"decline"})).is_ok());
        for advertised in [json!([]),Value::Null,json!(["futureDecision"]),json!({"accept":true})] {
            let params=json!({"availableDecisions":advertised});assert!(ConversationConnection::approval_decisions(&params).is_empty());
            assert!(validate_response(&json!({"method":"item/commandExecution/requestApproval","params":params}),&json!({"decision":"accept"})).is_err());
        }
        assert_eq!(ConversationConnection::approval_decisions(&json!({})),["accept","decline","cancel"]);
    }
    #[test]
    fn unsupported_passive_observer_and_gui_never_call_resume_or_create_a_thread() {
        for gui in [false,true] {
            let (control,mut rpc,listener)=fixture();
            let server=thread::spawn(move||{
                let(stream,_)=listener.accept().unwrap();stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
                let mut socket=WebSocket::from_raw_socket(stream,tungstenite::protocol::Role::Server,None);let mut methods=Vec::new();
                if gui {
                    let request=read_request(&mut socket);assert_eq!(request["method"],"initialize");methods.push("initialize");reply(&mut socket,&request,json!({}));
                    let request=read_request(&mut socket);assert_eq!(request["method"],"initialized");methods.push("initialized");
                }
                let request=read_request(&mut socket);assert_eq!(request["method"],PASSIVE_SUBSCRIBE);assert_eq!(request["params"],json!({"threadId":"original"}));methods.push(PASSIVE_SUBSCRIBE);
                socket.send(Message::Text(json!({"id":request["id"],"error":{"code":-32601,"message":"unknown method"}}).to_string().into())).unwrap();
                assert!(socket.read().is_err(),"미지원 응답 뒤 resume/start/read 대체 요청이 한 건이라도 나가면 안 된다");methods
            });
            let error=if gui {let(_sender,receiver)=mpsc::sync_channel(1);serve(&control.0,&mut rpc,"original",Path::new("."),receiver,&AtomicBool::new(false),&Mutex::new(None)).unwrap_err()}
                else {control.observer_subscribe(&mut rpc,"original",Path::new(".")).unwrap_err()};
            assert!(error.contains("호환되는 서버"));assert!(!control.0.unknown.load(Ordering::Acquire));drop(rpc);
            let expected=if gui {vec!["initialize","initialized",PASSIVE_SUBSCRIBE]} else {vec![PASSIVE_SUBSCRIBE]};assert_eq!(server.join().unwrap(),expected);
        }
    }
    #[test]
    fn atomic_user_admission_has_no_pre_read_and_preserves_model_effort_and_client_id() {
        for status in ["started","skippedBusy","notStarted","unsupported","invalidParams","invalidRequest","internal","invalid"] {
            let (control,mut rpc,listener)=fixture();let client_id=Uuid::new_v4().to_string();let expected_id=client_id.clone();
            let server=thread::spawn(move||{
                let(stream,_)=listener.accept().unwrap();stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();let mut socket=WebSocket::from_raw_socket(stream,tungstenite::protocol::Role::Server,None);
                let request=read_request(&mut socket);assert_eq!(request["method"],START_USER_IF_IDLE);assert_eq!(request["params"],json!({"threadId":"original","input":[{"type":"text","text":"원래 사용자 입력","text_elements":[]}],"clientUserMessageId":expected_id,"model":"server-model","effort":"high"}));
                if matches!(status,"unsupported" | "invalidParams" | "invalidRequest" | "internal") {
                    let code=match status {"unsupported"=>-32601,"invalidParams"=>-32602,"invalidRequest"=>-32600,_=>-32603};
                    socket.send(Message::Text(json!({"id":request["id"],"error":{"code":code,"message":"server rejection"}}).to_string().into())).unwrap();
                }
                else {reply(&mut socket,&request,json!({"status":status,"turn":{"id":"same-turn"},"reason":"busy mailbox"}));}
                assert!(socket.read().is_err(),"원자적 접수 뒤 일반 turn/start 등 대체·재전송이 나가면 안 된다");
            });
            let result=execute(&control.0,&mut rpc,"original",ConversationRequest::Send{request_id:client_id,text:"원래 사용자 입력".into(),attachments:vec![],model:Some("server-model".into()),effort:Some("high".into())},&AtomicBool::new(false));
            let unknown=matches!(status,"invalid" | "internal" | "invalidRequest");
            assert_eq!(result.is_ok(),status=="started");assert_eq!(control.0.unknown.load(Ordering::Acquire),unknown);
            if unknown {assert!(execute(&control.0,&mut rpc,"original",ConversationRequest::Send{request_id:Uuid::new_v4().to_string(),text:"다음 입력".into(),attachments:vec![],model:None,effort:None},&AtomicBool::new(false)).unwrap_err().contains("이전 전송 결과"));}
            if status=="unsupported" {assert!(result.unwrap_err().contains("호환되는 서버"));}
            drop(rpc);server.join().unwrap();
        }
    }
    #[test]
    fn unavailable_history_is_not_an_empty_page_or_detail() {
        for request in [ConversationRequest::Page{cursor:Some("older".into())},ConversationRequest::Items{turn_id:"turn".into(),cursor:None},ConversationRequest::Detail{turn_id:"turn".into(),item_id:"item".into(),offset:Some(0),snapshot_id:None}] {
            let (control,mut rpc,listener)=fixture();let server=thread::spawn(move||{
                let(stream,_)=listener.accept().unwrap();let mut socket=WebSocket::from_raw_socket(stream,tungstenite::protocol::Role::Server,None);let request=read_request(&mut socket);
                socket.send(Message::Text(json!({"id":request["id"],"error":{"code":-32600,"message":"not materialized","data":{"kind":"historyProjectionUnavailable","reason":"unmaterialized"}}}).to_string().into())).unwrap();
            });
            assert_eq!(execute(&control.0,&mut rpc,"original",request,&AtomicBool::new(false)).unwrap_err(),HISTORY_UNAVAILABLE);assert!(control.0.detail.lock().unwrap().is_none());drop(rpc);server.join().unwrap();
        }
    }
    #[test]
    fn first_page_waits_only_for_exact_unmaterialized_error_without_inventing_history() {
        for(case,page_cursor,payload,waiting)in [
            ("exact",None,json!({"error":{"code":-32600,"data":{"kind":"historyProjectionUnavailable","reason":"unmaterialized"}}}),true),
            ("older",Some("older"),json!({"error":{"code":-32600,"data":{"kind":"historyProjectionUnavailable","reason":"unmaterialized"}}}),false),
            ("emptyCursor",Some(""),json!({"error":{"code":-32600,"data":{"kind":"historyProjectionUnavailable","reason":"unmaterialized"}}}),false),
            ("internal",None,json!({"error":{"code":-32603,"data":{"kind":"historyProjectionUnavailable","reason":"unmaterialized"}}}),false),
            ("stringCode",None,json!({"error":{"code":"-32600","data":{"kind":"historyProjectionUnavailable","reason":"unmaterialized"}}}),false),
            ("otherKind",None,json!({"error":{"code":-32600,"data":{"kind":"other","reason":"unmaterialized"}}}),false),
            ("otherReason",None,json!({"error":{"code":-32600,"data":{"kind":"historyProjectionUnavailable","reason":"other"}}}),false),
            ("missingReason",None,json!({"error":{"code":-32600,"data":{"kind":"historyProjectionUnavailable"}}}),false),
            ("forgedSuccess",None,json!({"result":{"historyUnavailable":true,"reason":"unmaterialized"}}),false),
            ("forgedEmptySuccess",None,json!({"result":{"historyUnavailable":true,"reason":"unmaterialized","data":[]}}),false),
        ] {
            let(control,mut rpc,listener)=fixture();let server=thread::spawn(move||{
                let(stream,_)=listener.accept().unwrap();stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();let mut socket=WebSocket::from_raw_socket(stream,tungstenite::protocol::Role::Server,None);
                let request=read_request(&mut socket);assert_eq!(request["method"],"thread/turns/list");assert_eq!(request["params"]["threadId"],"original");assert_eq!(request["params"]["cursor"],json!(page_cursor));
                let mut response=payload;response["id"]=request["id"].clone();socket.send(Message::Text(response.to_string().into())).unwrap();assert_closed_without_input(&mut socket);
            });
            let result=execute(&control.0,&mut rpc,"original",ConversationRequest::Page{cursor:page_cursor.map(str::to_owned)},&AtomicBool::new(false));assert_eq!(result.is_ok(),waiting,"{case}");
            if waiting {let value=result.unwrap();assert_eq!(value,json!({"historyUnavailable":true,"reason":"unmaterialized"}));assert!(value.get("data").is_none());assert!(value.get("activeTurnId").is_none());}
            assert!(!control.0.unknown.load(Ordering::Acquire));assert!(control.0.detail.lock().unwrap().is_none());drop(rpc);server.join().unwrap();
        }
    }
    #[test]
    fn passive_attach_preserves_metadata_when_history_is_unmaterialized() {
        let (control,mut rpc,listener)=fixture();let cwd=std::env::current_dir().unwrap().canonicalize().unwrap();let expected_cwd=cwd.clone();
        let server=thread::spawn(move||{
            let(stream,_)=listener.accept().unwrap();let mut socket=WebSocket::from_raw_socket(stream,tungstenite::protocol::Role::Server,None);
            let init=read_request(&mut socket);assert_eq!(init["method"],"initialize");reply(&mut socket,&init,json!({}));assert_eq!(read_request(&mut socket)["method"],"initialized");
            let request=read_request(&mut socket);assert_eq!(request["method"],PASSIVE_SUBSCRIBE);reply(&mut socket,&request,json!({"thread":{"id":"original","cwd":expected_cwd,"turns":[]},"model":"actual-model","modelProvider":"actual-provider","reasoningEffort":"high","serviceTier":"ultrafast"}));
            let history=read_request(&mut socket);assert_eq!(history["method"],"thread/turns/list");socket.send(Message::Text(json!({"id":history["id"],"error":{"code":-32600,"message":"not materialized","data":{"kind":"historyProjectionUnavailable","reason":"unmaterialized"}}}).to_string().into())).unwrap();
        });
        let(sender,receiver)=mpsc::sync_channel(2);let(reply,attached)=mpsc::sync_channel(1);sender.send(Work{request:ConversationRequest::Attach,reply,deadline:Instant::now()+RPC_TIMEOUT,send_context:None}).ok().unwrap();let(reply,detached)=mpsc::sync_channel(1);sender.send(Work{request:ConversationRequest::Detach,reply,deadline:Instant::now()+RPC_TIMEOUT,send_context:None}).ok().unwrap();
        serve(&control.0,&mut rpc,"original",&cwd,receiver,&AtomicBool::new(false),&Mutex::new(None)).unwrap();let value=attached.recv().unwrap().unwrap();assert_eq!(value["historyUnavailable"],true);assert_eq!(value["activeTurnKnown"],false);assert!(value["activeTurnId"].is_null());assert_eq!(value["model"],"actual-model");assert_eq!(value["serviceTier"],"ultrafast");assert!(detached.recv().unwrap().is_ok());drop(rpc);server.join().unwrap();
    }
    #[test]
    fn deleted_thread_invalidates_both_pending_maps_and_detail_but_releases_only_after_join() {
        for unknown in [false,true] {
            let(control,mut rpc,_listener)=fixture();install_finished_worker(&control);control.0.unknown.store(unknown,Ordering::Release);
            let request=json!({"id":7,"method":"item/tool/requestUserInput","params":{"threadId":"original","questions":[]}});control.observer_message("original",request.clone()).unwrap();observe_message(&control.0,"original",request).unwrap();
            *control.0.detail.lock().unwrap()=Some(DetailSnapshot{id:"snapshot".into(),turn_id:"turn".into(),item_id:"item".into(),text:Arc::from("old content")});
            let deleted=json!({"method":"thread/deleted","params":{"threadId":"original"}});assert!(control.observer_message("original",deleted.clone()).is_err());assert!(observe_message(&control.0,"original",deleted).is_err());
            assert!(control.is_deleted());assert!(control.pending_snapshot().unwrap().2.is_empty());assert!(control.0.detail.lock().unwrap().is_none());assert!(control.terminal_write(||Ok(1)).is_err(),"알림 수신만으로 입력 소유권을 해제하면 안 된다");
            let poll=control.request("original",Path::new("."),ConversationRequest::Poll).unwrap();assert_eq!(poll["events"].as_array().unwrap().len(),1);assert_eq!(poll["events"][0]["method"],"thread/deleted");assert_eq!(poll["connected"],false);
            assert!(execute(&control.0,&mut rpc,"original",ConversationRequest::Detail{turn_id:"turn".into(),item_id:"item".into(),offset:Some(1),snapshot_id:Some("snapshot".into())},&AtomicBool::new(false)).is_err());
            assert_eq!(control.request("original",Path::new("."),ConversationRequest::Detach).is_err(),unknown);assert_eq!(control.0.owned.load(Ordering::Acquire),unknown);
            assert!(control.request("original",Path::new("."),ConversationRequest::Attach).is_err());control.cancel();
        }
    }
    #[test]
    fn lost_mutation_reply_blocks_later_send_and_never_replays_the_message() {
        let (control,mut rpc,listener) = fixture();
        let server = thread::spawn(move || {
            let (stream,_) = listener.accept().unwrap();
            let mut socket = WebSocket::from_raw_socket(stream,tungstenite::protocol::Role::Server,None);
            let send = read_request(&mut socket); assert_eq!(send["method"],START_USER_IF_IDLE);
            assert_eq!(send["params"]["threadId"],"original");
            assert_eq!(send["params"]["clientUserMessageId"],"00000000-0000-0000-0000-000000000001");
            assert!(send["params"].get("approvalPolicy").is_none());
            // 실제 서버가 요청을 적용한 직후 응답 없이 연결을 닫는 경계다.
            drop(socket); 1
        });
        let request = || ConversationRequest::Send {request_id:"00000000-0000-0000-0000-000000000001".into(),text:"hello".into(),attachments:vec![],model:None,effort:None};
        assert!(execute(&control.0,&mut rpc,"original",request(),&AtomicBool::new(false)).is_err());
        assert!(control.0.unknown.load(Ordering::Acquire));
        let error = execute(&control.0,&mut rpc,"original",request(),&AtomicBool::new(false)).unwrap_err();
        assert!(error.contains("이전 전송 결과")); assert_eq!(server.join().unwrap(),1);
    }
    #[test]
    fn approval_during_rpc_is_preserved_and_consumed_only_once() {
        let (control,mut rpc,listener) = fixture();
        let server = thread::spawn(move || {
            let (stream,_) = listener.accept().unwrap(); stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            let mut socket = WebSocket::from_raw_socket(stream,tungstenite::protocol::Role::Server,None);
            let read = read_request(&mut socket);
            socket.send(Message::Text(json!({"id":read["id"],"method":"item/commandExecution/requestApproval","params":{"threadId":"original","turnId":"turn","itemId":"item"}}).to_string().into())).unwrap();
            reply(&mut socket,&read,json!({"data":[],"nextCursor":null}));
            let approval = read_request(&mut socket); assert!(approval.get("method").is_none()); assert_eq!(approval["result"],json!({"decision":"accept"}));
            assert!(socket.read().is_err(),"동일 승인 응답을 재전송하면 안 된다");
        });
        call(&control.0,&mut rpc,"original","model/list",json!({}),&AtomicBool::new(false)).unwrap();
        assert_eq!(control.0.events.lock().unwrap().pending.len(),1);
        let respond = || ConversationRequest::Respond {request_id:"1".into(),result:json!({"decision":"accept"})};
        execute(&control.0,&mut rpc,"original",respond(),&AtomicBool::new(false)).unwrap();
        assert!(execute(&control.0,&mut rpc,"original",respond(),&AtomicBool::new(false)).is_err());
        assert_eq!(control.0.events.lock().unwrap().pending.len(),1,"처리 확인 전에는 승인 소유권을 유지해야 한다");
        drop(rpc); server.join().unwrap();
    }
    #[test]
    fn acknowledged_interrupt_does_not_erase_an_earlier_unknown_send() {
        let (control,mut rpc,listener) = fixture(); control.0.unknown.store(true,Ordering::Release);
        {let mut events=control.0.events.lock().unwrap();events.connected=true;events.gui_interrupt_supported=true;}
        let server = thread::spawn(move || {
            let (stream,_) = listener.accept().unwrap(); let mut socket = WebSocket::from_raw_socket(stream,tungstenite::protocol::Role::Server,None);
            let request = read_request(&mut socket); assert_eq!(request["params"],json!({"threadId":"original","turnId":"original-turn"})); reply(&mut socket,&request,json!({}));
        });
        execute(&control.0,&mut rpc,"original",ConversationRequest::Interrupt{turn_id:"original-turn".into()},&AtomicBool::new(false)).unwrap();
        assert!(control.0.unknown.load(Ordering::Acquire)); server.join().unwrap();
    }
    #[test]
    fn request_queue_rejects_oversized_data_before_admission() {
        let request = ConversationRequest::Send {request_id:Uuid::new_v4().to_string(),text:"a".repeat(65537),attachments:vec![],model:None,effort:None};
        assert!(validate_request(&request).is_err());
        assert!(bounded_page(json!({"data":vec![json!({});21]}),20).is_err());
        assert!(validate_request(&ConversationRequest::Page{cursor:Some("a".repeat(4097))}).is_err());
    }
    #[test]
    fn deadline_closes_the_owned_socket_and_reaps_its_watcher() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut stream = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let (mut peer,_) = listener.accept().unwrap();peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let transport = Arc::new(Mutex::new(Transport{socket:Some(stream.try_clone().unwrap()),proxy:None}));
        let cancelled = Arc::new(AtomicBool::new(false));
        let start = Instant::now();
        let mut watch = DeadlineWatch::start(Arc::new(Mutex::new(Some(start+Duration::from_millis(25)))),transport,cancelled.clone(),Arc::new(AtomicBool::new(false))).unwrap();
        // 감시자의 실제 종료를 먼저 회수한다. 스케줄러 지연을 연결 회수 실패로
        // 해석하지 않고, 이어지는 두 read의 실제 EOF/연결 종료만 판정한다.
        watch.join.take().unwrap().join().unwrap();
        let result = stream.read(&mut [0u8;1]);
        let closed=|result:&io::Result<usize>|match result {Ok(0)=>true,Err(error)=>matches!(error.kind(),io::ErrorKind::ConnectionAborted|io::ErrorKind::ConnectionReset),_=>false};
        assert!(closed(&result),"socket shutdown 대신 시간 초과·무관한 오류가 관측됨: {result:?}");
        assert!(cancelled.load(Ordering::Acquire));
        drop(watch);
        let peer_result=peer.read(&mut [0u8;1]);assert!(closed(&peer_result),"상대 연결도 종료되어야 한다: {peer_result:?}");
        eprintln!("기한 감시 수명 시험: local={result:?}, peer={peer_result:?}, watcherJoined=true, elapsed={:?}",start.elapsed());
    }
    #[test]
    fn pending_approval_survives_event_overflow_and_duplicate_delivery() {
        let (control,_rpc,_listener) = fixture();
        let request = json!({"id":"same","method":"item/tool/requestUserInput","params":{"threadId":"original","questions":[{"id":"q","question":"answer?"}]}});
        observe_message(&control.0,"original",request.clone()).unwrap();
        for _ in 0..EVENT_COUNT*2 {observe_message(&control.0,"original",json!({"method":"item/agentMessage/delta","params":{"threadId":"original","delta":"x"}})).unwrap();}
        observe_message(&control.0,"original",request).unwrap();
        let mut events = control.0.events.lock().unwrap();
        assert_eq!(events.pending.len(),1); assert!(events.gap); assert_eq!(events.poll()["pendingRequests"].as_array().unwrap().len(),1);
        assert_eq!(events.pending.len(),1,"화면 조회가 승인 요청을 소비하면 안 된다");
    }
    #[test]
    fn terminal_observer_preserves_requests_during_reads_without_claiming_or_responding() {
        let (control,mut rpc,listener) = fixture();
        let server = thread::spawn(move || {
            let (stream,_) = listener.accept().unwrap(); stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            let mut socket = WebSocket::from_raw_socket(stream,tungstenite::protocol::Role::Server,None);
            let request = read_request(&mut socket); assert_eq!(request["method"],"thread/read");
            socket.send(Message::Text(json!({"id":44,"method":"item/tool/requestUserInput","params":{"threadId":"original","questions":[{"id":"q","question":"원래 터미널 질문"}]}}).to_string().into())).unwrap();
            socket.send(Message::Text(json!({"method":"item/agentMessage/delta","params":{"threadId":"original","delta":"두 번 표시하면 안 됨"}}).to_string().into())).unwrap();
            reply(&mut socket,&request,json!({"thread":{"id":"original"}}));
            assert!(socket.read().is_err(),"관측 연결에서 질문 응답을 보내면 안 된다");
        });
        control.observer_call(&mut rpc,"original","thread/read",json!({"threadId":"original","includeTurns":false})).unwrap();
        let (owned,connected,pending) = control.pending_snapshot().unwrap();
        assert!(!owned&&!connected); assert_eq!(pending.len(),1); assert_eq!(pending[0]["params"]["questions"][0]["question"],"원래 터미널 질문"); assert_eq!(pending[0]["guiOwned"],false);
        assert!(control.0.events.lock().unwrap().queue.is_empty(),"관측 연결이 GUI delta를 중복 적재하면 안 된다");
        assert!(execute(&control.0,&mut rpc,"original",ConversationRequest::Respond{request_id:"44".into(),result:json!({"answers":{"q":{"answers":["답"]}}})},&AtomicBool::new(false)).is_err());
        drop(rpc); server.join().unwrap();
    }
    #[test]
    fn observer_and_gui_deduplicate_one_pending_request_and_resolution_clears_both() {
        let (control,_rpc,_listener)=fixture();
        let request=json!({"id":9,"method":"item/commandExecution/requestApproval","params":{"threadId":"original"}});
        control.observer_message("original",request.clone()).unwrap(); observe_message(&control.0,"original",request).unwrap();
        let (_,_,pending)=control.pending_snapshot().unwrap(); assert_eq!(pending.len(),1); assert_eq!(pending[0]["guiOwned"],true);
        control.observer_message("original",json!({"method":"serverRequest/resolved","params":{"threadId":"original","requestId":9}})).unwrap();
        assert!(control.pending_snapshot().unwrap().2.is_empty()); assert!(control.0.events.lock().unwrap().pending.is_empty());
    }
    #[test]
    fn resolved_ids_block_late_replay_and_stop_request_tracking_at_the_bounded_limit() {
        let(control,_rpc,_listener)=fixture();
        let pending=json!({"id":9,"method":"item/tool/requestUserInput","params":{"threadId":"original","questions":[]}});
        control.observer_message("original",pending.clone()).unwrap();observe_message(&control.0,"original",pending.clone()).unwrap();
        control.observer_message("original",json!({"method":"serverRequest/resolved","params":{"threadId":"original","requestId":9}})).unwrap();
        install_finished_worker(&control);control.request("original",Path::new("."),ConversationRequest::Detach).unwrap();
        // 새 Attach의 준비와 같은 경계에서도 해결 기록을 지우지 않는다.
        assert!(!control.0.events.lock().unwrap().blocks_attach(false,false));assert!(control.0.events.lock().unwrap().answered.contains("9"));
        observe_message(&control.0,"original",pending.clone()).unwrap();control.observer_message("original",pending).unwrap();assert!(control.pending_snapshot().unwrap().2.is_empty());
        let(control,_rpc,_listener)=fixture();
        for id in 0..1024 {control.observer_message("original",json!({"method":"serverRequest/resolved","params":{"threadId":"original","requestId":id}})).unwrap();}
        control.observer_message("original",json!({"method":"serverRequest/resolved","params":{"threadId":"original","requestId":0}})).unwrap();
        let pending=json!({"id":9000,"method":"item/commandExecution/requestApproval","params":{"threadId":"original"}});observe_message(&control.0,"original",pending.clone()).unwrap();control.observer_message("original",pending).unwrap();
        {let mut events=control.0.events.lock().unwrap();events.connected=true;events.observer_connected=true;}
        assert!(control.observer_message("original",json!({"method":"serverRequest/resolved","params":{"threadId":"original","requestId":1024}})).unwrap_err().contains("상한"));
        assert!(observe_message(&control.0,"original",json!({"id":1024,"method":"item/commandExecution/requestApproval","params":{"threadId":"original"}})).is_err(),"상한 뒤 다른 연결의 늦은 사본도 삽입하면 안 된다");
        let(_,connected,pending)=control.pending_snapshot().unwrap();assert!(!connected);assert_eq!(pending.len(),1);assert_eq!(pending[0]["requestId"],"9000");assert_eq!(pending[0]["metadataAvailable"],false);
        let events=control.0.events.lock().unwrap();assert_eq!(events.answered.len(),1024);assert!(events.answered.contains("0"));assert!(events.blocks_attach(false,false));assert!(events.blocks_release(true));assert!(events.error.as_deref().is_some_and(|error|error.contains("상한")));
    }
    #[test]
    fn pending_metadata_is_current_only_when_the_connection_holding_that_id_is_live() {
        let (control,_rpc,_listener)=fixture();
        let request=json!({"id":9,"method":"item/commandExecution/requestApproval","params":{"threadId":"original"}});
        control.observer_message("original",request.clone()).unwrap();
        control.0.events.lock().unwrap().observer_connected=true;
        assert_eq!(control.pending_snapshot().unwrap().2[0]["metadataAvailable"],true);
        {let mut events=control.0.events.lock().unwrap();events.observer_connected=false;events.connected=true;}
        assert_eq!(control.pending_snapshot().unwrap().2[0]["metadataAvailable"],false,"관계없는 살아 있는 GUI 연결이 과거 관측을 확정하면 안 된다");
        observe_message(&control.0,"original",request).unwrap();
        let snapshot=control.pending_snapshot().unwrap().2;assert_eq!(snapshot.len(),1);assert_eq!(snapshot[0]["metadataAvailable"],true);assert_eq!(snapshot[0]["guiOwned"],true);
        {let mut events=control.0.events.lock().unwrap();events.connected=false;events.observer_connected=true;events.pending.get_mut("9").unwrap()["responded"]=json!(true);}
        let snapshot=control.pending_snapshot().unwrap().2;assert_eq!(snapshot[0]["metadataAvailable"],true);assert_eq!(snapshot[0]["responded"],true);
        control.0.events.lock().unwrap().observed_pending.clear();
        assert_eq!(control.pending_snapshot().unwrap().2[0]["metadataAvailable"],false);
        assert_eq!(control.pending_snapshot().unwrap().2.len(),1,"관측 단절은 서버 해결 확인이 아니므로 요청을 제거하면 안 된다");
    }
    fn install_finished_worker(control: &ConversationConnection) {
        let (sender,receiver)=mpsc::sync_channel(1);drop(receiver);
        let join=thread::spawn(||()); let deadline=Instant::now()+Duration::from_secs(2);
        while !join.is_finished() && Instant::now()<deadline {thread::yield_now();}
        assert!(join.is_finished());
        *control.0.worker.lock().unwrap()=Some(Worker{sender,cancelled:Arc::new(AtomicBool::new(false)),transport:Arc::new(Mutex::new(Transport::default())),join:Some(join),joined:false,closing:None});
        *control.0.input.lock().unwrap()=true;control.0.owned.store(true,Ordering::Release);
    }
    fn install_actor(control: &ConversationConnection, actor: impl FnOnce(Arc<Control>,mpsc::Receiver<Work>,Arc<AtomicBool>)+Send+'static) {
        let(sender,receiver)=mpsc::sync_channel(8);let cancelled=Arc::new(AtomicBool::new(false));
        let state=control.0.clone();let running=cancelled.clone();
        let join=thread::spawn(move||{actor(state.clone(),receiver,running);state.events.lock().unwrap().connected=false;});
        *control.0.worker.lock().unwrap()=Some(Worker{sender,cancelled,transport:Arc::new(Mutex::new(Transport::default())),join:Some(join),joined:false,closing:None});
        *control.0.input.lock().unwrap()=true;control.0.owned.store(true,Ordering::Release);
    }
    #[test]
    fn explicit_terminal_handoff_preserves_general_pending_and_joins_before_pty_input() {
        for method in ["item/commandExecution/requestApproval","item/fileChange/requestApproval","item/tool/requestUserInput","item/permissions/requestApproval"] {
            let(control,mut rpc,listener)=fixture();let cwd=env::current_dir().unwrap().canonicalize().unwrap();let expected_cwd=cwd.clone();
            let pending=json!({"id":17,"method":method,"params":{"threadId":"original","availableDecisions":[],"questions":[],"reason":"안전 선택지가 없거나 지원하지 않는 일반 요청 원문"}});let expected=pending.clone();
            let server=thread::spawn(move||{
                let(stream,_)=listener.accept().unwrap();stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();let mut socket=WebSocket::from_raw_socket(stream,tungstenite::protocol::Role::Server,None);
                let init=read_request(&mut socket);assert_eq!(init["method"],"initialize");reply(&mut socket,&init,json!({}));assert_eq!(read_request(&mut socket)["method"],"initialized");
                let subscribe=read_request(&mut socket);assert_eq!(subscribe["method"],PASSIVE_SUBSCRIBE);
                socket.send(Message::Text(expected.to_string().into())).unwrap();reply(&mut socket,&subscribe,json!({"thread":{"id":"original","cwd":expected_cwd,"turns":[]}}));
                let history=read_request(&mut socket);assert_eq!(history["method"],"thread/turns/list");reply(&mut socket,&history,json!({"data":[]}));
                assert_closed_without_input(&mut socket);
            });
            install_actor(&control,move|state,receiver,cancelled|{serve(&state,&mut rpc,"original",&cwd,receiver,&cancelled,&Mutex::new(None)).unwrap();});
            // 동일 원문의 두 연결 사본을 보존한다. Attach 응답은 초기 구독 완료의 장벽이다.
            control.observer_message("original",pending.clone()).unwrap();control.0.events.lock().unwrap().observer_connected=true;
            let(reply,attached)=mpsc::sync_channel(1);control.0.worker.lock().unwrap().as_ref().unwrap().sender.try_send(Work{request:ConversationRequest::Attach,reply,deadline:Instant::now()+RPC_TIMEOUT,send_context:None}).ok().unwrap();attached.recv_timeout(Duration::from_secs(3)).unwrap().unwrap();
            assert!(control.request("original",Path::new("."),ConversationRequest::Detach).is_err(),"자동 정리 규칙은 완화하면 안 된다");
            assert!(control.0.worker.lock().unwrap().as_ref().unwrap().closing.is_none(),"거절된 자동 정리는 살아 있는 actor의 admission을 복원해야 한다");
            assert!(control.terminal_write(||Ok(1)).is_err());
            control.request("original",Path::new("."),ConversationRequest::HandoffToTerminal).unwrap();
            assert!(control.0.worker.lock().unwrap().is_none());assert!(!control.0.owned.load(Ordering::Acquire));assert_eq!(control.terminal_write(||Ok(1)).unwrap(),1);
            let(owned,connected,snapshot)=control.pending_snapshot().unwrap();assert!(!owned&&!connected);assert_eq!(snapshot.len(),1);assert_eq!(snapshot[0]["method"],method);assert_eq!(snapshot[0]["params"],pending["params"]);assert_eq!(snapshot[0]["requestId"],"17");assert_eq!(snapshot[0]["guiOwned"],false);assert_eq!(snapshot[0]["metadataAvailable"],true);assert_ne!(snapshot[0]["responded"],true);
            {let events=control.0.events.lock().unwrap();assert_eq!(events.pending.len(),1);assert_eq!(events.observed_pending.len(),1);assert!(events.answered.is_empty());}
            server.join().unwrap();
            control.observer_message("original",json!({"method":"serverRequest/resolved","params":{"threadId":"original","requestId":17}})).unwrap();assert!(control.pending_snapshot().unwrap().2.is_empty());
        }
    }
    #[test]
    fn handoff_rejects_every_unsafe_copy_and_unknown_without_releasing_input() {
        for reason in ["guiResponded","observerResponded","answered","verification","mcpForm","futureMethod","unknown"] {
            let(control,_rpc,_listener)=fixture();install_finished_worker(&control);
            let method=match reason {"verification"|"mcpForm"=>"mcpServer/elicitation/request","futureMethod"=>"item/future/requestApproval",_=>"item/commandExecution/requestApproval"};
            let request=json!({"id":9,"method":method,"params":{"threadId":"original","mode":if reason=="verification" {"openai/userVerification"} else {"form"}}});
            control.observer_message("original",request.clone()).unwrap();observe_message(&control.0,"original",request).unwrap();
            {let mut events=control.0.events.lock().unwrap();match reason {"guiResponded"=>events.pending.get_mut("9").unwrap()["responded"]=json!(true),"observerResponded"=>events.observed_pending.get_mut("9").unwrap()["responded"]=json!(true),"answered"=>{events.answered.insert("9".into());},"unknown"=>control.0.unknown.store(true,Ordering::Release),_=>{}}}
            assert!(control.request("original",Path::new("."),ConversationRequest::HandoffToTerminal).is_err(),"{reason} 상태에서 입력을 인계하면 안 된다");assert!(control.0.owned.load(Ordering::Acquire));assert!(control.terminal_write(||Ok(1)).is_err());assert_eq!(control.pending_snapshot().unwrap().2.len(),1);control.cancel();
        }
    }
    #[test]
    fn handoff_closes_admission_and_rechecks_late_requests_after_join() {
        for late in ["general","owner","responded","unknown","panic"] {
            let(control,_rpc,_listener)=fixture();let(entered,ready)=mpsc::sync_channel(1);let(release,released)=mpsc::sync_channel(1);
            control.0.events.lock().unwrap().connected=true;
            install_actor(&control,move|state,receiver,_cancelled|{
                let work=receiver.recv_timeout(Duration::from_secs(3)).unwrap();assert!(matches!(work.request,ConversationRequest::HandoffToTerminal));work.reply.send(Ok(json!({}))).unwrap();entered.send(()).unwrap();released.recv_timeout(Duration::from_secs(3)).unwrap();
                if late=="panic" {panic!("인계 중 actor 종료 실패 합성");}
                if late=="unknown" {state.unknown.store(true,Ordering::Release);} else {
                    let method=if late=="owner" {"mcpServer/elicitation/request"} else {"item/commandExecution/requestApproval"};
                    observe_message(&state,"original",json!({"id":19,"method":method,"params":{"threadId":"original","mode":"openai/userVerification"}})).unwrap();
                    if late=="responded" {state.events.lock().unwrap().pending.get_mut("19").unwrap()["responded"]=json!(true);}
                }
            });
            let request_control=control.clone();let caller=thread::spawn(move||request_control.request("original",Path::new("."),ConversationRequest::HandoffToTerminal));
            ready.recv_timeout(Duration::from_secs(3)).unwrap();assert!(control.terminal_write(||Ok(1)).is_err(),"join 전에는 PTY 입력을 허용하면 안 된다");
            // release가 worker 잠금으로 join 중일 수 있으므로 admission 검사는 별도 요청자가
            // 종료를 기다리는 것이 허용된다. 종료 뒤에는 살아 있는 actor가 없어 wire 입력은 0이다.
            release.send(()).unwrap();let result=caller.join().unwrap();assert_eq!(result.is_ok(),late=="general");
            assert_eq!(control.terminal_write(||Ok(1)).is_ok(),late=="general");assert_eq!(control.0.owned.load(Ordering::Acquire),late!="general");
            if late=="panic" {let poll=control.request("original",Path::new("."),ConversationRequest::Poll).unwrap();assert_eq!(poll["connected"],false);assert!(poll["error"].as_str().is_some_and(|error|error.contains("비정상 종료")));}
            if late!="panic"&&late!="unknown" {assert_eq!(control.pending_snapshot().unwrap().2.len(),1);}
            if late!="general" {assert!(control.request("original",Path::new("."),ConversationRequest::Page{cursor:None}).is_err());}control.cancel();
        }
    }
    #[test]
    fn queued_response_is_settled_before_handoff_and_rejection_restores_live_admission() {
        let(control,_rpc,_listener)=fixture();let(entered,ready)=mpsc::sync_channel(1);let(release,released)=mpsc::sync_channel(1);
        observe_message(&control.0,"original",json!({"id":9,"method":"item/commandExecution/requestApproval","params":{"threadId":"original"}})).unwrap();
        install_actor(&control,move|state,receiver,cancelled|{
            let work=receiver.recv_timeout(Duration::from_secs(3)).unwrap();assert!(matches!(work.request,ConversationRequest::Respond{..}));entered.send(()).unwrap();released.recv_timeout(Duration::from_secs(3)).unwrap();
            {let mut events=state.events.lock().unwrap();events.pending.get_mut("9").unwrap()["responded"]=json!(true);events.answered.insert("9".into());}work.reply.send(Ok(json!({}))).unwrap();
            let work=receiver.recv_timeout(Duration::from_secs(3)).unwrap();assert!(matches!(work.request,ConversationRequest::HandoffToTerminal));assert!(state.events.lock().unwrap().blocks_release(true));work.reply.send(Err("송신된 응답의 해결을 기다립니다.".into())).unwrap();
            let work=receiver.recv_timeout(Duration::from_secs(3)).unwrap();assert!(matches!(work.request,ConversationRequest::Models{..}));work.reply.send(Ok(json!({"data":[]}))).unwrap();
            while !cancelled.load(Ordering::Acquire) {thread::sleep(Duration::from_millis(1));}
        });
        let response_control=control.clone();let response=thread::spawn(move||response_control.request("original",Path::new("."),ConversationRequest::Respond{request_id:"9".into(),result:json!({"decision":"accept"})}));ready.recv_timeout(Duration::from_secs(3)).unwrap();
        let request_control=control.clone();let handoff=thread::spawn(move||request_control.request("original",Path::new("."),ConversationRequest::HandoffToTerminal));
        let deadline=Instant::now()+Duration::from_secs(3);loop {if control.0.worker.lock().unwrap().as_ref().unwrap().closing.is_some(){break;}assert!(Instant::now()<deadline);thread::yield_now();}
        for request in [ConversationRequest::Respond{request_id:"9".into(),result:json!({"decision":"decline"})},ConversationRequest::Send{request_id:Uuid::new_v4().to_string(),text:"경합 입력".into(),attachments:vec![],model:None,effort:None}] {assert!(control.request("original",Path::new("."),request).unwrap_err().contains("인계 중"));}
        assert!(control.0.sent.lock().unwrap().is_empty());assert!(control.terminal_write(||Ok(1)).is_err());release.send(()).unwrap();response.join().unwrap().unwrap();assert!(handoff.join().unwrap().is_err());
        assert!(control.0.worker.lock().unwrap().as_ref().unwrap().closing.is_none());assert!(control.terminal_write(||Ok(1)).is_err());control.request("original",Path::new("."),ConversationRequest::Models{cursor:None}).unwrap();control.cancel();
    }
    #[test]
    fn observer_defers_until_verified_loaded_and_marks_only_successful_subscription() {
        for outcome in ["success","unsupported","internal"] {
            let(control,mut rpc,listener)=fixture();let cwd=env::current_dir().unwrap().canonicalize().unwrap();let expected_cwd=cwd.clone();
            let server=thread::spawn(move||{
                let(stream,_)=listener.accept().unwrap();stream.set_read_timeout(Some(Duration::from_secs(4))).unwrap();let mut socket=WebSocket::from_raw_socket(stream,tungstenite::protocol::Role::Server,None);let mut methods=Vec::new();
                // 최초 thread/read가 notLoaded이면 수동 구독을 보내지 않고,
                // 동일 대화가 loaded임을 검증한 두 번째 조회 뒤에만 구독한다.
                for state in ["notLoaded","active"] {
                    let request=read_request(&mut socket);assert_eq!(request["method"],"thread/read");assert_eq!(request["params"],json!({"threadId":"original","includeTurns":false}));methods.push("thread/read");reply(&mut socket,&request,json!({"thread":{"id":"original","cwd":expected_cwd,"status":{"type":state}}}));
                }
                let subscribe=read_request(&mut socket);assert_eq!(subscribe["method"],PASSIVE_SUBSCRIBE);assert_eq!(subscribe["params"],json!({"threadId":"original"}));methods.push(PASSIVE_SUBSCRIBE);
                if outcome=="success" {
                    socket.send(Message::Text(json!({"id":31,"method":"item/tool/requestUserInput","params":{"threadId":"original","questions":[{"id":"q","question":"재개 후의 질문 원문"}]}}).to_string().into())).unwrap();
                    reply(&mut socket,&subscribe,json!({"thread":{"id":"original","cwd":expected_cwd,"turns":[]}}));
                } else {socket.send(Message::Text(json!({"id":subscribe["id"],"error":{"code":if outcome=="unsupported" {-32601} else {-32603},"message":"준비 여부를 분류할 최종 오류 계약 없음"}}).to_string().into())).unwrap();}
                let request=read_request(&mut socket);assert_eq!(request["method"],"thread/read","실패한 구독을 성공으로 간주하거나 일반 resume/문자열 기반 재시도를 하면 안 된다");methods.push("thread/read");reply(&mut socket,&request,json!({"thread":{"id":"original","cwd":expected_cwd,"status":{"type":"idle"}}}));
                assert_closed_without_input(&mut socket);methods
            });
            let states=std::cell::RefCell::new(Vec::new());let stopped=AtomicBool::new(false);
            let sink=|event|if let CodexEvent::Status(state)=event {
                states.borrow_mut().push(state.clone());
                if state=="notLoaded" {assert!(!control.0.events.lock().unwrap().observer_connected);assert!(control.pending_snapshot().unwrap().2.is_empty());}
                if state=="running" {
                    let(owned,connected,pending)=control.pending_snapshot().unwrap();assert!(!owned&&!connected);assert_eq!(control.0.events.lock().unwrap().observer_connected,outcome=="success");assert_eq!(pending.len(),usize::from(outcome=="success"));
                    if outcome=="success" {assert_eq!(pending[0]["params"]["questions"][0]["question"],"재개 후의 질문 원문");assert_eq!(pending[0]["guiOwned"],false);assert_eq!(pending[0]["metadataAvailable"],true);}
                }
                if state=="idle" {stopped.store(true,Ordering::Release);}
            };
            observe(&mut rpc,Some("original".into()),&cwd,&stopped,&sink,Some(&control));
            assert_eq!(*states.borrow(),["notLoaded","running","idle"]);assert!(!control.0.events.lock().unwrap().observer_connected);assert!(!control.0.owned.load(Ordering::Acquire));assert_eq!(control.pending_snapshot().unwrap().2.len(),usize::from(outcome=="success"));drop(rpc);assert_eq!(server.join().unwrap(),["thread/read","thread/read",PASSIVE_SUBSCRIBE,"thread/read"]);
        }
    }
    #[test]
    fn observer_preparation_classifier_uses_only_exact_rpc_error_data() {
        for(case,error,retryable)in [
            ("notLoaded",json!({"code":-32600,"data":{"kind":"threadSubscriptionUnavailable","reason":"notLoaded"}}),true),
            ("listenerNotReady",json!({"code":-32600,"data":{"kind":"threadSubscriptionUnavailable","reason":"listenerNotReady"}}),true),
            ("closing",json!({"code":-32600,"data":{"kind":"threadSubscriptionUnavailable","reason":"closing"}}),false),
            ("runtimeChanged",json!({"code":-32600,"data":{"kind":"threadSubscriptionUnavailable","reason":"runtimeChanged"}}),false),
            ("unsupportedRuntime",json!({"code":-32600,"data":{"kind":"threadSubscriptionUnavailable","reason":"unsupportedRuntime"}}),false),
            ("unknownReason",json!({"code":-32600,"data":{"kind":"threadSubscriptionUnavailable","reason":"later"}}),false),
            ("wrongCode",json!({"code":-32603,"data":{"kind":"threadSubscriptionUnavailable","reason":"notLoaded"}}),false),
            ("stringCode",json!({"code":"-32600","data":{"kind":"threadSubscriptionUnavailable","reason":"notLoaded"}}),false),
            ("wrongKind",json!({"code":-32600,"data":{"kind":"other","reason":"notLoaded"}}),false),
            ("wrongType",json!({"code":-32600,"data":{"kind":"threadSubscriptionUnavailable","reason":["notLoaded"]}}),false),
            ("oldDispatcher",json!({"code":-32600,"message":"listenerNotReady notLoaded"}),false),
            ("missingReason",json!({"code":-32600,"data":{"kind":"threadSubscriptionUnavailable"}}),false),
        ] {
            let(control,mut rpc,listener)=fixture();let server=thread::spawn(move||{
                let(stream,_)=listener.accept().unwrap();stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();let mut socket=WebSocket::from_raw_socket(stream,tungstenite::protocol::Role::Server,None);let request=read_request(&mut socket);assert_eq!(request["method"],PASSIVE_SUBSCRIBE);socket.send(Message::Text(json!({"id":request["id"],"error":error}).to_string().into())).unwrap();assert_closed_without_input(&mut socket);
            });
            let result=control.observer_subscribe(&mut rpc,"original",Path::new("."));assert_eq!(result.as_ref().ok(),if retryable{Some(&false)}else{None},"{case}");assert!(!control.0.events.lock().unwrap().observer_connected);assert!(!control.0.unknown.load(Ordering::Acquire));if case=="oldDispatcher"{assert!(result.unwrap_err().contains("호환되는 서버"));}drop(rpc);server.join().unwrap();
        }
    }
    #[test]
    fn observer_preparation_retries_are_spaced_and_bounded_and_terminal_errors_stop() {
        for outcome in ["success","exhausted","terminal"] {
            let(control,mut rpc,listener)=fixture();let cwd=env::current_dir().unwrap().canonicalize().unwrap();let expected_cwd=cwd.clone();
            let server=thread::spawn(move||{
                let(stream,_)=listener.accept().unwrap();stream.set_read_timeout(Some(Duration::from_secs(4))).unwrap();let mut socket=WebSocket::from_raw_socket(stream,tungstenite::protocol::Role::Server,None);let mut subscriptions=Vec::new();
                let attempts=if outcome=="terminal"{2}else{3};
                for attempt in 0..attempts {
                    let read=read_request(&mut socket);assert_eq!(read["method"],"thread/read");reply(&mut socket,&read,json!({"thread":{"id":"original","cwd":expected_cwd,"status":{"type":"active"}}}));
                    let subscribe=read_request(&mut socket);assert_eq!(subscribe["method"],PASSIVE_SUBSCRIBE);assert_eq!(subscribe["params"],json!({"threadId":"original"}));subscriptions.push(Instant::now());
                    if attempt==2&&outcome=="success" {reply(&mut socket,&subscribe,json!({"thread":{"id":"original","cwd":expected_cwd,"turns":[]}}));}
                    else {let reason=if outcome=="terminal"&&attempt==1 {"closing"}else if attempt==0{"notLoaded"}else{"listenerNotReady"};socket.send(Message::Text(json!({"id":subscribe["id"],"error":{"code":-32600,"data":{"kind":"threadSubscriptionUnavailable","reason":reason}}}).to_string().into())).unwrap();}
                }
                let read=read_request(&mut socket);assert_eq!(read["method"],"thread/read");reply(&mut socket,&read,json!({"thread":{"id":"original","cwd":expected_cwd,"status":{"type":"idle"}}}));assert_closed_without_input(&mut socket);
                for pair in subscriptions.windows(2){assert!(pair[1].duration_since(pair[0])>=Duration::from_millis(900),"준비 재시도는 즉시 반복하지 않고 다음 1초 조회 뒤에 해야 한다");}subscriptions.len()
            });
            let stopped=AtomicBool::new(false);let sink=|event|if let CodexEvent::Status(state)=event {if state=="idle"{assert_eq!(control.0.events.lock().unwrap().observer_connected,outcome=="success");stopped.store(true,Ordering::Release);}};
            observe(&mut rpc,Some("original".into()),&cwd,&stopped,&sink,Some(&control));assert!(!control.0.events.lock().unwrap().observer_connected);drop(rpc);assert_eq!(server.join().unwrap(),if outcome=="terminal"{2}else{3});
        }
    }
    #[test]
    fn handed_off_pending_is_rebound_only_by_identical_replay_and_failed_reentry_preserves_it() {
        for outcome in ["same","resolved","missing","changed","startFailure","deleted","responded","stopped"] {
            let(control,mut rpc,listener)=fixture();install_finished_worker(&control);
            let pending=json!({"id":19,"method":"item/commandExecution/requestApproval","params":{"threadId":"original","availableDecisions":[],"reason":"인계 전 원문"}});
            observe_message(&control.0,"original",pending.clone()).unwrap();control.observer_message("original",pending.clone()).unwrap();control.request("original",Path::new("."),ConversationRequest::HandoffToTerminal).unwrap();
            assert!(!control.0.events.lock().unwrap().blocks_attach(false,false));let cwd=env::current_dir().unwrap().canonicalize().unwrap();let expected_cwd=cwd.clone();let expected=pending.clone();
            let(response_sent,response_ready)=mpsc::sync_channel(1);let(replay_release,replay_ready)=mpsc::sync_channel(1);
            let server=thread::spawn(move||{
                let(stream,_)=listener.accept().unwrap();stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();let mut socket=WebSocket::from_raw_socket(stream,tungstenite::protocol::Role::Server,None);let init=read_request(&mut socket);assert_eq!(init["method"],"initialize");
                if outcome=="startFailure"{socket.send(Message::Text(json!({"id":init["id"],"error":{"code":-32603,"message":"합성 초기화 실패"}}).to_string().into())).unwrap();assert_closed_without_input(&mut socket);return;}
                reply(&mut socket,&init,json!({}));assert_eq!(read_request(&mut socket)["method"],"initialized");let subscribe=read_request(&mut socket);assert_eq!(subscribe["method"],PASSIVE_SUBSCRIBE);
                reply(&mut socket,&subscribe,json!({"thread":{"id":"original","cwd":expected_cwd,"turns":[]}}));response_sent.send(()).unwrap();replay_ready.recv_timeout(Duration::from_secs(3)).unwrap();
                match outcome {
                    "same"|"changed"=>{let mut replay=expected;if outcome=="changed"{replay["params"]["reason"]=json!("다른 본문");}socket.send(Message::Text(replay.to_string().into())).unwrap();},
                    "resolved"=>{
                        socket.send(Message::Text(json!({"method":"serverRequest/resolved","params":{"threadId":"original","requestId":19}}).to_string().into())).unwrap();
                        // 실제 CLI의 복사 후 소비 경합: 해결 알림보다 오래된 replay가 늦게 온다.
                        socket.send(Message::Text(expected.to_string().into())).unwrap();
                    },
                    "deleted"=>{socket.send(Message::Text(json!({"method":"thread/deleted","params":{"threadId":"original"}}).to_string().into())).unwrap();},
                    _=>{},
                }
                if matches!(outcome,"same"|"resolved"){let history=read_request(&mut socket);assert_eq!(history["method"],"thread/turns/list");reply(&mut socket,&history,json!({"data":[]}));}assert_closed_without_input(&mut socket);
            });
            let(finished,result)=mpsc::sync_channel(1);let(start,started)=mpsc::sync_channel(1);install_actor(&control,move|state,receiver,cancelled|{started.recv_timeout(Duration::from_secs(3)).unwrap();let value=serve(&state,&mut rpc,"original",&cwd,receiver,&cancelled,&Mutex::new(Some(Instant::now()+Duration::from_secs(1))));finished.send(value).unwrap();});
            let(reply,attached)=mpsc::sync_channel(1);control.0.worker.lock().unwrap().as_ref().unwrap().sender.try_send(Work{request:ConversationRequest::Attach,reply,deadline:Instant::now()+RPC_TIMEOUT,send_context:None}).ok().unwrap();
            start.send(()).unwrap();
            if outcome!="startFailure" {
                response_ready.recv_timeout(Duration::from_secs(3)).unwrap();assert!(matches!(attached.try_recv(),Err(mpsc::TryRecvError::Empty)));assert!(control.terminal_write(||Ok(1)).is_err());assert_eq!(control.pending_snapshot().unwrap().2[0]["guiOwned"],false);
                if outcome=="responded" {let mut events=control.0.events.lock().unwrap();events.pending.get_mut("19").unwrap()["responded"]=json!(true);events.answered.insert("19".into());}
                if outcome=="stopped" {control.0.stopped.store(true,Ordering::Release);}
                replay_release.send(()).unwrap();
            }
            if matches!(outcome,"same"|"resolved") {
                attached.recv_timeout(Duration::from_secs(3)).unwrap().unwrap();
                if outcome=="same" {assert_eq!(control.pending_snapshot().unwrap().2[0]["guiOwned"],true);assert!(control.request("original",Path::new("."),ConversationRequest::Detach).is_err());control.request("original",Path::new("."),ConversationRequest::HandoffToTerminal).unwrap();}
                else {assert!(control.pending_snapshot().unwrap().2.is_empty());control.request("original",Path::new("."),ConversationRequest::Detach).unwrap();}
                assert!(result.recv_timeout(Duration::from_secs(3)).unwrap().is_ok());
            }
            else {
                let error=result.recv_timeout(Duration::from_secs(3)).unwrap().unwrap_err();if outcome=="missing" {assert!(error.contains("기한"),"정상 기한 만료 대신 다른 실패가 발생했다: {error}");}assert!(attached.recv_timeout(Duration::from_secs(3)).is_err());assert!(control.terminal_write(||Ok(1)).is_err());
                let deadline=Instant::now()+Duration::from_secs(3);while !control.0.worker.lock().unwrap().as_ref().unwrap().join.as_ref().unwrap().is_finished(){assert!(Instant::now()<deadline);thread::yield_now();}
                let handoff=control.request("original",Path::new("."),ConversationRequest::HandoffToTerminal);assert_eq!(handoff.is_err(),outcome=="responded");
            }
            let snapshot=control.pending_snapshot().unwrap().2;assert_eq!(snapshot.len(),if matches!(outcome,"resolved"|"deleted"){0}else{1});
            if let Some(request)=snapshot.first() {assert_eq!(request["params"],pending["params"]);assert_eq!(request["guiOwned"],false);assert_eq!(request["responded"]==true,outcome=="responded");}
            assert_eq!(control.is_deleted(),outcome=="deleted");server.join().unwrap();control.cancel();
        }
        for unsafe_copy in ["owned","unknown","responded","answered","owner","unclassified","notHandedOff"] {
            let(control,_rpc,_listener)=fixture();let request=json!({"id":9,"method":"item/commandExecution/requestApproval","params":{"threadId":"original"}});observe_message(&control.0,"original",request.clone()).unwrap();control.observer_message("original",request).unwrap();
            {let mut events=control.0.events.lock().unwrap();events.pending.get_mut("9").unwrap()["guiOwned"]=json!(false);match unsafe_copy {"responded"=>events.observed_pending.get_mut("9").unwrap()["responded"]=json!(true),"answered"=>{events.answered.insert("9".into());},"owner"=>events.observed_pending.get_mut("9").unwrap()["method"]=json!("mcpServer/elicitation/request"),"unclassified"=>events.pending.get_mut("9").unwrap()["method"]=json!("future/request"),"notHandedOff"=>events.pending.get_mut("9").unwrap()["guiOwned"]=json!(true),_=>{}}}
            control.0.owned.store(unsafe_copy=="owned",Ordering::Release);control.0.unknown.store(unsafe_copy=="unknown",Ordering::Release);assert!(control.request("original",Path::new("."),ConversationRequest::Attach).is_err(),"{unsafe_copy}에서 재진입을 허용하면 안 된다");assert!(control.0.worker.lock().unwrap().is_none());assert_eq!(control.pending_snapshot().unwrap().2.len(),1);
        }
    }
    #[test]
    fn failed_attach_releases_input_only_after_a_finished_worker_joins_with_no_pending() {
        let (control,_rpc,_listener)=fixture();install_finished_worker(&control);
        control.request("original",Path::new("."),ConversationRequest::Detach).unwrap();
        assert!(!control.0.owned.load(Ordering::Acquire));assert!(control.0.worker.lock().unwrap().is_none());assert_eq!(control.terminal_write(||Ok(1)).unwrap(),1);
        for source in ["gui","observer","unknown"] {
            let (control,_rpc,_listener)=fixture();install_finished_worker(&control);
            let request=json!({"id":9,"method":"item/commandExecution/requestApproval","params":{"threadId":"original"}});
            match source {"gui"=>observe_message(&control.0,"original",request).unwrap(),"observer"=>control.observer_message("original",request).unwrap(),_=>control.0.unknown.store(true,Ordering::Release)}
            assert!(control.request("original",Path::new("."),ConversationRequest::Detach).is_err());assert!(control.0.owned.load(Ordering::Acquire));assert!(control.terminal_write(||Ok(1)).is_err());
            control.observer_message("original",json!({"method":"serverRequest/resolved","params":{"threadId":"original","requestId":9}})).unwrap();
            if source!="unknown" {control.request("original",Path::new("."),ConversationRequest::Detach).unwrap();assert!(!control.0.owned.load(Ordering::Acquire));}
            control.cancel();
        }
    }
    #[test]
    fn a_delayed_worker_is_not_treated_as_a_failed_attach() {
        let (control,_rpc,_listener)=fixture();let (sender,receiver)=mpsc::sync_channel(1);drop(receiver);
        let cancelled=Arc::new(AtomicBool::new(false));let running=cancelled.clone();
        let join=thread::spawn(move||while !running.load(Ordering::Acquire){thread::sleep(Duration::from_millis(1));});
        *control.0.worker.lock().unwrap()=Some(Worker{sender,cancelled,transport:Arc::new(Mutex::new(Transport::default())),join:Some(join),joined:false,closing:None});
        *control.0.input.lock().unwrap()=true;control.0.owned.store(true,Ordering::Release);
        assert!(!control.release(true,false).unwrap());assert!(control.0.owned.load(Ordering::Acquire));assert!(control.terminal_write(||Ok(1)).is_err());
        control.cancel();assert!(control.0.worker.lock().unwrap().is_none());
    }
    #[cfg(windows)]
    fn staged_send_fixture() -> (SendContext,Attachment,PathBuf) {
        let root=std::env::temp_dir().join(format!("opendiy-send-retention-test-{}",Uuid::new_v4()));
        let store=crate::conversation_attachments::ConversationAttachments::test_store(root.clone());
        let session_id=Uuid::new_v4().to_string();
        let staged=store.stage_image(&session_id,"data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=").unwrap();
        (SendContext{session_id,store},Attachment{r#type:"localImage".into(),path:staged.path},root)
    }
    #[cfg(windows)]
    fn remove_staged_send_fixture(context: SendContext,attachment: Attachment,root: PathBuf) {
        context.store.cleanup_all().unwrap();let path=PathBuf::from(&attachment.path);let directory=path.parent().unwrap().to_owned();
        assert!(path.starts_with(&root)&&directory.starts_with(&root));drop(context);
        if path.exists(){std::fs::remove_file(path).unwrap();}if directory.exists(){std::fs::remove_dir(directory).unwrap();}
        std::fs::remove_file(root.join(".staging.lock")).unwrap();std::fs::remove_dir(root).unwrap();
    }
    #[cfg(windows)]
    #[test]
    fn known_pre_send_rejections_leave_staged_images_removable() {
        for reason in ["noWorker","fullQueue","unknown","invalidInput","expired","cancelled"] {
            let (context,attachment,root)=staged_send_fixture();let (control,mut rpc,listener)=fixture();
            let request=ConversationRequest::Send{request_id:Uuid::new_v4().to_string(),text:"hello".into(),attachments:vec![Attachment{r#type:if reason=="invalidInput" {"unsupported".into()} else {"localImage".into()},path:attachment.path.clone()}],model:None,effort:None};
            let result=match reason {
                "noWorker"=>control.request_for_session("original",Path::new("."),request,&context.session_id,context.store.clone()),
                "fullQueue"=>{
                    let (sender,receiver)=mpsc::sync_channel(1);let (reply,_reply_rx)=mpsc::sync_channel(1);
                    sender.try_send(Work{request:ConversationRequest::Poll,reply,deadline:Instant::now()+RPC_TIMEOUT,send_context:None}).ok().unwrap();
                    let cancelled=Arc::new(AtomicBool::new(false));let running=cancelled.clone();
                    let join=thread::spawn(move||{while !running.load(Ordering::Acquire){thread::sleep(Duration::from_millis(1));}drop(receiver);});
                    *control.0.worker.lock().unwrap()=Some(Worker{sender,cancelled,transport:Arc::new(Mutex::new(Transport::default())),join:Some(join),joined:false,closing:None});
                    control.request_for_session("original",Path::new("."),request,&context.session_id,context.store.clone())
                },
                _=>{
                    if reason=="unknown" {control.0.unknown.store(true,Ordering::Release);}
                    if reason=="expired" {rpc.deadline=Some(Instant::now()-Duration::from_millis(1));}
                    if reason=="cancelled" {rpc.cancelled=Some(Arc::new(AtomicBool::new(true)));}
                    execute_context(&control.0,&mut rpc,"original",request,&AtomicBool::new(false),Some(&context))
                },
            };
            assert!(result.is_err(),"{reason}이 송신 경계에 도달하면 안 된다");
            assert_eq!(control.0.unknown.load(Ordering::Acquire),reason=="unknown","{reason}에서 기존 불명 상태를 훼손하거나 미전송을 불명으로 바꾸면 안 된다");
            context.store.cleanup_staged(&context.session_id,&attachment.path).unwrap();assert!(!Path::new(&attachment.path).exists(),"{reason}에서 미전송 이미지를 보존 처리하면 안 된다");
            control.cancel();drop(rpc);remove_staged_send_fixture(context,attachment,root);
        }
    }
    #[cfg(windows)]
    #[test]
    fn atomic_rejections_rollback_only_this_send_and_unknown_errors_block_next_send() {
        for outcome in ["skippedBusy","notStarted","unsupported","invalidParams","internal","invalidRequest","lost","started"] {
            for previously_retained in [false,true] {
                let(context,attachment,root)=staged_send_fixture();let(control,mut rpc,listener)=fixture();
                if previously_retained {drop(context.store.retain_for_send(&context.session_id,&[attachment.path.clone()]).unwrap());}
                let known_rejected=matches!(outcome,"skippedBusy" | "notStarted" | "unsupported" | "invalidParams");
                let unknown=matches!(outcome,"internal" | "invalidRequest" | "lost");
                let server=thread::spawn(move||{
                    let(stream,_)=listener.accept().unwrap();stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();let mut socket=WebSocket::from_raw_socket(stream,tungstenite::protocol::Role::Server,None);
                    let first=read_request(&mut socket);assert_eq!(first["method"],START_USER_IF_IDLE);
                    if outcome=="lost" {return 1;}
                    match outcome {
                        "unsupported" | "invalidParams" | "internal" | "invalidRequest" => {
                            let code=match outcome {"unsupported"=>-32601,"invalidParams"=>-32602,"invalidRequest"=>-32600,_=>-32603};
                            socket.send(Message::Text(json!({"id":first["id"],"error":{"code":code,"message":"authoritative or uncertain error"}}).to_string().into())).unwrap();
                        },
                        _=>reply(&mut socket,&first,json!({"status":outcome,"turn":{"id":"turn"},"reason":"admission rejected"})),
                    }
                    if unknown {assert!(socket.read().is_err(),"결과 불명 뒤 후속 입력을 보내면 안 된다");1}
                    else {let next=read_request(&mut socket);assert_eq!(next["method"],START_USER_IF_IDLE);assert_ne!(next["params"]["clientUserMessageId"],first["params"]["clientUserMessageId"]);reply(&mut socket,&next,json!({"status":"skippedBusy"}));assert!(socket.read().is_err());2}
                });
                let request=ConversationRequest::Send{request_id:Uuid::new_v4().to_string(),text:String::new(),attachments:vec![Attachment{r#type:"localImage".into(),path:attachment.path.clone()}],model:None,effort:None};
                let first=execute_context(&control.0,&mut rpc,"original",request,&AtomicBool::new(false),Some(&context));
                assert_eq!(first.is_ok(),outcome=="started");assert_eq!(control.0.unknown.load(Ordering::Acquire),unknown);
                assert_eq!(context.store.cleanup_staged(&context.session_id,&attachment.path).is_ok(),known_rejected&&!previously_retained,"{outcome}, 이전 보존={previously_retained}");
                let next=execute_context(&control.0,&mut rpc,"original",ConversationRequest::Send{request_id:Uuid::new_v4().to_string(),text:"새로운 사용자 입력".into(),attachments:vec![],model:None,effort:None},&AtomicBool::new(false),Some(&context)).unwrap_err();
                assert_eq!(next.contains("이전 전송 결과"),unknown);assert_eq!(control.0.unknown.load(Ordering::Acquire),unknown);
                drop(rpc);assert_eq!(server.join().unwrap(),if unknown {1} else {2});remove_staged_send_fixture(context,attachment,root);
            }
        }
    }
    #[cfg(windows)]
    #[test]
    fn actual_send_attempts_retain_images_on_acknowledgement_and_lost_reply() {
        for acknowledge in [true,false] {
            let (context,attachment,root)=staged_send_fixture();let (control,mut rpc,listener)=fixture();
            let expected_path=attachment.path.clone();let store=context.store.clone();let session_id=context.session_id.clone();
            let server=thread::spawn(move||{
                let(stream,_)=listener.accept().unwrap();let mut socket=WebSocket::from_raw_socket(stream,tungstenite::protocol::Role::Server,None);
                let send=read_request(&mut socket);assert_eq!(send["method"],START_USER_IF_IDLE);assert_eq!(send["params"]["input"][0]["path"],expected_path);
                assert!(store.cleanup_staged(&session_id,&expected_path).is_err(),"송신 바이트 관측 시에는 원문이 보존되어야 한다");
                if acknowledge {reply(&mut socket,&send,json!({"status":"started","turn":{"id":"original-turn"}}));}1
            });
            let request=ConversationRequest::Send{request_id:Uuid::new_v4().to_string(),text:String::new(),attachments:vec![Attachment{r#type:"localImage".into(),path:attachment.path.clone()}],model:None,effort:None};
            let result=execute_context(&control.0,&mut rpc,"original",request,&AtomicBool::new(false),Some(&context));
            assert_eq!(result.is_ok(),acknowledge);assert_eq!(control.0.unknown.load(Ordering::Acquire),!acknowledge);assert_eq!(server.join().unwrap(),1);
            assert!(context.store.cleanup_staged(&context.session_id,&attachment.path).is_err());context.store.cleanup_session(&context.session_id).unwrap();assert!(Path::new(&attachment.path).is_file());
            drop(rpc);remove_staged_send_fixture(context,attachment,root);
        }
    }
}
