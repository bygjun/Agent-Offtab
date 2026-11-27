use crate::{
    mobile_attachment::{validate_uploads, Upload, MAX_TOTAL_BYTES},
    mobile_devices::{DeviceStore, SavedDevice, SavedSend},
    mobile_reply::{Dispatcher, ResultCode},
    model::{
        is_uuid, ListScope, Mode, Provider, Role, Session, SessionId, Snapshot, Source, Waiting,
        Watched,
    },
    session_reads::{has_source_rows, read_version, ReadStatus, ReadStore, ReadUpdate, READ_BATCH},
    transcript::epoch,
};
use aes_gcm::{aead::Aead, Aes256Gcm, KeyInit, Nonce};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD as B64, Engine};
use hkdf::Hkdf;
use p256::{ecdh::EphemeralSecret, elliptic_curve::rand_core::OsRng, PublicKey};
use reqwest::{blocking::Client, redirect, StatusCode, Url};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, VecDeque},
    io::{BufReader, Read},
    path::Path,
    sync::{mpsc, Arc, Mutex},
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime},
};
use subtle::ConstantTimeEq;

const PAIR_WINDOW: Duration = Duration::from_secs(120);
const PAIR_POLL: Duration = Duration::from_secs(1);
const SYNC_POLL: Duration = Duration::from_secs(8);
const REQUEST_POLL: Duration = Duration::from_secs(1);
const REPLY_POLL: Duration = Duration::from_secs(2);
const MAX_ATTEMPTS: u8 = 5;
const MAX_ROWS: usize = 90;
const MAX_DEVICES: usize = 4;
const REPLY_WINDOW_MS: i64 = 30_000;
const ATTACHMENT_REPLY_WINDOW_MS: i64 = 120_000;
const REPLY_RESULT_WINDOW_MS: i64 = 5 * 60_000;
const MAX_REPLY_CHARS: usize = 2_000;
const MAX_SEALED_REPLY: usize = 38_000_000;
// Both the attachment bytes and the encrypted JSON envelope are base64-encoded.
const _: () =
    assert!((MAX_TOTAL_BYTES.div_ceil(3) * 4 + 32_768).div_ceil(3) * 4 < MAX_SEALED_REPLY);
const CONTEXT_WINDOW: Duration = Duration::from_secs(300);

#[derive(Deserialize)]
struct Registration {
    id: String,
    host: String,
    invite: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RelayState {
    phase: Phase,
    public_key: Option<String>,
    request: Option<PinRequest>,
    reply: Option<SealedReply>,
    context: Option<SealedReply>,
    read: Option<SealedReply>,
}

#[derive(Deserialize)]
struct PinRequest {
    id: String,
    sealed: String,
}

#[derive(Deserialize)]
struct SealedReply {
    id: String,
    sealed: String,
}

#[derive(Deserialize)]
struct MobileReadUpdate {
    id: String,
    version: String,
    at: i64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReadRequest {
    id: String,
    sent_at: i64,
    reads: Vec<MobileReadUpdate>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Reply {
    id: String,
    target: String,
    permit: Option<String>,
    updated_at: i64,
    sent_at: i64,
    #[serde(default)]
    text: String,
    attachment: Option<Upload>,
    #[serde(default)]
    attachments: Vec<Upload>,
    #[serde(default)]
    kind: ReplyKind,
    #[serde(default)]
    history_limit: usize,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "lowercase")]
enum ReplyKind {
    #[default]
    Send,
    Context,
}

struct ContextSelection {
    id: String,
    target: String,
    expires: Instant,
    limit: usize,
    cached: Option<(i64, Result<Vec<crate::transcript::VisibleTurn>, ()>)>,
}

#[derive(Serialize)]
struct ReplyContext<'a> {
    id: &'a str,
    answer: Option<&'a str>,
    turns: Option<&'a [crate::transcript::VisibleTurn]>,
    failed: bool,
}

#[derive(Clone, Serialize)]
struct Receipt {
    id: String,
    code: ResultCode,
}

#[derive(Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum Phase {
    Scan,
    Pin,
    Paired,
    Locked,
    Expired,
    Revoking,
    Revoked,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    id: String,
    label: String,
    #[serde(skip)]
    permit: Option<String>,
    reply_enabled: bool,
    phase: Phase,
    url: Option<String>,
    pin: Option<String>,
    interrupted: bool,
}

struct Relay {
    client: Client,
    endpoint: Url,
    host: String,
}

impl Relay {
    fn url(&self, action: &str) -> Url {
        let mut url = self.endpoint.clone();
        url.path_segments_mut()
            .expect("validated HTTP origin")
            .push(action);
        url
    }

    fn state(&self) -> Result<Option<RelayState>, ()> {
        let response = self
            .client
            .get(self.url("host"))
            .timeout(Duration::from_secs(12))
            .bearer_auth(&self.host)
            .send()
            .map_err(|_| ())?;
        if response.status() == StatusCode::NOT_FOUND {
            let body = response.take(512);
            let error: serde_json::Value = serde_json::from_reader(body).map_err(|_| ())?;
            return if error["code"] == "link_ended" {
                Ok(None)
            } else {
                Err(())
            };
        }
        decode(response, (MAX_SEALED_REPLY + 16_384) as u64).map(Some)
    }

    fn revoke(&self) -> Result<(), ()> {
        let response = self
            .client
            .delete(self.endpoint.clone())
            .bearer_auth(&self.host)
            .send()
            .map_err(|_| ())?;
        if response.status() == StatusCode::NO_CONTENT {
            return Ok(());
        }
        if response.status() == StatusCode::NOT_FOUND {
            let error: serde_json::Value =
                serde_json::from_reader(response.take(512)).map_err(|_| ())?;
            if error["code"] == "link_ended" {
                return Ok(());
            }
        }
        Err(())
    }
}

fn decode<T: DeserializeOwned>(response: reqwest::blocking::Response, limit: u64) -> Result<T, ()> {
    let body = BufReader::new(response.error_for_status().map_err(|_| ())?.take(limit));
    serde_json::from_reader(body).map_err(|_| ())
}

struct Connection {
    relay: Arc<Relay>,
    status: Arc<Mutex<Status>>,
    worker: Option<(mpsc::Sender<()>, JoinHandle<()>)>,
}

impl Connection {
    fn revoke_in_background(&mut self, store: Option<DeviceStore>) {
        let previous = self.worker.take();
        if let Some((stop, _)) = &previous {
            let _ = stop.send(());
        }
        let relay = Arc::clone(&self.relay);
        let status = Arc::clone(&self.status);
        let (stop, stopped) = mpsc::channel();
        let worker = thread::spawn(move || {
            if let Some((_, worker)) = previous {
                let _ = worker.join();
            }
            let Ok(guard) = status.lock() else { return };
            let id = guard.id.clone();
            drop(guard);
            let mut relay_revoked = false;
            loop {
                if !matches!(stopped.try_recv(), Err(mpsc::TryRecvError::Empty)) {
                    return;
                }
                relay_revoked = relay_revoked || relay.revoke().is_ok();
                if relay_revoked && store.as_ref().is_none_or(|store| store.remove(&id).is_ok()) {
                    if let Ok(mut status) = status.lock() {
                        status.phase = Phase::Revoked;
                    }
                    return;
                }
                if !matches!(
                    stopped.recv_timeout(SYNC_POLL),
                    Err(mpsc::RecvTimeoutError::Timeout)
                ) {
                    return;
                }
            }
        });
        self.worker = Some((stop, worker));
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        if let Some((stop, _)) = self.worker.take() {
            let _ = stop.send(());
        }
    }
}

#[derive(Default)]
pub struct Mobile {
    connections: Vec<Connection>,
    store: Option<DeviceStore>,
    startup_error: Option<String>,
    reads: ReadStore,
}

#[derive(Clone, Copy)]
pub enum ReplyTargets<'a> {
    Device(&'a str),
    All,
}

impl Mobile {
    pub fn with_reads(reads: ReadStore) -> Self {
        Self {
            reads,
            ..Self::default()
        }
    }

