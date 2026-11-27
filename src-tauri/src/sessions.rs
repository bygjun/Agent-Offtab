use crate::{
    activity::{self, Activity},
    hooks::{self, Signal},
    model::{
        is_uuid, ListScope, Mode, Provider, Role, Session, SessionId, Snapshot, Source,
        SourceStatus, Waiting, Watched, WEEK_MS,
    },
    targets::{TerminalEvidence, TerminalTab},
    transcript::{
        compact, content, date, epoch, first_text, message, text, user_body, Message, Stamp,
        TranscriptCache, Turn,
    },
};
use rusqlite::{
    params_from_iter, types::Value as SqlValue, Connection, OpenFlags, OptionalExtension,
};
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet},
    fs, io,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

#[cfg(test)]
mod tests;

type ReadResult = Result<(bool, Vec<Session>, HashSet<String>), Box<dyn std::error::Error>>;
const RECENT_WORK_MS: i64 = 15_000;

#[derive(Default)]
pub struct Collector {
    transcripts: TranscriptCache,
    desktop_cache: HashMap<PathBuf, (Stamp, DesktopInfo)>,
    first_scan_at: Option<i64>,
}

struct ScanScope<'a> {
    mode: Mode,
    history_since: i64,
    work_since: i64,
    watched: HashMap<&'a str, &'a Watched>,
    signals: &'a HashMap<String, Signal>,
    activity: &'a Activity,
}

#[derive(Clone, Default)]
struct DesktopInfo {
    id: String,
    native_id: String,
    parent_native_id: Option<String>,
    title: String,
    cwd: String,
    at: i64,
    archived: bool,
    waiting_at: Option<i64>,
    latest_user_frame: i64,
    turn_end: i64,
    has_mounted_wrap_up: bool,
    prior_ids: Vec<String>,
}

impl DesktopInfo {
    fn waiting_at(&self, run: Option<&ClaudeRun>) -> Option<i64> {
        self.waiting_at
            .filter(|&at| !self.archived && !run.is_some_and(|run| run.active && run.at > at))
    }

    fn confirmed_card_at(
        &self,
        transcript_at: Option<i64>,
        run: Option<&ClaudeRun>,
    ) -> Option<i64> {
        let card_at = transcript_at?;
        (self.has_mounted_wrap_up
            && !self.archived
            && card_at > self.latest_user_frame
            && card_at <= self.turn_end
            && !run.is_some_and(|run| run.active && run.at > self.turn_end))
        .then_some(self.turn_end)
    }
}

#[derive(Default)]
struct DesktopCatalog {
    found: bool,
    sessions: HashMap<String, DesktopInfo>,
    prior_ids: HashSet<String>,
}

struct ClaudeRun {
    name: String,
    cwd: String,
    at: i64,
    active: bool,
    live_pid: Option<u32>,
    metadata: PathBuf,
    desktop: bool,
}

struct CodexRow {
    id: String,
    title: String,
    cwd: String,
    path: PathBuf,
    at: i64,
}

fn entries(path: &Path) -> io::Result<Vec<fs::DirEntry>> {
    match fs::read_dir(path) {
        Ok(dir) => dir.collect(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error),
    }
}

fn partial_entries(path: &Path, partial: &mut bool) -> Vec<fs::DirEntry> {
    entries(path).unwrap_or_else(|_| {
        *partial = true;
        Vec::new()
    })
}

fn is_dir(entry: &fs::DirEntry, partial: &mut bool) -> bool {
    match entry.file_type() {
        Ok(kind) => kind.is_dir(),
        Err(error) => {
            report_io(&error, partial);
            false
        }
    }
}

fn report_io(error: &io::Error, partial: &mut bool) {
    if error.kind() != io::ErrorKind::NotFound {
        *partial = true;
    }
}

fn json_file(path: &Path) -> io::Result<Value> {
    serde_json::from_slice(&fs::read(path)?).map_err(io::Error::other)
}

fn source(
    provider: Provider,
    partial: bool,
    result: ReadResult,
) -> (Source, Vec<Session>, impl Iterator<Item = SessionId>) {
    let (status, sessions, removed) = match result {
        Ok((true, rows, removed)) if partial => (SourceStatus::Partial, rows, removed),
        Ok((true, rows, removed)) => (SourceStatus::Ready, rows, removed),
        Ok((false, rows, removed)) if !partial => (SourceStatus::Missing, rows, removed),
        _ => (SourceStatus::Error, Vec::new(), HashSet::new()),
    };
    (
        Source { provider, status },
        sessions,
        removed
            .into_iter()
            .map(move |id| SessionId { provider, id }),
    )
}

fn versioned(entries: &[fs::DirEntry], prefix: &str) -> Option<PathBuf> {
    entries
        .iter()
        .filter_map(|entry| {
            let name = entry.file_name();
            let version = name
                .to_str()?
                .strip_prefix(prefix)?
                .strip_suffix(".sqlite")?
                .parse::<u32>()
                .ok()?;
            Some((version, entry.path()))
        })
        .max_by_key(|(version, _)| *version)
        .map(|(_, path)| path)
}

