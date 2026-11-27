import { createHash, randomBytes, timingSafeEqual } from 'node:crypto';
import { mkdirSync, readFileSync, renameSync, rmSync, writeFileSync } from 'node:fs';
import { homedir } from 'node:os';
import { dirname, join } from 'node:path';
import { createServer } from 'node:http';
import { fileURLToPath } from 'node:url';
import { createNotifications, pushSubscription } from './web-notifications.mjs';

const inviteLifetime = 120_000;
const browserCookieSeconds = 365 * 24 * 60 * 60;
const cookieRefreshInterval = 24 * 60 * 60_000;
const snapshotLifetime = 30_000;
const maxAttempts = 5;
const replyLifetime = 30_000;
const maxReplyBody = 38_000_000;
const maxAttachmentBytes = 20 * 1024 * 1024;
const token = () => randomBytes(32).toString('base64url');
const validToken = /^[A-Za-z0-9_-]{43}$/;
const validKey = /^B[A-Za-z0-9_-]{86}$/;
const validSealedPin = /^[A-Za-z0-9_-]{46}$/;
const validReplyId = /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/;
const route = /^\/api\/links\/([A-Za-z0-9_-]{43})(?:\/(host|scan|pin|decision|snapshot|view|reply|reply-ack|push))?$/;

function reply(res, status, data) {
  res.writeHead(status, data === undefined ? {} : { 'Content-Type': 'application/json; charset=utf-8' });
  res.end(data === undefined ? undefined : JSON.stringify(data));
}

function digest(value) {
  return createHash('sha256').update(value).digest('base64url');
}

function matchesCredential(hash, supplied) {
  return typeof supplied === 'string' && validToken.test(supplied) && equal(hash, digest(supplied));
}

function loadPairings(stateFile, origin) {
  if (!stateFile) return new Map();
  let saved;
  try {
    saved = JSON.parse(readFileSync(stateFile, 'utf8'));
  } catch (error) {
    if (error.code === 'ENOENT') return new Map();
    throw error;
  }
  if (saved.version !== 1 || saved.origin !== origin || !Array.isArray(saved.devices)
    || saved.devices.length > 48 || saved.devices.some(device => !device || !validToken.test(device.id ?? '')
      || !validToken.test(device.hostHash ?? '') || !validToken.test(device.browserHash ?? '')
      || !Number.isSafeInteger(device.pairedAt))) {
    throw new Error('Invalid Offtab relay pairing store.');
  }
  return new Map(saved.devices.map(({ id, hostHash, browserHash, pairedAt, subscription }) => [
    id, { hostHash, browserHash, pairedAt, subscription: pushSubscription(subscription), expiresAt: null, phase: 'paired', attempts: 0 },
  ]));
}

function savePairings(stateFile, origin, links, changedId, replacement) {
  if (!stateFile) return;
  const devices = Array.from(links, ([id, link]) => {
    const record = id === changedId ? replacement : link;
    if (record?.phase !== 'paired') return null;
    return { id, hostHash: record.hostHash, browserHash: record.browserHash, pairedAt: record.pairedAt, subscription: record.subscription };
  }).filter(Boolean);
  const temp = stateFile + '.' + token() + '.tmp';
  mkdirSync(dirname(stateFile), { recursive: true, mode: 0o700 });
  try {
    writeFileSync(temp, JSON.stringify({ version: 1, origin, devices }), { mode: 0o600, flag: 'wx', flush: true });
    renameSync(temp, stateFile);
  } finally {
    rmSync(temp, { force: true });
  }
}

function browserCookie(value, id, secure) {
  return `offtab_view=${value}; Path=/api/links/${id}/; Max-Age=${browserCookieSeconds}; HttpOnly; SameSite=Strict${secure ? '; Secure' : ''}`;
}

function equal(expected, supplied) {
  return typeof expected === 'string' && typeof supplied === 'string' && validToken.test(supplied)
    && timingSafeEqual(Buffer.from(expected), Buffer.from(supplied));
}

async function readJson(req, limit) {
  const length = Number(req.headers['content-length']);
  if (!Number.isSafeInteger(length) || length < 1 || length > limit
    || req.headers['content-type']?.split(';', 1)[0] !== 'application/json') {
    req.resume();
    return undefined;
  }
  const chunks = [];
  let size = 0;
  for await (const chunk of req) {
    size += chunk.length;
    if (size > limit) return undefined;
    chunks.push(chunk);
  }
  if (size !== length) return undefined;
  try { return JSON.parse(Buffer.concat(chunks).toString('utf8')); } catch { return undefined; }
}