    pub fn restore(
        &mut self,
        relay: &str,
        local_relay: Option<&str>,
        dispatcher: Dispatcher,
        collect: impl FnMut(&[Watched]) -> Result<Snapshot, String> + Clone + Send + 'static,
    ) {
        let result = relay_url(relay).and_then(|origin| {
            let store = DeviceStore::load(origin.to_string())?;
            let endpoint = local_relay.map(relay_url).transpose()?.unwrap_or(origin);
            self.restore_from_store(endpoint, store, dispatcher, collect)
        });
        self.startup_error = result.err();
    }

    fn restore_from_store(
        &mut self,
        origin: Url,
        store: DeviceStore,
        dispatcher: Dispatcher,
        collect: impl FnMut(&[Watched]) -> Result<Snapshot, String> + Clone + Send + 'static,
    ) -> Result<(), String> {
        let client = relay_client()?;
        let devices = store.list().map_err(|_| storage_error())?;
        if devices.len() > MAX_DEVICES
            || devices
                .iter()
                .any(|saved| !valid_token(&saved.id) || !valid_token(&saved.host))
        {
            return Err(storage_error());
        }
        self.store = Some(store.clone());
        for saved in devices {
            let salt = B64
                .decode(&saved.id)
                .ok()
                .filter(|salt| salt.len() == 32)
                .ok_or_else(storage_error)?;
            let endpoint = origin
                .join(&format!("api/links/{}", saved.id))
                .map_err(|_| storage_error())?;
            let relay = Arc::new(Relay {
                client: client.clone(),
                endpoint,
                host: saved.host,
            });
            let status = Arc::new(Mutex::new(Status {
                id: saved.id.clone(),
                label: saved.label,
                reply_enabled: saved.permit.is_some() && !saved.revoking,
                permit: if saved.revoking { None } else { saved.permit },
                phase: if saved.revoking {
                    Phase::Revoking
                } else {
                    Phase::Paired
                },
                url: None,
                pin: None,
                interrupted: true,
            }));
            let mut connection = Connection {
                relay,
                status,
                worker: None,
            };
            if saved.revoking {
                connection.revoke_in_background(Some(store.clone()));
                self.connections.push(connection);
                continue;
            }
            let (stop, stopped) = mpsc::channel();
            let worker = {
                let relay = Arc::clone(&connection.relay);
                let status = Arc::clone(&connection.status);
                let store = store.clone();
                let dispatcher = dispatcher.clone();
                let reads = self.reads.clone();
                let mut collect = collect.clone();
                thread::spawn(move || {
                    let mut bridge =
                        Bridge::new(&relay, &status, Some(store), salt, dispatcher, reads);
                    bridge.channel = Some(Channel::from_keys(saved.keys));
                    bridge.decision = Some((saved.accepted_request, true));
                    let now = epoch(SystemTime::now());
                    if let Some((id, until)) = saved.last_send.filter(|(_, until)| *until > now) {
                        bridge.seen_replies.insert(id.clone(), until);
                        bridge.remember_receipt(id, ResultCode::Unknown);
                    }
                    for send in saved
                        .recent_sends
                        .into_iter()
                        .filter(|send| send.until > now)
                    {
                        bridge.seen_replies.insert(send.id.clone(), send.until);
                        bridge.remember_receipt(send.id, send.outcome);
                    }
                    bridge.run(stopped, &mut collect);
                })
            };
            connection.worker = Some((stop, worker));
            self.connections.push(connection);
        }
        Ok(())
    }

    pub fn initialized(&self) -> bool {
        self.store.is_some() || self.startup_error.is_some()
    }

    pub fn startup_error(&self) -> Option<&str> {
        self.startup_error.as_deref()
    }

    pub fn status(&self) -> Vec<Status> {
        self.connections
            .iter()
            .filter_map(|connection| {
                let status = connection.status.lock().ok()?;
                (status.phase != Phase::Revoked).then(|| status.clone())
            })
            .collect()
    }

    pub fn preview_url(&self, device: &str) -> Option<String> {
        self.connections.iter().find_map(|connection| {
            let status = connection.status.lock().ok()?;
            if status.id == device && status.phase == Phase::Scan {
                return status
                    .url
                    .as_ref()
                    .filter(|url| is_local_preview(url))
                    .cloned();
            }
            None
        })
    }

    pub fn allow_reply(
        &mut self,
        targets: ReplyTargets<'_>,
        enabled: bool,
    ) -> Result<Vec<Status>, String> {
        let mut changes = Vec::new();
        let mut found = false;
        for connection in &self.connections {
            let status = connection
                .status
                .lock()
                .map_err(|_| "기기 상태를 읽지 못했습니다.")?;
            if status.phase != Phase::Paired
                || matches!(targets, ReplyTargets::Device(id) if status.id != id)
            {
                continue;
            }
            found = true;
            if status.reply_enabled != enabled {
                let permit = if enabled {
                    Some(B64.encode(random::<32>().map_err(|_| "답장 권한을 만들지 못했습니다.")?))
                } else {
                    None
                };
                changes.push((status, permit));
            }
        }
        if !found {
            return Err("연결된 기기를 찾을 수 없습니다.".into());
        }
        if !changes.is_empty() {
            if let Some(store) = &self.store {
                store
                    .update(|devices| {
                        for (status, permit) in &changes {
                            let device = devices
                                .iter_mut()
                                .find(|device| device.id == status.id && !device.revoking)
                                .ok_or(())?;
                            device.permit = permit.clone();
                        }
                        Ok(())
                    })
                    .map_err(|_| storage_error())?;
            }
        }
        for (mut status, permit) in changes {
            status.permit = permit;
            status.reply_enabled = enabled;
        }
        Ok(self.status())
    }

