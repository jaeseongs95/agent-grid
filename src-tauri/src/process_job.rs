//! Only processes created by this application are assigned to these jobs.
//! LaunchGate prevents app-created helpers from spawning before Job assignment.
//! Do not extend this to external PIDs or existing vendor services.
pub use crate::launch_gate::{run_child, LaunchGate};

#[cfg(windows)]
mod platform {
    use std::{os::windows::io::AsRawHandle, sync::OnceLock};
    use windows_sys::Win32::{
        Foundation::{CloseHandle, HANDLE},
        System::{
            JobObjects::{AssignProcessToJobObject, CreateJobObjectW, JobObjectBasicAccountingInformation, JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject, TerminateJobObject, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE},
            Threading::GetCurrentProcess,
        },
    };

    #[derive(Debug)]
    pub struct ProcessJob(HANDLE);
    // A Job handle may be used from any thread. This type owns its only handle.
    unsafe impl Send for ProcessJob {}
    unsafe impl Sync for ProcessJob {}
    static APP_JOB: OnceLock<ProcessJob> = OnceLock::new();

    impl ProcessJob {
        fn assign(handle: HANDLE) -> Result<Self, String> {
            unsafe {
                let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if job.is_null() {
                    return Err(format!("create owned process job: {}", std::io::Error::last_os_error()));
                }
                let guard = Self(job);
                let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
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
            Self::assign(child.as_raw_handle() as HANDLE)
        }

        pub fn assign_pty(child: &dyn portable_pty::Child) -> Result<Self, String> {
            let handle = child.as_raw_handle().ok_or("owned PTY has no Windows process handle")?;
            Self::assign(handle as HANDLE)
        }

        pub fn terminate(&self) -> Result<(), String> {
            if unsafe { TerminateJobObject(self.0, 1) } == 0 {
                Err(format!("terminate owned process job: {}", std::io::Error::last_os_error()))
            } else {
                Ok(())
            }
        }

        fn active_processes(&self) -> Result<u32, String> {
            let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { std::mem::zeroed() };
            if unsafe { QueryInformationJobObject(self.0, JobObjectBasicAccountingInformation, &mut info as *mut _ as _, std::mem::size_of_val(&info) as u32, std::ptr::null_mut()) } == 0 {
                Err(format!("inspect owned process job: {}", std::io::Error::last_os_error()))
            } else { Ok(info.ActiveProcesses) }
        }
    }

    impl Drop for ProcessJob {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.0); }
        }
    }

    pub fn install_app_job() -> Result<(), String> {
        // Never dropped in-process: closing this handle would also kill this app.
        // Windows closes it on exit/crash, reclaiming every app-created descendant.
        if APP_JOB.get().is_none() {
            let job = ProcessJob::assign(unsafe { GetCurrentProcess() })?;
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
        pub fn assign_pty(_: &dyn portable_pty::Child) -> Result<Self, String> { Ok(Self) }
        pub fn terminate(&self) -> Result<(), String> { Ok(()) }
    }
    pub fn install_app_job() -> Result<(), String> { Ok(()) }
    pub fn app_owned_processes() -> Result<Option<u32>, String> { Ok(None) }
}

pub use platform::{app_owned_processes, install_app_job, ProcessJob};
