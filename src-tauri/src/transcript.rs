use crate::model::{ChoiceQuestion, ChoiceRequest, Provider, Role, Waiting, WaitingQuestion};
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
const MAX_CLAUDE_PREVIEW_TAIL: u64 = 16 * 1024 * 1024;

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
    pub pending_questions: Option<(String, Vec<WaitingQuestion>)>,
    pub codex_choice: Option<ChoiceRequest>,
    pub desktop_card_at: Option<i64>,
    pub desktop_questions: Vec<WaitingQuestion>,
    pub desktop_choice: Option<ChoiceRequest>,
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

fn waiting_question(prompt: &Value, items: &Value) -> Option<WaitingQuestion> {
    let question = compact(text(prompt), 600);
    if question.is_empty() {
        return None;
    }
    let options = items
        .as_array()
        .into_iter()
        .flatten()
        .map(|item| compact(text(&item["label"]), 160))
        .filter(|label| !label.is_empty())
        .take(8)
        .collect();
    Some(WaitingQuestion { question, options })
}

fn choice_question(id: &str, prompt: &Value, items: &Value) -> Option<ChoiceQuestion> {
    let question = text(prompt);
    let items = items.as_array()?;
    if question.is_empty() || question.chars().count() > 600 || !(1..=8).contains(&items.len()) {
        return None;
    }
    let options = items.iter().map(|item| {
        let label = text(&item["label"]);
        (!label.trim().is_empty() && label.chars().count() <= 160 && !label.chars().any(char::is_control))
            .then(|| label.to_owned())
    }).collect::<Option<Vec<_>>>()?;
    if options.iter().collect::<HashSet<_>>().len() != options.len() {
        return None;
    }
    Some(ChoiceQuestion { id: id.to_owned(), question: question.to_owned(), options })
}

fn codex_choice(call_id: &str, item_id: &str, input: &Value) -> Option<ChoiceRequest> {
    let items = input["questions"].as_array()?;
    if !(1..=4).contains(&items.len()) {
        return None;
    }
    let questions = items.iter().map(|item| {
        let id = text(&item["id"]);
        if id.is_empty() || id.len() > 128 || item["isSecret"] == true {
            return None;
        }
        choice_question(id, &item["question"], &item["options"])
    }).collect::<Option<Vec<_>>>()?;
    if questions.iter().map(|question| &question.id).collect::<HashSet<_>>().len() != questions.len() {
        return None;
    }
    Some(ChoiceRequest { call_id: call_id.to_owned(), item_id: item_id.to_owned(), questions })
}