    pub fn start(
        &mut self,
        relay: &str,
        local_relay: Option<&str>,
        label: Option<String>,
        dispatcher: Dispatcher,
        collect: impl FnMut(&[Watched]) -> Result<Snapshot, String> + Send + 'static,
    ) -> Result<Vec<Status>, String> {
        let store = self.store.clone().ok_or_else(|| {
            self.startup_error.clone().unwrap_or_else(|| {
                "휴대폰 연결을 불러오는 중입니다. 잠시 후 다시 시도해 주세요.".into()
            })
        })?;
        self.connections.retain(|connection| {
            connection
                .status
                .lock()
                .is_ok_and(|status| status.phase != Phase::Revoked)
        });
        let statuses = self.status();
        if statuses
            .iter()
            .any(|status| matches!(status.phase, Phase::Scan | Phase::Pin))
        {
            return Err("진행 중인 기기 연결을 먼저 마쳐 주세요.".into());
        }
        if self.connections.len() >= MAX_DEVICES {
            return Err("기기는 최대 4대까지 연결할 수 있습니다.".into());
        }
        let mut origin = relay_url(relay)?;
        let endpoint_origin = local_relay
            .map(relay_url)
            .transpose()?
            .unwrap_or_else(|| origin.clone());
        let client = relay_client()?;
        let registration: Registration = client
            .post(
                endpoint_origin
                    .join("api/links")
                    .map_err(|_| relay_error())?,
            )
            .send()
            .map_err(|_| relay_error())
            .and_then(|response| decode(response, 40_000).map_err(|_| relay_error()))?;
        if !valid_token(&registration.id)
            || !valid_token(&registration.host)
            || !valid_token(&registration.invite)
        {
            return Err(relay_error());
        }

        let salt = B64.decode(&registration.id).map_err(|_| relay_error())?;
        let endpoint = endpoint_origin
            .join(&format!("api/links/{}", registration.id))
            .map_err(|_| relay_error())?;
        let secret = EphemeralSecret::random(&mut OsRng);
        let public_key = B64.encode(PublicKey::from(&secret).to_sec1_bytes());
        // Give each QR its own page URL so an existing tab opens a fresh pairing screen.
        let navigation = B64.encode(random::<8>().map_err(|_| relay_error())?);
        origin.query_pairs_mut().append_pair("join", &navigation);
        origin.set_fragment(Some(&format!(
            "{}.{}.{}",
            registration.id, registration.invite, public_key
        )));
        let label = label
            .map(|value| excerpt(value.trim(), 30))
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| format!("기기 {}", &registration.id[..4]));
        let status = Arc::new(Mutex::new(Status {
            id: registration.id,
            label,
            permit: None,
            reply_enabled: false,
            phase: Phase::Scan,
            url: Some(origin.into()),
            pin: None,
            interrupted: false,
        }));
        let relay = Arc::new(Relay {
            client,
            endpoint,
            host: registration.host,
        });
        let (stop, stopped) = mpsc::channel();
        let worker = {
            let relay = Arc::clone(&relay);
            let status = Arc::clone(&status);
            let reads = self.reads.clone();
            thread::spawn(move || {
                let mut bridge = Bridge::new(&relay, &status, Some(store), salt, dispatcher, reads);
                bridge.secret = Some(secret);
                bridge.deadline = Some(Instant::now() + PAIR_WINDOW);
                bridge.run(stopped, collect);
            })
        };
        self.connections.push(Connection {
            relay,
            status,
            worker: Some((stop, worker)),
        });
        Ok(self.status())
    }

    pub fn stop(&mut self, device: &str) -> Result<Vec<Status>, String> {
        let connection = self
            .connections
            .iter_mut()
            .find(|connection| {
                connection
                    .status
                    .lock()
                    .is_ok_and(|status| status.id == device)
            })
            .ok_or_else(|| "연결된 기기를 찾을 수 없습니다.".to_string())?;
        {
            let mut status = connection.status.lock().map_err(|_| storage_error())?;
            if matches!(status.phase, Phase::Revoking | Phase::Revoked) {
                drop(status);
                return Ok(self.status());
            }
            if let Some(store) = &self.store {
                store
                    .update(|devices| {
                        if let Some(saved) = devices.iter_mut().find(|saved| saved.id == device) {
                            saved.revoking = true;
                            saved.permit = None;
                        }
                        Ok(())
                    })
                    .map_err(|_| storage_error())?;
            }
            status.permit = None;
            status.reply_enabled = false;
            status.phase = Phase::Revoking;
            status.url = None;
            status.pin = None;
        }
        connection.revoke_in_background(self.store.clone());
        Ok(self.status())
    }
}

struct Channel {
    keys: [[u8; 32]; 3],
    pin: Aes256Gcm,
    snapshot: Aes256Gcm,
    reply: Aes256Gcm,
}

impl Channel {
    fn from_keys(keys: [[u8; 32]; 3]) -> Self {
        Self {
            pin: Aes256Gcm::new((&keys[0]).into()),
            snapshot: Aes256Gcm::new((&keys[1]).into()),
            reply: Aes256Gcm::new((&keys[2]).into()),
            keys,
        }
    }

    fn establish(secret: EphemeralSecret, public_key: &str, salt: &[u8]) -> Result<Self, ()> {
        let public =
            PublicKey::from_sec1_bytes(&B64.decode(public_key).map_err(|_| ())?).map_err(|_| ())?;
        let shared = secret.diffie_hellman(&public);
        let hkdf = Hkdf::<Sha256>::new(Some(salt), shared.raw_secret_bytes());
        let key = |context| {
            let mut key = [0; 32];
            hkdf.expand(context, &mut key).map_err(|_| ())?;
            Ok(key)
        };
        Ok(Self::from_keys([
            key(b"offtab/pin/v1")?,
            key(b"offtab/snapshot/v1")?,
            key(b"offtab/reply/v1")?,
        ]))
    }
}

struct Bridge<'a> {
    relay: &'a Relay,
    status: &'a Mutex<Status>,
    store: Option<DeviceStore>,
    reads: ReadStore,
    secret: Option<EphemeralSecret>,
    salt: Vec<u8>,
    deadline: Option<Instant>,
    pin: Option<String>,
    channel: Option<Channel>,
    attempts: u8,
    decision: Option<(String, bool)>,
    dispatcher: Dispatcher,
    reply_job: Option<(String, mpsc::Receiver<ResultCode>)>,
    last_published: Option<Instant>,
    seen_replies: HashMap<String, i64>,
    receipts: VecDeque<Receipt>,
    context: Option<ContextSelection>,
    watched: Vec<Watched>,
    waiting_sessions: Option<HashMap<String, (Waiting, i64)>>,
}

impl<'a> Bridge<'a> {
    fn new(
        relay: &'a Relay,
        status: &'a Mutex<Status>,
        store: Option<DeviceStore>,
        salt: Vec<u8>,
        dispatcher: Dispatcher,
        reads: ReadStore,
    ) -> Self {
        Self {
            relay,
            status,
            store,
            reads,
            salt,
            dispatcher,
            secret: None,
            deadline: None,
            pin: None,
            channel: None,
            attempts: 0,
            decision: None,
            reply_job: None,
            last_published: None,
            seen_replies: HashMap::new(),
            receipts: VecDeque::new(),
            context: None,
            watched: Vec::new(),
            waiting_sessions: None,
        }
    }

    fn forget_registration(&self) {
        if let (Some(store), Ok(status)) = (&self.store, self.status.lock()) {
            let _ = store.remove(&status.id);
        }
    }

    fn show(&self, phase: Phase, pin: Option<String>, interrupted: bool) {
        if let Ok(mut status) = self.status.lock() {
            if matches!(status.phase, Phase::Revoking | Phase::Revoked) {
                return;
            }
            status.phase = phase;
            if matches!(phase, Phase::Locked | Phase::Expired) {
                status.permit = None;
                status.reply_enabled = false;
            }
            if phase != Phase::Scan {
                status.url = None;
            }
            status.pin = pin;
            status.interrupted = interrupted;
        }
    }

    fn run(
        &mut self,
        stopped: mpsc::Receiver<()>,
        mut collect: impl FnMut(&[Watched]) -> Result<Snapshot, String>,
    ) {
        let mut delay = PAIR_POLL;
        loop {
            if let Ok(()) | Err(mpsc::TryRecvError::Disconnected) = stopped.try_recv() {
                return;
            }
            if self
                .deadline
                .is_some_and(|deadline| Instant::now() >= deadline)
            {
                self.show(Phase::Expired, None, false);
                if self.relay.revoke().is_ok() {
                    self.forget_registration();
                }
                return;
            }
            match self.relay.state() {
                Ok(Some(state)) => match self.sync(state, &mut collect) {
                    Ok(Some(next_delay)) => delay = next_delay,
                    Ok(None) => return,
                    Err(()) => self.interrupted(),
                },
                Ok(None) => {
                    self.show(Phase::Expired, None, false);
                    self.forget_registration();
                    return;
                }
                Err(()) => self.interrupted(),
            }
            if !matches!(
                stopped.recv_timeout(delay),
                Err(mpsc::RecvTimeoutError::Timeout)
            ) {
                return;
            }
        }
    }

    fn interrupted(&self) {
        if let Ok(mut status) = self.status.lock() {
            status.interrupted = true;
        }
    }

