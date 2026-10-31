//! 제안된 하위 에이전트 관리 통로. 초기화 응답의 명시 지원 없이는 관리 요청을 보내지 않는다.
use super::*;

const LIST: &str = "thread/subagents/list";
const CONTROL: &str = "thread/subagents/control";
const RECEIPTS: usize = 1024;

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "camelCase", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum SubagentRequest {
    List { cursor: Option<String> },
    Send { agent_id: String, request_id: String, text: String, expected_parent_runtime_id: String, #[serde(deserialize_with="nullable_runtime")] expected_runtime_id: Option<String> },
    Interrupt { agent_id: String, request_id: String, expected_turn_id: String, expected_parent_runtime_id: String, #[serde(deserialize_with="nullable_runtime")] expected_runtime_id: Option<String> },
    Close { agent_id: String, request_id: String, expected_parent_runtime_id: String, #[serde(deserialize_with="nullable_runtime")] expected_runtime_id: Option<String> },
    Resume { agent_id: String, request_id: String, expected_parent_runtime_id: String, #[serde(deserialize_with="nullable_runtime")] expected_runtime_id: Option<String> },
}
impl SubagentRequest {
    pub(crate) fn receipt_identity(&self) -> Result<Option<(String,[u8;32])>,String> {
        self.mutation().map(|(_,id,_)| {uuid(id)?;digest(self).map(|digest|(id.to_owned(),digest))}).transpose()
    }
    pub(crate) fn validate(&self) -> Result<(), String> {
        match self {
            Self::List { cursor } => return valid_cursor(cursor),
            Self::Send { text, .. } if text.trim().is_empty() || text.len() > 64 * 1024 || text.contains('\0') =>
                return Err("하위 에이전트 입력은 비어 있지 않은 64KiB 이하 텍스트여야 합니다.".into()),
            Self::Interrupt { expected_turn_id, .. } => turn_id(expected_turn_id)?,
            _ => {},
        }
        let (agent_id, request_id, _) = self.mutation().unwrap();
        uuid(agent_id)?; uuid(request_id)?;
        let (parent_runtime,runtime)=self.expected_runtime().unwrap();
        uuid(parent_runtime)?;if let Some(runtime)=runtime {uuid(runtime)?;}Ok(())
    }
    fn mutation(&self) -> Option<(&str, &str, &'static str)> {
        match self {
            Self::List { .. } => None,
            Self::Send { agent_id, request_id, .. } => Some((agent_id,request_id,"send")),
            Self::Interrupt { agent_id, request_id, .. } => Some((agent_id,request_id,"interrupt")),
            Self::Close { agent_id, request_id, .. } => Some((agent_id,request_id,"close")),
            Self::Resume { agent_id, request_id, .. } => Some((agent_id,request_id,"resume")),
        }
    }
    fn expected_runtime(&self) -> Option<(&str,Option<&str>)> {
        match self {
            Self::List {..}=>None,
            Self::Send {expected_parent_runtime_id,expected_runtime_id,..}|Self::Interrupt {expected_parent_runtime_id,expected_runtime_id,..}|Self::Close {expected_parent_runtime_id,expected_runtime_id,..}|Self::Resume {expected_parent_runtime_id,expected_runtime_id,..}=>Some((expected_parent_runtime_id,expected_runtime_id.as_deref())),
        }
    }
    fn unsupported(&self) -> Value {
        if self.mutation().is_none() {
            json!({"supported":false,"connected":false,"parentRuntimeId":null,"agents":[],"nextCursor":null,"mutationState":"clear","reconciledRequestId":null,"reason":"unsupported"})
        } else { json!({"status":"unsupported","reason":"unsupported"}) }
    }
}
fn nullable_runtime<'de,D: serde::Deserializer<'de>>(deserializer: D) -> Result<Option<String>,D::Error> {
    Option::<String>::deserialize(deserializer)
}
fn uuid(id: &str) -> Result<(),String> {
    if id.len() != 36 || !Uuid::parse_str(id).is_ok_and(|uuid|uuid.to_string()==id) {
        Err("대화·하위 에이전트·요청·실행 세대 식별자는 표준 소문자 UUID여야 합니다.".into())
    } else { Ok(()) }
}
fn turn_id(id: &str) -> Result<(),String> {
    if id.is_empty() || id.len()>1024 || id.contains('\0') {
        Err("하위 에이전트 작업 식별자는 비어 있지 않은 1024바이트 이하 문자열이어야 합니다.".into())
    } else { Ok(()) }
}
fn valid_cursor(cursor: &Option<String>) -> Result<(),String> {
    if cursor.as_ref().is_some_and(|value| value.len()>8192 || value.contains('\0')) {
        Err("하위 에이전트 페이지 위치가 허용 범위를 벗어났습니다.".into())
    } else { Ok(()) }
}

#[derive(Default)]
pub(super) struct State {
    parent: Option<String>,
    parent_runtime: Option<String>,
    agents: HashMap<String, Agent>,
    receipts: HashMap<String, Receipt>,
    unresolved: Option<String>,
    unresolved_runtime: Option<String>,
    remote_unknown: bool,
    in_flight: Option<InFlight>,
}
enum InFlight { List, Mutation { request_id: String, digest: [u8;32] } }
#[derive(Clone)]
struct Receipt { digest: [u8;32], status: &'static str }
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Capabilities { send: bool, interrupt: bool, close: bool, resume: bool }
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Agent {
    id: String, parent_thread_id: String,
    #[serde(deserialize_with="nullable_runtime")] runtime_id: Option<String>,
    nickname: Option<String>, role: Option<String>, status: Status,
    model: Option<String>, effort: Option<String>, active_turn_id: Option<String>,
    capabilities: Capabilities,
}
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
enum Status { Pending, Running, Idle, Completed, Errored, Closed, Unknown }
#[derive(Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
enum MutationState { Clear, Unknown }
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Page {
    parent_thread_id: String, parent_runtime_id: String, agents: Vec<Agent>, next_cursor: Option<String>,
    mutation_state: MutationState, reconciled_request_id: Option<String>,
}
impl State {
    fn bind(&mut self, thread_id: &str) -> Result<(),String> {
        if self.parent.as_deref().is_some_and(|parent|parent!=thread_id) {
            return Err("다른 부모 대화의 하위 에이전트 상태를 재사용하지 않았습니다.".into());
        }
        self.parent.get_or_insert_with(||thread_id.to_owned()); Ok(())
    }
    fn previous(&self, request: &SubagentRequest) -> Result<Option<Value>,String> {
        let Some((_, id, _)) = request.mutation() else {return Ok(None);};
        if let Some(receipt) = self.receipts.get(id) {
            if receipt.digest != digest(request)? {return Err("동일 요청 식별자에 다른 하위 에이전트 변경을 지정할 수 없습니다.".into());}
            return Ok(Some(json!({"status":receipt.status})));
        }
        if self.unresolved.is_some() || self.remote_unknown {return Err("결과가 확인되지 않은 하위 에이전트 변경이 남았습니다. 목록에서 같은 요청의 확인 결과를 먼저 조회하세요.".into());}
        if self.receipts.len()>=RECEIPTS {return Err("하위 에이전트 변경 영수증 상한에 도달해 새 변경을 보내지 않았습니다.".into());}
        Ok(None)
    }
    fn page(&mut self, thread_id: &str, value: Value) -> Result<Value,String> {
        let page: Page = serde_json::from_value(value).map_err(|_|"하위 에이전트 목록의 필드·자료형이 올바르지 않습니다.")?;
        if page.parent_thread_id!=thread_id || page.agents.len()>100 {return Err("하위 에이전트 목록의 부모 또는 개수 상한이 올바르지 않습니다.".into());}
        uuid(&page.parent_runtime_id)?;
        valid_cursor(&page.next_cursor)?;
        if let Some(id)=&page.reconciled_request_id {uuid(id)?;}
        let mut ids=HashSet::new();
        for agent in &page.agents {
            uuid(&agent.id)?;
            if let Some(runtime)=&agent.runtime_id {uuid(runtime)?;}
            if matches!(agent.status,Status::Closed) && agent.runtime_id.is_some() {return Err("종료된 하위 에이전트의 실행 세대가 비어 있지 않습니다.".into());}
            if agent.parent_thread_id!=thread_id || agent.id==thread_id || !ids.insert(&agent.id) {
                return Err("하위 에이전트 목록에 다른 부모·자기 자신·중복 항목이 포함되었습니다.".into());
            }
            for (field,max) in [(&agent.nickname,256),(&agent.role,256),(&agent.model,256),(&agent.effort,64)] {
                if field.as_ref().is_some_and(|value|value.len()>max || value.contains('\0')) {return Err("하위 에이전트 정보가 허용 크기를 넘었습니다.".into());}
            }
            if let Some(id)=&agent.active_turn_id {turn_id(id)?;}
        }
        let matching=self.parent_runtime.as_deref()==Some(page.parent_runtime_id.as_str())
            && self.unresolved_runtime.as_deref()==Some(page.parent_runtime_id.as_str())
            && self.unresolved.as_deref().is_some_and(|id|page.reconciled_request_id.as_deref()==Some(id));
        if matching && page.mutation_state==MutationState::Clear {self.unresolved=None;self.unresolved_runtime=None;}
        self.remote_unknown=page.mutation_state==MutationState::Unknown;
        let mutation_state=if self.unresolved.is_some() {MutationState::Unknown} else {page.mutation_state};
        let result=json!({"supported":true,"connected":true,"parentRuntimeId":page.parent_runtime_id,"agents":page.agents,"nextCursor":page.next_cursor,
            "mutationState":mutation_state,"reconciledRequestId":page.reconciled_request_id});
        // 최근 검증한 한 페이지만 보존한다. 페이지 이동은 캐시를 무한히 늘리지 않는다.
        self.parent_runtime=Some(page.parent_runtime_id);
        self.agents=page.agents.into_iter().map(|agent|(agent.id.clone(),agent)).collect();
        Ok(result)
    }
}
fn digest(request: &SubagentRequest) -> Result<[u8;32],String> {
    let bytes=serde_json::to_vec(request).map_err(|_|"하위 에이전트 요청을 변환할 수 없습니다.")?;
    Ok(Sha256::digest(bytes).into())
}

// 공유 잠금은 인계·복원 때만 잡는다. 바깥 상태에는 진행 중 식별자와 기존
// 영수증만 남겨, 다른 새 요청의 확정 미송신과 같은 요청의 결과 불명을 구분한다.
struct Checkout<'a> { shared: &'a Mutex<State>, state: Option<State> }
impl Drop for Checkout<'_> {
    fn drop(&mut self) {
        if let Some(state)=self.state.take() {*self.shared.lock().unwrap_or_else(|error|error.into_inner())=state;}
    }
}
fn with_request(shared: &Mutex<State>,request: &SubagentRequest,execute: impl FnOnce(&mut State)->Result<Value,String>) -> Result<Value,String> {
    let mut slot=shared.lock().map_err(|_|"하위 에이전트 관리 상태 오류")?;
    if let Some(active)=&slot.in_flight {
        let Some((_,id,_))=request.mutation() else {return Err("하위 에이전트 요청이 이미 진행 중입니다.".into());};
        if let Some(receipt)=slot.receipts.get(id) {
            if receipt.digest!=digest(request)? {return Err("동일 요청 식별자에 다른 하위 에이전트 변경을 지정할 수 없습니다.".into());}
            return Ok(json!({"status":receipt.status}));
        }
        if let InFlight::Mutation {request_id,digest:active_digest}=active {
            if request_id==id {
                if *active_digest!=digest(request)? {return Err("진행 중인 요청 식별자에 다른 변경을 지정할 수 없습니다.".into());}
                return Ok(json!({"status":"unknown","reason":"같은 하위 에이전트 요청이 진행 중입니다."}));
            }
        }
        return Ok(json!({"status":"notApplied","reason":"다른 하위 에이전트 요청이 진행 중이어서 새 요청을 보내지 않았습니다."}));
    }
    let active=match request.mutation() {
        None=>InFlight::List,
        Some((_,id,_))=>InFlight::Mutation {request_id:id.into(),digest:digest(request)?},
    };
    let local=std::mem::take(&mut *slot);
    slot.receipts=local.receipts.clone();slot.in_flight=Some(active);
    drop(slot);
    let mut checkout=Checkout {shared,state:Some(local)};
    let state=checkout.state.as_mut().unwrap();
    let result=execute(state);
    match (result,request.mutation()) {
        (Err(reason),Some((_,id,_)))=>match state.receipts.get(id) {
            Some(receipt) if receipt.digest==digest(request)?=>Ok(json!({"status":receipt.status,"reason":reason})),
            Some(_)=>Err(reason),
            None=>Ok(json!({"status":"notApplied","reason":reason})),
        },
        (result,_)=>result,
    }
}

impl ConversationConnection {
    pub(crate) fn subagents_request(&self, thread_id: &str, _cwd: &Path, request: SubagentRequest) -> Result<Value,String> {
        with_request(&self.0.subagents,&request,|state| {
        request.validate()?; uuid(thread_id)?;
        state.bind(thread_id)?;
        if let Some(previous)=state.previous(&request)? {return Ok(previous);}
        live(&self.0,&AtomicBool::new(false))?;
        let deadline=Instant::now()+RPC_TIMEOUT;
        let cancelled=Arc::new(AtomicBool::new(false));
        let transport=Arc::new(Mutex::new(Transport::default()));
        let _watch=DeadlineWatch::start(Arc::new(Mutex::new(Some(deadline))),transport.clone(),cancelled.clone(),self.0.stopped.clone())?;
        match &self.0.endpoint {
            Endpoint::Shared(endpoint)=>{
                let mut proxy=endpoint.open_until(Some(cancelled.clone()),Some(deadline))?;
                transport.lock().map_err(|_|"하위 에이전트 연결 정리 상태 오류")?.proxy=proxy.job.as_ref().map(Arc::downgrade);
                let result=exchange(&self.0,state,proxy.rpc.as_mut().ok_or("하위 에이전트 연결을 찾을 수 없습니다.")?,thread_id,&request,&cancelled,deadline);
                match (result,proxy.finish()) { (Ok(result),Ok(()))=>Ok(result),(Err(error),Ok(()))=>Err(error),(result,Err(error))=>Err(format!("{}; 하위 에이전트 연결 정리 실패: {error}",result.err().unwrap_or_default())) }
            },
            Endpoint::Owned { address,token,job }=>{
                let socket=address.trim_start_matches("ws://").parse().map_err(|_|"하위 에이전트 서버 주소가 올바르지 않습니다.")?;
                let stream=TcpStream::connect_timeout(&socket,Duration::from_secs(2)).map_err(|error|format!("하위 에이전트 연결 실패: {error}"))?;
                job.upgrade().ok_or("기존 대화 서버가 종료되었습니다.")?.verify_tcp_peer(&stream).map_err(|_|"하위 에이전트 서버의 연결 소유권을 확인할 수 없습니다.")?;
                transport.lock().map_err(|_|"하위 에이전트 연결 정리 상태 오류")?.socket=Some(stream.try_clone().map_err(|error|error.to_string())?);
                let mut rpc=authenticated_socket(address,token,stream)?;
                let result=exchange(&self.0,state,&mut rpc,thread_id,&request,&cancelled,deadline);
                let _=rpc.ws.get_ref().shutdown(Shutdown::Both); result
            },
        }
        })
    }
}

fn exchange<S: Read+Write>(control: &Control,state: &mut State,rpc: &mut Rpc<S>,parent: &str,request: &SubagentRequest,cancelled: &Arc<AtomicBool>,deadline: Instant) -> Result<Value,String> {
    rpc.cancelled=Some(cancelled.clone());rpc.deadline=Some(deadline);
    live(control,cancelled)?;
    let initialized=rpc.call("initialize",initialize())?;
    rpc.send(json!({"method":"initialized","params":{}}))?;
    // 제안 계약의 기능 신호다. 기존 CLI의 다른 기능이나 버전으로 지원을 추측하지 않는다.
    match initialized.get("supportsSubagentManagement") {
        None|Some(Value::Bool(false))=>return Ok(request.unsupported()),
        Some(Value::Bool(true))=>{},
        Some(_)=>return Err("하위 에이전트 관리 지원 정보가 bool이 아닙니다.".into()),
    }
    if let SubagentRequest::List { cursor }=request {
        let page=rpc.call(LIST,json!({"parentThreadId":parent,"cursor":cursor,"limit":100,"reconcileRequestId":state.unresolved}))?;
        live(control,cancelled)?;
        return state.page(parent,page);
    }
    let (agent_id,request_id,action)=request.mutation().unwrap();
    let agent=state.agents.get(agent_id).ok_or("목록에서 직접 확인한 하위 에이전트만 변경할 수 있습니다. 먼저 목록을 조회하세요.")?;
    let (expected_parent_runtime,expected_runtime)=request.expected_runtime().unwrap();
    if state.parent_runtime.as_deref()!=Some(expected_parent_runtime) || agent.runtime_id.as_deref()!=expected_runtime {return Err("하위 에이전트 또는 부모 실행 세대가 마지막 목록과 다릅니다. 새 목록을 조회하세요.".into());}
    let permitted=match action {"send"=>agent.capabilities.send,"interrupt"=>agent.capabilities.interrupt,"close"=>agent.capabilities.close,"resume"=>agent.capabilities.resume,_=>false};
    if !permitted {return Err("해당 하위 에이전트가 요청한 관리 동작을 지원하지 않습니다.".into());}
    if let SubagentRequest::Interrupt { expected_turn_id,.. }=request {
        if agent.active_turn_id.as_deref()!=Some(expected_turn_id) {return Err("중단 대상 작업이 마지막으로 확인한 하위 에이전트 작업과 다릅니다.".into());}
    }
    let mut params=json!({"parentThreadId":parent,"agentId":agent_id,"action":action,"requestId":request_id,"expectedParentRuntimeId":expected_parent_runtime,"expectedRuntimeId":expected_runtime});
    match request {SubagentRequest::Send { text,.. }=>params["text"]=json!(text),SubagentRequest::Interrupt { expected_turn_id,.. }=>params["expectedTurnId"]=json!(expected_turn_id),_=>{}}
    let receipt=Receipt {digest:digest(request)?,status:"unknown"};
    let response=control_call(control,rpc,params,cancelled,deadline,|| {
        // 전송 함수의 마지막 사전 검사가 끝난 실제 송신 경계에서만 기록한다.
        state.receipts.insert(request_id.into(),receipt);state.unresolved=Some(request_id.into());state.unresolved_runtime=Some(expected_parent_runtime.into());
    })?;
    live(control,cancelled)?;
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Reply { status: ReplyStatus }
    #[derive(Deserialize)]
    #[serde(rename_all="camelCase")]
    enum ReplyStatus { Applied, NotApplied, Unknown, Unsupported }
    let response: Reply=serde_json::from_value(response).map_err(|_|"하위 에이전트 변경 응답 형식이 올바르지 않아 결과 불명을 보존했습니다.")?;
    let status=match response.status {ReplyStatus::Applied=>"applied",ReplyStatus::NotApplied=>"notApplied",ReplyStatus::Unknown=>"unknown",ReplyStatus::Unsupported=>"unsupported"};
    state.receipts.get_mut(request_id).unwrap().status=status;
    if status!="unknown" {state.unresolved=None;state.unresolved_runtime=None;}
    Ok(json!({"status":status}))
}

fn control_call<S: Read+Write>(control: &Control,rpc: &mut Rpc<S>,params: Value,cancelled: &AtomicBool,overall_deadline: Instant,before_send: impl FnOnce()) -> Result<Value,String> {
    let deadline=rpc.deadline.map_or(overall_deadline,|deadline|deadline.min(overall_deadline));
    rpc.next_id+=1;let id=rpc.next_id;
    let message=Message::Text(json!({"id":id,"method":CONTROL,"params":params}).to_string().into());
    live(control,cancelled)?;
    check_cancelled(rpc.cancelled.as_ref())?;
    if Instant::now()>=deadline {return Err("하위 에이전트 요청 기한이 지나 변경을 보내지 않았습니다.".into());}
    before_send();
    // 취소·기한 검사와 영수증 기록을 같은 전송 경계가 소유한다.
    // 이 뒤의 부분 송신·응답 유실은 결과 불명이며 재전송하지 않는다.
    rpc.ws.send(message)
        .map_err(|error|format!("하위 에이전트 변경 송신 결과를 확인할 수 없습니다: {error}"))?;
    while Instant::now()<deadline {
        check_cancelled(rpc.cancelled.as_ref())?;
        if let Some(message)=rpc.read()? {if let Some(result)=response(&message,id) {return result;}}
    }
    Err("하위 에이전트 변경 응답 기한이 지나 결과 불명을 보존했습니다.".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    const PARENT: &str="00000000-0000-0000-0000-000000000001";
    const CHILD: &str="00000000-0000-0000-0000-000000000002";
    const TURN: &str="turn-1";
    const PARENT_RUNTIME: &str="00000000-0000-0000-0000-000000000005";
    const RUNTIME: &str="00000000-0000-0000-0000-000000000006";
    const REQUEST: &str="00000000-0000-0000-0000-000000000004";
    fn connection() -> ConversationConnection {
        ConversationConnection::new(Endpoint::Owned {address:String::new(),token:String::new(),job:Weak::new()},Arc::new(AtomicBool::new(false)))
    }
    fn page() -> Value {
        json!({"parentThreadId":PARENT,"parentRuntimeId":PARENT_RUNTIME,"agents":[{"id":CHILD,"parentThreadId":PARENT,"runtimeId":RUNTIME,"nickname":"도우미","role":"검토","status":"idle",
            "model":"동적으로 발견한 모델","effort":"동적으로 발견한 수준","activeTurnId":TURN,"capabilities":{"send":true,"interrupt":true,"close":true,"resume":true}}],
            "nextCursor":null,"mutationState":"clear","reconciledRequestId":null})
    }
    fn send() -> SubagentRequest {SubagentRequest::Send {expected_parent_runtime_id:PARENT_RUNTIME.into(),expected_runtime_id:Some(RUNTIME.into()),agent_id:CHILD.into(),request_id:REQUEST.into(),text:"확인할 내용".into()}}
    fn raw_rpc() -> (Rpc<TcpStream>,WebSocket<TcpStream>) {
        let listener=TcpListener::bind("127.0.0.1:0").unwrap();
        let stream=TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server,_)=listener.accept().unwrap();
        stream.set_read_timeout(Some(Duration::from_millis(20))).unwrap();
        server.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        (Rpc {ws:WebSocket::from_raw_socket(stream,tungstenite::protocol::Role::Client,None),next_id:0,cancelled:None,deadline:None},
            WebSocket::from_raw_socket(server,tungstenite::protocol::Role::Server,None))
    }
    fn read(ws: &mut WebSocket<TcpStream>) -> Value {
        match ws.read().unwrap() {Message::Text(text)=>serde_json::from_str(&text).unwrap(),other=>panic!("구조화 요청이 필요합니다: {other:?}")}
    }
    fn reply(ws: &mut WebSocket<TcpStream>,id: &Value,result: Value) {
        ws.send(Message::Text(json!({"id":id,"result":result}).to_string().into())).unwrap();
    }
    fn initialize_peer(ws: &mut WebSocket<TcpStream>,capability: Value) {
        let request=read(ws);assert_eq!(request["method"],"initialize");reply(ws,&request["id"],capability);
        assert_eq!(read(ws)["method"],"initialized");
    }
    impl ConversationConnection {
        pub(crate) fn fixture_subagent_connection() -> Self {connection()}
        pub(crate) fn fixture_subagent_lost_reply(&self,parent: &str,request: SubagentRequest) -> Result<Value,String> {
            let (mut rpc,mut peer)=raw_rpc();
            let server=thread::spawn(move|| {initialize_peer(&mut peer,json!({"supportsSubagentManagement":true}));assert_eq!(read(&mut peer)["method"],CONTROL);});
            let result=with_request(&self.0.subagents,&request,|state| {
                request.validate()?;state.bind(parent)?;
                let (agent,_,_)=request.mutation().unwrap();let (parent_runtime,runtime)=request.expected_runtime().unwrap();
                let mut value=page();value["parentThreadId"]=json!(parent);value["parentRuntimeId"]=json!(parent_runtime);
                value["agents"][0]["id"]=json!(agent);value["agents"][0]["parentThreadId"]=json!(parent);value["agents"][0]["runtimeId"]=json!(runtime);
                state.page(parent,value)?;
                exchange(&self.0,state,&mut rpc,parent,&request,&Arc::new(AtomicBool::new(false)),Instant::now()+RPC_TIMEOUT)
            });
            drop(rpc);server.join().unwrap();result
        }
    }
    #[test]
    fn subagents_requests_validate_identity_bytes_and_strict_fields() {
        for value in [json!({"type":"list","cursor":null}),serde_json::to_value(send()).unwrap(),
            json!({"type":"interrupt","agentId":CHILD,"expectedParentRuntimeId":PARENT_RUNTIME,"expectedRuntimeId":RUNTIME,"requestId":REQUEST,"expectedTurnId":TURN}),
            json!({"type":"close","agentId":CHILD,"expectedParentRuntimeId":PARENT_RUNTIME,"expectedRuntimeId":RUNTIME,"requestId":REQUEST}),json!({"type":"resume","agentId":CHILD,"expectedParentRuntimeId":PARENT_RUNTIME,"expectedRuntimeId":RUNTIME,"requestId":REQUEST})] {
            serde_json::from_value::<SubagentRequest>(value).unwrap().validate().unwrap();
        }
        for value in [json!({"type":"list","threadId":PARENT}),json!({"type":"send","agentId":CHILD,"requestId":REQUEST,"text":"x","model":"숨은 변경"}),
            json!({"type":"interrupt","agentId":CHILD,"expectedParentRuntimeId":PARENT_RUNTIME,"expectedRuntimeId":RUNTIME,"requestId":REQUEST,"expectedTurnId":null})] {
            assert!(serde_json::from_value::<SubagentRequest>(value).is_err());
        }
        for text in ["".into()," \n ".into(),"x\0y".into(),"가".repeat(21846)] {
            assert!(SubagentRequest::Send {expected_parent_runtime_id:PARENT_RUNTIME.into(),expected_runtime_id:Some(RUNTIME.into()),agent_id:CHILD.into(),request_id:REQUEST.into(),text}.validate().is_err());
        }
        assert!(SubagentRequest::Send {expected_parent_runtime_id:PARENT_RUNTIME.into(),expected_runtime_id:Some(RUNTIME.into()),agent_id:CHILD.into(),request_id:REQUEST.into(),text:"x".repeat(65536)}.validate().is_ok());
        assert!(SubagentRequest::List {cursor:Some("x".repeat(8193))}.validate().is_err());
        assert!(SubagentRequest::List {cursor:Some("x\0y".into())}.validate().is_err());
        assert!(SubagentRequest::Close {expected_parent_runtime_id:PARENT_RUNTIME.into(),expected_runtime_id:Some(RUNTIME.into()),agent_id:"다른 대상".into(),request_id:REQUEST.into()}.validate().is_err());
        for invalid_turn in ["".into(),"x\0y".into(),"x".repeat(1025)] {
            assert!(SubagentRequest::Interrupt {agent_id:CHILD.into(),request_id:REQUEST.into(),expected_turn_id:invalid_turn,
                expected_parent_runtime_id:PARENT_RUNTIME.into(),expected_runtime_id:Some(RUNTIME.into())}.validate().is_err());
        }
    }
    #[test]
    fn subagents_pages_reject_scope_duplicates_unknown_status_and_capability_types() {
        for edit in [0,1,2,3,4,5,6] {
            let mut value=page();
            match edit {
                0=>value["parentThreadId"]=json!(CHILD),
                1=>value["agents"][0]["parentThreadId"]=json!(CHILD),
                2=>value["agents"]=json!([value["agents"][0],value["agents"][0]]),
                3=>value["agents"][0]["status"]=json!("알 수 없는 상태 이름"),
                4=>value["agents"][0]["capabilities"]["send"]=json!("true"),
                5=>value["agents"]=json!(vec![value["agents"][0].clone();101]),
                _=>value["agents"][0]["hiddenPrompt"]=json!("원문"),
            }
            assert!(State::default().page(PARENT,value).is_err());
        }
        let mut state=State::default();state.bind(PARENT).unwrap();state.page(PARENT,page()).unwrap();
        assert!(state.bind(CHILD).is_err());
        let mut empty=page();empty["agents"]=json!([]);state.page(PARENT,empty).unwrap();assert!(state.agents.is_empty());
    }
    #[test]
    fn subagents_unknown_live_runtime_is_visible_but_all_controls_stay_unsent() {
        for explicit_null in [false,true] {
            let mut value=page();value["agents"][0]["status"]=json!("unknown");
            value["agents"][0]["capabilities"]=json!({"send":false,"interrupt":false,"close":false,"resume":false});
            if explicit_null {value["agents"][0]["activeTurnId"]=Value::Null;} else {value["agents"][0].as_object_mut().unwrap().remove("activeTurnId");}
            for request in [send(),SubagentRequest::Interrupt {agent_id:CHILD.into(),request_id:REQUEST.into(),expected_turn_id:TURN.into(),expected_parent_runtime_id:PARENT_RUNTIME.into(),expected_runtime_id:Some(RUNTIME.into())},
                SubagentRequest::Close {agent_id:CHILD.into(),request_id:REQUEST.into(),expected_parent_runtime_id:PARENT_RUNTIME.into(),expected_runtime_id:Some(RUNTIME.into())},
                SubagentRequest::Resume {agent_id:CHILD.into(),request_id:REQUEST.into(),expected_parent_runtime_id:PARENT_RUNTIME.into(),expected_runtime_id:Some(RUNTIME.into())}] {
                let connection=connection();let (mut rpc,mut peer)=raw_rpc();
                let server=thread::spawn(move|| {initialize_peer(&mut peer,json!({"supportsSubagentManagement":true}));assert!(peer.read().is_err(),"보존된 불명 실행의 권한이 모두 false이면 관리 프레임을 보내면 안 됩니다.");});
                let result=with_request(&connection.0.subagents,&request,|state| {
                    let normalized=state.page(PARENT,value.clone())?;
                    assert_eq!(normalized["mutationState"],"clear","전역 불명 잠금과 무관하게 항목 권한을 확인합니다.");
                    assert_eq!(normalized["agents"][0]["status"],"unknown");assert_eq!(normalized["agents"][0]["runtimeId"],RUNTIME);
                    assert!(normalized["agents"][0]["activeTurnId"].is_null());
                    assert_eq!(normalized["agents"][0]["capabilities"],json!({"send":false,"interrupt":false,"close":false,"resume":false}));
                    exchange(&connection.0,state,&mut rpc,PARENT,&request,&Arc::new(AtomicBool::new(false)),Instant::now()+RPC_TIMEOUT)
                }).unwrap();
                assert_eq!(result["status"],"notApplied");assert!(result["reason"].as_str().unwrap().contains("지원하지 않습니다"));
                let state=connection.0.subagents.lock().unwrap();assert!(state.receipts.is_empty());assert!(state.unresolved.is_none());
                drop(state);drop(rpc);server.join().unwrap();assert!(!*connection.0.input.lock().unwrap());
            }
            value["agents"][0]["status"]=json!("closed");
            assert!(State::default().page(PARENT,value).is_err(),"closed와 살아 있는 runtimeId 조합은 계속 거절해야 합니다.");
        }
    }
    #[test]
    fn subagents_unsupported_sends_no_list_or_mutation_rpc() {
        for request in [SubagentRequest::List {cursor:None},send()] {
            let (mut rpc,mut peer)=raw_rpc();
            let server=thread::spawn(move|| {initialize_peer(&mut peer,json!({}));assert!(peer.read().is_err(),"미지원 서버에는 추가 요청이 없어야 합니다.");});
            let connection=connection();let mut state=State::default();
            let value=exchange(&connection.0,&mut state,&mut rpc,PARENT,&request,&Arc::new(AtomicBool::new(false)),Instant::now()+RPC_TIMEOUT).unwrap();
            assert_eq!(value,request.unsupported());assert!(state.receipts.is_empty());assert!(!*connection.0.input.lock().unwrap());
            drop(rpc);server.join().unwrap();
        }
    }
    #[test]
    fn subagents_actual_wire_preserves_parent_child_text_and_expected_turn() {
        for request in [send(),SubagentRequest::Interrupt {expected_parent_runtime_id:PARENT_RUNTIME.into(),expected_runtime_id:Some(RUNTIME.into()),agent_id:CHILD.into(),request_id:REQUEST.into(),expected_turn_id:TURN.into()},
            SubagentRequest::Close {expected_parent_runtime_id:PARENT_RUNTIME.into(),expected_runtime_id:Some(RUNTIME.into()),agent_id:CHILD.into(),request_id:REQUEST.into()},SubagentRequest::Resume {expected_parent_runtime_id:PARENT_RUNTIME.into(),expected_runtime_id:Some(RUNTIME.into()),agent_id:CHILD.into(),request_id:REQUEST.into()}] {
            let (mut rpc,mut peer)=raw_rpc();let expected=serde_json::to_value(&request).unwrap();
            let server=thread::spawn(move|| {
                initialize_peer(&mut peer,json!({"supportsSubagentManagement":true}));
                let wire=read(&mut peer);assert_eq!(wire["method"],CONTROL);let params=&wire["params"];
                assert_eq!(params["parentThreadId"],PARENT);assert_eq!(params["agentId"],CHILD);assert_eq!(params["requestId"],REQUEST);assert_eq!(params["action"],expected["type"]);assert_eq!(params["expectedParentRuntimeId"],PARENT_RUNTIME);assert_eq!(params["expectedRuntimeId"],RUNTIME);
                assert_eq!(params.get("text"),expected.get("text"));assert_eq!(params.get("expectedTurnId"),expected.get("expectedTurnId"));
                reply(&mut peer,&wire["id"],json!({"status":"applied"}));
            });
            let connection=connection();let mut state=State::default();state.page(PARENT,page()).unwrap();
            let result=exchange(&connection.0,&mut state,&mut rpc,PARENT,&request,&Arc::new(AtomicBool::new(false)),Instant::now()+RPC_TIMEOUT).unwrap();
            assert_eq!(result["status"],"applied");assert!(state.unresolved.is_none());assert_eq!(state.previous(&request).unwrap().unwrap()["status"],"applied");
            assert!(!*connection.0.input.lock().unwrap());server.join().unwrap();
        }
    }
    #[test]
    fn subagents_lost_reply_blocks_replay_and_only_matching_clear_receipt_reconciles() {
        let (mut rpc,mut peer)=raw_rpc();
        let server=thread::spawn(move|| {initialize_peer(&mut peer,json!({"supportsSubagentManagement":true}));assert_eq!(read(&mut peer)["method"],CONTROL);});
        let connection=connection();let mut state=State::default();state.page(PARENT,page()).unwrap();
        assert!(exchange(&connection.0,&mut state,&mut rpc,PARENT,&send(),&Arc::new(AtomicBool::new(false)),Instant::now()+RPC_TIMEOUT).is_err());server.join().unwrap();
        assert_eq!(state.previous(&send()).unwrap().unwrap()["status"],"unknown");
        let next=SubagentRequest::Close {expected_parent_runtime_id:PARENT_RUNTIME.into(),expected_runtime_id:Some(RUNTIME.into()),agent_id:CHILD.into(),request_id:Uuid::new_v4().to_string()};assert!(state.previous(&next).is_err());
        let different=SubagentRequest::Send {expected_parent_runtime_id:PARENT_RUNTIME.into(),expected_runtime_id:Some(RUNTIME.into()),agent_id:CHILD.into(),request_id:REQUEST.into(),text:"다른 내용".into()};assert!(state.previous(&different).is_err());
        assert_eq!(state.page(PARENT,page()).unwrap()["mutationState"],"unknown");
        let mut mismatch=page();mismatch["reconciledRequestId"]=json!(Uuid::new_v4().to_string());assert_eq!(state.page(PARENT,mismatch).unwrap()["mutationState"],"unknown");
        let mut matching=page();matching["reconciledRequestId"]=json!(REQUEST);matching["mutationState"]=json!("unknown");state.page(PARENT,matching.clone()).unwrap();assert!(state.previous(&next).is_err());
        let (mut rpc,mut peer)=raw_rpc();let server=thread::spawn(move|| {
            initialize_peer(&mut peer,json!({"supportsSubagentManagement":true}));let wire=read(&mut peer);assert_eq!(wire["method"],LIST);
            assert_eq!(wire["params"],json!({"parentThreadId":PARENT,"cursor":"페이지","limit":100,"reconcileRequestId":REQUEST}));
            matching["mutationState"]=json!("clear");reply(&mut peer,&wire["id"],matching);
        });
        let value=exchange(&connection.0,&mut state,&mut rpc,PARENT,&SubagentRequest::List {cursor:Some("페이지".into())},&Arc::new(AtomicBool::new(false)),Instant::now()+RPC_TIMEOUT).unwrap();server.join().unwrap();
        assert_eq!(value["mutationState"],"clear");assert!(state.previous(&next).unwrap().is_none());
        assert_eq!(state.previous(&send()).unwrap().unwrap()["status"],"unknown","목록 확인은 이전 불명 결과를 성공으로 바꾸지 않습니다.");
    }
    #[test]
    fn subagents_unknown_remote_state_and_receipt_limit_fail_closed() {
        let mut state=State::default();let mut value=page();value["mutationState"]=json!("unknown");state.page(PARENT,value).unwrap();assert!(state.previous(&send()).is_err());
        state.page(PARENT,page()).unwrap();assert!(state.previous(&send()).unwrap().is_none());
        for _ in 0..RECEIPTS {state.receipts.insert(Uuid::new_v4().to_string(),Receipt {digest:[0;32],status:"applied"});}
        assert!(state.previous(&send()).is_err());
    }
    #[test]
    fn subagents_reloaded_parent_cannot_clear_previous_runtime_unknown_receipt() {
        let mut state=State::default();state.page(PARENT,page()).unwrap();
        state.receipts.insert(REQUEST.into(),Receipt {digest:digest(&send()).unwrap(),status:"unknown"});state.unresolved=Some(REQUEST.into());state.unresolved_runtime=Some(PARENT_RUNTIME.into());
        let mut reloaded=page();reloaded["parentRuntimeId"]=json!(Uuid::new_v4().to_string());reloaded["reconciledRequestId"]=json!(REQUEST);
        assert_eq!(state.page(PARENT,reloaded.clone()).unwrap()["mutationState"],"unknown");
        assert_eq!(state.page(PARENT,reloaded).unwrap()["mutationState"],"unknown","같은 새 세대 목록을 반복해도 이전 세대 영수증을 해제하지 않습니다.");
        assert_eq!(state.unresolved.as_deref(),Some(REQUEST));
    }
    #[test]
    fn subagents_runtime_fields_are_required_and_generation_changes_block_controls() {
        let mut request=serde_json::to_value(send()).unwrap();request.as_object_mut().unwrap().remove("expectedRuntimeId");
        assert!(serde_json::from_value::<SubagentRequest>(request).is_err());
        let mut value=page();value["agents"][0].as_object_mut().unwrap().remove("runtimeId");assert!(State::default().page(PARENT,value).is_err());
        let mut value=page();value["agents"][0]["runtimeId"]=Value::Null;value["agents"][0]["status"]=json!("closed");assert!(State::default().page(PARENT,value).is_ok());
        for parent_changed in [false,true] {
            let (mut rpc,mut peer)=raw_rpc();let server=thread::spawn(move|| {initialize_peer(&mut peer,json!({"supportsSubagentManagement":true}));assert!(peer.read().is_err());});
            let connection=connection();let mut state=State::default();let mut value=page();
            if parent_changed {value["parentRuntimeId"]=json!(Uuid::new_v4().to_string());} else {value["agents"][0]["runtimeId"]=json!(Uuid::new_v4().to_string());}
            state.page(PARENT,value).unwrap();
            assert!(exchange(&connection.0,&mut state,&mut rpc,PARENT,&send(),&Arc::new(AtomicBool::new(false)),Instant::now()+RPC_TIMEOUT).is_err());
            assert!(state.receipts.is_empty());drop(rpc);server.join().unwrap();
        }
    }
    #[test]
    fn subagents_concurrent_or_stopped_requests_fail_before_opening_a_transport() {
        let connection=connection();
        with_request(&connection.0.subagents,&SubagentRequest::List {cursor:None},|_| {
            assert!(connection.subagents_request(PARENT,Path::new("."),SubagentRequest::List {cursor:None}).unwrap_err().contains("이미 진행"));
            Ok(json!({}))
        }).unwrap();
        connection.0.stopped.store(true,Ordering::Release);
        assert!(connection.subagents_request(PARENT,Path::new("."),SubagentRequest::List {cursor:None}).unwrap_err().contains("종료"));
        assert_eq!(connection.subagents_request(PARENT,Path::new("."),send()).unwrap()["status"],"notApplied");
        assert!(!*connection.0.input.lock().unwrap());assert!(connection.0.worker.lock().unwrap().is_none());
    }
    #[test]
    fn subagents_pretransmission_initialize_failure_returns_not_applied_and_allows_recovery() {
        let connection=connection();let (mut rpc,mut peer)=raw_rpc();
        let server=thread::spawn(move|| {
            let initialize=read(&mut peer);assert_eq!(initialize["method"],"initialize");
            peer.send(Message::Text(json!({"id":initialize["id"],"error":{"code":-1,"message":"초기화 실패"}}).to_string().into())).unwrap();
            assert!(peer.read().is_err(),"초기화 실패 뒤 관리 요청을 보내면 안 됩니다.");
        });
        let result=with_request(&connection.0.subagents,&send(),|state|
            exchange(&connection.0,state,&mut rpc,PARENT,&send(),&Arc::new(AtomicBool::new(false)),Instant::now()+RPC_TIMEOUT)).unwrap();
        assert_eq!(result["status"],"notApplied");drop(rpc);server.join().unwrap();
        let (mut rpc,mut peer)=raw_rpc();let server=thread::spawn(move|| {
            initialize_peer(&mut peer,json!({"supportsSubagentManagement":true}));let wire=read(&mut peer);assert_eq!(wire["method"],CONTROL);reply(&mut peer,&wire["id"],json!({"status":"applied"}));
        });
        let result=with_request(&connection.0.subagents,&send(),|state| {
            assert!(state.receipts.is_empty());assert!(state.unresolved.is_none());assert!(state.in_flight.is_none());
            state.page(PARENT,page())?;assert!(state.previous(&send())?.is_none());
            exchange(&connection.0,state,&mut rpc,PARENT,&send(),&Arc::new(AtomicBool::new(false)),Instant::now()+RPC_TIMEOUT)
        }).unwrap();
        assert_eq!(result["status"],"applied");server.join().unwrap();
    }
    #[test]
    fn subagents_stale_runtime_is_not_applied_but_lost_control_reply_remains_unknown() {
        for transmitted in [false,true] {
            let connection=connection();let (mut rpc,mut peer)=raw_rpc();
            let server=thread::spawn(move|| {
                initialize_peer(&mut peer,json!({"supportsSubagentManagement":true}));
                if transmitted {assert_eq!(read(&mut peer)["method"],CONTROL);} else {assert!(peer.read().is_err(),"실행 세대 사전 오류 뒤 관리 요청을 보내면 안 됩니다.");}
            });
            let result=with_request(&connection.0.subagents,&send(),|state| {
                let mut value=page();if !transmitted {value["parentRuntimeId"]=json!(Uuid::new_v4().to_string());}state.page(PARENT,value)?;
                exchange(&connection.0,state,&mut rpc,PARENT,&send(),&Arc::new(AtomicBool::new(false)),Instant::now()+RPC_TIMEOUT)
            }).unwrap();
            assert_eq!(result["status"],if transmitted {"unknown"} else {"notApplied"});drop(rpc);server.join().unwrap();
            let state=connection.0.subagents.lock().unwrap();
            if transmitted {assert_eq!(state.unresolved.as_deref(),Some(REQUEST));assert_eq!(state.previous(&send()).unwrap().unwrap()["status"],"unknown");}
            else {assert!(state.unresolved.is_none());assert!(state.receipts.is_empty());}
            drop(state);
            if transmitted {
                connection.0.stopped.store(true,Ordering::Release);
                assert_eq!(connection.subagents_request(PARENT,Path::new("."),send()).unwrap()["status"],"unknown","연결 종료는 기존 송신 결과 불명을 미적용으로 바꾸면 안 됩니다.");
            }
        }
    }
    #[test]
    fn subagents_open_failure_is_confirmed_not_applied_without_a_receipt() {
        let connection=connection();
        assert_eq!(connection.subagents_request(PARENT,Path::new("."),send()).unwrap()["status"],"notApplied");
        let state=connection.0.subagents.lock().unwrap();assert!(state.receipts.is_empty());assert!(state.unresolved.is_none());assert!(state.in_flight.is_none());
    }
    #[test]
    fn subagents_control_entry_cancellation_stop_or_deadline_writes_no_frame_or_receipt() {
        for boundary in 0..3 {
            let connection=connection();let (mut rpc,mut peer)=raw_rpc();
            let server=thread::spawn(move||assert!(peer.read().is_err(),"이미 취소·종료·만료된 관리 요청은 프레임을 보내면 안 됩니다."));
            let cancelled=Arc::new(AtomicBool::new(boundary==0));rpc.cancelled=Some(cancelled.clone());
            if boundary==1 {connection.0.stopped.store(true,Ordering::Release);}
            rpc.deadline=Some(if boundary==2 {Instant::now()-Duration::from_millis(1)} else {Instant::now()+RPC_TIMEOUT});
            let result=with_request(&connection.0.subagents,&send(),|state| {
                control_call(&connection.0,&mut rpc,json!({"parentThreadId":PARENT,"agentId":CHILD,"action":"send","requestId":REQUEST}),&cancelled,Instant::now()+RPC_TIMEOUT,|| {
                    state.receipts.insert(REQUEST.into(),Receipt {digest:digest(&send()).unwrap(),status:"unknown"});
                    state.unresolved=Some(REQUEST.into());state.unresolved_runtime=Some(PARENT_RUNTIME.into());
                })
            }).unwrap();
            assert_eq!(result["status"],"notApplied");
            let state=connection.0.subagents.lock().unwrap();assert!(state.receipts.is_empty());assert!(state.unresolved.is_none());
            drop(state);drop(rpc);server.join().unwrap();
        }
    }
    #[test]
    fn subagents_control_honors_expired_overall_deadline_without_rpc_deadline() {
        let connection=connection();let (mut rpc,mut peer)=raw_rpc();assert!(rpc.deadline.is_none());
        let server=thread::spawn(move||assert!(peer.read().is_err(),"전체 기한이 만료되면 RPC 자체 기한이 없어도 프레임을 보내면 안 됩니다."));
        let cancelled=AtomicBool::new(false);
        let result=with_request(&connection.0.subagents,&send(),|state| {
            control_call(&connection.0,&mut rpc,json!({"parentThreadId":PARENT,"agentId":CHILD,"action":"send","requestId":REQUEST}),&cancelled,Instant::now()-Duration::from_millis(1),|| {
                state.receipts.insert(REQUEST.into(),Receipt {digest:digest(&send()).unwrap(),status:"unknown"});
                state.unresolved=Some(REQUEST.into());state.unresolved_runtime=Some(PARENT_RUNTIME.into());
            })
        }).unwrap();
        assert_eq!(result["status"],"notApplied");
        let state=connection.0.subagents.lock().unwrap();assert!(state.receipts.is_empty());assert!(state.unresolved.is_none());
        drop(state);drop(rpc);server.join().unwrap();
    }
    #[test]
    fn subagents_busy_same_request_is_unknown_and_new_request_is_not_applied() {
        let shared=Mutex::new(State::default());
        let result=with_request(&shared,&send(),|state| {
            let duplicate=with_request(&shared,&send(),|_|panic!("같은 진행 요청을 실행하면 안 됩니다.")).unwrap();
            assert_eq!(duplicate["status"],"unknown");
            let fresh=SubagentRequest::Close {agent_id:CHILD.into(),request_id:Uuid::new_v4().to_string(),expected_parent_runtime_id:PARENT_RUNTIME.into(),expected_runtime_id:Some(RUNTIME.into())};
            let rejected=with_request(&shared,&fresh,|_|panic!("다른 새 요청을 실행하면 안 됩니다.")).unwrap();assert_eq!(rejected["status"],"notApplied");
            let changed=SubagentRequest::Send {agent_id:CHILD.into(),request_id:REQUEST.into(),text:"다른 내용".into(),expected_parent_runtime_id:PARENT_RUNTIME.into(),expected_runtime_id:Some(RUNTIME.into())};
            assert!(with_request(&shared,&changed,|_|panic!("다른 내용을 실행하면 안 됩니다.")).is_err());
            // 실제 송신을 시도한 뒤의 실패 상태와 같이 기록한다.
            state.receipts.insert(REQUEST.into(),Receipt {digest:digest(&send())?,status:"unknown"});state.unresolved=Some(REQUEST.into());state.unresolved_runtime=Some(PARENT_RUNTIME.into());
            Err("관리 응답 유실".into())
        }).unwrap();
        assert_eq!(result["status"],"unknown");
        let repeated=with_request(&shared,&send(),|state|Ok(state.previous(&send())?.unwrap())).unwrap();assert_eq!(repeated["status"],"unknown");
        assert!(shared.lock().unwrap().in_flight.is_none());
    }
    #[test]
    fn subagents_cancellation_and_deadline_abort_pending_io_without_late_management_send() {
        for stop in [false,true] {
            let (mut rpc,mut peer)=raw_rpc();let stream=rpc.ws.get_ref().try_clone().unwrap();
            let server=thread::spawn(move|| {let request=read(&mut peer);assert_eq!(request["method"],"initialize");assert!(peer.read().is_err());});
            let connection=connection();let cancelled=Arc::new(AtomicBool::new(false));let transport=Arc::new(Mutex::new(Transport {socket:Some(stream),proxy:None}));
            let started=Instant::now();let deadline=started+if stop {Duration::from_secs(2)} else {Duration::from_millis(50)};
            let watch=DeadlineWatch::start(Arc::new(Mutex::new(Some(deadline))),transport,cancelled.clone(),connection.0.stopped.clone()).unwrap();
            let stopping=if stop {let stopped=connection.0.stopped.clone();Some(thread::spawn(move|| {thread::sleep(Duration::from_millis(50));stopped.store(true,Ordering::Release);} ))} else {None};
            let mut state=State::default();
            assert!(exchange(&connection.0,&mut state,&mut rpc,PARENT,&send(),&cancelled,deadline).is_err());
            drop(rpc);drop(watch);if let Some(stopping)=stopping {stopping.join().unwrap();}server.join().unwrap();
            assert!(started.elapsed()<Duration::from_secs(1));assert!(state.receipts.is_empty());
        }
    }
    #[test]
    fn subagents_stop_during_management_reply_preserves_unknown_receipt() {
        let (mut rpc,mut peer)=raw_rpc();let connection=connection();let stopped=connection.0.stopped.clone();
        let server=thread::spawn(move|| {
            initialize_peer(&mut peer,json!({"supportsSubagentManagement":true}));assert_eq!(read(&mut peer)["method"],CONTROL);
            stopped.store(true,Ordering::Release);assert!(peer.read().is_err());
        });
        let transport=Arc::new(Mutex::new(Transport {socket:Some(rpc.ws.get_ref().try_clone().unwrap()),proxy:None}));
        let cancelled=Arc::new(AtomicBool::new(false));let deadline=Instant::now()+RPC_TIMEOUT;
        let watch=DeadlineWatch::start(Arc::new(Mutex::new(Some(deadline))),transport,cancelled.clone(),connection.0.stopped.clone()).unwrap();
        let mut state=State::default();state.page(PARENT,page()).unwrap();
        assert!(exchange(&connection.0,&mut state,&mut rpc,PARENT,&send(),&cancelled,deadline).is_err());
        assert_eq!(state.unresolved.as_deref(),Some(REQUEST));assert_eq!(state.previous(&send()).unwrap().unwrap()["status"],"unknown");
        drop(rpc);drop(watch);server.join().unwrap();
    }
}
