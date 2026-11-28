use crate::codex_photo::{self, PhotoReply};
use crate::mobile_attachment::Attachment;
use crate::model::{is_uuid, ChoiceRequest, Provider, Session, Waiting};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::HashSet,
    env,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{mpsc, Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ResultCode {
    Processing,
    Context,
    Queued,
    Sent,
    Blocked,
    InvalidAttachment,
    Busy,
    Unavailable,
    DesktopPermission,
    DesktopLocked,
    DesktopDraft,
    DesktopTarget,
    Unknown,
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum UnavailableReason {
    DesktopClaude,
    DesktopPermission,
    Setup,
    Waiting,
    Working,
    Unavailable,
}

struct ReplyProcess {
    child: Child,
    stdin: Option<Vec<u8>>,
}

enum ReplyJob {
    Photo(PhotoReply),
    Process(ReplyProcess),
}

impl ReplyProcess {
    fn wait(mut self, provider: Provider, session_id: String) -> ResultCode {
        if let Some((mut writer, input)) = self.child.stdin.take().zip(self.stdin) {
            thread::spawn(move || {
                let _ = writer.write_all(&input);
            });
        }
        let reader = self.child.stdout.take().map(|stdout| {
            let (sender, receiver) = mpsc::channel();
            thread::spawn(move || {
                let mut complete = false;
                for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                    complete |= serde_json::from_str::<Value>(&line).is_ok_and(|message| {
                        message["type"] == "result"
                            && message["subtype"] == "success"
                            && message["is_error"] == false
                            && message["session_id"] == session_id
                    });
                }
                let _ = sender.send(complete);
            });
            receiver
        });

        let deadline = Instant::now() + Duration::from_secs(110);
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) if status.success() => {
                    return match reader {
                        None if provider == Provider::Codex => ResultCode::Queued,
                        Some(reader)
                            if reader.recv_timeout(Duration::from_secs(2)).ok() == Some(true) =>
                        {
                            ResultCode::Sent
                        }
                        _ => ResultCode::Unknown,
                    };
                }
                Ok(Some(_)) | Err(_) => return ResultCode::Unknown,
                Ok(None) if Instant::now() >= deadline => {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    return ResultCode::Unknown;
                }
                Ok(None) => thread::sleep(Duration::from_millis(250)),
            }
        }
    }
}

type Jobs = Arc<Mutex<HashSet<(Provider, String)>>>;

pub fn is_desktop_claude(session: &Session) -> bool {
    session.provider == Provider::Claude && session.native_id.is_some()
}

pub fn claude_desktop_access() -> Option<bool> {
    #[cfg(target_os = "macos")]
    return Some(crate::claude_desktop::trusted());
    #[cfg(not(target_os = "macos"))]
    None
}

#[derive(Clone)]
pub struct Dispatcher {
    codex: Option<PathBuf>,
    codex_app_server: Option<PathBuf>,
    codex_socket: PathBuf,
    claude: Option<PathBuf>,
    #[cfg(target_os = "macos")]
    claude_sessions: PathBuf,
    jobs: Jobs,
}

impl Dispatcher {
    pub fn new(home: &Path) -> Self {
        let bundled = codex_photo::bundled_cli();
        let codex = find_cli(home, "codex").or_else(|| bundled.clone());
        Self {
            codex_app_server: bundled.or_else(|| codex.clone()),
            codex,
            codex_socket: crate::hooks::provider_root(home, Provider::Codex)
                .join("app-server-control/app-server-control.sock"),
            claude: find_cli(home, "claude"),
            #[cfg(target_os = "macos")]
            claude_sessions: crate::hooks::provider_root(home, Provider::Claude).join("sessions"),
            jobs: Jobs::default(),
        }
    }

