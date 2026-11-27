import { useCallback, useEffect, useMemo, useRef, useState, type FormEvent } from 'react';
import { createRoot } from 'react-dom/client';
import { Radio } from 'lucide-react';
import { CompactList } from '../compact-list';
import { hideSession, markRead, readUpdates, restoreTracked, saveTracked, sessionKey, trackSessions, type TrackedSessions } from '../tracked-state';
import type { Session, Snapshot } from '../types';
import { forgetDevice, loadDevice, loadReplyHistory, saveReplyHistory, openSnapshot, queueReads, queueReply, scan, submitPin, view, type Device, type Receipt, type ReplyCode, type RelayView, type ReplyContext } from './connection';
import { ReplyComposer, replyLabels, type ReplyDraft, type ReplyProgress } from './reply';
import { attachmentLimit, type AttachmentCapability } from './reply-attachment';
import './app.css';

type MobileSession = Session & { canReply?: boolean; canReadAnswer?: boolean; canAttach?: boolean; desktopReply?: boolean; replyUnavailable?: keyof typeof replyUnavailableLabels };
type MobileSnapshot = Omit<Snapshot, 'sessions'> & AttachmentCapability & { sessions: MobileSession[]; partial: boolean; deviceLabel?: string; permit?: string | null; receipt?: Receipt | null; receipts?: Receipt[]; context?: ReplyContext };
type DeviceTracking = { id: string; entries: TrackedSessions };

const replyUnavailableLabels = {
  'desktop-claude': 'Offtab에서는 이 Claude 데스크톱 채팅을 조회할 수 있어요.',
  'desktop-permission': replyLabels['desktop-permission'],
  setup: 'Mac에서 이 세션에 맞는 CLI 연결을 확인해 주세요.',
  waiting: 'Mac의 원본 채팅에서 대기 중인 질문이나 승인을 먼저 처리해 주세요.',
  working: '이 세션의 작업이 끝나면 답장을 보낼 수 있어요.',
  unavailable: '이 세션은 지금 답장을 받을 수 없습니다.',
};

const emptyDraft: ReplyDraft = { text: '' };
const validProgress = new Set(Object.keys(replyLabels));
type ReplyEntry = { draft: ReplyDraft; progress?: ReplyProgress };
const replyTimeout = 145_000;
const replyConfirmationMs = 3_000;

function isReplyConfirmed(code?: ReplyCode) {
  return code === 'sent' || code === 'queued';
}

const freshness = 45_000;
const initialSyncGrace = 30_000;
const trackingKey = (id: string) => `offtab.mobile.tracked.${id}`;

function restoreMobileTracking(id: string): DeviceTracking {
  try { return { id, entries: restoreTracked(localStorage, trackingKey(id)) }; }
  catch { return { id, entries: new Map() }; }
}

// An older Mac can stay connected while the mobile page is updated.
function forTracking(payload: MobileSnapshot): Snapshot {
  if (Array.isArray(payload.sources)) return payload;
  return {
    ...payload,
    sources: [
      { provider: 'codex', status: payload.partial ? 'partial' : 'ready' },
      { provider: 'claude', status: payload.partial ? 'partial' : 'ready' },
    ],
    sessions: payload.sessions.map(session => ({
      ...session,
      recentlyActive: session.updatedAt >= payload.refreshedAt - 15_000,
    })),
  };
}

function storedReplies(value: unknown): Record<string, ReplyEntry> {
  if (!value || typeof value !== 'object') return {};
  const restored: Record<string, ReplyEntry> = {};
  for (const [key, entry] of Object.entries(value)) {
    if (!/^(codex|claude):[\w-]+$/.test(key) || !entry || typeof entry.draft?.text !== 'string') continue;
    const progress = entry.progress;
    restored[key] = {
      draft: { text: entry.draft.text.slice(0, 2_000) },
      progress: progress && typeof progress.id === 'string' && Number.isSafeInteger(progress.deadline)
        && validProgress.has(progress.code) ? progress : undefined,
    };
  }
  return restored;
}