    fn sync(
        &mut self,
        state: RelayState,
        collect: &mut impl FnMut(&[Watched]) -> Result<Snapshot, String>,
    ) -> Result<Option<Duration>, ()> {
        match state.phase {
            Phase::Scan if self.secret.is_some() => {
                self.show(Phase::Scan, None, false);
                Ok(Some(PAIR_POLL))
            }
            Phase::Pin => {
                if let Some(secret) = self.secret.take() {
                    let channel = Channel::establish(
                        secret,
                        state.public_key.as_deref().ok_or(())?,
                        &self.salt,
                    )?;
                    self.channel = Some(channel);
                    self.pin = Some(random_pin()?);
                    self.deadline = Some(Instant::now() + PAIR_WINDOW);
                }
                if let Some(request) = state.request {
                    self.answer_pin(request)?;
                }
                let accepted = self
                    .decision
                    .as_ref()
                    .is_some_and(|(_, accepted)| *accepted);
                self.show(
                    Phase::Pin,
                    if accepted { None } else { self.pin.clone() },
                    false,
                );
                Ok(Some(PAIR_POLL))
            }
            Phase::Paired
                if self
                    .decision
                    .as_ref()
                    .is_some_and(|(_, accepted)| *accepted) =>
            {
                self.pin = None;
                self.deadline = None;
                self.show(Phase::Paired, None, false);
                let finished = self.check_job();
                let interval = if self.reply_job.is_some() {
                    REPLY_POLL
                } else {
                    SYNC_POLL
                };
                let due = self
                    .last_published
                    .is_none_or(|last| last.elapsed() >= interval);
                if state.reply.is_some()
                    || state.context.is_some()
                    || state.read.is_some()
                    || finished
                    || due
                {
                    let mut snapshot = collect(&self.watched).map_err(|_| ())?;
                    if let Some(reply) = state.reply {
                        self.handle_reply(reply, &snapshot, false);
                    }
                    if let Some(context) = state.context {
                        self.handle_reply(context, &snapshot, true);
                    }
                    if let Some(read) = state.read {
                        self.handle_read(read, &snapshot)?;
                    }
                    self.reads.apply(&mut snapshot);
                    self.publish(snapshot)?;
                    self.last_published = Some(Instant::now());
                }
                self.show(Phase::Paired, None, false);
                Ok(Some(REQUEST_POLL))
            }
            Phase::Locked => {
                self.show(Phase::Locked, None, false);
                Ok(None)
            }
            _ => Err(()),
        }
    }

    fn answer_pin(&mut self, request: PinRequest) -> Result<(), ()> {
        if self
            .decision
            .as_ref()
            .is_none_or(|(id, _)| id != &request.id)
        {
            let channel = self.channel.as_ref().ok_or(())?;
            let pin = self.pin.as_ref().ok_or(())?;
            // Enforce the guess budget on the Mac even if the relay is tampered with.
            let accepted = self.attempts < MAX_ATTEMPTS
                && decrypt_pin(&channel.pin, &request.sealed)
                    .is_some_and(|value| bool::from(value.ct_eq(pin.as_bytes())));
            self.attempts = self.attempts.saturating_add(1);
            self.decision = Some((request.id, accepted));
        }
        let (id, accepted) = self.decision.as_ref().ok_or(())?;
        if let Some(store) = self.store.as_ref().filter(|_| *accepted) {
            let status = self.status.lock().map_err(|_| ())?;
            if matches!(status.phase, Phase::Revoking | Phase::Revoked) {
                return Err(());
            }
            let channel = self.channel.as_ref().ok_or(())?;
            // Save before acknowledging the PIN so a crash cannot strand the browser.
            if !store.list()?.iter().any(|device| device.id == status.id) {
                store.update(|devices| {
                    devices.push(SavedDevice {
                        id: status.id.clone(),
                        label: status.label.clone(),
                        host: self.relay.host.clone(),
                        keys: channel.keys,
                        accepted_request: id.clone(),
                        permit: None,
                        revoking: false,
                        last_send: None,
                        recent_sends: Vec::new(),
                    });
                    Ok(())
                })?;
            }
        }
        self.relay
            .client
            .post(self.relay.url("decision"))
            .bearer_auth(&self.relay.host)
            .json(&serde_json::json!({ "id": id, "accepted": accepted }))
            .send()
            .and_then(|response| response.error_for_status())
            .map_err(|_| ())?;
        if *accepted {
            self.deadline = None;
        }
        Ok(())
    }

    fn remember_receipt(&mut self, id: String, code: ResultCode) {
        self.receipts.retain(|receipt| receipt.id != id);
        if self.receipts.len() == 16 {
            self.receipts.pop_front();
        }
        self.receipts.push_back(Receipt { id, code });
    }

    fn check_job(&mut self) -> bool {
        let Some((id, result)) = self.reply_job.as_ref() else {
            return false;
        };
        let code = match result.try_recv() {
            Ok(code) => code,
            Err(mpsc::TryRecvError::Disconnected) => ResultCode::Unknown,
            Err(mpsc::TryRecvError::Empty) => return false,
        };
        let id = id.clone();
        self.reply_job = None;
        self.save_reply_outcome(&id, code);
        self.remember_receipt(id, code);
        true
    }

    fn save_reply_outcome(&self, reply_id: &str, outcome: ResultCode) {
        let Some(store) = &self.store else { return };
        let Ok(status) = self.status.lock() else {
            return;
        };
        let _ = store.update(|devices| {
            let entry = devices
                .iter_mut()
                .find(|device| device.id == status.id)
                .and_then(|device| {
                    device
                        .recent_sends
                        .iter_mut()
                        .find(|send| send.id == reply_id)
                })
                .ok_or(())?;
            entry.outcome = outcome;
            Ok(())
        });
    }

    fn handle_reply(&mut self, envelope: SealedReply, snapshot: &Snapshot, context_only: bool) {
        let now = epoch(SystemTime::now());
        self.seen_replies.retain(|_, expires| now < *expires);
        if is_uuid(&envelope.id) && !self.seen_replies.contains_key(&envelope.id) {
            let code = self.try_reply(&envelope, snapshot, now, context_only);
            self.seen_replies
                .insert(envelope.id.clone(), now.saturating_add(REPLY_WINDOW_MS * 2));
            if !matches!(code, ResultCode::Context) {
                self.remember_receipt(envelope.id.clone(), code);
            }
        }
        let _ = self
            .relay
            .client
            .post(self.relay.url("reply-ack"))
            .bearer_auth(&self.relay.host)
            .json(&serde_json::json!({ "id": envelope.id }))
            .send();
    }

    fn handle_read(&self, envelope: SealedReply, snapshot: &Snapshot) -> Result<(), ()> {
        let status = self.status.lock().map_err(|_| ())?;
        if status.phase != Phase::Paired {
            return Ok(());
        }
        if let Some(request) = self.channel.as_ref().and_then(|channel| {
            open_envelope::<ReadRequest>(&channel.reply, &envelope.sealed, 4096)
        }) {
            let now = epoch(SystemTime::now());
            if request.id == envelope.id
                && is_uuid(&request.id)
                && request.reads.len() <= READ_BATCH
                && request.sent_at <= now.saturating_add(5_000)
                && now.saturating_sub(request.sent_at) < REPLY_WINDOW_MS
            {
                let sessions = shared_rows(snapshot);
                let updates = request
                    .reads
                    .into_iter()
                    .filter_map(|read| {
                        let session = sessions
                            .iter()
                            .find(|session| opaque_session_id(&self.salt, session) == read.id)?;
                        if !has_source_rows(snapshot, session.provider)
                            || read_version(session).as_deref() != Some(&read.version)
                        {
                            return None;
                        }
                        Some(ReadUpdate {
                            id: session
                                .native_id
                                .clone()
                                .unwrap_or_else(|| session.id.clone()),
                            provider: session.provider,
                            version: read.version,
                            at: read.at,
                        })
                    })
                    .collect();
                self.reads.mark(updates).map_err(|_| ())?;
            }
        }
        drop(status);
        let _ = self
            .relay
            .client
            .post(self.relay.url("reply-ack"))
            .bearer_auth(&self.relay.host)
            .json(&serde_json::json!({ "id": envelope.id, "kind": "read" }))
            .send();
        Ok(())
    }

