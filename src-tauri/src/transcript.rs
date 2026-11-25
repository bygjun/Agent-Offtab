use crate::model::{Provider, Role, Waiting};
use chrono::DateTime;
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use regex::Regex;
use serde_json::Value;
use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    fs::{self, File},
    io::{self, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::LazyLock,
    time::{SystemTime, UNIX_EPOCH},
};

const CHUNK: u64 = 256 * 1024;
const MAX_TAIL: u64 = 4 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct Message {
    pub text: String,
    pub role: Role,
    pub at: i64,
}

#[derive(Clone, Copy, Debug)]
pub struct Turn {
    pub active: bool,
    pub at: i64,
}

#[derive(Clone, Debug, Default)]
pub struct Log {
    pub custom_title: String,
    pub ai_title: String,
    pub first_user: String,
    pub cwd: String,
    pub latest: Option<Message>,
    pub last_user_at: Option<i64>,
    pub desktop: bool,
    pub turn: Option<Turn>,
    pub waiting: Option<(Waiting, i64)>,
    pub desktop_card_at: Option<i64>,
    pub background: HashSet<String>,
}

#[derive(Default)]
struct JobEvidence {
    answered: HashSet<String>,
    shown_cards: HashSet<String>,
    latest_wrap_checked: bool,
    tasks: HashMap<String, bool>,
    finished_turn: bool,
}

fn xml_field<'a>(value: &'a str, name: &str) -> &'a str {
    value
        .split_once(&format!("<{name}>"))
        .and_then(|(_, rest)| rest.split_once(&format!("</{name}>")))
        .map_or("", |(body, _)| body.trim())
}

impl JobEvidence {
    fn notification(&mut self, value: &str) {
        if value.trim_start().starts_with("<task-notification>")
            && matches!(
                xml_field(value, "status"),
                "completed" | "failed" | "stopped" | "killed" | "terminated" | "cancelled"
            )
        {
            self.task(xml_field(value, "task-id"), false);
        }
    }

    fn pending_question(&self, id: &str) -> bool {
        !self.finished_turn && !id.is_empty() && !self.answered.contains(id)
    }

    fn task(&mut self, id: &str, running: bool) {
        if !id.is_empty() && id.len() <= 128 {
            self.tasks.entry(id.to_owned()).or_insert(running);
        }
    }

