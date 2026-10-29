use crate::{project::{ProjectDirectory, ProjectInfo}, session::SessionInfo};
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
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
                cwd TEXT,
                name TEXT NOT NULL,
                created_at INTEGER NOT NULL DEFAULT (unixepoch()),
                favorite INTEGER NOT NULL DEFAULT 0 CHECK (favorite IN (0,1))
             );"
        ).map_err(|e| format!("initialize application metadata: {e}"))?;
        // 구버전의 프로젝트 식별자와 경로 문자열을 그대로 옮긴다. DDL도
        // 같은 트랜잭션에 묶어 중단 시 원래 표 전체가 남도록 한다.
        let migration = Transaction::new_unchecked(&connection, TransactionBehavior::Immediate).map_err(|e| format!("Begin project migration: {e}"))?;
        let columns = migration.prepare("PRAGMA table_info(projects)").map_err(|e| e.to_string())?
            .query_map([], |row| Ok((row.get::<_, String>(1)?, row.get::<_, bool>(3)?)))
            .map_err(|e| e.to_string())?.collect::<Result<Vec<_>, _>>().map_err(|e| e.to_string())?;
        if !columns.iter().any(|(name, _)| name == "favorite") || columns.iter().any(|(name, required)| name == "cwd" && *required) {
            let favorite = if columns.iter().any(|(name, _)| name == "favorite") { "favorite" } else { "0" };
            migration.execute_batch(&format!("CREATE TABLE projects_v050 (
                id TEXT PRIMARY KEY NOT NULL, canonical_cwd TEXT UNIQUE NOT NULL,
                cwd TEXT, name TEXT NOT NULL, created_at INTEGER NOT NULL DEFAULT (unixepoch()),
                favorite INTEGER NOT NULL DEFAULT 0 CHECK (favorite IN (0,1)));
                INSERT INTO projects_v050(id,canonical_cwd,cwd,name,created_at,favorite)
                    SELECT id,canonical_cwd,cwd,name,created_at,{favorite} FROM projects;
                DROP TABLE projects;
                ALTER TABLE projects_v050 RENAME TO projects;"))
                .map_err(|e| format!("Migrate project metadata: {e}"))?;
        }
        migration.commit().map_err(|e| format!("Commit project migration: {e}"))?;
        Ok(Self { connection })
    }

    pub fn save_session(&self, info: &SessionInfo) -> Result<(), String> {
        let transaction = Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate).map_err(|e| format!("Begin session metadata save: {e}"))?;
        Self::ensure_project_in(&transaction, info.project_id.as_deref())?;
        let metadata = serde_json::to_string(info).map_err(|e| e.to_string())?;
        transaction.execute(
            "INSERT INTO session_metadata(id,generation,metadata) VALUES(?1,?2,?3)
             ON CONFLICT(id) DO UPDATE SET generation=excluded.generation,metadata=excluded.metadata,updated_at=unixepoch()
             WHERE excluded.generation > session_metadata.generation OR
                (excluded.generation = session_metadata.generation AND
                 COALESCE(json_extract(excluded.metadata,'$.projectRevision'),0) >= COALESCE(json_extract(session_metadata.metadata,'$.projectRevision'),0))",
            params![info.id, info.generation, metadata]
        ).map_err(|e| format!("save session metadata: {e}"))?;
        transaction.commit().map_err(|e| format!("Commit session metadata save: {e}"))?;
        Ok(())
    }

    pub fn sessions(&self) -> Result<Vec<SessionInfo>, String> {
        let mut statement = self.connection.prepare("SELECT id,generation,metadata FROM session_metadata ORDER BY updated_at DESC,id LIMIT 512").map_err(|e| e.to_string())?;
        let rows = statement.query_map([], |row| Ok((row.get::<_, String>(0)?,row.get::<_, u64>(1)?,row.get::<_, String>(2)?))).map_err(|e| e.to_string())?;
        let mut infos = Vec::new();
        for row in rows {
            let (id,generation,metadata) = row.map_err(|e| e.to_string())?;
            let mut info: SessionInfo = serde_json::from_str(&metadata).map_err(|e| format!("invalid stored session metadata: {e}"))?;
            if info.id != id || info.generation != generation || generation == 0 { return Err("Stored session identity or generation does not match its record".into()); }
            // Metadata is a restore suggestion, never evidence of a running process.
            info.state = "restored".into();
            info.active = false;
            if let Some(id) = info.project_id.as_deref() {
                if !self.project_exists(id)? { info.project_id = None; info.project_assigned = true; }
            }
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

    pub fn ui_language(&self) -> Result<crate::ui_language::UiLanguage, String> {
        let saved: Option<String> = self.connection.query_row("SELECT value FROM app_settings WHERE key='ui_language'", [], |row| row.get(0))
            .optional().map_err(|e| format!("Read app language setting: {e}"))?;
        match saved.as_deref() {
            None | Some("ko") => Ok(crate::ui_language::UiLanguage::Ko),
            Some("en") => Ok(crate::ui_language::UiLanguage::En),
            Some(_) => Err("Stored app language is unsupported; choose Korean or English again".into()),
        }
    }

    pub fn set_ui_language(&self, language: crate::ui_language::UiLanguage) -> Result<(), String> {
        self.connection.execute("INSERT INTO app_settings(key,value) VALUES('ui_language',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [language.as_str()])
            .map(|_| ()).map_err(|e| format!("Save app language setting: {e}"))
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
        let transaction = self.project_transaction()?;
        let existing = transaction.query_row("SELECT id,name,cwd,created_at,favorite FROM projects WHERE canonical_cwd=?1", [&directory.identity], project_row)
            .optional().map_err(|e| format!("Find imported project: {e}"))?;
        if let Some(project) = existing { return Ok(project); }
        Self::check_project_limit(&transaction)?;
        let id = uuid::Uuid::new_v4().to_string();
        transaction.execute("INSERT INTO projects(id,canonical_cwd,cwd,name) VALUES(?1,?2,?3,?4)", params![id, directory.identity, directory.cwd, directory.name])
            .map_err(|e| format!("Register project directory: {e}"))?;
        let project = Self::project_in(&transaction, &id)?;
        transaction.commit().map_err(|e| format!("Commit imported project: {e}"))?;
        Ok(project)
    }

    pub fn create_project(&self, name: &str, directory: Option<&ProjectDirectory>) -> Result<ProjectInfo, String> {
        let name = crate::project::validate_name(name)?;
        let transaction = self.project_transaction()?;
        if let Some(directory) = directory {
            let exists: bool = transaction.query_row("SELECT EXISTS(SELECT 1 FROM projects WHERE canonical_cwd=?1)", [&directory.identity], |row| row.get(0))
                .map_err(|e| format!("Check existing project folder: {e}"))?;
            if exists { return Err("A project already uses this folder; use the existing project".into()); }
        }
        Self::check_project_limit(&transaction)?;
        let id = uuid::Uuid::new_v4().to_string();
        transaction.execute("INSERT INTO projects(id,canonical_cwd,cwd,name) VALUES(?1,?2,?3,?4)",
            params![id, directory.map(|directory| directory.identity.clone()).unwrap_or_else(|| format!("virtual:{id}")), directory.map(|directory| &directory.cwd), name])
            .map_err(|e| format!("Create project: {e}"))?;
        let project = Self::project_in(&transaction, &id)?;
        transaction.commit().map_err(|e| format!("Commit created project: {e}"))?;
        Ok(project)
    }

    pub fn rename_project(&self, id: &str, name: &str) -> Result<ProjectInfo, String> {
        let name = crate::project::validate_name(name)?;
        let transaction = self.project_transaction()?;
        if transaction.execute("UPDATE projects SET name=?2 WHERE id=?1", params![id,name]).map_err(|e| format!("Rename project: {e}"))? != 1 {
            return Err("Project not found".into());
        }
        let project = Self::project_in(&transaction, id)?;
        transaction.commit().map_err(|e| format!("Commit renamed project: {e}"))?;
        Ok(project)
    }

    pub fn set_project_favorite(&self, id: &str, favorite: bool) -> Result<ProjectInfo, String> {
        let transaction = self.project_transaction()?;
        if transaction.execute("UPDATE projects SET favorite=?2 WHERE id=?1", params![id,favorite]).map_err(|e| format!("Set project favorite: {e}"))? != 1 {
            return Err("Project not found".into());
        }
        let project = Self::project_in(&transaction, id)?;
        transaction.commit().map_err(|e| format!("Commit project favorite: {e}"))?;
        Ok(project)
    }

    pub fn delete_project(&self, id: &str) -> Result<Vec<(String,u64)>, String> {
        let transaction = self.project_transaction()?;
        let project = Self::project_in(&transaction, id)?;
        let rows = {
            let mut statement = transaction.prepare("SELECT id,metadata FROM session_metadata").map_err(|e| e.to_string())?;
            let rows = statement.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))).map_err(|e| e.to_string())?;
            rows.collect::<Result<Vec<_>, _>>().map_err(|e| e.to_string())?
        };
        let mut detached = Vec::new();
        for (session_id, metadata) in rows {
            let info: SessionInfo = serde_json::from_str(&metadata).map_err(|e| format!("Read project session metadata: {e}"))?;
            if info.project_id.as_deref() == Some(id) || (!info.project_assigned && info.project_id.is_none() && project.cwd.as_deref().is_some_and(|cwd| crate::project::same_cwd(cwd, &info.cwd))) {
                let revision = crate::project::next_revision(info.project_revision)?;
                // 소속 필드만 바꾸고 제목·실행 폴더·대화 식별자·알 수 없는 필드는 보존한다.
                transaction.execute("UPDATE session_metadata SET metadata=json_set(metadata,'$.projectId',NULL,'$.projectAssigned',json('true'),'$.projectRevision',?2) WHERE id=?1", params![session_id,revision])
                    .map_err(|e| format!("Detach project session: {e}"))?;
                detached.push((session_id,revision));
            }
        }
        let layout: Option<String> = transaction.query_row("SELECT layout FROM workspace_layout WHERE id=1", [], |row| row.get(0)).optional().map_err(|e| e.to_string())?;
        if let Some(layout) = layout {
            let mut value: Value = serde_json::from_str(&layout).map_err(|e| format!("Read saved project layout: {e}"))?;
            let mut changed = false;
            if let Some(sessions) = value.get_mut("sessions").and_then(Value::as_array_mut) {
                for session in sessions {
                    let matches_id = session.get("projectId").and_then(Value::as_str) == Some(id);
                    let matches_cwd = session.get("projectAssigned").and_then(Value::as_bool) != Some(true)
                        && session.get("projectId").and_then(Value::as_str).is_none()
                        && project.cwd.as_deref().zip(session.get("cwd").and_then(Value::as_str)).is_some_and(|(left,right)| crate::project::same_cwd(left,right));
                    if matches_id || matches_cwd {
                        let revision = match session.get("projectRevision") { None => 0, Some(revision) => revision.as_u64().ok_or("Invalid saved project revision")? };
                        let revision = crate::project::next_revision(revision)?;
                        let fields = session.as_object_mut().ok_or("Invalid saved project session")?;
                        fields.insert("projectId".into(), Value::Null);
                        fields.insert("projectAssigned".into(), Value::Bool(true));
                        fields.insert("projectRevision".into(), Value::from(revision));
                        changed = true;
                    }
                }
            }
            if changed { transaction.execute("UPDATE workspace_layout SET layout=?1 WHERE id=1", [value.to_string()]).map_err(|e| format!("Detach saved project sessions: {e}"))?; }
        }
        transaction.execute("DELETE FROM projects WHERE id=?1", [id]).map_err(|e| format!("Delete project metadata: {e}"))?;
        transaction.commit().map_err(|e| format!("Commit project removal: {e}"))?;
        Ok(detached)
    }

    fn project_transaction(&self) -> Result<Transaction<'_>, String> {
        Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate).map_err(|e| format!("Begin project metadata update: {e}"))
    }

    fn check_project_limit(connection: &Connection) -> Result<(), String> {
        let count: usize = connection.query_row("SELECT COUNT(*) FROM projects", [], |row| row.get(0)).map_err(|e| e.to_string())?;
        if count >= crate::project::MAX_PROJECTS { return Err("250 projects limit reached".into()); }
        Ok(())
    }

    fn project_in(connection: &Connection, id: &str) -> Result<ProjectInfo, String> {
        connection.query_row("SELECT id,name,cwd,created_at,favorite FROM projects WHERE id=?1", [id], project_row)
            .optional().map_err(|e| format!("Read project: {e}"))?.ok_or_else(|| "Project not found".into())
    }

    fn project_exists(&self, id: &str) -> Result<bool, String> {
        Self::project_exists_in(&self.connection, id)
    }

    fn project_exists_in(connection: &Connection, id: &str) -> Result<bool, String> {
        connection.query_row("SELECT EXISTS(SELECT 1 FROM projects WHERE id=?1)", [id], |row| row.get(0)).map_err(|e| format!("Check project membership: {e}"))
    }

    fn ensure_project_in(connection: &Connection, id: Option<&str>) -> Result<(), String> {
        if let Some(id) = id { if !Self::project_exists_in(connection, id)? { return Err("Project not found".into()); } }
        Ok(())
    }

    pub fn projects(&self) -> Result<Vec<ProjectInfo>, String> {
        let mut statement = self.connection.prepare("SELECT id,name,cwd,created_at,favorite FROM projects ORDER BY favorite DESC,created_at DESC,id")
            .map_err(|e| format!("List imported projects: {e}"))?;
        let rows = statement.query_map([], project_row)
            .map_err(|e| format!("Query imported projects: {e}"))?;
        rows.collect::<Result<Vec<_>, _>>().map_err(|e| format!("Read imported project row: {e}"))
    }
}

