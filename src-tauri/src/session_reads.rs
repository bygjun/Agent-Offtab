use crate::model::{is_uuid, Provider, Session, Snapshot, SourceStatus, WEEK_MS};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    fs, io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::SystemTime,
};

pub const READ_BATCH: usize = 12;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ReadStatus {
    pub version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub at: Option<i64>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadUpdate {
    pub provider: Provider,
    pub id: String,
    pub version: String,
    pub at: i64,
}

#[derive(Clone, Default)]
pub struct ReadStore {
    path: Option<PathBuf>,
    entries: Arc<Mutex<Vec<ReadUpdate>>>,
}

pub fn read_version(session: &Session) -> Option<String> {
    if session.context_only
        || session.active != Some(false)
        || session.background > 0
        || session.waiting.is_some()
    {
        return None;
    }
    Some(format!(
        "{}:{}:{}",
        session.activity_at,
        session.updated_at,
        session.last_user_at.unwrap_or(0)
    ))
}

pub fn has_source_rows(snapshot: &Snapshot, provider: Provider) -> bool {
    snapshot.sources.iter().any(|source| {
        source.provider == provider
            && matches!(source.status, SourceStatus::Ready | SourceStatus::Partial)
    })
}

impl ReadStore {
    pub fn load(path: PathBuf) -> Self {
        let entries = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(io::Error::from),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(error) => Err(error),
        }
        .unwrap_or_else(|error| {
            eprintln!("Could not read Offtab session reads: {error}");
            Vec::new()
        });
        Self {
            path: Some(path),
            entries: Arc::new(Mutex::new(entries)),
        }
    }

    pub fn apply(&self, snapshot: &mut Snapshot) {
        let Ok(mut entries) = self.entries.lock() else {
            return;
        };
        let ready: Vec<_> = snapshot
            .sources
            .iter()
            .filter(|source| matches!(source.status, SourceStatus::Ready | SourceStatus::Partial))
            .map(|source| source.provider)
            .collect();
        if !entries.is_empty() {
            let working: HashSet<_> = snapshot
                .sessions
                .iter()
                .filter(|session| {
                    !session.context_only
                        && ready.contains(&session.provider)
                        && (session.active == Some(true)
                            || session.background > 0
                            || session.waiting.is_some())
                })
                .map(|session| {
                    (
                        session.provider,
                        session.native_id.as_deref().unwrap_or(&session.id),
                    )
                })
                .collect();
            let reset = |entry: &ReadUpdate| {
                entry.at <= snapshot.refreshed_at
                    && working.contains(&(entry.provider, entry.id.as_str()))
            };
            if entries.iter().any(reset) {
                let next: Vec<_> = entries
                    .iter()
                    .filter(|entry| !reset(entry))
                    .cloned()
                    .collect();
                if self
                    .path
                    .as_ref()
                    .is_none_or(|path| save(path, &next).is_ok())
                {
                    *entries = next;
                }
            }
        }
        let read_times: HashMap<_, _> = entries
            .iter()
            .map(|entry| {
                (
                    (entry.provider, entry.id.as_str(), entry.version.as_str()),
                    entry.at,
                )
            })
            .collect();
        for session in &mut snapshot.sessions {
            let id = session.native_id.as_deref().unwrap_or(&session.id);
            session.read = read_version(session)
                .filter(|_| ready.contains(&session.provider))
                .map(|version| {
                    let at = read_times
                        .get(&(session.provider, id, version.as_str()))
                        .copied();
                    ReadStatus { version, at }
                });
        }
    }

    pub fn mark(&self, updates: Vec<ReadUpdate>) -> io::Result<()> {
        if updates.len() > READ_BATCH {
            return Err(io::Error::other("Too many read updates"));
        }
        let now = crate::transcript::epoch(SystemTime::now());
        let mut entries = self
            .entries
            .lock()
            .map_err(|_| io::Error::other("Read store unavailable"))?;
        let mut next = entries.clone();
        next.retain(|entry| entry.at >= now.saturating_sub(WEEK_MS));
        let mut changed = next.len() != entries.len();
        for mut update in updates {
            if !is_uuid(&update.id) && !update.id.strip_prefix("local_").is_some_and(is_uuid) {
                continue;
            }
            if update.version.len() > 64
                || update.version.split(':').count() != 3
                || !update
                    .version
                    .split(':')
                    .all(|value| value.parse::<i64>().is_ok())
                || update.at < now.saturating_sub(WEEK_MS)
            {
                continue;
            }
            update.at = update.at.min(now);
            let activity_at = update
                .version
                .split(':')
                .next()
                .and_then(|value| value.parse::<i64>().ok());
            if activity_at.is_none_or(|activity_at| update.at < activity_at) {
                continue;
            }
            if let Some(previous) = next.iter_mut().find(|entry| {
                entry.provider == update.provider
                    && entry.id == update.id
                    && entry.version == update.version
            }) {
                if update.at < previous.at {
                    previous.at = update.at;
                    changed = true;
                }
            } else {
                next.push(update);
                changed = true;
            }
        }
        if changed {
            if let Some(path) = &self.path {
                save(path, &next)?;
            }
            *entries = next;
        }
        Ok(())
    }
}

fn save(path: &Path, entries: &[ReadUpdate]) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("Missing read store directory"))?;
    fs::create_dir_all(parent)?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer(&mut file, entries)?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|error| error.error)?;
    Ok(())
}
