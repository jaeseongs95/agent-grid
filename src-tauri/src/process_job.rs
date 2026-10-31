//! Only processes created by this application are assigned to these jobs.
//! LaunchGate prevents app-created helpers from spawning before Job assignment.
//! Do not extend this to external PIDs or existing vendor services.
pub use crate::launch_gate::{run_child, LaunchGate};

#[cfg(windows)]
mod platform {
    use std::{net::{Ipv4Addr, SocketAddr, TcpStream}, os::windows::io::{AsRawHandle, BorrowedHandle, FromRawHandle, OwnedHandle}, sync::OnceLock};
    use windows_sys::Win32::{
        Foundation::{CloseHandle, ERROR_INSUFFICIENT_BUFFER, FILETIME, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT},
        NetworkManagement::IpHelper::{GetExtendedTcpTable, MIB_TCPTABLE_OWNER_PID, MIB_TCPROW_OWNER_PID, MIB_TCP_STATE_ESTAB, TCP_TABLE_OWNER_PID_ALL},
        Networking::WinSock::AF_INET,
        System::{
            JobObjects::{AssignProcessToJobObject, CreateJobObjectW, IsProcessInJob, JobObjectBasicAccountingInformation, JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject, TerminateJobObject, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_BREAKAWAY_OK, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE},
            Pipes::GetNamedPipeClientProcessId,
            Threading::{GetCurrentProcess, GetProcessId, GetProcessTimes, OpenProcess, WaitForSingleObject, PROCESS_QUERY_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE},
        },
    };

