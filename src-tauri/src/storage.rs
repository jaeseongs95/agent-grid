use crate::session::SessionInfo;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;
use std::{path::Path, time::Duration};

pub struct Store { connection: Connection }
impl Store {
    pub fn open(path: &Path) -> Result<Self, String> {
        let connection = Connection::open(path).map_err(|e| format!("open application metadata: {e}"))?;
        connection.busy_timeout(Duration::from_secs(5)).map_err(|e| e.to_string())?;
        connection.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=FULL;
             CREATE TABLE IF NOT EXISTS session_metadata (
                id TEXT PRIMARY KEY NOT NULL,
                generation INTEGER NOT NULL CHECK (generation > 0),
                metadata TEXT NOT NULL,
                updated_at INTEGER NOT NULL DEFAULT (unixepoch())
             );
             CREATE TABLE IF NOT EXISTS workspace_layout (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                layout TEXT NOT NULL
             );"
        ).map_err(|e| format!("initialize application metadata: {e}"))?;
        Ok(Self { connection })
    }

    pub fn save_session(&self, info: &SessionInfo) -> Result<(), String> {
        let metadata = serde_json::to_string(info).map_err(|e| e.to_string())?;
        self.connection.execute(
            "INSERT INTO session_metadata(id,generation,metadata) VALUES(?1,?2,?3)
             ON CONFLICT(id) DO UPDATE SET generation=excluded.generation,metadata=excluded.metadata,updated_at=unixepoch()
             WHERE excluded.generation >= session_metadata.generation",
            params![info.id, info.generation, metadata]
        ).map_err(|e| format!("save session metadata: {e}"))?;
        Ok(())
    }

    pub fn sessions(&self) -> Result<Vec<SessionInfo>, String> {
        let mut statement = self.connection.prepare("SELECT metadata FROM session_metadata ORDER BY updated_at DESC,id LIMIT 512").map_err(|e| e.to_string())?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0)).map_err(|e| e.to_string())?;
        let mut infos = Vec::new();
        for row in rows {
            let mut info: SessionInfo = serde_json::from_str(&row.map_err(|e| e.to_string())?).map_err(|e| format!("invalid stored session metadata: {e}"))?;
            // Metadata is a restore suggestion, never evidence of a running process.
            info.state = "restored".into();
            infos.push(info);
        }
        Ok(infos)
    }

    pub fn prune_sessions(&self, live_ids: &[String]) -> Result<(), String> {
        // Keep every current process plus the 512 newest records. Restored
        // historical records have no process ownership and can be retired.
        let keep = serde_json::to_string(live_ids).map_err(|e| e.to_string())?;
        self.connection.execute(
            "DELETE FROM session_metadata
             WHERE id NOT IN (SELECT value FROM json_each(?1))
             AND id NOT IN (SELECT id FROM session_metadata ORDER BY updated_at DESC,rowid DESC LIMIT 512)",
            [keep]
        ).map_err(|e| format!("retain bounded session metadata: {e}"))?;
        Ok(())
    }

    pub fn load_layout(&self) -> Result<Value, String> {
        let stored: Option<String> = self.connection.query_row("SELECT layout FROM workspace_layout WHERE id=1", [], |row| row.get(0)).optional().map_err(|e| e.to_string())?;
        stored.map(|text| serde_json::from_str(&text).map_err(|e| format!("invalid stored layout: {e}"))).unwrap_or(Ok(Value::Null))
    }

    pub fn save_layout(&self, layout: &Value) -> Result<(), String> {
        let text = serde_json::to_string(layout).map_err(|e| e.to_string())?;
        if text.len() > 1024 * 1024 { return Err("layout exceeds 1 MiB limit".into()); }
        self.connection.execute("INSERT INTO workspace_layout(id,layout) VALUES(1,?1) ON CONFLICT(id) DO UPDATE SET layout=excluded.layout", [text]).map_err(|e| format!("save workspace layout: {e}"))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn metadata_restores_without_running_and_layout_replaces_atomically() {
        let store = Store::open(Path::new(":memory:")).unwrap();
        let mut info = SessionInfo { id: "test".into(), generation: 2, title: "title".into(), cwd: "directory".into(), thread_id: None, state: "running".into() };
        store.save_session(&info).unwrap();
        info.generation = 1;
        info.title = "stale".into();
        store.save_session(&info).unwrap();
        let restored = store.sessions().unwrap();
        assert_eq!(restored[0].generation, 2);
        assert_eq!(restored[0].title, "title");
        assert_eq!(restored[0].state, "restored");
        assert!(store.load_layout().unwrap().is_null());
        let layout = serde_json::json!({"panels":[{"id":"test"}],"columns":2});
        store.save_layout(&layout).unwrap();
        assert_eq!(store.load_layout().unwrap(), layout);
        assert!(store.save_layout(&Value::String("x".repeat(1024 * 1024))).is_err());
        assert_eq!(store.load_layout().unwrap(), layout);
    }

    #[test]
    fn metadata_retention_preserves_active_sessions_without_unbounded_history() {
        let store = Store::open(Path::new(":memory:")).unwrap();
        for index in 0..600 {
            store.save_session(&SessionInfo { id: format!("session-{index:04}"), generation: 1, title: "fixture".into(), cwd: ".".into(), thread_id: None, state: "exited".into() }).unwrap();
        }
        store.prune_sessions(&["session-0000".into()]).unwrap();
        let count: usize = store.connection.query_row("SELECT COUNT(*) FROM session_metadata", [], |r| r.get(0)).unwrap();
        assert_eq!(count, 513);
        let active: usize = store.connection.query_row("SELECT COUNT(*) FROM session_metadata WHERE id='session-0000'", [], |r| r.get(0)).unwrap();
        assert_eq!(active, 1);
    }
}
