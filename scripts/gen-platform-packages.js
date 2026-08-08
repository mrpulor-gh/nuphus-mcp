#!/usr/bin/env node
// Generate the 5 platform-package skeletons under npm/packages/nuphus-mcp-<platform>-<arch>.
// Each platform package declares os/cpu and points its bin at the compiled binary.
// The actual binary is copied in by the release workflow (or manually for local dev);
// this script only writes the package.json + a placeholder .gitkeep so the dirs exist.
//
// Platform set mirrors what ORT 1.27 ships in its NuGet all-platform package:
//   win32-x64, win32-arm64, linux-x64, linux-arm64, osx-arm64 (no Intel-mac lib).
'use strict';

const fs = require('fs');
const path = require('path');

const ROOT = path.join(__dirname, '..');
const OUT_DIR = path.join(ROOT, 'npm', 'packages');

const VERSION = require(path.join(ROOT, 'npm', 'packages', 'nuphus-mcp', 'package.json')).version;

const PLATFORMS = [
  { platform: 'win32', arch: 'x64', exe: 'nuphus-mcp.exe' },
  { platform: 'win32', arch: 'arm64', exe: 'nuphus-mcp.exe' },
  { platform: 'linux', arch: 'x64', exe: 'nuphus-mcp' },
  { platform: 'linux', arch: 'arm64', exe: 'nuphus-mcp' },
  { platform: 'osx', arch: 'arm64', exe: 'nuphus-mcp' },
];

// npm matches a package's `os` field against Node's process.platform, which is
// "darwin" on macOS — "osx" never matches, so mac users get notsup from the
// optional dependency. The platform suffix in the package NAME/dir stays "osx"
// (keeps the published @nuphus/nuphus-mcp-osx-arm64 name stable); only the os
// FIELD value must be npm's platform name.
const NPM_OS = { win32: 'win32', linux: 'linux', osx: 'darwin' };

for (const p of PLATFORMS) {
  // Scoped package name — must match the name the meta package depends on.
  // Regression: an unscoped name ("nuphus-mcp-win32-x64") made npm treat each
  // platform package as a brand-new package and trigger spam detection (E403).
  const pkgName = `@nuphus/nuphus-mcp-${p.platform}-${p.arch}`;
  const dir = path.join(OUT_DIR, `nuphus-mcp-${p.platform}-${p.arch}`);
  fs.mkdirSync(path.join(dir, 'bin'), { recursive: true });

  const manifest = {
    name: pkgName,
    version: VERSION,
    description: `nuphus-mcp binary for ${p.platform} ${p.arch}. Installed automatically by the nuphus-mcp meta package.`,
    license: 'MIT',
    repository: {
      type: 'git',
      url: 'git+https://github.com/mrpulor-gh/nuphus-mcp.git',
    },
    os: [NPM_OS[p.platform]],
    cpu: [p.arch],
    bin: {
      'nuphus-mcp': path.join('bin', p.exe),
    },
    files: ['bin'],
    scripts: {
      // Local-publish guard: refuses `npm publish` when bin/ holds a compiled
      // binary (a workstation stale binary must never ship). CI packs via
      // `npm pack` + `npm publish <tgz>`, which does not run prepublishOnly.
      prepublishOnly: 'node ./scripts/check-clean-bin.js',
    },
  };

  fs.writeFileSync(path.join(dir, 'package.json'), JSON.stringify(manifest, null, 2) + '\n');
  // The guard script ships with the package skeleton (single source of truth:
  // the meta package's copy).
  fs.mkdirSync(path.join(dir, 'scripts'), { recursive: true });
  fs.copyFileSync(
    path.join(ROOT, 'npm', 'packages', 'nuphus-mcp', 'scripts', 'check-clean-bin.js'),
    path.join(dir, 'scripts', 'check-clean-bin.js')
  );
  // Placeholder so the dir is non-empty before CI copies the real binary in.
  const gitkeep = path.join(dir, 'bin', '.gitkeep');
  if (!fs.existsSync(gitkeep)) fs.writeFileSync(gitkeep, '');
  console.log(`generated ${pkgName}`);
}

console.log('Done. Copy the compiled binary into each platform package bin/ before npm pack.');