    fn record(&mut self, event: &Value, provider: Provider, log: &mut Log) {
        if provider == Provider::Codex {
            let payload = &event["payload"];
            if event["type"] == "response_item" {
                let id = text(&payload["call_id"]);
                match text(&payload["type"]) {
                    "function_call_output" if !id.is_empty() => {
                        self.answered.insert(id.to_owned());
                    }
                    "function_call"
                        if payload["name"] == "request_user_input" && self.pending_question(id) =>
                    {
                        log.waiting
                            .get_or_insert_with(|| (Waiting::Input, date(&event["timestamp"])));
                    }
                    _ => {}
                }
            }
            if event["type"] == "event_msg" {
                let item = &payload["item"];
                match text(&payload["type"]) {
                    "exec_command_begin" | "exec_command_end" => {
                        self.task(
                            text(&payload["call_id"]),
                            payload["type"] == "exec_command_begin",
                        );
                    }
                    "item_started" | "item_completed"
                        if matches!(
                            text(&item["type"]),
                            "command_execution" | "commandExecution"
                        ) =>
                    {
                        self.task(
                            text(&item["id"]),
                            payload["type"] == "item_started"
                                && !matches!(
                                    text(&item["status"]),
                                    "completed" | "failed" | "declined"
                                ),
                        );
                    }
                    _ => {}
                }
            }
            if codex_turn(event).is_some_and(|turn| !turn.active) {
                self.finished_turn = true;
            }
            return;
        }
        if event["isSidechain"] == true {
            return;
        }
        let blocks = event["message"]["content"].as_array().into_iter().flatten();
        match text(&event["type"]) {
            "assistant" => {
                for block in blocks.rev().filter(|block| block["type"] == "tool_use") {
                    let id = text(&block["id"]);
                    if block["name"] == "AskUserQuestion" && self.pending_question(id) {
                        log.waiting
                            .get_or_insert_with(|| (Waiting::Input, date(&event["timestamp"])));
                    }
                    if block["name"] == "mcp__ccd_turn__wrap_up" && !self.latest_wrap_checked {
                        self.latest_wrap_checked = true;
                        // This tool result confirms the card was rendered. It is not the user's reply.
                        let at = date(&event["timestamp"]);
                        if !self.finished_turn
                            && self.shown_cards.contains(id)
                            && !text(&block["input"]["ask"]).trim().is_empty()
                            && at > 0
                        {
                            log.desktop_card_at = Some(at);
                        }
                    }
                }
            }
            "user" => {
                self.notification(text(&event["message"]["content"]));
                for block in blocks {
                    if block["type"] == "tool_result" {
                        let id = text(&block["tool_use_id"]);
                        if !id.is_empty() {
                            self.answered.insert(id.to_owned());
                            let content = &block["content"];
                            let shown =
                                |body: &str| body.starts_with("Shown in the next-step box.");
                            if block["is_error"] != true
                                && (content.as_str().is_some_and(shown)
                                    || content.as_array().into_iter().flatten().any(|part| {
                                        part["type"] == "text" && shown(text(&part["text"]))
                                    }))
                            {
                                self.shown_cards.insert(id.to_owned());
                            }
                        }
                    }
                    if block["type"] == "text" {
                        self.notification(text(&block["text"]));
                    }
                }
                let tool = &event["toolUseResult"];
                match text(&tool["status"]) {
                    "async_launched" => self.task(text(&tool["agentId"]), true),
                    "completed" | "failed" | "stopped" | "killed" => {
                        self.task(text(&tool["agentId"]), false)
                    }
                    _ => {}
                }
                let task = &tool["task"];
                if matches!(
                    text(&task["status"]),
                    "completed" | "failed" | "stopped" | "killed"
                ) {
                    self.task(text(&task["id"]), false);
                }
                if event["isMeta"] != true
                    && (event["message"]["content"]
                        .as_str()
                        .is_some_and(|text| !text.trim().is_empty())
                        || event["message"]["content"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .any(|block| {
                                block["type"] == "text" && !text(&block["text"]).trim().is_empty()
                            }))
                {
                    self.finished_turn = true;
                }
            }
            "attachment" => {
                let detail = &event["attachment"];
                match text(&detail["type"]) {
                    "task_status" => match text(&detail["status"]) {
                        "running" if detail["taskType"] == "local_agent" => {
                            self.task(text(&detail["taskId"]), true)
                        }
                        "completed" | "failed" | "stopped" | "killed" => {
                            self.task(text(&detail["taskId"]), false)
                        }
                        _ => {}
                    },
                    "queued_command" => self.notification(text(&detail["prompt"])),
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Stamp {
    len: u64,
    modified: Option<SystemTime>,
    #[cfg(unix)]
    inode: u64,
}

impl Stamp {
    pub fn of(meta: &fs::Metadata) -> Self {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        Self {
            len: meta.len(),
            modified: meta.modified().ok(),
            #[cfg(unix)]
            inode: meta.ino(),
        }
    }
}

#[derive(Default)]
pub struct TranscriptCache(HashMap<PathBuf, (Stamp, bool, Log)>);

pub fn text(value: &Value) -> &str {
    value.as_str().unwrap_or_default()
}

pub fn first_text<'a>(texts: &[&'a str]) -> &'a str {
    texts
        .iter()
        .copied()
        .find(|text| !text.is_empty())
        .unwrap_or_default()
}

pub fn epoch(time: SystemTime) -> i64 {
    time.duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis() as i64)
}

pub fn date(value: &Value) -> i64 {
    value
        .as_i64()
        .or_else(|| {
            DateTime::parse_from_rfc3339(text(value))
                .ok()
                .map(|date| date.timestamp_millis())
        })
        .unwrap_or_default()
}

pub fn compact(text: &str, max: usize) -> String {
    let mut chars = text
        .split_whitespace()
        .flat_map(|word| std::iter::once(' ').chain(word.chars()))
        .skip(1);
    let mut normalized: String = chars.by_ref().take(max).collect();
    if max > 0 && chars.next().is_some() {
        normalized.pop();
        normalized.push('…');
    }
    normalized
}

fn preview_text(markdown: &str) -> String {
    let options =
        Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES | Options::ENABLE_TASKLISTS;
    let mut text = String::new();
    for event in Parser::new_ext(markdown, options) {
        match event {
            Event::Text(value) | Event::Code(value) => text.push_str(&value),
            Event::Start(Tag::Item) if !text.trim_end().is_empty() => text.push_str(" · "),
            Event::TaskListMarker(checked) => text.push_str(if checked { "✓ " } else { "○ " }),
            Event::SoftBreak
            | Event::HardBreak
            | Event::Rule
            | Event::End(
                TagEnd::Paragraph
                | TagEnd::Heading(_)
                | TagEnd::BlockQuote(_)
                | TagEnd::TableCell
                | TagEnd::CodeBlock,
            ) => text.push(' '),
            _ => {}
        }
    }
    compact(&text, 420)
}

pub fn content(value: &Value) -> String {
    if let Some(text) = value.as_str() {
        return text.to_owned();
    }
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter(|block| matches!(text(&block["type"]), "text" | "input_text" | "output_text"))
        .map(|block| text(&block["text"]))
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn user_body(text: &str) -> Option<Cow<'_, str>> {
    static REMINDERS: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
        r"(?is)<environment_context>.*?</environment_context>|<system-reminder>.*?</system-reminder>"
    ).expect("valid reminder pattern")
    });
    static INTERNAL: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
        r"(?i)^(?:<(?:local-command-[\w-]+|command-[\w-]+|task-notification|teammate-message)\b|# AGENTS\.md instructions for |Caveat: The messages below were generated)"
    ).expect("valid internal-message pattern")
    });
    static ATTACHMENT_TAGS: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r#"(?m)^[ \t]*(?:<image name=\[Image #[0-9]+\] path="[^"\r\n]+">|</image>)[ \t]*\r?$"#,
        )
        .expect("valid app-attachment pattern")
    });

    let cleaned = REMINDERS.replace_all(text, "");
    if INTERNAL.is_match(cleaned.trim_start()) {
        return None;
    }
    let cleaned = if let Some((_, request)) = cleaned
        .trim_start()
        .strip_prefix("# Files mentioned by the user:")
        .and_then(|body| body.split_once("\n## My request:\n"))
    {
        ATTACHMENT_TAGS.replace_all(request, "").into_owned().into()
    } else {
        cleaned
    };
    (!cleaned.trim().is_empty()).then_some(cleaned)
}

