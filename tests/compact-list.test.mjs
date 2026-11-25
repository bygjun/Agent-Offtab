import assert from 'node:assert/strict';
import { test } from 'node:test';
import { build } from 'esbuild';

const bundle = await build({ entryPoints: ['src/tracked-state.ts'], bundle: true, format: 'esm', platform: 'node', write: false });
const { trackSessions, saveTracked, restoreTracked, markRead, hideSession, sessionKey } = await import(`data:text/javascript;base64,${Buffer.from(bundle.outputFiles[0].contents).toString('base64')}`);
const hour = 3_600_000;
const time = 10_000;
const key = 'codex:recent';
const session = { id: 'recent', provider: 'codex', title: 'Example', preview: '', cwd: '/example', updatedAt: time, activityAt: time };

function snap(sessions, refreshedAt = time, status = 'ready') {
  return { sessions, refreshedAt, sources: [{ provider: 'codex', status }] };
}

test('discovers active sessions and work completed between checks while ignoring past history', () => {
  const previous = trackSessions(new Map(), snap([]));
  const rows = trackSessions(previous, snap([
    { ...session, active: false, recentlyActive: true },
    { ...session, id: 'live', active: true },
    { ...session, id: 'history', active: false },
  ]));
  assert.deepEqual([...rows.keys()], [key, 'codex:live']);
  assert.equal(rows.get(key).readAt, undefined);
});

test('background work stays visible and becomes unread on completion until the result is opened', () => {
  const active = trackSessions(new Map(), snap([{ ...session, active: true }]));
  active.get(key).readAt = time;
  const done = { ...session, active: false };
  assert.ok(trackSessions(active, snap([{ ...session, active: true }], time + 2 * hour)).has(key));
  assert.equal(trackSessions(active, snap([done], time + 100, 'partial')).get(key).session.active, true);
  assert.equal(trackSessions(active, snap([], time + 100, 'error')).get(key).session.active, true);
  assert.equal(trackSessions(active, snap([], time + 100)).get(key).readAt, undefined);
  const stopped = trackSessions(active, snap([done], time + 5_000));
  assert.equal(stopped.get(key).readAt, undefined);
  assert.ok(trackSessions(stopped, snap([done], time + 2 * hour)).has(key));
  stopped.get(key).readAt = time + 6_000;
  assert.ok(trackSessions(stopped, snap([done], time + hour + 5_999)).has(key));
  assert.ok(!trackSessions(stopped, snap([done], time + hour + 6_000)).has(key));
  assert.ok(trackSessions(stopped, snap([done], time + hour + 6_000, 'partial')).has(key));
  assert.ok(trackSessions(stopped, snap([], time + hour + 6_000, 'error')).has(key));
});

test('a resumed session becomes unread and a timestamp regression does not reset read state', () => {
  const unread = trackSessions(new Map(), snap([{ ...session, active: true }]));
  unread.get(key).readAt = time;
  const stopped = trackSessions(unread, snap([{ ...session, active: false, activityAt: time - 100 }], time + 100));
  stopped.get(key).readAt = time + 120;
  const same = trackSessions(stopped, snap([{ ...session, active: false }], time + 150));
  assert.equal(same.get(key).readAt, time + 120);
  const working = trackSessions(same, snap([{ ...session, active: true, activityAt: time + 200 }], time + 200));
  assert.equal(working.get(key).readAt, undefined);
});

test('restores unread completions and read deadlines without saving message text', () => {
  const id = '00000000-0000-0000-0000-000000000005';
  const entry = { ...session, id, title: 'private title', preview: 'private preview', active: false, recentlyActive: true };
  const key = `codex:${id}`;
  const values = new Map();
  const storage = { getItem: key => values.get(key) ?? null, setItem: (key, value) => values.set(key, value) };
  let rows = trackSessions(new Map(), snap([entry]));
  saveTracked(storage, rows);
  assert.doesNotMatch([...values.values()][0], /private/);
  assert.equal(restoreTracked(storage).get(key).restoring, true);
  assert.equal(trackSessions(restoreTracked(storage), snap([entry], time + 3 * hour)).get(key).readAt, undefined);
  rows = markRead(rows, key, time + 100, true);
  saveTracked(storage, rows);
  assert.ok(trackSessions(restoreTracked(storage), snap([entry], time + hour + 99)).has(key));
  assert.ok(!trackSessions(restoreTracked(storage), snap([entry], time + hour + 100)).has(key));
});

test('waiting and detached jobs hold a session until the last job actually stops', () => {
  const pending = { ...session, active: false, waiting: 'approval', waitingAt: time, background: 2 };
  let rows = trackSessions(new Map(), snap([pending]));
  assert.equal(markRead(rows, key, time, true), rows);
  rows = trackSessions(rows, snap([{ ...pending, waiting: undefined, waitingAt: undefined, background: 1 }], time + hour));
  assert.ok(rows.has(key));
  rows = markRead(rows, key, time + hour);
  rows = trackSessions(rows, snap([{ ...session, active: false, background: 0 }], time + 2 * hour));
  assert.equal(rows.get(key).readAt, undefined);
});

test('a confirmed deletion removes tracked and explicitly hidden sessions even with a partial scan', () => {
  const active = { ...session, active: true };
  const done = { ...session, id: 'other', active: false, recentlyActive: true };
  const rows = trackSessions(new Map(), snap([active, done]));
  const hidden = hideSession(rows, active, time);
  const scan = { ...snap([], time + 1000, 'partial'), removed: [active] };
  assert.deepEqual([...trackSessions(rows, scan).keys()], ['codex:other']);
  assert.deepEqual([...trackSessions(hidden, scan).keys()], ['codex:other']);
});

test('hiding survives restarts, agent-only activity, and Claude changing its transcript ID', () => {
  const item = {
    ...session, id: '00000000-0000-0000-0000-000000000005', provider: 'claude',
    nativeId: 'local_00000000-0000-0000-0000-000000000009', active: true, lastUserAt: time - 200,
  };
  const key = sessionKey(item);
  const values = new Map();
  const storage = { getItem: key => values.get(key) ?? null, setItem: (key, value) => values.set(key, value) };
  const scan = (sessions, refreshedAt, status = 'ready') => ({
    sessions, refreshedAt, sources: [{provider: 'claude', status}],
  });
  const hidden = hideSession(trackSessions(new Map(), scan([item], time)), item, time);
  saveTracked(storage, hidden);
  let rows = trackSessions(restoreTracked(storage), scan([], time + 100, 'partial'));
  const continued = {
    ...item, id: '00000000-0000-0000-0000-000000000006',
    active: false, background: 3, activityAt: time + 2000, waiting: 'input',
  };
  rows = trackSessions(rows, scan([continued], time + 10 * hour));
  assert.equal(rows.get(key).hiddenAt, time);
  assert.equal(rows.get(key).session.id, continued.id);
  const replied = { ...continued, lastUserAt: time + 10 * hour + 100, background: 0, waiting: undefined };
  rows = trackSessions(rows, scan([replied], time + 10 * hour + 100));
  assert.equal(rows.get(key).hiddenAt, undefined);
  assert.equal(rows.get(key).readAt, undefined);
});
