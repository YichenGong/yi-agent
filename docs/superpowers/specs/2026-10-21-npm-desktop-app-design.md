# macOS 桌面 App 随 npm 分发 Design

日期：2026-10-21
状态：设计已确认（用户拍板：装进 npm 主渠道；只在全局安装时复制到 `/Applications`）

## 1. 背景

桌面 App（`.app` / `.dmg`）目前只有一条分发路径：GitLab 的 `release:macos-dmg`
job 把 dmg 传到 GitLab Generic Package Registry，内网同事到
**Deploy → Releases** 手动下载（`README.md:44-52`）。

公网用户拿不到桌面 App：GitHub Release 只有 CLI 的 `.tar.gz`，npm 只有 CLI。
用户希望「装 npm 包就能拿到桌面 App」，不必去 GitLab 找 dmg。

## 2. 已核实的事实

设计前实测（2026-10-21），这些数字决定了方案取舍：

| 事实 | 证据 |
|---|---|
| `.app` 未压缩 47 MB，压缩后 18.7 MB | `du -sh` / `tar -czf` 实测 |
| `.app` 内 CLI sidecar 占 40.9 MB（压缩 15.2 MB），GUI 主二进制仅 5.5 MB | `find -exec ls -la` + 分别压缩 |
| sidecar **不能删**：GUI 通过它跑 agent，`tauri.conf.json` 的 `externalBin` 声明了它 | `tauri.conf.json` `externalBin: ["binaries/yi-agent"]`；删掉后 GUI 能起但功能残缺 |
| 现有 `darwin-arm64` 子包 unpackedSize 13.2 MB，tarball 约 15 MB | registry `0.1.3` 的 `dist.unpackedSize` |
| Tauri 产出的 `.app` 签名是**坏的**：`code has no resources but signature indicates they must be present` | `codesign --verify --deep` 实测 |
| `codesign --force --deep --sign -` 能修复它（Info.plist 绑定、sealed resources 正常） | 修复后 `codesign --verify` 通过 |
| 修复后的 `.app` 经 `npm pack` → `npm install` 往返，签名仍有效、能正常启动 | 端到端实测，`rc=0` + 进程存活 |
| npm 安装的 `.app` **不带** `com.apple.quarantine`，可被消费 | `xattr -l` 为空；带 quarantine 时也可启动 |
| npm registry tarball 上限 256 MB | npm/GitHub Packages 文档 |
| `npm_config_global` 在 `-g` 时为 `"true"`，本地安装时未定义 | postinstall 探针实测 |
| `/Applications` 是 `drwxrwxr-x root admin`，admin 组成员可写 | `ls -ld` + `id -Gn` |
| `codesign` 在 `/usr/bin/codesign`，系统自带，无需 Xcode CLT | `ls -la /usr/bin/codesign` |
| GitHub runner 有 Xcode 26.6，是 arm64 | `xcodebuild -version` / `uname -m` |

## 3. 决策

| 决策 | 选择 | 理由 |
|---|---|---|
| 放到哪个包 | **`@yi-agent/yi-agent-darwin-arm64` 子包**，不是主包 | 用户要「一次 `npm i -g` 装好」。放子包同样满足，但 Linux / Intel Mac 用户**不会**多下 18 MB。放主包会让所有平台都背上这个体积 |
| 何时复制到 `/Applications` | **仅 `npm i -g`**（`npm_config_global === "true"`） | 用户拍板。避免本包作为别人的依赖被装进 `node_modules` 时污染 `/Applications` |
| 签名 | 打包时 `codesign --force --deep --sign -`（ad-hoc） | Tauri 产出的签名是坏的，不修则 `codesign --verify` 失败。不买 Apple 证书（符合现有「先不签名」的取舍） |
| 用 dmg 还是 .app | **`.app` 目录**，不用 dmg | .app 可直接复制和启动；dmg 还要挂载，且需 `hdiutil`，在 npm 场景纯属多余 |
| 已有版本怎么办 | 比对版本，相同则跳过；不同则替换 | 重复 `npm i -g` 不该反复动 `/Applications` |
| 现有 GitLab dmg 流程 | **保留不动** | 内网分发继续用它，两条路径互不影响 |

## 4. 架构

