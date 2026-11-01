# 手机 APP 版本号跟随主线

**目标：** iOS App 的版本号不再是写死的 `0.1.0`，而是跟随
`yi-agent-rs/Cargo.toml` 的 `[workspace.package] version`（版本号唯一真源）；
并且「漂移」这件事在构建链与 CI 两侧都拦得住。

**状态：** 已设计，待实现。

---

## 1. 现状与问题（已核实）

主线工作区版本为 `0.1.13`（`yi-agent-rs/Cargo.toml:22`）。手机 APP 的版本没有跟它走，
硬编码在三个被 git 追踪的文件里：

| 文件 | 键 | 值 |
| --- | --- | --- |
| `desktop/src-tauri/tauri.conf.json:4` | `.version` | `0.1.0` |
| `desktop/src-tauri/gen/apple/project.yml` | `CFBundleShortVersionString` / `CFBundleVersion` | `0.1.0` |
| `desktop/src-tauri/gen/apple/desktop_iOS/Info.plist` | `CFBundleShortVersionString` / `CFBundleVersion` | `0.1.0` |

harness 实测：本地既有的构建产物
`desktop/src-tauri/gen/apple/build/desktop_iOS.xcarchive/Info.plist` 与其中
`Products/Applications/yi-agent.app/Info.plist`，两处 `CFBundleShortVersionString`
均为 `0.1.0`——即「装到手机上的包当然是 0.1.0」，不是显示层的错觉。

**为什么桌面端没这个问题**：macOS 的 `.app` 走的是构建时注入——
`desktop/scripts/package-npm-app.sh:53` 用 `jq` 把 Cargo 版本写进
`tauri.conf.json`，`scripts/rebuild.sh` 用 `--config` 注入，
`.github/workflows/release.yml:160` 还会校验产物
`CFBundleShortVersionString == workspace version`。

`scripts/rebuild.sh` 此前被 `.git/info/exclude` 的 `/scripts/` 规则挡在仓库外，
只存在于本机；本次已把它纳入版本控制（`chore(scripts)` 提交），因此它现在是一条
可依赖的链路，dev 桌面构建也跟 workspace 版本走。

**为什么 iOS 漏了**：这条链两头都空。

1. `docs/relay-deploy.md:377` 记录的构建命令是裸的 `npx tauri ios build --ci --export-method debugging`，
   没有任何版本注入；
2. `.github/workflows/` 里 `grep -i 'ios|xcode'` 零命中——CI 根本不构建 iOS，也就没有
   release.yml 那样的产物版本校验。

`desktop/package.json` 与 `desktop/src-tauri/Cargo.toml` 同样停在 `0.1.0`，但见 §5 的非目标说明。

### 1.1 为什么必须两处都写 `project.yml`

`project.yml` 是 `desktop.xcodeproj` 与 `desktop_iOS/Info.plist` 的**生成源**：
跑一次 `npx tauri ios init` 会按它重生成工程。这是 `CFBundleDisplayName` 已经踩过的坑
（`docs/superpowers/specs/2026-10-11-ios-app-display-name-design.md:25`）——只改产物不写生成源，
下次 init 就被抹掉。因此两处都写。

反方向也要成立：`desktop.xcodeproj/project.pbxproj:392` / `:424` 的 `INFOPLIST_FILE`
指向 `desktop_iOS/Info.plist`，且工程里没有 `MARKETING_VERSION` 或 `INFOPLIST_KEY_*`
会覆盖它——所以改 `Info.plist` 是有效的，不是自欺。

---

## 2. 范围

### 2.1 做什么

- **注入脚本** `desktop/scripts/sync-ios-version.sh`（新增）：从 Cargo 读版本，写进
  §1 表格的三处；带 `--check` 只比对不改。