    fn try_reply(
        &mut self,
        envelope: &SealedReply,
        snapshot: &Snapshot,
        now: i64,
        context_only: bool,
    ) -> ResultCode {
        let Some(request) = self
            .channel
            .as_ref()
            .and_then(|channel| open_reply(&channel.reply, &envelope.sealed))
        else {
            return ResultCode::Blocked;
        };
        let window = if request.attachment.is_some() || !request.attachments.is_empty() {
            ATTACHMENT_REPLY_WINDOW_MS
        } else {
            REPLY_WINDOW_MS
        };
        if (context_only && !matches!(request.kind, ReplyKind::Context))
            || request.id != envelope.id
            || request.sent_at > now.saturating_add(5_000)
            || now.saturating_sub(request.sent_at) >= window
        {
            return ResultCode::Blocked;
        }
        if matches!(request.kind, ReplyKind::Send) {
            let Ok(status) = self.status.lock() else {
                return ResultCode::Blocked;
            };
            let (Some(expected), Some(provided)) =
                (status.permit.as_ref(), request.permit.as_ref())
            else {
                return ResultCode::Blocked;
            };
            if !bool::from(expected.as_bytes().ct_eq(provided.as_bytes())) {
                return ResultCode::Blocked;
            }
        }
        let Some(target) = shared_rows(snapshot)
            .into_iter()
            .find(|session| opaque_session_id(&self.salt, session) == request.target)
        else {
            return ResultCode::Unavailable;
        };
        if matches!(request.kind, ReplyKind::Context) {
            if answer_path(target).is_none() {
                return ResultCode::Unavailable;
            }
            self.context = Some(ContextSelection {
                id: request.id,
                target: request.target,
                expires: Instant::now() + CONTEXT_WINDOW,
                limit: request.history_limit.min(10),
                cached: None,
            });
            return ResultCode::Context;
        }
        if self.reply_job.is_some() {
            return ResultCode::Busy;
        }
        let text = request.text.trim();
        if (text.is_empty() && request.attachment.is_none() && request.attachments.is_empty())
            || text.starts_with(['/', '!'])
            || text.chars().count() > MAX_REPLY_CHARS
            || text
                .chars()
                .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
        {
            return ResultCode::Blocked;
        }

        if target.updated_at != request.updated_at
            || !snapshot.sources.iter().any(|source| {
                source.provider == target.provider
                    && matches!(source.status, crate::model::SourceStatus::Ready)
            })
        {
            return ResultCode::Busy;
        }
        let Some(attachments) =
            validate_uploads(request.attachment.into_iter().chain(request.attachments))
        else {
            return ResultCode::InvalidAttachment;
        };
        if let Some(store) = &self.store {
            let Ok(status) = self.status.lock() else {
                return ResultCode::Blocked;
            };
            let saved = store.update(|devices| {
                let device = devices
                    .iter_mut()
                    .find(|device| device.id == status.id && !device.revoking)
                    .ok_or(())?;
                if device.permit != request.permit {
                    return Err(());
                }
                device.recent_sends.retain(|send| send.until > now);
                if device.recent_sends.iter().any(|send| send.id == request.id) {
                    return Err(());
                }
                device.recent_sends.push(SavedSend {
                    id: request.id.clone(),
                    until: now.saturating_add(REPLY_RESULT_WINDOW_MS),
                    // A crash during dispatch leaves the delivery outcome unknown.
                    outcome: ResultCode::Unknown,
                });
                Ok(())
            });
            if saved.is_err() {
                return ResultCode::Blocked;
            }
            // Linearize dispatch against changes to the permission.
            let result = self.dispatch_result(target, text, attachments, &request.id);
            drop(status);
            if result != ResultCode::Processing {
                self.save_reply_outcome(&request.id, result);
            }
            return result;
        }
        self.dispatch_result(target, text, attachments, &request.id)
    }

    fn dispatch_result(
        &mut self,
        target: &Session,
        text: &str,
        attachments: Vec<crate::mobile_attachment::Attachment>,
        id: &str,
    ) -> ResultCode {
        match self.dispatcher.dispatch(target, id, text, attachments) {
            Ok(result) => {
                self.reply_job = Some((id.into(), result));
                ResultCode::Processing
            }
            Err(code) => code,
        }
    }

    fn publish(&mut self, snapshot: Snapshot) -> Result<(), ()> {
        let channel = self.channel.as_ref().ok_or(())?;
        let sessions = shared_rows(&snapshot);
        let status = self.status.lock().map_err(|_| ())?.clone();
        let next_watched = sessions
            .iter()
            .map(|session| Watched {
                provider: session.provider,
                id: session.id.clone(),
                native_id: session.native_id.clone(),
                has_transcript: session.has_transcript,
                hidden_at: None,
            })
            .collect();
        let removed = snapshot
            .removed
            .iter()
            .map(|removed| {
                let local = self
                    .watched
                    .iter()
                    .find(|item| item.provider == removed.provider && item.id == removed.id)
                    .and_then(|item| item.native_id.as_deref())
                    .unwrap_or(&removed.id);
                SessionId {
                    provider: removed.provider,
                    id: opaque_id(&self.salt, removed.provider, local),
                }
            })
            .collect();
        let context = self
            .context
            .as_mut()
            .filter(|context| Instant::now() < context.expires)
            .map(|context| {
                let target = sessions
                    .iter()
                    .copied()
                    .find(|session| opaque_session_id(&self.salt, session) == context.target)
                    .and_then(|session| answer_path(session).map(|path| (session, path)));
                if let Some((session, path)) = target {
                    if context
                        .cached
                        .as_ref()
                        .is_none_or(|(updated_at, _)| *updated_at != session.updated_at)
                    {
                        let answer = if context.limit == 0 {
                            crate::transcript::last_assistant(path, session.provider).map(|text| {
                                text.into_iter()
                                    .map(|text| crate::transcript::VisibleTurn {
                                        role: Role::Assistant,
                                        text,
                                        truncated: false,
                                    })
                                    .collect()
                            })
                        } else {
                            crate::transcript::recent_visible_turns(
                                path,
                                session.provider,
                                context.limit,
                            )
                        }
                        .map_err(|_| ());
                        context.cached = Some((session.updated_at, answer));
                    }
                }
                let answer = target
                    .and(context.cached.as_ref())
                    .map(|(_, answer)| answer);
                ReplyContext {
                    id: &context.id,
                    answer: answer.filter(|_| context.limit == 0).and_then(|answer| {
                        answer
                            .as_ref()
                            .ok()?
                            .iter()
                            .rev()
                            .find(|turn| turn.role == Role::Assistant)
                            .map(|turn| turn.text.as_str())
                    }),
                    turns: answer
                        .filter(|_| context.limit > 0)
                        .and_then(|answer| answer.as_ref().ok().map(Vec::as_slice)),
                    failed: answer.is_none_or(|answer| answer.is_err()),
                }
            });
        let waiting_sessions: HashMap<_, _> = sessions
            .iter()
            .filter_map(|session| {
                session.waiting.map(|waiting| {
                    (
                        opaque_session_id(&self.salt, session),
                        (waiting, session.waiting_at.unwrap_or(0)),
                    )
                })
            })
            .collect();
        let waiting = self.waiting_sessions.as_ref().and_then(|previous| {
            sessions.iter().find_map(|session| {
                let key = opaque_session_id(&self.salt, session);
                let state = waiting_sessions.get(&key)?;
                (previous.get(&key) != Some(state)).then_some(key)
            })
        });
        let desktop_access = crate::mobile_reply::claude_desktop_access();
        let rows = sessions
            .into_iter()
            .map(|session| {
                let reason = self.dispatcher.unavailable_reason(session, desktop_access);
                Preview::from_session(
                    session,
                    &self.salt,
                    status.reply_enabled && reason.is_none(),
                    reason,
                )
            })
            .collect();
        let partial = snapshot.sources.iter().any(|source| {
            matches!(
                source.status,
                crate::model::SourceStatus::Partial | crate::model::SourceStatus::Error
            )
        });
        let payload = serde_json::to_vec(&MobileSnapshot {
            sessions: rows,
            refreshed_at: snapshot.refreshed_at,
            partial,
            sources: &snapshot.sources,
            removed,
            device_label: &status.label,
            permit: status.permit.as_deref(),
            large_attachments: true,
            max_attachment_bytes: MAX_TOTAL_BYTES,
            receipt: self.receipts.back(),
            receipts: &self.receipts,
            context,
        })
        .map_err(|_| ())?;
        let nonce = random::<12>()?;
        let mut envelope = nonce.to_vec();
        envelope.extend(
            channel
                .snapshot
                .encrypt(Nonce::from_slice(&nonce), payload.as_slice())
                .map_err(|_| ())?,
        );
        self.relay
            .client
            .put(self.relay.url("snapshot"))
            .bearer_auth(&self.relay.host)
            .json(&serde_json::json!({ "sealed": B64.encode(envelope), "waiting": waiting }))
            .send()
            .and_then(|response| response.error_for_status())
            .map_err(|_| ())?;
        self.watched = next_watched;
        self.waiting_sessions = Some(waiting_sessions);
        Ok(())
    }
}

