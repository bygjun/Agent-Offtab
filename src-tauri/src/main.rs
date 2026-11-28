mod activity;
#[cfg(target_os = "macos")]
mod claude_desktop;
#[cfg(target_os = "macos")]
mod claude_pasteboard;
mod codex_photo;
mod codex_choice;
mod hooks;
mod mobile;
mod mobile_attachment;
mod mobile_devices;
mod mobile_reply;
mod model;
#[cfg(target_os = "macos")]
mod pairing_keychain;
mod session_reads;
mod sessions;
mod targets;
mod transcript;
mod window_layout;

use model::{is_uuid, ListScope, Mode, OpenResult, Provider, Snapshot};
use session_reads::{ReadStore, ReadUpdate};
use sessions::Collector;
use std::{
    collections::HashMap,
    env, fs, io,
    path::PathBuf,
    sync::{Arc, Mutex},
};
use tauri::{Manager, RunEvent, State, WebviewWindow, WindowEvent};
use window_layout::Layout;

#[derive(Default)]
struct Data {
    collector: Collector,
    links: HashMap<(Provider, String), targets::Target>,
    read_versions: HashMap<(Provider, String), String>,
}

struct DeskState {
    home: PathBuf,
    hook_root: PathBuf,
    data: Arc<Mutex<Data>>,
    reads: ReadStore,
    mobile: Option<Arc<MobileState>>,
    window: Mutex<Layout>,
    layout_path: PathBuf,
}

struct MobileState {
    connections: Mutex<mobile::Mobile>,
    compact: mobile::DesktopCompact,
    relay: Mutex<RelayPreferences>,
    relay_path: PathBuf,
    dispatcher: mobile_reply::Dispatcher,
}

const MOBILE_DISABLED: &str = "휴대폰 연결을 사용하려면 Mac에서 중계 주소를 설정해 주세요.";

#[tauri::command]
fn publish_compact(view: model::CompactView, state: State<'_, DeskState>) -> Result<(), String> {
    let mobile = state.mobile.as_ref().ok_or(MOBILE_DISABLED)?;
    *mobile
        .compact
        .lock()
        .map_err(|_| "작은 창의 목록을 동기화하지 못했습니다.")? = Some(Arc::new(view));
    Ok(())
}