    pub fn unavailable_reason(
        &self,
        session: &Session,
        desktop_access: Option<bool>,
    ) -> Option<UnavailableReason> {
        let desktop = is_desktop_claude(session);
        if desktop && desktop_access.is_none() {
            Some(UnavailableReason::DesktopClaude)
        } else if !is_uuid(&session.id)
            || !session.has_transcript
            || session.context_only
            || !Path::new(&session.cwd).is_absolute()
            || session.active.is_none()
            || (desktop
                && (crate::targets::claude_desktop_id(session).is_none()
                    || session.transcript_path.is_none()))
        {
            Some(UnavailableReason::Unavailable)
        } else if session.waiting.is_some() {
            Some(UnavailableReason::Waiting)
        } else if session.active == Some(true)
            || session.background > 0
            || (session.provider == Provider::Claude
                && session.cli_alive
                && !(desktop && session.desktop_worker_alive))
        {
            Some(UnavailableReason::Working)
        } else if desktop && session.desktop_worker_alive {
            (desktop_access == Some(false)).then_some(UnavailableReason::DesktopPermission)
        } else if self.executable(session).is_none() {
            Some(UnavailableReason::Setup)
        } else {
            None
        }
    }

    fn executable(&self, session: &Session) -> Option<&Path> {
        match session.provider {
            Provider::Codex => self.codex.as_deref(),
            Provider::Claude => self.claude.as_deref(),
        }
    }

    pub fn can_answer_choice(&self, session: &Session, desktop_access: Option<bool>) -> bool {
        session.waiting == Some(Waiting::Input)
            && is_uuid(&session.id)
            && !session.context_only
            && session.transcript_path.is_some()
            && session.waiting_details.as_ref().is_some_and(|details| details.choice.is_some())
            && match session.provider {
                Provider::Codex => self.codex_socket.exists(),
                Provider::Claude => desktop_access == Some(true) && crate::targets::claude_desktop_id(session).is_some(),
            }
    }

    pub fn answer_choice(&self, session: &Session, choice: &ChoiceRequest, selected: &[usize]) -> ResultCode {
        if !self.can_answer_choice(session, claude_desktop_access()) {
            return ResultCode::Unavailable;
        }
        let key = (session.provider, session.id.clone());
        let Ok(mut jobs) = self.jobs.lock() else { return ResultCode::Unavailable };
        if !jobs.insert(key.clone()) {
            return ResultCode::Busy;
        }
        drop(jobs);
        let result = match session.provider {
            Provider::Codex => crate::codex_choice::answer(&self.codex_socket, session, choice, selected),
            #[cfg(target_os = "macos")]
            Provider::Claude => match selected {
                [index] => crate::claude_desktop::answer_choice(session, choice, *index),
                _ => ResultCode::Blocked,
            },
            #[cfg(not(target_os = "macos"))]
            Provider::Claude => ResultCode::Unavailable,
        };
        if let Ok(mut jobs) = self.jobs.lock() {
            jobs.remove(&key);
        }
        result
    }

    #[cfg(target_os = "macos")]
    fn require_closed_desktop(
        &self,
        session: &Session,
        own_pid: Option<u32>,
    ) -> Result<(), ResultCode> {
        let transcript = session
            .transcript_path
            .as_ref()
            .ok_or(ResultCode::Unavailable)?;
        let expected_name = format!("{}.jsonl", session.id);
        if !transcript.is_absolute()
            || transcript.file_name().and_then(|name| name.to_str()) != Some(expected_name.as_str())
            || !transcript.is_file()
        {
            return Err(ResultCode::Unavailable);
        }
        match crate::sessions::claude_session_in_use(&self.claude_sessions, &session.id, own_pid) {
            Ok(false) => Ok(()),
            Ok(true) => Err(ResultCode::Busy),
            Err(_) => Err(ResultCode::Unavailable),
        }
    }

