//! Read-only representative account quota from the selected CLI.
use super::*;

const POLL_INTERVAL: Duration = Duration::from_secs(5 * 60);
static USAGE_REQUEST: Mutex<Option<Instant>> = Mutex::new(None);

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WeeklyUsageWindow {
    pub used_percent: f64,
    pub resets_at: Option<i64>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WeeklyUsageSnapshot {
    pub executable: String,
    pub observed_at: i64,
    pub limit_name: Option<String>,
    pub weekly: Option<WeeklyUsageWindow>,
}

fn admission(request: &Mutex<Option<Instant>>, now: Instant) -> Result<std::sync::MutexGuard<'_, Option<Instant>>, String> {
    let mut permit = request.try_lock().map_err(|error| match error {
        std::sync::TryLockError::WouldBlock => "A usage read is already in progress".to_owned(),
        std::sync::TryLockError::Poisoned(_) => "Usage read lock is damaged; restart OpenDIY Desktop".to_owned(),
    })?;
    if permit.is_some_and(|previous| now.saturating_duration_since(previous) < POLL_INTERVAL) {
        return Err("Usage reads must be at least five minutes apart".into());
    }
    *permit = Some(now);
    Ok(permit)
}

fn weekly_window(value: &Value) -> Result<Option<WeeklyUsageWindow>, String> {
    // The CLI chooses this backward-compatible representative account bucket.
    // A secondary window is not necessarily weekly; only the explicit duration
    // identifies a week. Missing usage must never become zero usage.
    let limits = value["rateLimits"].as_object().ok_or("Codex returned no account rate-limit snapshot")?;
    let mut weekly = None;
    for key in ["primary", "secondary"] {
        let Some(window) = limits.get(key).filter(|window| !window.is_null()) else { continue; };
        if !window.is_object() { return Err("Codex returned an invalid rate-limit window".into()); }
        if window["windowDurationMins"].as_i64() != Some(10080) { continue; }
        if weekly.is_some() { return Err("Codex returned multiple representative weekly windows".into()); }
        let used_percent = window["usedPercent"].as_f64().filter(|used| used.is_finite() && (0.0..=100.0).contains(used))
            .ok_or("Codex returned an invalid weekly usage percentage")?;
        let resets_at = match window.get("resetsAt") {
            None | Some(Value::Null) => None,
            Some(value) => Some(value.as_i64().filter(|time| *time > 0 && *time <= 8_640_000_000_000)
                .ok_or("Codex returned an invalid weekly reset timestamp")?),
        };
        weekly = Some(WeeklyUsageWindow { used_percent, resets_at });
    }
    Ok(weekly)
}

pub fn weekly_usage_with_executable(exe: &OsString) -> Result<WeeklyUsageSnapshot, String> {
    // ponytail: one nonblocking permit owns the read and its complete process
    // cleanup; a backend cooldown also bounds direct renderer invocations.
    let _permit = admission(&USAGE_REQUEST, Instant::now())?;
    catalog::with_reader(exe, |rpc| {
        let response = rpc.call("account/rateLimits/read", json!({"excludeResetCreditDetails":true}))?;
        let weekly = weekly_window(&response)?;
        let limit_name = response["rateLimits"]["limitName"].as_str()
            .filter(|name| !name.is_empty() && name.len() <= 512).map(str::to_owned);
        Ok(WeeklyUsageSnapshot {
            executable: exe.to_string_lossy().into_owned(),
            observed_at: SystemTime::now().duration_since(UNIX_EPOCH).map_err(|e| e.to_string())?.as_millis() as i64,
            limit_name,
            weekly,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_explicit_weekly_duration_is_usage_and_missing_is_not_zero() {
        for value in [json!({"rateLimits":{}}), json!({"rateLimits":{"secondary":{"usedPercent":0}}}),
            json!({"rateLimits":{"secondary":{"windowDurationMins":1440,"usedPercent":25}}})] {
            assert!(weekly_window(&value).unwrap().is_none());
        }
        for key in ["primary", "secondary"] {
            let mut value = json!({"rateLimits":{}});
            value["rateLimits"][key] = json!({"windowDurationMins":10080,"usedPercent":0,"resetsAt":2000000000});
            let weekly = weekly_window(&value).unwrap().unwrap();
            assert_eq!(weekly.used_percent, 0.0);
            assert_eq!(weekly.resets_at, Some(2000000000));
        }
        assert!(weekly_window(&json!({"rateLimits":{"secondary":{"windowDurationMins":10080,"usedPercent":32.5,"resetsAt":null}}})).unwrap().unwrap().resets_at.is_none());
    }

    #[test]
    fn malformed_weekly_data_is_rejected_instead_of_fabricated() {
        assert!(weekly_window(&json!({})).is_err());
        for percentage in [Value::Null, json!("42"), json!(-1), json!(101)] {
            assert!(weekly_window(&json!({"rateLimits":{"secondary":{"windowDurationMins":10080,"usedPercent":percentage}}})).is_err());
        }
        for reset in [json!("tomorrow"), json!(-1), json!(0), json!(8_640_000_000_001i64)] {
            assert!(weekly_window(&json!({"rateLimits":{"secondary":{"windowDurationMins":10080,"usedPercent":42,"resetsAt":reset}}})).is_err());
        }
        let window = json!({"windowDurationMins":10080,"usedPercent":42});
        assert!(weekly_window(&json!({"rateLimits":{"primary":window,"secondary":window}})).is_err());
    }

    #[test]
    fn admission_rejects_overlap_and_does_not_queue_or_bypass_five_minute_cooldown() {
        let request = Mutex::new(None);
        let now = Instant::now();
        let permit = admission(&request, now).unwrap();
        assert!(admission(&request, now + POLL_INTERVAL).unwrap_err().contains("already"));
        drop(permit);
        assert!(admission(&request, now + POLL_INTERVAL - Duration::from_millis(1)).unwrap_err().contains("five minutes"));
        assert!(admission(&request, now + POLL_INTERVAL).is_ok());
    }
}
