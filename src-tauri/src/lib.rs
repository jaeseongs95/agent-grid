mod codex;
mod codex_settings;
mod conversation_attachments;
mod executable_picker;
mod flow;
mod launch_gate;
mod project;
pub mod process_job;
mod owned_tui_control;
mod session;
pub mod self_test;
mod storage;
mod ui_language;

use session::{BackendEvent, Engine, SessionInfo, StartRequest};
use std::sync::{atomic::{AtomicBool, Ordering}, Arc};
use tauri::{menu::{Menu, MenuItem}, tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent}, Emitter, Manager, State};

#[derive(Default)]
struct ExitState(AtomicBool);

struct UiSettings {
    english: AtomicBool,
    show: MenuItem<tauri::Wry>,
    exit: MenuItem<tauri::Wry>,
}

#[tauri::command]
async fn get_ui_language(engine: State<'_, Engine>) -> Result<ui_language::UiLanguage, String> {
    let engine = engine.inner().clone();
    tauri::async_runtime::spawn_blocking(move || engine.get_ui_language()).await.map_err(|e| format!("Read app language worker: {e}"))?
}

#[tauri::command]
async fn set_ui_language(settings: State<'_, UiSettings>, engine: State<'_, Engine>, language: ui_language::UiLanguage) -> Result<serde_json::Value, String> {
    let engine = engine.inner().clone();
    tauri::async_runtime::spawn_blocking(move || engine.set_ui_language(language)).await.map_err(|e| format!("Save app language worker: {e}"))??;
    // The store lock has been released before dispatching native menu updates.
    settings.show.set_text(language.text("OpenDIY Desktop 열기", "Open OpenDIY Desktop")).map_err(|e| format!("Update tray open label: {e}"))?;
    settings.exit.set_text(language.text("종료…", "Exit…")).map_err(|e| format!("Update tray exit label: {e}"))?;
    settings.english.store(language == ui_language::UiLanguage::En, Ordering::Release);
    Ok(serde_json::json!({
        "show": settings.show.text().map_err(|e| format!("Read tray open label: {e}"))?,
        "exit": settings.exit.text().map_err(|e| format!("Read tray exit label: {e}"))?,
    }))
}

fn workspace_directory(default: std::path::PathBuf, override_path: Option<std::ffi::OsString>) -> Result<std::path::PathBuf, String> {
    match override_path {
        None => Ok(default),
        Some(path) => {
            if path.is_empty() { return Err("AGENT_GRID_WORKSPACE_DIR is empty".into()); }
            let path = std::path::PathBuf::from(path);
            if !path.is_absolute() { return Err("AGENT_GRID_WORKSPACE_DIR must be an absolute directory path".into()); }
            Ok(path)
        }
    }
}

#[tauri::command]
async fn prepare_runtime(engine: State<'_, Engine>) -> Result<(), String> {
    let engine = engine.inner().clone();
    tauri::async_runtime::spawn_blocking(move || engine.prepare_runtime()).await.map_err(|error| format!("공유 서버 준비 작업자: {error}"))?
}

