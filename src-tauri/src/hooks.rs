use crate::{
    model::{is_uuid, Provider, Waiting},
    transcript::{epoch, text},
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    env,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::unix::{fs::PermissionsExt, io::AsRawFd},
    path::{Path, PathBuf},
    time::SystemTime,
};

const COMMON: &[&str] = &[
    "PermissionRequest",
    "PostToolUse",
    "UserPromptSubmit",
    "Stop",
    "SessionStart",
    "SessionEnd",
    "SubagentStart",
    "SubagentStop",
    "PreToolUse",
];

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct Signal {
    pub waiting: Option<Waiting>,
    pub waiting_at: i64,
    pub turn: String,
    pub tool: String,
    pub tool_id: String,
    pub children: HashMap<String, i64>,
    #[serde(default)]
    pub stopped_children: HashMap<String, i64>,
    pub changed_at: i64,
}

#[derive(Serialize, PartialEq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum HookStatus {
    Off,
    Connected,
    Invalid,
}

pub fn provider_root(home: &Path, provider: Provider) -> PathBuf {
    let (variable, fallback) = match provider {
        Provider::Claude => ("CLAUDE_CONFIG_DIR", ".claude"),
        Provider::Codex => ("CODEX_HOME", ".codex"),
    };
    env::var_os(variable)
        .filter(|value| !value.is_empty())
        .map_or_else(|| home.join(fallback), PathBuf::from)
}

fn name(provider: Provider) -> &'static str {
    match provider {
        Provider::Codex => "codex",
        Provider::Claude => "claude",
    }
}

fn events(provider: Provider) -> impl Iterator<Item = &'static str> {
    let extra: &[&str] = match provider {
        Provider::Codex => &["Interrupt"],
        Provider::Claude => &["Notification", "PostToolUseFailure"],
    };
    COMMON.iter().chain(extra).copied()
}

fn field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    let field = text(&value[key]);
    (!field.is_empty() && field.len() <= 128).then_some(field)
}

fn config_path(home: &Path, provider: Provider) -> PathBuf {
    provider_root(home, provider).join(match provider {
        Provider::Codex => "hooks.json",
        Provider::Claude => "settings.json",
    })
}

fn shell_quote(path: &Path) -> io::Result<String> {
    let path = path
        .to_str()
        .filter(|value| !value.contains(['\n', '\r']))
        .ok_or_else(|| io::Error::other("Unsupported configuration path"))?;
    Ok(format!("'{}'", path.replace('\'', "'\"'\"'")))
}

fn command(root: &Path, provider: Provider) -> io::Result<String> {
    Ok(format!(
        "{} --observe {} {}",
        shell_quote(&root.join("hook-helper"))?,
        name(provider),
        shell_quote(root)?
    ))
}

fn read_value(path: &Path) -> io::Result<Value> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(io::Error::other),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(json!({})),
        Err(error) => Err(error),
    }
}

fn atomic(path: &Path, data: &[u8], mode: u32) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("Invalid parent path"))?;
    fs::create_dir_all(parent)?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.as_file()
        .set_permissions(fs::Permissions::from_mode(mode))?;
    file.write_all(data)?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|error| error.error)?;
    Ok(())
}

fn hooks(value: &mut Value) -> io::Result<&mut serde_json::Map<String, Value>> {
    value
        .as_object_mut()
        .ok_or_else(|| io::Error::other("Expected a JSON object"))?
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| io::Error::other("Expected a hooks object"))
}

fn has_hook(value: &Value, event: &str, command: &str) -> bool {
    value["hooks"][event]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|group| group["hooks"].as_array().into_iter().flatten())
        .any(|handler| handler["command"] == command)
}

fn state_dir(root: &Path, provider: Provider) -> PathBuf {
    root.join(name(provider))
}

pub fn configure(
    home: &Path,
    root: &Path,
    provider: Provider,
    enabled: Option<bool>,
) -> io::Result<HookStatus> {
    configure_at(config_path(home, provider), root, provider, enabled)
}

