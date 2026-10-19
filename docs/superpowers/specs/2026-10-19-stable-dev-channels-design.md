# 稳定版 / 开发版分道与多实例并行 — 设计

日期：2026-10-19
状态：已批准（D1–D5 与三项待批事项均已确认）

## 1. 问题

当前仓库只有 `main` 一条长期分支，`v0.1.0`–`v0.1.7` 的 tag 直接打在 `main` 上。本机
`scripts/rebuild.sh` 会把本地编译产物直接 `cp` 到 `~/.cargo/bin/yi-agent` 并软链到
`/opt/homebrew/bin/yi-agent`，覆盖掉发布渠道安装的二进制。结果是：

- "稳定版"没有明确的定义：它可能是 npm/brew 装的，也可能是某个下午随手编的 `main`；
- 无法回滚到已知良好的版本，因为没有留下版本化的安装；
- 无法在同一台机器上同时运行两个版本做对比验证；
- 所有实例共享 `$HOME/.yi-agent`，dev 的实验会污染 stable 的设备配对、模型清单、
  trace 日志与常驻登记。

## 2. 目标与非目标

### 目标

1. 稳定版是**已发布 tag 的构建**，升级与回滚都是带版本号的安装。
2. 开发版是**某个 worktree 的当前构建**，一条命令创建、一条命令销毁，不进发布渠道。
3. 开发版的任何操作不得改变稳定版的状态、进程、launchd 注册与二进制。
4. 同一 worktree 内允许稳定版与开发版同时运行。

### 非目标

- 不做开发版的桌面 App bundle、dmg、npm 包或 Homebrew formula。
- 不做多个稳定版共存与 `current` 软链切换。
- 不做开发版的 launchd 登录自启值守。
- 不做 crates.io 发布。

## 3. 设计原则

稳定版与开发版的投入必须不对称：

| | 稳定版 | 开发版 |
|---|---|---|
| 定义 | 已发布 tag 的构建 | 某 worktree 的当前构建 |
| 生命周期 | 长期，跨项目 | 一次验证任务 |
| 来源 | npm / Homebrew | 本机 `cargo build` |
| 失败代价 | 影响日常开发，必须可回滚 | 删掉重建 |
| 允许的复杂度 | 高 | 必须低 |

由此得出硬规则：**任何只为开发版增加的基础设施，如果不同时服务稳定版，就不做。**
D4 的取舍直接由这条规则决定。

## 4. 总体结构

```
stable   yi-agent            ~/.yi-agent/                      ← npm / brew 安装
dev-A    yi-agent-dev        ~/.yi-agent-instances/<id-A>/     ← worktree A
dev-B    yi-agent-dev        ~/.yi-agent-instances/<id-B>/     ← worktree B
                                                               （可同时运行）

项目级（按 worktree 天然隔离）
         <worktree>/.yi-agent/runtime/   ← 独占实例锁 + runtime.sock
		 <worktree>/.yi-agent/superpowers-kanban/、logs/、supervisors/
```

三层边界：

1. **宿主态**按实例切分，由 `YI_AGENT_HOME` 决定；
2. **项目态**按 worktree 切分，由 `YI_AGENT_RUNTIME_DIR`（已存在）决定；
3. **网络**由内核分配（`127.0.0.1:0`），无冲突。

## 5. 决策

### D1 分支模型：`main` + `release/X.Y`

- `main` 是开发主干。`feat/*`、`fix/*`、`kanban/*` 在 worktree 里开发，`merge --no-ff`
  回 `main`。
- 发版：从 `main` 切 `release/X.Y`，版本 bump 与 tag 都落在这条分支上。
- hotfix：从 `release/X.Y` 切 `fix/*`，合回该 release 分支后打 patch tag，再回灌 `main`。
- tag 仍为 `vX.Y.Z`，`release.yml` 的 tag 校验逻辑不变。
- 需要补的缺口：
  - CI 触发增加 `release/**`（现状只有 `pull_request: branches: [main]`）；
  - `main` 上禁止打 tag（分支保护或发布前检查）。

被否方案：`develop` + `main` 双长期分支。单人维护下每次发布多一次合并，收益接近零。

### D2 版本号：构建期注入，不入库

- `yi-agent-rs/Cargo.toml` 始终是下一个正式版的干净号，永不写 `-dev` 后缀。
- 新增 `crates/yi-agent/build.rs`：
  - `git describe --tags --exact-match --match 'v*'` 成功 → release；
  - 否则 → dev，附 `git rev-parse --short HEAD` 与 dirty 标记。
- `yi-agent --version`：release 输出 `0.1.8`；dev 输出
  `0.1.8-dev+ad81221-dirty`。
- `build.rs` 必须声明 `rerun-if-changed` 指向 `.git/HEAD` 与 `.git/index`，否则
  checkout 切换后版本号不刷新。
