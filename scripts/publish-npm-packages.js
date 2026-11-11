#!/usr/bin/env node
// Publish already-packed tarballs to npm — idempotent, retry-aware, honest logs.
//
// Postmortem for 0.3.4 (release run 42485751946, job "Publish platform packages
// to npm" failed): the old inline bash loop ran under `set -e`, published
// `*.tgz` in glob order, and printed only the last three lines of a failed
// attempt. A single 404 on the FIRST tarball
// (nuphus-nuphus-mcp-linux-arm64-0.3.4.tgz) therefore aborted the step before
// the other four were even tried, and the log lost the `npm error code` /
// request line that would have named the real culprit — the publish job was
// never re-run and 0.3.4 had to be published by hand.
//
// This script keeps the same publish semantics and fixes the diagnosis:
//   * every tarball is attempted — one failure no longer hides the other four;
//   * "already published" is decided by asking the registry
//     (npm view <name>@<version>), never by pattern-matching one error string
//     and hoping (the 0.1.5 ghost-publish failure mode);
//   * a failing attempt prints npm's own output (code + method + URL + body),
//     not a three-line tail;
//   * auth/permission failures stop the loop instead of burning five retries
//     per package, and say so;
//   * exit code is 1 iff at least one tarball is neither published nor live.
//
// name+version come from the tarball's package/package.json, so prereleases
// work — the old `ver="${ver##*-}"` slice read the version out of the filename
// and would have turned 0.4.0-rc.1 into "rc.1".
//
// Usage:
//   node scripts/publish-npm-packages.js <tarball>...
//   node scripts/publish-npm-packages.js --dry-run <tarball>...
'use strict';

const { spawnSync } = require('node:child_process');
const path = require('node:path');

const MAX_ATTEMPTS = 5;
// "this exact package@version is already live" — the only acceptable
// publish refusal. npm 10 says "You cannot publish over the previously
// published versions", older/npmjs bodies say E409/EPUBLISHCONFLICT.
const ALREADY_PUBLISHED = /(E409|EPUBLISHCONFLICT|cannot publish over|previously published versions)/i;
const SPAM_BLOCKED = /(spam detection|triggered spam|flagged as spam)/i;
// Kept deliberately narrow: a broad "403" would swallow the publish-conflict
// case above, which is checked first.
const AUTH_FAILURE = /(E401|ENEEDAUTH|unauthorized|forbidden|no permission to publish|authentication token)/i;

const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));
const indent = text => (text || '')
  .split('\n')
  .filter(line => line.trim() !== '')
  .map(line => `    ${line}`)
  .join('\n');

