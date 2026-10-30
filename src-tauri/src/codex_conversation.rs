//! 기존 서버·대화의 구조화 연결. 입력과 응답은 한 작업자가 소유하며 재전송하지 않는다.
use super::*;
use std::{collections::{HashMap, HashSet, VecDeque}, path::PathBuf, sync::Weak};

const EVENT_COUNT: usize = 128;
const EVENT_BYTES: usize = 1024 * 1024;
const REQUEST_COUNT: usize = 32;
const TEXT_PREVIEW: usize = 4096;
const DETAIL_BYTES: usize = 16 * 1024;

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum ConversationRequest {
    Attach,
    Detach,
    Poll,
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
    detail: Mutex<Option<DetailSnapshot>>,
}
struct DetailSnapshot { id: String, turn_id: String, item_id: String, text: Arc<str> }
struct Worker {
    sender: mpsc::SyncSender<Work>,
    cancelled: Arc<AtomicBool>,
    transport: Arc<Mutex<Transport>>,
    join: Option<JoinHandle<()>>,
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
struct Work { request: ConversationRequest, reply: mpsc::SyncSender<Result<Value, String>>, deadline: Instant }
#[derive(Default)]
struct Events {
    queue: VecDeque<(Value, usize)>, bytes: usize, seq: u64, gap: bool,
    pending: HashMap<String, Value>, connected: bool, error: Option<String>,
    answered: HashSet<String>,
    observed_pending: HashMap<String,Value>, observer_connected: bool,
}
impl Events {
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
    pub(super) fn new(endpoint: Endpoint, stopped: Arc<AtomicBool>) -> Self {
        Self(Arc::new(Control {endpoint,stopped,input:Mutex::new(false),owned:AtomicBool::new(false),worker:Mutex::new(None),events:Mutex::new(Events::default()),sent:Mutex::new(HashSet::new()),unknown:AtomicBool::new(false),detail:Mutex::new(None)}))
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
        for request in requests.values_mut() {request["metadataAvailable"]=json!(events.connected || events.observer_connected);}
        Ok((owned,events.connected,requests.into_values().collect()))
    }
    pub(super) fn observer_message(&self,thread_id: &str,message: Value) -> Result<(),String> {record_message(&self.0,thread_id,message,true)}
    pub(super) fn observer_call<S: Read+Write>(&self,rpc: &mut Rpc<S>,thread_id: &str,method: &str,params: Value) -> Result<Value,String> {
        call_observed(&self.0,rpc,thread_id,method,params,self.0.stopped.as_ref(),true)
    }
    pub(super) fn observer_subscribe<S: Read+Write>(&self,rpc: &mut Rpc<S>,thread_id: &str,cwd: &Path) -> Result<(),String> {
        let result = self.observer_call(rpc,thread_id,"thread/resume",json!({"threadId":thread_id,"excludeTurns":true}))?;
        verified_thread(&result["thread"],thread_id,cwd)?;
        if result["thread"]["turns"].as_array().is_some_and(|turns|!turns.is_empty()) {return Err("관측 구독이 전체 이력을 반환해 읽기 구독을 제한했습니다.".into());}
        self.0.events.lock().map_err(|_| "질문 관측 상태 오류")?.observer_connected=true;
        Ok(())
    }
    pub(super) fn observer_disconnected(&self) {self.0.events.lock().unwrap_or_else(|e|e.into_inner()).observer_connected=false;}
    pub(crate) fn cancel(&self) {
        let mut slot = self.0.worker.lock().unwrap_or_else(|e| e.into_inner());
        let worker = slot.take();
        if let Some(mut worker) = worker {
            worker.cancelled.store(true, Ordering::Release);
            let transport = worker.transport.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(socket) = &transport.socket { let _ = socket.shutdown(Shutdown::Both); }
            if let Some(job) = transport.proxy.as_ref().and_then(Weak::upgrade) { let _ = job.terminate(); }
            drop(transport);
            drop(worker.sender);
            if let Some(join) = worker.join.take() { let _ = join.join(); }
        }
        *self.0.input.lock().unwrap_or_else(|e| e.into_inner()) = false;
        self.0.owned.store(false,Ordering::Release);
        self.0.detail.lock().unwrap_or_else(|e|e.into_inner()).take();
    }
    pub(crate) fn request(&self, thread_id: &str, cwd: &Path, request: ConversationRequest) -> Result<Value, String> {
        validate_request(&request)?;
        if matches!(request, ConversationRequest::Poll) {
            let mut poll = self.0.events.lock().map_err(|_| "대화 이벤트 상태를 확인할 수 없습니다.")?.poll();
            poll["unknownResult"] = json!(self.0.unknown.load(Ordering::Acquire)); return Ok(poll);
        }
        // 해제도 같은 단일 작업자의 요청으로 직렬화한다. 승인/결과 불명 상태에서는
        // 연결과 입력 소유권을 보존해 터미널의 두 번째 응답을 막는다.
        if matches!(request, ConversationRequest::Detach) {
            let (reply, receiver) = mpsc::sync_channel(1);
            {
                let worker = self.0.worker.lock().map_err(|_| "대화 연결 상태 오류")?;
                if let Some(worker) = worker.as_ref() {
                    worker.sender.try_send(Work {request,reply,deadline:Instant::now()+RPC_TIMEOUT}).map_err(|_| "대화 해제 요청을 전달할 수 없습니다. 입력 소유권을 유지했습니다.")?;
                } else { return Ok(json!({})); }
            }
            let result = receiver.recv_timeout(RPC_TIMEOUT+Duration::from_secs(2)).map_err(|_| "대화 해제 결과를 확인할 수 없어 입력 소유권을 유지했습니다.")??;
            self.cancel(); return Ok(result);
        }
        if self.0.stopped.load(Ordering::Acquire) { return Err("대화 세션이 종료되었습니다.".into()); }
        let mut worker = self.0.worker.lock().map_err(|_| "대화 연결 상태를 확인할 수 없습니다.")?;
        if matches!(request, ConversationRequest::Attach) {
            if worker.is_some() { return Err("이미 대화 화면이 연결되어 있습니다.".into()); }
            {
                let mut events = self.0.events.lock().map_err(|_| "대화 이벤트 상태 오류")?;
                if !events.pending.is_empty() || self.0.unknown.load(Ordering::Acquire) {return Err("이전 연결의 미확인 요청을 보존하고 있어 새 입력 소유권을 만들지 않았습니다.".into());}
                events.queue.clear(); events.bytes=0; events.gap=false; events.answered.clear(); events.error=None;
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
                events.connected = false;
                if let Err(error) = result { events.error = Some(error); }
            }) { Ok(join) => join, Err(error) => { *self.0.input.lock().unwrap_or_else(|e| e.into_inner()) = false; self.0.owned.store(false,Ordering::Release); return Err(format!("대화 연결 작업자를 시작할 수 없습니다: {error}")); } };
            *worker = Some(Worker {sender,cancelled,transport,join:Some(join)});
        }
        if let ConversationRequest::Send {request_id,..} = &request {
            Uuid::parse_str(request_id).map_err(|_| "전송 식별자는 UUID여야 합니다.")?;
            let mut sent = self.0.sent.lock().map_err(|_| "대화 전송 기록을 확인할 수 없습니다.")?;
            if sent.contains(request_id) { return Err("이미 처리한 전송 식별자입니다. 결과가 불명확한 요청을 재전송하지 않았습니다.".into()); }
            // 삭제한 식별자의 재전송을 허용하지 않으므로 상한에 도달하면 명시적으로 거부한다.
            if sent.len() >= 1024 { return Err("이 세션의 전송 식별자 상한에 도달했습니다. 세션을 다시 여세요.".into()); }
            sent.insert(request_id.clone());
        }
        let (reply, receiver) = mpsc::sync_channel(1);
        worker.as_ref().ok_or("대화 화면을 먼저 연결하세요.")?.sender.try_send(Work {request,reply,deadline:Instant::now() + RPC_TIMEOUT})
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
    if control.stopped.load(Ordering::Acquire) || cancelled.load(Ordering::Acquire) { Err("대화 연결이 종료되었습니다. 진행 중 요청 결과는 확인되지 않았습니다.".into()) } else { Ok(()) }
}
fn call<S: Read + Write>(control: &Control, rpc: &mut Rpc<S>, thread_id: &str, method: &str, params: Value, cancelled: &AtomicBool) -> Result<Value, String> {
    call_observed(control,rpc,thread_id,method,params,cancelled,false)
}
fn call_observed<S: Read+Write>(control: &Control,rpc: &mut Rpc<S>,thread_id: &str,method: &str,params: Value,cancelled: &AtomicBool,metadata_only: bool) -> Result<Value,String> {
    live(control, cancelled)?;
    let mutation = matches!(method,"turn/start" | "turn/interrupt");
    let previous_unknown = if mutation {control.unknown.swap(true,Ordering::AcqRel)} else {false};
    rpc.next_id += 1; let id = rpc.next_id;
    rpc.send(json!({"id":id,"method":method,"params":params}))?;
    let deadline = rpc.deadline.unwrap_or_else(|| Instant::now()+RPC_TIMEOUT).min(Instant::now()+RPC_TIMEOUT);
    while Instant::now() < deadline {
        live(control, cancelled)?;
        if let Some(message) = rpc.read()? {
            if let Some(result) = response(&message, id) {
                if mutation && (message.get("result").is_some() || message.get("error").is_some()) { control.unknown.store(previous_unknown,Ordering::Release); }
                return result;
            }
            record_message(control, thread_id, message, metadata_only)?;
        }
    }
    Err(format!("대화 {method} 응답 시간이 초과되었습니다. 요청 결과는 불명확하며 재전송하지 않았습니다."))
}
fn serve<S: Read + Write>(control: &Control, rpc: &mut Rpc<S>, thread_id: &str, cwd: &Path, receiver: mpsc::Receiver<Work>, cancelled: &AtomicBool, deadline: &Mutex<Option<Instant>>) -> Result<(), String> {
    rpc.deadline = *deadline.lock().map_err(|_| "대화 요청 기한 상태 오류")?;
    let mut init = initialize(); init["capabilities"] = json!({"experimentalApi":true});
    call(control, rpc, thread_id, "initialize", init, cancelled)?;
    rpc.send(json!({"method":"initialized","params":{}}))?;
    // 이 경로는 기존 식별자만 재개하며 경로·이력·모델·권한 재정의를 전달하지 않는다.
    let metadata = call(control, rpc, thread_id, "thread/read", json!({"threadId":thread_id,"includeTurns":false}), cancelled)?;
    verified_thread(&metadata["thread"], thread_id, cwd)?;
    let mut resumed = call(control, rpc, thread_id, "thread/resume", json!({"threadId":thread_id,"excludeTurns":true}), cancelled)?;
    verified_thread(&resumed["thread"], thread_id, cwd)?;
    if resumed["thread"]["turns"].as_array().is_some_and(|v| !v.is_empty()) { return Err("현재 CLI가 이력 제외 재개를 지원하지 않아 전체 이력 로드를 거부했습니다.".into()); }
    resumed["threadId"] = json!(thread_id); resumed["owner"] = json!("gui");
    let current = call(control,rpc,thread_id,"thread/turns/list",json!({"threadId":thread_id,"limit":1,"sortDirection":"desc","itemsView":"summary"}),cancelled)?;
    let current = current["data"].as_array().filter(|values| values.len() <= 1).ok_or("현재 대화 작업 목록이 요청 상한을 넘었습니다.")?;
    resumed["activeTurnId"] = current.first().filter(|turn| turn["status"] == "inProgress").and_then(|turn| turn["id"].as_str()).map_or(Value::Null,|id|json!(id));
    { let mut events = control.events.lock().map_err(|_| "대화 이벤트 상태 오류")?; events.connected = true; events.error = None; }
    rpc.deadline = None; *deadline.lock().map_err(|_| "대화 요청 기한 상태 오류")? = None;
    loop {
        live(control, cancelled)?;
        match receiver.try_recv() {
            Ok(work) => {
                if Instant::now() >= work.deadline { let _ = work.reply.send(Err("대기 중 대화 요청의 기한이 지나 전송하지 않았습니다.".into())); continue; }
                if matches!(work.request,ConversationRequest::Detach) {
                    let pending = !control.events.lock().map_err(|_| "대화 승인 상태 오류")?.pending.is_empty();
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
                        let result = execute(control, rpc, thread_id, request, cancelled);
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
fn execute<S: Read + Write>(control: &Control, rpc: &mut Rpc<S>, thread_id: &str, request: ConversationRequest, cancelled: &AtomicBool) -> Result<Value, String> {
    match request {
        ConversationRequest::Page {cursor: page_cursor} => { cursor(&page_cursor)?; bounded_page(call(control,rpc,thread_id,"thread/turns/list",json!({"threadId":thread_id,"cursor":page_cursor,"limit":20,"sortDirection":"desc","itemsView":"summary"}),cancelled)?,20) },
        ConversationRequest::Items {turn_id,cursor: page_cursor} => { valid_id(&turn_id)?; cursor(&page_cursor)?; bounded_page(call(control,rpc,thread_id,"thread/items/list",json!({"threadId":thread_id,"turnId":turn_id,"cursor":page_cursor,"limit":20,"sortDirection":"desc"}),cancelled)?,20) },
        ConversationRequest::Detail {turn_id,item_id,offset,snapshot_id} => {
            valid_id(&turn_id)?; valid_id(&item_id)?;
            let offset=offset.unwrap_or(0);
            if offset==0 {
                let result = call(control,rpc,thread_id,"thread/items/read",json!({"threadId":thread_id,"turnId":turn_id,"itemIds":[item_id]}),cancelled)?;
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
        ConversationRequest::Models {cursor: page_cursor} => { cursor(&page_cursor)?; bounded_page(call(control,rpc,thread_id,"model/list",json!({"cursor":page_cursor,"limit":50,"includeHidden":false}),cancelled)?,50) },
        ConversationRequest::Send {request_id,text,attachments,model,effort} => {
            if control.unknown.load(Ordering::Acquire) { return Err("이전 전송 결과가 불명확하여 새 전송을 차단했습니다. 원래 대화 상태를 먼저 확인하세요.".into()); }
            let input = user_input(&text, &attachments)?;
            if model.as_ref().is_some_and(|v| v.is_empty() || v.len() > 256) || effort.as_ref().is_some_and(|v| v.len() > 64) { return Err("모델 또는 추론 설정이 올바르지 않습니다.".into()); }
            // 서버는 활성 turn/start를 steer로 취급한다. GUI는 새 메시지만 보내므로 실행 중 전송을 명시 거부한다.
            let state = call(control,rpc,thread_id,"thread/read",json!({"threadId":thread_id,"includeTurns":false}),cancelled)?;
            if state["thread"]["id"] != thread_id || state["thread"]["status"]["type"] != "idle" { return Err("대화가 대기 상태가 아니어서 새 메시지를 보내지 않았습니다.".into()); }
            let mut params = json!({"threadId":thread_id,"input":input,"clientUserMessageId":request_id});
            if let Some(model) = model { params["model"] = json!(model); }
            if let Some(effort) = effort { params["effort"] = json!(effort); }
            call(control,rpc,thread_id,"turn/start",params,cancelled)
        },
        ConversationRequest::Interrupt {turn_id} => { valid_id(&turn_id)?; call(control,rpc,thread_id,"turn/interrupt",json!({"threadId":thread_id,"turnId":turn_id}),cancelled) },
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
fn observe_message(control: &Control, thread_id: &str, message: Value) -> Result<(), String> {
    record_message(control,thread_id,message,false)
}
fn record_message(control: &Control, thread_id: &str, message: Value, metadata_only: bool) -> Result<(), String> {
    let Some(method) = message["method"].as_str() else { return Ok(()); };
    if metadata_only && message.get("id").is_none() && method!="serverRequest/resolved" {return Ok(());}
    let params = &message["params"];
    let selected = params["threadId"].as_str().or_else(|| params["thread"]["id"].as_str());
    if selected != Some(thread_id) { return Ok(()); }
    let mut events = control.events.lock().map_err(|_| "대화 이벤트 상태 오류")?;
    if method == "serverRequest/resolved" {
        if let Some(id) = params.get("requestId") { events.pending.remove(&id.to_string()); events.observed_pending.remove(&id.to_string()); }
    }
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
        if events.pending.contains_key(&key) {return Ok(());}
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
            if result.as_object().is_some_and(|v| v.len() == 1) && matches!(result["decision"].as_str(), Some("accept" | "acceptForSession" | "decline" | "cancel")) { Ok(()) }
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
    fn lost_mutation_reply_blocks_later_send_and_never_replays_the_message() {
        let (control,mut rpc,listener) = fixture();
        let server = thread::spawn(move || {
            let (stream,_) = listener.accept().unwrap();
            let mut socket = WebSocket::from_raw_socket(stream,tungstenite::protocol::Role::Server,None);
            let read = read_request(&mut socket); assert_eq!(read["method"],"thread/read");
            reply(&mut socket,&read,json!({"thread":{"id":"original","status":{"type":"idle"}}}));
            let send = read_request(&mut socket); assert_eq!(send["method"],"turn/start");
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
        let (_peer,_) = listener.accept().unwrap();
        let transport = Arc::new(Mutex::new(Transport{socket:Some(stream.try_clone().unwrap()),proxy:None}));
        let cancelled = Arc::new(AtomicBool::new(false));
        let start = Instant::now();
        let watch = DeadlineWatch::start(Arc::new(Mutex::new(Some(start+Duration::from_millis(25)))),transport,cancelled.clone(),Arc::new(AtomicBool::new(false))).unwrap();
        let result = stream.read(&mut [0u8;1]);
        assert!(matches!(result,Ok(0)) || result.is_err());
        assert!(cancelled.load(Ordering::Acquire)); assert!(start.elapsed()<Duration::from_secs(1));
        drop(watch);
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
}
