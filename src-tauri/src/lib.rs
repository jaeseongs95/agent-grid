mod codex;
mod flow;
pub mod process_job;
mod session;
pub mod self_test;
mod storage;

use session::{BackendEvent, Engine, SessionInfo, StartRequest};
use std::sync::{atomic::{AtomicBool, Ordering}, Arc};
use tauri::{menu::{Menu, MenuItem}, tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent}, Emitter, Manager, State};

#[derive(Default)]
struct ExitState(AtomicBool);

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
async fn list_external() -> Result<Vec<codex::ExternalSession>, String> {
    tauri::async_runtime::spawn_blocking(codex::list_external).await.map_err(|e| format!("external session observer: {e}"))?
}
#[tauri::command]
async fn complete_exit(app: tauri::AppHandle, engine: State<'_, Engine>, confirmed: bool) -> Result<(), String> {
    if !confirmed { return Err("explicit exit confirmation is required".into()); }
    let engine = engine.inner().clone();
    tauri::async_runtime::spawn_blocking(move || engine.shutdown()).await.map_err(|e| format!("application cleanup worker: {e}"))??;
    app.state::<ExitState>().0.store(true, Ordering::Release);
    app.exit(0);
    Ok(())
}

fn show_main(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        if let Err(error) = window.show().and_then(|_| window.unminimize()).and_then(|_| window.set_focus()) {
            let _ = app.emit("application-error", format!("cannot show main window: {error}"));
        }
    }
}

pub fn run() {
    if let Err(error) = process_job::install_app_job() {
        // Fail closed: starting unowned background processes is never a fallback.
        eprintln!("Agent Grid startup failed: {error}");
        return;
    }
    let result = tauri::Builder::default()
        .manage(ExitState::default())
        .setup(|app| {
            let directory = app.path().app_data_dir()?;
            std::fs::create_dir_all(&directory)?;
            let handle = app.handle().clone();
            let sink = Arc::new(move |event| match event {
                BackendEvent::Output(output) => handle.emit("terminal-output", output),
                BackendEvent::Changed(info) => handle.emit("session-changed", info),
            }.map_err(|e| format!("deliver renderer event: {e}")));
            app.manage(Engine::new(&directory.join("workspace.sqlite3"), sink).map_err(std::io::Error::other)?);
            let show = MenuItem::with_id(app, "show", "Agent Grid 열기", true, None::<&str>)?;
            let exit = MenuItem::with_id(app, "exit", "종료…", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show, &exit])?;
            let mut tray = TrayIconBuilder::new().tooltip("Agent Grid").menu(&menu).show_menu_on_left_click(false)
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
        .invoke_handler(tauri::generate_handler![start_session, list_sessions, session_write, session_resize, session_stop, session_output_ack, load_layout, save_layout, list_external, complete_exit])
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                if !window.state::<ExitState>().0.load(Ordering::Acquire) {
                    api.prevent_close();
                    if let Err(error) = window.hide() { let _ = window.emit("application-error", format!("cannot hide main window: {error}")); }
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
        Err(error) => eprintln!("Agent Grid initialization failed: {error}"),
    }
}
