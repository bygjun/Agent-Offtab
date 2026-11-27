import assert from 'node:assert/strict';
import { randomBytes } from 'node:crypto';
import { test } from 'node:test';
import { request } from 'node:http';
import { mkdtempSync, readFileSync, rmSync, statSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createRelay } from '../relay/server.mjs';

const origin = 'https://offtab.example';
const publicKey = Buffer.concat([Buffer.from([4]), randomBytes(64)]).toString('base64url');
const encryptedPin = randomBytes(34).toString('base64url');

async function setup(t, stateFile, startingTime = 0, options = {}) {
  let time = startingTime;
  const server = createRelay({ origin, stateFile, now: () => time, ...options });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  t.after(() => server.close());
  const base = `http://127.0.0.1:${server.address().port}/api/links`;
  const call = (path, method, headers, data) => fetch(base + path, {
    method,
    headers: { ...headers, ...(data && { 'Content-Type': 'application/json' }) },
    body: data && JSON.stringify(data),
  });
  async function registration() {
    const res = await call('', 'POST');
    assert.equal(res.status, 201);
    const { id, invite, host } = await res.json();
    const owner = { Authorization: `Bearer ${host}` };
    const path = `/${id}`;
    return {
      path, owner,
      status: async () => (await call(path + '/host', 'GET', owner)).json(),
      scan: (scanOrigin = origin) => call(path + '/scan', 'POST', { Origin: scanOrigin }, { invite, publicKey }),
      revoke: () => call(path, 'DELETE', owner),
    };
  }
  return { base, call, registration, close: () => new Promise(resolve => server.close(resolve)), advance: ms => { time += ms; } };
}

function browser(response) {
  const cookie = response.headers.get('set-cookie');
  assert.match(cookie, /HttpOnly; SameSite=Strict; Secure$/);
  return { Cookie: cookie.split(';', 1)[0], Origin: origin };
}

test('remote view uses single-use scan, browser authorization, ciphertext expiry and revocation', async t => {
  const { call, registration, advance } = await setup(t);
  const a = await registration();
  const b = await registration();
  assert.equal((await a.scan('https://other.example')).status, 404);
  const scans = await Promise.all([a.scan(), a.scan()]);
  assert.deepEqual(scans.map(response => response.status).sort(), [204, 404]);
  const headers = browser(scans.find(response => response.status === 204));
  const foreign = browser(await b.scan());
  assert.equal((await call(a.path + '/view', 'GET', foreign)).status, 404);
  assert.equal((await call(a.path + '/view', 'GET')).status, 404);
  const sealed = randomBytes(180).toString('base64url');
  assert.equal((await call(a.path + '/snapshot', 'PUT', a.owner, { sealed })).status, 404);
  assert.equal((await call(a.path + '/pin', 'POST', { ...headers, Origin: 'https://other.example' }, { sealed: encryptedPin })).status, 404);
  assert.equal((await call(a.path + '/pin', 'POST', headers, { sealed: encryptedPin })).status, 202);
  assert.equal((await call(a.path + '/pin', 'POST', headers, { sealed: encryptedPin })).status, 409);
  const { request } = await a.status();
  assert.equal(request.sealed, encryptedPin);
  assert.equal((await call(a.path + '/decision', 'POST', b.owner, { id: request.id, accepted: true })).status, 404);
  assert.equal((await call(a.path + '/decision', 'POST', a.owner, { id: request.id, accepted: true })).status, 204);
  const readView = async () => (await call(a.path + '/view', 'GET', headers)).json();
  assert.equal((await readView()).sync, 'waiting');
  advance(30_000);
  assert.equal((await readView()).sync, 'offline');
  assert.equal((await call(a.path + '/snapshot', 'PUT', a.owner, { sealed })).status, 204);
  const read = await call(a.path + '/view', 'GET', headers);
  assert.equal(read.headers.get('Cache-Control'), 'no-store');
  const currentView = await read.json();
  assert.equal(currentView.snapshot, sealed);
  assert.equal(currentView.sync, 'ready');
  const etag = read.headers.get('etag');
  assert.ok(etag);
  const unchanged = await call(a.path + '/view', 'GET', { ...headers, 'If-None-Match': etag });
  assert.equal(unchanged.status, 304);
  assert.equal(await unchanged.text(), '');
  const weakVersion = await call(a.path + '/view', 'GET', { ...headers, 'If-None-Match': 'W/' + etag });
  assert.equal(weakVersion.status, 304);
  assert.equal(await weakVersion.text(), '');
  assert.equal((await call(a.path + '/view', 'GET', { ...foreign, 'If-None-Match': etag })).status, 404);
  advance(14 * 60_000);
  const afterExpiry = await call(a.path + '/view', 'GET', { ...headers, 'If-None-Match': etag });
  assert.equal(afterExpiry.status, 200);
  assert.equal(afterExpiry.headers.get('etag'), null);
  const expiredView = await readView();
  assert.equal(expiredView.phase, 'paired');
  assert.equal(expiredView.snapshot, undefined);
  assert.equal(expiredView.sync, 'offline');
  assert.equal((await call(a.path + '/snapshot', 'PUT', a.owner, { sealed })).status, 204);
  assert.equal((await readView()).sync, 'ready');
  const updated = randomBytes(180).toString('base64url');
  assert.equal((await call(a.path + '/snapshot', 'PUT', a.owner, { sealed: updated })).status, 204);
  const changed = await call(a.path + '/view', 'GET', { ...headers, 'If-None-Match': etag });
  assert.equal(changed.status, 200);
  assert.notEqual(changed.headers.get('etag'), etag);
  assert.equal((await changed.json()).snapshot, updated);
  assert.equal((await a.revoke()).status, 204);
  assert.equal((await call(a.path + '/view', 'GET', headers)).status, 404);
  assert.equal((await call(a.path + '/snapshot', 'PUT', a.owner, { sealed })).status, 404);
});

