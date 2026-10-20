#!/usr/bin/env node
// postinstall：把随包分发的 .app 复制到 /Applications。
//
// 硬性要求：任何情况下都退出 0。桌面 App 装不上不能拖垮 CLI 安装——
// 用户敲 `npm i -g` 的首要目的是拿到 yi-agent 命令。
//
// 只在 `npm i -g` 时动手，避免本包被当作依赖装进别人的 node_modules
// 时顺手污染 /Applications。
'use strict';

const fs = require('fs');
const path = require('path');
const { execFileSync } = require('child_process');

const APP_NAME = 'yi-agent.app';
const TARGET_DIR = '/Applications';

/// 判断是否该在本机安装桌面 App。
/// 返回 { ok, reason }；ok 为 false 时 reason 说明原因（便于日志排查）。
function shouldInstall(env, platform, arch) {
  if (env.npm_config_global !== 'true') {
    return { ok: false, reason: 'not a global install' };
  }
  if (platform !== 'darwin') {
    return { ok: false, reason: `platform ${platform} is not darwin` };
  }
  if (arch !== 'arm64') {
    return { ok: false, reason: `arch ${arch} is not arm64` };
  }
  return { ok: true, reason: 'ok' };
}

/// 已有版本与待装版本不同才需要替换。没装过（任一侧为空）视为需要装。
function needsReplace(installedVersion, bundledVersion) {
  if (!installedVersion) return true;
  if (!bundledVersion) return true;
  return installedVersion !== bundledVersion;
}

/// 从 Info.plist 文本里取 CFBundleShortVersionString。
function parseAppVersion(plistText) {
  const m = plistText.match(
    /<key>CFBundleShortVersionString<\/key>\s*<string>([^<]+)<\/string>/
  );
  return m ? m[1].trim() : null;
}

function readAppVersion(appPath) {
  try {
    const plist = fs.readFileSync(
      path.join(appPath, 'Contents', 'Info.plist'),
      'utf8'
    );
    return parseAppVersion(plist);
  } catch (e) {
    return null;
  }
}

function isWritable(dir) {
  try {
    fs.accessSync(dir, fs.constants.W_OK);
    return true;
  } catch (e) {
    return false;
  }
}

/// 复制 .app 并清 quarantine。先复制到临时目录再原子替换，
/// 避免中途失败留下半个 App（沿用 desktop/packaging/install.sh 的做法）。
function installApp(sourceApp, targetApp) {
  const staging = `${targetApp}.staging-${process.pid}`;
  fs.rmSync(staging, { recursive: true, force: true });
  fs.cpSync(sourceApp, staging, { recursive: true, dereference: true });

  // ad-hoc 重签。Tauri 产出的签名是坏的（codesign --verify 报
  // "code has no resources but signature indicates they must be present"），
  // 复制后重签可修复。失败不致命：签名坏了多半仍能启动。
  try {
    execFileSync('codesign', ['--force', '--deep', '--sign', '-', staging], {
      stdio: 'ignore',
    });
  } catch (e) {
    console.warn(`[yi-agent] codesign failed (non-fatal): ${e.message}`);
  }

  fs.rmSync(targetApp, { recursive: true, force: true });
  fs.renameSync(staging, targetApp);

  // 从 npm 装的包不带 quarantine，但用户若曾手工拷过同路径可能残留标记。
  try {
    execFileSync('xattr', ['-dr', 'com.apple.quarantine', targetApp], {
      stdio: 'ignore',
    });
  } catch (e) {
    // 没有该属性时 xattr 返回非 0，忽略。
  }
}

function main() {
  try {
    const verdict = shouldInstall(process.env, process.platform, process.arch);
    if (!verdict.ok) {
      // 静默退出：本地开发安装不该有任何输出噪音。
      return;
    }

    const sourceApp = path.join(__dirname, APP_NAME);
    if (!fs.existsSync(sourceApp)) {
      console.log('[yi-agent] desktop app not found in package, skipping');
      return;
    }

    const targetApp = path.join(TARGET_DIR, APP_NAME);
    const bundledVersion = readAppVersion(sourceApp);
    const installedVersion = readAppVersion(targetApp);

    if (!needsReplace(installedVersion, bundledVersion)) {
      console.log(
        `[yi-agent] desktop app ${bundledVersion} already in ${TARGET_DIR}`
      );
      return;
    }

    if (!fs.existsSync(TARGET_DIR) || !isWritable(TARGET_DIR)) {
      console.log(
        `[yi-agent] cannot write to ${TARGET_DIR} (needs admin). ` +
          `CLI is installed; to add the desktop app run:\n` +
          `  sudo cp -R "${sourceApp}" "${TARGET_DIR}/"`
      );
      return;
    }

    installApp(sourceApp, targetApp);
    console.log(
      `[yi-agent] desktop app ${bundledVersion} installed to ${targetApp}`
    );
  } catch (e) {
    // 兜底：绝不因装饰性功能让 install 失败。
    console.warn(`[yi-agent] desktop app install skipped: ${e.message}`);
  }
}

module.exports = { shouldInstall, needsReplace, parseAppVersion, isWritable };

if (require.main === module) {
  main();
  process.exit(0);
}