fn open_db(path: &Path) -> rusqlite::Result<Connection> {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    connection.busy_timeout(Duration::from_millis(150))?;
    Ok(connection)
}

fn projected_message(history: &Connection, id: &str) -> rusqlite::Result<Option<Message>> {
    let mut statement = history.prepare_cached(
        "SELECT item_json, created_at_ms FROM thread_items WHERE thread_id = ?
         AND item_type IN ('userMessage', 'agentMessage') ORDER BY rollout_ordinal DESC LIMIT 12",
    )?;
    let mut rows = statement.query([id])?;
    while let Some(row) = rows.next()? {
        let raw: String = row.get(0)?;
        let Ok(payload) = serde_json::from_str::<Value>(&raw) else {
            continue;
        };
        let (body, role) = if payload["type"] == "agentMessage" {
            (text(&payload["text"]).to_owned(), Role::Assistant)
        } else {
            (content(&payload["content"]), Role::User)
        };
        if let Some(message) = message(&body, role, row.get(1)?) {
            return Ok(Some(message));
        }
    }
    Ok(None)
}

fn projected_turn(history: &Connection, id: &str) -> rusqlite::Result<Option<Turn>> {
    history.prepare_cached(
        "SELECT status, started_at FROM thread_turns WHERE thread_id = ? ORDER BY rollout_ordinal DESC LIMIT 1"
    )?.query_row([id], |row| {
        let status: String = row.get(0)?;
        let at = row.get::<_, Option<i64>>(1)?.unwrap_or(0);
        Ok(Turn { active: status == "inProgress", at: if at < 100_000_000_000 { at * 1000 } else { at } })
    }).optional()
}

fn projected_user_at(history: &Connection, id: &str, since: i64) -> rusqlite::Result<Option<i64>> {
    let mut statement = history.prepare_cached(
        "SELECT item_json, created_at_ms FROM thread_items WHERE thread_id = ?
         AND item_type = 'userMessage' AND created_at_ms > ? ORDER BY rollout_ordinal DESC",
    )?;
    let mut rows = statement.query(rusqlite::params![id, since])?;
    while let Some(row) = rows.next()? {
        let raw: String = row.get(0)?;
        let Ok(payload) = serde_json::from_str::<Value>(&raw) else {
            continue;
        };
        let at = row.get(1)?;
        let input = &payload["content"];
        if user_body(&content(input)).is_some()
            || input
                .as_array()
                .into_iter()
                .flatten()
                .any(|part| matches!(text(&part["type"]), "image" | "localImage"))
        {
            return Ok(Some(at));
        }
    }
    Ok(None)
}

fn codex_rows(
    db: &Connection,
    scope: &ScanScope<'_>,
    open_logs: Option<&HashSet<PathBuf>>,
    parents: &HashMap<String, HashSet<String>>,
) -> rusqlite::Result<Vec<CodexRow>> {
    let columns: HashSet<String> = db
        .prepare("PRAGMA table_info(threads)")?
        .query_map([], |row| row.get(1))?
        .collect::<rusqlite::Result<_>>()?;
    if !columns.contains("id") {
        return Err(rusqlite::Error::InvalidQuery);
    }
    let update = if columns.contains("updated_at_ms") {
        "COALESCE(updated_at_ms, updated_at * 1000)"
    } else {
        "updated_at * 1000"
    };
    let name = if columns.contains("name") {
        "COALESCE(NULLIF(name, ''), title)"
    } else {
        "title"
    };
    let parent = if columns.contains("attached_thread_parent_id") {
        "AND attached_thread_parent_id IS NULL"
    } else {
        ""
    };
    let since = match scope.mode {
        Mode::Compact => scope.work_since,
        Mode::Full => scope.history_since,
    };
    let mut clauses = vec![format!("(updated_at >= ? AND {update} >= ?)")];
    let mut values = vec![SqlValue::Integer(since / 1000), SqlValue::Integer(since)];
    let paths: Vec<_> = open_logs
        .into_iter()
        .flatten()
        .filter_map(|path| path.to_str())
        .collect();
    for (column, matches) in [
        ("rollout_path", paths),
        (
            "id",
            scope
                .watched
                .keys()
                .copied()
                .chain(parents.keys().map(String::as_str))
                .collect(),
        ),
    ] {
        if !matches.is_empty() {
            let placeholders = vec!["?"; matches.len()].join(",");
            clauses.push(format!("{column} IN ({placeholders})"));
            values.extend(
                matches
                    .into_iter()
                    .map(|value| SqlValue::Text(value.to_owned())),
            );
        }
    }
    let candidates = clauses.join(" OR ");
    db.prepare(&format!(
        "SELECT id, {name}, cwd, rollout_path, {update} FROM threads
         WHERE archived = 0 AND ({candidates}) AND source NOT LIKE '%\"subagent\"%' {parent}"
    ))?
    .query_map(params_from_iter(values), |row| {
        Ok(CodexRow {
            id: row.get(0)?,
            title: row.get(1)?,
            cwd: row.get(2)?,
            path: PathBuf::from(row.get::<_, String>(3)?),
            at: row.get(4)?,
        })
    })?
    .collect()
}