test('wrong PIN attempts persist across requests and expired or locked links cannot be reused', async t => {
  const { call, registration, advance } = await setup(t);
  const pair = await registration();
  const headers = browser(await pair.scan());
  for (let attempt = 1; attempt <= 5; attempt++) {
    assert.equal((await call(pair.path + '/pin', 'POST', headers, { sealed: encryptedPin })).status, 202);
    const { request } = await pair.status();
    assert.equal((await call(pair.path + '/decision', 'POST', pair.owner, { id: request.id, accepted: false })).status, 204);
    const view = await (await call(pair.path + '/view', 'GET', headers)).json();
    assert.equal(view.attempts, attempt);
    assert.equal(view.phase, attempt === 5 ? 'locked' : 'pin');
  }
  assert.equal((await call(pair.path + '/pin', 'POST', headers, { sealed: encryptedPin })).status, 404);
  assert.equal((await pair.scan()).status, 404);
  const unscanned = await registration();
  const scanned = await registration();
  const pending = browser(await scanned.scan());
  advance(120_000);
  assert.equal((await unscanned.scan()).status, 404);
  assert.equal((await call(scanned.path + '/pin', 'POST', pending, { sealed: encryptedPin })).status, 404);
});

test('replies are isolated by paired browser, single pending request and expiry', async t => {
  const { call, registration, advance } = await setup(t);
  const a = await registration();
  const b = await registration();
  const first = browser(await a.scan());
  const second = browser(await b.scan());
  const id = 'ed0f9cfc-cb7f-4646-8fe1-6dfb386c1b53';
  const sealed = randomBytes(100).toString('base64url');
  assert.equal((await call(a.path + '/reply', 'POST', first, { id, sealed })).status, 404);
  await call(a.path + '/pin', 'POST', first, { sealed: encryptedPin });
  const { request } = await a.status();
  await call(a.path + '/decision', 'POST', a.owner, { id: request.id, accepted: true });
  assert.equal((await call(a.path + '/reply', 'POST', first, { id, sealed })).status, 503);
  await call(a.path + '/snapshot', 'PUT', a.owner, { sealed });
  assert.equal((await call(a.path + '/reply', 'POST', second, { id, sealed })).status, 404);
  assert.equal((await (await call(a.path + '/view', 'GET', first)).json()).largeAttachments, true);
  assert.equal((await (await call(a.path + '/view', 'GET', first)).json()).maxAttachmentBytes, 20 * 1024 * 1024);
  const tooLarge = 'A'.repeat(38_000_000);
  assert.equal((await call(a.path + '/reply', 'POST', first, { id, sealed: tooLarge })).status, 413);
  const photo = 'A'.repeat(37_400_000);
  assert.equal((await call(a.path + '/reply', 'POST', first, { id, sealed: photo })).status, 202);
  assert.equal((await a.status()).reply.sealed.length, photo.length);
  assert.equal((await call(a.path + '/reply-ack', 'POST', a.owner, { id })).status, 204);
  assert.equal((await call(a.path + '/reply', 'POST', first, { id, sealed })).status, 202);
  assert.equal((await call(a.path + '/reply', 'POST', first, { id, sealed })).status, 202);
  assert.equal((await call(a.path + '/reply', 'POST', first, {
    id: 'ed0f9cfc-cb7f-4646-8fe1-6dfb386c1b54', sealed,
  })).status, 409);
  assert.deepEqual((await a.status()).reply, { id, sealed, expiresAt: 30_000 });
  assert.equal((await b.status()).reply, undefined);
  assert.equal((await call(a.path + '/reply-ack', 'POST', b.owner, { id })).status, 404);
  assert.equal((await call(a.path + '/reply-ack', 'POST', a.owner, { id })).status, 204);
  assert.equal((await a.status()).reply, undefined);
  assert.equal((await call(a.path + '/reply', 'POST', first, { id, sealed })).status, 202);
  advance(30_000);
  assert.equal((await a.status()).reply, undefined);
});

