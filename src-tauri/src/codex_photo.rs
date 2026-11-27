use crate::{mobile_attachment::MAX_TOTAL_BYTES, mobile_reply::ResultCode};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    io::{self, BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::mpsc,
    thread,
    time::Duration,
};

pub struct PhotoReply {
    child: Child,
    params: Value,
    photos: Vec<tempfile::NamedTempFile>,
}

pub fn bundled_cli() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        use objc2_app_kit::NSWorkspace;
        use objc2_foundation::NSString;
        objc2::rc::autoreleasepool(|_| {
            let app = NSWorkspace::sharedWorkspace()
                .URLForApplicationWithBundleIdentifier(&NSString::from_str("com.openai.codex"))?;
            let app = PathBuf::from(app.path()?.to_string());
            [
                "Contents/Resources/codex-cli/CodexCLI.app/Contents/MacOS/codex",
                "Contents/Resources/codex",
            ]
            .iter()
            .map(|path| app.join(path))
            .find(|path| path.is_file())
        })
    }
    #[cfg(not(target_os = "macos"))]
    None
}

impl PhotoReply {
    pub fn launch(
        cli: &Path,
        daemon_socket: &Path,
        cwd: &str,
        thread_id: &str,
        request_id: &str,
        text: &str,
        images: &[(&str, Vec<u8>)],
    ) -> io::Result<Self> {
        let mut input = vec![json!({"type": "text", "text": text})];
        let mut photos = Vec::with_capacity(images.len());
        for (mime, bytes) in images {
            let suffix = match *mime {
                "image/png" => ".png",
                "image/webp" => ".webp",
                _ => ".jpg",
            };
            let mut photo = tempfile::Builder::new()
                .prefix("offtab-photo-")
                .suffix(suffix)
                .tempfile()?;
            photo.write_all(bytes)?;
            input.push(json!({"type": "localImage", "path": photo.path()}));
            photos.push(photo);
        }
        let params = json!({
            "threadId": thread_id,
            "clientUserMessageId": request_id,
            "input": input,
        });
        let mut command = Command::new(cli);
        command.arg("app-server");
        if daemon_socket.exists() {
            command.args(["proxy", "--sock"]).arg(daemon_socket);
        } else {
            command.args(["--listen", "stdio://"]);
        }
        let child = command
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        Ok(Self {
            child,
            params,
            photos,
        })
    }

    pub fn wait(mut self) -> ResultCode {
        let mut input = self.child.stdin.take().expect("piped Codex stdin");
        let output = self.child.stdout.take().expect("piped Codex stdout");
        let (sender, receiver) = mpsc::channel();
        let count = self.photos.len();
        thread::spawn(move || {
            let _ = sender.send(queue_photo(
                &mut input,
                BufReader::new(output),
                self.params,
                count,
            ));
        });
        let outcome = receiver
            .recv_timeout(Duration::from_secs(25))
            .unwrap_or(ResultCode::Unknown);
        // This process only submits to the queue. The thread's own server runs the turn.
        let _ = self.child.kill();
        let _ = self.child.wait();
        outcome
    }
}

#[derive(Deserialize)]
struct Response {
    id: Option<u64>,
    result: Option<QueueResult>,
    error: Option<serde::de::IgnoredAny>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct QueueResult {
    queued_submission: Option<Submission>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Submission {
    id: String,
    client_user_message_id: String,
    input: Vec<SubmissionInput>,
}

#[derive(Deserialize)]
struct SubmissionInput {
    #[serde(rename = "type")]
    kind: String,
}

fn queue_photo(
    writer: &mut impl Write,
    reader: impl BufRead,
    params: Value,
    image_count: usize,
) -> ResultCode {
    // The queue response can echo the base64 image. Bound reads and skip the image bytes.
    let mut reader = BufReader::new(reader.take((MAX_TOTAL_BYTES * 2) as u64));
    let mut line = Vec::new();
    let receive = |reader: &mut dyn BufRead, line: &mut Vec<u8>, id: u64| -> Option<Response> {
        loop {
            line.clear();
            if reader.read_until(b'\n', line).ok()? == 0 || !line.ends_with(b"\n") {
                return None;
            }
            if let Ok(response) = serde_json::from_slice::<Response>(line) {
                if response.id == Some(id) {
                    return Some(response);
                }
            }
        }
    };
    let initialize = json!({
        "id": 1, "method": "initialize", "params": {
            "clientInfo": {"name": "offtab", "version": env!("CARGO_PKG_VERSION")},
            "capabilities": {"experimentalApi": true}
        }
    });
    if send(writer, &initialize).is_err()
        || !receive(&mut reader, &mut line, 1)
            .is_some_and(|r| r.error.is_none() && r.result.is_some())
        || send(writer, &json!({"method": "initialized", "params": {}})).is_err()
    {
        return ResultCode::Unavailable;
    }
    let request = json!({"id": 2, "method": "thread/queue/add", "params": params});
    if send(writer, &request).is_err() {
        return ResultCode::Unknown;
    }
    let Some(reply) = receive(&mut reader, &mut line, 2) else {
        return ResultCode::Unknown;
    };
    if reply.error.is_some() {
        return ResultCode::Unavailable;
    }
    if reply
        .result
        .and_then(|r| r.queued_submission)
        .is_some_and(|item| {
            !item.id.is_empty()
                && request["params"]["clientUserMessageId"] == item.client_user_message_id
                && item
                    .input
                    .iter()
                    .filter(|input| input.kind == "image")
                    .count()
                    == image_count
        })
    {
        ResultCode::Queued
    } else {
        ResultCode::Unknown
    }
}

fn send(writer: &mut impl Write, request: &Value) -> io::Result<()> {
    serde_json::to_writer(&mut *writer, request)?;
    writer.write_all(b"\n")?;
    writer.flush()
}