- 理由：`release.yml` 与 `desktop/scripts/release-dmg.sh` 都要求 tag 与
  `Cargo.toml` 版本严格相等。把 `-dev` 写进 `Cargo.toml` 会被自己的发布门禁拦下。

### D3 状态隔离：单一 `YI_AGENT_HOME`

新增 `crates/yi-agent-store/src/paths.rs`，作为唯一的宿主路径解析点：

```
instance_home()   = $YI_AGENT_HOME，缺省 $HOME/.yi-agent
skills_dir()      = $YI_AGENT_SKILLS_DIR，缺省 instance_home()/skills
runtime_dir(wd)   = $YI_AGENT_RUNTIME_DIR，缺省 <wd>/.yi-agent/runtime   （已存在）
os_home()         = 真实 $HOME，仅供 launchd 与 LaunchAgents 使用
```

必须替换的调用点（均为生产代码）：

| 文件 | 现状用途 |
|---|---|
| `crates/yi-agent-store/src/resident.rs:13` | `resident-daemons.json` |
| `crates/yi-agent-runtime/src/config.rs:186` | 全局 `.env` |
| `crates/yi-agent-runtime/src/models.rs:287` | `models.json` |
| `crates/yi-agent-supervisors/src/supervisor.rs:44` | 全局 `preferences.json` |
| `crates/yi-agent-app-server/src/device_store.rs:51` | `devices.json` |
| `crates/yi-agent-app-server/src/workspace_index.rs:40` | `workspaces.json` |
| `crates/yi-agent-app-server/src/server.rs:258` | 拆成 `os_home()` 与 `instance_home()` |
| `crates/yi-agent/src/tracing_init.rs:77` | `trace/` |
| `crates/yi-agent-boards/src/lib.rs:24` | 看板全局注册表 |
| `crates/yi-agent-web/src/lib.rs:13` | web 端 `.env` |
| `crates/yi-agent-runtime/src/bootstrap.rs:529` | skills 根（`dirs::home_dir()`） |
| `crates/yi-agent-app-server/src/skill_rpc.rs:56-63` | skills 三根（user / system / project） |

后两处改为统一走 `skills_dir()`：user 根为 `skills_dir()`，system 根为
`skills_dir()/.system`。项目级根（`<worktree>/.yi-agent/skills`）保持不变。

另外把这些硬编码改为调用 `project_runtime_directory`：

- `crates/yi-agent-boards/src/board_daemon.rs:23`
- `crates/yi-agent-boards/src/lifecycle.rs:57`
- `crates/yi-agent/src/tui/board.rs:131` 与 `:337`

依赖方向：`yi-agent-store` 位于依赖链底部（`yi-agent-boards` 已依赖它），`paths.rs`
放这里不会引入环。

**`server.rs:258` 必须拆分**：`home_dir()` 同时被用于 launchd plist 路径。若整体换成
`instance_home()`，开发实例会写进真实 `~/Library/LaunchAgents`。

**skills 不需要额外设计**：内置 skill 以 `include_dir!` 嵌入二进制
（`crates/yi-agent-skills/src/system.rs:5`），首次启动自动安装到实例目录
（`crates/yi-agent-runtime/src/bootstrap.rs:530`）。全新实例开箱即可用。

被否方案：XDG 四变量拆分。`threads/`、`devices.json`、`skills/`、`trace/` 语义混杂，
拆完"新建实例"要设四个变量，漏一个就是半隔离，比不隔离更难排查。

### D4 值守：开发版禁用 launchd，会话内前台值守

背景：`ai.yi-agent.board-watchman` 是全局单例 label，plist 固定写
`~/Library/LaunchAgents`（`crates/yi-agent-boards/src/watchman.rs:12-22`）。开发版一旦
安装，会把稳定版的 plist 改成指向 `yi-agent-dev`；而 `bootstrap` 返回
"already bootstrapped" 时被当作成功（`watchman.rs:110-120`），结果是 plist 指向开发
版、实际运行的仍是稳定版进程，两边状态都不对。

处理方式：

1. 开发版会话全程设置 `YI_AGENT_DISABLE_WATCHMAN=1`。该开关已存在
   （`crates/yi-agent-app-server/src/server.rs:273-288`），安装与卸载都退化为 no-op。
2. 需要常驻 daemon 的验证，由 `dev up` 起一个前台
   `yi-agent-dev boards watch` 子进程，随会话结束而终止。
3. `dev down` 先尝试 `bootout`（无则跳过）再终止子进程。

代价：开发实例没有登录自启。对临时验证环境这是可接受的。

被否方案：给开发版独立 label（`ai.yi-agent.board-watchman.<instance>`）。技术上更完整，
但要改 `watchman.rs` 的常量与三个函数签名，还要在 `dev down` 中处理 `bootout` 与
残留 plist，否则 `KeepAlive` 会反复重启一个已删除的二进制。这违反第 3 节的原则。
仅当需要跑通宵回归时才重新评估。

