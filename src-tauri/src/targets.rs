use crate::{
    activity::{self, Terminal, TerminalHost},
    model::{is_uuid, OpenResult, Provider, Session},
};
use std::{path::PathBuf, process::Command};

#[derive(Clone, Debug)]
pub struct TerminalTab {
    pub process: Terminal,
    pub evidence: TerminalEvidence,
}

#[derive(Clone, Debug)]
pub enum TerminalEvidence {
    Codex(PathBuf),
    Claude(PathBuf, String),
}

#[derive(Clone)]
pub enum Target {
    Chat(Provider, String),
    Terminal(TerminalTab),
}

pub const CLAUDE_BUNDLE: &str = "com.anthropic.claudefordesktop";

pub fn claude_desktop_id(item: &Session) -> Option<&str> {
    item.native_id
        .as_deref()
        .filter(|id| id.strip_prefix("local_").is_some_and(is_uuid))
}

pub fn target(item: &Session) -> Option<Target> {
    match item.provider {
        Provider::Codex if is_uuid(&item.id) => {
            item.terminal.clone().map(Target::Terminal).or_else(|| {
                Some(Target::Chat(
                    Provider::Codex,
                    format!("codex://threads/{}", item.id),
                ))
            })
        }
        Provider::Claude => {
            if let Some(id) = claude_desktop_id(item) {
                return Some(Target::Chat(
                    Provider::Claude,
                    format!("claude://code/continue?session={id}"),
                ));
            }
            item.terminal.clone().map(Target::Terminal)
        }
        _ => None,
    }
}

pub fn open(target: &Target) -> OpenResult {
    match target {
        Target::Terminal(terminal) => open_terminal(terminal),
        Target::Chat(provider, url) => {
            let mut opener = Command::new("/usr/bin/open");
            if *provider == Provider::Claude {
                opener.args(["-b", CLAUDE_BUNDLE]);
            }
            if opener
                .arg(url)
                .output()
                .is_ok_and(|output| output.status.success())
            {
                OpenResult::Opened
            } else {
                OpenResult::Failed
            }
        }
    }
}

const APPLE: &str = r#"
on run argv
    set targetTTY to item 1 of argv
    if application id "com.apple.Terminal" is not running then return "missing"
    tell application id "com.apple.Terminal"
        repeat with terminalWindow in windows
            repeat with terminalTab in tabs of terminalWindow
                if tty of terminalTab is targetTTY then
                    set selected of terminalTab to true
                    set frontmost of terminalWindow to true
                    activate
                    return "matched"
                end if
            end repeat
        end repeat
    end tell
    return "missing"
end run
"#;

const ITERM: &str = r#"
on run argv
    set targetTTY to item 1 of argv
    if application id "com.googlecode.iterm2" is not running then return "missing"
    tell application id "com.googlecode.iterm2"
        repeat with terminalWindow in windows
            repeat with terminalTab in tabs of terminalWindow
                repeat with terminalPane in sessions of terminalTab
                    if tty of terminalPane is targetTTY then
                        select terminalWindow
                        select terminalTab
                        select terminalPane
                        activate
                        return "matched"
                    end if
                end repeat
            end repeat
        end repeat
    end tell
    return "missing"
end run
"#;

fn open_terminal(terminal: &TerminalTab) -> OpenResult {
    let script = match terminal.process.host {
        TerminalHost::Apple => APPLE,
        TerminalHost::Iterm => ITERM,
        TerminalHost::Unsupported => return OpenResult::TerminalUnavailable,
    };
    let Some(tty) = activity::tty(terminal.process) else {
        return OpenResult::TerminalUnavailable;
    };
    let still_associated = match &terminal.evidence {
        TerminalEvidence::Codex(path) => activity::still_open(terminal.process, path),
        TerminalEvidence::Claude(path, id) => std::fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
            .is_some_and(|metadata| {
                metadata["sessionId"] == *id
                    && metadata["pid"].as_u64() == Some(u64::from(terminal.process.pid))
            }),
    };
    if !still_associated {
        return OpenResult::TerminalUnavailable;
    }
    let Ok(output) = Command::new("/usr/bin/osascript")
        .args(["-e", script, &tty])
        .output()
    else {
        return OpenResult::Failed;
    };
    if output.status.success() && output.stdout.starts_with(b"matched") {
        OpenResult::Opened
    } else if String::from_utf8_lossy(&output.stderr).contains("-1743") {
        OpenResult::TerminalPermission
    } else if output.status.success() {
        OpenResult::TerminalUnavailable
    } else {
        OpenResult::Failed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_script_does_not_submit_commands() {
        for script in [APPLE, ITERM] {
            assert!(script.contains("tty of"));
            assert!(script.contains("activate"));
            assert!(!script.contains("write text") && !script.contains("do script"));
        }
    }
}