fn codex_children(
    db: &Connection,
    active_logs: Option<&HashSet<PathBuf>>,
    cache: &mut TranscriptCache,
) -> rusqlite::Result<HashMap<String, HashSet<String>>> {
    let mut parents = HashMap::<String, HashSet<String>>::new();
    let paths: Vec<_> = active_logs
        .into_iter()
        .flatten()
        .filter_map(|path| path.to_str())
        .collect();
    if paths.is_empty() {
        return Ok(parents);
    }
    let attached = db
        .prepare("PRAGMA table_info(threads)")?
        .query_map([], |row| row.get::<_, String>(1))?
        .filter_map(Result::ok)
        .any(|column| column == "attached_thread_parent_id");
    let parent_col = if attached {
        "attached_thread_parent_id"
    } else {
        "NULL"
    };
    let params = vec!["?"; paths.len()].join(",");
    let sql = format!("SELECT id, rollout_path, source, {parent_col} FROM threads WHERE archived = 0 AND rollout_path IN ({params})");
    let mut statement = db.prepare(&sql)?;
    let mut rows = statement.query(params_from_iter(paths))?;
    while let Some(row) = rows.next()? {
        let source: String = row.get(2)?;
        let explicit: Option<String> = row.get(3)?;
        let parent = explicit.or_else(|| {
            if !source.contains("\"subagent\"") {
                return None;
            }
            let data: Value = serde_json::from_str(&source).ok()?;
            data["subagent"]["thread_spawn"]["parent_thread_id"]
                .as_str()
                .map(str::to_owned)
        });
        let Some(parent) = parent.filter(|id| is_uuid(id)) else {
            continue;
        };
        let path = PathBuf::from(row.get::<_, String>(1)?);
        if cache
            .read(&path, Provider::Codex, true)
            .is_ok_and(|log| log.turn.is_some_and(|turn| turn.active))
        {
            parents.entry(parent).or_default().insert(row.get(0)?);
        }
    }
    Ok(parents)
}

