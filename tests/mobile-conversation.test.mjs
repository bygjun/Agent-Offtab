import assert from 'node:assert/strict';
import { test } from 'node:test';
import { build } from 'esbuild';

const bundle = await build({
  entryPoints: ['src/mobile/conversation-state.ts'], bundle: true, format: 'esm', platform: 'node', write: false,
});
const { matchOutgoing, turnKey } = await import(`data:text/javascript;base64,${Buffer.from(bundle.outputFiles[0].contents).toString('base64')}`);

const user = (text, at, id, attachments) => ({ role: 'user', text, at, id, attachments, truncated: false });

test('links a reply to the new transcript turn and leaves an identical earlier message alone', () => {
  const before = user('확인', 50, 'old');
  const message = { text: '확인', after: 100, baseline: [turnKey(before)] };
  assert.equal(matchOutgoing([before], message), -1);
  assert.equal(matchOutgoing([before, user('확인', 105, 'new')], message), 1);
});

test('does not replace an attachment reply with a text-only turn', () => {
  const photo = { kind: 'image', name: 'example.png' };
  const message = { text: '', after: 100, attachments: [photo] };
  assert.equal(matchOutgoing([user('', 105, 'different', [{ kind: 'file' }])], message), -1);
  assert.equal(matchOutgoing([user('첨부한 사진을 확인해 주세요.', 105, 'new', [{ kind: 'image' }])], message), 0);
});

test('uses an unambiguous history overlap for legacy Mac builds', () => {
  const old = user('같은 질문');
  const answer = { role: 'assistant', text: '이전 답변' };
  assert.equal(matchOutgoing([old, answer, old], { text: old.text, after: 100, baseline: [turnKey(old), turnKey(answer)] }), 2);
  assert.equal(matchOutgoing([old, old], { text: old.text, after: 100, baseline: [turnKey(old)] }), -1);
});
