import { base64Bytes, type AttachmentCapability, type ReplyAttachment } from './reply-attachment';
import type { ReadUpdate } from '../types';

export interface Device {
  id: string;
  pinKey: CryptoKey;
  snapshotKey: CryptoKey;
  replyKey?: CryptoKey;
  draftKey?: CryptoKey;
}

export interface RelayView extends AttachmentCapability {
  phase: 'pin' | 'paired' | 'locked';
  pending: boolean;
  attempts: number;
  expiresAt: number | null;
  snapshot?: string;
  sync?: 'waiting' | 'ready' | 'offline';
  readSync?: boolean;
}

const encoder = new TextEncoder();
const tokenPattern = /^[A-Za-z0-9_-]{43}$/;
const publicKeyPattern = /^B[A-Za-z0-9_-]{86}$/;

function decode(data: string) {
  const padded = data.replace(/-/g, '+').replace(/_/g, '/');
  return Uint8Array.from(atob(padded), char => char.charCodeAt(0));
}

function encode(data: Uint8Array) {
  return base64Bytes(data).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
}

async function accessDevice<T = Device>(mode: IDBTransactionMode, apply: (store: IDBObjectStore) => IDBRequest) {
  const database = await new Promise<IDBDatabase>((resolve, reject) => {
    const open = indexedDB.open('offtab-mobile', 1);
    open.onupgradeneeded = () => open.result.createObjectStore('connection');
    open.onsuccess = () => resolve(open.result);
    open.onerror = () => reject(open.error);
  });
  try {
    return await new Promise<T | undefined>((resolve, reject) => {
      const transaction = database.transaction('connection', mode);
      const request = apply(transaction.objectStore('connection'));
      transaction.oncomplete = () => resolve(request.result);
      transaction.onabort = () => reject(transaction.error);
    });
  } finally {
    database.close();
  }
}

function replyKeys(id: string) {
  const prefix = 'replies:' + id + ':';
  return IDBKeyRange.bound(prefix, prefix + '\uffff');
}

export function forgetDevice(id: string) {
  return accessDevice('readwrite', store => {
    const request = store.get('active');
    request.onsuccess = () => {
      if (request.result?.id === id) store.delete('active');
      store.delete(replyKeys(id));
    };
    return request;
  });
}

export async function loadDevice() {
  const device = await accessDevice('readonly', store => store.get('active'));
  if (!device || device.draftKey) return device;
  const draftKey = await crypto.subtle.generateKey({ name: 'AES-GCM', length: 256 }, false, ['encrypt', 'decrypt']);
  return accessDevice('readwrite', store => {
    const request = store.get('active');
    request.onsuccess = () => {
      if (request.result?.id !== device.id || request.result.draftKey) return;
      request.result.draftKey = draftKey;
      store.put(request.result, 'active');
    };
    return request;
  });
}

export async function loadReplyHistory(device: Device): Promise<unknown> {
  if (!device.draftKey) return undefined;
  const key = device.draftKey;
  const records = await accessDevice<Uint8Array<ArrayBuffer>[]>('readonly', store => store.getAll(replyKeys(device.id)));
  const entries = await Promise.all((records ?? []).map(async sealed => {
    const plaintext = await crypto.subtle.decrypt({ name: 'AES-GCM', iv: sealed.subarray(0, 12) }, key, sealed.subarray(12));
    return JSON.parse(new TextDecoder().decode(plaintext)) as [string, unknown];
  }));
  return Object.fromEntries(entries);
}

export async function saveReplyHistory(device: Device, changes: Record<string, object | null>) {
  if (!device.draftKey) return;
  const key = device.draftKey;
  const entries = await Promise.all(Object.entries(changes).map(async ([name, value]) => {
    if (!value) return [name, null] as const;
    const nonce = crypto.getRandomValues(new Uint8Array(12));
    const encoded = encoder.encode(JSON.stringify([name, value]));
    const ciphertext = await crypto.subtle.encrypt({ name: 'AES-GCM', iv: nonce }, key, encoded);
    const sealed = new Uint8Array(12 + ciphertext.byteLength);
    sealed.set(nonce);
    sealed.set(new Uint8Array(ciphertext), 12);
    return [name, sealed] as const;
  }));
  if (!entries.length) return;
  await accessDevice('readwrite', store => {
    const request = store.get('active');
    request.onsuccess = () => {
      if (request.result?.id !== device.id) return;
      for (const [name, sealed] of entries) {
        const entry = 'replies:' + device.id + ':' + name;
        if (sealed) store.put(sealed, entry);
        else store.delete(entry);
      }
    };
    return request;
  });
}