impl Collector {
    fn codex(
        &mut self,
        root: &Path,
        scope: &ScanScope<'_>,
        open_logs: Option<&HashSet<PathBuf>>,
        partial: &mut bool,
    ) -> ReadResult {
        let files = entries(root)?;
        let Some(path) = versioned(&files, "state_") else {
            return Ok((false, Vec::new(), HashSet::new()));
        };
        *partial |= open_logs.is_none();
        let db = open_db(&path)?;
        let parents = codex_children(&db, open_logs, &mut self.transcripts)?;
        let rows = codex_rows(&db, scope, open_logs, &parents)?;
        // Watched IDs bypass the time filter, so their absence here is definitive.
        let mut removed: HashSet<_> = scope.watched.keys().map(|&id| id.to_owned()).collect();
        for row in &rows {
            removed.remove(&row.id);
        }
        if rows.is_empty() {
            return Ok((true, Vec::new(), removed));
        }
        let history = versioned(&files, "thread_history_").and_then(|path| match open_db(&path) {
            Ok(db) => Some(db),
            Err(_) => {
                *partial = true;
                None
            }
        });
        let mut sessions = Vec::new();
        for row in rows {
            let opened = open_logs.map(|paths| paths.contains(&row.path));
            let log = if row.path.is_absolute() {
                match self
                    .transcripts
                    .read(&row.path, Provider::Codex, opened == Some(true))
                {
                    Ok(log) => Some(log),
                    Err(error) => {
                        report_io(&error, partial);
                        None
                    }
                }
            } else {
                None
            };
            let mut turn = log.as_ref().and_then(|log| log.turn);
            if opened == Some(true) && turn.is_none() {
                if let Some(history) = &history {
                    turn = projected_turn(history, &row.id).unwrap_or_else(|_| {
                        *partial = true;
                        None
                    });
                }
                *partial |= turn.is_none();
            }
            let active = match opened {
                Some(false) => Some(false),
                Some(true) => turn.map(|turn| turn.active),
                None => turn.filter(|turn| !turn.active).map(|_| false),
            };
            let observed = scope.signals.get(&row.id).filter(|_| opened == Some(true));
            let active_turn = turn.filter(|turn| turn.active).map_or(0, |turn| turn.at);
            let observed_waiting = observed.and_then(|signal| {
                (active == Some(true) && signal.waiting_at >= active_turn)
                    .then_some(signal.waiting)
                    .flatten()
                    .map(|waiting| (waiting, signal.waiting_at))
            });
            let waiting = if active == Some(true) {
                observed_waiting.or_else(|| log.as_ref().and_then(|log| log.waiting))
            } else {
                None
            };
            let background = parents
                .get(&row.id)
                .into_iter()
                .flatten()
                .map(String::as_str)
                .chain(
                    observed
                        .into_iter()
                        .flat_map(|signal| signal.children.keys().map(String::as_str)),
                )
                .chain(
                    log.as_ref()
                        .filter(|_| opened == Some(true))
                        .into_iter()
                        .flat_map(|log| log.background.iter().map(String::as_str)),
                )
                .filter(|id| {
                    observed.is_none_or(|signal| !signal.stopped_children.contains_key(*id))
                })
                .collect::<HashSet<_>>()
                .len();
            let mut last_user_at = log.as_ref().and_then(|log| log.last_user_at);
            if let (Some(history), Some(since)) = (
                history.as_ref(),
                scope
                    .watched
                    .get(row.id.as_str())
                    .and_then(|entry| entry.hidden_at),
            ) {
                if last_user_at.is_none_or(|at| at <= since) && row.at.saturating_add(999) > since {
                    last_user_at = last_user_at.max(
                        projected_user_at(history, &row.id, since).unwrap_or_else(|_| {
                            *partial = true;
                            None
                        }),
                    );
                }
            }
            let has_transcript = log.is_some();
            let latest = log.and_then(|log| log.latest).or_else(|| {
                history.as_ref().and_then(|history| {
                    projected_message(history, &row.id).unwrap_or_else(|_| {
                        *partial = true;
                        None
                    })
                })
            });
            let message_at = latest.as_ref().map_or(0, |message| message.at);
            let recently_active = message_at >= scope.work_since
                || turn.is_some_and(|turn| turn.at >= scope.work_since);
            if scope.mode == Mode::Compact
                && active != Some(true)
                && !recently_active
                && waiting.is_none()
                && background == 0
                && !scope.watched.contains_key(row.id.as_str())
            {
                continue;
            }
            sessions.push(Session {
                id: row.id,
                provider: Provider::Codex,
                title: compact(&row.title, 150),
                preview: latest
                    .as_ref()
                    .map_or_else(String::new, |message| message.text.clone()),
                role: latest.map(|message| message.role),
                cwd: row.cwd,
                updated_at: row.at.max(message_at),
                active,
                waiting: waiting.map(|(waiting, _)| waiting),
                waiting_at: waiting.map(|(_, at)| at),
                background,
                recently_active,
                activity_at: message_at
                    .max(turn.filter(|turn| turn.active).map_or(0, |turn| turn.at)),
                last_user_at,
                native_id: None,
                parent_native_id: None,
                context_only: false,
                has_transcript,
                read: None,
                cli_alive: opened == Some(true),
                desktop_worker_alive: false,
                transcript_path: has_transcript.then(|| row.path.clone()),
                terminal: scope
                    .activity
                    .codex_terminals
                    .get(&row.path)
                    .map(|process| TerminalTab {
                        process: *process,
                        evidence: TerminalEvidence::Codex(row.path),
                    }),
            });
        }
        Ok((true, sessions, removed))
    }

    fn desktop_info(&mut self, path: &Path) -> io::Result<DesktopInfo> {
        let stamp = Stamp::of(&fs::metadata(path)?);
        if let Some((cached, data)) = self.desktop_cache.get(path) {
            if *cached == stamp {
                return Ok(data.clone());
            }
        }
        let data = json_file(path)?;
        // Desktop can end the CLI turn while its wrap-up card still waits for a choice.
        let turn_end = date(&data["lastTurnEdgeAt"]);
        let user_frame = date(&data["latestUserFrameAt"]);
        let has_answer_card = !text(&data["turnWrapUp"]["needsUser"]).trim().is_empty();
        let spawned_from = &data["spawnedFrom"];
        let parent_native_id = text(&spawned_from["sessionId"]);
        let info = DesktopInfo {
            id: text(&data["cliSessionId"]).to_owned(),
            native_id: text(&data["sessionId"]).to_owned(),
            parent_native_id: (spawned_from["sideSession"] == true
                && parent_native_id != text(&data["sessionId"])
                && parent_native_id.strip_prefix("local_").is_some_and(is_uuid))
            .then(|| parent_native_id.to_owned()),
            cwd: text(&data["cwd"]).to_owned(),
            title: text(&data["title"]).to_owned(),
            at: date(&data["lastActivityAt"]).max(date(&data["createdAt"])),
            archived: data["isArchived"].as_bool().unwrap_or(false),
            waiting_at: (has_answer_card && turn_end > 0 && turn_end > user_frame)
                .then_some(turn_end),
            latest_user_frame: user_frame,
            turn_end,
            has_mounted_wrap_up: data["turnBoxMounted"] == true && data["turnWrapUp"].is_object(),
            prior_ids: data["priorCliSessionIds"]
                .as_array()
                .into_iter()
                .flatten()
                .map(text)
                .map(str::to_owned)
                .collect(),
        };
        self.desktop_cache
            .insert(path.to_owned(), (stamp, info.clone()));
        Ok(info)
    }

