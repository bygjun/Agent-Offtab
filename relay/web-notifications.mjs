import { createHash } from 'node:crypto';
import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { dirname } from 'node:path';
import { generateVAPIDKeys, sendNotification } from 'web-push';

export function pushSubscription(value) {
  if (!value || typeof value.endpoint !== 'string' || value.endpoint.length > 2048
    || !/^B[A-Za-z0-9_-]{86}$/.test(value.keys?.p256dh ?? '')
    || !/^[A-Za-z0-9_-]{22}$/.test(value.keys?.auth ?? '')) return;
  let url;
  try { url = new URL(value.endpoint); } catch { return; }
  const allowed = url.hostname.endsWith('.push.apple.com')
    || url.hostname === 'fcm.googleapis.com' || url.hostname === 'updates.push.services.mozilla.com';
  if (url.protocol !== 'https:' || url.username || url.password || url.port || !allowed || url.hash) return;
  return { endpoint: url.href, keys: { p256dh: value.keys.p256dh, auth: value.keys.auth } };
}

export function createNotifications(stateFile, origin) {
  if (!origin.startsWith('https:')) return undefined;
  const path = stateFile + '.push.json';
  let keys;
  try { keys = JSON.parse(readFileSync(path, 'utf8')); }
  catch (error) {
    if (error.code !== 'ENOENT') throw error;
    keys = generateVAPIDKeys();
    mkdirSync(dirname(path), { recursive: true, mode: 0o700 });
    try { writeFileSync(path, JSON.stringify(keys), { mode: 0o600, flag: 'wx', flush: true }); }
    catch (failure) {
      if (failure.code !== 'EEXIST') throw failure;
      keys = JSON.parse(readFileSync(path, 'utf8'));
    }
  }
  return {
    publicKey: keys.publicKey,
    send: (subscription, id) => sendNotification(subscription, JSON.stringify({ id }), {
      TTL: 120,
      timeout: 5000,
      topic: createHash('sha256').update(id).digest('base64url').slice(0, 32),
      vapidDetails: { ...keys, subject: origin },
    }),
  };
}
