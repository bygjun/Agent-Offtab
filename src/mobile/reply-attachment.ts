export const maxAttachments = 4;
export const bytesPerMB = 1024 * 1024;
export type AttachmentCapability = { maxAttachmentBytes?: number; largeAttachments?: boolean };

export function attachmentLimit(capability?: AttachmentCapability) {
  const { maxAttachmentBytes, largeAttachments } = capability ?? {};
  if (typeof maxAttachmentBytes === 'number' && Number.isSafeInteger(maxAttachmentBytes) && maxAttachmentBytes > 0) {
    return Math.min(maxAttachmentBytes, 20 * bytesPerMB);
  }
  return (largeAttachments === true ? 10 : 2) * bytesPerMB;
}

const maxText = 16 * 1024;
const imageTypes = new Set(['image/jpeg', 'image/png', 'image/webp']);
const textFile = /\.(?:txt|md|csv|json|log|ts|js|css|html|xml|ya?ml)$/i;

export const attachmentTypes = 'image/jpeg,image/png,image/webp,.heic,.heif,.txt,.md,.csv,.json,.log,.ts,.js,.css,.html,.xml,.yaml,.yml';
export type ReplyAttachment = { name: string; mime: string; data: string };

export function attachmentBytes(attachment: ReplyAttachment) {
  return attachment.data.length * 3 / 4 - (attachment.data.endsWith('==') ? 2 : attachment.data.endsWith('=') ? 1 : 0);
}

export function base64Bytes(bytes: Uint8Array) {
  let binary = '';
  for (let offset = 0; offset < bytes.length; offset += 16_384) {
    binary += String.fromCharCode(...bytes.subarray(offset, offset + 16_384));
  }
  return btoa(binary);
}

async function shrinkPhoto(file: File) {
  const photo = await createImageBitmap(file);
  try {
    const scale = Math.min(1, 1600 / Math.max(photo.width, photo.height));
    const canvas = document.createElement('canvas');
    canvas.width = Math.max(1, Math.round(photo.width * scale));
    canvas.height = Math.max(1, Math.round(photo.height * scale));
    const context = canvas.getContext('2d', { alpha: false });
    if (!context) throw new Error();
    context.fillStyle = '#fff';
    context.fillRect(0, 0, canvas.width, canvas.height);
    context.drawImage(photo, 0, 0, canvas.width, canvas.height);
    return await new Promise<Blob | null>(resolve => canvas.toBlob(resolve, 'image/jpeg', .84));
  } finally {
    photo.close();
  }
}

export async function prepareAttachment(file: File, maxImageBytes: number): Promise<ReplyAttachment> {
  if (!file.size) throw new Error('빈 파일은 첨부할 수 없습니다.');
  if (!file.name.trim() || Array.from(file.name).length > 128 || /[/\\\u0000-\u001f\u007f]/u.test(file.name)) {
    throw new Error('파일 이름을 확인해 주세요.');
  }
  if (file.type === 'text/plain' || textFile.test(file.name)) {
    if (file.size > maxText) throw new Error('텍스트 파일은 16KB까지 첨부할 수 있습니다.');
    const bytes = new Uint8Array(await file.arrayBuffer());
    try { new TextDecoder('utf-8', { fatal: true }).decode(bytes); }
    catch { throw new Error('UTF-8로 저장된 텍스트 파일을 선택해 주세요.'); }
    return { name: file.name, mime: 'text/plain', data: base64Bytes(bytes) };
  }
  if (!imageTypes.has(file.type) && !/\.(?:heic|heif)$/i.test(file.name)) {
    throw new Error('JPEG, PNG, WebP 사진이나 텍스트 파일을 선택해 주세요.');
  }
  let photo: Blob | null = file;
  if (file.size > maxImageBytes || !imageTypes.has(file.type)) {
    try { photo = await shrinkPhoto(file); }
    catch { throw new Error('사진을 열 수 없습니다. JPEG, PNG, WebP로 다시 선택해 주세요.'); }
  }
  if (!photo || photo.size > maxImageBytes) throw new Error(`사진을 ${maxImageBytes / bytesPerMB}MB 이하로 줄여 주세요.`);
  return { name: file.name, mime: photo.type, data: base64Bytes(new Uint8Array(await photo.arrayBuffer())) };
}