    fn desktop_catalog(&mut self, root: &Path, partial: &mut bool) -> DesktopCatalog {
        let mut catalog = DesktopCatalog::default();
        let accounts = partial_entries(root, partial);
        catalog.found = !accounts.is_empty();
        for account in accounts {
            if !is_dir(&account, partial) {
                continue;
            }
            for workspace in partial_entries(&account.path(), partial) {
                if !is_dir(&workspace, partial) {
                    continue;
                }
                for entry in partial_entries(&workspace.path(), partial) {
                    let name = entry.file_name();
                    if !name
                        .to_str()
                        .is_some_and(|name| name.starts_with("local_") && name.ends_with(".json"))
                    {
                        continue;
                    }
                    let info = match self.desktop_info(&entry.path()) {
                        Ok(info) => info,
                        Err(error) => {
                            report_io(&error, partial);
                            continue;
                        }
                    };
                    if info.id.is_empty() {
                        continue;
                    }
                    catalog.prior_ids.extend(info.prior_ids.iter().cloned());
                    if catalog
                        .sessions
                        .get(&info.id)
                        .is_none_or(|previous| info.at >= previous.at)
                    {
                        catalog.sessions.insert(info.id.clone(), info);
                    }
                }
            }
        }
        catalog
    }

    fn claude(
        &mut self,
        root: &Path,
        desktop: &Path,
        scope: &ScanScope<'_>,
        processes: Option<&HashMap<u32, i64>>,
        partial: &mut bool,
    ) -> ReadResult {
        let projects = partial_entries(&root.join("projects"), partial);
        let runs = cli_runs(&root.join("sessions"), processes, partial);
        let present = !projects.is_empty() || !runs.is_empty();
        let mut sessions = HashMap::<String, Session>::new();
        let mut catalog_partial = false;
        let meta = self.desktop_catalog(desktop, &mut catalog_partial);
        *partial |= catalog_partial;
        let native: HashMap<_, _> = meta
            .sessions
            .values()
            .filter(|info| !info.native_id.is_empty())
            .map(|info| (info.native_id.as_str(), info))
            .collect();
        let wanted: HashSet<_> = scope
            .watched
            .keys()
            .copied()
            .chain(scope.watched.values().filter_map(|item| {
                let info = native.get(item.native_id.as_deref()?)?;
                Some(info.id.as_str())
            }))
            .chain(scope.activity.claude_shells.keys().map(String::as_str))
            .chain(
                meta.sessions
                    .iter()
                    .filter_map(|(id, info)| info.waiting_at(runs.get(id)).map(|_| id.as_str())),
            )
            .chain(
                runs.iter()
                    .filter(|(_, run)| run.live_pid.is_some())
                    .map(|(id, _)| id.as_str()),
            )
            .collect();
        let present = present || meta.found;
        *partial |= present && processes.is_none();
        let mut removed = HashSet::new();
        for (&id, item) in &scope.watched {
            let original = item
                .native_id
                .as_deref()
                .and_then(|id| native.get(id).copied());
            if original
                .or_else(|| meta.sessions.get(id))
                .is_some_and(|info| info.archived)
                || (original.is_none() && meta.prior_ids.contains(id))
                || (item.native_id.is_some()
                    && original.is_none()
                    && !catalog_partial
                    && desktop.is_dir())
            {
                removed.insert(id.to_owned());
            }
        }
        let mut seen_transcripts = HashSet::new();
        let parent_ids: HashSet<_> = meta
            .sessions
            .values()
            .filter_map(|info| native.get(info.parent_native_id.as_deref()?))
            .map(|info| info.id.as_str())
            .collect();
        let mut parent_transcripts: HashMap<&str, Vec<PathBuf>> = HashMap::new();

        for project in projects {
            if !is_dir(&project, partial) {
                continue;
            }
            for entry in partial_entries(&project.path(), partial) {
                let name = entry.file_name();
                let Some(id) = name
                    .to_str()
                    .and_then(|name| name.strip_suffix(".jsonl"))
                    .filter(|id| is_uuid(id))
                else {
                    continue;
                };
                let info = meta.sessions.get(id);
                let run = runs.get(id);
                let original = scope
                    .watched
                    .get(id)
                    .and_then(|item| item.native_id.as_ref());
                if info.is_some_and(|info| info.archived)
                    || (info.is_none()
                        && (original.is_some()
                            || meta.prior_ids.contains(id)
                            || run.is_some_and(|run| run.desktop)))
                {
                    continue;
                }
                if scope.watched.contains_key(id) {
                    seen_transcripts.insert(id.to_owned());
                }
                let path = entry.path();
                let stat = match fs::metadata(&path) {
                    Ok(stat) if stat.is_file() => stat,
                    Ok(_) => continue,
                    Err(error) => {
                        report_io(&error, partial);
                        continue;
                    }
                };
                if let Some(info) = info.filter(|info| parent_ids.contains(info.id.as_str())) {
                    parent_transcripts
                        .entry(info.id.as_str())
                        .or_default()
                        .push(path.clone());
                }
                let modified = stat.modified().map_or(0, epoch);
                let boosted = info
                    .map_or(0, |info| info.at)
                    .max(run.filter(|run| run.active).map_or(0, |run| run.at));
                let threshold = match scope.mode {
                    Mode::Compact => scope.work_since,
                    Mode::Full => scope.history_since,
                };
                if !wanted.contains(id) && modified.max(boosted) < threshold {
                    continue;
                }
                let log = match self.transcripts.read(
                    &path,
                    Provider::Claude,
                    run.is_some_and(|run| run.live_pid.is_some()),
                ) {
                    Ok(log) => log,
                    Err(error) => {
                        report_io(&error, partial);
                        continue;
                    }
                };
                if info.is_none() && log.desktop {
                    if !catalog_partial && desktop.is_dir() && scope.watched.contains_key(id) {
                        removed.insert(id.to_owned());
                    }
                    continue;
                }
                if log.latest.is_none()
                    && log.first_user.is_empty()
                    && info.is_none()
                    && !wanted.contains(id)
                {
                    continue;
                }
                let latest = log.latest.clone();
                let message_at = latest.as_ref().map_or(0, |message| message.at);
                let card_at =
                    info.and_then(|info| info.confirmed_card_at(log.desktop_card_at, run));
                let at = boosted.max(if message_at == 0 {
                    modified
                } else {
                    message_at
                });
                if (!wanted.contains(id)
                    && card_at.is_none()
                    && (at < threshold
                        || (scope.mode == Mode::Compact && message_at < scope.work_since)))
                    || sessions
                        .get(id)
                        .is_some_and(|previous| previous.updated_at > at)
                {
                    continue;
                }
                let title = first_text(&[
                    info.map_or("", |info| &info.title),
                    &log.custom_title,
                    run.map_or("", |run| &run.name),
                    &log.ai_title,
                    &log.first_user,
                ]);
                let cwd = first_text(&[
                    info.map_or("", |info| &info.cwd),
                    &log.cwd,
                    run.map_or("", |run| &run.cwd),
                ]);
                let mut session =
                    claude_session(id, info, run, processes.is_some(), title, cwd, latest, at);
                session.last_user_at = log.last_user_at;
                session.has_transcript = true;
                session.transcript_path = Some(path.clone());
                session.recently_active = message_at >= scope.work_since;
                if let Some(at) = card_at {
                    session.waiting = Some(Waiting::Input);
                    session.waiting_at = Some(at);
                }
                decorate_claude(&mut session, run, Some(&log), scope);
                sessions.insert(id.to_owned(), session);
            }
        }
        for (id, info) in &meta.sessions {
            let run = runs.get(id);
            let at = info
                .at
                .max(run.filter(|run| run.active).map_or(0, |run| run.at));
            if sessions.contains_key(id)
                || info.archived
                || (!wanted.contains(id.as_str())
                    && (scope.mode == Mode::Compact || at < scope.history_since))
            {
                continue;
            }
            let cwd = first_text(&[&info.cwd, run.map_or("", |run| &run.cwd)]);
            let mut session = claude_session(
                id,
                Some(info),
                run,
                processes.is_some(),
                &info.title,
                cwd,
                None,
                at,
            );
            decorate_claude(&mut session, run, None, scope);
            sessions.insert(id.clone(), session);
        }
        for (id, run) in &runs {
            if run.live_pid.is_none()
                || run.desktop
                || sessions.contains_key(id)
                || meta.sessions.contains_key(id)
                || meta.prior_ids.contains(id)
                || scope
                    .watched
                    .get(id.as_str())
                    .is_some_and(|item| item.native_id.is_some())
            {
                continue;
            }
            let mut session = claude_session(
                id,
                None,
                Some(run),
                processes.is_some(),
                &run.name,
                &run.cwd,
                None,
                run.at,
            );
            decorate_claude(&mut session, Some(run), None, scope);
            if run.active || session.waiting.is_some() || session.background > 0 {
                sessions.insert(id.clone(), session);
            }
        }
        if !*partial && root.join("projects").is_dir() {
            for (&id, item) in &scope.watched {
                if item.native_id.is_none()
                    && item.has_transcript
                    && !seen_transcripts.contains(id)
                    && !sessions.contains_key(id)
                    && !runs.get(id).is_some_and(|run| run.live_pid.is_some())
                {
                    removed.insert(id.to_owned());
                }
            }
        }
        // A child may still be visible long after its parent left the current time window.
        let mut parents: Vec<_> = sessions
            .values()
            .filter_map(|session| session.parent_native_id.clone())
            .collect();
        while let Some(parent) = parents.pop() {
            let Some(info) = native.get(parent.as_str()).filter(|info| !info.archived) else {
                continue;
            };
            if sessions.contains_key(&info.id) {
                continue;
            }
            let latest = parent_transcripts
                .remove(info.id.as_str())
                .into_iter()
                .flatten()
                .filter_map(|path| match self.transcripts.read(&path, Provider::Claude, false) {
                    Ok(log) => log.latest,
                    Err(error) => {
                        report_io(&error, partial);
                        None
                    }
                })
                .max_by_key(|message| message.at);
            let mut context = claude_session(
                &info.id,
                Some(info),
                None,
                false,
                &info.title,
                &info.cwd,
                latest,
                info.at,
            );
            context.context_only = true;
            if let Some(grandparent) = &context.parent_native_id {
                parents.push(grandparent.clone());
            }
            sessions.insert(info.id.clone(), context);
        }
        Ok((present, sessions.into_values().collect(), removed))
    }

