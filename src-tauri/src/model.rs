use serde::{Deserialize, Serialize};

pub const WEEK_MS: i64 = 7 * 86_400_000;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Codex,
    Claude,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    Compact,
    Full,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Waiting {
    Input,
    Approval,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    pub id: String,
    pub provider: Provider,
    pub title: String,
    pub preview: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<Role>,
    pub cwd: String,
    pub updated_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub waiting: Option<Waiting>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub waiting_at: Option<i64>,
    pub background: usize,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub recently_active: bool,
    pub activity_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_user_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native_id: Option<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub has_transcript: bool,
    #[serde(skip)]
    pub terminal: Option<crate::targets::TerminalTab>,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceStatus {
    Ready,
    Missing,
    Partial,
    Error,
}

#[derive(Debug, Serialize)]
pub struct Source {
    pub provider: Provider,
    pub status: SourceStatus,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub sessions: Vec<Session>,
    pub removed: Vec<SessionId>,
    pub sources: Vec<Source>,
    pub refreshed_at: i64,
}

#[derive(Debug, Serialize)]
pub struct SessionId {
    pub provider: Provider,
    pub id: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Watched {
    pub provider: Provider,
    pub id: String,
    pub native_id: Option<String>,
    #[serde(default)]
    pub has_transcript: bool,
    pub hidden_at: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct ListScope {
    pub mode: Mode,
    pub watched: Vec<Watched>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum OpenResult {
    Opened,
    Unavailable,
    Failed,
    TerminalPermission,
    TerminalUnavailable,
}

pub fn is_uuid(id: &str) -> bool {
    id.len() == 36
        && id.bytes().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => byte == b'-',
            _ => byte.is_ascii_hexdigit(),
        })
}