#[tauri::command]
async fn list_sessions(scope: ListScope, state: State<'_, DeskState>) -> Result<Snapshot, String> {
    let home = state.home.clone();
    let hook_root = state.hook_root.clone();
    let data = Arc::clone(&state.data);
    let reads = state.reads.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let mut data = data.lock().map_err(|error| error.to_string())?;
        let mut snapshot = data.collector.scan(&home, &hook_root, &scope);
        reads.apply(&mut snapshot);
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
        data.read_versions = snapshot
            .sessions
            .iter()
            .filter_map(|session| {
                let read = session.read.as_ref()?;
                Some((
                    (
                        session.provider,
                        session
                            .native_id
                            .clone()
                            .unwrap_or_else(|| session.id.clone()),
                    ),
                    read.version.clone(),
                ))
            })
            .collect();
        Ok(snapshot)
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
async fn mark_sessions_read(
    updates: Vec<ReadUpdate>,
    state: State<'_, DeskState>,
) -> Result<(), String> {
    let data = Arc::clone(&state.data);
    let reads = state.reads.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let data = data.lock().map_err(|error| error.to_string())?;
        let current = updates
            .into_iter()
            .filter(|update| {
                data.read_versions
                    .get(&(update.provider, update.id.clone()))
                    == Some(&update.version)
            })
            .collect();
        reads.mark(current).map_err(|error| error.to_string())
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

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct MobileOverview {
    local_preview: bool,
    devices: Vec<mobile::Status>,
    relay_url: String,
    local_relay_url: Option<String>,
    reachable: Option<bool>,
    claude_desktop_access: Option<bool>,
}

#[derive(Clone, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct RelayPreferences {
    url: String,
    local_url: Option<String>,
}

impl RelayPreferences {
    fn load(
        path: &std::path::Path,
        url: Option<&str>,
        local_url: Option<&str>,
    ) -> Result<Option<Self>, String> {
        let from_file = match fs::read(path) {
            Ok(bytes) => Some(
                serde_json::from_slice::<Self>(&bytes)
                    .map_err(|_| "저장된 중계 주소를 읽지 못했습니다.")?,
            ),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(_) => return Err("저장된 중계 주소를 열지 못했습니다.".into()),
        };
        if let Some(url) = url {
            let local_url = local_url.or_else(|| {
                from_file
                    .as_ref()
                    .filter(|saved| saved.url.trim_end_matches('/') == url.trim_end_matches('/'))
                    .and_then(|saved| saved.local_url.as_deref())
            });
            let active = Self::validate(url, local_url)?;
            active.save(path)?;
            return Ok(Some(active));
        }
        from_file
            .map(|saved| Self::validate(&saved.url, saved.local_url.as_deref()))
            .transpose()
    }

    fn validate(url: &str, local_url: Option<&str>) -> Result<Self, String> {
        let url = mobile::relay_url(url)?.to_string();
        let local_url = local_url
            .filter(|value| !value.trim().is_empty())
            .map(|value| {
                if !mobile::is_local_preview(value) {
                    return Err("Mac 내부 주소는 http://127.0.0.1로 시작해야 합니다.".into());
                }
                mobile::relay_url(value).map(|url| url.to_string())
            })
            .transpose()?;
        Ok(Self { url, local_url })
    }

    fn save(&self, path: &std::path::Path) -> Result<(), String> {
        let save = || -> io::Result<()> {
            let parent = path
                .parent()
                .ok_or_else(|| io::Error::other("Missing settings directory"))?;
            fs::create_dir_all(parent)?;
            let mut file = tempfile::NamedTempFile::new_in(parent)?;
            serde_json::to_writer(&mut file, self)?;
            file.as_file().sync_all()?;
            file.persist(path).map_err(|error| error.error)?;
            Ok(())
        };
        save().map_err(|_| "중계 주소를 저장하지 못했습니다.".into())
    }
}

#[tauri::command]
fn mobile_connection_enabled(state: State<'_, DeskState>) -> bool {
    state.mobile.is_some()
}

#[tauri::command]
async fn mobile_connection(
    enabled: Option<bool>,
    device: Option<String>,
    label: Option<String>,
    reply_enabled: Option<bool>,
    all_devices: Option<bool>,
    relay_url: Option<String>,
    local_relay_url: Option<String>,
    check_relay: Option<bool>,
    state: State<'_, DeskState>,
) -> Result<MobileOverview, String> {
    let home = state.home.clone();
    let hook_root = state.hook_root.clone();
    let data = Arc::clone(&state.data);
    let reads = state.reads.clone();
    let mobile = state.mobile.clone().ok_or(MOBILE_DISABLED)?;
    tauri::async_runtime::spawn_blocking(move || {
        let mut connections = mobile
            .connections
            .lock()
            .map_err(|_| "휴대폰 연결 상태를 읽지 못했습니다.")?;
        let mut preferences = mobile
            .relay
            .lock()
            .map_err(|_| "중계 주소를 읽지 못했습니다.")?;
        let collect = move |watched: &[model::Watched]| {
            let mut data = data.lock().map_err(|_| "세션 목록을 읽지 못했습니다.")?;
            Ok(data
                .collector
                .scan(&home, &hook_root, &mobile::scope(watched)))
        };
        if let Some(url) = relay_url {
            let updated = RelayPreferences::validate(&url, local_relay_url.as_deref())?;
            if updated.url != preferences.url
                || updated.local_url != preferences.local_url
                || connections.startup_error().is_some()
            {
                let mut restored = mobile::Mobile::with_compact(reads, mobile.compact.clone());
                restored.restore(
                    &updated.url,
                    updated.local_url.as_deref(),
                    mobile.dispatcher.clone(),
                    collect.clone(),
                );
                if let Some(error) = restored.startup_error() {
                    return Err(error.to_owned());
                }
                updated.save(&mobile.relay_path)?;
                *preferences = updated;
                *connections = restored;
            } else {
                updated.save(&mobile.relay_path)?;
            }
        }
        let devices = if let Some(value) = reply_enabled {
            let targets = match (device.as_deref(), all_devices.unwrap_or(false)) {
                (Some(id), false) => mobile::ReplyTargets::Device(id),
                (None, true) => mobile::ReplyTargets::All,
                _ => return Err("답장 권한을 변경할 기기를 지정해 주세요.".into()),
            };
            connections.allow_reply(targets, value)?
        } else {
            match enabled {
                Some(true) => connections.start(
                    &preferences.url,
                    preferences.local_url.as_deref(),
                    label,
                    mobile.dispatcher.clone(),
                    collect,
                )?,
                Some(false) => connections.stop(device.as_deref().unwrap_or_default())?,
                None => connections.status(),
            }
        };
        let relay_url = preferences.url.clone();
        let local_relay_url = preferences.local_url.clone();
        drop(preferences);
        drop(connections);
        Ok(MobileOverview {
            local_preview: mobile::is_local_preview(&relay_url),
            reachable: check_relay
                .filter(|check| *check)
                .map(|_| mobile::remote_status(&relay_url)),
            devices,
            relay_url,
            local_relay_url,
            claude_desktop_access: mobile_reply::claude_desktop_access(),
        })
    })
    .await
    .map_err(|_| "휴대폰 연결 작업을 완료하지 못했습니다.")?
}

#[tauri::command]
fn mobile_open_claude_access(state: State<'_, DeskState>) -> Result<(), String> {
    state.mobile.as_ref().ok_or(MOBILE_DISABLED)?;
    #[cfg(target_os = "macos")]
    claude_desktop::request_access();
    Ok(())
}

#[tauri::command]
async fn mobile_open_preview(device: String, state: State<'_, DeskState>) -> Result<(), String> {
    let mobile = state.mobile.clone().ok_or(MOBILE_DISABLED)?;
    tauri::async_runtime::spawn_blocking(move || {
        let url = mobile
            .connections
            .lock()
            .map_err(|_| "미리보기 상태를 읽지 못했습니다.")?
            .preview_url(&device)
            .ok_or("미리보기 연결이 끝났습니다. 다시 시작해 주세요.")?;
        let opened = std::process::Command::new("/usr/bin/open")
            .arg(url)
            .output()
            .is_ok_and(|output| output.status.success());
        opened
            .then_some(())
            .ok_or_else(|| "브라우저를 열지 못했습니다. 다시 시도해 주세요.".into())
    })
    .await
    .map_err(|_| "브라우저를 열지 못했습니다.")?
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
            let home = app.path().home_dir()?;
            let hook_root = app_data.join("session-observer");
            let data = Arc::new(Mutex::new(Data::default()));
            let reads = ReadStore::load(app_data.join("session-reads.json"));
            let relay_path = app_data.join("mobile-relay.json");
            let preferences = RelayPreferences::load(
                &relay_path,
                env::var("OFFTAB_RELAY_URL").ok().as_deref(),
                env::var("OFFTAB_RELAY_LOCAL_URL").ok().as_deref(),
            )
            .unwrap_or_else(|error| {
                eprintln!("{error}");
                None
            });
            let mobile = preferences.map(|preferences| {
                let compact = mobile::DesktopCompact::default();
                let mobile = Arc::new(MobileState {
                    connections: Mutex::new(mobile::Mobile::with_compact(
                        reads.clone(),
                        compact.clone(),
                    )),
                    compact,
                    relay: Mutex::new(preferences.clone()),
                    relay_path,
                    dispatcher: mobile_reply::Dispatcher::new(&home),
                });
                let home = home.clone();
                let hook_root = hook_root.clone();
                let data = Arc::clone(&data);
                let startup = Arc::clone(&mobile);
                tauri::async_runtime::spawn_blocking(move || {
                    if let Ok(mut connections) = startup.connections.lock() {
                        if connections.initialized() {
                            return;
                        }
                        connections.restore(
                            &preferences.url,
                            preferences.local_url.as_deref(),
                            startup.dispatcher.clone(),
                            move |watched| {
                                let mut data =
                                    data.lock().map_err(|_| "세션 목록을 읽지 못했습니다.")?;
                                Ok(data
                                    .collector
                                    .scan(&home, &hook_root, &mobile::scope(watched)))
                            },
                        );
                    }
                });
                mobile
            });
            app.manage(DeskState {
                home,
                hook_root,
                data,
                reads,
                mobile,
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
            publish_compact,
            mark_sessions_read,
            open_session,
            session_hooks,
            mobile_connection_enabled,
            mobile_connection,
            mobile_open_preview,
            mobile_open_claude_access,
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

#[cfg(test)]
mod relay_preferences_tests {
    use super::*;

    #[test]
    fn mobile_requires_an_explicit_relay_url_or_saved_configuration() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("mobile-relay.json");
        assert!(RelayPreferences::load(&path, None, None).unwrap().is_none());
        assert!(!path.exists());
        assert!(RelayPreferences::load(&path, Some("https://relay.example/offtab"), None).is_err());
        assert!(!path.exists());

        let local = "http://127.0.0.1:63915/";
        let selected = RelayPreferences::load(&path, Some(local), None)
            .unwrap()
            .unwrap();
        assert_eq!(selected.url, local);
        assert_eq!(
            RelayPreferences::load(&path, None, None)
                .unwrap()
                .unwrap()
                .url,
            local
        );
    }

    #[test]
    fn persisted_relay_keeps_public_and_mac_local_addresses() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("mobile-relay.json");
        let config =
            RelayPreferences::validate("https://offtab.example", Some("http://127.0.0.1:63915"))
                .unwrap();
        config.save(&path).unwrap();
        let decoded = RelayPreferences::load(&path, None, None).unwrap().unwrap();
        assert_eq!(decoded.url, "https://offtab.example/");
        assert_eq!(
            decoded.local_url.as_deref(),
            Some("http://127.0.0.1:63915/")
        );
        let reopened = RelayPreferences::load(&path, Some("https://offtab.example"), None)
            .unwrap()
            .unwrap();
        assert_eq!(reopened.local_url, decoded.local_url);
        assert!(RelayPreferences::validate(
            "https://offtab.example",
            Some("https://other.example")
        )
        .is_err());
    }
}