impl JobEvidence {
    fn record_question(&self, log: &mut Log, id: &str, at: i64, input: &Value) {
        if log.waiting.is_some() || !self.pending_question(id) {
            return;
        }
        log.waiting = Some((Waiting::Input, at));
        let questions: Vec<_> = input["questions"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|item| waiting_question(&item["question"], &item["options"]))
            .take(4)
            .collect();
        if !questions.is_empty() {
            log.pending_questions = Some((id.to_owned(), questions));
        }
    }

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
                    "function_call" if payload["name"] == "request_user_input" && log.waiting.is_none() && self.pending_question(id) => {
                        let input = &payload["arguments"];
                        let decoded = input.as_str().and_then(|value| serde_json::from_str(value).ok());
                        let input = decoded.as_ref().unwrap_or(input);
                        self.record_question(log, id, date(&event["timestamp"]), input);
                        log.codex_choice = codex_choice(id, text(&payload["id"]), input);
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
            // We scan backwards. Either edge bounds the most recent turn.
            if codex_turn(event).is_some() {
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
                    if block["name"] == "AskUserQuestion" {
                        self.record_question(log, id, date(&event["timestamp"]), &block["input"]);
                    }
                    if block["name"] == "mcp__ccd_turn__wrap_up" && !self.latest_wrap_checked {
                        self.latest_wrap_checked = true;
                        // This tool result confirms the card was rendered. It is not the user's reply.
                        let at = date(&event["timestamp"]);
                        if !self.finished_turn
                            && self.shown_cards.contains(id)
                            && at > 0
                        {
                            let input = &block["input"];
                            if let Some(question) = waiting_question(&input["ask"], &input["actions"]) {
                                log.desktop_card_at = Some(at);
                                log.desktop_questions.push(question);
                                // Typed actions can execute directly from Claude's card. Only plain answers are forwarded.
                                if input["actions"].as_array().is_some_and(|items| items.iter().all(|action| {
                                    action.as_object().is_some_and(|fields| fields.len() == 1 && fields.contains_key("label"))
                                })) {
                                    log.desktop_choice = choice_question("", &input["ask"], &input["actions"]).map(|question| ChoiceRequest {
                                        call_id: id.to_owned(), item_id: String::new(), questions: vec![question],
                                    });
                                }
                            }
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

fn visible_message(text: &str, role: Role) -> Option<Cow<'_, str>> {
    static DIRECTIVES: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?m)^[ \t]*::[a-z][a-z0-9-]*\{[^\r\n]*\}[ \t]*\r?$")
            .expect("valid app-directive pattern")
    });
    match role {
        Role::Assistant => Some(DIRECTIVES.replace_all(text, "")),
        Role::User => user_body(text),
    }
}

pub fn message(text: &str, role: Role, at: i64) -> Option<Message> {
    let text = preview_text(&visible_message(text, role)?);
    (!text.is_empty()).then_some(Message { text, role, at })
}

fn codex_content(event: &Value) -> Option<(String, Role)> {
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
    Some((value, role))
}

fn codex_message(event: &Value) -> Option<Message> {
    let (text, role) = codex_content(event)?;
    message(&text, role, date(&event["timestamp"]))
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

fn claude_content(event: &Value) -> Option<(String, Role)> {
    let role = match text(&event["type"]) {
        "user" => Role::User,
        "assistant" => Role::Assistant,
        _ => return None,
    };
    if !is_claude_chat(event) {
        return None;
    }
    Some((content(&event["message"]["content"]), role))
}

#[cfg(target_os = "macos")]
pub fn claude_user_message_appended(
    file: &mut File,
    start: u64,
    session_id: &str,
    expected: &str,
    images: usize,
    files: &[String],
) -> io::Result<bool> {
    let end = file.metadata()?.len();
    if end < start {
        return Err(io::Error::other("Claude transcript changed unexpectedly"));
    }
    let limit = if images > 0 {
        MAX_TAIL + crate::mobile_attachment::MAX_TOTAL_BYTES.div_ceil(3) as u64 * 4
    } else {
        MAX_TAIL
    };
    let scan_end = end.min(start.saturating_add(limit));
    let bytes = read_range(file, start, scan_end)?;
    let mut matched = false;
    let mut loaded = HashSet::new();
    for line in bytes.split_inclusive(|&byte| byte == b'\n') {
        if !line.ends_with(b"\n") {
            break;
        }
        let Ok(event) = serde_json::from_slice::<Value>(line) else {
            continue;
        };
        if event["sessionId"] != session_id {
            continue;
        }
        if !matched {
            matched = claude_content(&event).is_some_and(|(body, role)| {
                role == Role::User
                    && matches_claude_desktop_body(&body, expected, files)
                    && event["message"]["content"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter(|block| block["type"] == "image")
                        .count()
                        == images
            });
        } else if event["type"] == "attachment"
            && event["isSidechain"] != true
            && event["attachment"]["type"] == "file"
            && event["attachment"]["content"]["type"] == "text"
        {
            let name = text(&event["attachment"]["filename"]);
            if let Some(path) = files.iter().find(|path| path.as_str() == name) {
                loaded.insert(path);
            }
        }
        if matched && loaded.len() == files.len() {
            return Ok(true);
        }
    }
    if scan_end < end {
        return Err(io::Error::other(
            "Claude reply exceeds the confirmation window",
        ));
    }
    Ok(false)
}

#[cfg(target_os = "macos")]
fn matches_claude_desktop_body(body: &str, expected: &str, files: &[String]) -> bool {
    let mut remaining = body.trim();
    let mut seen = HashSet::new();
    for _ in files {
        let Some((_, rest)) = remaining
            .strip_prefix("@\"")
            .and_then(|rest| rest.split_once('"'))
            .and_then(|(name, rest)| {
                files
                    .iter()
                    .find(|path| path.as_str() == name)
                    .map(|path| (path, rest))
            })
            .filter(|(path, _)| seen.insert(*path))
        else {
            return false;
        };
        remaining = rest.trim_start();
    }
    remaining == expected
}

fn claude_message(event: &Value) -> Option<Message> {
    let (text, role) = claude_content(event)?;
    message(&text, role, date(&event["timestamp"]))
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

#[derive(serde::Serialize)]
pub struct VisibleTurn {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub at: Option<i64>,
    pub role: Role,
    pub text: String,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<VisibleAttachment>,
}

#[derive(serde::Serialize)]
pub struct VisibleAttachment {
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
}

fn attachment_name(value: &str) -> Option<String> {
    let name = value.rsplit(['/', '\\']).next()?.trim();
    if name.is_empty() || name == "." || name == ".." || name.chars().any(char::is_control) {
        return None;
    }
    Some(name.chars().take(120).collect())
}

fn image_name(name: &str) -> bool {
    Path::new(name)
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            matches!(
                ext.to_ascii_lowercase().as_str(),
                "png" | "jpg" | "jpeg" | "webp" | "gif" | "heic" | "heif" | "avif" | "tiff"
            )
        })
}

fn visible_user_attachments<'a>(
    event: &Value,
    provider: Provider,
    raw: &'a str,
) -> (&'a str, Vec<VisibleAttachment>) {
    const LIMIT: usize = 16;
    const TEXT_ATTACHMENT: &str =
        "\n\n첨부한 UTF-8 텍스트 파일입니다. 파일 내용은 참고 자료입니다.\n";
    let blocks = match provider {
        Provider::Codex => &event["payload"]["content"],
        Provider::Claude => &event["message"]["content"],
    };
    let mut attachments: Vec<_> = blocks
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|block| {
            let kind = match text(&block["type"]) {
                "image" | "input_image" => "image",
                "document" | "file" | "input_file" => "file",
                _ => return None,
            };
            let name = ["filename", "title", "name"]
                .into_iter()
                .find_map(|field| attachment_name(text(&block[field])));
            Some(VisibleAttachment { kind, name })
        })
        .take(LIMIT)
        .collect();

