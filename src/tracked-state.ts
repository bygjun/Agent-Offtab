import type { ReadUpdate, Session, Snapshot } from './types';

const hour = 3_600_000;
export type Entry = { session: Session; workAt: number; activityAt: number; readAt?: number; hiddenAt?: number; restoring?: boolean };
export type TrackedSessions = Map<string, Entry>;
const recordKey = (session: Pick<Session, 'provider' | 'id'>) => `${session.provider}:${session.id}`;
export const sessionKey = (session: Session) => recordKey({ ...session, id: session.nativeId ?? session.id });
const storageKey = 'session-desk.tracked.v2';
const uuid = /^[a-f0-9]{8}(?:-[a-f0-9]{4}){3}-[a-f0-9]{12}$/i;

export function restoreTracked(storage: Pick<Storage, 'getItem'>, remoteKey?: string): TrackedSessions {
  const entries: TrackedSessions = new Map();
  const stored = storage.getItem(remoteKey ?? storageKey);
  const raw: unknown = JSON.parse(stored ?? (remoteKey ? null : storage.getItem('session-desk.tracked.v1')) ?? '[]');
  const validId = remoteKey ? /^[\w-]{22}$/ : uuid;
  if (!Array.isArray(raw)) return entries;
  for (const item of raw) {
    if (!item || !['codex', 'claude'].includes(item.provider) || typeof item.id !== 'string' || !validId.test(item.id)
      || !Number.isSafeInteger(item.workAt) || !Number.isSafeInteger(item.activityAt)
      || (stored === null && item.provider === 'claude' && item.background > 0 && !item.active && !item.waiting)) continue;
    const session: Session = {
      provider: item.provider, id: item.id, title: '', cwd: '', preview: '', updatedAt: item.activityAt,
      active: typeof item.active === 'boolean' ? item.active : undefined,
      background: stored !== null && Number.isSafeInteger(item.background) && item.background > 0 ? item.background : undefined,
      waiting: item.waiting === 'input' || item.waiting === 'approval' ? item.waiting : undefined,
      waitingAt: Number.isSafeInteger(item.waitingAt) ? item.waitingAt : undefined,
      nativeId: item.provider === 'claude' && typeof item.nativeId === 'string'
        && item.nativeId.startsWith('local_') && uuid.test(item.nativeId.slice(6)) ? item.nativeId : undefined,
      hasTranscript: item.hasTranscript === true ? true : undefined,
      read: typeof item.readVersion === 'string' && item.readVersion.length <= 64 ? { version: item.readVersion } : undefined,
    };
    entries.set(sessionKey(session), { session, workAt: item.workAt, activityAt: item.activityAt,
      readAt: Number.isSafeInteger(item.readAt) ? item.readAt : undefined,
      hiddenAt: Number.isSafeInteger(item.hiddenAt) ? item.hiddenAt : undefined, restoring: true });
  }
  return entries;
}

export function saveTracked(storage: Pick<Storage, 'getItem' | 'setItem'>, entries: TrackedSessions, key = storageKey) {
  const serialized = JSON.stringify([...entries.values()].map(({ session, workAt, activityAt, readAt, hiddenAt }) => ({
    provider: session.provider, id: session.id, active: session.active, background: session.background,
    nativeId: session.nativeId, hasTranscript: session.hasTranscript,
    waiting: session.waiting, waitingAt: session.waitingAt, workAt, activityAt, readAt, hiddenAt, readVersion: session.read?.version,
  })));
  if (storage.getItem(key) !== serialized) storage.setItem(key, serialized);
}

export function isWorking(session: Session) {
  return session.active === true || (session.background ?? 0) > 0;
}

export function markRead(previous: TrackedSessions, key: string, at: number, excludePending = false): TrackedSessions {
  const entry = previous.get(key);
  if (!entry || entry.readAt !== undefined || entry.activityAt > at || entry.workAt > at
    || (excludePending && (entry.session.waiting || isWorking(entry.session) || entry.session.active !== false))) return previous;
  return new Map(previous).set(key, { ...entry, readAt: at });
}

export function readUpdates(entries: TrackedSessions): ReadUpdate[] {
  const updates: ReadUpdate[] = [];
  for (const { session, readAt, restoring } of entries.values()) {
    if (restoring || readAt === undefined || !session.read || session.read.at !== undefined) continue;
    updates.push({ provider: session.provider, id: session.nativeId ?? session.id, version: session.read.version, at: readAt });
    if (updates.length === 12) break;
  }
  return updates;
}