function request(id: string, path: string, data?: object, timeout = 5_000) {
  return fetch(`/api/links/${id}/${path}`, {
    cache: 'no-store', credentials: 'same-origin', signal: AbortSignal.timeout(timeout),
    ...(data && { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(data) }),
  });
}

export async function scan(fragment: string, signal?: AbortSignal) {
  const [id, invite, hostKey, extra] = fragment.slice(1).split('.');
  if (extra !== undefined || !tokenPattern.test(id) || !tokenPattern.test(invite) || !publicKeyPattern.test(hostKey)) {
    throw new Error('연결 주소가 올바르지 않습니다. Mac에서 QR을 다시 찍으세요.');
  }
  if (!crypto.subtle) throw new Error('HTTPS 연결에서만 휴대폰을 등록할 수 있습니다.');

  const keyPair = await crypto.subtle.generateKey({ name: 'ECDH', namedCurve: 'P-256' }, false, ['deriveBits']);
  const publicKey = await crypto.subtle.importKey('raw', decode(hostKey), { name: 'ECDH', namedCurve: 'P-256' }, false, []);
  const shared = await crypto.subtle.deriveBits({ name: 'ECDH', public: publicKey }, keyPair.privateKey, 256);
  const material = await crypto.subtle.importKey('raw', shared, 'HKDF', false, ['deriveKey']);
  new Uint8Array(shared).fill(0);
  const salt = decode(id);
  const derive = (context: string, usages: KeyUsage[]) => crypto.subtle.deriveKey(
    { name: 'HKDF', hash: 'SHA-256', salt, info: encoder.encode(context) },
    material, { name: 'AES-GCM', length: 256 }, false, usages,
  );
  const [pinKey, snapshotKey, replyKey, draftKey, browserKey] = await Promise.all([
    derive('offtab/pin/v1', ['encrypt']),
    derive('offtab/snapshot/v1', ['decrypt']),
    derive('offtab/reply/v1', ['encrypt']),
    crypto.subtle.generateKey({ name: 'AES-GCM', length: 256 }, false, ['encrypt', 'decrypt']),
    crypto.subtle.exportKey('raw', keyPair.publicKey),
  ]);

  const device = { id, pinKey, snapshotKey, replyKey, draftKey };
  signal?.throwIfAborted();
  const response = await request(id, 'scan', { invite, publicKey: encode(new Uint8Array(browserKey)) });
  if (response.status === 404) {
    throw new Error('이 QR은 만료되었거나 이미 사용했습니다. Mac에서 새 QR을 만드세요.');
  }
  if (!response.ok) throw new Error('중계 서버에 등록하지 못했습니다. 잠시 후 다시 시도하세요.');
  signal?.throwIfAborted();
  try {
    await accessDevice('readwrite', store => {
      const request = store.get('active');
      request.onsuccess = () => {
        if (request.result && request.result.id !== device.id) store.delete(replyKeys(request.result.id));
        store.put(device, 'active');
      };
      return request;
    });
  } catch {
    throw new Error('이 브라우저에 연결 정보를 저장할 수 없습니다. 브라우저 설정을 확인하고 새 QR을 찍으세요.');
  }
  return device;
}

export async function submitPin(device: Device, pin: string) {
  const nonce = crypto.getRandomValues(new Uint8Array(12));
  const sealed = new Uint8Array(34);
  sealed.set(nonce);
  sealed.set(new Uint8Array(await crypto.subtle.encrypt({ name: 'AES-GCM', iv: nonce }, device.pinKey, encoder.encode(pin))), 12);
  const response = await request(device.id, 'pin', { sealed: encode(sealed) });
  if (response.status === 404) throw new Error('연결이 만료되었습니다. Mac에서 새 QR을 찍으세요.');
  if (response.status === 409) throw new Error('Mac에서 앞서 보낸 PIN을 확인하고 있습니다.');
  if (!response.ok) throw new Error('PIN을 전달하지 못했습니다. 잠시 후 다시 시도하세요.');
}

