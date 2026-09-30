// SPDX-License-Identifier: MPL-2.0
import test from 'node:test';
import assert from 'node:assert/strict';
import { buildInvite } from './join.mjs';

const payload = '010007070707070707070707070707070707514011766f6963652e6578616d706c652e636f6d';
// Rust protocol::Invite 也检查同一组向量，防止浏览器邀请与客户端格式分叉。
test('public and Unicode private invitations match Rust protocol vectors', async () => {
  assert.equal(await buildInvite(payload), 'gouhuo://j/0400e1r70w3ge1r70w3ge1r70w3gema025v6ytb3cmq6ay31dnr6rs9ecdqpt1v8');
  assert.equal(await buildInvite(payload, '开黑+朋友'), 'gouhuo://j/040ge1r70w3ge1r70w3ge1r70w3gema025v6ytb3cmq6ay31dnr6rs9ecdqpt3f5qj0ekewh5fk9s2z5hy5ry3r');
});
test('invalid payloads and oversized UTF-8 credentials fail', async () => {
  for (const invalid of ['', 'xx', '0100', payload.replace(/^01/, '02')]) {
    await assert.rejects(buildInvite(invalid));
  }
  await assert.rejects(buildInvite(payload, '中'.repeat(86)), /加入码太长/);
  assert.ok((await buildInvite(payload, '中'.repeat(85))).startsWith('gouhuo://j/'));
});