fn project_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ProjectInfo> {
    Ok(ProjectInfo { id: row.get(0)?, name: row.get(1)?, cwd: row.get(2)?, created_at: row.get(3)?, favorite: row.get(4)? })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project_session(id: &str, project_id: Option<String>) -> SessionInfo {
        SessionInfo { id: id.into(), generation: 1, title: "보존할 대화 제목".into(), cwd: "C:\\workspace".into(), project_assigned: project_id.is_some(), project_id, project_revision: 0, thread_id: Some("keep-thread".into()), state: "running".into(), active: true }
    }

    #[test]
    fn stale_session_snapshots_cannot_undo_a_newer_project_assignment() {
        let store = Store::open(Path::new(":memory:")).unwrap();
        let first = store.create_project("이전 소속", None).unwrap();
        let second = store.create_project("새 소속", None).unwrap();
        let original = project_session("moving", Some(first.id));
        store.save_session(&original).unwrap();
        let mut moved = original.clone();
        moved.project_id = Some(second.id.clone());
        moved.project_revision = 1;
        store.save_session(&moved).unwrap();
        store.save_session(&original).unwrap();
        assert_eq!(store.sessions().unwrap()[0].project_id.as_deref(), Some(second.id.as_str()));
        assert_eq!(store.sessions().unwrap()[0].project_revision, 1);
        moved.project_id = None;
        moved.project_assigned = true;
        moved.project_revision = 2;
        store.save_session(&moved).unwrap();
        store.save_session(&original).unwrap();
        let detached = &store.sessions().unwrap()[0];
        assert!(detached.project_id.is_none());
        assert!(detached.project_assigned);
        assert_eq!(detached.project_revision, 2);
        assert_eq!(detached.thread_id, original.thread_id);
        assert_eq!(detached.cwd, original.cwd);
    }

    #[test]
    fn failed_project_migration_keeps_the_legacy_table_and_can_retry() {
        let root = std::env::temp_dir().join(format!("opendiy-project-migration-failure-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let database = root.join("metadata.sqlite3");
        {
            let connection = Connection::open(&database).unwrap();
            // 기존 열의 잘못된 값을 새 CHECK가 거부하면 DDL도 함께 원복되어야 한다.
            connection.execute_batch("CREATE TABLE projects (id TEXT PRIMARY KEY NOT NULL,canonical_cwd TEXT UNIQUE NOT NULL,cwd TEXT NOT NULL,name TEXT NOT NULL,created_at INTEGER NOT NULL,favorite INTEGER NOT NULL);
                INSERT INTO projects VALUES('legacy','C:/existing','C:/existing','원본',123,2);").unwrap();
        }
        assert!(Store::open(&database).err().unwrap().contains("Migrate project metadata"));
        {
            let connection = Connection::open(&database).unwrap();
            let original: (String,String,String,i64,i64) = connection.query_row("SELECT canonical_cwd,cwd,name,created_at,favorite FROM projects WHERE id='legacy'", [], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?))).unwrap();
            assert_eq!(original, ("C:/existing".into(),"C:/existing".into(),"원본".into(),123,2));
            let legacy_cwd_required: bool = connection.query_row("SELECT \"notnull\" FROM pragma_table_info('projects') WHERE name='cwd'", [], |row| row.get(0)).unwrap();
            assert!(legacy_cwd_required);
            let partial: usize = connection.query_row("SELECT COUNT(*) FROM sqlite_master WHERE name='projects_v050'", [], |row| row.get(0)).unwrap();
            assert_eq!(partial, 0);
            connection.execute("UPDATE projects SET favorite=1 WHERE id='legacy'", []).unwrap();
        }
        {
            let store = Store::open(&database).unwrap();
            let projects = store.projects().unwrap();
            assert_eq!(projects[0].id, "legacy");
            assert_eq!(projects[0].cwd.as_deref(), Some("C:/existing"));
            assert!(projects[0].favorite);
            store.create_project("재시도 성공", None).unwrap();
        }
        for name in ["metadata.sqlite3", "metadata.sqlite3-wal", "metadata.sqlite3-shm"] {
            let file = root.join(name); if file.exists() { std::fs::remove_file(file).unwrap(); }
        }
        std::fs::remove_dir(root).unwrap();
    }

    #[test]
    fn logical_projects_validate_names_preserve_folder_identity_and_bound_count() {
        let store = Store::open(Path::new(":memory:")).unwrap();
        let directory = crate::project::directory(std::env::current_dir().unwrap().to_str().unwrap()).unwrap();
        let imported = store.import_project(&directory).unwrap();
        assert!(store.create_project("가져온 폴더의 중복", Some(&directory)).unwrap_err().contains("use the existing project"));
        assert_eq!(store.projects().unwrap()[0].name, imported.name);
        let first = store.create_project("  첫 논리 프로젝트  ", None).unwrap();
        let second = store.create_project("폴더 없는 다른 프로젝트", None).unwrap();
        assert_eq!(first.name, "첫 논리 프로젝트");
        assert_ne!(first.id, second.id);
        assert_ne!(first.id, imported.id);
        assert!(first.cwd.is_none());
        let virtual_project = store.create_project("폴더 없는 프로젝트", None).unwrap();
        assert!(virtual_project.cwd.is_none());
        assert!(store.create_project(" \t ", None).is_err());
        assert!(store.rename_project(&first.id, "줄\n바꿈").is_err());
        assert!(store.rename_project(&first.id, &"한".repeat(121)).is_err());
        assert_eq!(store.rename_project(&first.id, &"한".repeat(120)).unwrap().name.chars().count(), 120);
        assert!(store.set_project_favorite(&first.id, true).unwrap().favorite);
        assert_eq!(store.projects().unwrap()[0].id, first.id);
        assert!(store.connection.execute("UPDATE projects SET favorite=2 WHERE id=?1", [&first.id]).is_err());
        for index in 4..crate::project::MAX_PROJECTS { store.create_project(&format!("project-{index}"), None).unwrap(); }
        assert_eq!(store.projects().unwrap().len(), 250);
        assert!(store.create_project("넘침", None).is_err());
        assert_eq!(store.import_project(&directory).unwrap().id, imported.id);
        let unknown = ProjectDirectory { identity: "other-canonical".into(), cwd: "C:\\other".into(), name: "other".into() };
        assert!(store.import_project(&unknown).is_err());
    }

    #[test]
    fn named_folder_project_import_reuses_identity_and_rejects_duplicate_creation() {
        let store = Store::open(Path::new(":memory:")).unwrap();
        let folder = std::env::current_dir().unwrap();
        let directory = crate::project::directory(folder.to_str().unwrap()).unwrap();
        let project = store.create_project("사용자 지정 이름", Some(&directory)).unwrap();
        let alias = folder.join(".");
        let alias = crate::project::directory(alias.to_str().unwrap()).unwrap();
        assert!(store.create_project("바꾸면 안 되는 이름", Some(&alias)).unwrap_err().contains("use the existing project"));
        let imported = store.import_project(&alias).unwrap();
        assert_eq!(imported.id, project.id);
        assert_eq!(imported.name, "사용자 지정 이름");
        assert_eq!(imported.cwd.as_deref(), Some(directory.cwd.as_str()));
        assert_eq!(store.projects().unwrap().len(), 1);
    }

    #[test]
    fn deletion_rolls_back_membership_and_only_changes_owned_grouping_metadata() {
        let store = Store::open(Path::new(":memory:")).unwrap();
        let project = store.create_project("삭제 대상", None).unwrap();
        let info = project_session("owned", Some(project.id.clone()));
        store.save_session(&info).unwrap();
        store.save_session(&project_session("unrelated", None)).unwrap();
        store.connection.execute("UPDATE session_metadata SET metadata=json_set(metadata,'$.futureField','preserve-me') WHERE id='owned'", []).unwrap();
        let original: String = store.connection.query_row("SELECT metadata FROM session_metadata WHERE id='owned'", [], |row| row.get(0)).unwrap();
        let unrelated: String = store.connection.query_row("SELECT metadata FROM session_metadata WHERE id='unrelated'", [], |row| row.get(0)).unwrap();
        let layout = serde_json::json!({"version":1,"unknownRoot":{"keep":"me"},"sessions":[{"title":"saved","cwd":"C:/workspace","projectId":project.id,"threadId":"keep-saved-thread","future":"keep-saved-field"},{"title":"other","cwd":"D:/other","future":42}]});
        store.save_layout(&layout).unwrap();
        store.connection.execute_batch("CREATE TRIGGER reject_project_delete BEFORE DELETE ON projects BEGIN SELECT RAISE(ABORT,'injected delete failure'); END;").unwrap();
        assert!(store.delete_project(&project.id).unwrap_err().contains("injected delete failure"));
        assert_eq!(store.projects().unwrap()[0].id, project.id);
        let failed: String = store.connection.query_row("SELECT metadata FROM session_metadata WHERE id='owned'", [], |row| row.get(0)).unwrap();
        assert_eq!(failed, original);
        assert_eq!(store.load_layout().unwrap(), layout);
        store.connection.execute_batch("DROP TRIGGER reject_project_delete;").unwrap();
        assert_eq!(store.delete_project(&project.id).unwrap(), vec![("owned".into(),1)]);
        let detached: String = store.connection.query_row("SELECT metadata FROM session_metadata WHERE id='owned'", [], |row| row.get(0)).unwrap();
        let value: Value = serde_json::from_str(&detached).unwrap();
        assert!(value["projectId"].is_null());
        assert_eq!(value["projectAssigned"], true);
        assert_eq!(value["projectRevision"], 1);
        assert_eq!(value["cwd"], "C:\\workspace");
        assert_eq!(value["threadId"], "keep-thread");
        assert_eq!(value["title"], "보존할 대화 제목");
        assert_eq!(value["futureField"], "preserve-me");
        let after: String = store.connection.query_row("SELECT metadata FROM session_metadata WHERE id='unrelated'", [], |row| row.get(0)).unwrap();
        assert_eq!(after, unrelated);
        let saved = store.load_layout().unwrap();
        assert_eq!(saved["unknownRoot"], layout["unknownRoot"]);
        assert_eq!(saved["sessions"][1], layout["sessions"][1]);
        assert!(saved["sessions"][0]["projectId"].is_null());
        assert_eq!(saved["sessions"][0]["projectAssigned"], true);
        assert_eq!(saved["sessions"][0]["projectRevision"], 1);
        assert_eq!(saved["sessions"][0]["threadId"], "keep-saved-thread");
        assert_eq!(saved["sessions"][0]["future"], "keep-saved-field");
        assert!(store.save_session(&info).unwrap_err().contains("Project not found"));
        assert!(store.delete_project(&project.id).is_err());
    }

    #[test]
    fn legacy_project_migration_preserves_raw_records_and_is_reopen_safe() {
        let root = std::env::temp_dir().join(format!("opendiy-project-migration-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let database = root.join("metadata.sqlite3");
        let raw = "{ \"id\":\"legacy-session\",\"generation\":7,\"title\":\"원문\",\"cwd\":\"C:/old\",\"state\":\"idle\",\"future\":true }";
        let layout = "{ \"version\": 1, \"sessions\": [] }";
        {
            let connection = Connection::open(&database).unwrap();
            connection.execute_batch("CREATE TABLE projects (id TEXT PRIMARY KEY NOT NULL,canonical_cwd TEXT UNIQUE NOT NULL,cwd TEXT NOT NULL,name TEXT NOT NULL,created_at INTEGER NOT NULL DEFAULT (unixepoch()));
                CREATE TABLE session_metadata (id TEXT PRIMARY KEY NOT NULL,generation INTEGER NOT NULL,metadata TEXT NOT NULL,updated_at INTEGER NOT NULL DEFAULT (unixepoch()));
                CREATE TABLE workspace_layout (id INTEGER PRIMARY KEY,layout TEXT NOT NULL);
                CREATE TABLE app_settings (key TEXT PRIMARY KEY,value TEXT NOT NULL);
                INSERT INTO projects VALUES('legacy-project','C:/old','C:/old','기존 프로젝트',123);
                INSERT INTO app_settings VALUES('existing-key','unchanged-value');").unwrap();
            connection.execute("INSERT INTO session_metadata(id,generation,metadata) VALUES('legacy-session',7,?1)", [raw]).unwrap();
            connection.execute("INSERT INTO workspace_layout VALUES(1,?1)", [layout]).unwrap();
        }
        let logical;
        {
            let store = Store::open(&database).unwrap();
            let project = &store.projects().unwrap()[0];
            assert_eq!(project.id, "legacy-project");
            assert_eq!(project.created_at, 123);
            assert_eq!(project.cwd.as_deref(), Some("C:/old"));
            assert!(!project.favorite);
            assert!(!store.sessions().unwrap()[0].project_assigned);
            assert!(store.sessions().unwrap()[0].project_id.is_none());
            logical = store.create_project("새 논리 프로젝트", None).unwrap();
            store.set_project_favorite(&logical.id, true).unwrap();
        }
        for _ in 0..2 {
            let store = Store::open(&database).unwrap();
            assert_eq!(store.projects().unwrap().len(), 2);
            assert!(store.projects().unwrap().iter().any(|project| project.id == logical.id && project.favorite && project.cwd.is_none()));
            let metadata: String = store.connection.query_row("SELECT metadata FROM session_metadata WHERE id='legacy-session'", [], |row| row.get(0)).unwrap();
            let saved_layout: String = store.connection.query_row("SELECT layout FROM workspace_layout", [], |row| row.get(0)).unwrap();
            let setting: String = store.connection.query_row("SELECT value FROM app_settings WHERE key='existing-key'", [], |row| row.get(0)).unwrap();
            assert_eq!(metadata, raw);
            assert_eq!(saved_layout, layout);
            assert_eq!(setting, "unchanged-value");
        }
        for name in ["metadata.sqlite3", "metadata.sqlite3-wal", "metadata.sqlite3-shm"] {
            let file = root.join(name); if file.exists() { std::fs::remove_file(file).unwrap(); }
        }
        std::fs::remove_dir(root).unwrap();
    }
    #[test]
    fn language_survives_database_reopen_and_preserves_other_preferences() {
        use crate::ui_language::UiLanguage;
        let directory = std::env::temp_dir().join(format!("agent-grid-language-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let database = directory.join("workspace.sqlite3");
        {
            let store = Store::open(&database).unwrap();
            assert_eq!(store.ui_language().unwrap(), UiLanguage::Ko);
            store.set_codex_executable(Some("C:\\tools\\codex.exe")).unwrap();
            store.set_ui_language(UiLanguage::En).unwrap();
        }
        {
            let store = Store::open(&database).unwrap();
            assert_eq!(store.ui_language().unwrap(), UiLanguage::En);
            assert_eq!(store.codex_executable().unwrap().as_deref(), Some("C:\\tools\\codex.exe"));
            store.set_ui_language(UiLanguage::Ko).unwrap();
            store.connection.execute("UPDATE app_settings SET value='unsupported' WHERE key='ui_language'", []).unwrap();
            assert!(store.ui_language().is_err());
            store.set_ui_language(UiLanguage::En).unwrap();
            assert_eq!(store.ui_language().unwrap(), UiLanguage::En);
        }
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn legacy_database_migrates_and_executable_reset_preserves_other_records() {
        let directory = std::env::temp_dir().join(format!("agent-grid-setting-store-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let database = directory.join("workspace.sqlite3");
        let layout = serde_json::json!({"version":1,"sessions":[],"columns":2});
        let project = crate::project::directory(directory.to_str().unwrap()).unwrap();
        {
            // The previous schema had no settings table and metadata had no active flag.
            let legacy = Connection::open(&database).unwrap();
            legacy.execute_batch("CREATE TABLE session_metadata (id TEXT PRIMARY KEY NOT NULL, generation INTEGER NOT NULL CHECK (generation > 0), metadata TEXT NOT NULL, updated_at INTEGER NOT NULL DEFAULT (unixepoch()));
                CREATE TABLE workspace_layout (id INTEGER PRIMARY KEY CHECK (id = 1), layout TEXT NOT NULL);
                CREATE TABLE projects (id TEXT PRIMARY KEY NOT NULL, canonical_cwd TEXT UNIQUE NOT NULL, cwd TEXT NOT NULL, name TEXT NOT NULL, created_at INTEGER NOT NULL DEFAULT (unixepoch()));").unwrap();
            let metadata = serde_json::json!({"id":"existing-session","generation":3,"title":"existing title","cwd":directory.to_string_lossy(),"state":"idle"});
            legacy.execute("INSERT INTO session_metadata(id,generation,metadata) VALUES('existing-session',3,?1)", [metadata.to_string()]).unwrap();
            legacy.execute("INSERT INTO workspace_layout(id,layout) VALUES(1,?1)", [layout.to_string()]).unwrap();
            legacy.execute("INSERT INTO projects(id,canonical_cwd,cwd,name) VALUES('existing-project',?1,?2,?3)", params![project.identity, project.cwd, project.name]).unwrap();
            let settings_tables: usize = legacy.query_row("SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='app_settings'", [], |row| row.get(0)).unwrap();
            assert_eq!(settings_tables, 0);
        }
        {
            let store = Store::open(&database).unwrap();
            assert_eq!(store.load_layout().unwrap(), layout);
            assert!(!store.sessions().unwrap()[0].active);
            assert_eq!(store.projects().unwrap()[0].id, "existing-project");
            store.connection.execute("INSERT INTO app_settings(key,value) VALUES('unrelated','keep-me')", []).unwrap();
            store.set_codex_executable(Some("C:\\native\\codex.exe")).unwrap();
        }
        {
            let store = Store::open(&database).unwrap();
            assert_eq!(store.codex_executable().unwrap().as_deref(), Some("C:\\native\\codex.exe"));
            assert_eq!(store.load_layout().unwrap(), layout);
            assert_eq!(store.sessions().unwrap()[0].generation, 3);
            assert_eq!(store.projects().unwrap()[0].id, "existing-project");
            store.set_codex_executable(Some("C:\\other\\codex.exe")).unwrap();
            assert_eq!(store.codex_executable().unwrap().as_deref(), Some("C:\\other\\codex.exe"));
            store.set_codex_executable(None).unwrap();
        }
        {
            let store = Store::open(&database).unwrap();
            assert!(store.codex_executable().unwrap().is_none());
            assert_eq!(store.load_layout().unwrap(), layout);
            assert_eq!(store.sessions().unwrap()[0].id, "existing-session");
            assert_eq!(store.projects().unwrap()[0].id, "existing-project");
            let unrelated: String = store.connection.query_row("SELECT value FROM app_settings WHERE key='unrelated'", [], |row| row.get(0)).unwrap();
            assert_eq!(unrelated, "keep-me");
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
        let mut info = SessionInfo { id: "test".into(), generation: 2, title: "title".into(), cwd: "directory".into(), project_id: None, project_assigned: false, project_revision: 0, thread_id: None, state: "running".into(), active: true };
        store.save_session(&info).unwrap();
        info.generation = 1;
        info.title = "stale".into();
        store.save_session(&info).unwrap();
        let restored = store.sessions().unwrap();
        assert_eq!(restored[0].generation, 2);
        assert_eq!(restored[0].title, "title");
        assert_eq!(restored[0].state, "restored");
        assert!(!restored[0].active);
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
            store.save_session(&SessionInfo { id: format!("session-{index:04}"), generation: 1, title: "fixture".into(), cwd: ".".into(), project_id: None, project_assigned: false, project_revision: 0, thread_id: None, state: "exited".into(), active: false }).unwrap();
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
            store.save_session(&SessionInfo { id: "existing".into(), generation: 1, title: "Existing session".into(), cwd: first.cwd.clone().unwrap(), project_id: None, project_assigned: false, project_revision: 0, thread_id: None, state: "exited".into(), active: false }).unwrap();
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
