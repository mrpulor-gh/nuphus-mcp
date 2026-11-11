#!/usr/bin/env node
// Unit tests for scripts/publish-npm-packages.js — no registry, no network.
// Run: node --test scripts/tests/publish-npm-packages.test.js
'use strict';

const test = require('node:test');
const assert = require('node:assert');

const { publishOne, readPublishedVersion } = require('../publish-npm-packages.js');

const LABEL = { name: '@nuphus/nuphus-mcp-linux-arm64', version: '0.3.4' };
const readPkg = () => LABEL;
const quiet = () => {};
const noWait = async () => {};

// Shapes copied from real npm output (npm 10.8.2 / 10.9.3).
const CONFLICT = [
  'npm error code E403',
  'npm error 403 403 Forbidden - PUT https://registry.npmjs.org/@nuphus%2fnuphus-mcp-linux-arm64 - You cannot publish over the previously published versions: 0.3.4.',
].join('\n');
const NOT_FOUND = [
  'npm error code E404',
  'npm error 404 Not Found - PUT https://registry.npmjs.org/@nuphus%2fnuphus-mcp-linux-arm64 - Not found',
  "npm error 404  '@nuphus/nuphus-mcp-linux-arm64@0.3.4' is not in this registry.",
].join('\n');
const SPAM = [
  'npm error code E403',
  'npm error 403 403 Forbidden - PUT https://registry.npmjs.org/@nuphus%2fnuphus-mcp-linux-arm64 - This package has been flagged as spam.',
].join('\n');
const UNAUTHORIZED = [
  'npm error code E401',
  'npm error 401 Unauthorized - PUT https://registry.npmjs.org/@nuphus%2fnuphus-mcp-linux-arm64',
].join('\n');

const fakeRun = handler => args => handler(args);

test('publish refusal + version live on npm counts as success (idempotent re-run)', async () => {
  const calls = [];
  const run = fakeRun(args => {
    calls.push(args.join(' '));
    if (args[0] === 'publish') return { ok: false, stdout: '', stderr: CONFLICT, out: CONFLICT };
    return { ok: true, stdout: '"0.3.4"', stderr: '', out: '"0.3.4"' };
  });

  const res = await publishOne('x.tgz', { run, readPkg, wait: noWait, log: quiet });

  assert.equal(res.ok, true);
  assert.equal(res.attempts, 1);
  assert.match(res.reason, /already published \(verified via npm view\)/);
  assert.deepEqual(calls, [
    'publish x.tgz --access public',
    'view @nuphus/nuphus-mcp-linux-arm64@0.3.4 version --json',
  ]);
});

test('publish refusal with an invisible version fails instead of faking success', async () => {
  const run = fakeRun(args => (args[0] === 'publish'
    ? { ok: false, stdout: '', stderr: CONFLICT, out: CONFLICT }
    : { ok: false, stdout: '', stderr: 'npm error code E404', out: '' }));

  const res = await publishOne('x.tgz', { run, readPkg, wait: noWait, log: quiet });

  assert.equal(res.ok, false);
  assert.equal(res.fatal, true);
  assert.match(res.reason, /not visible on npm/);
});

test('a 404 keeps retrying and never reports success', async () => {
  let attempts = 0;
  const run = fakeRun(args => {
    if (args[0] !== 'publish') return { ok: false, stdout: '', stderr: '', out: '' };
    attempts++;
    return { ok: false, stdout: '', stderr: NOT_FOUND, out: NOT_FOUND };
  });

  const res = await publishOne('x.tgz', { run, readPkg, wait: noWait, log: quiet });

  assert.equal(res.ok, false);
  assert.equal(attempts, 5);
  assert.match(res.reason, /gave up after 5 attempts/);
});

test('spam block backs off and then publishes', async () => {
  const waits = [];
  let attempts = 0;
  const run = fakeRun(args => {
    if (args[0] !== 'publish') return { ok: true, stdout: '"0.3.4"', stderr: '', out: '' };
    attempts++;
    if (attempts === 1) return { ok: false, stdout: '', stderr: SPAM, out: SPAM };
    return { ok: true, stdout: 'published', stderr: '', out: 'published' };
  });

  const res = await publishOne('x.tgz', { run, readPkg, wait: async ms => waits.push(ms), log: quiet });

  assert.equal(res.ok, true);
  assert.equal(res.attempts, 2);
  assert.deepEqual(waits, [12]);
});

test('a 401 stops immediately instead of burning every retry', async () => {
  let attempts = 0;
  const run = fakeRun(args => {
    if (args[0] !== 'publish') return { ok: false, stdout: '', stderr: '', out: '' };
    attempts++;
    return { ok: false, stdout: '', stderr: UNAUTHORIZED, out: UNAUTHORIZED };
  });

  const res = await publishOne('x.tgz', { run, readPkg, wait: noWait, log: quiet });

  assert.equal(res.ok, false);
  assert.equal(res.fatal, true);
  assert.equal(attempts, 1);
});

test('readPublishedVersion tolerates plain, quoted and missing answers', () => {
  const answer = stdout => ({ run: () => ({ ok: true, stdout, stderr: '', out: stdout }) });

  assert.equal(readPublishedVersion('p', '1.0.0', answer('"1.0.0"').run), '1.0.0');
  assert.equal(readPublishedVersion('p', '1.0.0', answer('1.0.0').run), '1.0.0');
  assert.equal(readPublishedVersion('p', '1.0.0', answer('["1.0.0"]').run), '1.0.0');
  assert.equal(readPublishedVersion('p', '1.0.0', answer('').run), null);
  assert.equal(readPublishedVersion('p', '1.0.0', () => ({ ok: false, stdout: '', stderr: '', out: '' })), null);
});