- **构建链接入**：`desktop/package.json` 新增 `ios:build`，让注入成为 iOS 构建的必经步骤。
- **文档**：`docs/relay-deploy.md` 的构建命令改用 `npm run ios:build`。
- **门禁**：`yi-agent-rs/justfile` 新增 `ios-version-check` 并加入 `ci:`；
  `desktop/scripts/test_ios_version_sync.sh`（新增）做 fixture 自测 + 仓库快照断言。
- **bump 流程**：`chore: bump` 提交必须带上注入产物，否则 `just ci` 红。

### 2.2 不做什么（非目标，YAGNI）

- **不改 `desktop/package.json` 的 `version`**：该包 `"private": true`，不发布到 npm，
  运行时也没有任何代码读它（`desktop/src` 里 `getVersion` / `CFBundleShortVersionString`
  零命中）。为它增加一个同步点只会多一处会漂的地方。
- **不改 `desktop/src-tauri/Cargo.toml` 的 `version`**：改它必须连带改已提交的
  `desktop/src-tauri/Cargo.lock`，而 iOS 的版本由 `tauri.conf.json` 的 `version` 显式提供
  （Tauri 文档：`version` 只在**省略时**才回退到 `Cargo.toml`），不产生行为价值。
  它保留 `0.1.0` 是 Tauri 模板的默认值。
- **不做 Android**：手机构建只有 iOS 一条路（`docs/superpowers/specs/2026-10-02-mobile-remote-access-design.md` §2.2 把 Android 划进 v2）。
- **不动 app-server 上报的版本**：`initialize` 回的 `serverInfo.version` 取
  `env!("CARGO_PKG_VERSION")`（`yi-agent-app-server/src/server.rs:3830`），天生就跟 workspace
  版本一致，无需干预。手机端也还没有展示它的 UI。
- **不做 iOS CI 构建**：iOS 构建要 Xcode + 真机/模拟器运行时，GitHub/GitLab runner 上不具备
  （`docs/project-management/desktop.md:76` 记录本机都曾被缺失的 Simulator 组件阻塞）。
  本次只在本地构建链与静态门禁上闭合，不引入 CI 构建任务。

---

## 3. 设计

### 3.1 真源与跟随面

唯一真源：`yi-agent-rs/Cargo.toml` 的 `[workspace.package] version`。
它已经在被这些地方消费：`justfile` 的 `version` 变量（:169）、`package-npm-app.sh:34`、
`release-dmg.sh`、`release.yml:62`。本次新增 iOS 一处，沿用同一读法
（`grep '^version' | head -1 | awk -F'"'`），不引入第二套解析。
跟随面（三处，共 5 行键值）：

1. `desktop/src-tauri/tauri.conf.json` → `.version`
2. `desktop/src-tauri/gen/apple/project.yml` → `CFBundleShortVersionString`、`CFBundleVersion`
3. `desktop/src-tauri/gen/apple/desktop_iOS/Info.plist` →
   `<key>CFBundleShortVersionString</key>` 与 `<key>CFBundleVersion</key>` 各自的 `<string>`

### 3.2 注入脚本

```
desktop/scripts/sync-ios-version.sh              # 读 Cargo 版本，写入三处；幂等
desktop/scripts/sync-ios-version.sh --check      # 只比对；一致 exit 0，漂移 exit 1
```

退出码约定（与 `yi-agent-rs/scripts/sync-npm-versions.sh` 同构，便于调用方分诊）：

| 码 | 含义 |
| --- | --- |
| 0 | 已同步 / 已一致 |
| 1 | `--check` 检出漂移（逐处打印「期望 X，实际 Y」） |
| 2 | 用法错误、读不到 Cargo 版本、或版本不是 `X.Y.Z` |

**版本格式硬校验**：必须是 `^[0-9]+\.[0-9]+\.[0-9]+$`。理由不是洁癖——
`CFBundleShortVersionString` 必须恰好三段整数，带 prerelease 时 Tauri 会**静默**截断并打印一行
警告（CLI 二进制里实测该字符串：`CFBundleShortVersionString cannot have prerelease tag; stripping from`），
产物版本与主线版本就此分叉而无人察觉。宁可 exit 2 显式失败。