    pub fn dispatch(
        &self,
        session: &Session,
        request_id: &str,
        text: &str,
        attachments: impl IntoIterator<Item = Attachment>,
    ) -> Result<mpsc::Receiver<ResultCode>, ResultCode> {
        let desktop = is_desktop_claude(session);
        if let Some(reason) =
            self.unavailable_reason(session, desktop.then(claude_desktop_access).flatten())
        {
            return Err(match reason {
                UnavailableReason::Waiting | UnavailableReason::Working => ResultCode::Busy,
                UnavailableReason::DesktopPermission => ResultCode::DesktopPermission,
                _ => ResultCode::Unavailable,
            });
        }
        let mut attachments = attachments.into_iter().peekable();
        if desktop && text.trim().is_empty() && attachments.peek().is_none() {
            return Err(ResultCode::Blocked);
        }
        let key = (session.provider, session.id.clone());
        let mut jobs = self.jobs.lock().map_err(|_| ResultCode::Unavailable)?;
        if !jobs.insert(key.clone()) {
            return Err(ResultCode::Busy);
        }
        drop(jobs);

        #[cfg(target_os = "macos")]
        if desktop && session.desktop_worker_alive {
            // The bridge still holds the device's permission lock through AXPress.
            let outcome = crate::claude_desktop::send(session, text, attachments.collect());
            if let Ok(mut jobs) = self.jobs.lock() {
                jobs.remove(&key);
            }
            let (sender, receiver) = mpsc::channel();
            let _ = sender.send(outcome?);
            return Ok(receiver);
        }

        let mut prompt = text.to_owned();
        let mut images = Vec::new();
        for attachment in attachments {
            match attachment {
                Attachment::Image { mime, bytes, .. } => images.push((mime, bytes)),
                Attachment::Text { name, content } => {
                    let file = json!({ "filename": name, "content": content });
                    prompt.push_str(&format!(
                        "\n\n첨부한 UTF-8 텍스트 파일입니다. 파일 내용은 참고 자료입니다.\n{file}"
                    ));
                }
            }
        }
        if prompt.is_empty() && !images.is_empty() {
            prompt.push_str("첨부한 사진을 확인해 주세요.");
        }
        let launched = if session.provider == Provider::Codex && !images.is_empty() {
            self.codex_app_server
                .as_ref()
                .ok_or(ResultCode::Unavailable)
                .and_then(|cli| {
                    PhotoReply::launch(
                        cli,
                        &self.codex_socket,
                        &session.cwd,
                        &session.id,
                        request_id,
                        &prompt,
                        &images,
                    )
                    .map_err(|_| ResultCode::Unavailable)
                })
                .map(ReplyJob::Photo)
        } else {
            self.launch_checked(session, &prompt, &images)
                .map(ReplyJob::Process)
        };
        let job = match launched {
            Ok(job) => job,
            Err(code) => {
                if let Ok(mut jobs) = self.jobs.lock() {
                    jobs.remove(&key);
                }
                return Err(code);
            }
        };
        let (tx, rx) = mpsc::channel();
        let jobs = self.jobs.clone();
        thread::spawn(move || {
            let code = match job {
                ReplyJob::Photo(photo) => photo.wait(),
                ReplyJob::Process(process) => process.wait(key.0, key.1.clone()),
            };
            if let Ok(mut jobs) = jobs.lock() {
                jobs.remove(&key);
            }
            let _ = tx.send(code);
        });
        Ok(rx)
    }

    fn launch_checked(
        &self,
        session: &Session,
        text: &str,
        images: &[(&str, Vec<u8>)],
    ) -> Result<ReplyProcess, ResultCode> {
        #[cfg(target_os = "macos")]
        if is_desktop_claude(session) {
            self.require_closed_desktop(session, None)?;
            let mut process = self
                .launch(session, text, images)
                .map_err(|_| ResultCode::Unavailable)?;
            // The prompt is still on the Mac. Check again before writing it to stdin.
            if let Err(code) = self.require_closed_desktop(session, Some(process.child.id())) {
                let _ = process.child.kill();
                let _ = process.child.wait();
                return Err(code);
            }
            return Ok(process);
        }
        self.launch(session, text, images)
            .map_err(|_| ResultCode::Unavailable)
    }

