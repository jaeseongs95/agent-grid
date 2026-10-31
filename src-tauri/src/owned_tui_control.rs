//! Desktop이 소유하는 단일 Windows TUI 통로. 인증·예약 정책은 상위 어댑터 책임이다.
//! 기존 Tokio 실행기 안에서 생성한다. 이 모듈은 실행기·작업·Job을 만들거나 보유하지 않는다.
#![cfg(windows)]

use crate::process_job::{OwnedProcessIdentity, ProcessIdentity, ProcessJob};
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::{
    ffi::c_void,
    future::{poll_fn, Future},
    io::{self, Write},
    os::windows::io::{AsHandle, AsRawHandle, FromRawHandle, OwnedHandle},
    pin::pin,
    task::Poll,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::windows::named_pipe::{NamedPipeServer, PipeMode, ServerOptions},
    sync::watch,
    time::{timeout_at, Instant},
};
use windows_sys::Win32::{
    Foundation::{LocalFree, ERROR_INSUFFICIENT_BUFFER, ERROR_NOT_FOUND, ERROR_PIPE_NOT_CONNECTED},
    Security::{
        Authorization::{ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1},
        GetTokenInformation, TokenUser, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
    },
    System::{IO::CancelIoEx, Threading::{GetCurrentProcess, OpenProcessToken}},
};

pub(crate) const MAX_FRAME_BYTES: usize = 1024 * 1024;
const PIPE_BUFFER_BYTES: u32 = 64 * 1024;

/// 대기 중인 자기 연결 작업을 깨운다. idle 연결의 소유자는 close/drop도 수행한다.
#[derive(Clone)]
pub(crate) struct PipeCancellation(watch::Sender<bool>);
impl PipeCancellation {
    pub(crate) fn cancel(&self) { self.0.send_replace(true); }
}

/// 처음 만든 서버 핸들 하나만 유지한다. 재연결·자동 재송신·인증 성공을 제공하지 않는다.
pub(crate) struct OwnedTuiPipe {
    name: String,
    pipe: Option<ClosingPipe>,
    connected: bool,
    peer: Option<OwnedProcessIdentity>,
    cancellation: PipeCancellation,
}

impl OwnedTuiPipe {
    /// I/O와 시간이 활성화된 기존 Tokio 실행기에서 호출한다.
    pub(crate) fn bind() -> io::Result<Self> {
        Self::bind_named(format!(r"\\.\pipe\OpenDIY-TuiControl-{}", uuid::Uuid::new_v4().simple()))
    }

