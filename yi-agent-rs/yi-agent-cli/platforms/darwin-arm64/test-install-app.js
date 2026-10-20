const test = require('node:test');
const assert = require('node:assert');
const {
  shouldInstall,
  needsReplace,
  parseAppVersion,
  isWritable,
} = require('./install-app.js');

test('非全局安装时不安装', () => {
  const r = shouldInstall({}, 'darwin', 'arm64');
  assert.equal(r.ok, false);
  assert.match(r.reason, /not a global install/);
});

test('全局安装且 darwin/arm64 时安装', () => {
  const r = shouldInstall({ npm_config_global: 'true' }, 'darwin', 'arm64');
  assert.equal(r.ok, true);
});

test('全局安装但在 linux 上不安装', () => {
  const r = shouldInstall({ npm_config_global: 'true' }, 'linux', 'x64');
  assert.equal(r.ok, false);
  assert.match(r.reason, /linux/);
});

test('全局安装但 darwin/x64 不安装', () => {
  const r = shouldInstall({ npm_config_global: 'true' }, 'darwin', 'x64');
  assert.equal(r.ok, false);
  assert.match(r.reason, /x64/);
});

test('未装过时需要安装', () => {
  assert.equal(needsReplace(null, '0.1.8'), true);
  assert.equal(needsReplace('', '0.1.8'), true);
});

test('版本相同不替换', () => {
  assert.equal(needsReplace('0.1.8', '0.1.8'), false);
});

test('版本不同需要替换', () => {
  assert.equal(needsReplace('0.1.7', '0.1.8'), true);
});

test('能从 Info.plist 解析出版本号', () => {
  const plist = `<?xml version="1.0"?>
<plist version="1.0"><dict>
  <key>CFBundleShortVersionString</key>
  <string>1.2.3</string>
  <key>CFBundleVersion</key>
  <string>1.2.3</string>
</dict></plist>`;
  assert.equal(parseAppVersion(plist), '1.2.3');
});

test('Info.plist 缺版本号时返回 null', () => {
  assert.equal(parseAppVersion('<plist><dict></dict></plist>'), null);
});

test('isWritable 对不存在的目录返回 false', () => {
  assert.equal(isWritable('/nonexistent-dir-for-test-xyz'), false);
});

test('isWritable 对可写目录返回 true', () => {
  assert.equal(isWritable(require('os').tmpdir()), true);
});
