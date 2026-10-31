//! 예약 제어의 닫힌 자료형과 일회 서버 인증. 입출력·Job·예약 실행은 소유하지 않는다.
#![cfg(windows)]

use super::cron::{CronRequest, CronSchedule};
use serde::{de::DeserializeOwned, Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use std::sync::atomic::{compiler_fence, Ordering};
use windows_sys::Win32::Security::Cryptography::{
    BCryptCloseAlgorithmProvider, BCryptCreateHash, BCryptDestroyHash, BCryptFinishHash,
    BCryptGenRandom, BCryptHashData, BCryptOpenAlgorithmProvider, BCRYPT_ALG_HANDLE,
    BCRYPT_ALG_HANDLE_HMAC_FLAG, BCRYPT_HASH_HANDLE, BCRYPT_SHA256_ALGORITHM,
    BCRYPT_USE_SYSTEM_PREFERRED_RNG,
};

const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const MAX_FRAME_BYTES: usize = 1_048_576;
const PIPE_PREFIX: &str = r"\\.\pipe\OpenDIY-TuiControl-";
pub(crate) const BOOTSTRAP_ENV: &str = "CODEX_DESKTOP_CRON_BOOTSTRAP";
pub(crate) const TOKEN_ENV: &str = "CODEX_DESKTOP_CRON_TOKEN";

fn invalid() -> String { "예약 통신 자료의 형식 또는 허용 범위가 올바르지 않습니다.".into() }
fn denied() -> String { "예약 통신의 소유 세대 또는 인증 증명이 일치하지 않습니다.".into() }
fn canonical_uuid(value: &str) -> bool {
    value.len() == 36 && value.bytes().enumerate().all(|(index, byte)| {
        if matches!(index, 8 | 13 | 18 | 23) { byte == b'-' }
        else { byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte) }
    })
}
fn canonical_u64(value: &str) -> bool {
    !value.is_empty() && value.len() <= 20 && value.bytes().all(|byte| byte.is_ascii_digit())
        && (value == "0" || !value.starts_with('0')) && value.parse::<u64>().is_ok()
}
fn lower_hex(value: &str, length: usize) -> bool {
    value.len() == length && value.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}