fn configure_at(
    path: PathBuf,
    root: &Path,
    provider: Provider,
    enabled: Option<bool>,
) -> io::Result<HookStatus> {
    let path = match fs::symlink_metadata(&path) {
        Ok(meta) if meta.file_type().is_symlink() => path.canonicalize()?,
        Ok(_) => path,
        Err(error) if error.kind() == io::ErrorKind::NotFound => path,
        Err(error) => return Err(error),
    };
    let command = command(root, provider)?;
    let old = match read_value(&path) {
        Ok(value) => value,
        Err(_) if enabled.is_none() => return Ok(HookStatus::Invalid),
        Err(error) => return Err(error),
    };
    let mut next = old.clone();
    let configured = events(provider).any(|event| has_hook(&old, event, &command));
    let dir = state_dir(root, provider);
    let marker = dir.join("enabled");
    if enabled.is_none() {
        if hooks(&mut next).is_err() {
            return Ok(HookStatus::Invalid);
        }
        return Ok(
            if events(provider).all(|event| has_hook(&old, event, &command))
                && marker.is_file()
                && root.join("hook-helper").is_file()
            {
                HookStatus::Connected
            } else {
                HookStatus::Off
            },
        );
    }
    let enabled = enabled.unwrap_or(false);
    if !enabled && !configured && !marker.exists() {
        return Ok(HookStatus::Off);
    }
    let table = hooks(&mut next)?;
    for event in events(provider) {
        let entry = table.entry(event).or_insert_with(|| json!([]));
        let groups = entry
            .as_array_mut()
            .ok_or_else(|| io::Error::other("Expected a hook group list"))?;
        if enabled {
            if !has_hook(&old, event, &command) {
                let matcher = if event == "Notification" {
                    "permission_prompt|elicitation_dialog|elicitation_url_dialog|agent_needs_input"
                } else {
                    ""
                };
                groups.push(json!({"matcher":matcher, "hooks":[{"type":"command","command":command,"timeout":2}]}));
            }
        } else {
            for group in groups.iter_mut() {
                if let Some(handlers) = group["hooks"].as_array_mut() {
                    handlers.retain(|handler| handler["command"] != command);
                }
            }
            // Preserve existing groups with an empty handler list if Offtab did not create them.
            groups.retain(|group| {
                !group["hooks"].as_array().is_some_and(Vec::is_empty)
                    || old["hooks"][event]
                        .as_array()
                        .is_some_and(|previous| previous.contains(group))
            });
        }
        if groups.is_empty() {
            table.remove(event);
        }
    }
    if enabled {
        let binary = env::current_exe()?;
        let helper = root.join("hook-helper");
        if fs::metadata(&helper).ok().map(|metadata| metadata.len())
            != Some(fs::metadata(&binary)?.len())
        {
            atomic(&helper, &fs::read(binary)?, 0o700)?;
        }
    }
    if next != old {
        if read_value(&path)? != old {
            return Err(io::Error::other("Hooks changed concurrently"));
        }
        let mode =
            fs::metadata(&path).map_or(0o600, |metadata| metadata.permissions().mode() & 0o777);
        atomic(&path, &serde_json::to_vec_pretty(&next)?, mode)?;
    }
    if enabled {
        if !marker.is_file() {
            atomic(&marker, b"enabled", 0o600)?;
        }
    } else if marker.exists() {
        fs::remove_file(marker)?;
    }
    Ok(if enabled {
        HookStatus::Connected
    } else {
        HookStatus::Off
    })
}

pub fn signals(root: &Path, provider: Provider) -> HashMap<String, Signal> {
    let dir = state_dir(root, provider);
    let Some(enabled_at) = enabled_at(&dir) else {
        return HashMap::new();
    };
    fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let id = entry
                .file_name()
                .to_str()?
                .strip_suffix(".json")?
                .to_owned();
            if !is_uuid(&id) {
                return None;
            }
            let value: Signal = serde_json::from_reader(File::open(entry.path()).ok()?).ok()?;
            (value.changed_at >= enabled_at && has_state(&value)).then_some((id, value))
        })
        .collect()
}