fn answer_path(session: &Session) -> Option<&Path> {
    if session.context_only {
        return None;
    }
    session.transcript_path.as_deref()
}

fn shared_rows(snapshot: &Snapshot) -> Vec<&Session> {
    let mut sessions: Vec<_> = snapshot.sessions.iter().collect();
    sessions.sort_by_key(|s| {
        (
            s.waiting.is_none(),
            s.active != Some(true) && s.background == 0,
        )
    });
    sessions.truncate(MAX_ROWS);
    sessions
}

fn open_reply(cipher: &Aes256Gcm, sealed: &str) -> Option<Reply> {
    open_envelope(cipher, sealed, MAX_SEALED_REPLY)
}

fn open_envelope<T: DeserializeOwned>(cipher: &Aes256Gcm, sealed: &str, limit: usize) -> Option<T> {
    if sealed.len() > limit {
        return None;
    }
    let bytes = B64.decode(sealed).ok()?;
    let nonce = bytes.get(..12)?;
    let text = cipher
        .decrypt(Nonce::from_slice(nonce), bytes.get(12..)?)
        .ok()?;
    serde_json::from_slice(&text).ok()
}

fn decrypt_pin(cipher: &Aes256Gcm, sealed: &str) -> Option<Vec<u8>> {
    let bytes = B64.decode(sealed).ok()?;
    if bytes.len() != 34 {
        return None;
    }
    cipher
        .decrypt(Nonce::from_slice(&bytes[..12]), &bytes[12..])
        .ok()
}

fn random<const N: usize>() -> Result<[u8; N], ()> {
    let mut bytes = [0; N];
    getrandom::fill(&mut bytes).map_err(|_| ())?;
    Ok(bytes)
}

fn random_pin() -> Result<String, ()> {
    // Rejection sampling gives all one million PINs the same probability.
    loop {
        let value = u32::from_ne_bytes(random::<4>()?);
        if value < 4_294_000_000 {
            return Ok(format!("{:06}", value % 1_000_000));
        }
    }
}

fn relay_error() -> String {
    "중계 서버에 연결할 수 없습니다. 서버 실행 상태와 주소를 확인해 주세요.".into()
}

pub fn is_local_preview(value: &str) -> bool {
    Url::parse(value.trim())
        .is_ok_and(|url| url.scheme() == "http" && url.host_str() == Some("127.0.0.1"))
}

fn valid_token(value: &str) -> bool {
    B64.decode(value).is_ok_and(|bytes| bytes.len() == 32)
}

pub fn remote_status(value: &str) -> bool {
    relay_url(value)
        .ok()
        .filter(|url| url.scheme() == "https")
        .and_then(|url| relay_client().ok()?.get(url).send().ok())
        .filter(|response| response.status().is_success())
        .is_some_and(|response| {
            let mut html = String::new();
            response.take(4096).read_to_string(&mut html).is_ok()
                && html.contains("<title>Offtab</title>")
        })
}

fn storage_error() -> String {
    "휴대폰 연결 정보를 macOS 키체인에 저장하지 못했습니다.".into()
}

fn relay_client() -> Result<Client, String> {
    Client::builder()
        .timeout(Duration::from_secs(4))
        .redirect(redirect::Policy::none())
        .build()
        .map_err(|_| relay_error())
}

pub fn relay_url(value: &str) -> Result<Url, String> {
    let url = Url::parse(value.trim()).map_err(|_| relay_error())?;
    let loopback = url.host_str() == Some("127.0.0.1");
    if (url.scheme() == "https" || (url.scheme() == "http" && loopback))
        && url.host_str().is_some()
        && url.path() == "/"
        && url.query().is_none()
        && url.fragment().is_none()
        && url.username().is_empty()
        && url.password().is_none()
    {
        return Ok(url);
    }
    Err("중계 서버 주소는 HTTPS 또는 이 Mac의 로컬 주소여야 합니다.".into())
}

fn opaque_id(salt: &[u8], provider: Provider, local_id: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(salt);
    hash.update(match provider {
        Provider::Codex => b"codex:",
        Provider::Claude => b"claude",
    });
    hash.update(local_id.as_bytes());
    B64.encode(&hash.finalize()[..16])
}

fn opaque_session_id(salt: &[u8], session: &Session) -> String {
    opaque_id(
        salt,
        session.provider,
        session.native_id.as_deref().unwrap_or(&session.id),
    )
}