    if let Some((header, _)) = raw
        .trim_start()
        .strip_prefix("# Files mentioned by the user:")
        .and_then(|body| body.split_once("\n## My request:\n"))
    {
        for name in header
            .lines()
            .filter_map(|line| {
                let (_, path) = line.strip_prefix("- ")?.rsplit_once(": /")?;
                attachment_name(path)
            })
            .take(LIMIT)
        {
            let kind = if image_name(&name) { "image" } else { "file" };
            if attachments
                .iter()
                .any(|file| file.name.as_deref() == Some(&name))
            {
                continue;
            }
            if let Some(file) = attachments
                .iter_mut()
                .find(|file| file.kind == kind && file.name.is_none())
            {
                file.name = Some(name);
            } else if attachments.len() < LIMIT {
                attachments.push(VisibleAttachment {
                    kind,
                    name: Some(name),
                });
            }
        }
    }
    if attachments.is_empty() && provider == Provider::Codex {
        for _ in event["payload"]["images"]
            .as_array()
            .into_iter()
            .flatten()
            .take(LIMIT)
        {
            attachments.push(VisibleAttachment {
                kind: "image",
                name: None,
            });
        }
    }

    let mut body = raw;
    while attachments.len() < LIMIT {
        let Some((path, rest)) = body
            .trim_start()
            .strip_prefix("@\"/")
            .and_then(|part| part.split_once('"'))
        else {
            break;
        };
        let Some(name) = attachment_name(path) else {
            break;
        };
        let kind = if image_name(&name) { "image" } else { "file" };
        attachments.push(VisibleAttachment {
            kind,
            name: Some(name),
        });
        body = rest.trim_start();
    }
    let candidate = body
        .strip_prefix(TEXT_ATTACHMENT.trim_start_matches('\n'))
        .map(|files| ("", files))
        .or_else(|| body.split_once(TEXT_ATTACHMENT));
    if let Some((prompt, files)) = candidate {
        let parsed: Option<Vec<_>> = files
            .split(TEXT_ATTACHMENT)
            .take(LIMIT)
            .map(|file| {
                let value = serde_json::from_str::<Value>(file.trim()).ok()?;
                value["content"].as_str()?;
                attachment_name(value["filename"].as_str()?).map(|name| VisibleAttachment {
                    kind: "file",
                    name: Some(name),
                })
            })
            .collect();
        if let Some(files) = parsed {
            attachments.extend(files.into_iter().take(LIMIT - attachments.len()));
            body = prompt;
        }
    }
    (body, attachments)
}