test('paired devices survive relay restarts, while invite, snapshot and reply data do not', async t => {
  const folder = mkdtempSync(join(tmpdir(), 'offtab-relay-test-'));
  t.after(() => rmSync(folder, { recursive: true, force: true }));
  const stateFile = join(folder, 'devices.json');
  const first = await setup(t, stateFile);
  const paired = await first.registration();
  const unused = await first.registration();
  const headers = browser(await paired.scan());
  await first.call(paired.path + '/pin', 'POST', headers, { sealed: encryptedPin });
  const { request } = await paired.status();
  assert.equal((await first.call(paired.path + '/decision', 'POST', paired.owner, { id: request.id, accepted: true })).status, 204);
  const sealed = randomBytes(100).toString('base64url');
  const id = 'ed0f9cfc-cb7f-4646-8fe1-6dfb386c1b53';
  assert.equal((await first.call(paired.path + '/snapshot', 'PUT', paired.owner, { sealed })).status, 204);
  assert.equal((await first.call(paired.path + '/reply', 'POST', headers, { id, sealed })).status, 202);
  const stored = readFileSync(stateFile, 'utf8');
  assert.equal(statSync(stateFile).mode & 0o777, 0o600);
  assert(!stored.includes(paired.owner.Authorization.slice(7)));
  assert(!stored.includes(headers.Cookie.split('=')[1]));
  assert(!stored.includes(sealed));
  await first.close();

  const next = await setup(t, stateFile, 2 * 86_400_000);
  assert.equal((await next.call(unused.path + '/host', 'GET', unused.owner)).status, 404);
  const response = await next.call(paired.path + '/view', 'GET', headers);
  assert.equal(response.status, 200);
  assert.match(response.headers.get('set-cookie'), /HttpOnly; SameSite=Strict; Secure/);
  const view = await response.json();
  assert.equal(view.phase, 'paired');
  assert.equal(view.snapshot, undefined);
  assert.equal(view.sync, 'offline');
  const host = await (await next.call(paired.path + '/host', 'GET', paired.owner)).json();
  assert.equal(host.reply, undefined);
  assert.equal((await next.call(paired.path + '/snapshot', 'PUT', paired.owner, { sealed })).status, 204);
  assert.equal((await (await next.call(paired.path + '/view', 'GET', headers)).json()).sync, 'ready');
  assert.equal((await next.call(paired.path, 'DELETE', paired.owner)).status, 204);
  await next.close();

  const again = await setup(t, stateFile);
  const revoked = await again.call(paired.path + '/view', 'GET', headers);
  assert.equal(revoked.status, 404);
  assert.equal((await revoked.json()).code, 'link_ended');
});