**实现要点**

- `SCRIPT_DIR` 按自身路径解析（`cd "$(dirname "$0")" && pwd`），再上溯两级定位仓库根；
  从任何 cwd 调用都成立。
- `project.yml` 的正则**保留原有引号风格**而不是统一成一种：本文件里
  `CFBundleShortVersionString` 不带引号、`CFBundleVersion` 带引号，把两者统一会多出
  一行与版本无关的 diff（实测过一次，被只改版本行的约束挡回来）。
- `Info.plist` **不能用 `PlistBuddy Set`**：实测它重写整个文件——键顺序变了
  （`CFBundleDisplayName`、`NSCameraUsageDescription` 被挪位）、文件末尾多出换行。
  值虽然对，但 diff 从 2 行涨到 14 行，review 时看不出真正改了什么。改用 awk 维护
  「上一行是哪把 key」的状态，只替换目标 `<key>` 之后第一个 `<string>`；写后再用
  `plutil -lint` 确认产物仍是合法 plist。
- 末尾换行特征要原样保留：这份 `Info.plist` 本来没有末尾换行，awk 会补一个，
  于是凭空多一条 `\ No newline at end of file`。
- 全程只改目标键，其余字节不动（幂等：连跑两次，`git diff` 第二次为空——已实测）。

### 3.3 构建链接入

`desktop/package.json`：

```json
"ios:build": "bash scripts/sync-ios-version.sh && tauri ios build"
```

`docs/relay-deploy.md` §4.2 的构建步骤改为：

```bash
npm run ios:build -- --ci --export-method debugging
```

（`--` 之后的参数原样透传给 `tauri ios build`，`--export-method debugging` 与 `--ci` 语义不变。）

这样「注入」不是可选礼仪，而是构建命令的一部分——忘记注入的那个包根本构建不出来。

### 3.4 门禁

`desktop/scripts/test_ios_version_sync.sh`，照 `yi-agent-rs/scripts/test_sync_npm_versions.sh`
的结构（`check` / `check_contains` / `check_absent` 计数器 + fixture + 退出码断言）：

**A. fixture 自测**（在 `mktemp -d` 里铺一份三文件 mini 结构，Cargo 版本 `9.9.9`）

1. 注入后三处都是 `9.9.9`；
2. 幂等：连跑两次，第二次前后字节一致（`shasum` 比对整个文件）；
3. 只动目标键：同一文件里的 `CFBundleDisplayName`、`bundle.targets`、`icons` 等保持不变；
4. `CFBundleVersion` 缺失时能 `Add` 出来；
5. `--check` 一致时 exit 0；
6. `--check` 漂移时 exit 1，且输出里同时出现期望值与实际值；
7. 非法版本（`0.1.13-rc1`、`abc`）exit 2，且**没有**任何文件被改写；
8. 读不到 Cargo 版本 exit 2。

**B. 仓库快照断言**（对真实仓库跑）

在仓库根跑 `bash desktop/scripts/sync-ios-version.sh --check`，必须 exit 0。
这一条把「提交进仓库的值 == 主线版本」钉死——也就是 bump 忘记同步时 `just ci` 变红的地方。

`yi-agent-rs/justfile`：

```
# iOS 版本必须跟随 workspace 版本：写死的 0.1.0 会让手机 APP 与主线分叉。
ios-version-check:
    bash ../desktop/scripts/test_ios_version_sync.sh
```

并加入 `ci:` 依赖列表（与 `npm-version-check`、`desktop-target-dir-check` 同级）。

### 3.5 bump 流程

`chore: bump` 提交从此必须包含 `bash desktop/scripts/sync-ios-version.sh` 的产物。
写入 `docs/project-management/desktop.md` 的 iOS 条目，并按项目规则同步更新
`README.md` 模块索引表的计数。