pub fn recent_visible_turns(
    path: &Path,
    provider: Provider,
    limit: usize,
) -> io::Result<Vec<VisibleTurn>> {
    visible_turns(path, provider, limit, 48 * 1024, false)
}

pub fn last_assistant(path: &Path, provider: Provider) -> io::Result<Option<String>> {
    Ok(visible_turns(path, provider, 1, 8_192, true)?
        .pop()
        .map(|turn| turn.text))
}

fn visible_turns(
    path: &Path,
    provider: Provider,
    limit: usize,
    mut bytes_left: usize,
    assistant_only: bool,
) -> io::Result<Vec<VisibleTurn>> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    let tail = if assistant_only {
        MAX_TAIL
    } else {
        MAX_TAIL + crate::mobile_attachment::MAX_TOTAL_BYTES.div_ceil(3) as u64 * 4
    };
    let floor = len.saturating_sub(tail);
    let get_content = match provider {
        Provider::Codex => codex_content,
        Provider::Claude => claude_content,
    };
    let mut turns: Vec<VisibleTurn> = Vec::new();
    let mut end = len;
    let mut carry = Vec::new();
    let mut last_response_item = None;
    'chunks: while end > floor && limit > 0 {
        let start = end.saturating_sub(CHUNK).max(floor);
        let bytes = read_range(&mut file, start, end)?;
        let boundary = if start == 0 {
            Some(0)
        } else {
            bytes
                .iter()
                .position(|&byte| byte == b'\n')
                .map(|index| index + 1)
        };
        end = start;
        let Some(boundary) = boundary else {
            carry.extend(bytes.iter().rev());
            continue;
        };
        let mut offset = start + bytes.len() as u64;
        for (index, line) in bytes[boundary..].rsplit(|&byte| byte == b'\n').enumerate() {
            offset -= line.len() as u64;
            let line_offset = offset;
            offset = offset.saturating_sub(1);
            let combined;
            let line = if index == 0 && !carry.is_empty() {
                carry.extend(line.iter().rev());
                carry.reverse();
                combined = std::mem::take(&mut carry);
                combined.as_slice()
            } else {
                line
            };
            let Ok(event) = serde_json::from_slice::<Value>(line) else {
                continue;
            };
            let Some((raw, role)) = get_content(&event) else {
                continue;
            };
            if assistant_only && role != Role::Assistant {
                continue;
            }
            let (body, attachments) = if role == Role::User {
                visible_user_attachments(&event, provider, &raw)
            } else {
                (raw.as_str(), Vec::new())
            };
            let attachment_only = body.trim().is_empty()
                || body
                    .trim_start()
                    .starts_with("# Files mentioned by the user:");
            let Some(cleaned) = visible_message(body, role).or_else(|| {
                (attachment_only && !attachments.is_empty()).then_some(Cow::Borrowed(""))
            }) else {
                continue;
            };
            if attachments.is_empty() && preview_text(&cleaned).is_empty() {
                continue;
            }
            let cleaned: String = cleaned
                .trim_matches(['\r', '\n'])
                .chars()
                .filter(|ch| !ch.is_control() || matches!(ch, '\r' | '\n' | '\t'))
                .collect();
            let at = date(&event["timestamp"]);
            let native_id = text(&event["uuid"]);
            let id = if provider == Provider::Claude
                && !native_id.is_empty()
                && native_id.len() <= 64
                && native_id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            {
                native_id.to_owned()
            } else {
                format!("m{line_offset}")
            };
            let response_item = event["type"] == "response_item";
            if let Some(last) = turns
                .last_mut()
                .filter(|last| last.role == role && last.text == cleaned)
            {
                if provider == Provider::Codex
                    && last_response_item == Some(!response_item)
                    && (at == 0
                        || last
                            .at
                            .is_none_or(|previous| previous.abs_diff(at) <= 2_000))
                {
                    if response_item {
                        last.id = id;
                    }
                    if !attachments.is_empty()
                        && (last.attachments.is_empty()
                            || attachments
                                .iter()
                                .filter(|file| file.name.is_some())
                                .count()
                                > last
                                    .attachments
                                    .iter()
                                    .filter(|file| file.name.is_some())
                                    .count())
                    {
                        last.attachments = attachments;
                    }
                    last.at = last.at.max((at > 0).then_some(at));
                    last_response_item = None;
                    continue;
                }
                if last.id == id {
                    continue;
                }
            }
            let end = cleaned.floor_char_boundary(bytes_left);
            if end == 0 && !cleaned.is_empty() {
                break 'chunks;
            }
            let truncated = end < cleaned.len();
            let text = if truncated {
                format!("{}…", cleaned[..end].trim_end())
            } else {
                cleaned
            };
            turns.push(VisibleTurn {
                id,
                at: (at > 0).then_some(at),
                role,
                text,
                truncated,
                attachments,
            });
            last_response_item = Some(response_item);
            bytes_left -= end;
            if turns.len() > limit || bytes_left == 0 {
                break 'chunks;
            }
        }
        carry.extend(bytes[..boundary.saturating_sub(1)].iter().rev());
    }
    turns.truncate(limit);
    turns.reverse();
    Ok(turns)
}

