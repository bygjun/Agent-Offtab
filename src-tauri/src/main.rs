mod activity;
mod hooks;
mod model;
mod sessions;
mod targets;
mod transcript;
mod window_layout;

use model::{is_uuid, ListScope, Mode, OpenResult, Provider, Snapshot};
use sessions::Collector;
use std::{
    collections::HashMap,
    env, io,
    path::PathBuf,
    sync::{Arc, Mutex},
};
use tauri::{Manager, RunEvent, State, WebviewWindow, WindowEvent};
use window_layout::Layout;

#[derive(Default)]
struct Data {
    collector: Collector,
    links: HashMap<(Provider, String), targets::Target>,
}

struct DeskState {
    home: PathBuf,
    hook_root: PathBuf,
    data: Arc<Mutex<Data>>,
    window: Mutex<Layout>,
    layout_path: PathBuf,
}

#[tauri::command]
async fn list_sessions(scope: ListScope, state: State<'_, DeskState>) -> Result<Snapshot, String> {
    let home = state.home.clone();
    let hook_root = state.hook_root.clone();
    let data = Arc::clone(&state.data);
    tauri::async_runtime::spawn_blocking(move || {
        let mut data = data.lock().map_err(|error| error.to_string())?;
        let snapshot = data.collector.scan(&home, &hook_root, &scope);
        let mut links = HashMap::new();
        for watched in &scope.watched {
            let key = (watched.provider, watched.id.clone());
            if let Some(url) = data.links.remove(&key) {
                links.insert(key, url);
            }
        }
        for item in &snapshot.sessions {
            let key = (item.provider, item.id.clone());
            if let Some(target) = targets::target(item) {
                links.insert(key, target);
            } else {
                links.remove(&key);
            }
        }
        for item in &snapshot.removed {
            links.remove(&(item.provider, item.id.clone()));
        }
        data.links = links;
        Ok(snapshot)
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
async fn open_session(
    provider: Provider,
    id: String,
    state: State<'_, DeskState>,
) -> Result<OpenResult, String> {
    if !is_uuid(&id) {
        return Ok(OpenResult::Unavailable);
    }
    let data = Arc::clone(&state.data);
    tauri::async_runtime::spawn_blocking(move || {
        let target = data
            .lock()
            .map_err(|error| error.to_string())?
            .links
            .get(&(provider, id))
            .cloned();
        let Some(target) = target else {
            return Ok(OpenResult::Unavailable);
        };
        Ok(targets::open(&target))
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
async fn session_hooks(
    provider: Provider,
    enabled: Option<bool>,
    state: State<'_, DeskState>,
) -> Result<hooks::HookStatus, String> {
    let home = state.home.clone();
    let hook_root = state.hook_root.clone();
    let data = Arc::clone(&state.data);
    tauri::async_runtime::spawn_blocking(move || {
        let _guard = data.lock().map_err(|error| error.to_string())?;
        hooks::configure(&home, &hook_root, provider, enabled).map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
fn window_pin(window: WebviewWindow, value: Option<bool>) -> Result<bool, String> {
    if let Some(pinned) = value {
        window
            .set_always_on_top(pinned)
            .map_err(|error| error.to_string())?;
    }
    window.is_always_on_top().map_err(|error| error.to_string())
}

#[tauri::command]
fn window_mode(
    window: WebviewWindow,
    value: Option<Mode>,
    state: State<'_, DeskState>,
) -> Result<Mode, String> {
    let mut settings = state.window.lock().map_err(|error| error.to_string())?;
    if let Some(mode) = value.filter(|mode| *mode != settings.mode) {
        let previous = settings.mode;
        settings
            .remember(&window)
            .map_err(|error| error.to_string())?;
        settings
            .save(&state.layout_path)
            .map_err(|error| error.to_string())?;
        if let Err(error) = settings.restore(&window, mode) {
            let _ = settings.restore(&window, previous);
            return Err(error.to_string());
        }
        settings.mode = mode;
    }
    Ok(settings.mode)
}

fn remember_layout(window: &WebviewWindow, state: &DeskState) {
    if let Ok(mut settings) = state.window.lock() {
        if window.is_minimized().is_ok_and(|minimized| minimized) {
            return;
        }
        if settings.remember(window).is_ok() {
            if let Err(error) = settings.save(&state.layout_path) {
                eprintln!("Could not save window layout: {error}");
            }
        }
    }
}

fn main() {
    let mut args = env::args_os().skip(1);
    if args.next().as_deref().is_some_and(|arg| arg == "--observe") {
        let provider = match args.next().as_deref().and_then(|arg| arg.to_str()) {
            Some("claude") => Provider::Claude,
            Some("codex") => Provider::Codex,
            _ => return,
        };
        if let Some(root) = args.next() {
            let _ = hooks::observe(provider, &PathBuf::from(root), io::stdin().lock());
        }
        return;
    }
    tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        .setup(|app| {
            let app_data = app.path().app_data_dir()?;
            let layout_path = app_data.join("window-layout.json");
            let mut layout = Layout::load(&layout_path).unwrap_or_else(|error| {
                eprintln!("Could not read window layout: {error}");
                Layout::default()
            });
            if let Some(window) = app.get_webview_window("main") {
                layout.restore(&window, Mode::Compact)?;
            }
            app.manage(DeskState {
                home: app.path().home_dir()?,
                hook_root: app_data.join("session-observer"),
                data: Arc::new(Mutex::new(Data::default())),
                window: Mutex::new(layout),
                layout_path,
            });
            Ok(())
        })
        .on_window_event(|window, event| {
            if matches!(event, WindowEvent::CloseRequested { .. }) {
                if let Some(webview) = window.get_webview_window(window.label()) {
                    remember_layout(&webview, &window.state::<DeskState>());
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            list_sessions,
            open_session,
            session_hooks,
            window_pin,
            window_mode
        ])
        .build(tauri::generate_context!())
        .expect("could not run Offtab")
        .run(|app, event| {
            if matches!(event, RunEvent::ExitRequested { .. }) {
                if let Some(window) = app.get_webview_window("main") {
                    remember_layout(&window, &app.state::<DeskState>());
                }
            }
        });
}