    /// PID 재사용과 실행 세대를 구분하는 OS 신원. 인증 handshake를 대신하지 않는다.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct ProcessIdentity {
        pub pid: u32,
        pub created_filetime: u64,
    }

    /// 조회·종료 확인 권한만 보유한다. Job이나 종료 권한은 보유하지 않는다.
    #[derive(Debug)]
    pub struct OwnedProcessIdentity {
        handle: OwnedHandle,
        identity: ProcessIdentity,
    }

    impl OwnedProcessIdentity {
        pub fn current() -> std::io::Result<Self> {
            Self::open(std::process::id())
        }

        fn open(pid: u32) -> std::io::Result<Self> {
            // SAFETY: 반환된 실제 프로세스 핸들의 유일한 소유권을 받는다.
            let raw = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE, 0, pid) };
            if raw.is_null() { return Err(std::io::Error::last_os_error()); }
            let handle = unsafe { OwnedHandle::from_raw_handle(raw) };
            let mut created: FILETIME = unsafe { std::mem::zeroed() };
            let (mut exited, mut kernel, mut user): (FILETIME, FILETIME, FILETIME) = unsafe { std::mem::zeroed() };
            // SAFETY: 유효한 조회 핸들과 초기화 가능한 FILETIME 포인터다.
            if unsafe { GetProcessTimes(raw, &mut created, &mut exited, &mut kernel, &mut user) } == 0 {
                return Err(std::io::Error::last_os_error());
            }
            let identity = ProcessIdentity {
                pid: unsafe { GetProcessId(raw) },
                created_filetime: (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime),
            };
            if identity.pid == 0 { return Err(std::io::Error::last_os_error()); }
            if identity.pid != pid { return Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "프로세스 신원이 바뀌었습니다.")); }
            let process = Self { handle, identity };
            process.ensure_running()?;
            Ok(process)
        }

        pub fn identity(&self) -> ProcessIdentity { self.identity }

        pub fn ensure_running(&self) -> std::io::Result<()> {
            // SAFETY: PROCESS_SYNCHRONIZE 권한을 가진 소유 핸들이다. 대기하지 않는다.
            match unsafe { WaitForSingleObject(self.handle.as_raw_handle(), 0) } {
                WAIT_TIMEOUT => Ok(()),
                WAIT_OBJECT_0 => Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "통로 상대 프로세스가 종료됐습니다.")),
                _ => Err(std::io::Error::last_os_error()),
            }
        }
    }

    #[derive(Debug)]
    pub struct ProcessJob(HANDLE);
    // A Job handle may be used from any thread. This type owns its only handle.
    unsafe impl Send for ProcessJob {}
    unsafe impl Sync for ProcessJob {}
    static APP_JOB: OnceLock<ProcessJob> = OnceLock::new();

    impl ProcessJob {
        fn assign(handle: HANDLE, allow_breakaway: bool) -> Result<Self, String> {
            unsafe {
                let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if job.is_null() {
                    return Err(format!("create owned process job: {}", std::io::Error::last_os_error()));
                }
                let guard = Self(job);
                let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                if allow_breakaway {
                    limits.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_BREAKAWAY_OK;
                }
                if SetInformationJobObject(job, JobObjectExtendedLimitInformation, &limits as *const _ as _, std::mem::size_of_val(&limits) as u32) == 0 {
                    return Err(format!("configure owned process job: {}", std::io::Error::last_os_error()));
                }
                if AssignProcessToJobObject(job, handle) == 0 {
                    return Err(format!("assign owned process job: {}", std::io::Error::last_os_error()));
                }
                Ok(guard)
            }
        }

        pub fn assign_std(child: &std::process::Child) -> Result<Self, String> {
            Self::assign(child.as_raw_handle() as HANDLE, false)
        }

        /// 공유 daemon을 시작하는 전용 LaunchGate 실행기에만 사용한다.
        /// 명시적 CREATE_BREAKAWAY_FROM_JOB만 허용하며, 보통 자손은 계속 회수한다.
        pub fn assign_daemon_launcher(child: &std::process::Child) -> Result<Self, String> {
            Self::assign(child.as_raw_handle() as HANDLE, true)
        }

        pub fn assign_pty(child: &dyn portable_pty::Child) -> Result<Self, String> {
            let handle = child.as_raw_handle().ok_or("owned PTY has no Windows process handle")?;
            Self::assign(handle as HANDLE, false)
        }

        /// 실제 연결 상대만 검사한다. Job을 짧게 빌리고 I/O·프로세스 대기를 하지 않는다.
        pub fn verify_pipe_client(&self, pipe: BorrowedHandle<'_>) -> std::io::Result<OwnedProcessIdentity> {
            let mut pid = 0;
            // SAFETY: 호출자가 연결된 서버 통로 핸들의 수명을 빌려준다.
            if unsafe { GetNamedPipeClientProcessId(pipe.as_raw_handle(), &mut pid) } == 0 {
                return Err(std::io::Error::last_os_error());
            }
            let process = OwnedProcessIdentity::open(pid)?;
            let mut owned = 0;
            // SAFETY: 두 핸들 모두 이 호출이 끝날 때까지 살아 있다.
            if unsafe { IsProcessInJob(process.handle.as_raw_handle(), self.0, &mut owned) } == 0 {
                return Err(std::io::Error::last_os_error());
            }
            if owned == 0 {
                return Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "연결 상대가 이 PTY의 소유 Job에 속하지 않습니다."));
            }
            let mut rechecked_pid = 0;
            if unsafe { GetNamedPipeClientProcessId(pipe.as_raw_handle(), &mut rechecked_pid) } == 0 {
                return Err(std::io::Error::last_os_error());
            }
            if rechecked_pid != process.identity.pid {
                return Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "검사 중 통로 상대가 바뀌었습니다."));
            }
            process.ensure_running()?;
            Ok(process)
        }

        pub fn terminate(&self) -> Result<(), String> {
            if unsafe { TerminateJobObject(self.0, 1) } == 0 {
                Err(format!("terminate owned process job: {}", std::io::Error::last_os_error()))
            } else {
                Ok(())
            }
        }

        /// Verify the already-connected server endpoint, before the caller sends
        /// any HTTP bytes. A listener PID checked before connect would be a TOCTOU.
        pub fn verify_tcp_peer(&self, stream: &TcpStream) -> Result<(), String> {
            let local = stream.local_addr().map_err(|e| format!("inspect connected local endpoint: {e}"))?;
            let peer = stream.peer_addr().map_err(|e| format!("inspect connected server endpoint: {e}"))?;
            let (SocketAddr::V4(local), SocketAddr::V4(peer)) = (local, peer) else { return Err("owned server verification requires IPv4 loopback".into()); };
            if !local.ip().is_loopback() || !peer.ip().is_loopback() { return Err("owned server verification requires loopback endpoints".into()); }
            let pid = connected_server_pid(*local.ip(), local.port(), *peer.ip(), peer.port())?;
            unsafe {
                let process = OpenProcess(PROCESS_QUERY_INFORMATION, 0, pid);
                if process.is_null() { return Err(format!("open connected server for ownership verification: {}", std::io::Error::last_os_error())); }
                let mut owned = 0;
                let result = IsProcessInJob(process, self.0, &mut owned);
                let error = if result == 0 { Some(std::io::Error::last_os_error()) } else { None };
                CloseHandle(process);
                if let Some(error) = error { return Err(format!("verify connected server job membership: {error}")); }
                if owned == 0 { return Err("connected TCP server does not belong to the owned server job".into()); }
            }
            Ok(())
        }

        pub(crate) fn active_processes(&self) -> Result<u32, String> {
            let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { std::mem::zeroed() };
            if unsafe { QueryInformationJobObject(self.0, JobObjectBasicAccountingInformation, &mut info as *mut _ as _, std::mem::size_of_val(&info) as u32, std::ptr::null_mut()) } == 0 {
                Err(format!("inspect owned process job: {}", std::io::Error::last_os_error()))
            } else { Ok(info.ActiveProcesses) }
        }
    }

    fn connected_server_pid(client_ip: Ipv4Addr, client_port: u16, server_ip: Ipv4Addr, server_port: u16) -> Result<u32, String> {
        const MAX_TABLE_BYTES: usize = 16 * 1024 * 1024;
        let offset = std::mem::offset_of!(MIB_TCPTABLE_OWNER_PID, table);
        let mut required = 0u32;
        let initial = unsafe { GetExtendedTcpTable(std::ptr::null_mut(), &mut required, 0, AF_INET as u32, TCP_TABLE_OWNER_PID_ALL, 0) };
        if initial != 0 && initial != ERROR_INSUFFICIENT_BUFFER { return Err(format!("size TCP ownership table failed with Windows error {initial}")); }
        for _ in 0..4 {
            let bytes = required as usize;
            if bytes < offset || bytes > MAX_TABLE_BYTES { return Err("TCP ownership table size is outside its supported bounds".into()); }
            // DWORD storage supplies the alignment of both documented C structs.
            // OS table growth is retried with a bounded allocation and attempt count.
            let mut storage = vec![0u32; bytes.div_ceil(std::mem::size_of::<u32>())];
            let capacity = storage.len() * std::mem::size_of::<u32>();
            let mut written = capacity as u32;
            let status = unsafe { GetExtendedTcpTable(storage.as_mut_ptr().cast(), &mut written, 0, AF_INET as u32, TCP_TABLE_OWNER_PID_ALL, 0) };
            if status == ERROR_INSUFFICIENT_BUFFER { required = written; continue; }
            if status != 0 { return Err(format!("read TCP ownership table failed with Windows error {status}")); }
            let used = written as usize;
            if used < offset || used > capacity { return Err("TCP ownership table returned an invalid byte count".into()); }
            let count = storage[0] as usize;
            let end = count.checked_mul(std::mem::size_of::<MIB_TCPROW_OWNER_PID>()).and_then(|length| offset.checked_add(length)).ok_or("TCP ownership table count overflow")?;
            if end > used { return Err("TCP ownership table entries exceed its returned buffer".into()); }
            let rows = unsafe { storage.as_ptr().cast::<u8>().add(offset).cast::<MIB_TCPROW_OWNER_PID>() };
            for index in 0..count {
                // Bounds, count and struct alignment were checked above. Use the
                // server side's full connected tuple, never a listening port alone.
                let row = unsafe { std::ptr::read(rows.add(index)) };
                if row.dwState == MIB_TCP_STATE_ESTAB as u32
                    && Ipv4Addr::from(row.dwLocalAddr.to_ne_bytes()) == server_ip
                    && u16::from_be(row.dwLocalPort as u16) == server_port
                    && Ipv4Addr::from(row.dwRemoteAddr.to_ne_bytes()) == client_ip
                    && u16::from_be(row.dwRemotePort as u16) == client_port {
                    return Ok(row.dwOwningPid);
                }
            }
            return Err("connected server tuple was not found in the established TCP ownership table".into());
        }
        Err("TCP ownership table kept growing beyond the bounded retry count".into())
    }

    impl Drop for ProcessJob {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.0); }
        }
    }

    pub fn install_app_job() -> Result<(), String> {
        // Never dropped in-process: closing this handle would also kill this app.
        // Windows closes it on exit/crash, reclaiming descendants except the
        // explicitly detached shared daemon. Per-query/PTY jobs forbid breakaway.
        if APP_JOB.get().is_none() {
            let job = ProcessJob::assign(unsafe { GetCurrentProcess() }, true)?;
            if let Err(job) = APP_JOB.set(job) {
                // Startup is single threaded, but never close a second self job.
                std::mem::forget(job);
            }
        }
        Ok(())
    }

    pub fn app_owned_processes() -> Result<Option<u32>, String> { APP_JOB.get().map(ProcessJob::active_processes).transpose() }
}

#[cfg(not(windows))]
mod platform {
    #[derive(Debug)]
    pub struct ProcessJob;
    impl ProcessJob {
        pub fn assign_std(_: &std::process::Child) -> Result<Self, String> { Ok(Self) }
        pub fn assign_daemon_launcher(_: &std::process::Child) -> Result<Self, String> { Ok(Self) }
        pub fn assign_pty(_: &dyn portable_pty::Child) -> Result<Self, String> { Ok(Self) }
        pub fn terminate(&self) -> Result<(), String> { Ok(()) }
        pub fn verify_tcp_peer(&self, _: &std::net::TcpStream) -> Result<(), String> { Err("owned TCP peer verification is supported only on Windows".into()) }
    }
    pub fn install_app_job() -> Result<(), String> { Ok(()) }
    pub fn app_owned_processes() -> Result<Option<u32>, String> { Ok(None) }
}

pub use platform::{app_owned_processes, install_app_job, ProcessJob};

#[cfg(windows)]
pub use platform::{OwnedProcessIdentity, ProcessIdentity};

#[cfg(all(test, windows))]
#[path = "process_job_tests.rs"]
mod tests;