export async function view(device: Device, version?: string) {
  const response = await fetch(`/api/links/${device.id}/view`, {
    cache: 'no-store',
    credentials: 'same-origin',
    signal: AbortSignal.timeout(5_000),
    headers: version ? { 'If-None-Match': version } : undefined,
  });
  if (response.status === 304 && version) return { unchanged: true } as const;
  if (response.status === 404) {
    const error = await response.json().catch(() => undefined);
    if (error?.code === 'link_ended') return undefined;
  }
  if (!response.ok) throw new Error('중계 서버와 연결할 수 없습니다.');
  const state: RelayView = await response.json();
  return { unchanged: false, state, version: response.headers.get('ETag') ?? undefined } as const;
}

export async function openSnapshot(device: Device, sealed: string) {
  const bytes = decode(sealed);
  const plaintext = await crypto.subtle.decrypt({ name: 'AES-GCM', iv: bytes.subarray(0, 12) }, device.snapshotKey, bytes.subarray(12));
  return JSON.parse(new TextDecoder().decode(plaintext));
}

export type ReplyTarget = { id: string; updatedAt: number; title: string };
export type ReplyCode = 'processing' | 'queued' | 'sent' | 'blocked' | 'invalid-attachment' | 'busy' | 'unavailable' | 'unknown'
  | 'desktop-permission' | 'desktop-locked' | 'desktop-draft' | 'desktop-target';
export type Receipt = { id: string; code: ReplyCode };
export type ConversationTurn = { role: 'assistant' | 'user'; text: string; truncated: boolean };
export type ReplyContext = { id: string; answer: string | null; turns?: ConversationTurn[]; failed: boolean };

export function queueReply(device: Device, id: string, target: ReplyTarget, text: string, permit: string, attachments: ReplyAttachment[], sentAt: number, onUpload?: (percent: number) => void) {
  return sendReplyRequest(device, id, { target: target.id, text, permit, updatedAt: target.updatedAt, attachments, sentAt }, attachments.length ? onUpload : undefined);
}

export function requestReplyContext(device: Device, id: string, target: string, historyLimit: number, permit?: string | null) {
  return sendReplyRequest(device, id, { kind: 'context', target, permit, updatedAt: 0, historyLimit });
}

export function queueReads(device: Device, updates: ReadUpdate[]) {
  const reads = updates.map(({ id, version, at }) => ({ id, version, at }));
  return sendReplyRequest(device, crypto.randomUUID(), { kind: 'read', reads });
}

function uploadReply(id: string, data: object, progress: (percent: number) => void) {
  return new Promise<number>((resolve, reject) => {
    const xhr = new XMLHttpRequest();
    xhr.open('POST', `/api/links/${id}/reply`);
    xhr.timeout = 100_000;
    xhr.setRequestHeader('Content-Type', 'application/json');
    xhr.upload.onprogress = event => {
      if (event.lengthComputable) progress(Math.round(event.loaded * 100 / event.total));
    };
    xhr.onload = () => resolve(xhr.status);
    xhr.onerror = xhr.ontimeout = xhr.onabort = () => reject(new Error('Upload was interrupted.'));
    xhr.send(JSON.stringify(data));
  });
}

async function sendReplyRequest(device: Device, id: string, fields: { kind?: 'context' | 'read'; [key: string]: unknown }, onUpload?: (percent: number) => void): Promise<ReplyCode | undefined> {
  if (!device.replyKey) return 'blocked';
  const nonce = crypto.getRandomValues(new Uint8Array(12));
  const payload = encoder.encode(JSON.stringify({ sentAt: Date.now(), ...fields, id }));
  const encrypted = new Uint8Array(await crypto.subtle.encrypt({ name: 'AES-GCM', iv: nonce }, device.replyKey, payload));
  const sealed = new Uint8Array(nonce.length + encrypted.length);
  sealed.set(nonce);
  sealed.set(encrypted, nonce.length);
  const data = { id, sealed: encode(sealed), kind: fields.kind };
  const status = onUpload ? await uploadReply(device.id, data, onUpload)
    : (await request(device.id, fields.kind ? `reply?kind=${fields.kind}` : 'reply', data)).status;
  if (status === 202) return;
  if (status === 409) return 'busy';
  if (status === 404 || status === 503) return 'unavailable';
  if (status === 400) return 'blocked';
  if (status === 413) return 'invalid-attachment';
  return 'unknown';
}
