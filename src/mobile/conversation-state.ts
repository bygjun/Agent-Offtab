import type { ConversationAttachment, ConversationTurn } from './connection';

export type OutgoingMessage = {
  text: string;
  attachments?: ConversationAttachment[];
  after: number;
  baseline?: string[];
};

export type ConversationPosition = { bottom: boolean; key?: string; offset: number };
export type ConversationCache = { turns?: ConversationTurn[]; answer?: string; position?: ConversationPosition };

export function sameTurns(current: ConversationTurn[] | undefined, incoming: ConversationTurn[]) {
  return Boolean(current && current.length === incoming.length && current.every((old, index) => {
    const next = incoming[index];
    return old.id === next.id && old.at === next.at && old.role === next.role && old.text === next.text
      && old.truncated === next.truncated && old.attachments?.length === next.attachments?.length
      && !old.attachments?.some((file, fileIndex) =>
        file.kind !== next.attachments?.[fileIndex].kind || file.name !== next.attachments[fileIndex].name);
  }));
}

export function turnKey(turn: ConversationTurn) {
  if (turn.id) return turn.id;
  const value = JSON.stringify([turn.role, turn.text, turn.attachments]);
  let hash = 2166136261;
  for (let index = 0; index < value.length; index++) hash = Math.imul(hash ^ value.charCodeAt(index), 16777619);
  return `legacy:${value.length}:${hash >>> 0}`;
}

export function matchOutgoing(turns: ConversationTurn[], message: OutgoingMessage) {
  const baseline = message.baseline ?? [];
  const keys = turns.map(turnKey);
  // Older Mac builds do not report turn IDs or timestamps. Match their
  // rolling history only after a known overlap with the pre-send history.
  let legacyStart = -1;
  if (turns.some(turn => !turn.at && !turn.id)) {
    for (let length = Math.min(baseline.length, keys.length); length > 0; length--) {
      const suffix = baseline.slice(-length);
      const match = (_: string, start: number) => suffix.every((key, offset) => keys[start + offset] === key);
      const index = keys.findIndex(match);
      if (index !== -1 && keys.findLastIndex(match) === index) {
        legacyStart = index + length;
        break;
      }
    }
  }
  return turns.findIndex((turn, index) => {
    if (turn.role !== 'user' || turn.truncated) return false;
    if (turn.at) {
      if (turn.at <= message.after || baseline.includes(keys[index])) return false;
    } else if (turn.id || legacyStart < 0 || index < legacyStart) return false;
    const sentText = message.text.trim();
    const turnText = turn.text.trim();
    const imageOnly = !sentText && message.attachments?.some(file => file.kind === 'image');
    if (sentText !== turnText && !(imageOnly && turnText === '첨부한 사진을 확인해 주세요.')) return false;
    if (!turn.id) return true;
    const expected = (message.attachments ?? []).map(file => file.kind).sort().join(',');
    const actual = (turn.attachments ?? []).map(file => file.kind).sort().join(',');
    return expected === actual;
  });
}