#[tauri::command]
async fn start_session(engine: State<'_, Engine>, request: StartRequest) -> Result<SessionInfo, String> {
    let engine = engine.inner().clone();
    tauri::async_runtime::spawn_blocking(move || engine.start(request)).await.map_err(|e| format!("start session worker: {e}"))?
}
#[tauri::command]
fn list_sessions(engine: State<'_, Engine>) -> Result<Vec<SessionInfo>, String> { engine.list() }
#[tauri::command]
async fn session_write(engine: State<'_, Engine>, id: String, generation: u64, data: String) -> Result<(), String> {
    let engine = engine.inner().clone();
    tauri::async_runtime::spawn_blocking(move || engine.write(&id, generation, &data)).await.map_err(|e| format!("terminal write worker: {e}"))?
}
#[tauri::command]
async fn session_resize(engine: State<'_, Engine>, id: String, generation: u64, cols: u16, rows: u16) -> Result<(), String> {
    let engine = engine.inner().clone();
    tauri::async_runtime::spawn_blocking(move || engine.resize(&id, generation, cols, rows)).await.map_err(|e| format!("terminal resize worker: {e}"))?
}
#[tauri::command]
async fn session_stop(engine: State<'_, Engine>, id: String, generation: u64) -> Result<(), String> {
    let engine = engine.inner().clone();
    tauri::async_runtime::spawn_blocking(move || engine.stop(&id, generation)).await.map_err(|e| format!("terminal cleanup worker: {e}"))?
}
#[tauri::command]
fn session_output_ack(engine: State<'_, Engine>, id: String, generation: u64, seq: u64) -> Result<(), String> { engine.acknowledge(&id, generation, seq) }
#[tauri::command]
fn load_layout(engine: State<'_, Engine>) -> Result<serde_json::Value, String> { engine.load_layout() }
#[tauri::command]
fn save_layout(engine: State<'_, Engine>, layout: serde_json::Value) -> Result<(), String> { engine.save_layout(&layout) }
#[tauri::command]
async fn list_external(engine: State<'_, Engine>) -> Result<Vec<codex::ExternalSession>, String> {
    let engine = engine.inner().clone();
    tauri::async_runtime::spawn_blocking(move || engine.list_external()).await.map_err(|e| format!("external session observer: {e}"))?
}
#[tauri::command]
async fn get_codex_settings(engine: State<'_, Engine>) -> Result<codex_settings::CodexSettings, String> {
    let engine = engine.inner().clone();
    tauri::async_runtime::spawn_blocking(move || engine.get_codex_settings()).await.map_err(|e| format!("Read executable setting worker: {e}"))?
}
#[tauri::command]
async fn set_codex_executable(engine: State<'_, Engine>, path: Option<String>) -> Result<codex_settings::CodexSettings, String> {
    let engine = engine.inner().clone();
    tauri::async_runtime::spawn_blocking(move || engine.set_codex_executable(path.as_deref())).await.map_err(|e| format!("Save executable setting worker: {e}"))?
}
#[tauri::command]
async fn choose_codex_executable(app: tauri::AppHandle, engine: State<'_, Engine>, picker: State<'_, executable_picker::ExecutablePicker>, ui: State<'_, UiSettings>) -> Result<Option<codex_settings::CodexSettings>, String> {
    let engine = engine.inner().clone();
    let picker = picker.inner().clone();
    let language = if ui.english.load(Ordering::Acquire) { ui_language::UiLanguage::En } else { ui_language::UiLanguage::Ko };
    #[cfg(windows)]
    let owner = app.get_webview_window("main").ok_or("Main window is unavailable")?.hwnd().map_err(|e| format!("Get executable chooser parent window: {e}"))?.0 as isize;
    #[cfg(not(windows))]
    let owner = { let _ = app; 0 };
    tauri::async_runtime::spawn_blocking(move || picker.choose(owner, language)?.map(|path| engine.set_codex_executable(Some(&path))).transpose())
        .await.map_err(|e| format!("Executable chooser worker: {e}"))?
}
#[tauri::command]
async fn list_projects(engine: State<'_, Engine>) -> Result<Vec<project::ProjectInfo>, String> {
    let engine = engine.inner().clone();
    tauri::async_runtime::spawn_blocking(move || engine.list_projects()).await.map_err(|e| format!("List projects worker: {e}"))?
}
#[tauri::command]
async fn import_project(engine: State<'_, Engine>, cwd: String) -> Result<project::ProjectInfo, String> {
    let engine = engine.inner().clone();
    tauri::async_runtime::spawn_blocking(move || engine.import_project(&cwd)).await.map_err(|e| format!("Import project worker: {e}"))?
}
#[tauri::command]
async fn create_project(engine: State<'_, Engine>, name: String, cwd: Option<String>) -> Result<project::ProjectInfo, String> {
    let engine = engine.inner().clone();
    tauri::async_runtime::spawn_blocking(move || engine.create_project(&name, cwd.as_deref())).await.map_err(|e| format!("Create project worker: {e}"))?
}
#[tauri::command]
async fn rename_project(engine: State<'_, Engine>, id: String, name: String) -> Result<project::ProjectInfo, String> {
    let engine = engine.inner().clone();
    tauri::async_runtime::spawn_blocking(move || engine.rename_project(&id, &name)).await.map_err(|e| format!("Rename project worker: {e}"))?
}
#[tauri::command]
async fn set_project_favorite(engine: State<'_, Engine>, id: String, favorite: bool) -> Result<project::ProjectInfo, String> {
    let engine = engine.inner().clone();
    tauri::async_runtime::spawn_blocking(move || engine.set_project_favorite(&id, favorite)).await.map_err(|e| format!("Favorite project worker: {e}"))?
}
#[tauri::command]
async fn delete_project(engine: State<'_, Engine>, id: String) -> Result<(), String> {
    let engine = engine.inner().clone();
    tauri::async_runtime::spawn_blocking(move || engine.delete_project(&id)).await.map_err(|e| format!("Delete project worker: {e}"))?
}
#[tauri::command]
async fn move_session_to_project(engine: State<'_, Engine>, id: String, generation: u64, project_id: Option<String>) -> Result<SessionInfo, String> {
    let engine = engine.inner().clone();
    tauri::async_runtime::spawn_blocking(move || engine.move_session_to_project(&id, generation, project_id.as_deref())).await.map_err(|e| format!("Move session worker: {e}"))?
}
#[tauri::command]
async fn choose_project_folder(app: tauri::AppHandle, picker: State<'_, project::FolderPicker>, ui: State<'_, UiSettings>) -> Result<Option<String>, String> {
    let picker = picker.inner().clone();
    let language = if ui.english.load(Ordering::Acquire) { ui_language::UiLanguage::En } else { ui_language::UiLanguage::Ko };
    #[cfg(windows)]
    let owner = app.get_webview_window("main").ok_or("Main window is unavailable")?.hwnd().map_err(|e| format!("Get project chooser parent window: {e}"))?.0 as isize;
    #[cfg(not(windows))]
    let owner = { let _ = app; 0 };
    tauri::async_runtime::spawn_blocking(move || picker.choose(owner, language)).await.map_err(|e| format!("Project folder chooser worker: {e}"))?
}
#[tauri::command]
async fn choose_project(app: tauri::AppHandle, engine: State<'_, Engine>, picker: State<'_, project::FolderPicker>, ui: State<'_, UiSettings>) -> Result<Option<project::ProjectInfo>, String> {
    let engine = engine.inner().clone();
    let picker = picker.inner().clone();
    let language = if ui.english.load(Ordering::Acquire) { ui_language::UiLanguage::En } else { ui_language::UiLanguage::Ko };
    #[cfg(windows)]
    let owner = app.get_webview_window("main").ok_or("Main window is unavailable")?.hwnd().map_err(|e| format!("Get project chooser parent window: {e}"))?.0 as isize;
    #[cfg(not(windows))]
    let owner = { let _ = app; 0 };
    tauri::async_runtime::spawn_blocking(move || {
        picker.choose(owner, language)?.map(|cwd| engine.import_project(&cwd)).transpose()
    }).await.map_err(|e| format!("Project folder chooser worker: {e}"))?
}
#[tauri::command]
async fn catalog_list(engine: State<'_, Engine>, request: codex::CatalogQuery) -> Result<codex::CatalogPage, String> {
    let engine = engine.inner().clone();
    tauri::async_runtime::spawn_blocking(move || engine.catalog_list(request)).await.map_err(|e| format!("Session catalog worker: {e}"))?
}
#[tauri::command]
async fn catalog_preview(engine: State<'_, Engine>, thread_id: String, cursor: Option<String>) -> Result<codex::CatalogPreview, String> {
    let engine = engine.inner().clone();
    tauri::async_runtime::spawn_blocking(move || engine.catalog_preview(&thread_id, cursor)).await.map_err(|e| format!("Session preview worker: {e}"))?
}
#[tauri::command]
async fn extensions_list(engine: State<'_, Engine>, cwd: Option<String>) -> Result<codex::ExtensionsSnapshot, String> {
    let engine = engine.inner().clone();
    tauri::async_runtime::spawn_blocking(move || engine.extensions_list(cwd)).await.map_err(|e| format!("Extension list worker: {e}"))?
}
#[tauri::command]
async fn extensions_mutate(engine: State<'_, Engine>, cwd: Option<String>, request: codex::ExtensionMutation) -> Result<codex::ExtensionsSnapshot, String> {
    let engine = engine.inner().clone();
    tauri::async_runtime::spawn_blocking(move || engine.extensions_mutate(cwd, request)).await.map_err(|e| format!("Extension mutation worker: {e}"))?
}
#[tauri::command]
async fn mobile_control(engine: State<'_, Engine>, id: String, generation: u64, action: codex::MobileAction) -> Result<codex::MobileReply, String> {
    let engine = engine.inner().clone();
    tauri::async_runtime::spawn_blocking(move || engine.mobile_control(&id, generation, action)).await.map_err(|e| format!("Mobile control worker: {e}"))?
}
#[tauri::command]
async fn subagents_request(engine: State<'_, Engine>, id: String, generation: u64, request: codex::SubagentRequest) -> Result<serde_json::Value, String> {
    let engine = engine.inner().clone();
    tauri::async_runtime::spawn_blocking(move || engine.subagents_request(&id, generation, request)).await.map_err(|e| format!("하위 에이전트 요청 작업자: {e}"))?
}
#[tauri::command]
async fn cron_request(engine: State<'_, Engine>, id: String, generation: u64, request: codex::CronRequest) -> Result<serde_json::Value,String> {
    let engine=engine.inner().clone();
    tauri::async_runtime::spawn_blocking(move||engine.cron_request(&id,generation,request)).await.map_err(|e|format!("예약 요청 작업자: {e}"))?
}
#[tauri::command]
async fn conversation_request(engine: State<'_, Engine>, id: String, generation: u64, request: codex::ConversationRequest) -> Result<serde_json::Value, String> {
    let engine = engine.inner().clone();
    tauri::async_runtime::spawn_blocking(move || engine.conversation_request(&id, generation, request)).await.map_err(|e| format!("대화 요청 작업자: {e}"))?
}
#[tauri::command]
async fn list_pending_questions(engine: State<'_, Engine>) -> Result<serde_json::Value,String> {
    let engine = engine.inner().clone();
    tauri::async_runtime::spawn_blocking(move || engine.list_pending_questions()).await.map_err(|e|format!("질문함 조회 작업자: {e}"))?
}
#[tauri::command]
async fn respond_pending_question(engine: State<'_, Engine>, session_id: String, generation: u64, request_id: String, answers: Option<serde_json::Value>, decision: Option<String>) -> Result<serde_json::Value,String> {
    let result = match (answers,decision) {
        (Some(answers),None) => serde_json::json!({"answers":answers}),
        (None,Some(decision)) => serde_json::json!({"decision":decision}),
        _ => return Err("질문 답변 또는 승인 결정 중 하나만 지정하세요.".into()),
    };
    let engine = engine.inner().clone();
    tauri::async_runtime::spawn_blocking(move || engine.conversation_request(&session_id,generation,codex::ConversationRequest::Respond{request_id,result})).await.map_err(|e|format!("질문함 응답 작업자: {e}"))?
}
#[tauri::command]
async fn choose_conversation_attachments(app: tauri::AppHandle, engine: State<'_, Engine>, ui: State<'_, UiSettings>) -> Result<Vec<conversation_attachments::ConversationAttachment>,String> {
    let attachments = engine.conversation_attachments();
    let language = if ui.english.load(Ordering::Acquire) {ui_language::UiLanguage::En} else {ui_language::UiLanguage::Ko};
    #[cfg(windows)]
    let owner = app.get_webview_window("main").ok_or("메인 창을 찾을 수 없습니다.")?.hwnd().map_err(|e|format!("첨부 선택창 부모 확인: {e}"))?.0 as isize;
    #[cfg(not(windows))]
    let owner = {let _=app;0};
    tauri::async_runtime::spawn_blocking(move || attachments.choose(owner,language)).await.map_err(|e|format!("첨부 선택 작업자: {e}"))?
}
#[tauri::command]
async fn stage_conversation_image(engine: State<'_, Engine>, id: String, generation: u64, data_url: String) -> Result<conversation_attachments::ConversationAttachment,String> {
    let engine = engine.inner().clone();
    tauri::async_runtime::spawn_blocking(move || engine.stage_conversation_image(&id,generation,&data_url)).await.map_err(|e|format!("첨부 이미지 준비 작업자: {e}"))?
}
#[tauri::command]
async fn cleanup_conversation_attachment(engine: State<'_, Engine>, id: String, generation: u64, path: String) -> Result<(),String> {
    let engine = engine.inner().clone();
    tauri::async_runtime::spawn_blocking(move || engine.cleanup_conversation_attachment(&id,generation,&path)).await.map_err(|e|format!("첨부 초안 정리 작업자: {e}"))?
}
#[tauri::command]
async fn weekly_usage(engine: State<'_, Engine>, manual: Option<bool>) -> Result<codex::WeeklyUsageSnapshot, String> {
    let engine = engine.inner().clone();
    tauri::async_runtime::spawn_blocking(move || engine.weekly_usage(manual.unwrap_or(false))).await.map_err(|e| format!("Weekly usage worker: {e}"))?
}
#[tauri::command]
async fn choose_skill_folder(app: tauri::AppHandle, picker: State<'_, project::FolderPicker>, ui: State<'_, UiSettings>) -> Result<Option<String>, String> {
    let picker = picker.inner().clone();
    let language = if ui.english.load(Ordering::Acquire) { ui_language::UiLanguage::En } else { ui_language::UiLanguage::Ko };
    #[cfg(windows)]
    let owner = app.get_webview_window("main").ok_or("Main window is unavailable")?.hwnd().map_err(|e| format!("Get skill chooser parent window: {e}"))?.0 as isize;
    #[cfg(not(windows))]
    let owner = { let _ = app; 0 };
    tauri::async_runtime::spawn_blocking(move || picker.choose_skill(owner, language)).await.map_err(|e| format!("Skill folder chooser worker: {e}"))?
}
#[tauri::command]
async fn unconfirmed_exit_available(engine: State<'_, Engine>) -> Result<bool, String> {
    let engine = engine.inner().clone();
    tauri::async_runtime::spawn_blocking(move || engine.unconfirmed_exit_available()).await.map_err(|e| format!("application cleanup status worker: {e}"))?
}
#[tauri::command]
async fn complete_exit(app: tauri::AppHandle, engine: State<'_, Engine>, confirmed: bool, allow_unconfirmed_remote: Option<bool>) -> Result<(), String> {
    if !confirmed { return Err("explicit exit confirmation is required".into()); }
    let engine = engine.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        if allow_unconfirmed_remote == Some(true) { engine.shutdown_with_unconfirmed_remote() } else { engine.shutdown() }
    }).await.map_err(|e| format!("application cleanup worker: {e}"))??;
    app.state::<ExitState>().0.store(true, Ordering::Release);
    app.exit(0);
    Ok(())
}

fn show_main(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        if let Err(error) = window.show().and_then(|_| window.unminimize()).and_then(|_| window.set_focus()) {
            let _ = app.emit("application-error", format!("cannot show main window: {error}"));
        } else {
            let _ = app.emit("application-visibility", true);
        }
    }
}

pub fn run() {
    if let Err(error) = process_job::install_app_job() {
        // Fail closed: starting unowned background processes is never a fallback.
        eprintln!("OpenDIY Desktop startup failed: {error}");
        std::process::exit(1);
    }
    let result = tauri::Builder::default()
        // Claim the app before setup opens storage or restores any CLI sessions.
        // A repeated launch only reveals the existing window, including from the tray.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| show_main(app)))
        .manage(ExitState::default())
        .manage(project::FolderPicker::default())
        .manage(executable_picker::ExecutablePicker::default())
        .setup(|app| {
            // Process-scoped isolation for this app's metadata only. Codex's
            // home, credentials and configuration remain entirely unchanged.
            let directory = workspace_directory(app.path().app_data_dir()?, std::env::var_os("AGENT_GRID_WORKSPACE_DIR"))
                .map_err(std::io::Error::other)?;
            std::fs::create_dir_all(&directory)?;
            let handle = app.handle().clone();
            let sink = Arc::new(move |event| match event {
                BackendEvent::Output(output) => handle.emit("terminal-output", output),
                BackendEvent::Changed(info) => handle.emit("session-changed", info),
            }.map_err(|e| format!("deliver renderer event: {e}")));
            app.manage(Engine::new(&directory.join("workspace.sqlite3"), sink).map_err(std::io::Error::other)?);
            let show = MenuItem::with_id(app, "show", "OpenDIY Desktop 열기", true, None::<&str>)?;
            let exit = MenuItem::with_id(app, "exit", "종료…", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show, &exit])?;
            app.manage(UiSettings { english: AtomicBool::new(false), show, exit });
            let mut tray = TrayIconBuilder::new().tooltip("OpenDIY Desktop").menu(&menu).show_menu_on_left_click(false)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "show" => show_main(app),
                    "exit" => { show_main(app); let _ = app.emit("exit-requested", ()); }
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if matches!(event, TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. }) { show_main(tray.app_handle()); }
                });
            if let Some(icon) = app.default_window_icon() { tray = tray.icon(icon.clone()); }
            tray.build(app)?;
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![get_ui_language, set_ui_language, prepare_runtime, start_session, list_sessions, session_write, session_resize, session_stop, session_output_ack, load_layout, save_layout, list_external, list_projects, import_project, create_project, rename_project, set_project_favorite, delete_project, move_session_to_project, choose_project_folder, choose_project, catalog_list, catalog_preview, get_codex_settings, set_codex_executable, choose_codex_executable, extensions_list, extensions_mutate, choose_skill_folder, mobile_control, cron_request, subagents_request, conversation_request, list_pending_questions, respond_pending_question, choose_conversation_attachments, stage_conversation_image, cleanup_conversation_attachment, weekly_usage, unconfirmed_exit_available, complete_exit])
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                if !window.state::<ExitState>().0.load(Ordering::Acquire) {
                    api.prevent_close();
                    if let Err(error) = window.hide() { let _ = window.emit("application-error", format!("cannot hide main window: {error}")); }
                    else { let _ = window.emit("application-visibility", false); }
                }
            }
        })
        .build(tauri::generate_context!());
    match result {
        Ok(app) => app.run(|app, event| {
            if let tauri::RunEvent::ExitRequested { api, .. } = event {
                if !app.state::<ExitState>().0.load(Ordering::Acquire) {
                    api.prevent_exit();
                    show_main(app);
                    let _ = app.emit("exit-requested", ());
                }
            }
        }),
        Err(error) => { eprintln!("OpenDIY Desktop initialization failed: {error}"); std::process::exit(1); }
    }
}

#[cfg(test)]
mod workspace_tests {
    use super::workspace_directory;
    use std::ffi::OsString;

    #[test]
    fn workspace_override_is_explicit_absolute_and_does_not_change_the_default() {
        let default = std::env::temp_dir().join("agent-grid-default-workspace");
        let isolated = std::env::temp_dir().join("agent-grid-isolated-workspace");
        assert_eq!(workspace_directory(default.clone(), None).unwrap(), default);
        assert_eq!(workspace_directory(default.clone(), Some(isolated.clone().into_os_string())).unwrap(), isolated);
        assert!(workspace_directory(default.clone(), Some(OsString::new())).is_err());
        assert!(workspace_directory(default, Some(OsString::from("relative-workspace"))).is_err());
    }
}
