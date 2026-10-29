use super::*;

impl Engine {
    /// This is only an offer. The explicit exit action rechecks under admission.
    pub fn unconfirmed_exit_available(&self) -> Result<bool, String> {
        let sessions = self.inner.sessions.lock().map_err(|_| "session registry lock poisoned")?;
        let mut unknown = false;
        for session in sessions.values() {
            if session.phase.load(Ordering::Acquire) != 3
                || session.process.lock().map_err(|_| "session process lock poisoned")?.is_some()
                || !*session.done.0.lock().map_err(|_| "session completion lock poisoned")? {
                return Ok(false);
            }
            unknown |= session.cleanup_failed.load(Ordering::Acquire);
        }
        Ok(unknown)
    }

    /// Close only after the user separately accepts that remote work may continue.
    /// No remote request or persisted session status is changed by this path.
    pub fn shutdown_with_unconfirmed_remote(&self) -> Result<(), String> {
        let _extensions = codex::extension_admission().map_err(|error| format!("Cannot exit while extension management is busy: {error}"))?;
        let was_exiting = self.inner.exiting.swap(true, Ordering::AcqRel);
        // Allocate rechecks exiting while holding the same registry lock used by
        // the availability check, so a stale renderer offer cannot bypass a PTY.
        let result = self.unconfirmed_exit_available().and_then(|ready| {
            if ready { Ok(()) } else { Err("로컬 자원 정리가 완료되고 원격 중단이 미확인인 경우에만 이 종료 방식을 사용할 수 있습니다.".into()) }
        });
        if result.is_err() { self.inner.exiting.store(was_exiting, Ordering::Release); }
        result
    }
}

#[cfg(test)]
#[path = "session_unconfirmed_exit_tests.rs"]
mod tests;