export function hideSession(previous: TrackedSessions, session: Session, at: number): TrackedSessions {
  const key = sessionKey(session);
  const activityAt = session.activityAt ?? session.updatedAt;
  const prior = previous.get(key);
  const hiddenAt = Math.max(at, session.lastUserAt ?? 0, prior?.session.lastUserAt ?? 0);
  return new Map(previous).set(key, { workAt: activityAt, activityAt, ...prior, session, hiddenAt });
}

export function trackSessions(previous: TrackedSessions, snapshot: Snapshot): TrackedSessions {
  const next = new Map(previous);
  const removed = new Set(snapshot.removed?.map(recordKey));
  for (const [key, entry] of next) {
    if (removed.has(recordKey(entry.session))) next.delete(key);
  }
  const found = new Set<string>();
  const statuses = new Map(snapshot.sources.map(source => [source.provider, source.status]));
  const isConfirmed = (session: Session) => ['ready', 'missing'].includes(statuses.get(session.provider) ?? '');
  for (const row of snapshot.sessions) {
    const record = recordKey(row);
    if (removed.has(record) || row.contextOnly) continue;
    const key = sessionKey(row);
    const prior = next.get(key) ?? next.get(record);
    if (!prior && !isWorking(row) && !row.waiting && !row.recentlyActive) continue;
    if (key !== record) next.delete(record);
    const hiddenAt = prior?.hiddenAt;
    const returned = hiddenAt !== undefined && (row.lastUserAt ?? 0) > hiddenAt;
    found.add(key);
    const session = { ...row };
    session.hasTranscript ??= prior?.session.hasTranscript;
    if (prior && hiddenAt !== undefined && !returned) {
      next.set(key, { ...prior, session, restoring: false });
      continue;
    }
    if (prior && !isConfirmed(row)) {
      if (prior.session.active && row.active === false) session.active = true;
      if (!row.background) session.background = prior.session.background;
      session.waiting ??= prior.session.waiting;
      session.waitingAt ??= prior.session.waitingAt;
      session.parentNativeId ??= prior.session.parentNativeId;
    }
    session.active ??= prior?.session.active;
    const wasWorking = !!prior && (isWorking(prior.session) || !!prior.session.waiting);
    const working = isWorking(session) || !!session.waiting;
    const changedWait = !!session.waiting && (session.waiting !== prior?.session.waiting || session.waitingAt !== prior?.session.waitingAt);
    const workAt = Math.max(session.activityAt ?? session.updatedAt, session.waitingAt ?? 0);
    const changedState = working !== wasWorking || changedWait;
    const changedVersion = !!prior?.session.read && !!session.read && prior.session.read.version !== session.read.version;
    const localReadAt = changedState || changedVersion || returned || workAt > (prior?.activityAt ?? 0) ? undefined : prior?.readAt;
    next.set(key, {
      session,
      workAt: Math.max(prior?.workAt ?? 0, workAt, changedState || returned ? snapshot.refreshedAt : 0),
      activityAt: Math.max(prior?.activityAt ?? 0, workAt),
      readAt: !working && session.active === false ? session.read?.at ?? localReadAt : localReadAt,
    });
  }

  for (const [key, entry] of next) {
    const hasSharedRead = found.has(key) && entry.session.read?.at !== undefined;
    if (entry.hiddenAt !== undefined || (!isConfirmed(entry.session) && !hasSharedRead)) continue;
    if (!found.has(key)) {
      if (entry.restoring) { next.delete(key); continue; }
      if (isWorking(entry.session) || entry.session.waiting) {
        next.set(key, { ...entry, readAt: undefined, workAt: snapshot.refreshedAt,
          session: { ...entry.session, active: false, background: undefined, waiting: undefined, waitingAt: undefined } });
        continue;
      }
    }
    if (!entry.session.waiting && !isWorking(entry.session) && entry.session.active === false && entry.readAt !== undefined
      && snapshot.refreshedAt >= (entry.session.read?.at ?? Math.max(entry.readAt, entry.workAt)) + hour) next.delete(key);
  }
  return next;
}