pub fn message(text: &str, role: Role, at: i64) -> Option<Message> {
    static DIRECTIVES: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?m)^[ \t]*::[a-z][a-z0-9-]*\{[^\r\n]*\}[ \t]*\r?$")
            .expect("valid app-directive pattern")
    });
    let cleaned = match role {
        Role::Assistant => DIRECTIVES.replace_all(text, ""),
        Role::User => user_body(text)?,
    };
    let text = preview_text(&cleaned);
    (!text.is_empty()).then_some(Message { text, role, at })
}

fn codex_message(event: &Value) -> Option<Message> {
    let payload = &event["payload"];
    let (value, role) = match (
        text(&event["type"]),
        text(&payload["type"]),
        text(&payload["role"]),
    ) {
        ("response_item", "message", "user") => (content(&payload["content"]), Role::User),
        ("response_item", "message", "assistant") => {
            (content(&payload["content"]), Role::Assistant)
        }
        ("event_msg", "user_message", _) => (text(&payload["message"]).to_owned(), Role::User),
        ("event_msg", "agent_message", _) => {
            (text(&payload["message"]).to_owned(), Role::Assistant)
        }
        _ => return None,
    };
    message(&value, role, date(&event["timestamp"]))
}

fn codex_turn(event: &Value) -> Option<Turn> {
    if event["type"] != "event_msg" {
        return None;
    }
    let active = match text(&event["payload"]["type"]) {
        "task_started" | "turn_started" => true,
        "task_complete" | "turn_complete" | "turn_aborted" => false,
        _ => return None,
    };
    Some(Turn {
        active,
        at: date(&event["timestamp"]),
    })
}

fn claude_message(event: &Value) -> Option<Message> {
    let role = match text(&event["type"]) {
        "user" => Role::User,
        "assistant" => Role::Assistant,
        _ => return None,
    };
    if !is_claude_chat(event) {
        return None;
    }
    message(
        &content(&event["message"]["content"]),
        role,
        date(&event["timestamp"]),
    )
}

fn is_claude_chat(event: &Value) -> bool {
    !["isSidechain", "isMeta", "isSynthetic"]
        .iter()
        .any(|key| event[key].as_bool() == Some(true))
        && event["promptSource"] != "system"
        && matches!(text(&event["turnOrigin"]), "" | "human")
}

fn user_message_at(event: &Value, provider: Provider) -> Option<i64> {
    let at = date(&event["timestamp"]);
    if at <= 0 {
        return None;
    }
    let sent = match provider {
        Provider::Codex => {
            user_body(text(&event["payload"]["message"])).is_some()
                || ["images", "local_images"].iter().any(|key| {
                    event["payload"][key]
                        .as_array()
                        .is_some_and(|items| !items.is_empty())
                })
        }
        Provider::Claude => {
            let input = &event["message"]["content"];
            user_body(&content(input)).is_some()
                || input.as_array().is_some_and(|blocks| {
                    blocks
                        .iter()
                        .any(|block| matches!(text(&block["type"]), "image" | "document"))
                })
        }
    };
    sent.then_some(at)
}