### D5 交付：稳定版走发布渠道，开发版走独立二进制名

- 稳定版：只经 npm（`@yi-agent/yi-agent@x.y.z`）或 Homebrew 安装。回滚 = 装回旧版本号。
- 开发版：二进制名 `yi-agent-dev`，安装到 `~/.local/bin`，不与稳定版同名。
  该目录已通过 `.zshrc:110`（`~/.local/bin/env`）加入交互 shell 的 PATH，无需额外配置。
  注意非交互 shell（CI、脚本）默认没有该目录，脚本内需显式使用绝对路径。
- 实例目录：`~/.yi-agent-instances/<worktree>-<branch>-<path-hash-6>/`，位于仓库外，
  删除目录即销毁。
- `scripts/rebuild.sh` 增加 `--profile dev`：只产出并安装 `yi-agent-dev`，**不再**写
  `~/.cargo/bin/yi-agent` 与 `/opt/homebrew/bin/yi-agent`。这是修正稳定版定义被污染
  的根源。
- 桌面端保持只装稳定版。`desktop/src-tauri/src/bridge.rs:211-217` 读取 preferences 的
  路径**故意不改**：它代表稳定版的路径，改动会让稳定版在缺少环境变量时读到开发版的
  偏好。

## 6. 实例管理命令

`scripts/dev-env.sh` 提供三个动作：

| 动作 | 行为 |
|---|---|
| `dev up` | 计算实例 id；创建实例目录；首次从 `~/.yi-agent/.env` 复制并打印提示；导出 `YI_AGENT_HOME`、`YI_AGENT_RUNTIME_DIR`、`YI_AGENT_DISABLE_WATCHMAN=1`；起子 shell |
| `dev run <args>` | 同样导出环境变量，执行一次命令后退出 |
| `dev down [--purge]` | 停止该实例的 daemon 与值守子进程；`--purge` 时删除实例目录 |

实例 id 由 `worktree 目录名 + 分支名 + 路径 sha256 前 6 位` 组成，保证不同 worktree
不冲突，同一 worktree 重复执行幂等。

`dev up --sync-skills` 可选：把主仓库的自定义 skill 复制到实例目录。

## 7. 并行规则

| 场景 | 是否允许 | 依靠什么隔离 |
|---|---|---|
| 不同 worktree 各跑一个 dev | 允许 | 项目级 `.yi-agent/runtime` 各一份 |
| 同一 worktree 跑 stable + dev | 允许 | `YI_AGENT_RUNTIME_DIR` 指向实例目录 |
| 两个 dev 实例跑同一 worktree | 允许 | 同上，各自实例目录 |
| 两个实例同时 relay 配对 | 允许，需各自配对 | `devices.json` 随实例走 |

## 8. 风险

| 风险 | 后果 | 缓解 |
|---|---|---|
| 开发版覆盖稳定版 launchd plist | 开机自启失效或指向错误二进制 | D4：开发版强制禁用 |
| 开发版复用稳定版 `.env` | 实验跑量消耗额度 | `dev up` 复制时打印提示；可改用独立 key |
| `trace/` 混流 | 无法定位问题来源 | D3 隔离 `tracing_init.rs:77` |
| 开发版登记的 daemon 被稳定版值守拉起 | 两个版本互相拉起对方进程 | D3 隔离 `resident-daemons.json` |
| 自定义 skill 未同步 | 开发版看不到最新 skill | 内置 skill 自动安装已覆盖大头；`--sync-skills` 兜底 |
| `paths.rs` 替换遗漏 | 半隔离，难排查 | 验收门禁：生产代码中不得再有裸 `HOME` 拼 `.yi-agent` |

## 9. 验收

1. 环境变量未设置时，全部现有测试通过，行为与改动前一致。
2. tag 检出下 `yi-agent --version` 不含 `-dev`；普通检出下含 commit 与 dirty 标记。
3. 同一 worktree 同时启动稳定版与开发版两个 daemon：
   - 两个 `runtime.sock` 同时存在且互不可见；
   - 两边的 `devices.json`、`trace/`、`resident-daemons.json` 互不可见；
   - `~/Library/LaunchAgents` 中没有任何开发版写入的 plist。
4. `grep -rn 'var_os("HOME")' yi-agent-rs/crates --include=*.rs` 在生产代码中只应
   出现在 `paths.rs` 内部（`os_home()` / `instance_home()`）。
5. `dev down --purge` 后实例目录消失，没有残留进程。

## 10. 实施顺序

1. `paths.rs` 与调用点替换（行为零变化）。
2. `build.rs` 版本标识。
3. `scripts/dev-env.sh` 与 `rebuild.sh --profile dev`。
4. 并发验收。
5. CI 分支触发调整与文档更新（`docs/project-management/ci-cd.md`、
   `docs/project-management/tooling.md`）。