    fn bind_named(name: String) -> io::Result<Self> {
        let descriptor = current_user_descriptor()?;
        let mut attributes = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.0,
            bInheritHandle: 0,
        };
        // SAFETY: 속성과 보안 설명자를 실제 CreateNamedPipeW 호출까지 보유한다.
        let server = unsafe {
            ServerOptions::new()
                .access_inbound(true)
                .access_outbound(true)
                .first_pipe_instance(true)
                .max_instances(1)
                .reject_remote_clients(true)
                .pipe_mode(PipeMode::Byte)
                .in_buffer_size(PIPE_BUFFER_BYTES)
                .out_buffer_size(PIPE_BUFFER_BYTES)
                .create_with_security_attributes_raw(&name, (&mut attributes as *mut SECURITY_ATTRIBUTES).cast())?
        };
        let (sender, _) = watch::channel(false);
        Ok(Self {
            name, pipe: Some(ClosingPipe { server, aborted: false }), connected: false,
            peer: None, cancellation: PipeCancellation(sender),
        })
    }

    pub(crate) fn name(&self) -> &str { &self.name }
    pub(crate) fn cancellation(&self) -> PipeCancellation { self.cancellation.clone() }

    /// 취소·기한 초과·future drop 뒤 같은 인스턴스로 다시 accept하지 않는다.
    pub(crate) async fn accept(&mut self, deadline: Instant) -> io::Result<()> {
        if self.connected { return Err(io::Error::new(io::ErrorKind::AlreadyExists, "통로에 이미 연결됐습니다.")); }
        let mut pipe = self.take_pipe()?;
        let result = bounded(&self.cancellation, deadline, pipe.server.connect()).await;
        match result {
            Ok(()) => { self.connected = true; self.pipe = Some(pipe); Ok(()) }
            Err(error) => Err(cleanup_error(error, pipe.abort())),
        }
    }

    /// 기다리지 않는 OS 소유 확인이다. 상위 인증 handshake를 완료했다는 뜻은 아니다.
    /// 호출자는 Job 잠금 안에서 이 메서드만 호출하고 frame I/O 전에 잠금을 푼다.
    pub(crate) fn verify_peer(&mut self, job: &ProcessJob) -> io::Result<ProcessIdentity> {
        if !self.connected { return Err(not_connected()); }
        let result = (|| {
            let pipe = self.pipe.as_ref().ok_or_else(not_connected)?;
            let peer = job.verify_pipe_client(pipe.server.as_handle())?;
            let identity = peer.identity();
            if self.peer.as_ref().is_some_and(|old| old.identity() != identity) {
                return Err(io::Error::new(io::ErrorKind::PermissionDenied, "통로의 프로세스 생성 세대가 바뀌었습니다."));
            }
            self.peer = Some(peer);
            Ok(identity)
        })();
        match result {
            Ok(identity) => Ok(identity),
            Err(error) => Err(cleanup_error(error, self.close())),
        }
    }

    /// 원문을 상위 wire 타입으로 직접 읽는다. 중복·미지 필드는 그 타입의
    /// deny_unknown_fields 등으로 거절한다. Value를 거친 재변환은 사용하지 않는다.
    pub(crate) async fn read_frame<T: DeserializeOwned>(&mut self, deadline: Instant) -> io::Result<T> {
        self.ensure_connected()?;
        let mut pipe = self.take_pipe()?;
        let result = bounded(&self.cancellation, deadline, async {
            let mut prefix = [0; 4];
            pipe.server.read_exact(&mut prefix).await?;
            let length = u32::from_le_bytes(prefix) as usize;
            if length == 0 || length > MAX_FRAME_BYTES {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "통로 frame 길이가 허용 범위를 벗어났습니다."));
            }
            let mut bytes = vec![0; length];
            pipe.server.read_exact(&mut bytes).await?;
            serde_json::from_slice(&bytes).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "통로 frame이 지정한 UTF-8 JSON 형식에 맞지 않습니다."))
        }).await;
        match result {
            Ok(value) => { self.pipe = Some(pipe); Ok(value) }
            Err(error) => Err(cleanup_error(error, pipe.abort())),
        }
    }

    /// 성공은 한정된 로컬 버퍼 접수다. 상대 수신·적용·인증 완료를 뜻하지 않는다.
    /// 마지막 쓰기의 OS 완료는 Tokio/mio가 소유하며 close에서 미완료 쓰기도 취소한다.
    pub(crate) async fn write_frame(&mut self, value: &Value, deadline: Instant) -> io::Result<()> {
        self.ensure_connected()?;
        let mut bytes = LimitedFrame(Vec::new());
        serde_json::to_writer(&mut bytes, value)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "직렬화된 통로 frame이 허용 크기를 넘었습니다."))?;
        let mut pipe = self.take_pipe()?;
        let result = bounded(&self.cancellation, deadline, async {
            pipe.server.write_all(&(bytes.0.len() as u32).to_le_bytes()).await?;
            pipe.server.write_all(&bytes.0).await
        }).await;
        match result {
            Ok(()) => { self.pipe = Some(pipe); Ok(()) }
            Err(error) => Err(cleanup_error(error, pipe.abort())),
        }
    }

    /// 신규 대기를 막고 자기 I/O만 취소·연결 해제한다. 외부 프로세스는 종료하지 않는다.
    /// 완료 버퍼·핸들의 최종 회수는 살아 있는 기존 Tokio IOCP 실행기가 처리한다.
    pub(crate) fn close(&mut self) -> io::Result<()> {
        self.cancellation.cancel();
        self.connected = false;
        self.peer = None;
        self.pipe.take().map(|mut pipe| pipe.abort()).unwrap_or(Ok(()))
    }

    fn take_pipe(&mut self) -> io::Result<ClosingPipe> {
        self.pipe.take().ok_or_else(not_connected)
    }

    fn ensure_connected(&mut self) -> io::Result<()> {
        if !self.connected || self.pipe.is_none() { return Err(not_connected()); }
        if let Some(error) = self.peer.as_ref().and_then(|peer| peer.ensure_running().err()) {
            return Err(cleanup_error(error, self.close()));
        }
        Ok(())
    }
}