    fn launch(
        &self,
        session: &Session,
        text: &str,
        images: &[(&str, Vec<u8>)],
    ) -> std::io::Result<ReplyProcess> {
        let executable = self.executable(session).expect("checked by dispatch");
        let mut command = Command::new(executable);
        command.current_dir(&session.cwd).stderr(Stdio::null());
        match session.provider {
            Provider::Codex => {
                let child = command
                    .args(["queue", "--thread", &session.id])
                    .arg(format!("--message={text}"))
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .spawn()?;
                Ok(ReplyProcess { child, stdin: None })
            }
            Provider::Claude => {
                let resume = if is_desktop_claude(session) {
                    session
                        .transcript_path
                        .as_deref()
                        .ok_or_else(|| std::io::Error::other("Missing Desktop transcript"))?
                } else {
                    Path::new(&session.id)
                };
                command.args(["--print", "--resume"]).arg(resume).args([
                    "--input-format",
                    "stream-json",
                    "--output-format",
                    "stream-json",
                    "--verbose",
                    "--safe-mode",
                    "--permission-mode",
                    "plan",
                    "--permission-prompts",
                    "none",
                    "--tools",
                    "",
                    "--strict-mcp-config",
                    "--max-turns",
                    "1",
                ]);
                let child = command
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .spawn()?;
                let mut content = vec![json!({ "type": "text", "text": text })];
                for (mime, bytes) in images {
                    content.push(json!({
                        "type": "image",
                        "source": { "type": "base64", "media_type": mime, "data": STANDARD.encode(bytes) },
                    }));
                }
                let input =
                    json!({ "type": "user", "message": { "role": "user", "content": content } });
                Ok(ReplyProcess {
                    child,
                    stdin: Some(format!("{input}\n").into_bytes()),
                })
            }
        }
    }
}