```
构建期（CI，GitHub Actions / Mac mini arm64）
  desktop/scripts/package-npm-app.sh
    1. 版本门禁：tag 的版本 == Cargo.toml 版本（复用 release-dmg.sh 的逻辑）
    2. jq 注入 tauri.conf.json 的 version；收窄 bundle.targets 为 ["app"]
    3. npm ci（desktop/）
    4. npm run sidecar:release   → 编译 release 模式 CLI 并放进 src-tauri/binaries
    5. npm run tauri build       → 产出 yi-agent.app
    6. codesign --force --deep --sign -  → 修复签名
    7. codesign --verify --deep --strict → 修不好就构建失败
    8. 复制到 yi-agent-cli/platforms/darwin-arm64/yi-agent.app/
    9. 写 .sha256 便于发布前校验

发布期
  just npm-pack 已有的 sync-npm-versions.sh 同步版本
  darwin-arm64/package.json 的 files 含 "yi-agent.app/"  → 自动进 tarball
  npm publish（现有流程，无需改动）

安装期（用户机器，Apple Silicon）
  npm i -g @yi-agent/yi-agent
    → 装主包 + darwin-arm64 子包
    → 子包 postinstall 触发
    → npm_config_global === "true" 且 平台是 darwin/arm64 才继续
    → 从父安装目录解析 /Applications 目标版本
    → 版本不同才复制
    → xattr -dr com.apple.quarantine（幂等）
```

### 组件职责

- `desktop/scripts/package-npm-app.sh` — 唯一的 .app→npm 打包入口。
  只管「产出一个已签名、已就位的 .app」，不管上传。
- `yi-agent-cli/platforms/darwin-arm64/package.json` — 新增 `files: ["binaries/", "yi-agent.app/"]`
  与 `postinstall`。
- `yi-agent-cli/platforms/darwin-arm64/install-app.js` — postinstall 的实现。
  只做「判断该不该装 → 装」，不做构建。
- `.github/workflows/release.yml` — 在 `npm-pack` 之前插入「构建桌面 App」step。

## 5. install-app.js 的行为

必须是**失败安全**的：装桌面 App 失败绝不能导致 `npm i -g` 失败，
否则用户连 CLI 都用不了。

```
1. 若 npm_config_global !== "true"        → 静默退出 0
2. 若 process.platform !== "darwin"       → 退出 0
   若 process.arch !== "arm64"             → 退出 0
3. 若 /Applications 不可写                 → 打印提示（需 sudo）→ 退出 0
4. 读本次安装的 .app 的 CFBundleShortVersionString
   读 /Applications/yi-agent.app 的版本（不存在则视为未安装）
5. 版本相同 → 打印「已是最新」→ 退出 0
   版本不同/未安装 → 复制
6. 清 quarantine（xattr，失败不致命）
7. 任何异常 → 打印原因 → 退出 0（绝不非 0）
```

第 3 步的提示很关键：企业机器上 `/Applications` 常常不可写，
此时必须明确告诉用户「CLI 已装好，桌面 App 需 sudo 手动安装」，而不是静默跳过。

## 6. 错误处理

| 情况 | 行为 |
|---|---|
| tauri build 失败 | CI 失败，不发 npm（构建是发布的前置） |
| codesign 修复后仍 verify 失败 | CI 失败。坏的签名装到用户机器上会无法启动 |
| `/Applications` 不可写 | postinstall 打印 sudo 提示，退出 0 |
| 复制途中失败 | 清理半成品目录，退出 0 |
| 已有版本更新 | 先复制到临时目录再原子替换（沿用 `desktop/packaging/install.sh:208` 的做法） |
| 用户在 Linux/Intel | postinstall 立即退出，零开销 |

## 7. 测试

- `package-npm-app.sh` 的版本门禁：沿用 `release-dmg.sh` 的校验逻辑，
  加一个「tag 与 Cargo.toml 不一致必须失败」的用例。
- `install-app.js` 用 Node 内置 `node:test` 覆盖纯判断逻辑：
  - 非全局安装 → 不复制
  - 非 darwin → 不复制
  - 版本相同 → 不复制
  - 版本不同 → 复制
  - `/Applications` 不可写 → 退出 0 且给出提示
  把「是否该装」「是否该替换」抽成纯函数，便于测试。
- 端到端（手动/一次性）：本地 `npm pack` → `npm i -g` → 检查
  `/Applications/yi-agent.app` 存在且能启动。这条路径已在设计阶段实测通过。

## 8. 取舍与风险

- **体积**：Apple Silicon 用户多下约 18 MB。这是「一次装好」的代价，
  已用子包隔离，不外溢到其他平台。
- **未公证**：App 仍是 ad-hoc 签名。npm 安装路径不带 quarantine，实测可启动；
  但若用户从别的途径（浏览器）拿到同一个 App，仍会被 Gatekeeper 拦。
  这与现状一致，不是本次引入的问题。
- **CI 时间**：新增一次 Tauri 构建（约 10 分钟），且需要 macOS arm64 runner。
  GitHub Actions 的 release job 本来就在 Mac mini self-hosted 上跑，已满足。
- **不改 GitLab dmg 流程**：两条分发路径并存，未来若要统一需另开议题。

## 9. 不在本次范围

- Apple 公证 / Developer ID 签名
- Windows / Linux 桌面 App
- 把桌面 App 放进主包（已明确否决）
- 改动 GitLab 的 dmg 发布流程