function saveableReplies(entries: Record<string, ReplyEntry>) {
  const saved: Record<string, ReplyEntry> = {};
  for (const [key, { draft, progress }] of Object.entries(entries)) {
    const savedProgress = isReplyConfirmed(progress?.code) ? undefined : progress;
    if (draft.text || savedProgress) saved[key] = { draft: { text: draft.text }, progress: savedProgress };
  }
  return saved;
}

function App() {
  const [device, setDevice] = useState<Device>();
  const [invite, setInvite] = useState(() => location.hash);
  const [inviteRetry, setInviteRetry] = useState(0);
  const [relay, setRelay] = useState<RelayView>();
  const [selectedSession, setSelectedSession] = useState<MobileSession>();
  const [snapshot, setSnapshot] = useState<MobileSnapshot>();
  const [tracking, setTracking] = useState<DeviceTracking>();
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState('');
  const [online, setOnline] = useState(false);
  const [syncWaiting, setSyncWaiting] = useState(true);
  const [pin, setPin] = useState('');
  const [sending, setSending] = useState(false);
  const [pinError, setPinError] = useState('');
  const lastSealed = useRef('');
  const lastRefresh = useRef(0);
  const sentAt = useRef<number | null>(null);
  const currentDevice = useRef<string | undefined>(undefined);
  const wakeSync = useRef<(() => void) | undefined>(undefined);
  const fastSync = useRef(false);
  const [replies, setReplies] = useState<Record<string, ReplyEntry>>({});
  const [storageError, setStorageError] = useState(false);
  const [upload, setUpload] = useState<{ id: string; percent: number }>();
  const [lastSynced, setLastSynced] = useState(0);
  const [query, setQuery] = useState('');
  const [pairingAddress, setPairingAddress] = useState('');
  const fromPush = useRef(new URLSearchParams(location.search).get('waiting'));
  const [lastHidden, setLastHidden] = useState<string>();
  const [showHidden, setShowHidden] = useState(false);
  const persistence = useRef(Promise.resolve());
  const savedReplies = useRef<{ id: string; entries: Record<string, ReplyEntry> } | undefined>(undefined);
  const saveReplies = useCallback((target: Device, entries: Record<string, ReplyEntry>) => {
    const readable = saveableReplies(entries);
    const prior = savedReplies.current?.id === target.id ? savedReplies.current.entries : {};
    const changes: Record<string, object | null> = {};
    for (const key of new Set([...Object.keys(readable), ...Object.keys(prior)])) {
      if (prior[key]?.draft.text === readable[key]?.draft.text && prior[key]?.progress === readable[key]?.progress) continue;
      changes[key] = readable[key] ?? null;
    }
    savedReplies.current = { id: target.id, entries: readable };
    if (!Object.keys(changes).length) return persistence.current;
    const save = persistence.current.catch(() => {}).then(() => saveReplyHistory(target, changes));
    persistence.current = save;
    void save.catch(() => {
      if (currentDevice.current === target.id) setStorageError(true);
    });
    return save;
  }, []);
  const sendingReply = useRef<{ key: string; id: string } | undefined>(undefined);
  const syncingReads = useRef(false);
  const answers = useRef(new Map<string, { body: string; version: number }>());
  const waitForDetail = useCallback((waiting: boolean) => {
    fastSync.current = waiting;
    if (waiting) wakeSync.current?.();
  }, []);
  const rememberAnswer = useCallback((session: Session, body: string) => {
    const cache = answers.current;
    const key = sessionKey(session);
    cache.delete(key);
    cache.set(key, { body, version: session.updatedAt });
    if (cache.size > 8) {
      const oldest = cache.keys().next();
      if (!oldest.done) cache.delete(oldest.value);
    }
  }, []);

  useEffect(() => {
    const checkInvite = () => { if (location.hash) setInvite(location.hash); };
    window.addEventListener('hashchange', checkInvite);
    window.addEventListener('pageshow', checkInvite);
    document.addEventListener('visibilitychange', checkInvite);
    checkInvite();
    return () => {
      window.removeEventListener('hashchange', checkInvite);
      window.removeEventListener('pageshow', checkInvite);
      document.removeEventListener('visibilitychange', checkInvite);
    };
  }, []);

  useEffect(() => {
    const request = new AbortController();
    setLoading(true);
    if (invite) {
      currentDevice.current = undefined;
      answers.current.clear();
      sendingReply.current = undefined;
      setReplies({});
      setLastSynced(0);
      setStorageError(false);
      setDevice(undefined);
      setTracking(undefined);
      setRelay(undefined);
      setSnapshot(undefined);
      setSelectedSession(undefined);
      setError('');
      setOnline(false);
      setSyncWaiting(true);
      setPin('');
      setPinError('');
      setSending(false);
      lastSealed.current = '';
      lastRefresh.current = 0;
      sentAt.current = null;
    }
    const stillCurrent = () => !request.signal.aborted && location.hash === invite;
    const stored = invite ? scan(invite, request.signal) : loadDevice();
    void stored.then(async value => {
      if (!stillCurrent()) return;
      if (value) {
        try {
          const saved = storedReplies(await loadReplyHistory(value));
          if (!stillCurrent()) return;
          savedReplies.current = { id: value.id, entries: saved };
          setReplies(saveableReplies(saved));
          const pending = Object.entries(saved).find(([, entry]) => entry.progress?.code === 'processing');
          if (pending?.[1].progress) sendingReply.current = { key: pending[0], id: pending[1].progress.id };
        } catch {
          if (!stillCurrent()) return;
          setStorageError(true);
        }
        setTracking(restoreMobileTracking(value.id));
      }
      if (!stillCurrent()) return;
      if (invite) history.replaceState(null, '', location.pathname);
      currentDevice.current = value?.id;
      setDevice(value);
    }).catch(failure => {
      if (stillCurrent()) setError(failure instanceof Error ? failure.message : '연결 정보를 열 수 없습니다.');
    }).finally(() => {
      if (!request.signal.aborted && (!location.hash || location.hash === invite)) setLoading(false);
    });
    return () => request.abort();
  }, [invite, inviteRetry]);

  useEffect(() => {
    if (!device) return;
    const pairedDevice = device;
    let disposed = false;
    let timer: ReturnType<typeof setTimeout>;
    let emptySince: number | undefined;
    let refreshing = false;
    let refreshRequested = false;
    let version: string | undefined;
    let failures = 0;
    const pairedDelay = () => document.hidden ? 15_000 : fastSync.current || sendingReply.current ? 650 : 4_000;

    async function refresh() {
      if (disposed || location.hash || refreshing) return;
      refreshing = true;
      let delay = 1_100;
      try {
        const update = await view(pairedDevice, version);
        if (disposed || location.hash) return;
        if (!update) {
          currentDevice.current = undefined;
          answers.current.clear();
          sendingReply.current = undefined;
          setReplies({});
          setLastSynced(0);
          setDevice(undefined);
          setTracking(undefined);
          try { localStorage.removeItem(trackingKey(pairedDevice.id)); } catch {}
          setRelay(undefined);
          setSnapshot(undefined);
          setSelectedSession(undefined);
          setError('연결이 종료되었습니다. Mac에서 새 QR을 찍으세요.');
          void forgetDevice(pairedDevice.id).catch(() => {});
          return;
        }
        failures = 0;
        setError('');
        if (update.unchanged) {
          delay = pairedDelay();
          setOnline(Date.now() - lastRefresh.current <= freshness);
          return;
        }
        const next = update.state;
        const sealed = next.snapshot;
        version = undefined;
        setRelay(previous => previous && previous.phase === next.phase &&
          previous.attempts === next.attempts && previous.pending === next.pending &&
          previous.expiresAt === next.expiresAt && previous.sync === next.sync && previous.readSync === next.readSync
          && attachmentLimit(previous) === attachmentLimit(next)
          ? previous : { ...next, snapshot: undefined });
        if (!sealed) setOnline(false);
        if (next.phase === 'paired') {
          delay = sealed ? pairedDelay() : document.hidden ? 15_000 : 4_000;
          if (sealed) {
            emptySince = undefined;
            setSyncWaiting(false);
          } else {
            const receivedAt = performance.now();
            emptySince ??= receivedAt;
            setSyncWaiting(lastRefresh.current === 0 &&
              (next.sync === 'waiting' || (next.sync === undefined && receivedAt - emptySince < initialSyncGrace)));
          }
          setPin('');
          setPinError('');
          if (sealed && sealed !== lastSealed.current) {
            const payload: MobileSnapshot = await openSnapshot(pairedDevice, sealed);
            if (disposed || location.hash) return;
            if (!Number.isSafeInteger(payload.refreshedAt) || payload.refreshedAt > Date.now() + freshness
              || payload.refreshedAt < lastRefresh.current || !Array.isArray(payload.sessions)) {
              setOnline(false);
              setError('이전 세션 목록을 수신했습니다. 새 갱신을 기다립니다.');
            } else {
              lastRefresh.current = payload.refreshedAt;
              setLastSynced(Date.now());
              setOnline(Date.now() - payload.refreshedAt <= freshness);
              lastSealed.current = sealed;
              version = update.version;
              setSnapshot(payload);
              setTracking(previous => {
                const sessionState = previous?.id === pairedDevice.id ? previous : restoreMobileTracking(pairedDevice.id);
                return { id: pairedDevice.id, entries: trackSessions(sessionState.entries, forTracking(payload)) };
              });
            }
          } else if (sealed) {
            version = update.version;
            setOnline(Date.now() - lastRefresh.current <= freshness);
          }
        }
        if (sentAt.current !== null && next.attempts > sentAt.current && !next.pending) {
          sentAt.current = null;
          if (next.phase === 'pin') setPinError(`PIN이 맞지 않습니다. ${5 - next.attempts}회 남았습니다.`);
        }
      } catch {
        if (!disposed && !location.hash) {
          setOnline(false);
          setSyncWaiting(false);
          setError('중계 서버에 연결할 수 없습니다. 연결되면 다시 불러옵니다.');
          delay = Math.min(30_000, 4_000 * 2 ** failures);
          failures = Math.min(3, failures + 1);
        }
      } finally {
        refreshing = false;
        if (!disposed && !location.hash && currentDevice.current === pairedDevice.id) {
          timer = setTimeout(refresh, refreshRequested ? 0 : delay);
        }
        refreshRequested = false;
      }
    }

    function reconnect() {
      failures = 0;
      clearTimeout(timer);
      if (refreshing) refreshRequested = true;
      else void refresh();
    }

    const onVisible = () => { if (!document.hidden) reconnect(); };
    wakeSync.current = reconnect;
    window.addEventListener('online', reconnect);
    window.addEventListener('pageshow', reconnect);
    document.addEventListener('visibilitychange', onVisible);
    void refresh();
    return () => {
      disposed = true;
      wakeSync.current = undefined;
      clearTimeout(timer);
      window.removeEventListener('online', reconnect);
      window.removeEventListener('pageshow', reconnect);
      document.removeEventListener('visibilitychange', onVisible);
    };
  }, [device]);

  useEffect(() => {
    if (!tracking || tracking.id !== device?.id) return;
    try { saveTracked(localStorage, tracking.entries, trackingKey(tracking.id)); } catch {}
  }, [device, tracking]);

  useEffect(() => {
    if (!device?.replyKey || !relay?.readSync || !online || tracking?.id !== device.id || syncingReads.current) return;
    const updates = readUpdates(tracking.entries);
    if (!updates.length) return;
    syncingReads.current = true;
    void queueReads(device, updates).catch(() => {}).finally(() => { syncingReads.current = false; });
  }, [device, online, relay?.readSync, tracking]);

  useEffect(() => {
    if (device && currentDevice.current === device.id && !storageError) void saveReplies(device, replies);
  }, [device, replies, storageError, saveReplies]);

  async function connect(event: FormEvent) {
    event.preventDefault();
    if (!device || !relay || sending || relay.pending || sentAt.current !== null || !/^\d{6}$/.test(pin)) return;
    setSending(true);
    setPinError('');
    sentAt.current = relay.attempts;
    try {
      await submitPin(device, pin);
      if (currentDevice.current === device.id) setPin('');
    } catch (failure) {
      if (currentDevice.current !== device.id) return;
      sentAt.current = null;
      setPinError(failure instanceof Error ? failure.message : 'PIN을 전달하지 못했습니다.');
    } finally {
      if (currentDevice.current === device.id) setSending(false);
    }
  }

  function read(session: Session) {
    setTracking(previous => previous && {
      ...previous, entries: markRead(previous.entries, sessionKey(session), Date.now(), true),
    });
  }

  function hide(session: Session) {
    setLastHidden(sessionKey(session));
    setTracking(previous => previous && {
      ...previous, entries: hideSession(previous.entries, session, Date.now()),
    });
  }

  function undoHide(key: string) {
    setTracking(previous => {
      const entry = previous?.entries.get(key);
      return previous && entry ? { ...previous, entries: new Map(previous.entries).set(key, { ...entry, hiddenAt: undefined }) } : previous;
    });
    setLastHidden(undefined);
  }

  const visibleEntries = useMemo(() => {
    const entries = tracking?.entries ?? new Map();
    const text = query.trim().toLocaleLowerCase();
    if (!text) return entries;
    return new Map([...entries].filter(([, { session }]) =>
      [session.title, session.cwd, session.preview].some(value => value.toLocaleLowerCase().includes(text))));
  }, [tracking, query]);
  const hiddenEntries = [...(tracking?.entries ?? [])].filter(([, entry]) => entry.hiddenAt !== undefined && !entry.restoring);

  const finishReply = useCallback((key: string, id: string, code: ReplyCode, retryable = false) => {
    if (code === 'processing') return;
    if (sendingReply.current?.id === id) sendingReply.current = undefined;
    const confirmed = isReplyConfirmed(code);
    const confirmationDeadline = Date.now() + replyConfirmationMs;
    setReplies(previous => {
      const entry = previous[key];
      if (entry?.progress?.id !== id || (entry.progress.code === code && Boolean(entry.progress.retryable) === retryable) ||
        (entry.progress.code !== 'processing' && entry.progress.code !== 'unknown')) return previous;
      return { ...previous, [key]: {
        draft: confirmed ? emptyDraft : entry.draft,
        progress: { ...entry.progress, code, retryable, deadline: confirmed ? confirmationDeadline : entry.progress.deadline },
      } };
    });
  }, []);

  useEffect(() => {
    const timers: ReturnType<typeof setTimeout>[] = [];
    for (const [key, { progress }] of Object.entries(replies)) {
      if (!progress || !isReplyConfirmed(progress.code)) continue;
      timers.push(setTimeout(() => setReplies(previous => {
        const entry = previous[key];
        if (entry?.progress !== progress) return previous;
        return { ...previous, [key]: { draft: entry.draft } };
      }), Math.max(0, progress.deadline - Date.now())));
    }
    return () => timers.forEach(timer => clearTimeout(timer));
  }, [replies]);

  const pendingReply = Object.entries(replies).find(([, entry]) => entry.progress?.code === 'processing');
  const pendingKey = pendingReply?.[0];
  const pendingProgress = pendingReply?.[1].progress;
  useEffect(() => {
    if (!pendingKey || !pendingProgress) return;
    const timer = setTimeout(() => finishReply(pendingKey, pendingProgress.id, 'unknown'),
      Math.max(0, pendingProgress.deadline - Date.now()));
    return () => clearTimeout(timer);
  }, [finishReply, pendingKey, pendingProgress]);

  const receipt = snapshot?.receipt;
  const receipts = snapshot?.receipts;
  useEffect(() => {
    const results = receipts ?? (receipt ? [receipt] : []);
    for (const [key, { progress }] of Object.entries(replies)) {
      if (!progress || (progress.code !== 'processing' && progress.code !== 'unknown')) continue;
      const result = results.find(result => result.id === progress.id);
      if (result && (result.code !== progress.code || progress.retryable)) finishReply(key, result.id, result.code);
    }
  }, [finishReply, receipt, receipts, replies]);

  const latestRows = useMemo(() => new Map(snapshot?.sessions.map(session => [sessionKey(session), session])), [snapshot]);
  const parents = useMemo(() => snapshot?.sessions.filter(session => session.provider === 'claude') ?? [], [snapshot]);
  useEffect(() => {
    if (!fromPush.current || !snapshot) return;
    const target = snapshot.sessions.find(session => session.id === fromPush.current);
    fromPush.current = null;
    history.replaceState(null, '', location.pathname);
    if (target) setSelectedSession(target);
  }, [snapshot]);
  const checking = sending || relay?.pending || sentAt.current !== null;
  const permit = device?.replyKey ? snapshot?.permit : undefined;
  const latestTarget = selectedSession && latestRows.get(sessionKey(selectedSession));
  const modalSession = latestTarget ?? selectedSession;
  const replyKey = modalSession && sessionKey(modalSession);
  const replyEntry = replyKey ? replies[replyKey] : undefined;
  const cachedAnswer = modalSession ? answers.current.get(sessionKey(modalSession)) : undefined;
  // The original Mac bridge accepted answer requests only with a reply permit.
  const canLoadAnswer = Boolean(online && device?.replyKey && latestTarget &&
    (latestTarget.canReadAnswer ?? (permit && !latestTarget.contextOnly)));
  const canSendTo = (session: Session) => Boolean(online && permit && latestRows.get(sessionKey(session))?.canReply);
  const reason = modalSession?.replyUnavailable;
  const replyUnavailable = reason === 'desktop-claude'
    ? <>{replyUnavailableLabels[reason]} Remote Control을 켜둔 채팅은 <a href="https://claude.ai/code"
        target="_blank" rel="noopener noreferrer">Claude Code에서 이어가기</a>로 열 수 있어요.</>
    : !online ? 'Mac에 다시 연결되면 답장을 보낼 수 있어요.'
    : !snapshot?.permit ? 'Mac Offtab에서 이 기기의 답장을 허용해 주세요.'
      : !permit ? '답장을 사용하려면 Mac에서 새 QR로 연결해 주세요.'
        : !latestTarget?.canReply ? (reason && replyUnavailableLabels[reason]) ?? replyUnavailableLabels.unavailable : undefined;

  function updateDraft(key: string, draft: ReplyDraft) {
    setReplies(previous => ({ ...previous, [key]: { ...previous[key], draft } }));
  }

  async function sendReply(target: Session, key: string, retry = false) {
    const entry = replies[key];
    if (!device || !permit || !entry || sendingReply.current || !canSendTo(target)
      || (!retry && entry.progress?.code === 'unknown')) return;
    const attachments = entry.draft.attachments ?? [];
    const previous = entry.progress;
    const startedAt = retry ? previous?.startedAt : Date.now();
    if (!startedAt || (retry && (!previous?.retryable || !attachments.length || Date.now() >= startedAt + 120_000))) return;
    const id = retry && previous ? previous.id : crypto.randomUUID();
    sendingReply.current = { key, id };
    const progress: ReplyProgress = { id, startedAt, code: 'processing', deadline: Date.now() + replyTimeout + (attachments.length ? 100_000 : 0) };
    const next = { ...replies, [key]: { draft: entry.draft, progress } };
    setReplies(next);
    setUpload(attachments.length ? { id, percent: 0 } : undefined);
    wakeSync.current?.();
    try {
      if (!storageError) await saveReplies(device, next).catch(() => {});
      if (currentDevice.current !== device.id) return;
      const code = await queueReply(device, id, target, entry.draft.text.trim(), permit, attachments, startedAt,
        percent => setUpload(previous => previous?.id === id ? { id, percent } : previous));
      if (code && currentDevice.current === device.id) finishReply(key, id, code);
    } catch {
      if (currentDevice.current === device.id) finishReply(key, id, 'unknown', attachments.length > 0);
    } finally {
      setUpload(previous => previous?.id === id ? undefined : previous);
    }
  }

  function openRow(session: Session) {
    read(session);
    setSelectedSession(latestRows.get(sessionKey(session)) ?? session);
  }

  function openInvitation(event: FormEvent) {
    event.preventDefault();
    setError('');
    try {
      const url = new URL(pairingAddress.trim());
      if (url.origin !== location.origin || !url.hash || url.pathname !== '/') throw new Error();
      if (location.hash === url.hash) setInviteRetry(value => value + 1);
      else {
        history.replaceState(null, '', url.pathname + url.hash);
        setInvite(url.hash);
      }
      setPairingAddress('');
    } catch {
      setError('같은 Offtab 주소에서 Mac이 만든 연결 주소를 붙여넣으세요.');
    }
  }

  const paired = relay?.phase === 'paired';
  const connectionLabel = online ? 'Mac 연결됨' : error ? '중계 연결 끊김' : syncWaiting ? '동기화 대기' : 'Mac 응답 대기';

  return <div className={paired ? 'mobile-site mobile-compact' : 'mobile-site'}>
    {paired ? <>
      <header className="compact-toolbar mobile-toolbar">
        <img src="/offtab.png" alt="Offtab" width="18" height="18" />
        <h1>진행 중 세션</h1>
        <span className="connection-state" data-online={online} role="status"
          aria-label={connectionLabel} title={snapshot?.deviceLabel ? `${snapshot.deviceLabel} · ${connectionLabel}` : connectionLabel}>
          <Radio aria-hidden="true" />
          <span>{connectionLabel}</span>
        </span>
      </header>
      <div className="mobile-list-tools">
        <input type="search" aria-label="세션 검색" placeholder="세션 검색" value={query} onChange={event => setQuery(event.currentTarget.value)} />
        {lastSynced > 0 && <time dateTime={new Date(lastSynced).toISOString()}>마지막 갱신 {new Date(lastSynced).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' })}</time>}
      </div>
      {storageError && <p className="notice" role="status">브라우저에 초안을 저장하지 못했습니다. 이 화면에서 계속 작성할 수 있습니다.</p>}
      {error && <p className="notice" role="status">{error}</p>}
      {lastHidden && <p className="notice" role="status">목록에서 제외했습니다. <button onClick={() => undoHide(lastHidden)}>실행 취소</button></p>}
      {hiddenEntries.length > 0 && <div className="mobile-hidden-tools">
        <button type="button" aria-expanded={showHidden} onClick={() => setShowHidden(value => !value)}>제외한 세션 {hiddenEntries.length}개</button>
        {showHidden && <ul>{hiddenEntries.map(([key, { session }]) => <li key={key}>
          <span>{session.title || '제목 없음'}</span><button type="button" onClick={() => undoHide(key)}>다시 표시</button>
        </li>)}</ul>}
      </div>}
      {!online && !syncWaiting && !error && <p className="notice" role="status">
        Mac에 연결되면 자동으로 동기화를 다시 시작합니다.
      </p>}
      {snapshot?.permit && !device?.replyKey && <p className="notice" role="status">답장을 사용하려면 Mac에서 이 기기를 새 QR로 다시 연결하세요.</p>}
      {!modalSession && pendingProgress && <p className="notice" role="status">{replyLabels.processing}</p>}
      {device && modalSession && replyKey && <ReplyComposer key={replyKey} device={device} target={modalSession}
        unavailable={replyUnavailable} permit={reason === 'desktop-claude' ? undefined : permit} canLoadAnswer={canLoadAnswer} canAttach={latestTarget?.canAttach === true}
        maxTotalBytes={Math.min(attachmentLimit(snapshot), attachmentLimit(relay))}
        textOnly={modalSession.desktopReply === true}
        initialAnswer={cachedAnswer?.version === modalSession.updatedAt ? cachedAnswer?.body : undefined}
        draft={replyEntry?.draft ?? emptyDraft} outcome={replyEntry?.progress?.code}
        uploadPercent={upload && upload.id === replyEntry?.progress?.id ? upload.percent : undefined}
        otherPending={Boolean(pendingKey && pendingKey !== replyKey)}
        onDraft={draft => updateDraft(replyKey, draft)} onSend={() => void sendReply(modalSession, replyKey)}
        onRetry={!replyUnavailable && !pendingKey && replyEntry?.progress?.retryable && replyEntry.draft.attachments?.length &&
          replyEntry.progress.startedAt && Date.now() < replyEntry.progress.startedAt + 120_000
          ? () => void sendReply(modalSession, replyKey, true) : undefined}
        onDismiss={() => setReplies(previous => ({ ...previous, [replyKey]: { draft: previous[replyKey]?.draft ?? emptyDraft } }))}
        receipt={receipt} receipts={receipts} context={snapshot?.context} onWait={waitForDetail} onAnswer={rememberAnswer} onClose={() => setSelectedSession(undefined)} />}
      {snapshot?.partial && <p className="notice" role="status">일부 세션을 읽지 못했습니다.</p>}
      {(snapshot || (!error && syncWaiting)) && <CompactList
        sessions={visibleEntries} parents={parents} loading={!snapshot}
        notice={query.trim() && ![...visibleEntries.values()].some(entry => entry.hiddenAt === undefined) ? '검색 결과가 없습니다.' : undefined}
        onOpen={openRow} onRead={read} onHide={hide} showMenuButton
        rowTitle={() => '대화 열기'}
        rowAction={session => canSendTo(session) && <button type="button" className="reply-link"
          onClick={() => openRow(session)} aria-label={`${session.title}에 답장`}>답장하기</button>} />}
    </> : <>
      <header className="top-bar">
        <img src="/offtab.png" alt="" width="28" height="28" />
        <span>Offtab</span>
      </header>
      <section className="pairing">
        <h1>{relay?.phase === 'pin' ? 'PIN 입력' : 'Mac 연결'}</h1>
        {relay?.phase === 'pin' ? <form onSubmit={connect}>
          <p>Mac의 Offtab에 표시된 6자리를 입력하세요.</p>
          <label htmlFor="pin">6자리 PIN</label>
          <input id="pin" autoComplete="off" inputMode="numeric" pattern="[0-9]{6}"
            maxLength={6} value={pin} onChange={event => setPin(event.target.value.replace(/\D/g, ''))} />
          <button type="submit" disabled={pin.length !== 6 || checking}>{checking ? '확인 중' : '연결하기'}</button>
          {!(pinError || error) && relay.attempts > 0 && !checking && <p>남은 시도 {5 - relay.attempts}회</p>}
          {(pinError || error) && <p className="notice" role="status">{pinError || error}</p>}
        </form> : <>
          <p role="status">{relay?.phase === 'locked' ? 'PIN 입력 횟수를 초과했습니다. Mac에서 새 QR을 찍으세요.'
            : error || (loading || device ? '연결 상태를 확인하는 중입니다.' : 'Mac Offtab의 설정에서 휴대폰 연결을 열고 QR을 찍으세요.')}</p>
          {location.hash && error && <button type="button" onClick={() => setInviteRetry(value => value + 1)}>다시 시도</button>}
          {!loading && !device && <details>
            <summary>연결 주소 붙여넣기</summary>
            <form onSubmit={openInvitation}>
              <label htmlFor="pairing-address">Mac Offtab에서 복사한 연결 주소</label>
              <input id="pairing-address" type="url" value={pairingAddress} onChange={event => setPairingAddress(event.currentTarget.value)} />
              <button type="submit" disabled={!pairingAddress.trim()}>연결 주소 열기</button>
            </form>
          </details>}
        </>}
      </section>
    </>}
  </div>;
}

createRoot(document.getElementById('root')!).render(<App />);