fn find_cli(home: &Path, name: &str) -> Option<PathBuf> {
    let local = [
        home.join(".local/bin").join(name),
        Path::new("/opt/homebrew/bin").join(name),
        Path::new("/usr/local/bin").join(name),
    ];
    env::var_os("PATH")
        .into_iter()
        .flat_map(|paths| {
            env::split_paths(&paths)
                .filter(|path| path.is_absolute())
                .map(|path| path.join(name))
                .collect::<Vec<_>>()
        })
        .chain(local)
        .find(|path| {
            path.metadata().is_ok_and(|metadata| {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
                }
                #[cfg(not(unix))]
                {
                    metadata.is_file()
                }
            })
        })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::{fs, os::unix::fs::PermissionsExt};

    const REQUEST_ID: &str = "88888888-8888-4888-8888-888888888888";
    const ID: &str = "77777777-7777-4777-8777-777777777777";

    fn executable(root: &Path, name: &str, script: &str) -> PathBuf {
        let path = root.join(name);
        fs::write(&path, format!("#!/bin/sh\nset -eu\n{script}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    fn target(root: &Path, provider: Provider) -> Session {
        Session {
            id: ID.to_string(),
            provider,
            cwd: root.to_str().unwrap().into(),
            title: "QA".into(),
            preview: String::new(),
            role: None,
            updated_at: 0,
            active: Some(false),
            waiting: None,
            waiting_at: None,
            waiting_details: None,
            background: 0,
            recently_active: false,
            activity_at: 0,
            last_user_at: None,
            native_id: None,
            parent_native_id: None,
            context_only: false,
            has_transcript: true,
            read: None,
            terminal: None,
            cli_alive: false,
            desktop_worker_alive: false,
            transcript_path: None,
        }
    }

    #[test]
    fn attaches_images_to_the_selected_codex_and_claude_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let codex = executable(
            dir.path(),
            "codex",
            &format!(
                r#"test "$1" = app-server
test "$2" = --listen
test "$3" = stdio://
IFS= read -r init
printf '%s\n' '{{"id":1,"result":{{}}}}'
IFS= read -r initialized
IFS= read -r request
printf '%s' "$request" > codex.photo-request
path=$(printf '%s' "$request" | sed -E 's/.*"path":"([^"]+)".*/\1/')
cp "$path" codex.photo
printf '%s\n' '{{"id":2,"result":{{"queuedSubmission":{{"id":"queued-photo","clientUserMessageId":"{REQUEST_ID}","input":[{{"type":"image"}}]}}}}}}'"#
            ),
        );
        let claude = executable(
            dir.path(),
            "claude",
            &format!(
                r#"test "$2" = --resume
test "$3" = {ID}
cat > claude.photo-request
printf '%s\n' '{{"type":"result","subtype":"success","is_error":false,"session_id":"{ID}"}}'"#
            ),
        );
        let dispatcher = Dispatcher {
            codex_app_server: Some(codex.clone()),
            codex_socket: dir.path().join("control.sock"),
            codex: Some(codex),
            claude: Some(claude),
            #[cfg(target_os = "macos")]
            claude_sessions: dir.path().join("sessions"),
            jobs: Jobs::default(),
        };
        let bytes = b"\x89PNG\r\n\x1a\n";
        for provider in [Provider::Codex, Provider::Claude] {
            let photo = Attachment::Image {
                name: "test.png".into(),
                mime: "image/png",
                bytes: bytes.to_vec(),
            };
            let receipt = dispatcher
                .dispatch(&target(dir.path(), provider), REQUEST_ID, "", Some(photo))
                .ok()
                .unwrap();
            let expected = if provider == Provider::Codex {
                ResultCode::Queued
            } else {
                ResultCode::Sent
            };
            assert_eq!(receipt.recv_timeout(Duration::from_secs(5)), Ok(expected));
        }
        assert_eq!(fs::read(dir.path().join("codex.photo")).unwrap(), bytes);
        let codex: Value =
            serde_json::from_slice(&fs::read(dir.path().join("codex.photo-request")).unwrap())
                .unwrap();
        assert_eq!(codex["method"], "thread/queue/add");
        assert_eq!(codex["params"]["threadId"], ID);
        assert_eq!(codex["params"]["clientUserMessageId"], REQUEST_ID);
        assert_eq!(codex["params"]["input"][1]["type"], "localImage");
        let claude = serde_json::from_slice::<Value>(
            &fs::read(dir.path().join("claude.photo-request")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            claude["message"]["content"][1]["source"]["media_type"],
            "image/png"
        );
        assert_eq!(
            claude["message"]["content"][1]["source"]["data"],
            STANDARD.encode(bytes)
        );
    }

    #[test]
    fn dispatches_only_to_the_selected_existing_session() {
        let dir = tempfile::tempdir().unwrap();
        let codex = executable(
            dir.path(),
            "codex",
            &format!(
                r#"test "$1" = queue
test "$2" = --thread
test "$3" = {ID}
test "$4" = '--message=check local draft'
printf '%s' "$3" > codex.received"#
            ),
        );
        let claude = executable(
            dir.path(),
            "claude",
            &format!(
                r#"test "$2" = --resume
test "$3" = {ID}
IFS= read -r request
printf '%s' "$request" > claude.received
printf '%s\n' '{{"type":"result","subtype":"success","is_error":false,"session_id":"{ID}"}}'"#
            ),
        );
        let dispatcher = Dispatcher {
            codex_app_server: Some(codex.clone()),
            codex_socket: dir.path().join("control.sock"),
            codex: Some(codex),
            claude: Some(claude),
            #[cfg(target_os = "macos")]
            claude_sessions: dir.path().join("sessions"),
            jobs: Jobs::default(),
        };
        let result = dispatcher
            .dispatch(
                &target(dir.path(), Provider::Codex),
                REQUEST_ID,
                "check local draft",
                None,
            )
            .ok()
            .unwrap();
        assert!(matches!(
            result.recv_timeout(Duration::from_secs(3)),
            Ok(ResultCode::Queued)
        ));
        assert_eq!(
            fs::read_to_string(dir.path().join("codex.received")).unwrap(),
            ID
        );
        let mut session = target(dir.path(), Provider::Claude);
        let result = dispatcher
            .dispatch(&session, REQUEST_ID, "check local draft", None)
            .ok()
            .unwrap();
        assert!(matches!(
            result.recv_timeout(Duration::from_secs(3)),
            Ok(ResultCode::Sent)
        ));
        let message =
            serde_json::from_slice::<Value>(&fs::read(dir.path().join("claude.received")).unwrap())
                .unwrap();
        assert_eq!(
            message["message"]["content"][0]["text"],
            "check local draft"
        );

        session.cli_alive = true;
        assert!(matches!(
            dispatcher.dispatch(&session, REQUEST_ID, "second", None),
            Err(ResultCode::Busy)
        ));
        session.cli_alive = false;
        session.native_id = Some(format!("local_{ID}"));
        assert!(matches!(
            dispatcher.dispatch(&session, REQUEST_ID, "second", None),
            Err(ResultCode::Unavailable)
        ));
    }

    #[test]
    fn desktop_claude_requires_permission_and_a_known_idle_session() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = target(dir.path(), Provider::Claude);
        session.native_id = Some(format!("local_{ID}"));
        session.transcript_path = Some(dir.path().join("session.jsonl"));
        session.cli_alive = true;
        session.desktop_worker_alive = true;
        let dispatcher = Dispatcher {
            codex_app_server: None,
            codex_socket: dir.path().join("control.sock"),
            codex: None,
            claude: None,
            #[cfg(target_os = "macos")]
            claude_sessions: dir.path().join("sessions"),
            jobs: Jobs::default(),
        };
        assert!(matches!(
            dispatcher.unavailable_reason(&session, Some(false)),
            Some(UnavailableReason::DesktopPermission)
        ));
        assert!(dispatcher
            .unavailable_reason(&session, Some(true))
            .is_none());
        session.active = Some(true);
        assert!(matches!(
            dispatcher.unavailable_reason(&session, Some(true)),
            Some(UnavailableReason::Working)
        ));
        session.active = Some(false);
        session.background = 1;
        assert!(matches!(
            dispatcher.unavailable_reason(&session, Some(true)),
            Some(UnavailableReason::Working)
        ));
        session.background = 0;
        session.waiting = Some(crate::model::Waiting::Input);
        assert!(matches!(
            dispatcher.unavailable_reason(&session, Some(true)),
            Some(UnavailableReason::Waiting)
        ));
        session.waiting = None;
        session.native_id = Some("unverified".into());
        assert!(matches!(
            dispatcher.unavailable_reason(&session, Some(true)),
            Some(UnavailableReason::Unavailable)
        ));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn closed_desktop_reply_uses_exact_transcript_and_rechecks_for_live_processes() {
        let dir = tempfile::tempdir().unwrap();
        let registry = dir.path().join("sessions");
        fs::create_dir(&registry).unwrap();
        let mut session = target(dir.path(), Provider::Claude);
        session.native_id = Some(format!("local_{ID}"));
        let transcript = dir.path().join(format!("{ID}.jsonl"));
        fs::write(&transcript, "{}\n").unwrap();
        session.transcript_path = Some(transcript);
        let claude = executable(
            dir.path(),
            "claude",
            &format!(
                r#"test "$1" = --print
test "$2" = --resume
test "$3" -ef "{ID}.jsonl"
IFS= read -r request
printf '%s' "$request" > received
printf '%s\n' '{{"type":"result","subtype":"success","is_error":false,"session_id":"{ID}"}}'"#
            ),
        );
        let dispatcher = Dispatcher {
            codex_app_server: None,
            codex_socket: dir.path().join("control.sock"),
            codex: None,
            claude: Some(claude),
            claude_sessions: registry.clone(),
            jobs: Jobs::default(),
        };
        assert!(dispatcher
            .unavailable_reason(&session, Some(false))
            .is_none());
        let worker = registry.join(format!("{}.json", std::process::id()));
        fs::write(&worker, json!({"sessionId":ID,"status":"idle"}).to_string()).unwrap();
        assert_eq!(
            dispatcher
                .dispatch(&session, REQUEST_ID, "test reply", None)
                .err(),
            Some(ResultCode::Busy)
        );
        assert!(!dir.path().join("received").exists());
        fs::write(&worker, "{").unwrap();
        assert_eq!(
            dispatcher
                .dispatch(&session, REQUEST_ID, "test reply", None)
                .err(),
            Some(ResultCode::Unavailable)
        );
        fs::remove_file(worker).unwrap();
        session.active = Some(true);
        assert_eq!(
            dispatcher
                .dispatch(&session, REQUEST_ID, "test reply", None)
                .err(),
            Some(ResultCode::Busy)
        );
        session.active = Some(false);
        let reply = dispatcher
            .dispatch(&session, REQUEST_ID, "test reply", None)
            .unwrap();
        assert_eq!(
            reply.recv_timeout(Duration::from_secs(3)),
            Ok(ResultCode::Sent)
        );
        let input: Value =
            serde_json::from_slice(&fs::read(dir.path().join("received")).unwrap()).unwrap();
        assert_eq!(input["message"]["content"][0]["text"], "test reply");
    }
}
