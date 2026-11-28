import assert from 'node:assert/strict';
import { test } from 'node:test';
import { build } from 'esbuild';

const bundle = await build({ stdin: {
  contents: "export * from './src/tracked-state'; export * from './src/session-tree'; export * from './src/compact-state';",
  resolveDir: process.cwd(),
}, bundle: true, format: 'esm', platform: 'node', write: false });
const { trackSessions, saveTracked, restoreTracked, markRead, readUpdates, hideSession, sessionKey, sessionTree, hasPendingSessions, compactEntries, compactView, mirrorCompact, trackMirrored } = await import(`data:text/javascript;base64,${Buffer.from(bundle.outputFiles[0].contents).toString('base64')}`);
const hour = 3_600_000;
const day = 24 * hour;
const time = 10_000;
const key = 'codex:recent';
const session = { id: 'recent', provider: 'codex', title: 'Example', preview: '', cwd: '/example', updatedAt: time, activityAt: time };

function snap(sessions, refreshedAt = time, status = 'ready') {
  return { sessions, refreshedAt, sources: [{ provider: 'codex', status }] };
}

test('the phone mirrors desktop exclusions, active children, read state, and list order', () => {
  const parent = { ...session, provider: 'claude', id: 'parent', nativeId: 'local_parent' };
  const child = { ...session, provider: 'claude', id: 'child', parentNativeId: parent.nativeId, active: true };
  const excluded = { ...session, id: 'excluded', lastUserAt: time, recentlyActive: true, active: false };
  const done = { ...session, read: { version: 'same', at: time }, recentlyActive: true, active: false };
  const source = { ...snap([done, excluded, child, parent]), sources: [{ provider: 'codex', status: 'ready' }, { provider: 'claude', status: 'ready' }] };
  const desktop = hideSession(trackSessions(new Map(), source), excluded, time);
  const view = compactView(desktop, [parent]);
  const payload = [...view.entries.map(({ session, ...compact }) => ({ ...session, compact })), ...view.parents];
  const phone = trackMirrored(trackSessions(new Map(), source), payload);
  assert.deepEqual(compactEntries(phone).map(row => row.session.id), ['child', 'recent']);
  assert.equal(phone.get(key).readAt, time);
  assert.deepEqual(view.parents.map(row => row.id), ['parent']);
  assert.deepEqual(compactView(hideSession(desktop, parent, time), [parent]).parents, []);

  const newInput = { ...excluded, lastUserAt: time + 1 };
  const updated = trackSessions(desktop, { ...source, sessions: [done, newInput, child, parent], refreshedAt: time + 1 });
  assert.ok(compactView(updated, [parent]).entries.some(row => row.session.id === 'excluded'));
});

test('pending mobile reads are sent without changing the mirrored desktop order or read state', () => {
  const done = { ...session, active: false, read: { version: 'same' }, compact: { workAt: time } };
  const pending = markRead(mirrorCompact([done]), key, time + 1, true);
  assert.equal(readUpdates(trackMirrored(pending, [done]))[0].at, time + 1);
  assert.equal(mirrorCompact([done]).get(key).readAt, undefined);
  const changed = { ...done, read: { version: 'new' } };
  assert.deepEqual(readUpdates(trackMirrored(pending, [changed])), []);
  assert.equal(trackMirrored(pending, []).size, 0);
});

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

test('background work stays visible and read completions expire after 24 hours', () => {
  const active = trackSessions(new Map(), snap([{ ...session, active: true }]));
  active.get(key).readAt = time;
  const done = { ...session, active: false };
  assert.ok(trackSessions(active, snap([{ ...session, active: true }], time + 2 * day)).has(key));
  assert.equal(trackSessions(active, snap([done], time + 100, 'partial')).get(key).session.active, true);
  assert.equal(trackSessions(active, snap([], time + 100, 'error')).get(key).session.active, true);
  assert.equal(trackSessions(active, snap([], time + 100)).get(key).readAt, undefined);
  const stopped = trackSessions(active, snap([done], time + 5_000));
  assert.equal(stopped.get(key).readAt, undefined);
  assert.ok(trackSessions(stopped, snap([done], time + 2 * day)).has(key));
  stopped.get(key).readAt = time + 6_000;
  assert.ok(trackSessions(stopped, snap([done], time + hour + 6_000)).has(key));
  assert.ok(trackSessions(stopped, snap([done], time + day + 5_999)).has(key));
  assert.ok(!trackSessions(stopped, snap([done], time + day + 6_000)).has(key));
  assert.ok(trackSessions(stopped, snap([done], time + day + 6_000, 'partial')).has(key));
  assert.ok(trackSessions(stopped, snap([], time + day + 6_000, 'error')).has(key));
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
  assert.ok(trackSessions(restoreTracked(storage), snap([entry], time + day + 99)).has(key));
  assert.ok(!trackSessions(restoreTracked(storage), snap([entry], time + day + 100)).has(key));
});