fn metadata(result: &mut Log, event: &Value) {
    result.desktop |= event["entrypoint"] == "claude-desktop";
    if result.cwd.is_empty() {
        result.cwd = text(&event["cwd"]).to_owned();
    }
    match text(&event["type"]) {
        "custom-title" if result.custom_title.is_empty() => {
            result.custom_title = text(&event["customTitle"]).to_owned()
        }
        "ai-title" if result.ai_title.is_empty() => {
            result.ai_title = text(&event["aiTitle"]).to_owned()
        }
        _ => {}
    }
}

fn read_range(file: &mut File, start: u64, end: u64) -> io::Result<Vec<u8>> {
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::with_capacity((end - start) as usize);
    file.take(end - start).read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn parse_log(
    file: &mut File,
    len: u64,
    provider: Provider,
    live: bool,
    previous: Option<(u64, &Log)>,
    previous_user: Option<(u64, Option<i64>)>,
) -> io::Result<Log> {
    let mut result = Log::default();
    let mut jobs = JobEvidence::default();
    let mut end = len;
    let floor = len.saturating_sub(MAX_TAIL);
    let mut carry = Vec::new();
    let get_message = match provider {
        Provider::Codex => codex_message,
        Provider::Claude => claude_message,
    };
    while end > floor {
        let start = floor.max(end.saturating_sub(CHUNK));
        let mut bytes = read_range(file, start, end)?;
        bytes.append(&mut carry);
        let boundary = if start == 0 {
            Some(0)
        } else {
            bytes
                .iter()
                .position(|&byte| byte == b'\n')
                .map(|index| index + 1)
        };
        if let Some(boundary) = boundary {
            for line in bytes[boundary..].rsplit(|&byte| byte == b'\n') {
                let Ok(event) = serde_json::from_slice::<Value>(line) else {
                    continue;
                };
                let needs_user = result.last_user_at.is_none()
                    && match provider {
                        Provider::Codex => {
                            event["type"] == "event_msg"
                                && event["payload"]["type"] == "user_message"
                        }
                        Provider::Claude => event["type"] == "user" && is_claude_chat(&event),
                    };
                if needs_user {
                    result.last_user_at = user_message_at(&event, provider);
                }
                if result.latest.is_none() {
                    result.latest = get_message(&event);
                }
                if provider == Provider::Codex && result.turn.is_none() {
                    result.turn = codex_turn(&event);
                }
                jobs.record(&event, provider, &mut result);
                metadata(&mut result, &event);
            }
            bytes.truncate(boundary.saturating_sub(1));
        }
        carry = bytes;
        end = start;
        let enough = match provider {
            Provider::Codex => result.turn.is_some(),
            Provider::Claude => !result.custom_title.is_empty(),
        };
        let tasks_covered = !live || previous.is_some_and(|(len, _)| end <= len);
        let user_covered =
            result.last_user_at.is_some() || previous_user.is_some_and(|(len, _)| end <= len);
        if result.latest.is_some() && user_covered && enough && tasks_covered {
            break;
        }
    }
    result.last_user_at = result
        .last_user_at
        .max(previous_user.and_then(|(_, at)| at));
    if let Some((_, old)) = previous {
        for id in &old.background {
            jobs.tasks.entry(id.clone()).or_insert(true);
        }
    }
    result.background = jobs
        .tasks
        .into_iter()
        .filter_map(|(id, running)| running.then_some(id))
        .collect();
    if provider == Provider::Claude {
        for line in read_range(file, 0, len.min(CHUNK))?.split(|&byte| byte == b'\n') {
            let Ok(event) = serde_json::from_slice::<Value>(line) else {
                continue;
            };
            metadata(&mut result, &event);
            if result.first_user.is_empty() && event["type"] == "user" {
                if let Some(message) = get_message(&event) {
                    result.first_user = message.text;
                }
            }
        }
    }
    Ok(result)
}

impl TranscriptCache {
    pub fn read(&mut self, path: &Path, provider: Provider, live: bool) -> io::Result<Log> {
        let mut file = File::open(path)?;
        let stamp = Stamp::of(&file.metadata()?);
        let cached = self.0.get(path);
        if let Some((cached, known_live, log)) = cached {
            if *cached == stamp && (*known_live || !live) {
                return Ok(log.clone());
            }
        }
        let appended = cached.filter(|(old, _, _)| old.inode == stamp.inode && stamp.len > old.len);
        let previous = appended
            .filter(|(old, known_live, _)| live && *known_live && stamp.len - old.len <= MAX_TAIL)
            .map(|(old, _, log)| (old.len, log));
        let log = parse_log(
            &mut file,
            stamp.len,
            provider,
            live,
            previous,
            appended.map(|(old, _, log)| (old.len, log.last_user_at)),
        )?;
        self.0.insert(path.to_owned(), (stamp, live, log.clone()));
        Ok(log)
    }

    pub fn limit(&mut self) {
        if self.0.len() > 1000 {
            self.0.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn codex_preview_skips_control_records_and_preserves_visible_text() {
        let messages = [
            json!({"type":"response_item", "timestamp":1, "payload":{
                "type":"message", "role":"assistant", "phase":"commentary",
                "content":[{"type":"output_text", "text":"화면을 확인했습니다.\n::code-comment{title=\"Example\"}"}]
            }}),
            json!({"type":"response_item", "timestamp":2, "payload":{
                "type":"message", "role":"assistant", "phase":"commentary",
                "content":[{"type":"output_text", "text":"::browser-complete{title=\"Checked the window\"}"}]
            }}),
        ];
        let latest = messages.iter().rev().find_map(codex_message).unwrap();
        assert_eq!(latest.text, "화면을 확인했습니다.");
        assert_eq!(latest.at, 1);
    }

    #[test]
    fn attachment_preview_shows_the_user_request_without_app_metadata() {
        let body = "\n# Files mentioned by the user:\n\n- image.png: /private/tmp/image.png\n\n## My request:\n화면을 확인해 줘\n<image name=[Image #1] path=\"/private/tmp/image.png\">\n</image>";
        assert_eq!(
            message(body, Role::User, 1).unwrap().text,
            "화면을 확인해 줘"
        );
        let visible = "::browser-complete{title=\"Example\"} 이 표기를 확인해 줘";
        assert_eq!(message(visible, Role::User, 2).unwrap().text, visible);
    }

    #[test]
    fn markdown_preview_keeps_meaning_and_hides_formatting_syntax() {
        let source = "## 상태\n\n네, 잘 돌고 있습니다.\n\n- **진행:** 799판 중 565판\n- **속도:** `run_batch` 한 판 평균 37초\n- [결과 보기](https://example.com/result)\n- ~~대기~~";
        assert_eq!(preview_text(source), "상태 네, 잘 돌고 있습니다. · 진행: 799판 중 565판 · 속도: run_batch 한 판 평균 37초 · 결과 보기 · 대기");
        assert_eq!(
            preview_text("[x](javascript:alert%281%29)\n\n<script>alert(1)</script>"),
            "x"
        );
        assert_eq!(
            preview_text(&format!("**{}**", "진".repeat(500)))
                .chars()
                .count(),
            420
        );
    }

    #[test]
    fn claude_task_notifications_close_detached_work_and_a_tool_result_clears_input() {
        let pending = json!({"type":"assistant", "timestamp":12, "message":{"content":[{"type":"tool_use", "name":"AskUserQuestion", "id":"question"}]}});
        let answer = json!({"type":"user", "timestamp":13, "message":{"content":[{"type":"tool_result", "tool_use_id":"question"}]}});
        let launched = json!({"type":"user", "timestamp":11, "toolUseResult":{"status":"async_launched", "agentId":"agent-1"}});
        let unconfirmed_shell =
            json!({"type":"user", "timestamp":11, "toolUseResult":{"backgroundTaskId":"shell-1"}});
        let complete = json!({"type":"user", "isMeta":true,"message":{"content":[{"type":"text","text":"<task-notification><task-id>agent-1</task-id><status>completed</status></task-notification>"}]}});
        let mut log = Log::default();
        let mut evidence = JobEvidence::default();
        evidence.record(&pending, Provider::Claude, &mut log);
        evidence.record(&launched, Provider::Claude, &mut log);
        assert_eq!(log.waiting, Some((Waiting::Input, 12)));
        evidence.record(&unconfirmed_shell, Provider::Claude, &mut log);
        assert_eq!(evidence.tasks.get("shell-1"), None);
        assert_eq!(evidence.tasks.get("agent-1"), Some(&true));
        let mut log = Log::default();
        let mut evidence = JobEvidence::default();
        for event in [complete, answer, pending, launched] {
            evidence.record(&event, Provider::Claude, &mut log);
        }
        assert_eq!(evidence.tasks.get("agent-1"), Some(&false));
        assert!(log.waiting.is_none());
    }
}
