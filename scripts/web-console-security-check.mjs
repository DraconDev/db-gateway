#!/usr/bin/env node
import { readFile, stat } from 'node:fs/promises';
import { join } from 'node:path';
import { strict as assert } from 'node:assert';

const root = new URL('../', import.meta.url).pathname;
const files = ['web/src/server.ts', 'web/src/api.ts', 'web/public/app.js', 'web/public/index.html'];
for (const file of files) {
  const text = await readFile(join(root, file), 'utf8');
  assert.doesNotMatch(text, /localStorage|sessionStorage|document\.cookie|Authorization: Bearer/, `${file} contains browser credential persistence/transport`);
  assert.doesNotMatch(text, /0\.0\.0\.0|\[::\]/, `${file} contains a non-loopback bind`);
}
const server = await readFile(join(root, 'web/src/server.ts'), 'utf8');
assert.match(server, /DG_CONSOLE_HOST must be loopback/);
assert.match(server, /credential file must not be accessible by group or others/);
assert.match(server, /content-type.*application\/json/);
console.log(JSON.stringify({ ok: true, checked_files: files.length, owner_key_in_browser: false, non_loopback_bind: false }));