impl Drop for OwnedTuiPipe {
    fn drop(&mut self) { let _ = self.close(); }
}

struct ClosingPipe {
    server: NamedPipeServer,
    aborted: bool,
}
impl ClosingPipe {
    fn abort(&mut self) -> io::Result<()> {
        if self.aborted { return Ok(()); }
        self.aborted = true;
        // SAFETY: 자기 핸들의 모든 pending I/O만 취소한다. OVERLAPPED·버퍼는
        // mio의 IOCP 소유로 유지되며 완료 통지 전에 수동 해제하지 않는다.
        let cancelled = unsafe { CancelIoEx(self.server.as_raw_handle(), std::ptr::null()) };
        let cancel_error = if cancelled == 0 {
            let error = io::Error::last_os_error();
            (error.raw_os_error() != Some(ERROR_NOT_FOUND as i32)).then_some(error)
        } else { None };
        // mio Drop은 쓰기를 자동 취소하지 않는다. disconnect도 수행하여
        // 일부 완료된 쓰기가 잔여 쓰기를 다시 예약하더라도 더 진행하지 못하게 한다.
        let disconnected = self.server.disconnect();
        let disconnect_error = disconnected.err().filter(|error| error.raw_os_error() != Some(ERROR_PIPE_NOT_CONNECTED as i32));
        match (cancel_error, disconnect_error) {
            (Some(error), other) => Err(cleanup_error(error, other.map(Err).unwrap_or(Ok(())))),
            (None, Some(error)) => Err(error),
            (None, None) => Ok(()),
        }
    }
}
impl Drop for ClosingPipe {
    fn drop(&mut self) { let _ = self.abort(); }
}

async fn bounded<F, T>(cancellation: &PipeCancellation, deadline: Instant, operation: F) -> io::Result<T>
where F: Future<Output = io::Result<T>> {
    let mut receiver = cancellation.0.subscribe();
    let cancelled = async {
        loop {
            if *receiver.borrow_and_update() { return; }
            if receiver.changed().await.is_err() { return; }
        }
    };
    let mut cancelled = pin!(cancelled);
    let mut operation = pin!(timeout_at(deadline, operation));
    poll_fn(|context| {
        if cancelled.as_mut().poll(context).is_ready() {
            return Poll::Ready(Err(io::Error::new(io::ErrorKind::Interrupted, "통로 대기를 취소했습니다.")));
        }
        if Instant::now() >= deadline {
            return Poll::Ready(Err(io::Error::new(io::ErrorKind::TimedOut, "통로 대기 기한을 넘었습니다.")));
        }
        match operation.as_mut().poll(context) {
            Poll::Ready(Ok(result)) => Poll::Ready(result),
            Poll::Ready(Err(_)) => Poll::Ready(Err(io::Error::new(io::ErrorKind::TimedOut, "통로 대기 기한을 넘었습니다."))),
            Poll::Pending => Poll::Pending,
        }
    }).await
}

struct LimitedFrame(Vec<u8>);
impl Write for LimitedFrame {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > MAX_FRAME_BYTES.saturating_sub(self.0.len()) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "통로 frame 크기를 넘었습니다."));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> { Ok(()) }
}

fn not_connected() -> io::Error { io::Error::new(io::ErrorKind::NotConnected, "통로가 닫혔거나 아직 연결되지 않았습니다.") }
fn cleanup_error(error: io::Error, cleanup: io::Result<()>) -> io::Error {
    match cleanup {
        Ok(()) => error,
        Err(cleanup) => io::Error::new(error.kind(), format!("{error}; 통로 정리 실패: {cleanup}")),
    }
}