fn enabled_at(dir: &Path) -> Option<i64> {
    fs::metadata(dir.join("enabled"))
        .ok()?
        .modified()
        .ok()
        .map(epoch)
}

fn has_state(signal: &Signal) -> bool {
    signal.waiting.is_some() || !signal.children.is_empty() || !signal.stopped_children.is_empty()
}

fn clear_wait(signal: &mut Signal) {
    signal.waiting = None;
    signal.waiting_at = 0;
    signal.tool.clear();
    signal.tool_id.clear();
    signal.turn.clear();
}

fn record(signal: &mut Signal, input: &Value, at: i64) {
    let event = text(&input["hook_event_name"]);
    let tool = field(input, "tool_name").unwrap_or("");
    let tool_id = field(input, "tool_use_id").unwrap_or("");
    let turn = field(input, "turn_id").unwrap_or("");
    if at < signal.changed_at {
        return;
    }
    match event {
        "PermissionRequest" | "Notification" => {
            let notification = text(&input["notification_type"]);
            let waiting = if tool == "AskUserQuestion"
                || matches!(
                    notification,
                    "elicitation_dialog" | "elicitation_url_dialog" | "agent_needs_input"
                ) {
                Waiting::Input
            } else if event == "PermissionRequest" || notification == "permission_prompt" {
                Waiting::Approval
            } else {
                return;
            };
            if event == "Notification" && signal.waiting.is_some() {
                return;
            }
            signal.waiting = Some(waiting);
            signal.waiting_at = at;
            signal.tool = tool.to_owned();
            signal.tool_id = tool_id.to_owned();
            signal.turn = turn.to_owned();
        }
        "PostToolUse" | "PostToolUseFailure"
            if signal.waiting.is_some()
                && (signal.tool.is_empty()
                    || (!tool_id.is_empty() && tool_id == signal.tool_id)
                    || (signal.tool_id.is_empty() && tool == signal.tool)) =>
        {
            clear_wait(signal)
        }
        "PreToolUse"
            if signal.waiting.is_some()
                && !signal.tool.is_empty()
                && !tool.is_empty()
                && ((turn != signal.turn && !turn.is_empty()) || tool != signal.tool) =>
        {
            clear_wait(signal)
        }
        "UserPromptSubmit" | "Stop" | "Interrupt" => clear_wait(signal),
        "SessionStart" | "SessionEnd" => {
            clear_wait(signal);
            signal.children.clear();
            signal.stopped_children.clear();
        }
        "SubagentStart" | "SubagentStop" => {
            let Some(id) = field(input, "agent_id") else {
                return;
            };
            if event == "SubagentStop" {
                signal.children.remove(id);
                signal.stopped_children.insert(id.to_owned(), at);
                if signal.stopped_children.len() > 128 {
                    if let Some(old) = signal
                        .stopped_children
                        .iter()
                        .min_by_key(|(_, at)| *at)
                        .map(|(id, _)| id.clone())
                    {
                        signal.stopped_children.remove(&old);
                    }
                }
            } else if signal.children.len() < 128 {
                signal.stopped_children.remove(id);
                signal.children.insert(id.to_owned(), at);
            }
        }
        _ => return,
    }
    signal.changed_at = at;
}