export function createRelay({ origin, assets = new Map(), now = Date.now, stateFile, notifications, registrationHost }) {
  const secure = new URL(origin).protocol === 'https:';
  const links = loadPairings(stateFile, origin);
  const expired = (link, time) => link.expiresAt !== null && time >= link.expiresAt;

  function expire() {
    const time = now();
    for (const [id, link] of links) {
      if (expired(link, time)) links.delete(id);
      else {
        if (link.snapshot && time >= link.snapshot.expiresAt) link.snapshot = undefined;
        if (link.reply && time >= link.reply.expiresAt) link.reply = undefined;
        if (link.context && time >= link.context.expiresAt) link.context = undefined;
        if (link.read && time >= link.read.expiresAt) link.read = undefined;
      }
    }
  }

  const server = createServer({ requestTimeout: 120_000, headersTimeout: 5_000, maxHeaderSize: 8_192 }, async (req, res) => {
    res.setHeader('Cache-Control', 'no-store');
    res.setHeader('Referrer-Policy', 'no-referrer');
    res.setHeader('X-Content-Type-Options', 'nosniff');
    res.setHeader('Content-Security-Policy', "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self'; manifest-src 'self'; worker-src 'self'; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'");
    if (secure) res.setHeader('Strict-Transport-Security', 'max-age=31536000');

    try {
      const path = req.url.split('?', 1)[0];
      const asset = assets.get(path);
      if (req.method === 'GET' && asset) {
        res.writeHead(200, { 'Content-Type': asset.type });
        return res.end(asset.bytes);
      }
      expire();
      if (path === '/api/links' && req.method === 'POST') {
        if (registrationHost && (req.headers.host !== registrationHost || req.headers['cf-connecting-ip'])) {
          req.resume();
          return reply(res, 404);
        }
        if (req.headers.origin || Number(req.headers['content-length']) > 0) return reply(res, 400);
        if (links.size >= 48) return reply(res, 503);
        const id = token();
        const host = token();
        const link = { hostHash: digest(host), invite: token(), expiresAt: now() + inviteLifetime, phase: 'scan', attempts: 0 };
        links.set(id, link);
        return reply(res, 201, { id, host, invite: link.invite, expiresAt: link.expiresAt });
      }

      const match = route.exec(path);
      if (!match) return reply(res, 404);
      const [, id, action] = match;
      const link = links.get(id);
      if (!link) return reply(res, 404, { code: 'link_ended' });
      const stillCurrent = () => links.get(id) === link && !expired(link, now());
      const originMatches = req.headers.origin === origin;
      const bearer = req.headers.authorization;
      const isHost = !req.headers.origin && bearer?.startsWith('Bearer ') && matchesCredential(link.hostHash, bearer.slice(7));
      const cookie = req.headers.cookie?.split(';').map(part => part.trim()).find(part => part.startsWith('offtab_view='))?.slice(12);
      const isBrowser = matchesCredential(link.browserHash, cookie);

      if (!action && req.method === 'DELETE' && isHost) {
        if (link.phase === 'paired') savePairings(stateFile, origin, links, id, undefined);
        links.delete(id);
        return reply(res, 204);
      }
      if (action === 'scan' && req.method === 'POST' && originMatches) {
        const payload = await readJson(req, 512);
        if (!stillCurrent() || link.phase !== 'scan' || !equal(link.invite, payload?.invite)) return reply(res, 404);
        if (typeof payload?.publicKey !== 'string' || !validKey.test(payload.publicKey)) return reply(res, 400);
        link.invite = undefined;
        const browser = token();
        link.browserHash = digest(browser);
        link.publicKey = payload.publicKey;
        link.expiresAt = now() + inviteLifetime;
        link.phase = 'pin';
        res.setHeader('Set-Cookie', browserCookie(browser, id, secure));
        return reply(res, 204);
      }
      if (action === 'host' && req.method === 'GET' && isHost) {
        return reply(res, 200, { phase: link.phase, publicKey: link.publicKey, request: link.request, reply: link.reply, context: link.context, read: link.read, expiresAt: link.expiresAt });
      }
      if (action === 'view' && req.method === 'GET') {
        if (!isBrowser) return reply(res, 404, { code: 'link_ended' });
        if (link.phase === 'paired' && (link.cookieRefreshedAt === undefined || now() - link.cookieRefreshedAt >= cookieRefreshInterval)) {
          res.setHeader('Set-Cookie', browserCookie(cookie, id, secure));
          link.cookieRefreshedAt = now();
        }
        if (link.phase === 'paired' && link.snapshot) {
          res.setHeader('ETag', link.snapshot.version);
          const version = req.headers['if-none-match']?.replace(/^W\//, '');
          if (version === link.snapshot.version) return reply(res, 304);
        }
        return reply(res, 200, {
          phase: link.phase, expiresAt: link.expiresAt, attempts: link.attempts, pending: Boolean(link.request),
          snapshot: link.phase === 'paired' ? link.snapshot?.sealed : undefined,
          largeAttachments: link.phase === 'paired' ? true : undefined,
          maxAttachmentBytes: link.phase === 'paired' ? maxAttachmentBytes : undefined,
          readSync: link.phase === 'paired' ? true : undefined,
          sync: link.phase === 'paired'
            ? link.snapshot ? 'ready' : now() < link.pairedAt + snapshotLifetime ? 'waiting' : 'offline'
            : undefined,
        });
      }
      if (action === 'push' && isBrowser && link.phase === 'paired' && notifications) {
        if (req.method === 'GET') return reply(res, 200, { publicKey: notifications.publicKey, enabled: Boolean(link.subscription) });
        if (req.method === 'POST' && originMatches) {
          const payload = await readJson(req, 4096);
          if (!stillCurrent()) return reply(res, 404);
          const subscription = pushSubscription(payload?.subscription);
          if (payload?.subscription !== null && !subscription) return reply(res, 400);
          savePairings(stateFile, origin, links, id, { ...link, subscription });
          link.subscription = subscription;
          return reply(res, 204);
        }
      }
      if (action === 'pin' && req.method === 'POST' && originMatches && isBrowser) {
        const payload = await readJson(req, 256);
        if (!stillCurrent() || link.phase !== 'pin') return reply(res, 404);
        if (link.request || link.attempts >= maxAttempts) return reply(res, 409);
        if (typeof payload?.sealed !== 'string' || !validSealedPin.test(payload.sealed)) return reply(res, 400);
        link.attempts += 1;
        link.request = { id: token(), sealed: payload.sealed };
        return reply(res, 202);
      }
      if (action === 'decision' && req.method === 'POST' && isHost) {
        const payload = await readJson(req, 256);
        if (!stillCurrent() || link.phase !== 'pin' || !equal(link.request?.id, payload?.id)) return reply(res, 409);
        if (typeof payload.accepted !== 'boolean') return reply(res, 400);
        if (payload.accepted) {
          const paired = { ...link, phase: 'paired', pairedAt: now(), expiresAt: null, request: undefined, publicKey: undefined };
          savePairings(stateFile, origin, links, id, paired);
          links.set(id, paired);
        } else {
          link.request = undefined;
          if (link.attempts >= maxAttempts) {
            link.phase = 'locked';
            link.publicKey = undefined;
          }
        }
        return reply(res, 204);
      }
      if (action === 'reply' && req.method === 'POST' && originMatches && isBrowser) {
        if (link.phase !== 'paired') { req.resume(); return reply(res, 404); }
        const contextRequest = req.url === path + '?kind=context';
        const readRequest = req.url === path + '?kind=read';
        const limit = contextRequest || readRequest ? 4608 : maxReplyBody;
        const receiving = contextRequest ? 'receivingContext' : readRequest ? 'receivingRead' : 'receivingReply';
        if (Number(req.headers['content-length']) > limit) { req.resume(); return reply(res, 413); }
        if (link[receiving]) { req.resume(); return reply(res, 409); }
        link[receiving] = true;
        let payload;
        try { payload = await readJson(req, limit); }
        finally { link[receiving] = false; }
        if (!stillCurrent()) return reply(res, 404);
        if (typeof payload?.id !== 'string' || !validReplyId.test(payload.id)
          || typeof payload?.sealed !== 'string' || payload.sealed.length < 38
          || /[^A-Za-z0-9_-]/.test(payload.sealed)) return reply(res, 400);
        if (!link.snapshot || now() >= link.snapshot.expiresAt) return reply(res, 503);
        const slot = payload.kind === 'context' ? 'context' : payload.kind === 'read' ? 'read' : 'reply';
        if (contextRequest && slot !== 'context') return reply(res, 400);
        if (readRequest !== (slot === 'read')) return reply(res, 400);
        if (slot !== 'reply' && payload.sealed.length > 4096) return reply(res, 413);
        if (link[slot]?.id === payload.id) return reply(res, 202);
        if (slot !== 'context' && link[slot] && now() < link[slot].expiresAt) return reply(res, 409);
        link[slot] = { id: payload.id, sealed: payload.sealed, expiresAt: now() + replyLifetime };
        return reply(res, 202);
      }
      if (action === 'reply-ack' && req.method === 'POST' && isHost) {
        const payload = await readJson(req, 256);
        if (!stillCurrent() || link.phase !== 'paired') return reply(res, 404);
        if (payload?.kind === 'read') {
          if (link.read?.id === payload.id) link.read = undefined;
          return reply(res, 204);
        }
        if (link.reply?.id === payload?.id) link.reply = undefined;
        if (link.context?.id === payload?.id) link.context = undefined;
        return reply(res, 204);
      }
      if (action === 'snapshot' && req.method === 'PUT' && isHost) {
        const payload = await readJson(req, 512_000);
        if (!stillCurrent() || link.phase !== 'paired') return reply(res, 404);
        if (typeof payload?.sealed !== 'string' || !/^[A-Za-z0-9_-]{38,}$/.test(payload.sealed)) return reply(res, 400);
        link.snapshot = {
          sealed: payload.sealed,
          version: `"${digest(payload.sealed)}"`,
          expiresAt: now() + snapshotLifetime,
        };
        if (link.subscription && notifications && typeof payload.waiting === 'string' && /^[A-Za-z0-9_-]{22}$/.test(payload.waiting)) {
          const subscribed = link.subscription;
          void notifications.send(subscribed, payload.waiting).catch(error => {
            if (![404, 410].includes(error.statusCode) || !stillCurrent() || link.subscription !== subscribed) return;
            try {
              savePairings(stateFile, origin, links, id, { ...link, subscription: undefined });
              link.subscription = undefined;
            } catch { process.stderr.write('Unable to remove a stale Offtab push subscription.\n'); }
          });
        }
        return reply(res, 204);
      }
      return reply(res, 404);
    } catch {
      if (!res.headersSent) reply(res, 500);
      else res.destroy();
    }
  });

  const timer = setInterval(expire, 5_000);
  timer.unref();
  server.on('close', () => clearInterval(timer));
  return server;
}

if (process.argv[1] && fileURLToPath(import.meta.url) === process.argv[1]) {
  const port = Number(process.env.PORT ?? 63915);
  const host = process.env.HOST ?? '127.0.0.1';
  const configured = new URL(process.env.OFFTAB_RELAY_ORIGIN ?? `http://127.0.0.1:${port}`);
  if (configured.href !== `${configured.origin}/` ||
    (configured.protocol !== 'https:' && !(configured.protocol === 'http:' && configured.hostname === '127.0.0.1' && host === '127.0.0.1'))) {
    throw new Error('Set OFFTAB_RELAY_ORIGIN to an HTTPS origin for a non-loopback relay.');
  }
  const files = [
    ['/', 'index.html', 'text/html; charset=utf-8'],
    ['/app.js', 'app.js', 'text/javascript; charset=utf-8'],
    ['/app.css', 'app.css', 'text/css; charset=utf-8'],
    ['/offtab.png', 'offtab.png', 'image/png'],
    ['/offtab-sw.js', 'offtab-sw.js', 'text/javascript; charset=utf-8'],
    ['/manifest.webmanifest', 'manifest.webmanifest', 'application/manifest+json'],
  ];
  const served = process.env.OFFTAB_RELAY_API_ONLY === '1' ? [] : files;
  const assets = new Map(served.map(([path, file, type]) => [
    path, { type, bytes: readFileSync(new URL(`../dist/mobile/${file}`, import.meta.url)) },
  ]));
  const stateFile = process.env.OFFTAB_RELAY_STATE
    ?? join(homedir(), '.offtab-relay', digest(configured.origin) + '.json');
  const notifications = createNotifications(stateFile, configured.origin);
  const registrationHost = process.env.OFFTAB_RELAY_LOCAL_REGISTRATION === '1' ? `${host}:${port}` : undefined;
  createRelay({ origin: configured.origin, assets, stateFile, notifications, registrationHost }).listen(port, host, () => {
    process.stdout.write(`Offtab relay listening at ${host}:${port}\n`);
  });
}