struct LocalMemory(*mut c_void);
impl Drop for LocalMemory {
    fn drop(&mut self) {
        // SAFETY: Windows가 LocalAlloc 계열로 반환한 단일 소유 메모리다.
        unsafe { LocalFree(self.0); }
    }
}

fn current_user_sid() -> io::Result<String> {
    let mut raw_token = std::ptr::null_mut();
    // SAFETY: 현재 프로세스 토큰을 조회 전용으로 열고 성공 때만 소유한다.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw_token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let token = unsafe { OwnedHandle::from_raw_handle(raw_token) };
    let mut required = 0;
    let sized = unsafe { GetTokenInformation(token.as_raw_handle(), TokenUser, std::ptr::null_mut(), 0, &mut required) };
    if sized != 0 || io::Error::last_os_error().raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER as i32)
        || required < std::mem::size_of::<TOKEN_USER>() as u32 || required > 64 * 1024 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "사용자 토큰 크기를 확인할 수 없습니다."));
    }
    // TOKEN_USER와 그 안의 SID 포인터에 충분한 정렬을 보장한다.
    let mut buffer = vec![0usize; (required as usize).div_ceil(std::mem::size_of::<usize>())];
    if unsafe { GetTokenInformation(token.as_raw_handle(), TokenUser, buffer.as_mut_ptr().cast(), required, &mut required) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
    let mut raw_sid = std::ptr::null_mut();
    if unsafe { ConvertSidToStringSidW(user.User.Sid, &mut raw_sid) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let _sid = LocalMemory(raw_sid.cast());
    for length in 0..256 {
        if unsafe { *raw_sid.add(length) } == 0 {
            return String::from_utf16(unsafe { std::slice::from_raw_parts(raw_sid, length) })
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "사용자 SID 문자열이 올바르지 않습니다."));
        }
    }
    Err(io::Error::new(io::ErrorKind::InvalidData, "사용자 SID 문자열이 허용 길이를 넘었습니다."))
}

