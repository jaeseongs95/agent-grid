//! One short-lived management connection to the session's existing owned server.
//! Endpoint and credential never cross the renderer boundary or extend server life.
use super::*;
use std::{net::SocketAddr, sync::Weak};

const MAX_REPLY: usize = 64 * 1024;

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MobileAction { Status, Enable, Disable, Pair }

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MobileReply {
    pub isolated: bool,
    pub shared: bool,
    pub mutations_allowed: bool,
    pub status: Option<MobileStatus>,
    pub pairing: Option<MobilePairing>,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MobileStatus {
    pub status: String,
    pub server_name: String,
    pub environment_id: Option<String>,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MobilePairing {
    pub manual_code: String,
    pub expires_at: i64,
    pub environment_id: String,
}

#[derive(Clone)]
pub struct MobileConnection(Arc<Management>);
struct Management {
    endpoint: String,
    token: String,
    job: Weak<ProcessJob>,
    stopped: Arc<AtomicBool>,
    admission: Mutex<()>,
    socket: Mutex<Option<TcpStream>>,
    namespaced_remote: bool,
    shared: Option<shared::SharedEndpoint>,
}

impl MobileConnection {
    pub(super) fn new(endpoint: String, token: String, job: &Arc<ProcessJob>, stopped: Arc<AtomicBool>, namespaced_remote: bool) -> Self {
        Self(Arc::new(Management { endpoint, token, job: Arc::downgrade(job), stopped, admission:Mutex::new(()), socket:Mutex::new(None), namespaced_remote, shared:None }))
    }

    pub(super) fn shared(endpoint: shared::SharedEndpoint, stopped: Arc<AtomicBool>) -> Self {
        Self(Arc::new(Management { endpoint:String::new(),token:String::new(),job:Weak::new(),stopped,admission:Mutex::new(()),socket:Mutex::new(None),namespaced_remote:false,shared:Some(endpoint) }))
    }

    pub(super) fn cancel(&self) {
        self.0.stopped.store(true, Ordering::Release);
        if let Ok(socket) = self.0.socket.lock() {
            if let Some(socket) = socket.as_ref() { let _ = socket.shutdown(Shutdown::Both); }
        }
    }

    fn live(&self) -> Result<(), String> {
        if self.0.stopped.load(Ordering::Acquire) { Err("모바일 연결 대상 세션이 종료되었습니다. 진행 중 요청의 결과는 확인되지 않았습니다.".into()) }
        else { Ok(()) }
    }

    pub fn request(&self, action: MobileAction) -> Result<MobileReply, String> {
        if !matches!(action, MobileAction::Status) && !self.0.namespaced_remote && self.0.shared.is_none() {
            return Err("기존 원격 등록을 보호하기 위해 개별 세션 서버의 모바일 변경을 차단했습니다. 기존 공유 Codex 서버를 실행한 뒤 새 세션을 시작하거나 --remote-control-client-name 지원 CLI를 선택하세요.".into());
        }
        let _permit = self.0.admission.try_lock().map_err(|error| match error {
            std::sync::TryLockError::WouldBlock => "이 세션의 모바일 연결 요청이 진행 중입니다.",
            std::sync::TryLockError::Poisoned(_) => "모바일 연결 요청 상태를 확인할 수 없습니다. 세션을 다시 여세요.",
        })?;
        self.live()?;
        if let Some(endpoint) = &self.0.shared {
            let deadline = Instant::now() + RPC_TIMEOUT;
            let mut proxy = endpoint.open_until(Some(self.0.stopped.clone()), Some(deadline))?;
            let mut rpc = proxy.rpc.take().unwrap();
            let result = self.run_rpc_until(action, &mut rpc, deadline);
            drop(rpc);
            drop(proxy);
            return result;
        }
        let address = owned_address(&self.0.endpoint)?;
        let stream = TcpStream::connect_timeout(&address, Duration::from_secs(2))
            .map_err(|_| "기존 세션 서버에 연결할 수 없습니다.")?;
        self.live()?;
        // Keep the exact connected tuple pinned to the session's original job.
        // A stale clone cannot authenticate to a replacement listener on this port.
        let job = self.0.job.upgrade().ok_or("모바일 연결 대상 서버가 종료되었습니다.")?;
        job.verify_tcp_peer(&stream).map_err(|_| "세션 서버의 연결 소유권을 확인할 수 없습니다.")?;
        drop(job);
        self.run_stream(action, stream, RPC_TIMEOUT)
    }

    fn run_stream(&self, action: MobileAction, stream: TcpStream, timeout: Duration) -> Result<MobileReply, String> {
        // Cancellation can close a handshake or read without waiting for admission.
        let duplicate = stream.try_clone().map_err(|_| "모바일 관리 연결을 준비할 수 없습니다.")?;
        *self.0.socket.lock().map_err(|_| "모바일 관리 연결 상태를 확인할 수 없습니다.")? = Some(duplicate);
        let _cleanup = SocketCleanup(self);
        self.live()?;
        let mut rpc = authenticated_socket(&self.0.endpoint, &self.0.token, stream)
            .map_err(|_| "세션 서버 인증 연결에 실패했습니다.")?;
        self.run_rpc(action, &mut rpc, timeout)
    }

    fn run_rpc<S: Read + Write>(&self, action: MobileAction, rpc: &mut Rpc<S>, timeout: Duration) -> Result<MobileReply, String> {
        self.run_rpc_until(action, rpc, Instant::now() + timeout)
    }
    fn run_rpc_until<S: Read + Write>(&self, action: MobileAction, rpc: &mut Rpc<S>, deadline: Instant) -> Result<MobileReply, String> {
        rpc.ws.set_config(|config| {
            config.max_message_size = Some(MAX_REPLY);
            config.max_frame_size = Some(MAX_REPLY);
        });
        let mut init = initialize();
        init["capabilities"] = json!({"experimentalApi":true});
        self.call(rpc, "initialize", init, deadline)?;
        self.live()?;
        rpc.send(json!({"method":"initialized","params":{}})).map_err(|_| "모바일 관리 초기화 전송에 실패했습니다.")?;
        let state = self.call(rpc, "remoteControl/status/read", json!({}), deadline)?;
        let current = parse_status(&state, &self.0.token)?;
        let mut reply = MobileReply { isolated:self.0.namespaced_remote, shared:self.0.shared.is_some(), mutations_allowed:self.0.namespaced_remote || self.0.shared.is_some(), status:None, pairing:None };
        match action {
            MobileAction::Status => reply.status = Some(current),
            MobileAction::Enable | MobileAction::Disable => {
                let method = if matches!(action, MobileAction::Enable) { "remoteControl/enable" } else { "remoteControl/disable" };
                let result = self.call(rpc, method, json!({"ephemeral":true}), deadline)?;
                reply.status = Some(parse_status(&result, &self.0.token)?);
            }
            MobileAction::Pair => {
                let result = self.call(rpc, "remoteControl/pairing/start", json!({"manualCode":true}), deadline)?;
                reply.pairing = Some(parse_pairing(&result, &self.0.token)?);
            }
        }
        self.live()?;
        Ok(reply)
    }

    fn call<S: Read + Write>(&self, rpc: &mut Rpc<S>, method: &str, params: Value, deadline: Instant) -> Result<Value, String> {
        self.live()?;
        if Instant::now() >= deadline { return Err(timeout_error()); }
        rpc.next_id += 1;
        let id = rpc.next_id;
        rpc.send(json!({"id":id,"method":method,"params":params})).map_err(|_| "모바일 관리 요청 전송에 실패했습니다. 요청 결과는 확인되지 않았습니다.")?;
        while Instant::now() < deadline {
            self.live()?;
            if let Some(message) = rpc.read().map_err(|_| "모바일 관리 응답을 읽을 수 없습니다. 요청 결과는 확인되지 않았습니다.")? {
                if message.get("id") != Some(&json!(id)) { continue; }
                if let Some(error) = message.get("error") {
                    return Err(match error["code"].as_i64() {
                        Some(-32601) => "현재 CLI는 모바일 원격제어를 지원하지 않습니다.",
                        _ => "모바일 원격제어 요청을 거부했습니다. ChatGPT 로그인이 필요하며 API 키 인증은 사용할 수 없습니다. 관리 정책도 확인하세요.",
                    }.into());
                }
                return message.get("result").cloned().ok_or_else(|| "모바일 관리 응답 형식이 잘못되었습니다.".into());
            }
        }
        Err(timeout_error())
    }
}

struct SocketCleanup<'a>(&'a MobileConnection);
impl Drop for SocketCleanup<'_> {
    fn drop(&mut self) {
        if let Ok(mut socket) = self.0.0.socket.lock() {
            if let Some(socket) = socket.take() { let _ = socket.shutdown(Shutdown::Both); }
        }
    }
}
fn timeout_error() -> String { "모바일 관리 요청 시간이 초과되었습니다. 요청 결과는 확인되지 않았습니다. 상태를 다시 조회하세요.".into() }
fn owned_address(endpoint: &str) -> Result<SocketAddr, String> {
    let address: SocketAddr = endpoint.strip_prefix("ws://").ok_or("모바일 관리는 앱이 소유한 로컬 서버에서만 가능합니다.")?
        .parse().map_err(|_| "모바일 관리 서버 주소가 잘못되었습니다.")?;
    if !matches!(address, SocketAddr::V4(value) if value.ip().is_loopback() && value.port() != 0) {
        return Err("모바일 관리는 앱이 소유한 로컬 서버에서만 가능합니다.".into());
    }
    Ok(address)
}
fn bounded(value: &Value, field: &str, max: usize, secret: &str) -> Result<String, String> {
    let text = value[field].as_str().ok_or("모바일 관리 응답 필드가 누락되었습니다.")?;
    if text.trim().is_empty() || text.len() > max || text.chars().any(char::is_control)
        || text.contains("Bearer ") || (!secret.is_empty() && text.contains(secret)) {
        return Err("모바일 관리 응답 필드가 허용 범위를 벗어났습니다.".into());
    }
    Ok(text.to_owned())
}
fn parse_status(value: &Value, secret: &str) -> Result<MobileStatus, String> {
    let status = bounded(value, "status", 16, secret)?;
    if !matches!(status.as_str(), "disabled" | "connecting" | "connected" | "errored") { return Err("알 수 없는 모바일 연결 상태입니다.".into()); }
    let server_name = bounded(value, "serverName", 256, secret)?;
    let environment_id = match value.get("environmentId") {
        Some(Value::Null) => None,
        Some(_) => Some(bounded(value, "environmentId", 512, secret)?),
        None => return Err("모바일 관리 응답에 환경 정보가 없습니다.".into()),
    };
    Ok(MobileStatus {status,server_name,environment_id})
}
fn parse_pairing(value: &Value, secret: &str) -> Result<MobilePairing, String> {
    let manual_code = bounded(value, "manualPairingCode", 128, secret)?;
    if !manual_code.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == ' ') { return Err("모바일 연결 코드 형식이 잘못되었습니다.".into()); }
    let expires_at = value["expiresAt"].as_i64().filter(|value| *value > 0).ok_or("모바일 연결 코드 만료 시간이 잘못되었습니다.")?;
    let environment_id = bounded(value, "environmentId", 512, secret)?;
    Ok(MobilePairing {manual_code,expires_at,environment_id})
}

#[cfg(test)]
#[path = "codex_mobile_tests.rs"]
mod tests;