fn excerpt(value: &str, limit: usize) -> String {
    // Keep the relay payload bounded even for long Unicode text or control characters.
    value
        .chars()
        .filter(|c| !c.is_control() || *c == '\n')
        .take(limit)
        .collect()
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Preview {
    provider: Provider,
    id: String,
    parent_native_id: Option<String>,
    title: String,
    preview: String,
    role: Option<Role>,
    cwd: String,
    updated_at: i64,
    waiting: Option<Waiting>,
    active: Option<bool>,
    background: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    waiting_at: Option<i64>,
    activity_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_user_at: Option<i64>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    recently_active: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    context_only: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    read: Option<ReadStatus>,
    can_reply: bool,
    can_read_answer: bool,
    can_attach: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    reply_unavailable: Option<crate::mobile_reply::UnavailableReason>,
}

impl Preview {
    fn from_session(
        session: &Session,
        salt: &[u8],
        can_reply: bool,
        reply_unavailable: Option<crate::mobile_reply::UnavailableReason>,
    ) -> Self {
        let id = |source| opaque_id(salt, session.provider, source);
        Self {
            provider: session.provider,
            id: opaque_session_id(salt, session),
            parent_native_id: session.parent_native_id.as_deref().map(id),
            title: excerpt(&session.title, 120),
            preview: excerpt(&session.preview, 220),
            role: session.role,
            cwd: excerpt(
                Path::new(&session.cwd)
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or_default(),
                64,
            ),
            updated_at: session.updated_at,
            waiting: session.waiting,
            active: session.active,
            background: session.background,
            waiting_at: session.waiting_at,
            activity_at: session.activity_at,
            last_user_at: session.last_user_at,
            recently_active: session.recently_active,
            context_only: session.context_only,
            read: session.read.clone(),
            can_reply,
            can_read_answer: answer_path(session).is_some(),
            can_attach: can_reply,
            reply_unavailable,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct MobileSnapshot<'a> {
    sessions: Vec<Preview>,
    refreshed_at: i64,
    partial: bool,
    sources: &'a [Source],
    #[serde(skip_serializing_if = "Vec::is_empty")]
    removed: Vec<SessionId>,
    device_label: &'a str,
    permit: Option<&'a str>,
    large_attachments: bool,
    max_attachment_bytes: usize,
    receipt: Option<&'a Receipt>,
    #[serde(skip_serializing_if = "VecDeque::is_empty")]
    receipts: &'a VecDeque<Receipt>,
    #[serde(skip_serializing_if = "Option::is_none")]
    context: Option<ReplyContext<'a>>,
}

pub fn scope(watched: &[Watched]) -> ListScope {
    ListScope {
        mode: Mode::Compact,
        watched: watched.to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bulk_reply_permission_changes_current_devices_only() {
        let make = |id: &str, phase| Connection {
            relay: Arc::new(Relay {
                client: Client::new(),
                endpoint: Url::parse("http://127.0.0.1/").unwrap(),
                host: String::new(),
            }),
            status: Arc::new(Mutex::new(Status {
                id: id.into(),
                label: id.into(),
                permit: None,
                reply_enabled: false,
                phase,
                url: None,
                pin: None,
                interrupted: false,
            })),
            worker: None,
        };
        let store = DeviceStore::memory();
        store
            .update(|devices| {
                for name in ["one", "two"] {
                    devices.push(SavedDevice {
                        id: name.into(),
                        label: name.into(),
                        host: "test".into(),
                        keys: [[42; 32]; 3],
                        accepted_request: "test".into(),
                        revoking: false,
                        permit: None,
                        last_send: None,
                        recent_sends: Vec::new(),
                    });
                }
                Ok(())
            })
            .unwrap();
        let mut mobile = Mobile {
            connections: vec![
                make("one", Phase::Paired),
                make("two", Phase::Paired),
                make("waiting", Phase::Pin),
            ],
            store: Some(store.clone()),
            startup_error: None,
            reads: ReadStore::default(),
        };
        let status = mobile
            .allow_reply(ReplyTargets::Device("one"), true)
            .unwrap();
        assert!(status[0].reply_enabled);
        assert!(!status[1].reply_enabled);
        let saved = store.list().unwrap();
        assert_eq!(saved[0].permit, status[0].permit);
        assert!(saved[1].permit.is_none());
        let status = mobile.allow_reply(ReplyTargets::All, true).unwrap();
        assert!(status[0].reply_enabled && status[1].reply_enabled);
        assert_ne!(status[0].permit, status[1].permit);
        assert!(!status[2].reply_enabled);
        assert!(store
            .list()
            .unwrap()
            .iter()
            .all(|device| device.permit.is_some()));
        let status = mobile.allow_reply(ReplyTargets::All, false).unwrap();
        assert!(status
            .iter()
            .all(|device| !device.reply_enabled && device.permit.is_none()));
        assert!(store
            .list()
            .unwrap()
            .iter()
            .all(|device| device.permit.is_none()));
    }

    fn sample_session(id: &str, path: &str) -> Session {
        Session {
            id: id.into(),
            provider: Provider::Codex,
            cwd: path.into(),
            title: id.into(),
            preview: String::new(),
            role: None,
            updated_at: 0,
            active: Some(false),
            waiting: None,
            waiting_at: None,
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
    fn paired_read_only_device_can_request_shared_markdown_but_cannot_send() {
        let relay = Relay {
            client: Client::new(),
            endpoint: Url::parse("http://127.0.0.1/").unwrap(),
            host: String::new(),
        };
        let status = Mutex::new(Status {
            id: "phone".into(),
            label: "Phone".into(),
            permit: None,
            reply_enabled: false,
            phase: Phase::Paired,
            url: None,
            pin: None,
            interrupted: false,
        });
        let key = Aes256Gcm::new((&[42; 32]).into());
        let mut bridge = Bridge {
            relay: &relay,
            status: &status,
            store: None,
            reads: ReadStore::default(),
            secret: None,
            salt: vec![1; 32],
            deadline: None,
            pin: None,
            channel: Some(Channel::from_keys([[42; 32]; 3])),
            attempts: 0,
            decision: None,
            dispatcher: Dispatcher::new(Path::new("/example")),
            reply_job: None,
            last_published: None,
            seen_replies: HashMap::new(),
            receipts: VecDeque::new(),
            context: None,
            watched: Vec::new(),
            waiting_sessions: None,
        };
        let mut target = sample_session("target", "/example");
        target.transcript_path = Some("/example/session.jsonl".into());
        let target_id = opaque_session_id(&bridge.salt, &target);
        let mut snapshot = Snapshot {
            sessions: vec![target],
            sources: vec![],
            removed: vec![],
            refreshed_at: 1000,
        };
        let envelope = |kind: &str, target: &str| {
            let id = "00000000-0000-4000-8000-000000000001";
            let request = serde_json::json!({
                "id": id, "kind": kind, "target": target, "sentAt": 1000,
                "updatedAt": 0, "text": "example",
            });
            let nonce = random::<12>().unwrap();
            let mut sealed = nonce.to_vec();
            sealed.extend(
                key.encrypt(Nonce::from_slice(&nonce), request.to_string().as_bytes())
                    .unwrap(),
            );
            SealedReply {
                id: id.into(),
                sealed: B64.encode(sealed),
            }
        };
        assert!(matches!(
            bridge.try_reply(&envelope("context", &target_id), &snapshot, 1000, false),
            ResultCode::Context
        ));
        assert!(matches!(
            bridge.try_reply(&envelope("send", &target_id), &snapshot, 1000, false),
            ResultCode::Blocked
        ));
        assert!(matches!(
            bridge.try_reply(&envelope("context", "unshared"), &snapshot, 1000, false),
            ResultCode::Unavailable
        ));
        snapshot.sessions[0].context_only = true;
        assert!(matches!(
            bridge.try_reply(&envelope("context", &target_id), &snapshot, 1000, false),
            ResultCode::Unavailable
        ));
        assert!(bridge.reply_job.is_none());
        bridge.last_published = Some(Instant::now());
        bridge.decision = Some(("pin".into(), true));
        let idle = RelayState {
            phase: Phase::Paired,
            public_key: None,
            request: None,
            reply: None,
            context: None,
            read: None,
        };
        assert!(matches!(
            bridge.sync(idle, &mut |_| panic!("idle request polling must not collect sessions")),
            Ok(Some(delay)) if delay == REQUEST_POLL
        ));
    }

    #[test]
    fn mobile_reads_share_the_mac_timestamp_and_only_match_the_current_revision() {
        let relay = Relay {
            client: Client::new(),
            endpoint: Url::parse("http://127.0.0.1:0/").unwrap(),
            host: String::new(),
        };
        let status = Mutex::new(Status {
            id: "phone".into(),
            label: "Phone".into(),
            permit: None,
            reply_enabled: false,
            phase: Phase::Paired,
            url: None,
            pin: None,
            interrupted: false,
        });
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("reads.json");
        let reads = ReadStore::load(path.clone());
        let mut bridge = Bridge::new(
            &relay,
            &status,
            None,
            vec![1; 32],
            Dispatcher::new(Path::new("/example")),
            reads.clone(),
        );
        let channel = Channel::from_keys([[42; 32]; 3]);
        let clock = epoch(SystemTime::now());
        let at = clock - 1000;
        let mut snapshot = Snapshot {
            sessions: vec![sample_session(
                "00000000-0000-4000-8000-000000000004",
                "/example",
            )],
            removed: vec![],
            sources: vec![Source {
                provider: Provider::Codex,
                status: crate::model::SourceStatus::Ready,
            }],
            refreshed_at: clock,
        };
        let target = opaque_session_id(&bridge.salt, &snapshot.sessions[0]);
        let make_read = |version: &str, at: i64| {
            let id = "00000000-0000-4000-8000-000000000001";
            let request = serde_json::json!({ "id": id, "sentAt": clock, "reads": [{ "id": target, "version": version, "at": at }] });
            let nonce = random::<12>().unwrap();
            let mut sealed = nonce.to_vec();
            sealed.extend(
                channel
                    .reply
                    .encrypt(Nonce::from_slice(&nonce), request.to_string().as_bytes())
                    .unwrap(),
            );
            SealedReply {
                id: id.into(),
                sealed: B64.encode(sealed),
            }
        };
        bridge.channel = Some(Channel::from_keys([[42; 32]; 3]));
        bridge
            .handle_read(make_read("0:1:0", at), &snapshot)
            .unwrap();
        reads.apply(&mut snapshot);
        let version = snapshot.sessions[0].read.as_ref().unwrap().version.clone();
        assert_eq!(snapshot.sessions[0].read.as_ref().unwrap().at, None);
        bridge
            .handle_read(make_read(&version, at), &snapshot)
            .unwrap();
        bridge
            .handle_read(make_read(&version, at + 1), &snapshot)
            .unwrap();
        ReadStore::load(path).apply(&mut snapshot);
        assert_eq!(snapshot.sessions[0].read.as_ref().unwrap().at, Some(at));
        snapshot.sessions[0].active = Some(true);
        reads.apply(&mut snapshot);
        assert!(snapshot.sessions[0].read.is_none());
        snapshot.sessions[0].active = Some(false);
        reads.apply(&mut snapshot);
        assert_eq!(snapshot.sessions[0].read.as_ref().unwrap().at, None);
    }

    #[test]
    fn stored_pairing_resumes_and_publishes_with_the_same_key_and_permission() {
        use std::io::{BufRead, BufReader, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let origin = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        let id = B64.encode([7; 32]);
        let host = B64.encode([8; 32]);
        let permit = B64.encode([9; 32]);
        let keys = [[1; 32], [2; 32], [3; 32]];
        let store = DeviceStore::memory();
        let now = epoch(SystemTime::now());
        let sent_id = "bade087d-b4c4-41d8-88f0-52840b44acd1";
        let unknown_id = "2ad6ce2b-ec3d-4241-a7de-80f2918890be";
        store
            .update(|devices| {
                devices.push(SavedDevice {
                    id: id.clone(),
                    label: "Test phone".into(),
                    host: host.clone(),
                    keys,
                    accepted_request: B64.encode([4; 32]),
                    permit: Some(permit.clone()),
                    revoking: false,
                    last_send: None,
                    recent_sends: vec![
                        SavedSend {
                            id: sent_id.into(),
                            until: now + 60_000,
                            outcome: ResultCode::Queued,
                        },
                        SavedSend {
                            id: unknown_id.into(),
                            until: now + 60_000,
                            outcome: ResultCode::Unknown,
                        },
                        SavedSend {
                            id: "expired".into(),
                            until: now - 1,
                            outcome: ResultCode::Sent,
                        },
                    ],
                });
                Ok(())
            })
            .unwrap();

        let (published, received) = mpsc::channel();
        let endpoint = format!("/api/links/{id}");
        let server = thread::spawn(move || {
            for (method, action) in [("GET", "host"), ("POST", "reply-ack"), ("PUT", "snapshot")] {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut reader = BufReader::new(&socket);
                let mut request = String::new();
                reader.read_line(&mut request).unwrap();
                assert!(request.starts_with(&format!("{method} {endpoint}/{action} ")));
                let mut length = 0;
                let mut authenticated = false;
                loop {
                    let mut line = String::new();
                    let size = reader.read_line(&mut line).unwrap();
                    if line == "\r\n" || size == 0 {
                        break;
                    }
                    let (name, value) = line.split_once(':').unwrap();
                    if name.eq_ignore_ascii_case("content-length") {
                        length = value.trim().parse().unwrap();
                    }
                    if name.eq_ignore_ascii_case("authorization") {
                        authenticated = value.trim() == format!("Bearer {host}");
                    }
                }
                assert!(authenticated);
                if method == "GET" {
                    let body = serde_json::json!({
                        "phase": "paired",
                        "reply": { "id": sent_id, "sealed": "already-consumed" }
                    })
                    .to_string();
                    write!(
                        socket,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    )
                    .unwrap();
                } else if method == "POST" {
                    let mut body = vec![0; length];
                    reader.read_exact(&mut body).unwrap();
                    let ack: serde_json::Value = serde_json::from_slice(&body).unwrap();
                    assert_eq!(ack["id"], sent_id);
                    write!(
                        socket,
                        "HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n"
                    )
                    .unwrap();
                } else {
                    let mut body = vec![0; length];
                    reader.read_exact(&mut body).unwrap();
                    let envelope: serde_json::Value = serde_json::from_slice(&body).unwrap();
                    let sealed = B64.decode(envelope["sealed"].as_str().unwrap()).unwrap();
                    let plaintext = Channel::from_keys(keys)
                        .snapshot
                        .decrypt(Nonce::from_slice(&sealed[..12]), &sealed[12..])
                        .unwrap();
                    let snapshot: serde_json::Value = serde_json::from_slice(&plaintext).unwrap();
                    write!(
                        socket,
                        "HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n"
                    )
                    .unwrap();
                    published.send(snapshot).unwrap();
                }
            }
        });
        let mut restored = Mobile::default();
        restored
            .restore_from_store(
                origin,
                store,
                Dispatcher::new(Path::new("/example")),
                |_| {
                    Ok(Snapshot {
                        sessions: vec![],
                        sources: vec![],
                        removed: vec![],
                        refreshed_at: 123,
                    })
                },
            )
            .unwrap();
        let snapshot = received.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(snapshot["refreshedAt"], 123);
        assert_eq!(snapshot["permit"], permit);
        assert_eq!(snapshot["deviceLabel"], "Test phone");
        assert_eq!(
            snapshot["receipts"],
            serde_json::json!([
                { "id": sent_id, "code": "queued" },
                { "id": unknown_id, "code": "unknown" },
            ])
        );
        assert!(restored.status()[0].reply_enabled);
        assert_eq!(restored.status()[0].id, id);
        drop(restored);
        server.join().unwrap();
    }

    #[test]
    fn revocation_is_recorded_and_retried_without_restarting_mac() {
        use std::io::{BufRead, BufReader, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let origin = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        let id = B64.encode([7; 32]);
        let store = DeviceStore::memory();
        store
            .update(|devices| {
                devices.push(SavedDevice {
                    id: id.clone(),
                    label: "Test phone".into(),
                    host: B64.encode([8; 32]),
                    keys: [[9; 32]; 3],
                    accepted_request: "test".into(),
                    permit: Some("enabled".into()),
                    revoking: false,
                    last_send: None,
                    recent_sends: Vec::new(),
                });
                Ok(())
            })
            .unwrap();

        let (sent, received) = mpsc::channel();
        let endpoint = origin.join(&format!("api/links/{id}")).unwrap();
        let path = endpoint.path().to_owned();
        let server = thread::spawn(move || {
            for status in ["503 Service Unavailable", "204 No Content"] {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut reader = BufReader::new(&socket);
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                assert!(line.starts_with(&format!("DELETE {path} ")));
                loop {
                    line.clear();
                    if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                        break;
                    }
                }
                write!(
                    socket,
                    "HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                )
                .unwrap();
                sent.send(()).unwrap();
            }
        });
        let mut mobile = Mobile {
            connections: vec![Connection {
                relay: Arc::new(Relay {
                    client: relay_client().unwrap(),
                    endpoint,
                    host: B64.encode([8; 32]),
                }),
                status: Arc::new(Mutex::new(Status {
                    id: id.clone(),
                    label: "Test phone".into(),
                    permit: Some("enabled".into()),
                    reply_enabled: true,
                    phase: Phase::Paired,
                    url: None,
                    pin: None,
                    interrupted: false,
                })),
                worker: None,
            }],
            store: Some(store.clone()),
            startup_error: None,
            reads: ReadStore::default(),
        };
        let status = mobile.stop(&id).unwrap();
        assert!(status[0].phase == Phase::Revoking && !status[0].reply_enabled);
        assert!(store.list().unwrap()[0].revoking);
        assert!(mobile.allow_reply(ReplyTargets::Device(&id), true).is_err());
        received.recv_timeout(Duration::from_secs(2)).unwrap();
        received
            .recv_timeout(SYNC_POLL + Duration::from_secs(2))
            .unwrap();
        let (_, worker) = mobile.connections[0].worker.take().unwrap();
        worker.join().unwrap();
        assert!(mobile.status().is_empty());
        assert!(store.list().unwrap().is_empty());
        server.join().unwrap();
    }

    #[test]
    fn mobile_rows_include_sessions_across_project_folders() {
        let snapshot = Snapshot {
            sessions: vec![
                sample_session("first", "/example/one"),
                sample_session("second", "/example/two"),
            ],
            sources: vec![],
            removed: vec![],
            refreshed_at: 0,
        };
        let rows = shared_rows(&snapshot);
        assert_eq!(
            rows.iter()
                .map(|session| session.cwd.as_str())
                .collect::<Vec<_>>(),
            ["/example/one", "/example/two"]
        );
    }
}