const quoteWinArg = arg => (/[\s"]/.test(arg) ? `"${arg.replace(/"/g, '\\"')}"` : arg);

// npm.cmd cannot be spawned without a shell on Node >= 20.12 (CVE-2024-27980),
// so on Windows the shim goes through cmd.exe with an explicitly quoted line.
function exec (cmd, args) {
  const viaShell = process.platform === 'win32' && /\.(?:cmd|bat)$/i.test(cmd);
  const res = viaShell
    ? spawnSync([cmd, ...args].map(quoteWinArg).join(' '), { encoding: 'utf8', shell: true })
    : spawnSync(cmd, args, { encoding: 'utf8' });
  if (res.error) throw res.error;
  const stdout = res.stdout || '';
  const stderr = res.stderr || '';
  return {
    ok: res.status === 0,
    status: res.status,
    stdout,
    stderr,
    out: [stderr.trim(), stdout.trim()].filter(Boolean).join('\n'),
  };
}

const npmRun = args => exec(process.platform === 'win32' ? 'npm.cmd' : 'npm', args);

function readManifest (tarball) {
  const res = exec('tar', ['-xOzf', tarball, 'package/package.json']);
  if (!res.ok) throw new Error(`cannot read package/package.json from ${tarball}: ${res.out}`);
  const manifest = JSON.parse(res.stdout);
  if (!manifest.name || !manifest.version) throw new Error(`${tarball}: package.json has no name/version`);
  return { name: manifest.name, version: manifest.version };
}

function readPublishedVersion (name, version, run = npmRun) {
  const res = run(['view', `${name}@${version}`, 'version', '--json']);
  if (!res.ok) return null;
  const raw = res.stdout.trim();
  if (!raw) return null;
  try {
    const parsed = JSON.parse(raw);
    return Array.isArray(parsed) ? (parsed[0] ?? null) : parsed;
  } catch {
    return raw.replace(/^"|"$/g, '') || null;
  }
}

const firstErrorLine = out => (out || '').split('\n').find(line => /npm (?:error|ERR!)/.test(line)) || '';

async function publishOne (tarball, options = {}) {
  const {
    dryRun = false,
    run = npmRun,
    readPkg = readManifest,
    log = () => {},
    wait = sleep,
    maxAttempts = MAX_ATTEMPTS,
  } = options;

  const { name, version } = readPkg(tarball);
  const label = `${name}@${version}`;
  const args = ['publish', tarball, '--access', 'public'];
  const result = extra => ({ tarball, name, version, label, ...extra });

  if (dryRun) {
    const res = run([...args, '--dry-run']);
    return result({
      ok: res.ok,
      attempts: 1,
      reason: res.ok ? 'dry run ok' : 'dry run failed',
      output: res.out,
    });
  }

  let lastOutput = '';
  let loggedFullOutput = false;
  for (let attempt = 1; attempt <= maxAttempts; attempt++) {
    const res = run(args);
    if (res.ok) return result({ ok: true, attempts: attempt, reason: 'published', output: res.out });
    lastOutput = res.out;

    if (ALREADY_PUBLISHED.test(res.out)) {
      const live = readPublishedVersion(name, version, run);
      if (live === version) {
        return result({ ok: true, attempts: attempt, reason: 'already published (verified via npm view)', output: res.out });
      }
      return result({ ok: false, attempts: attempt, reason: `${label} is not visible on npm`, output: res.out, fatal: true });
    }

    if (SPAM_BLOCKED.test(res.out)) {
      log(`  spam-blocked (attempt ${attempt}/${maxAttempts}), backing off ${attempt * 12}s`);
      await wait(attempt * 12);
      continue;
    }

    // Full npm output once (that is the evidence the old `tail -3` destroyed),
    // then just the error line, so five retries do not bury the log.
    if (loggedFullOutput) {
      log(`  attempt ${attempt}/${maxAttempts} failed: ${firstErrorLine(res.out)}`);
    } else {
      log(`  attempt ${attempt}/${maxAttempts} failed:`);
      log(indent(res.out));
      loggedFullOutput = true;
    }

    if (AUTH_FAILURE.test(res.out)) {
      return result({ ok: false, attempts: attempt, reason: 'auth/permission failure (see npm output above)', output: res.out, fatal: true });
    }
    await wait(10);
  }

  return result({ ok: false, attempts: maxAttempts, reason: `gave up after ${maxAttempts} attempts`, output: lastOutput });
}

async function main (argv) {
  const dryRun = argv.includes('--dry-run');
  const tarballs = argv.filter(arg => !arg.startsWith('--')).map(file => path.resolve(file));
  if (tarballs.length === 0) {
    console.error('usage: node scripts/publish-npm-packages.js [--dry-run] <tarball>...');
    process.exitCode = 2;
    return;
  }

  const npmVersion = npmRun(['--version']).stdout.trim();
  console.log(`${dryRun ? 'DRY RUN - ' : ''}${tarballs.length} tarball(s), npm ${npmVersion}, node ${process.version}`);
  console.log(`registry: ${npmRun(['config', 'get', 'registry']).stdout.trim()}`);
  console.log(`authenticated as: ${npmRun(['whoami']).stdout.trim() || '(whoami failed - publishing will fail)'}\n`);

  const results = [];
  for (const [index, tarball] of tarballs.entries()) {
    const res = await publishOne(tarball, { dryRun, log: message => console.log(message) });
    results.push(res);
    console.log(`${res.ok ? 'OK  ' : 'FAIL'} ${path.basename(tarball)} [${res.label}] - ${res.reason} (attempts: ${res.attempts})`);
    if (!res.ok) {
      console.log(indent(res.output));
      // One auth/permission wall is enough: the remaining packages would fail
      // the same way, and five retries each only buries the first error.
      if (res.fatal) {
        for (const skipped of tarballs.slice(index + 1)) {
          results.push({ tarball: skipped, ok: false, reason: 'skipped: earlier tarball failed with an auth/permission error', fatal: true });
        }
        break;
      }
    }
    if (!dryRun && index < tarballs.length - 1) await sleep(8);
  }

  const failed = results.filter(res => !res.ok);
  console.log(`\n=== ${results.length - failed.length}/${results.length} tarball(s) published or already live ===`);
  if (failed.length > 0) {
    console.log('\nstill unpublished:');
    for (const res of failed) {
      const label = res.label ? ` (${res.label})` : '';
      console.log(`  - ${path.basename(res.tarball)}${label}: ${res.reason}`);
    }
    process.exitCode = 1;
  }
}

module.exports = { publishOne, readManifest, readPublishedVersion, MAX_ATTEMPTS };

if (require.main === module) {
  main(process.argv.slice(2)).catch(err => {
    console.error(err);
    process.exitCode = 1;
  });
}