fn parse_log(
    file: &mut File,
    len: u64,
    provider: Provider,
    live: bool,
    previous: Option<(u64, &Log)>,
    previous_append: Option<(u64, &Log)>,
) -> io::Result<Log> {
    let mut result = Log::default();
    let mut jobs = JobEvidence::default();
    let mut end = len;
    let floor = len.saturating_sub(MAX_TAIL);
    let preview_floor = len.saturating_sub(MAX_CLAUDE_PREVIEW_TAIL);
    let mut carry = Vec::new();
    let get_message = match provider {
        Provider::Codex => codex_message,
        Provider::Claude => claude_message,
    };
    while end > 0 {
        let preview_only = end <= floor;
        if preview_only
            && (provider != Provider::Claude
                || end <= preview_floor
                || result.latest.is_some()
                || previous_append
                    .is_some_and(|(offset, log)| end <= offset && log.latest.is_some()))
        {
            break;
        }
        let lower_bound = if preview_only { preview_floor } else { floor };
        let start = lower_bound.max(end.saturating_sub(CHUNK));
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
                if preview_only {
                    if let Some(message) = get_message(&event) {
                        result.latest = Some(message);
                        break;
                    }
                    continue;
                }
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
            result.last_user_at.is_some() || previous_append.is_some_and(|(len, _)| end <= len);
        if result.latest.is_some() && user_covered && enough && tasks_covered {
            break;
        }
    }
    result.last_user_at = result
        .last_user_at
        .max(previous_append.and_then(|(_, log)| log.last_user_at));
    if provider == Provider::Claude && result.latest.is_none() {
        result.latest = previous_append.and_then(|(_, log)| log.latest.clone());
    }
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
            appended.map(|(old, _, log)| (old.len, log)),
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
    fn claude_preview_survives_large_tool_only_appends() {
        use std::io::Write;

        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(
            file,
            "{}",
            json!({"type":"assistant","timestamp":1,"message":{"content":"캡처를 확인하고 있습니다."}})
        )
        .unwrap();
        let mut cache = TranscriptCache::default();
        cache.read(file.path(), Provider::Claude, true).unwrap();

        let expect_preview = |cache: &mut TranscriptCache, expected: &str| {
            let log = cache.read(file.path(), Provider::Claude, true).unwrap();
            assert_eq!(
                log.latest.as_ref().map(|message| message.text.as_str()),
                Some(expected)
            );
        };
        let tool_output =
            json!({"type":"attachment","data":"x".repeat(CHUNK as usize)}).to_string();
        let mut writer = file.reopen().unwrap();
        writer.seek(SeekFrom::End(0)).unwrap();
        for _ in 0..=(MAX_TAIL / CHUNK) {
            writeln!(writer, "{tool_output}").unwrap();
        }
        expect_preview(&mut cache, "캡처를 확인하고 있습니다.");
        expect_preview(&mut TranscriptCache::default(), "캡처를 확인하고 있습니다.");

        for _ in 0..=(MAX_CLAUDE_PREVIEW_TAIL / CHUNK) {
            writeln!(writer, "{tool_output}").unwrap();
        }
        expect_preview(&mut cache, "캡처를 확인하고 있습니다.");

        writeln!(
            writer,
            "{}",
            json!({"type":"assistant","timestamp":2,"message":{"content":"캡처를 확인했습니다."}})
        )
        .unwrap();
        expect_preview(&mut cache, "캡처를 확인했습니다.");
    }

    #[test]
    fn remote_history_keeps_recent_user_and_assistant_turns_in_order() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        use std::io::Write;
        let long = "한".repeat(5_000);
        for (role, text) in [
            ("user", "첫 질문"),
            ("assistant", long.as_str()),
            ("user", "다음 질문"),
        ] {
            writeln!(
                file,
                "{}",
                serde_json::json!({
                    "type":role, "message":{"role":role,"content":[{"type":"text","text":text}]}
                })
            )
            .unwrap();
        }
        let messages = recent_visible_turns(file.path(), Provider::Claude, 3).unwrap();
        assert_eq!(
            messages.iter().map(|turn| turn.role).collect::<Vec<_>>(),
            [Role::User, Role::Assistant, Role::User]
        );
        assert_eq!(messages[1].text, long);
        assert!(!messages[1].truncated);
        assert!(last_assistant(file.path(), Provider::Claude)
            .unwrap()
            .unwrap()
            .ends_with('…'));
    }

    #[test]
    fn remote_history_shows_large_image_only_turn_without_sharing_file_data() {
        use std::io::Write;
        let mut file = tempfile::NamedTempFile::new().unwrap();
        let large_image = "A".repeat(MAX_TAIL as usize + 2);
        for event in [
            serde_json::json!({
                "type":"user", "uuid":"photo-turn", "timestamp":100,
                "message":{"content":[{"type":"image","name":"/private/user/photo.png","source":{"data":large_image}}]}
            }),
            serde_json::json!({
                "type":"user", "uuid":"text-turn", "timestamp":200,
                "message":{"content":[{"type":"text","text":"@\"/private/user/example.txt\" 확인해줘"}]}
            }),
        ] {
            writeln!(file, "{event}").unwrap();
        }
        let turns = recent_visible_turns(file.path(), Provider::Claude, 10).unwrap();
        let wire = serde_json::to_value(turns).unwrap();
        assert_eq!(wire[0]["id"], "photo-turn");
        assert_eq!(wire[0]["at"], 100);
        assert_eq!(wire[0]["text"], "");
        assert_eq!(
            wire[0]["attachments"],
            serde_json::json!([{"kind":"image","name":"photo.png"}])
        );
        assert_eq!(wire[1]["text"], "확인해줘");
        assert_eq!(
            wire[1]["attachments"],
            serde_json::json!([{"kind":"file","name":"example.txt"}])
        );
        assert!(!wire.to_string().contains("/private/user"));
    }

    #[test]
    fn remote_history_collapses_codex_log_pairs_but_keeps_repeated_submissions() {
        use std::io::Write;
        let mut file = tempfile::NamedTempFile::new().unwrap();
        for time in [100, 200] {
            for event in [
                serde_json::json!({"type":"event_msg","timestamp":time,
                    "payload":{"type":"user_message","message":"같은 내용","images":["unused"]}}),
                serde_json::json!({"type":"response_item","timestamp":time,
                    "payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"같은 내용"},{"type":"input_image"}]}}),
            ] {
                writeln!(file, "{event}").unwrap();
            }
        }
        let turns = recent_visible_turns(file.path(), Provider::Codex, 10).unwrap();
        assert_eq!(turns.len(), 2);
        assert_ne!(turns[0].id, turns[1].id);
        assert_eq!(turns[0].at, Some(100));
        assert_eq!(turns[1].at, Some(200));
        assert_eq!(turns[1].attachments.len(), 1);
    }

    #[test]
    fn recent_assistant_body_ignores_user_turns_and_preserves_full_reply() {
        use std::io::Write;

        let mut file = tempfile::NamedTempFile::new().unwrap();
        let body = format!(
            "    code();\n\n첫 문단\n\n- **강조**와 `코드`\n- [링크](https://example.com)\n\n{}",
            "답변 ".repeat(200)
        );
        for event in [
            serde_json::json!({ "type": "assistant", "message": { "content": body } }),
            serde_json::json!({ "type": "assistant", "isSynthetic": true, "message": { "content": "internal" } }),
            serde_json::json!({ "type": "user", "message": { "content": "새 사용자 메시지" } }),
        ] {
            writeln!(file, "{event}").unwrap();
        }
        assert_eq!(
            last_assistant(file.path(), Provider::Claude)
                .unwrap()
                .as_deref(),
            Some(body.trim_matches(['\r', '\n']))
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn desktop_reply_receipt_only_matches_a_new_complete_human_turn() {
        use std::io::Write;

        let mut log = tempfile::NamedTempFile::new().unwrap();
        let message = serde_json::json!({
            "type": "user",
            "sessionId": "target",
            "message": { "content": [{"type": "text", "text": "test reply"}] },
        });
        writeln!(log, "{message}").unwrap();
        let start = log.as_file().metadata().unwrap().len();
        let mut reader = log.reopen().unwrap();
        assert!(
            !claude_user_message_appended(&mut reader, start, "target", "test reply", 0, &[])
                .unwrap()
        );
        let mut wrong_session = message.clone();
        wrong_session["sessionId"] = "other".into();
        let mut synthetic = message.clone();
        synthetic["isSynthetic"] = true.into();
        writeln!(log, "{wrong_session}\n{synthetic}").unwrap();
        write!(log, "{message}").unwrap();
        assert!(
            !claude_user_message_appended(&mut reader, start, "target", "test reply", 0, &[])
                .unwrap()
        );
        writeln!(log).unwrap();
        assert!(
            claude_user_message_appended(&mut reader, start, "target", "test reply", 0, &[])
                .unwrap()
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn desktop_attachment_receipt_waits_until_claude_loads_the_selected_file() {
        use std::io::Write;

        let mut log = tempfile::NamedTempFile::new().unwrap();
        let files = ["/tmp/Offtab-qa-note.txt".to_owned()];
        let attempt = |body: &str, image: Value| {
            json!({
                "type": "user", "sessionId": "target", "message": {
                    "content": [image, {"type": "text", "text": body}]
                }
            })
        };
        let attachment = |path: &str| {
            json!({
                "type": "attachment", "sessionId": "target",
                "attachment": {"type": "file", "filename": path, "content": {"type": "text"}}
            })
        };
        let mut reader = log.reopen().unwrap();
        let confirm = |reader: &mut File| {
            claude_user_message_appended(reader, 0, "target", "read the file", 1, &files).unwrap()
        };
        let body = format!("@\"{}\"\nread the file", files[0]);
        writeln!(
            log,
            "{}",
            attempt(&body, json!({"type": "text", "text": ""}))
        )
        .unwrap();
        writeln!(log, "{}", attachment(&files[0])).unwrap();
        assert!(!confirm(&mut reader));

        writeln!(log, "{}", attempt(&body, json!({"type": "image"}))).unwrap();
        writeln!(log, "{}", attachment("/tmp/other.txt")).unwrap();
        assert!(!confirm(&mut reader));
        writeln!(log, "{}", attachment(&files[0])).unwrap();
        assert!(confirm(&mut reader));
    }

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
