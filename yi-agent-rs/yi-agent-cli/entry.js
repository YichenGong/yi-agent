#!/usr/bin/env node
const { existsSync } = require('fs');
const { join, dirname } = require('path');

const platform = process.platform;
const arch = process.arch;

const platformMap = {
  'darwin-x64': '@gongyichen/yi-agent-darwin-x64',
  'darwin-arm64': '@gongyichen/yi-agent-darwin-arm64',
  'linux-x64': '@gongyichen/yi-agent-linux-x64',
};

const key = `${platform}-${arch}`;
const pkgName = platformMap[key];

if (!pkgName) {
  console.error(`Unsupported platform: ${key}`);
  process.exit(1);
}

let binPath;
try {
  const pkgJsonPath = require.resolve(`${pkgName}/package.json`);
  binPath = join(dirname(pkgJsonPath), 'binaries', 'yi-agent');
} catch (e) {
  console.error(`Platform package ${pkgName} not installed.`);
  process.exit(1);
}

if (!existsSync(binPath)) {
  console.error(`Binary not found at ${binPath}`);
  process.exit(1);
}

const { spawn } = require('child_process');
const child = spawn(binPath, process.argv.slice(2), { stdio: 'inherit' });
child.on('close', (code) => process.exit(code ?? 1));