    pub fn scan(&mut self, home: &Path, hook_root: &Path, scope: &ListScope) -> Snapshot {
        let now = epoch(SystemTime::now());
        let work_since = (*self.first_scan_at.get_or_insert(now)).max(now - RECENT_WORK_MS);
        let codex_root = hooks::provider_root(home, Provider::Codex);
        let claude_root = hooks::provider_root(home, Provider::Claude);
        let desktop = home.join("Library/Application Support/Claude/claude-code-sessions");
        self.transcripts.limit();
        if self.desktop_cache.len() > 3000 {
            self.desktop_cache.clear();
        }
        let activity = activity::read();
        let codex_signals = hooks::signals(hook_root, Provider::Codex);
        let claude_signals = hooks::signals(hook_root, Provider::Claude);
        let for_provider = |provider| ScanScope {
            mode: scope.mode,
            history_since: now - WEEK_MS,
            work_since,
            watched: scope
                .watched
                .iter()
                .filter(|item| item.provider == provider && is_uuid(&item.id))
                .map(|item| (item.id.as_str(), item))
                .collect(),
            signals: if provider == Provider::Codex {
                &codex_signals
            } else {
                &claude_signals
            },
            activity: &activity,
        };
        let mut partial = false;
        let codex = self.codex(
            &codex_root,
            &for_provider(Provider::Codex),
            activity.codex_logs.as_ref(),
            &mut partial,
        );
        let (codex_source, mut sessions, codex_removed) = source(Provider::Codex, partial, codex);
        partial = false;
        let claude = self.claude(
            &claude_root,
            &desktop,
            &for_provider(Provider::Claude),
            activity.claude_processes.as_ref(),
            &mut partial,
        );
        let (claude_source, rows, claude_removed) = source(Provider::Claude, partial, claude);
        sessions.extend(rows);
        sessions.sort_by(|a, b| {
            b.updated_at
                .cmp(&a.updated_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        Snapshot {
            sessions,
            removed: codex_removed.chain(claude_removed).collect(),
            sources: vec![codex_source, claude_source],
            refreshed_at: epoch(SystemTime::now()),
        }
    }
}

fn decorate_claude(
    session: &mut Session,
    run: Option<&ClaudeRun>,
    log: Option<&crate::transcript::Log>,
    scope: &ScanScope<'_>,
) {
    let shells = scope.activity.claude_shells.get(&session.id);
    session.background = shells.map_or(0, HashSet::len);
    let Some((run, pid)) = run.and_then(|run| run.live_pid.map(|pid| (run, pid))) else {
        return;
    };
    session.terminal = scope
        .activity
        .claude_terminals
        .get(&pid)
        .map(|process| TerminalTab {
            process: *process,
            evidence: TerminalEvidence::Claude(run.metadata.clone(), session.id.clone()),
        });
    let minimum = scope
        .activity
        .claude_processes
        .as_ref()
        .and_then(|processes| processes.get(&pid))
        .copied()
        .unwrap_or(0);
    let signal = scope
        .signals
        .get(&session.id)
        .filter(|signal| signal.changed_at >= minimum);
    let waiting = signal
        .and_then(|signal| {
            signal
                .waiting
                .filter(|_| signal.waiting_at >= minimum)
                .map(|waiting| (waiting, signal.waiting_at))
        })
        .or_else(|| {
            log.and_then(|log| log.waiting)
                .filter(|(_, at)| *at >= minimum)
        });
    if let Some((kind, at)) =
        waiting.filter(|(_, at)| session.waiting_at.is_none_or(|saved| *at >= saved))
    {
        session.waiting = Some(kind);
        session.waiting_at = Some(at);
    }
    session.background = log
        .into_iter()
        .flat_map(|log| log.background.iter().map(String::as_str))
        .chain(shells.into_iter().flatten().map(String::as_str))
        .chain(signal.into_iter().flat_map(|signal| {
            signal
                .children
                .iter()
                .filter(|(_, at)| **at >= minimum)
                .map(|(id, _)| id.as_str())
        }))
        .filter(|id| {
            signal.is_none_or(|signal| {
                !signal
                    .stopped_children
                    .get(*id)
                    .is_some_and(|at| *at >= minimum)
            })
        })
        .collect::<HashSet<_>>()
        .len();
}

fn claude_session(
    id: &str,
    desktop: Option<&DesktopInfo>,
    run: Option<&ClaudeRun>,
    processes_read: bool,
    title: &str,
    cwd: &str,
    latest: Option<Message>,
    at: i64,
) -> Session {
    let work_at = run.filter(|run| run.active).map_or(0, |run| run.at);
    let waiting_at = desktop.and_then(|info| info.waiting_at(run));
    Session {
        id: id.to_owned(),
        provider: Provider::Claude,
        title: compact(title, 150),
        preview: latest
            .as_ref()
            .map_or_else(String::new, |message| message.text.clone()),
        role: latest.as_ref().map(|message| message.role),
        cwd: cwd.to_owned(),
        updated_at: at,
        active: processes_read.then(|| run.is_some_and(|run| run.active)),
        waiting: waiting_at.map(|_| Waiting::Input),
        waiting_at,
        background: 0,
        recently_active: false,
        activity_at: work_at.max(latest.as_ref().map_or(0, |message| message.at)),
        last_user_at: None,
        native_id: desktop
            .filter(|info| !info.native_id.is_empty())
            .map(|info| info.native_id.clone()),
        parent_native_id: desktop.and_then(|info| info.parent_native_id.clone()),
        context_only: false,
        has_transcript: false,
        read: None,
        terminal: None,
        cli_alive: run.is_some_and(|run| run.live_pid.is_some()),
        desktop_worker_alive: run.is_some_and(|run| run.desktop && run.live_pid.is_some()),
        transcript_path: None,
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn claude_session_in_use(
    root: &Path,
    id: &str,
    own_pid: Option<u32>,
) -> io::Result<bool> {
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(pid) = name
            .to_str()
            .and_then(|name| name.strip_suffix(".json"))
            .and_then(|pid| pid.parse::<u32>().ok())
            .filter(|&pid| pid > 0 && Some(pid) != own_pid && i32::try_from(pid).is_ok())
        else {
            continue;
        };
        if unsafe { libc::kill(pid as libc::pid_t, 0) } != 0
            && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
        {
            continue;
        }
        match json_file(&entry.path()) {
            Ok(data) if data["sessionId"] == id => return Ok(true),
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(false)
}

fn cli_runs(
    root: &Path,
    processes: Option<&HashMap<u32, i64>>,
    partial: &mut bool,
) -> HashMap<String, ClaudeRun> {
    let mut runs: HashMap<String, ClaudeRun> = HashMap::new();
    for entry in partial_entries(root, partial) {
        let name = entry.file_name();
        if !name
            .to_str()
            .and_then(|name| name.strip_suffix(".json"))
            .is_some_and(|pid| pid.parse::<u32>().is_ok())
        {
            continue;
        }
        let data = match json_file(&entry.path()) {
            Ok(data) => data,
            Err(error) => {
                report_io(&error, partial);
                continue;
            }
        };
        let id = text(&data["sessionId"]);
        if !is_uuid(id) {
            continue;
        }
        let started = date(&data["startedAt"]);
        let pid = data["pid"].as_u64().and_then(|pid| u32::try_from(pid).ok());
        let process_start = processes.and_then(|processes| pid.and_then(|pid| processes.get(&pid)));
        let live_pid = pid.filter(|_| {
            process_start.is_some_and(|process_start| started >= *process_start - 1000)
        });
        let active = live_pid.is_some() && data["status"] == "busy";
        let at = [
            date(&data["statusUpdatedAt"]),
            date(&data["updatedAt"]),
            started,
        ]
        .into_iter()
        .find(|&at| at > 0)
        .unwrap_or(0);
        if runs.get(id).is_none_or(|previous| {
            (active, live_pid.is_some(), at)
                >= (previous.active, previous.live_pid.is_some(), previous.at)
        }) {
            runs.insert(
                id.to_owned(),
                ClaudeRun {
                    name: text(&data["name"]).to_owned(),
                    cwd: text(&data["cwd"]).to_owned(),
                    active,
                    live_pid,
                    metadata: entry.path(),
                    desktop: data["entrypoint"] == "claude-desktop",
                    at,
                },
            );
        }
    }
    runs
}