---

## 4. 验证

| 层 | 命令 | 期望 |
| --- | --- | --- |
| 注入脚本自测 + 快照 | `bash desktop/scripts/test_ios_version_sync.sh` | 全绿（25 项：A 组注入行为 + B 组仓库快照） |
| CI 入口 | `cd yi-agent-rs && just ci` | 12 → 13 道门禁全绿 |
| 前端静态门禁 | `cd desktop && npx vitest run src/iosDisplayName.test.ts && npx tsc --noEmit` | 6 例绿（2 例显示名 + 4 例版本跟随） |
| 前端整体未受影响 | `cd desktop && npx vitest run` | 1828 例全绿（原 1824 + 新增 4） |
| 真实产物（可离线复核） | 构建后 `bash desktop/scripts/sync-ios-version.sh --check` | 仍 exit 0（证明 `tauri ios build` 没有回写版本） |

**变异验证**（已逐条实跑，每条都确实变红，否则门禁是假的）：

| 变异 | 结果 |
| --- | --- |
| `--check` 的漂移分支改成 `exit 0` | 自测例 5 红 |
| 删掉 `project.yml` 写入 | 自测例 1b/1c 红（回读校验另把它压成 exit 2） |
| 删掉 `Info.plist` 写入 | 自测例 1d/1e 红（同上 exit 2） |
| 放行非法版本（去掉 `X.Y.Z` 校验） | 自测例 6 红 ×3 |
| 把 `Info.plist` 里的版本改回 `0.1.0` | 前端门禁 `iosDisplayName.test.ts` 的版本断言红 |

**真机构建无法本次验收**：`xcrun devicectl list devices` 当前回
`Yichen iPhone ... unavailable`，装包与「设置里显示的版本号」需要设备在线。
故以 §4 表格的离线证据交付；真机复核（重构建 → 装包 → 进 iOS 设置看版本号 == 主线版本）
留待设备可用时补做，步骤与命令见 `docs/relay-deploy.md` §4.2。

---

## 5. 风险与已知限制

- **仓库快照断言的取舍**：B 组门禁要求「提交进去的值 == 主线版本」，因此 `just bump` 必须同步
  iOS 三处。这是刻意的：代价是三行手改（或跑一次脚本），换来的是「看 git 就知道手机 APP
  是哪个版本」。若将来嫌麻烦，可把同步塞进 bump 的收尾脚本，门禁不必动。
- **`tauri ios init` 不读 `--config` 的版本，且会抹掉 `Info.plist` 的手工合并**
  （本次实测，两条都要记）：在主仓库上跑
  `tauri ios init -c '{"version":"9.9.9"}'`，`project.yml` 一行未变——即 `-c` 的版本
  对 init 无效，版本只能靠「先写 `project.yml`，再 init」。同一次 init 还把
  `Info.plist` 里两条手工合并的键抹掉了：`CFBundleDisplayName`（`Yi-Agent`）被挪位
  （值还在，因它本就在 `project.yml` 里），而 **`NSCameraUsageDescription` 整条消失**
  ——它只由 `src-tauri/Info.ios.plist` 在构建期合并进来，init 重生成 `Info.plist`
  时并不看那个文件。结论：**跑完 `tauri ios init` 必须重跑一次本脚本**（版本回 0.1.0
  会被 `--check` 抓住），相机权限则在 `tauri ios build` 时由 Tauri 重新合并。
  实测后已把主仓库这两处改动还原，未提交。
  为降低踩坑成本，`ios:build` 里先跑本脚本再调 `tauri ios build`，顺序天然正确。
- **`CFBundleVersion` 与 `CFBundleShortVersionString` 取同一值**：App Store 要求
  `CFBundleVersion` 单调递增，若将来同版本要多次上传，需要引入 build number
  （`tauri ios build --build-number`）。当前走 development 签名侧载，不受此约束，故不预先设计。
