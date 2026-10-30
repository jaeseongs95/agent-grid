//! 기존 CLI 예약 관리자의 전용 앱 요청 경계. 별도 실행기와 텍스트 주입은 만들지 않는다.
use super::*;

const MAX_PROMPT_BYTES: usize = 64 * 1024;
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "camelCase", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum CronRequest {
    List {},
    Add { request_id: String, schedule: CronSchedule, prompt: String },
    Pause { request_id: String, job_id: String },
    Resume { request_id: String, job_id: String },
    Remove { request_id: String, job_id: String },
    Cancel { request_id: String, job_id: String },
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "camelCase", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum CronSchedule { Every { seconds: u64 }, At { at_unix_ms: u64 } }

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CronSnapshot {
    supported: bool,
    connected: bool,
    scope: CronScope,
    jobs: Vec<CronJob>,
    #[serde(default)]
    has_more: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mutation_state: Option<CronMutationState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reconciled_request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "optional_exact_bool")]
    supports_expected_turn_interrupt: Option<bool>,
}

fn optional_exact_bool<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Option<bool>, D::Error> {
    bool::deserialize(deserializer).map(Some)
}
#[derive(Deserialize, Serialize)]
enum CronScope { #[serde(rename = "cliLifetime")] CliLifetime }
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
enum CronMutationState { Clear, Unknown }
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CronJob {
    id: String,
    prompt_preview: String,
    schedule: CronSchedule,
    state: CronJobState,
    paused: bool,
    removed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    next_run_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    skipped: Option<u64>,
}
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
enum CronJobState { Ready, Pending, Running, Finished, Unknown, BlockedUnsupported, BlockedConnection }

fn canonical_job_id(id: &str) -> bool {
    id.len() <= 20 && id.parse::<u64>().is_ok_and(|value| value > 0 && value.to_string() == id)
}

/// 실제 통로에서 받은 목록만 이 경계로 넘긴다. 소유권·영수증 확인을 대신하지 않는다.
pub(super) fn bounded_cron_snapshot(value: Value) -> Result<Value, String> {
    let snapshot: CronSnapshot = serde_json::from_value(value)
        .map_err(|_| "예약 목록의 필드 또는 자료형이 올바르지 않습니다.")?;
    if snapshot.jobs.len() > 100 || (snapshot.connected && !snapshot.supported)
        || snapshot.reason.as_ref().is_some_and(|reason| reason.len() > 256 || reason.contains('\0'))
        || snapshot.reconciled_request_id.as_ref().is_some_and(|id| id.len() != 36 || Uuid::parse_str(id).is_err()) {
        return Err("예약 목록의 상한 또는 연결 상태가 올바르지 않습니다.".into());
    }
    let mut ids = std::collections::HashSet::new();
    for job in &snapshot.jobs {
        let schedule_valid = match job.schedule {
            CronSchedule::Every { seconds } => (1..=MAX_SAFE_INTEGER).contains(&seconds),
            // 이미 실행한 단발 예약도 조회해야 하므로 미래 시각 조건은 붙이지 않는다.
            CronSchedule::At { at_unix_ms } => (1..=MAX_SAFE_INTEGER).contains(&at_unix_ms),
        };
        if !canonical_job_id(&job.id) || !ids.insert(&job.id) || job.prompt_preview.chars().count() > 512
            || !schedule_valid || job.next_run_at.is_some_and(|time| !(1..=MAX_SAFE_INTEGER).contains(&time))
            || job.skipped.is_some_and(|count| count > MAX_SAFE_INTEGER) {
            return Err("예약 항목의 식별자·미리보기·시각이 허용 범위를 벗어났습니다.".into());
        }
    }
    serde_json::to_value(snapshot).map_err(|error| format!("예약 목록을 변환하지 못했습니다: {error}"))
}

impl CronRequest {
    pub(crate) fn validate(&self) -> Result<(),String> {
        self.validate_at(SystemTime::now(),Instant::now())
    }
    fn validate_at(&self,wall: SystemTime,monotonic: Instant) -> Result<(),String> {
        let (request_id,job_id)=match self {
            Self::List{}=>return Ok(()),
            Self::Add{request_id,schedule,prompt}=>{
                if prompt.len()>MAX_PROMPT_BYTES || prompt.trim().is_empty() || prompt.contains('\0') {return Err("예약 프롬프트는 비어 있지 않은 64KiB 이하 텍스트여야 합니다.".into());}
                match schedule {
                    CronSchedule::Every{seconds}=>{
                        if !(1..=MAX_SAFE_INTEGER).contains(seconds) || monotonic.checked_add(Duration::from_secs(*seconds)).is_none() {return Err("예약 간격은 표현 가능한 양의 안전 정수 초여야 합니다.".into());}
                    },
                    CronSchedule::At{at_unix_ms}=>{
                        let now=wall.duration_since(UNIX_EPOCH).map_err(|_| "현재 시각이 Unix 기준시각보다 이전입니다.")?.as_millis();
                        if !(1..=MAX_SAFE_INTEGER).contains(at_unix_ms) || u128::from(*at_unix_ms)<=now {return Err("예약 시각은 미래의 양의 안전 정수 Unix 밀리초여야 합니다.".into());}
                        let delay=Duration::from_millis((u128::from(*at_unix_ms)-now) as u64);
                        monotonic.checked_add(delay).ok_or("예약 시각을 실행 환경에서 표현할 수 없습니다.")?;
                    },
                }
                (request_id,None)
            },
            Self::Pause{request_id,job_id}|Self::Resume{request_id,job_id}|Self::Remove{request_id,job_id}|Self::Cancel{request_id,job_id}=>(request_id,Some(job_id)),
        };
        if request_id.len()!=36 || !Uuid::parse_str(request_id).is_ok_and(|id|id.to_string()==*request_id) {return Err("예약 변경 요청 식별자는 표준 소문자 UUID여야 합니다.".into());}
        if let Some(job_id)=job_id {
            if job_id.len()>20 {return Err("예약 식별자가 허용 크기를 넘었습니다.".into());}
            if !canonical_job_id(job_id) {return Err("예약 식별자는 양의 64비트 정수의 십진 문자열이어야 합니다.".into());}
        }
        Ok(())
    }
    fn unsupported(&self) -> Value {
        match self {
            Self::List{}=>json!({"supported":false,"connected":false,"scope":"cliLifetime","jobs":[],"hasMore":false,"reason":"unsupported"}),
            _=>json!({"status":"unsupported","reason":"unsupported"}),
        }
    }
}

impl CodexSession {
    pub(crate) fn cron_target_ready(&self,thread_id: &str) -> Result<(),String> {
        Uuid::parse_str(thread_id).map_err(|_| "예약 대상 대화 식별자가 올바르지 않습니다.")?;
        if self.stopped.load(Ordering::Acquire) {return Err("종료된 세션의 예약을 조회하거나 변경하지 않았습니다.".into());}
        if self.conversation.as_ref().is_some_and(ConversationConnection::is_deleted) {return Err("원래 대화가 삭제되어 예약을 조회하거나 변경하지 않았습니다.".into());}
        Ok(())
    }
    pub(crate) fn cron_request(&self,thread_id: &str,request: CronRequest) -> Result<Value,String> {
        request.validate()?;
        self.cron_target_ready(thread_id)?;
        // 예약은 TUI 프로세스가 소유한다. 구조화 제어 계약이 확인되기 전에는
        // app-server 메서드·PTY 입력·별도 실행기로 대체하지 않는다.
        let reply=request.unsupported();
        match request {CronRequest::List{}=>bounded_cron_snapshot(reply),_=>Ok(reply)}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn decode(value: Value) -> Result<CronRequest,String> {
        serde_json::from_value(value).map_err(|error|error.to_string())
    }
    fn valid(value: Value) -> bool {
        decode(value).and_then(|request|request.validate_at(UNIX_EPOCH+Duration::from_millis(1_700_000_000_000),Instant::now())).is_ok()
    }
    fn add(schedule: Value,prompt: String) -> Value {
        json!({"type":"add","requestId":Uuid::new_v4().to_string(),"schedule":schedule,"prompt":prompt})
    }
    fn snapshot() -> Value {
        json!({"supported":true,"connected":true,"scope":"cliLifetime","jobs":[{
            "id":"1","promptPreview":"시험 예약","schedule":{"type":"at","atUnixMs":1},
            "state":"finished","paused":false,"removed":false
        }],"hasMore":false})
    }
    #[test]
    fn cron_control_serialization_preserves_existing_request_shape_and_string_job_ids() {
        for value in [add(json!({"type":"every","seconds":5}),"여러 줄\n예약".into()),
            json!({"type":"cancel","requestId":Uuid::new_v4().to_string(),"jobId":"18446744073709551615"}),
            json!({"type":"list"})] {
            let request=decode(value.clone()).unwrap();
            assert_eq!(serde_json::to_value(request).unwrap(),value);
        }
    }
    #[test]
    fn cron_snapshot_bounds_real_rows_and_unicode_preview_without_rejecting_past_schedules() {
        let mut value=snapshot();
        let job=value["jobs"][0].clone();
        value["jobs"]=Value::Array((1..=100).map(|id| {
            let mut row=job.clone();row["id"]=json!(id.to_string());row["promptPreview"]=json!("🐈".repeat(512));row
        }).collect());
        assert_eq!(bounded_cron_snapshot(value.clone()).unwrap()["jobs"].as_array().unwrap().len(),100);
        value["jobs"][0]["promptPreview"]=json!("🐈".repeat(513));
        assert!(bounded_cron_snapshot(value).is_err());
        assert!(bounded_cron_snapshot(snapshot()).is_ok(),"과거 단발 예약은 목록 조회 대상이다");
    }
    #[test]
    fn cron_snapshot_interrupt_support_requires_an_exact_present_bool() {
        assert!(bounded_cron_snapshot(snapshot()).is_ok());
        for support in [json!(true),json!(false)] {
            let mut value=snapshot();value["supportsExpectedTurnInterrupt"]=support;
            assert!(bounded_cron_snapshot(value).is_ok());
        }
        for invalid in [Value::Null,json!("true"),json!(1),json!({}),json!([])] {
            let mut value=snapshot();value["supportsExpectedTurnInterrupt"]=invalid;
            assert!(bounded_cron_snapshot(value).is_err());
        }
    }
    #[test]
    fn cron_snapshot_rejects_oversized_hidden_duplicate_and_incoherent_peer_data() {
        let mut invalid=Vec::new();
        let mut value=snapshot();value["jobs"]=json!([value["jobs"][0].clone(),value["jobs"][0].clone()]);invalid.push(value);
        let mut value=snapshot();value["jobs"]=Value::Array(vec![value["jobs"][0].clone();101]);invalid.push(value);
        let mut value=snapshot();value["jobs"][0]["prompt"]=json!("전송하면 안 되는 원문");invalid.push(value);
        for (field,wrong) in [("id",json!("01")),("id",json!("18446744073709551616")),("state",json!("다른 상태")),("nextRunAt",json!(MAX_SAFE_INTEGER+1)),("skipped",json!(MAX_SAFE_INTEGER+1))] {
            let mut value=snapshot();value["jobs"][0][field]=wrong;invalid.push(value);
        }
        let mut value=snapshot();value["supported"]=json!(false);invalid.push(value);
        let mut value=snapshot();value["reconciledRequestId"]=json!("다른 요청");invalid.push(value);
        let mut value=snapshot();value["reason"]=json!("x".repeat(257));invalid.push(value);
        for value in invalid {
            let error=bounded_cron_snapshot(value).unwrap_err();
            assert!(!error.contains("전송하면 안 되는 원문"));
        }
    }
    fn fixture(socket: Option<TcpStream>) -> CodexSession {
        CodexSession {command:CommandBuilder::new("예약-미송신-시험"),server:None,job:None,_launch_gate:None,stopped:Arc::new(AtomicBool::new(false)),socket,observer:None,mobile:None,shared:None,proxy_job:None,conversation:None}
    }
    #[test]
    fn cron_requests_reject_unknown_fields_wrong_shapes_and_missing_mutation_identity() {
        assert!(valid(json!({"type":"list"})));
        for value in [
            json!({"type":"list","threadId":Uuid::new_v4().to_string()}),json!({"type":"list","prompt":"숨은 쓰기"}),
            json!({"type":"update"}),json!({"type":"add","schedule":{"type":"every","seconds":10},"prompt":"작업"}),
            json!({"type":"add","requestId":"잘못된 식별자","schedule":{"type":"every","seconds":10},"prompt":"작업"}),
            json!({"type":"pause","requestId":Uuid::new_v4().to_string()}),
            json!({"type":"cancel","requestId":Uuid::new_v4().to_string(),"jobId":"1","turnId":"다른 작업"}),
        ] {assert!(!valid(value.clone()),"허용하지 않는 예약 요청: {value}");}
        for action in ["pause","resume","remove","cancel"] {
            assert!(valid(json!({"type":action,"requestId":Uuid::new_v4().to_string(),"jobId":"18446744073709551615"})));
            for job_id in ["0","01","-1","+1","1.0","18446744073709551616","다른 예약"] {
                assert!(!valid(json!({"type":action,"requestId":Uuid::new_v4().to_string(),"jobId":job_id})));
            }
        }
    }
    #[test]
    fn cron_schedule_and_prompt_bounds_are_validated_before_any_operation() {
        for seconds in [json!(0),json!(-1),json!(1.5),json!(MAX_SAFE_INTEGER+1),json!("10")] {
            assert!(!valid(add(json!({"type":"every","seconds":seconds}),"작업".into())));
        }
        assert!(valid(add(json!({"type":"every","seconds":1}),"x".repeat(MAX_PROMPT_BYTES))));
        assert!(!valid(add(json!({"type":"every","seconds":1}),"x".repeat(MAX_PROMPT_BYTES+1))));
        assert!(valid(add(json!({"type":"every","seconds":1}),"가".repeat(MAX_PROMPT_BYTES/3))));
        assert!(!valid(add(json!({"type":"every","seconds":1}),"가".repeat(MAX_PROMPT_BYTES/3+1))));
        for prompt in ["", " \n\t", "작업\0문자"] {assert!(!valid(add(json!({"type":"every","seconds":1}),prompt.into())));}
        assert!(valid(add(json!({"type":"at","atUnixMs":1_700_000_000_001u64}),"일회성 작업".into())));
        for timestamp in [json!(0),json!(1_700_000_000_000u64),json!(MAX_SAFE_INTEGER+1),json!(1.5),Value::Null] {
            assert!(!valid(add(json!({"type":"at","atUnixMs":timestamp}),"작업".into())));
        }
        assert!(!valid(add(json!({"type":"at","at":"2030-01-01T09:00:00+09:00"}),"작업".into())));
        assert!(!valid(add(json!({"type":"every","seconds":10,"unexpected":true}),"작업".into())));
    }
    #[test]
    fn cron_unsupported_is_distinct_from_empty_list_and_never_sends_to_the_existing_socket() {
        let listener=TcpListener::bind("127.0.0.1:0").unwrap();
        let socket=TcpStream::connect(listener.local_addr().unwrap()).unwrap();let(mut server,_)=listener.accept().unwrap();
        server.set_read_timeout(Some(Duration::from_secs(2))).unwrap();let adapter=fixture(Some(socket));let thread_id=Uuid::new_v4().to_string();
        let list=adapter.cron_request(&thread_id,CronRequest::List{}).unwrap();
        assert_eq!(list["supported"],false);assert_eq!(list["connected"],false);assert_eq!(list["scope"],"cliLifetime");assert_eq!(list["reason"],"unsupported");assert_eq!(list["jobs"],json!([]));assert_eq!(list["hasMore"],false);assert!(list.get("mutationState").is_none());
        let mut requests=vec![decode(add(json!({"type":"every","seconds":10}),"원래 프롬프트".into())).unwrap()];
        for action in ["pause","resume","remove","cancel"] {requests.push(decode(json!({"type":action,"requestId":Uuid::new_v4().to_string(),"jobId":"1"})).unwrap());}
        for request in requests {let response=adapter.cron_request(&thread_id,request).unwrap();assert_eq!(response,json!({"status":"unsupported","reason":"unsupported"}));assert!(response.get("mutationState").is_none());}
        assert!(!adapter.stopped.load(Ordering::Acquire));drop(adapter);
        let read=server.read(&mut[0u8;1]);
        let closed=match &read {Ok(0)=>true,Err(error)=>matches!(error.kind(),io::ErrorKind::ConnectionAborted|io::ErrorKind::ConnectionReset),_=>false};
        assert!(closed,"미지원 예약은 기존 소켓·PTY에 바이트를 쓰지 않아야 한다. 시간 초과는 종료 근거가 아니다: {read:?}");
    }
    #[test]
    fn cron_rejects_stopped_deleted_or_invalid_thread_without_creating_a_transport() {
        let adapter=fixture(None);assert!(adapter.cron_request("잘못된 대화",CronRequest::List{}).is_err());
        adapter.stopped.store(true,Ordering::Release);assert!(adapter.cron_request(&Uuid::new_v4().to_string(),CronRequest::List{}).is_err());assert!(adapter.socket.is_none());
        let mut deleted=fixture(None);let thread_id=Uuid::new_v4().to_string();
        let connection=ConversationConnection::new(conversation::Endpoint::Owned{address:String::new(),token:String::new(),job:std::sync::Weak::new()},Arc::new(AtomicBool::new(false)));
        assert!(connection.observer_message(&thread_id,json!({"method":"thread/deleted","params":{"threadId":thread_id}})).is_err());
        deleted.conversation=Some(connection);assert!(deleted.cron_request(&thread_id,CronRequest::List{}).unwrap_err().contains("삭제"));assert!(deleted.socket.is_none());
    }
}