fn current_user_descriptor() -> io::Result<LocalMemory> {
    // 사용자 SID 하나만 허용한다. max_instances=1인 원래 서버 핸들을 끝까지
    // 유지하므로 같은 사용자에게 허용된 권한으로 두 번째 서버를 만들 수 없다.
    let sddl: Vec<u16> = format!("D:P(A;;GA;;;{})", current_user_sid()?).encode_utf16().chain(Some(0)).collect();
    let mut descriptor = std::ptr::null_mut();
    if unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl.as_ptr(), SDDL_REVISION_1, &mut descriptor, std::ptr::null_mut()) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(LocalMemory(descriptor))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{io::Read, os::windows::process::CommandExt, process::{Child, Command, Stdio}, time::Duration};
    use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient};

    const FIXTURE_PIPE: &str = "OPENDIY_OWNED_PIPE_TEST";
    fn deadline() -> Instant { Instant::now() + Duration::from_secs(5) }

    async fn pair() -> (OwnedTuiPipe, NamedPipeClient) {
        let mut pipe = OwnedTuiPipe::bind().unwrap();
        let client = ClientOptions::new().open(pipe.name()).unwrap();
        pipe.accept(deadline()).await.unwrap();
        (pipe, client)
    }

    async fn read_client(client: &mut NamedPipeClient) -> Value {
        timeout_at(deadline(), async {
            let mut size = [0; 4];
            client.read_exact(&mut size).await.unwrap();
            let length = u32::from_le_bytes(size) as usize;
            assert!(length <= MAX_FRAME_BYTES);
            let mut bytes = vec![0; length];
            client.read_exact(&mut bytes).await.unwrap();
            serde_json::from_slice(&bytes).unwrap()
        }).await.expect("시험 클라이언트 수신 기한")
    }

    async fn reclaimed(name: String) {
        let until = deadline();
        loop {
            match OwnedTuiPipe::bind_named(name.clone()) {
                Ok(mut replacement) => { replacement.close().unwrap(); return; }
                Err(error) if Instant::now() < until => {
                    assert!(matches!(error.raw_os_error(), Some(5 | 231)), "예상하지 않은 통로 오류: {error}");
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                Err(error) => panic!("취소된 통로의 OS 자원이 회수되지 않았습니다: {error}"),
            }
        }
    }

    #[test]
    fn owned_pipe_private_dacl_first_instance_and_noninherited_handle() {
        tauri::async_runtime::block_on(async {
            use windows_sys::Win32::{
                Foundation::GetHandleInformation,
                Security::{Authorization::{GetSecurityInfo, SE_KERNEL_OBJECT}, GetAce, ACCESS_ALLOWED_ACE, DACL_SECURITY_INFORMATION},
            };
            let mut pipe = OwnedTuiPipe::bind().unwrap();
            let server = &pipe.pipe.as_ref().unwrap().server;
            assert_eq!(server.info().unwrap().max_instances, 1);
            let mut flags = 0;
            assert_ne!(unsafe { GetHandleInformation(server.as_raw_handle(), &mut flags) }, 0);
            assert_eq!(flags & 1, 0, "서버 핸들을 자식에게 상속하면 안 됩니다.");
            let mut dacl = std::ptr::null_mut();
            let mut descriptor = std::ptr::null_mut();
            assert_eq!(unsafe {
                GetSecurityInfo(server.as_raw_handle(), SE_KERNEL_OBJECT, DACL_SECURITY_INFORMATION,
                    std::ptr::null_mut(), std::ptr::null_mut(), &mut dacl, std::ptr::null_mut(), &mut descriptor)
            }, 0);
            let _descriptor = LocalMemory(descriptor);
            assert!(!dacl.is_null());
            assert_eq!(unsafe { (*dacl).AceCount }, 1, "사용자 한 명만 허용해야 합니다.");
            let mut ace = std::ptr::null_mut();
            assert_ne!(unsafe { GetAce(dacl, 0, &mut ace) }, 0);
            let ace = unsafe { &*ace.cast::<ACCESS_ALLOWED_ACE>() };
            assert_eq!(ace.Header.AceType, 0, "명시적인 허용 항목이어야 합니다.");
            let mut sid = std::ptr::null_mut();
            assert_ne!(unsafe { ConvertSidToStringSidW((&ace.SidStart as *const u32).cast_mut().cast(), &mut sid) }, 0);
            let _sid = LocalMemory(sid.cast());
            let length = (0..256).find(|&index| unsafe { *sid.add(index) } == 0).unwrap();
            let actual = String::from_utf16(unsafe { std::slice::from_raw_parts(sid, length) }).unwrap();
            assert_eq!(actual, current_user_sid().unwrap());
            assert!(OwnedTuiPipe::bind_named(pipe.name().to_owned()).is_err(), "두 번째 서버가 생기면 안 됩니다.");
            let name = pipe.name().to_owned();
            pipe.close().unwrap();
            reclaimed(name).await;
        });
    }

    #[test]
    fn owned_pipe_frames_split_prefix_utf8_and_exact_serialized_limit() {
        tauri::async_runtime::block_on(async {
            let (mut pipe, mut client) = pair().await;
            let expected = serde_json::json!({"text":"가", "number":7});
            let body = serde_json::to_vec(&expected).unwrap();
            let mut wire = (body.len() as u32).to_le_bytes().to_vec();
            wire.extend(body);
            let writer = tokio::spawn(async move {
                for chunk in wire.chunks(1) {
                    client.write_all(chunk).await.unwrap();
                    tokio::task::yield_now().await;
                }
                client
            });
            assert_eq!(pipe.read_frame::<Value>(deadline()).await.unwrap(), expected);
            let mut client = writer.await.unwrap();
            let exact = Value::String("x".repeat(MAX_FRAME_BYTES - 2));
            pipe.write_frame(&exact, deadline()).await.unwrap();
            assert_eq!(read_client(&mut client).await, exact);
            let escaped = Value::String("\u{0001}".repeat(MAX_FRAME_BYTES / 6 + 1));
            assert!(serde_json::to_vec(&escaped).unwrap().len() > MAX_FRAME_BYTES, "시험 입력은 이스케이프 후 실제 상한을 넘어야 합니다.");
            assert_eq!(pipe.write_frame(&escaped, deadline()).await.unwrap_err().kind(), io::ErrorKind::InvalidInput);
            let small = serde_json::json!({"ok":true});
            pipe.write_frame(&small, deadline()).await.unwrap();
            assert_eq!(read_client(&mut client).await, small, "송신 전 크기 거절은 정상 frame을 훼손하면 안 됩니다.");
            pipe.close().unwrap();
        });
    }

    #[test]
    fn owned_pipe_typed_frame_rejects_duplicate_nested_duplicate_and_unknown_fields() {
        #[derive(Debug, serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct WireFrame { request: WireRequest }
        #[derive(Debug, serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct WireRequest { id: String }

        tauri::async_runtime::block_on(async {
            for body in [
                r#"{"request":{"id":"1"},"request":{"id":"2"}}"#,
                r#"{"request":{"id":"1","id":"2"}}"#,
                r#"{"request":{"id":"1"},"unexpected":true}"#,
                r#"{"request":{"id":"1","unexpected":true}}"#,
            ] {
                let (mut pipe, mut client) = pair().await;
                client.write_all(&(body.len() as u32).to_le_bytes()).await.unwrap();
                client.write_all(body.as_bytes()).await.unwrap();
                assert_eq!(pipe.read_frame::<WireFrame>(deadline()).await.unwrap_err().kind(), io::ErrorKind::InvalidData);
                assert_eq!(pipe.read_frame::<WireFrame>(deadline()).await.unwrap_err().kind(), io::ErrorKind::NotConnected);
            }
            let (mut pipe, mut client) = pair().await;
            let body = br#"{"request":{"id":"valid"}}"#;
            client.write_all(&(body.len() as u32).to_le_bytes()).await.unwrap();
            client.write_all(body).await.unwrap();
            assert_eq!(pipe.read_frame::<WireFrame>(deadline()).await.unwrap().request.id, "valid");
            pipe.close().unwrap();
        });
    }

    #[test]
    fn owned_pipe_rejects_oversize_zero_invalid_utf8_and_truncation_without_reuse() {
        tauri::async_runtime::block_on(async {
            for wire in [
                (MAX_FRAME_BYTES as u32 + 1).to_le_bytes().to_vec(),
                0u32.to_le_bytes().to_vec(),
                vec![1, 0, 0, 0, 255],
            ] {
                let (mut pipe, mut client) = pair().await;
                client.write_all(&wire).await.unwrap();
                assert_eq!(pipe.read_frame::<Value>(deadline()).await.unwrap_err().kind(), io::ErrorKind::InvalidData);
                assert_eq!(pipe.read_frame::<Value>(deadline()).await.unwrap_err().kind(), io::ErrorKind::NotConnected);
            }
            let (mut pipe, mut client) = pair().await;
            client.write_all(&[10, 0, 0, 0, b'{']).await.unwrap();
            // 실제 전송이 완료되도록 서버에서 prefix·본문 일부를 받은 뒤 취소한다.
            let until = Instant::now() + Duration::from_millis(30);
            assert_eq!(pipe.read_frame::<Value>(until).await.unwrap_err().kind(), io::ErrorKind::TimedOut);
            drop(client);
            assert_eq!(pipe.write_frame(&Value::Null, deadline()).await.unwrap_err().kind(), io::ErrorKind::NotConnected);
        });
    }

    #[test]
    fn owned_pipe_cancel_deadline_future_drop_and_saturated_write_reclaim_resources() {
        tauri::async_runtime::block_on(async {
            let mut waiting = OwnedTuiPipe::bind().unwrap();
            let name = waiting.name().to_owned();
            let cancel = waiting.cancellation();
            let canceller = tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(20)).await;
                cancel.cancel();
            });
            assert_eq!(waiting.accept(deadline()).await.unwrap_err().kind(), io::ErrorKind::Interrupted);
            canceller.await.unwrap();
            reclaimed(name).await;

            let (mut reading, mut client) = pair().await;
            let name = reading.name().to_owned();
            client.write_all(&[4, 0]).await.unwrap();
            assert!(tokio::time::timeout(Duration::from_millis(20), reading.read_frame::<Value>(deadline())).await.is_err());
            assert_eq!(reading.read_frame::<Value>(deadline()).await.unwrap_err().kind(), io::ErrorKind::NotConnected);
            drop(client);
            reclaimed(name).await;

            let (mut writing, mut client) = pair().await;
            let name = writing.name().to_owned();
            writing.write_frame(&Value::String("x".repeat(MAX_FRAME_BYTES - 2)), deadline()).await.unwrap();
            assert_eq!(writing.write_frame(&Value::Null, Instant::now() + Duration::from_millis(30)).await.unwrap_err().kind(), io::ErrorKind::TimedOut);
            assert_eq!(writing.write_frame(&Value::Bool(true), deadline()).await.unwrap_err().kind(), io::ErrorKind::NotConnected);
            // 취소 전에 완료된 첫 frame의 일부는 이미 상대/커널 버퍼에 남을 수 있다.
            // 그 바이트만 한정해서 배출하고, 추가 frame 없이 실제 끊김까지 확인한다.
            let mut first_frame = (MAX_FRAME_BYTES as u32).to_le_bytes().to_vec();
            first_frame.push(b'"');
            first_frame.extend(std::iter::repeat(b'x').take(MAX_FRAME_BYTES - 2));
            first_frame.push(b'"');
            let received = timeout_at(deadline(), async {
                let mut received = 0;
                let mut buffer = [0; 8192];
                loop {
                    match client.read(&mut buffer).await {
                        Ok(0) => break,
                        Ok(count) => {
                            assert!(count <= first_frame.len() - received, "취소된 쓰기 뒤 새 frame이나 상한 밖 바이트가 전송됐습니다.");
                            assert_eq!(&buffer[..count], &first_frame[received..received + count], "이미 접수된 첫 frame의 바이트만 남을 수 있습니다.");
                            received += count;
                        }
                        Err(error) if matches!(error.kind(), io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionAborted | io::ErrorKind::ConnectionReset | io::ErrorKind::NotConnected)
                            || matches!(error.raw_os_error(), Some(109 | 232 | 233)) => break,
                        Err(error) => panic!("취소 뒤 예상하지 않은 수신 오류: {error}"),
                    }
                }
                received
            }).await.expect("취소된 쓰기는 이미 전송된 바이트 배출 뒤 기한 안에 끊겨야 합니다.");
            assert!(received <= MAX_FRAME_BYTES + 4, "총 수신량은 처음 접수한 frame 하나를 넘으면 안 됩니다.");
            drop(client);
            reclaimed(name).await;

            let (mut expired, mut client) = pair().await;
            assert_eq!(expired.write_frame(&Value::Null, Instant::now()).await.unwrap_err().kind(), io::ErrorKind::TimedOut);
            let mut byte = [0];
            let result = timeout_at(deadline(), client.read(&mut byte)).await.unwrap();
            assert!(matches!(result, Ok(0) | Err(_)), "기한이 지난 송신은 바이트를 보내면 안 됩니다.");
        });
    }

    struct Fixture {
        child: Child,
        job: Option<ProcessJob>,
    }
    impl Fixture {
        fn launch(name: &str) -> Self {
            let mut child = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "owned_tui_control::tests::pipe_fixture_parent", "--nocapture"])
                .env(FIXTURE_PIPE, name).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null())
                .creation_flags(0x08000000).spawn().unwrap();
            let job = match ProcessJob::assign_std(&child) {
                Ok(job) => job,
                Err(error) => { let _ = child.kill(); let _ = child.wait(); panic!("시험 Job 배정 실패: {error}"); }
            };
            child.stdin.take().unwrap().write_all(b"1").unwrap();
            Self { child, job: Some(job) }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            if let Some(job) = &self.job { let _ = job.terminate(); }
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    #[test]
    fn owned_pipe_actual_job_descendant_identity_and_other_job_rejection() {
        tauri::async_runtime::block_on(async {
            let mut first = OwnedTuiPipe::bind().unwrap();
            let mut first_fixture = Fixture::launch(first.name());
            first.accept(deadline()).await.unwrap();
            let identity = first.verify_peer(first_fixture.job.as_ref().unwrap()).unwrap();
            assert_ne!(identity.pid, first_fixture.child.id(), "연결 상대는 도우미가 아닌 실제 자손이어야 합니다.");
            assert_ne!(identity.created_filetime, 0);
            let hello = first.read_frame::<Value>(deadline()).await.unwrap();
            assert_eq!(hello["pid"], identity.pid);
            assert_eq!(hello["createdFiletime"], identity.created_filetime.to_string());
            assert_eq!(first.verify_peer(first_fixture.job.as_ref().unwrap()).unwrap(), identity);

            let mut second = OwnedTuiPipe::bind().unwrap();
            let _second_fixture = Fixture::launch(second.name());
            second.accept(deadline()).await.unwrap();
            assert_eq!(second.verify_peer(first_fixture.job.as_ref().unwrap()).unwrap_err().kind(), io::ErrorKind::PermissionDenied);
            assert_eq!(second.read_frame::<Value>(deadline()).await.unwrap_err().kind(), io::ErrorKind::NotConnected);
            first.write_frame(&Value::Bool(true), deadline()).await.unwrap();
            let until = deadline();
            loop {
                if let Some(status) = first_fixture.child.try_wait().unwrap() { assert!(status.success()); break; }
                assert!(Instant::now() < until, "시험 자손이 종료되지 않았습니다.");
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            assert_eq!(first.peer.as_ref().unwrap().ensure_running().unwrap_err().kind(), io::ErrorKind::BrokenPipe);
            first.close().unwrap();

            let mut lifetime = OwnedTuiPipe::bind().unwrap();
            let mut fixture = Fixture::launch(lifetime.name());
            lifetime.accept(deadline()).await.unwrap();
            lifetime.verify_peer(fixture.job.as_ref().unwrap()).unwrap();
            drop(fixture.job.take());
            let until = deadline();
            loop {
                if lifetime.peer.as_ref().unwrap().ensure_running().is_err() { break; }
                assert!(Instant::now() < until, "통로가 소유 Job의 마지막 핸들 수명을 늘리면 안 됩니다.");
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            lifetime.close().unwrap();
        });
    }

    // 부모·자손 fixture는 아래 정확한 시험 이름으로 별도 프로세스에서만 실행한다.
    // 일반 시험 실행에서는 환경 표식이 없으므로 자식·통로를 만들지 않는다.
    #[test]
    fn pipe_fixture_parent() {
        let Some(name) = std::env::var_os(FIXTURE_PIPE) else { return; };
        std::io::stdin().read_exact(&mut [0]).unwrap();
        let status = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "owned_tui_control::tests::pipe_fixture_client", "--nocapture"])
            .env(FIXTURE_PIPE, name).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null())
            .creation_flags(0x08000000).status().unwrap();
        assert!(status.success());
    }

    #[test]
    fn pipe_fixture_client() {
        let Some(name) = std::env::var_os(FIXTURE_PIPE) else { return; };
        tauri::async_runtime::block_on(async {
            let mut client = ClientOptions::new().open(name).unwrap();
            let identity = OwnedProcessIdentity::current().unwrap().identity();
            let hello = serde_json::to_vec(&serde_json::json!({"pid":identity.pid,"createdFiletime":identity.created_filetime.to_string()})).unwrap();
            timeout_at(deadline(), async {
                client.write_all(&(hello.len() as u32).to_le_bytes()).await.unwrap();
                client.write_all(&hello).await.unwrap();
            }).await.unwrap();
            assert_eq!(read_client(&mut client).await, Value::Bool(true));
        });
    }
}