test('marking read, reading context and sending a reply have independent queues', async t => {
  const { call, registration, advance } = await setup(t);
  const device = await registration();
  const headers = browser(await device.scan());
  await call(device.path + '/pin', 'POST', headers, { sealed: encryptedPin });
  const { request } = await device.status();
  await call(device.path + '/decision', 'POST', device.owner, { id: request.id, accepted: true });
  const sealed = randomBytes(100).toString('base64url');
  await call(device.path + '/snapshot', 'PUT', device.owner, { sealed });
  const id = 'ed0f9cfc-cb7f-4646-8fe1-6dfb386c1b53';
  const context = 'ed0f9cfc-cb7f-4646-8fe1-6dfb386c1b54';
  const read = 'ed0f9cfc-cb7f-4646-8fe1-6dfb386c1b55';
  const readPath = device.path + '/reply?kind=read';
  assert.equal((await (await call(device.path + '/view', 'GET', headers)).json()).readSync, true);
  assert.equal((await call(device.path + '/reply', 'POST', headers, { id, sealed })).status, 202);
  assert.equal((await call(device.path + '/reply?kind=context', 'POST', headers, { id: context, sealed, kind: 'context' })).status, 202);
  assert.equal((await call(readPath, 'POST', {}, { id: read, sealed, kind: 'read' })).status, 404);
  assert.equal((await call(readPath, 'POST', headers, { id: read, sealed })).status, 400);
  assert.equal((await call(readPath, 'POST', headers, { id: read, sealed, kind: 'read' })).status, 202);
  assert.equal((await call(readPath, 'POST', headers, { id: context, sealed, kind: 'read' })).status, 409);
  assert.equal((await device.status()).reply.id, id);
  assert.equal((await device.status()).context.id, context);
  assert.equal((await device.status()).read.id, read);
  await call(device.path + '/reply-ack', 'POST', device.owner, { id: context, kind: 'read' });
  assert.equal((await device.status()).context.id, context);
  await call(device.path + '/reply-ack', 'POST', device.owner, { id: context });
  assert.equal((await device.status()).reply.id, id);
  assert.equal((await device.status()).context, undefined);
  await call(device.path + '/reply-ack', 'POST', device.owner, { id: read, kind: 'read' });
  assert.equal((await device.status()).read, undefined);
  assert.equal((await call(readPath, 'POST', headers, { id: read, sealed, kind: 'read' })).status, 202);
  advance(30_000);
  assert.equal((await device.status()).read, undefined);
});

test('paired devices opt in to push and only an opaque waiting-session ID is sent', async t => {
  const delivered = [];
  const notifications = { publicKey, send: async (...event) => { delivered.push(event); } };
  const { call, registration } = await setup(t, undefined, 0, { notifications });
  const device = await registration();
  const headers = browser(await device.scan());
  await call(device.path + '/pin', 'POST', headers, { sealed: encryptedPin });
  const { request } = await device.status();
  await call(device.path + '/decision', 'POST', device.owner, { id: request.id, accepted: true });
  const { enabled } = await (await call(device.path + '/push', 'GET', headers)).json();
  assert.equal(enabled, false);
  const subscription = { endpoint: 'https://web.push.apple.com/test', keys: { p256dh: publicKey, auth: randomBytes(16).toString('base64url') } };
  assert.equal((await call(device.path + '/push', 'POST', { ...headers, Origin: 'https://other.example' }, { subscription })).status, 404);
  assert.equal((await call(device.path + '/push', 'POST', headers, { subscription: { ...subscription, endpoint: 'https://localhost/test' } })).status, 400);
  assert.equal((await call(device.path + '/push', 'POST', headers, { subscription })).status, 204);
  const waiting = randomBytes(16).toString('base64url');
  const sealed = randomBytes(100).toString('base64url');
  assert.equal((await call(device.path + '/snapshot', 'PUT', device.owner, { sealed, waiting })).status, 204);
  assert.deepEqual(delivered, [[subscription, waiting]]);
  assert.equal((await call(device.path + '/push', 'POST', headers, { subscription: null })).status, 204);
  assert.equal((await call(device.path + '/snapshot', 'PUT', device.owner, { sealed, waiting })).status, 204);
  assert.equal(delivered.length, 1);
});

test('relay can keep device registration on the Mac-local address', async t => {
  const { base, call } = await setup(t, undefined, 0, { registrationHost: '127.0.0.1:63915' });
  const post = headers => new Promise((resolve, reject) => {
    const req = request(base, { method: 'POST', headers }, response => {
      response.resume();
      resolve(response.statusCode);
    });
    req.on('error', reject);
    req.end();
  });
  assert.equal((await call('', 'POST')).status, 404);
  assert.equal(await post({ Host: '127.0.0.1:63915', 'CF-Connecting-IP': '192.0.2.10' }), 404);
  assert.equal(await post({ Host: '127.0.0.1:63915' }), 201);
});