macro_rules! text_type {
    ($name:ident, $check:expr) => {
        #[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
        #[serde(try_from = "String", into = "String")]
        pub(crate) struct $name(String);
        impl TryFrom<String> for $name {
            type Error = String;
            fn try_from(value: String) -> Result<Self, String> {
                if ($check)(&value) { Ok(Self(value)) } else { Err(invalid()) }
            }
        }
        impl From<$name> for String { fn from(value: $name) -> Self { value.0 } }
        impl $name { pub(crate) fn as_str(&self) -> &str { &self.0 } }
    };
}
text_type!(UuidText, canonical_uuid);
text_type!(DecimalU64, canonical_u64);
text_type!(NonceHex, |value: &str| lower_hex(value, 32));
text_type!(MacHex, |value: &str| lower_hex(value, 64));
text_type!(PipeName, |value: &str| value.strip_prefix(PIPE_PREFIX).is_some_and(|suffix| lower_hex(suffix, 32)));
impl DecimalU64 {
    pub(crate) fn new(value: u64) -> Self { Self(value.to_string()) }
    pub(crate) fn get(&self) -> u64 { self.0.parse().expect("정규 십진수 자료형") }
}
impl UuidText {
    pub(crate) fn parse(value: &str) -> Result<Self, String> { Self::try_from(value.to_owned()) }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct VersionOne;
impl Serialize for VersionOne {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> { serializer.serialize_u8(1) }
}
impl<'de> Deserialize<'de> for VersionOne {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        if u8::deserialize(deserializer)? == 1 { Ok(Self) }
        else { Err(serde::de::Error::custom("지원하지 않는 예약 통신 버전입니다.")) }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub(crate) struct Pid(u32);
impl Pid {
    pub(crate) fn new(value: u32) -> Result<Self, String> {
        if value != 0 { Ok(Self(value)) } else { Err(invalid()) }
    }
    pub(crate) fn get(self) -> u32 { self.0 }
}
impl<'de> Deserialize<'de> for Pid {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::new(u32::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}
macro_rules! string_enum {
    ($name:ident { $($variant:ident => $wire:literal),+ $(,)? }) => {
        #[derive(Clone, Copy, Serialize)]
        pub(crate) enum $name { $(#[serde(rename = $wire)] $variant),+ }
        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                match String::deserialize(deserializer)?.as_str() {
                    $($wire => Ok(Self::$variant),)+
                    _ => Err(serde::de::Error::custom(invalid())),
                }
            }
        }
    };
}
string_enum!(HelloType { Value => "hello" });
string_enum!(ChallengeType { Value => "challenge" });
string_enum!(AuthType { Value => "auth" });
string_enum!(ReadyType { Value => "ready" });
string_enum!(RequestType { Value => "request" });
string_enum!(ResponseType { Value => "response" });

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Bootstrap {
    pub(crate) v: VersionOne,
    pub(crate) pipe: PipeName,
    pub(crate) host_pid: Pid,
    pub(crate) host_created_filetime: DecimalU64,
    pub(crate) session_id: UuidText,
    pub(crate) session_generation: DecimalU64,
}
impl Bootstrap {
    pub(crate) fn new(pipe: &str, host_pid: u32, host_created_filetime: u64, session_id: &str, generation: u64) -> Result<Self, String> {
        Ok(Self { v: VersionOne, pipe: PipeName::try_from(pipe.to_owned())?, host_pid: Pid::new(host_pid)?,
            host_created_filetime: DecimalU64::new(host_created_filetime), session_id: UuidText::parse(session_id)?,
            session_generation: DecimalU64::new(generation) })
    }
}
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Hello {
    pub(crate) v: VersionOne,
    #[serde(rename = "type")] pub(crate) kind: HelloType,
    pub(crate) session_id: UuidText,
    pub(crate) session_generation: DecimalU64,
    pub(crate) owner_instance_id: UuidText,
    pub(crate) client_pid: Pid,
    pub(crate) client_created_filetime: DecimalU64,
    pub(crate) client_nonce: NonceHex,
}
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Challenge {
    pub(crate) v: VersionOne,
    #[serde(rename = "type")] pub(crate) kind: ChallengeType,
    pub(crate) server_nonce: NonceHex,
    pub(crate) proof: MacHex,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Auth {
    pub(crate) v: VersionOne,
    #[serde(rename = "type")] pub(crate) kind: AuthType,
    pub(crate) proof: MacHex,
}
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Ready {
    pub(crate) v: VersionOne,
    #[serde(rename = "type")] pub(crate) kind: ReadyType,
    pub(crate) owner_instance_id: UuidText,
}

/// 비밀의 의도적 환경 전달만 허용한다. 형식 출력·직렬화·복제를 제공하지 않는다.
pub(crate) struct SecretToken([u8; 32]);
pub(crate) struct SecretHex([u8; 64]);
fn wipe(bytes: &mut [u8]) {
    for byte in bytes { unsafe { std::ptr::write_volatile(byte, 0); } }
    compiler_fence(Ordering::SeqCst);
}
impl Drop for SecretToken { fn drop(&mut self) { wipe(&mut self.0); } }
impl Drop for SecretHex { fn drop(&mut self) { wipe(&mut self.0); } }
impl SecretHex {
    pub(crate) fn as_str(&self) -> &str { std::str::from_utf8(&self.0).expect("소문자 16진수 비밀") }
}
impl SecretToken {
    pub(crate) fn generate() -> Result<Self, String> {
        let mut secret = Self([0; 32]);
        random(&mut secret.0)?;
        Ok(secret)
    }
    pub(crate) fn expose_hex(&self) -> SecretHex {
        let mut result = SecretHex([0; 64]);
        encode_hex_into(&self.0, &mut result.0);
        result
    }
}
fn random(bytes: &mut [u8]) -> Result<(), String> {
    // SAFETY: 출력 버퍼가 호출 완료까지 유효하며 시스템 난수 공급자만 사용한다.
    crypto_status(unsafe { BCryptGenRandom(std::ptr::null_mut(), bytes.as_mut_ptr(), bytes.len() as u32, BCRYPT_USE_SYSTEM_PREFERRED_RNG) })
}
fn encode_hex_into(bytes: &[u8], output: &mut [u8]) {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    for (byte, pair) in bytes.iter().zip(output.chunks_exact_mut(2)) {
        pair[0] = DIGITS[(byte >> 4) as usize]; pair[1] = DIGITS[(byte & 15) as usize];
    }
}
fn encode_hex(bytes: &[u8]) -> String {
    let mut output = vec![0; bytes.len() * 2];
    encode_hex_into(bytes, &mut output);
    String::from_utf8(output).expect("소문자 16진수")
}
fn decode_mac(mac: &MacHex) -> [u8; 32] {
    fn digit(byte: u8) -> u8 { if byte <= b'9' { byte - b'0' } else { byte - b'a' + 10 } }
    let mut result = [0; 32];
    for (byte, pair) in result.iter_mut().zip(mac.0.as_bytes().chunks_exact(2)) { *byte = digit(pair[0]) * 16 + digit(pair[1]); }
    result
}
fn crypto_status(status: i32) -> Result<(), String> {
    if status >= 0 { Ok(()) } else { Err(format!("운영체제 인증 연산이 실패했습니다: 0x{:08x}", status as u32)) }
}
struct Algorithm(BCRYPT_ALG_HANDLE);
impl Drop for Algorithm { fn drop(&mut self) { unsafe { BCryptCloseAlgorithmProvider(self.0, 0); } } }
struct Hash(BCRYPT_HASH_HANDLE);
impl Drop for Hash { fn drop(&mut self) { unsafe { BCryptDestroyHash(self.0); } } }
fn hmac_sha256(key: &[u8], parts: &[&[u8]]) -> Result<[u8; 32], String> {
    let mut algorithm = std::ptr::null_mut();
    // SAFETY: CNG 핸들을 성공 직후 단일 소유자로 감싼다. 비밀은 CNG에 읽기 전용으로 빌려준다.
    crypto_status(unsafe { BCryptOpenAlgorithmProvider(&mut algorithm, BCRYPT_SHA256_ALGORITHM, std::ptr::null(), BCRYPT_ALG_HANDLE_HMAC_FLAG) })?;
    let algorithm = Algorithm(algorithm);
    let mut hash = std::ptr::null_mut();
    // Windows 7 이후 CNG의 내부 객체 할당을 사용하고 해시 핸들과 함께 해제한다.
    crypto_status(unsafe { BCryptCreateHash(algorithm.0, &mut hash, std::ptr::null_mut(), 0, key.as_ptr(), key.len() as u32, 0) })?;
    let hash = Hash(hash);
    for part in parts {
        crypto_status(unsafe { BCryptHashData(hash.0, part.as_ptr(), part.len() as u32, 0) })?;
    }
    let mut output = [0; 32];
    crypto_status(unsafe { BCryptFinishHash(hash.0, output.as_mut_ptr(), output.len() as u32, 0) })?;
    Ok(output)
}
fn transcript(bootstrap: &Bootstrap, hello: &Hello, nonce: &NonceHex) -> Vec<u8> {
    format!("desktop-cron/pipe-v1\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n",
        bootstrap.session_id.as_str(), bootstrap.session_generation.as_str(), bootstrap.host_pid.get(),
        bootstrap.host_created_filetime.as_str(), hello.owner_instance_id.as_str(), hello.client_pid.get(),
        hello.client_created_filetime.as_str(), hello.client_nonce.as_str(), nonce.as_str()).into_bytes()
}
fn constant_time_equal(left: &[u8; 32], right: &[u8; 32]) -> bool {
    let mut difference = 0u8;
    for index in 0..32 {
        // SAFETY: 같은 고정 길이를 끝까지 읽어 최적화된 조기 종료 비교로 바뀌지 않게 한다.
        difference |= unsafe { std::ptr::read_volatile(&left[index]) ^ std::ptr::read_volatile(&right[index]) };
    }
    difference == 0
}

/// begin은 OS 신원·Job 검사 뒤 한 번만 호출한다. 토큰은 begin 반환 전에 폐기한다.
pub(crate) struct ServerHandshake { expected: [u8; 32], owner: UuidText }
impl Drop for ServerHandshake { fn drop(&mut self) { wipe(&mut self.expected); } }
impl ServerHandshake {
    pub(crate) fn begin(token: SecretToken, bootstrap: &Bootstrap, hello: Hello, verified_pid: u32, verified_created_filetime: u64) -> Result<(Self, Challenge), String> {
        if hello.session_id != bootstrap.session_id || hello.session_generation != bootstrap.session_generation
            || hello.client_pid.get() != verified_pid || hello.client_created_filetime.get() != verified_created_filetime {
            return Err(denied());
        }
        let mut random_nonce = [0; 16];
        random(&mut random_nonce)?;
        let nonce = NonceHex(encode_hex(&random_nonce));
        let transcript = transcript(bootstrap, &hello, &nonce);
        let desktop = hmac_sha256(&token.0, &[b"desktop\n", &transcript])?;
        let expected = hmac_sha256(&token.0, &[b"tui\n", &transcript])?;
        Ok((Self { expected, owner: hello.owner_instance_id }, Challenge {
            v: VersionOne, kind: ChallengeType::Value, server_nonce: nonce, proof: MacHex(encode_hex(&desktop)),
        }))
    }
    /// self를 소모하므로 성공·실패 모두 같은 도전을 재사용할 수 없다.
    pub(crate) fn finish(self, auth: Auth) -> Result<Ready, String> {
        let mut supplied = decode_mac(&auth.proof);
        let valid = constant_time_equal(&self.expected, &supplied);
        wipe(&mut supplied);
        if !valid { return Err(denied()); }
        Ok(Ready { v: VersionOne, kind: ReadyType::Value, owner_instance_id: self.owner.clone() })
    }
}

/// 실제 통로 결합 시험만을 위한 클라이언트 증명이다. 제품에는 포함하지 않는다.
#[cfg(test)]
pub(super) fn fixture_client_auth(token_hex: &str, bootstrap: &Bootstrap, hello: &Hello, challenge: &Challenge) -> Result<Auth, String> {
    if !lower_hex(token_hex, 64) { return Err(invalid()); }
    if hello.session_id != bootstrap.session_id || hello.session_generation != bootstrap.session_generation { return Err(denied()); }
    let mut token = SecretToken([0; 32]);
    let digit = |byte: u8| if byte <= b'9' { byte - b'0' } else { byte - b'a' + 10 };
    for (byte, pair) in token.0.iter_mut().zip(token_hex.as_bytes().chunks_exact(2)) {
        *byte = digit(pair[0]) * 16 + digit(pair[1]);
    }
    let data = transcript(bootstrap, hello, &challenge.server_nonce);
    let mut desktop = hmac_sha256(&token.0, &[b"desktop\n", &data])?;
    let mut supplied = decode_mac(&challenge.proof);
    let valid = constant_time_equal(&desktop, &supplied);
    wipe(&mut desktop);
    wipe(&mut supplied);
    if !valid { return Err(denied()); }
    let mut tui = hmac_sha256(&token.0, &[b"tui\n", &data])?;
    let proof = MacHex(encode_hex(&tui));
    wipe(&mut tui);
    Ok(Auth { v: VersionOne, kind: AuthType::Value, proof })
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Scope {
    pub(crate) backend_id: UuidText,
    pub(crate) backend_epoch: DecimalU64,
    pub(crate) thread_id: UuidText,
}
// 필드 생략과 null을 구분한다. nullable 경계만 별도 지정한다.
fn present<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where D: Deserializer<'de>, T: Deserialize<'de> { T::deserialize(deserializer).map(Some) }
fn nullable<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where D: Deserializer<'de>, T: Deserialize<'de> { Option::<T>::deserialize(deserializer) }
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Reconcile { pub(crate) request: CronRequest }
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Request {
    pub(crate) v: VersionOne,
    #[serde(rename = "type")] pub(crate) kind: RequestType,
    pub(crate) id: UuidText,
    pub(crate) owner_instance_id: UuidText,
    pub(crate) target_thread_id: UuidText,
    #[serde(deserialize_with = "nullable")] pub(crate) scope: Option<Scope>,
    pub(crate) body: CronRequest,
    #[serde(default, deserialize_with = "present", skip_serializing_if = "Option::is_none")]
    pub(crate) reconcile: Option<Reconcile>,
}
fn mutation_id(request: &CronRequest) -> Option<&str> {
    match request {
        CronRequest::List {} => None,
        CronRequest::Add { request_id, .. } | CronRequest::Pause { request_id, .. }
        | CronRequest::Resume { request_id, .. } | CronRequest::Remove { request_id, .. }
        | CronRequest::Cancel { request_id, .. } => Some(request_id),
    }
}
fn mutation_syntax(request: &CronRequest) -> Result<(), String> {
    if !mutation_id(request).is_some_and(canonical_uuid) { return Err(invalid()); }
    match request {
        CronRequest::Add { schedule, prompt, .. } => {
            if prompt.len() > 64 * 1024 || prompt.trim().is_empty() || prompt.contains('\0') { return Err(invalid()); }
            schedule_bounds(schedule)
        }
        CronRequest::Pause { job_id, .. } | CronRequest::Resume { job_id, .. }
        | CronRequest::Remove { job_id, .. } | CronRequest::Cancel { job_id, .. } => {
            if canonical_u64(job_id) && job_id != "0" { Ok(()) } else { Err(invalid()) }
        }
        CronRequest::List {} => Err(invalid()),
    }
}
impl Request {
    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.scope.as_ref().is_some_and(|scope| scope.thread_id != self.target_thread_id) { return Err(invalid()); }
        match &self.body {
            CronRequest::List {} => {
                if let Some(reconcile) = &self.reconcile {
                    if self.scope.is_none() { return Err(invalid()); }
                    // 재조정의 원래 단발 시각이 이미 지났어도 원문을 바꾸거나 거부하지 않는다.
                    mutation_syntax(&reconcile.request)?;
                }
            }
            mutation => {
                if self.scope.is_none() || self.reconcile.is_some() || mutation_id(mutation) != Some(self.id.as_str()) { return Err(invalid()); }
                mutation_syntax(mutation)?;
                mutation.validate()?;
            }
        }
        Ok(())
    }
}

string_enum!(MutationStatus { Applied => "applied", NotApplied => "notApplied", Unknown => "unknown", Unsupported => "unsupported" });
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct MutationResult {
    pub(crate) status: MutationStatus,
    #[serde(default, deserialize_with = "present", skip_serializing_if = "Option::is_none")]
    pub(crate) reason: Option<String>,
    #[serde(default, deserialize_with = "present", skip_serializing_if = "Option::is_none")]
    pub(crate) job_id: Option<String>,
}
string_enum!(LifetimeScope { CliLifetime => "cliLifetime" });
string_enum!(MutationState { Clear => "clear", Unknown => "unknown" });
string_enum!(JobState { Ready => "ready", Pending => "pending", Running => "running", Finished => "finished", Unknown => "unknown", BlockedUnsupported => "blockedUnsupported", BlockedConnection => "blockedConnection" });
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Job {
    pub(crate) id: String,
    pub(crate) prompt_preview: String,
    pub(crate) schedule: CronSchedule,
    pub(crate) state: JobState,
    pub(crate) paused: bool,
    pub(crate) removed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) next_run_at: Option<u64>,
    #[serde(default, deserialize_with = "present", skip_serializing_if = "Option::is_none")]
    pub(crate) skipped: Option<u64>,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ListResult {
    pub(crate) supported: bool,
    pub(crate) connected: bool,
    pub(crate) scope: LifetimeScope,
    pub(crate) jobs: Vec<Job>,
    pub(crate) has_more: bool,
    #[serde(skip_serializing_if = "Option::is_none")] pub(crate) supports_expected_turn_interrupt: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")] pub(crate) mutation_state: Option<MutationState>,
    #[serde(skip_serializing_if = "Option::is_none")] pub(crate) reconciled_request_id: Option<UuidText>,
    #[serde(skip_serializing_if = "Option::is_none")] pub(crate) reason: Option<String>,
}
#[derive(Serialize)]
#[serde(untagged)]
pub(crate) enum ResultBody { List(ListResult), Mutation(MutationResult) }
// untagged 역직렬화·Value 우회를 쓰지 않는다. 모든 키를 한 번만 읽는 닫힌 합집합이다.
#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ResultFields {
    #[serde(default, deserialize_with = "present")] supported: Option<bool>,
    #[serde(default, deserialize_with = "present")] connected: Option<bool>,
    #[serde(default, deserialize_with = "present")] scope: Option<LifetimeScope>,
    #[serde(default, deserialize_with = "present")] jobs: Option<Vec<Job>>,
    #[serde(default, deserialize_with = "present")] has_more: Option<bool>,
    #[serde(default, deserialize_with = "present")] supports_expected_turn_interrupt: Option<bool>,
    #[serde(default, deserialize_with = "present")] mutation_state: Option<MutationState>,
    #[serde(default, deserialize_with = "present")] reconciled_request_id: Option<UuidText>,
    #[serde(default, deserialize_with = "present")] reason: Option<String>,
    #[serde(default, deserialize_with = "present")] status: Option<MutationStatus>,
    #[serde(default, deserialize_with = "present")] job_id: Option<String>,
}
impl<'de> Deserialize<'de> for ResultBody {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let fields = ResultFields::deserialize(deserializer)?;
        let result = if let Some(status) = fields.status {
            if fields.supported.is_some() || fields.connected.is_some() || fields.scope.is_some() || fields.jobs.is_some()
                || fields.has_more.is_some() || fields.supports_expected_turn_interrupt.is_some() || fields.mutation_state.is_some() || fields.reconciled_request_id.is_some() {
                return Err(serde::de::Error::custom(invalid()));
            }
            Self::Mutation(MutationResult { status, reason: fields.reason, job_id: fields.job_id })
        } else {
            if fields.job_id.is_some() { return Err(serde::de::Error::custom(invalid())); }
            Self::List(ListResult {
                supported: fields.supported.ok_or_else(|| serde::de::Error::custom(invalid()))?,
                connected: fields.connected.ok_or_else(|| serde::de::Error::custom(invalid()))?,
                scope: fields.scope.ok_or_else(|| serde::de::Error::custom(invalid()))?,
                jobs: fields.jobs.ok_or_else(|| serde::de::Error::custom(invalid()))?,
                has_more: fields.has_more.ok_or_else(|| serde::de::Error::custom(invalid()))?,
                supports_expected_turn_interrupt: fields.supports_expected_turn_interrupt,
                mutation_state: fields.mutation_state, reconciled_request_id: fields.reconciled_request_id, reason: fields.reason,
            })
        };
        result.validate().map_err(serde::de::Error::custom)?;
        Ok(result)
    }
}
fn schedule_bounds(schedule: &CronSchedule) -> Result<(), String> {
    let value = match schedule { CronSchedule::Every { seconds } => *seconds, CronSchedule::At { at_unix_ms } => *at_unix_ms };
    if (1..=MAX_SAFE_INTEGER).contains(&value) { Ok(()) } else { Err(invalid()) }
}
fn reason_bounds(reason: &Option<String>) -> Result<(), String> {
    if reason.as_ref().is_some_and(|value| value.len() > 256 || value.contains('\0')) { Err(invalid()) } else { Ok(()) }
}
impl ResultBody {
    pub(crate) fn validate(&self) -> Result<(), String> {
        match self {
            Self::Mutation(result) => {
                reason_bounds(&result.reason)?;
                if result.job_id.as_ref().is_some_and(|id| !canonical_u64(id) || id == "0") { return Err(invalid()); }
            }
            Self::List(result) => {
                reason_bounds(&result.reason)?;
                if result.jobs.len() > 100 || (result.connected && !result.supported)
                    || matches!(result.mutation_state, Some(MutationState::Clear)) != result.reconciled_request_id.is_some() { return Err(invalid()); }
                let mut ids = std::collections::HashSet::new();
                for job in &result.jobs {
                    if !canonical_u64(&job.id) || job.id == "0" || !ids.insert(&job.id) || job.prompt_preview.chars().count() > 512
                        || job.next_run_at.is_some_and(|value| !(1..=MAX_SAFE_INTEGER).contains(&value))
                        || job.skipped.is_some_and(|value| value > MAX_SAFE_INTEGER) { return Err(invalid()); }
                    schedule_bounds(&job.schedule)?;
                }
            }
        }
        Ok(())
    }
    /// 직접 자료형 검사를 통과한 뒤에만 기존 화면의 bounded_cron_snapshot 경계로 보낸다.
    pub(crate) fn into_value(self) -> Result<Value, String> {
        self.validate()?;
        serde_json::to_value(self).map_err(|_| invalid())
    }
}
string_enum!(ErrorCode { InvalidRequest => "invalidRequest", StaleTarget => "staleTarget", RequestTimeout => "requestTimeout", Internal => "internal" });
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ResponseError { pub(crate) code: ErrorCode }
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ResponseFields {
    v: VersionOne,
    #[serde(rename = "type")] kind: ResponseType,
    id: UuidText,
    owner_instance_id: UuidText,
    #[serde(deserialize_with = "nullable")] scope: Option<Scope>,
    #[serde(default, deserialize_with = "present")] result: Option<ResultBody>,
    #[serde(default, deserialize_with = "present")] error: Option<ResponseError>,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Response {
    pub(crate) v: VersionOne,
    #[serde(rename = "type")] pub(crate) kind: ResponseType,
    pub(crate) id: UuidText,
    pub(crate) owner_instance_id: UuidText,
    pub(crate) scope: Option<Scope>,
    #[serde(skip_serializing_if = "Option::is_none")] pub(crate) result: Option<ResultBody>,
    #[serde(skip_serializing_if = "Option::is_none")] pub(crate) error: Option<ResponseError>,
}
impl<'de> Deserialize<'de> for Response {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let fields = ResponseFields::deserialize(deserializer)?;
        if fields.result.is_some() == fields.error.is_some() { return Err(serde::de::Error::custom(invalid())); }
        Ok(Self { v: fields.v, kind: fields.kind, id: fields.id, owner_instance_id: fields.owner_instance_id,
            scope: fields.scope, result: fields.result, error: fields.error })
    }
}
impl Response {
    pub(crate) fn validate_for(&self, request: &Request) -> Result<(), String> {
        if self.id != request.id || self.owner_instance_id != request.owner_instance_id { return Err(denied()); }
        if let Some(scope) = &request.scope {
            if self.scope.as_ref() != Some(scope) { return Err(denied()); }
        }
        if self.scope.as_ref().is_some_and(|scope| scope.thread_id != request.target_thread_id) { return Err(denied()); }
        if let Some(result) = &self.result {
            match (&request.body, result) {
                (CronRequest::List {}, ResultBody::List(list)) => {
                    if let Some(id) = &list.reconciled_request_id {
                        if self.scope.is_none() || request.reconcile.as_ref().and_then(|value| mutation_id(&value.request)) != Some(id.as_str()) { return Err(denied()); }
                    }
                }
                (CronRequest::List {}, _) | (_, ResultBody::List(_)) => return Err(invalid()),
                _ => {}
            }
        }
        Ok(())
    }
}

/// 원문 JSON을 직접 역직렬화한다. 오류에 입력이나 비밀을 섞지 않는다.
pub(crate) fn from_json<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, String> {
    if bytes.is_empty() || bytes.len() > MAX_FRAME_BYTES || bytes.starts_with(&[0xef, 0xbb, 0xbf]) { return Err(invalid()); }
    serde_json::from_slice(bytes).map_err(|_| invalid())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    const SESSION: &str = "11111111-2222-4333-8444-555555555555";
    const OWNER: &str = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";
    const THREAD: &str = "12345678-1234-4567-8123-123456789abc";
    const REQUEST: &str = "87654321-4321-4765-8321-cba987654321";
    fn bootstrap() -> Bootstrap {
        Bootstrap::new(&format!("{PIPE_PREFIX}0123456789abcdef0123456789abcdef"), 42, u64::MAX, SESSION, 7).unwrap()
    }
    fn hello() -> Hello {
        Hello { v: VersionOne, kind: HelloType::Value, session_id: UuidText::parse(SESSION).unwrap(),
            session_generation: DecimalU64::new(7), owner_instance_id: UuidText::parse(OWNER).unwrap(),
            client_pid: Pid::new(99).unwrap(), client_created_filetime: DecimalU64::new(123),
            client_nonce: NonceHex("0123456789abcdef0123456789abcdef".into()) }
    }
    fn scope() -> Scope {
        Scope { backend_id: UuidText::parse(SESSION).unwrap(), backend_epoch: DecimalU64::new(1), thread_id: UuidText::parse(THREAD).unwrap() }
    }
    fn request(body: CronRequest) -> Request {
        Request { v: VersionOne, kind: RequestType::Value, id: UuidText::parse(REQUEST).unwrap(),
            owner_instance_id: UuidText::parse(OWNER).unwrap(), target_thread_id: UuidText::parse(THREAD).unwrap(),
            scope: Some(scope()), body, reconcile: None }
    }
    fn list_result() -> Value {
        json!({"supported":true,"connected":true,"scope":"cliLifetime","jobs":[{
            "id":"1","promptPreview":"예약 미리보기","schedule":{"type":"at","atUnixMs":1},
            "state":"finished","paused":false,"removed":false,"nextRunAt":null,"skipped":0
        }],"hasMore":false})
    }
    fn response(result: Value) -> Value {
        json!({"v":1,"type":"response","id":REQUEST,"ownerInstanceId":OWNER,"scope":scope(),"result":result})
    }
    fn decode<T: DeserializeOwned>(value: &Value) -> Result<T, String> { from_json(&serde_json::to_vec(value).unwrap()) }

    #[test]
    fn strict_bootstrap_strings_numbers_and_names_are_not_coerced() {
        let valid = serde_json::to_value(bootstrap()).unwrap();
        assert!(decode::<Bootstrap>(&valid).is_ok());
        for (field, wrong) in [
            ("v", json!(1.0)), ("v", json!("1")), ("v", json!(2)),
            ("hostPid", json!(0)), ("hostPid", json!(1.0)), ("hostPid", json!("42")), ("hostPid", json!(4_294_967_296u64)),
            ("hostCreatedFiletime", json!("01")), ("hostCreatedFiletime", json!("18446744073709551616")),
            ("hostCreatedFiletime", json!(42)), ("sessionGeneration", json!("+1")), ("sessionGeneration", json!("1\n")),
            ("sessionId", json!(SESSION.replace('a', "A").replace('1', "G"))),
            ("sessionId", json!("11111111222243338444555555555555")),
            ("pipe", json!(r"\\remote\pipe\OpenDIY-TuiControl-0123456789abcdef0123456789abcdef")),
            ("pipe", json!(format!("{PIPE_PREFIX}0123456789ABCDEF0123456789abcdef"))),
        ] {
            let mut value = valid.clone(); value[field] = wrong;
            assert!(decode::<Bootstrap>(&value).is_err(), "잘못된 bootstrap 필드: {field}");
        }
        for value in ["0", "18446744073709551615"] { assert!(DecimalU64::try_from(value.to_owned()).is_ok()); }
        assert!(UuidText::parse("AAAAAAAA-BBBB-4CCC-8DDD-EEEEEEEEEEEE").is_err());
        assert!(UuidText::parse(OWNER).is_ok());
    }

    #[test]
    fn direct_deserialize_rejects_duplicate_unknown_null_and_non_json_bytes() {
        let original = serde_json::to_string(&hello()).unwrap();
        for bytes in [original.replacen("\"v\":1", "\"v\":1,\"v\":1", 1).into_bytes(),
            original.replacen("\"clientPid\":99", "\"clientPid\":99,\"clientPid\":99", 1).into_bytes(),
            original.replacen("\"v\":1", "\"v\":1,\"extra\":0", 1).into_bytes(),
            original.replacen("\"type\":\"hello\"", "\"type\":\"auth\"", 1).into_bytes(),
            [vec![0xef, 0xbb, 0xbf], original.as_bytes().to_vec()].concat(), vec![0xff], vec![], vec![b' '; MAX_FRAME_BYTES + 1],
        ] { assert!(from_json::<Hello>(&bytes).is_err()); }
        let mut valid = serde_json::to_value(hello()).unwrap();
        for (field, wrong) in [("clientNonce", json!("A".repeat(32))), ("clientNonce", json!("a".repeat(31))), ("clientNonce", Value::Null)] {
            valid[field] = wrong; assert!(decode::<Hello>(&valid).is_err());
        }
        assert!(decode::<Auth>(&json!({"v":1,"type":"auth","proof":"0".repeat(64),"token":"인증 비밀 표식"})).is_err());
        assert!(from_json::<Auth>(br#"{"v":1,"type":"auth","proof":"secret sentinel"}"#).err().unwrap().contains("sentinel") == false);
    }

    #[test]
    fn cng_hmac_matches_published_vector_and_transcript_has_exact_lf_order() {
        let mac = hmac_sha256(&[0x0b; 20], &[b"Hi There"]).unwrap();
        assert_eq!(encode_hex(&mac), "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7");
        let nonce = NonceHex("fedcba9876543210fedcba9876543210".into());
        let data = transcript(&bootstrap(), &hello(), &nonce);
        assert_eq!(String::from_utf8(data.clone()).unwrap(), format!(
            "desktop-cron/pipe-v1\n{SESSION}\n7\n42\n18446744073709551615\n{OWNER}\n99\n123\n0123456789abcdef0123456789abcdef\nfedcba9876543210fedcba9876543210\n"));
        // Python·Node 표준 구현으로 별도 계산한 합성 시험값이다. 운영 자격이 아니다.
        assert_eq!(encode_hex(&hmac_sha256(&[1; 32], &[b"desktop\n", &data]).unwrap()),
            "489d9ff0363b9e57d0c56f6cd896bf72950f2990214236aad9dab4af41aee059");
        assert_eq!(encode_hex(&hmac_sha256(&[1; 32], &[b"tui\n", &data]).unwrap()),
            "237a786ed717c860cf61a5ab25289403d012c1b565a322a008141d51f7751679");
    }

    #[test]
    fn one_shot_handshake_binds_actual_process_session_generation_and_both_nonces() {
        for (pid, created, generation) in [(98, 123, 7), (99, 122, 7), (99, 123, 8)] {
            let mut incoming = hello(); incoming.session_generation = DecimalU64::new(generation);
            assert!(ServerHandshake::begin(SecretToken([7; 32]), &bootstrap(), incoming, pid, created).is_err());
        }
        let (state, challenge) = ServerHandshake::begin(SecretToken([7; 32]), &bootstrap(), hello(), 99, 123).unwrap();
        let data = transcript(&bootstrap(), &hello(), &challenge.server_nonce);
        assert_eq!(challenge.proof.as_str(), encode_hex(&hmac_sha256(&[7; 32], &[b"desktop\n", &data]).unwrap()));
        let proof = MacHex(encode_hex(&hmac_sha256(&[7; 32], &[b"tui\n", &data]).unwrap()));
        let ready = state.finish(Auth { v: VersionOne, kind: AuthType::Value, proof }).unwrap();
        assert_eq!(ready.owner_instance_id.as_str(), OWNER);
        let (state, challenge) = ServerHandshake::begin(SecretToken([7; 32]), &bootstrap(), hello(), 99, 123).unwrap();
        // 상대 역할의 증명 반사를 허용하지 않는다.
        assert!(state.finish(Auth { v: VersionOne, kind: AuthType::Value, proof: challenge.proof }).is_err());
        let (state, _) = ServerHandshake::begin(SecretToken([7; 32]), &bootstrap(), hello(), 99, 123).unwrap();
        assert!(state.finish(Auth { v: VersionOne, kind: AuthType::Value, proof: MacHex("0".repeat(64)) }).is_err());
        let mut left = [0; 32];
        for index in 0..32 { left[index] = 1; assert!(!constant_time_equal(&left, &[0; 32])); left[index] = 0; }
        assert!(constant_time_equal(&left, &[0; 32]));
    }

    #[test]
    fn secret_encoding_and_nonce_sizes_use_os_rng_and_explicit_wipe() {
        let secret = SecretToken::generate().unwrap();
        let encoded = secret.expose_hex();
        assert!(lower_hex(encoded.as_str(), 64));
        let mut bytes = [7; 32]; wipe(&mut bytes); assert_eq!(bytes, [0; 32]);
        let (_, challenge) = ServerHandshake::begin(secret, &bootstrap(), hello(), 99, 123).unwrap();
        assert!(lower_hex(challenge.server_nonce.as_str(), 32));
    }

    #[test]
    fn request_scope_identity_and_reconcile_keep_the_original_mutation() {
        let mut list = request(CronRequest::List {}); list.scope = None; assert!(list.validate().is_ok());
        let mut mutation = request(CronRequest::Pause { request_id: REQUEST.into(), job_id: "1".into() });
        assert!(mutation.validate().is_ok()); mutation.scope = None; assert!(mutation.validate().is_err());
        let mut conflict = request(CronRequest::Pause { request_id: OWNER.into(), job_id: "1".into() });
        assert!(conflict.validate().is_err()); conflict.body = CronRequest::Pause { request_id: REQUEST.to_uppercase(), job_id: "1".into() };
        assert!(conflict.validate().is_err());
        let mut reconciled = request(CronRequest::List {});
        reconciled.reconcile = Some(Reconcile { request: CronRequest::Add { request_id: REQUEST.into(), schedule: CronSchedule::At { at_unix_ms: 1 }, prompt: "이전 단발 원문\n".into() } });
        assert!(reconciled.validate().is_ok()); reconciled.scope.as_mut().unwrap().thread_id = UuidText::parse(OWNER).unwrap();
        assert!(reconciled.validate().is_err());
        let mut invalid = serde_json::to_value(request(CronRequest::List {})).unwrap();
        invalid["reconcile"] = Value::Null; assert!(decode::<Request>(&invalid).is_err());
        invalid.as_object_mut().unwrap().remove("reconcile"); invalid.as_object_mut().unwrap().remove("scope");
        assert!(decode::<Request>(&invalid).is_err());
        let text = serde_json::to_string(&request(CronRequest::Pause { request_id: REQUEST.into(), job_id: "1".into() })).unwrap();
        assert!(from_json::<Request>(text.replacen("\"jobId\":\"1\"", "\"jobId\":\"1\",\"jobId\":\"1\"", 1).as_bytes()).is_err());
    }

    #[test]
    fn typed_results_reject_full_prompt_duplicate_keys_and_mixed_shapes_before_value_conversion() {
        let valid = response(list_result()); assert!(decode::<Response>(&valid).is_ok());
        let text = serde_json::to_string(&valid).unwrap();
        for broken in [
            text.replacen("\"supported\":true", "\"supported\":true,\"supported\":true", 1),
            text.replacen("\"id\":\"1\"", "\"id\":\"1\",\"id\":\"1\"", 1),
            text.replacen("\"atUnixMs\":1", "\"atUnixMs\":1,\"atUnixMs\":1", 1),
            text.replacen("\"promptPreview\":", "\"prompt\":\"유출 금지 원문\",\"promptPreview\":", 1),
        ] { assert!(from_json::<Response>(broken.as_bytes()).is_err()); }
        for extra in [json!({"status":"applied"}), json!({"prompt":"유출 금지 원문"}), json!({"jobId":"1"})] {
            let mut result = list_result(); result.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            assert!(decode::<Response>(&response(result)).is_err());
        }
        for result in [json!({"status":"applied","reason":null}), json!({"status":"applied","jobId":"0"}),
            json!({"status":"applied","jobs":[]}), json!({"status":"applied","reason":"x".repeat(257)}),
            json!({"status":"applied","reason":"오류\0문자"}), json!({"status":"새 상태"})] {
            assert!(decode::<Response>(&response(result)).is_err());
        }
        let mut both = response(list_result()); both["error"] = json!({"code":"internal"}); assert!(decode::<Response>(&both).is_err());
        both.as_object_mut().unwrap().remove("result"); assert!(decode::<Response>(&both).is_ok());
        both["error"] = Value::Null; assert!(decode::<Response>(&both).is_err());
    }

    #[test]
    fn list_bounds_and_correlations_are_checked_at_the_typed_boundary() {
        for (field, wrong) in [("supported", json!(false)), ("hasMore", Value::Null), ("reason", json!("x".repeat(257))),
            ("reconciledRequestId", json!(REQUEST)), ("mutationState", json!("clear"))] {
            let mut result = list_result(); result[field] = wrong; assert!(decode::<Response>(&response(result)).is_err());
        }
        for (field, wrong) in [("id", json!("01")), ("promptPreview", json!("가".repeat(513))),
            ("nextRunAt", json!(MAX_SAFE_INTEGER + 1)), ("skipped", Value::Null), ("skipped", json!(MAX_SAFE_INTEGER + 1))] {
            let mut result = list_result(); result["jobs"][0][field] = wrong; assert!(decode::<Response>(&response(result)).is_err());
        }
        let mut repeated = list_result(); repeated["jobs"] = json!([repeated["jobs"][0].clone(), repeated["jobs"][0].clone()]);
        assert!(decode::<Response>(&response(repeated)).is_err());
        let mut many = list_result(); many["jobs"] = Value::Array((1..=101).map(|index| { let mut job = many["jobs"][0].clone(); job["id"] = json!(index.to_string()); job }).collect());
        assert!(decode::<Response>(&response(many)).is_err());
        let list = request(CronRequest::List {});
        let good: Response = decode(&response(list_result())).unwrap(); assert!(good.validate_for(&list).is_ok());
        for (field, wrong) in [("id", json!(OWNER)), ("ownerInstanceId", json!(SESSION)), ("scope", json!(null))] {
            let mut value = response(list_result()); value[field] = wrong; let value: Response = decode(&value).unwrap(); assert!(value.validate_for(&list).is_err());
        }
        let wrong_kind: Response = decode(&response(json!({"status":"applied"}))).unwrap(); assert!(wrong_kind.validate_for(&list).is_err());
        assert_eq!(good.result.unwrap().into_value().unwrap()["jobs"][0]["promptPreview"], "예약 미리보기");
    }
    #[test]
    fn interrupt_capability_requires_an_actual_boolean_and_stays_inside_the_list() {
        for capability in [json!(false), json!(true)] {
            let mut list = list_result(); list["supportsExpectedTurnInterrupt"] = capability.clone();
            let decoded: Response = decode(&response(list)).unwrap();
            assert_eq!(decoded.result.unwrap().into_value().unwrap()["supportsExpectedTurnInterrupt"], capability);
        }
        for capability in [Value::Null, json!("true"), json!(1), json!({}), json!([])] {
            let mut list = list_result(); list["supportsExpectedTurnInterrupt"] = capability;
            assert!(decode::<Response>(&response(list)).is_err());
        }
        assert!(decode::<Response>(&response(json!({"status":"applied","supportsExpectedTurnInterrupt":true}))).is_err());
        let mut list = list_result(); list["supportsExpectedTurnInterrupt"] = json!(true);
        let text = serde_json::to_string(&response(list)).unwrap();
        assert!(from_json::<Response>(text.replacen("\"supportsExpectedTurnInterrupt\":true", "\"supportsExpectedTurnInterrupt\":true,\"supportsExpectedTurnInterrupt\":false", 1).as_bytes()).is_err());
    }

    #[test]
    fn protocol_discriminants_accept_strings_only_not_externally_tagged_objects() {
        let mut incoming = serde_json::to_value(hello()).unwrap(); incoming["type"] = json!({"hello":null});
        assert!(decode::<Hello>(&incoming).is_err());
        assert!(decode::<Response>(&response(json!({"status":{"applied":null}}))).is_err());
        let mut list = list_result(); list["scope"] = json!({"cliLifetime":null});
        assert!(decode::<Response>(&response(list)).is_err());
        let mut list = list_result(); list["jobs"][0]["state"] = json!({"finished":null});
        assert!(decode::<Response>(&response(list)).is_err());
        let mut error = response(list_result()); error.as_object_mut().unwrap().remove("result"); error["error"] = json!({"code":{"internal":null}});
        assert!(decode::<Response>(&error).is_err());
    }

    #[test]
    fn fixture_client_auth_rejects_noncanonical_secret_and_unverified_desktop_proof() {
        let (state, mut challenge) = ServerHandshake::begin(SecretToken([7; 32]), &bootstrap(), hello(), 99, 123).unwrap();
        for invalid_token in [String::new(), "0".repeat(63), "0".repeat(65), "A".repeat(64), "g".repeat(64)] {
            assert!(fixture_client_auth(&invalid_token, &bootstrap(), &hello(), &challenge).is_err());
        }
        let encoded = SecretToken([7; 32]).expose_hex();
        let original_proof = challenge.proof.clone();
        challenge.proof = MacHex("0".repeat(64));
        assert!(fixture_client_auth(encoded.as_str(), &bootstrap(), &hello(), &challenge).is_err());
        challenge.proof = original_proof;
        let mut wrong_generation = hello(); wrong_generation.session_generation = DecimalU64::new(8);
        assert!(fixture_client_auth(encoded.as_str(), &bootstrap(), &wrong_generation, &challenge).is_err());
        let auth = fixture_client_auth(encoded.as_str(), &bootstrap(), &hello(), &challenge).unwrap();
        assert_eq!(state.finish(auth).unwrap().owner_instance_id.as_str(), OWNER);
    }

}
