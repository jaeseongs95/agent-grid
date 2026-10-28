use crate::{project::{ProjectDirectory, ProjectInfo}, session::SessionInfo};
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
             );
             CREATE TABLE IF NOT EXISTS app_settings (
                key TEXT PRIMARY KEY NOT NULL,
                value TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS projects (
                id TEXT PRIMARY KEY NOT NULL,
                canonical_cwd TEXT UNIQUE NOT NULL,
                cwd TEXT NOT NULL,
                name TEXT NOT NULL,
                created_at INTEGER NOT NULL DEFAULT (unixepoch())
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

    pub fn codex_executable(&self) -> Result<Option<String>, String> {
        self.connection.query_row("SELECT value FROM app_settings WHERE key='codex_executable'", [], |row| row.get(0))
            .optional().map_err(|e| format!("Read app executable setting: {e}"))
    }

    pub fn set_codex_executable(&self, path: Option<&str>) -> Result<(), String> {
        match path {
            Some(path) => self.connection.execute(
                "INSERT INTO app_settings(key,value) VALUES('codex_executable',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [path]),
            None => self.connection.execute("DELETE FROM app_settings WHERE key='codex_executable'", []),
        }.map(|_| ()).map_err(|e| format!("Save app executable setting: {e}"))
    }

    pub fn import_project(&self, directory: &ProjectDirectory) -> Result<ProjectInfo, String> {
        self.connection.execute(
            "INSERT INTO projects(id,canonical_cwd,cwd,name) VALUES(?1,?2,?3,?4)
             ON CONFLICT(canonical_cwd) DO NOTHING",
            params![uuid::Uuid::new_v4().to_string(), directory.identity, directory.cwd, directory.name]
        ).map_err(|e| format!("Register project directory: {e}"))?;
        self.connection.query_row(
            "SELECT id,name,cwd,created_at FROM projects WHERE canonical_cwd=?1",
            [&directory.identity], |row| Ok(ProjectInfo { id: row.get(0)?, name: row.get(1)?, cwd: row.get(2)?, created_at: row.get(3)? })
        ).map_err(|e| format!("Read imported project: {e}"))
    }

    pub fn projects(&self) -> Result<Vec<ProjectInfo>, String> {
        let mut statement = self.connection.prepare("SELECT id,name,cwd,created_at FROM projects ORDER BY created_at DESC,id")
            .map_err(|e| format!("List imported projects: {e}"))?;
        let rows = statement.query_map([], |row| Ok(ProjectInfo { id: row.get(0)?, name: row.get(1)?, cwd: row.get(2)?, created_at: row.get(3)? }))
            .map_err(|e| format!("Query imported projects: {e}"))?;
        rows.collect::<Result<Vec<_>, _>>().map_err(|e| format!("Read imported project row: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn executable_setting_reopens_and_reset_preserves_workspace_records() {
        let directory = std::env::temp_dir().join(format!("agent-grid-setting-store-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let database = directory.join("workspace.sqlite3");
        let layout = serde_json::json!({"version":1,"sessions":[],"columns":2});
        let project;
        {
            let store = Store::open(&database).unwrap();
            store.save_layout(&layout).unwrap();
            store.save_session(&SessionInfo { id: "existing-session".into(), generation: 3, title: "existing title".into(), cwd: directory.to_string_lossy().into_owned(), thread_id: None, state: "idle".into() }).unwrap();
            project = store.import_project(&crate::project::directory(directory.to_str().unwrap()).unwrap()).unwrap();
            store.set_codex_executable(Some("C:\\native\\codex.exe")).unwrap();
        }
        {
            let store = Store::open(&database).unwrap();
            assert_eq!(store.codex_executable().unwrap().as_deref(), Some("C:\\native\\codex.exe"));
            assert_eq!(store.load_layout().unwrap(), layout);
            assert_eq!(store.sessions().unwrap()[0].generation, 3);
            assert_eq!(store.projects().unwrap()[0].id, project.id);
            store.set_codex_executable(Some("C:\\other\\codex.exe")).unwrap();
            assert_eq!(store.codex_executable().unwrap().as_deref(), Some("C:\\other\\codex.exe"));
            store.set_codex_executable(None).unwrap();
        }
        {
            let store = Store::open(&database).unwrap();
            assert!(store.codex_executable().unwrap().is_none());
            assert_eq!(store.load_layout().unwrap(), layout);
            assert_eq!(store.sessions().unwrap()[0].id, "existing-session");
            assert_eq!(store.projects().unwrap()[0].id, project.id);
        }
        for file in ["workspace.sqlite3-wal", "workspace.sqlite3-shm", "workspace.sqlite3"] {
            let path = directory.join(file);
            if path.exists() { std::fs::remove_file(path).unwrap(); }
        }
        std::fs::remove_dir(directory).unwrap();
    }
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

    #[test]
    fn imported_unicode_project_deduplicates_canonical_paths_and_survives_reopen() {
        let root = std::env::temp_dir().join(format!("agent-grid-project-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let folder = root.join("한글 프로젝트");
        std::fs::create_dir(&folder).unwrap();
        let database = root.join("metadata.sqlite3");
        let first;
        {
            let store = Store::open(&database).unwrap();
            let directory = crate::project::directory(folder.to_str().unwrap()).unwrap();
            first = store.import_project(&directory).unwrap();
            let alias = folder.join("..").join("한글 프로젝트");
            let repeated = store.import_project(&crate::project::directory(alias.to_str().unwrap()).unwrap()).unwrap();
            assert_eq!(first.id, repeated.id);
            assert_eq!(first.name, "한글 프로젝트");
            assert_eq!(store.projects().unwrap().len(), 1);
            store.save_layout(&serde_json::json!({"version":1,"sessions":[]})).unwrap();
            store.save_session(&SessionInfo { id: "existing".into(), generation: 1, title: "Existing session".into(), cwd: first.cwd.clone(), thread_id: None, state: "exited".into() }).unwrap();
        }
        {
            let reopened = Store::open(&database).unwrap();
            let saved = reopened.projects().unwrap();
            assert_eq!(saved.len(), 1);
            assert_eq!(saved[0].id, first.id);
            assert_eq!(saved[0].cwd, first.cwd);
            assert_eq!(reopened.sessions().unwrap()[0].id, "existing");
            assert_eq!(reopened.load_layout().unwrap()["sessions"], serde_json::json!([]));
        }
        // Delete only the files and empty directories created by this fixture.
        for name in ["metadata.sqlite3", "metadata.sqlite3-wal", "metadata.sqlite3-shm"] {
            let file = root.join(name);
            if file.exists() { std::fs::remove_file(file).unwrap(); }
        }
        std::fs::remove_dir(folder).unwrap();
        std::fs::remove_dir(root).unwrap();
    }
}