test('a shared read timestamp aligns both views and a new revision clears it', () => {
  const done = { ...session, active: false, recentlyActive: true, read: { version: '1:1:1' } };
  const openedAt = time + 100;
  const mac = markRead(trackSessions(new Map(), snap([done])), key, openedAt, true);
  assert.deepEqual(readUpdates(mac), [{ provider: 'codex', id: session.id, version: done.read.version, at: openedAt }]);
  const confirmed = { ...done, read: { ...done.read, at: openedAt } };
  const mobile = trackSessions(new Map(), snap([confirmed], openedAt + 10_000));
  assert.equal(mobile.get(key).readAt, openedAt);
  assert.deepEqual(readUpdates(mobile), []);
  assert.ok(trackSessions(mobile, snap([confirmed], openedAt + day - 1)).has(key));
  assert.ok(!trackSessions(mobile, snap([confirmed], openedAt + day)).has(key));
  assert.ok(!trackSessions(mobile, snap([confirmed], openedAt + day, 'partial')).has(key));
  const next = { ...done, read: { version: '1:2:1' } };
  assert.equal(trackSessions(mac, snap([next], openedAt + 10_000)).get(key).readAt, undefined);
  const working = trackSessions(new Map(), snap([{ ...done, active: true, read: undefined }]));
  assert.equal(markRead(working, key, openedAt, true), working);
});

test('mobile tracking restores only opaque session metadata and waits for the next user message', () => {
  const id = 'abcdefghijklmnopqrstuv';
  const key = `codex:${id}`;
  const deviceKey = 'offtab.mobile.tracked.example';
  const values = new Map();
  const storage = { getItem: key => values.get(key) ?? null, setItem: (key, value) => values.set(key, value) };
  const active = { ...session, id, title: 'private title', preview: 'private preview', active: true, lastUserAt: time };
  const hidden = hideSession(trackSessions(new Map(), snap([active])), active, time + 1);
  saveTracked(storage, hidden, deviceKey);
  assert.doesNotMatch(values.get(deviceKey), /private/);
  assert.equal(restoreTracked(storage).size, 0);
  const restored = restoreTracked(storage, deviceKey);
  assert.equal(restored.get(key).hiddenAt, time + 1);
  assert.equal(trackSessions(restored, snap([{ ...active, activityAt: time + 2 }])).get(key).hiddenAt, time + 1);
  assert.equal(trackSessions(restored, snap([{ ...active, lastUserAt: time + 3 }])).get(key).hiddenAt, undefined);
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


test('a Claude child places its parent group first without tracking a context-only ancestor', () => {
  const claude = { ...session, provider: 'claude' };
  const parent = { session: { ...claude, id: 'parent', nativeId: 'local_parent', contextOnly: true } };
  const child = { session: { ...claude, id: 'child', nativeId: 'local_child', parentNativeId: 'local_parent', waiting: 'input' } };
  const sibling = { session: { ...claude, id: 'sibling', parentNativeId: 'local_parent', active: true } };
  const unrelated = { session: { ...session, active: true } };
  const roots = sessionTree([child, unrelated, sibling], [parent]);
  assert.deepEqual(roots.map(node => node.row.session.id), ['parent', 'recent']);
  assert.equal(roots[0].context, true);
  assert.deepEqual(roots[0].children.map(node => node.row.session.id), ['child', 'sibling']);
  assert.equal(sessionTree([child, parent], [])[0].context, false);
  assert.equal(sessionTree([child], [])[0].row, child);
  assert.equal(trackSessions(new Map(), snap([parent.session, child.session])).size, 1);
});

test('nested Claude sessions stay in one tree, and cyclic parent data never loses a row', () => {
  const claude = { ...session, provider: 'claude' };
  const first = { session: { ...claude, id: 'root', nativeId: 'local_root' } };
  const second = { session: { ...claude, id: 'middle', nativeId: 'local_middle', parentNativeId: 'local_root' } };
  const third = { session: { ...claude, id: 'leaf', nativeId: 'local_leaf', parentNativeId: 'local_middle' } };
  const [root] = sessionTree([third], [first, second]);
  assert.equal(root.children[0].children[0].row, third);
  first.session.parentNativeId = 'local_leaf';
  const cycle = sessionTree([third, second, first], []);
  assert.equal(cycle.length, 1);
  assert.equal(cycle[0].children[0].children[0].row, third);
});


test('groups open for work or waiting in any descendant, including detached background work', () => {
  const claude = { ...session, provider: 'claude', active: false };
  const parent = { session: { ...claude, id: 'parent', nativeId: 'local_parent', active: true } };
  const branch = { session: { ...claude, id: 'branch', nativeId: 'local_branch', parentNativeId: 'local_parent' } };
  const peer = { session: { ...claude, id: 'peer', parentNativeId: 'local_parent' } };
  const leaf = { session: { ...claude, id: 'leaf', parentNativeId: 'local_branch' } };
  const children = (state = {}) =>
    sessionTree([parent, branch, peer, { session: { ...leaf.session, ...state } }], [])[0].children;
  assert.equal(hasPendingSessions(children()), false);
  for (const state of [{ active: true }, { waiting: 'input' }, { waiting: 'approval' }, { background: 1 }]) {
    assert.equal(hasPendingSessions(children(state)), true);
  }
});