pub fn observe(provider: Provider, root: &Path, reader: impl Read) -> io::Result<()> {
    let at = epoch(SystemTime::now());
    let input: Value = serde_json::from_reader(reader.take(4 * 1024 * 1024))?;
    let Some(id) = field(&input, "session_id").filter(|id| is_uuid(id)) else {
        return Ok(());
    };
    if !events(provider).any(|event| input["hook_event_name"] == event) {
        return Ok(());
    }
    let dir = state_dir(root, provider);
    if enabled_at(&dir).is_none() {
        return Ok(());
    }
    let path = dir.join(format!("{id}.json"));
    if !path.is_file()
        && !matches!(
            text(&input["hook_event_name"]),
            "PermissionRequest" | "Notification" | "SubagentStart"
        )
    {
        return Ok(());
    }
    let guard = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("lock"))?;
    if unsafe { libc::flock(guard.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let Some(enabled_at) = enabled_at(&dir) else {
        return Ok(());
    };
    let mut signal: Signal = read_value(&path)
        .ok()
        .and_then(|value| serde_json::from_value(value).ok())
        .filter(|signal: &Signal| signal.changed_at >= enabled_at)
        .unwrap_or_default();
    let previous = signal.clone();
    record(&mut signal, &input, at);
    if signal == previous {
        return Ok(());
    }
    if has_state(&signal) {
        atomic(&path, &serde_json::to_vec(&signal)?, 0o600)
    } else {
        match fs::remove_file(path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            result => result,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SESSION: &str = "00000000-0000-0000-0000-000000000044";

    #[test]
    fn connects_idempotently_and_disconnects_without_touching_existing_hooks() {
        let dir = tempfile::tempdir().unwrap();
        for provider in [Provider::Codex, Provider::Claude] {
            let path = dir.path().join(format!("{}.json", name(provider)));
            let root = dir.path().join("observer with a ' in it");
            let original = json!({"otherSetting":true,"hooks":{"Stop":[{"hooks":[{"type":"command","command":"existing"}]}]}});
            fs::write(&path, serde_json::to_vec(&original).unwrap()).unwrap();
            assert_eq!(
                configure_at(path.clone(), &root, provider, Some(true)).unwrap(),
                HookStatus::Connected
            );
            let installed = read_value(&path).unwrap();
            assert_eq!(
                configure_at(path.clone(), &root, provider, Some(true)).unwrap(),
                HookStatus::Connected
            );
            assert_eq!(read_value(&path).unwrap(), installed);
            assert!(installed["hooks"]["Stop"].as_array().unwrap().len() == 2);
            assert_eq!(
                configure_at(path.clone(), &root, provider, Some(false)).unwrap(),
                HookStatus::Off
            );
            assert_eq!(read_value(&path).unwrap(), original);
        }
    }

    #[test]
    fn observes_permission_and_subagent_lifecycles_without_recording_tool_input() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let provider = Provider::Claude;
        atomic(
            &state_dir(root, provider).join("enabled"),
            b"enabled",
            0o600,
        )
        .unwrap();
        let event = |event, extras: Value| {
            let mut value = json!({ "session_id":SESSION, "hook_event_name":event });
            value
                .as_object_mut()
                .unwrap()
                .extend(extras.as_object().unwrap().clone());
            observe(
                provider,
                root,
                serde_json::to_vec(&value).unwrap().as_slice(),
            )
            .unwrap();
        };
        event("SubagentStart", json!({"agent_id":"child-a"}));
        event(
            "PermissionRequest",
            json!({"tool_name":"Bash","tool_use_id":"pending-1","tool_input":{"command":"private payload"}}),
        );
        let observed = &signals(root, provider)[SESSION];
        assert_eq!(observed.waiting, Some(Waiting::Approval));
        assert!(observed.children.contains_key("child-a"));
        assert!(
            !fs::read_to_string(state_dir(root, provider).join(format!("{SESSION}.json")))
                .unwrap()
                .contains("private payload")
        );
        event(
            "PostToolUse",
            json!({"tool_name":"Bash","tool_use_id":"unrelated"}),
        );
        assert_eq!(
            signals(root, provider)[SESSION].waiting,
            Some(Waiting::Approval)
        );
        event(
            "PostToolUse",
            json!({"tool_name":"Bash","tool_use_id":"pending-1"}),
        );
        event("Stop", json!({}));
        assert!(signals(root, provider)[SESSION]
            .children
            .contains_key("child-a"));
        event("SubagentStop", json!({"agent_id":"child-a"}));
        let stopped = &signals(root, provider)[SESSION];
        assert!(
            stopped.waiting.is_none()
                && stopped.children.is_empty()
                && stopped.stopped_children.contains_key("child-a")
        );
        event("PermissionRequest", json!({"tool_name":"AskUserQuestion"}));
        assert_eq!(
            signals(root, provider)[SESSION].waiting,
            Some(Waiting::Input)
        );
        event("SessionEnd", json!({}));
        assert!(signals(root, provider).is_empty());
    }
}
