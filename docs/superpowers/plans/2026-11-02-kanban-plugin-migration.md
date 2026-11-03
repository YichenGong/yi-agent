# 看板迁移执行子计划（T21–T23、T25）

日期：2026-11-03

状态：**等待 G3 批准**。G3 未批准前不开始 T21。

对应总计划：[总计划](2026-11-02-kanban-independent-release.md)。
对应子 spec：[看板迁移与受管命令契约](../specs/2026-11-02-kanban-plugin-migration.md)（G3）。

## 0. 继承与基线

### 0.1 从总计划 §0 继承（逐字不改）

- 仓库根 `R`；每条命令从 `cd "$R"` 开始。
- 用本 worktree，不共用 `CARGO_TARGET_DIR`，不碰真实 `~/.yi-agent`。
- Rust 测试用绝对 manifest 路径。
- 每个任务走单行为红绿循环；编译失败**不算**行为红。
- 完成门禁 C 六条逐条执行。

### 0.2 三个子计划必须交错执行

总计划 T05 原文：「三个子计划按本表依赖交错执行，不得按文件顺序各自从头跑完，尤其不能跳过前置的通用第二插件验证。」

本子计划的前置：

| 任务 | 前置 |
|---|---|
| T21 | T19（通用插件包边界）、T20（第二插件验证） |
| T22 | T21 |
| T23 | T22 |
| T25 | T21、T23 |

**不得**先跑 T21 再回头补 T19/T20——通用部分没验证，迁移就是给一个未证明的机制做适配。

### 0.3 基线核对（2026-11-03）

| 断言 | 实际位置 |
|---|---|
| CLI 动词 17 个 | `crates/superpowers-kanban-runner/src/main.rs:144` `USAGE` |
| IPC 方法 20 个 | `crates/superpowers-kanban-runner/src/dispatch.rs:100` `dispatch_in` |
| effect 分类契约 | `plugins/superpowers-kanban/contract/managed-v1.json` |
| `sessions.running` 是宿主本进程上报 | `crates/superpowers-kanban-runner/contract/driver-protocol.json` 的 `request._comment` |
| supervisor 清单 | `crates/superpowers-kanban-runner/src/main.rs` 的 `manifest_json` |
| 现有共享配额域 | `crates/superpowers-kanban-runner/src/lease.rs:42` |
| 状态目录默认值风险 | `--state-dir` 可省；设计 §7.3 禁止默认成 worktree cwd |

### 0.4 已验证的环境陷阱

`just ci` 在 agent shell 里会假失败一条（`YI_AGENT_THREAD_ID` 被注入）。用：

```bash
cd "$R/yi-agent-rs"
env -u YI_AGENT_THREAD_ID -u YI_AGENT_SESSION_ID -u SUPERPOWERS_KANBAN_CARD \
  -u YI_AGENT_PLUGIN_INSTANCE -u YI_AGENT_PLUGIN_BINDING \
  CARGO_BUILD_JOBS=2 just ci
```

**不要**改那个测试或加 `#[ignore]`。

契约检查在仓库根跑：

```bash
cd "$R"
npm ci --prefix plugins/contracts
node scripts/plugins/check-contracts.mjs
node --test scripts/plugins/check-contracts.test.mjs
```

---

## T21：把看板改造为受管插件

**前置：** T19、T20。

**Files：** `plugins/superpowers-kanban` 全套（Cargo workspace、`crates/*/src/*`、`contract/managed-v1.json`、`skills/`、`supervisors/`、`superpowers-kanban.toml`、`INSTALL.md`）；新增 `plugins/superpowers-kanban/manifest-v1.json`。修改 `desktop/src`（看板页面迁出）、`scripts/plugins/check-contracts.mjs`（接入新 manifest）。

### 循环 T21-a：state-dir 不再有 worktree 默认值

- **测试**：不传 `--state-dir` 时，受管入口必须拒绝，错误信息说明需要宿主注入。
- **为什么先做这条**：设计 §7.3 明写「`--state-dir` 不能默认为 worktree cwd 而落到另一块看板」。这是迁移里最容易造成**静默数据分裂**的一条——两块看板都"能跑"，用户看到的是卡片莫名消失。
- **红**：现在 `--state-dir` 可省，会落到默认路径。
- **实现**：受管入口（由宿主传入上下文）要求显式 state-dir；裸 CLI 保留现状但打印警告。
- **绿**。

### 循环 T21-b：effect 分类与实际权限检查一致

- **测试**：对 `managed-v1.json` 里每个非 Read 动词，断言其执行路径经过一次授权检查。
- **实现边界**：这一步只建立"分类被消费"的结构，真实授权在 T13/T16 接通。测试可以用注入的 `RecordingGate` 记录"被问过"，**不**假装权限系统已经工作。
- **验收**：删掉任一非 Read 动词的 gate 调用 → 测试红。

### 循环 T21-c：CLI 与 IPC 名单不得漂移

- **测试**：沿用 G3 的抽取器，断言源码与契约一致。
- **已有**：`checkManagedContracts` 已实现，这条只把它接进插件的测试流程。

### 循环 T21-d：skill 从版本目录发现

- **测试**：skill 只对已授权项目的后续会话可见；不复制到用户全局目录；同名用户副本不被覆盖。
- **断言形态**：`assert!(!user_global_skills_dir.join("superpowers-kanban").exists());`

### 循环 T21-e：manifest-v1.json 通过契约检查

- **测试**：插件的发布清单满足 G1 的 `manifest-v1.schema.json`，且 `views` 恰好一个 `main`、`program.entry` 指向真实文件。
- **实现**：清单由 `manifest_json()` 的值改造而来，保留 supervisor 消费的字段。

### 循环 T21-f：旧绑定被拒绝

- **测试**：用版本 A 建立的绑定，在版本 B 激活后调用 → 明确错误"需要新会话"，**不**转发给新程序。
- **断言**：错误信息包含旧代次与新代次，且新程序**没有**收到那次调用（用 `RecordingGate` 断言调用次数为 0）。

### 循环 T21-g：UI 迁出主程序

- **测试**：`desktop/src` 中不再有看板专属页面组件；发布物检查不含看板业务资源。
- **实现边界**：沿用到 T26 的发布物检查。本循环只断言主程序侧已无引用。

**T21 出口**：
```bash
cd "$R/plugins/superpowers-kanban" && cargo test
cd "$R" && node scripts/plugins/check-contracts.mjs && node --test scripts/plugins/check-contracts.test.mjs
cd "$R/desktop" && npm ci && npm run build && npm test
```
外加：删掉任一 gate 调用或任一 effect 分类，必须打红。

---

## T22：受管命令与 skill 依赖的可验证声明

**前置：** T21。

**Files：** `plugins/superpowers-kanban/manifest-v1.json`（`dependencies.workflows[]`、`skills[]`）、`skills/superpowers-kanban/SKILL.md`、新增 `tests/dependencies.rs`。

### 循环 T22-a：依赖声明不使用编造的 semver

- **测试**：普通文件依赖用**能力/命令契约**表达，不得写成不存在的 skill semver。
- **断言**：schema 层已禁止 `range` 之外的形式；本条测语义——`range` 只在有真实发布版本的 skill 上出现。

### 循环 T22-b：缺依赖时不启动自动任务，但允许查看

- **测试**：移除一个必需 skill → 自动任务被拒并给出诊断；只读看板与普通聊天仍可用。
- **这是设计 §7.2 的核心**：缺依赖不能变成"整个插件不可用"。

### 循环 T22-c：同名 skill 冲突

- **测试**：用户全局目录有一个同名 skill → 显示路径与来源，不覆盖；冲突未解决前阻止自动任务。
- **断言**：用户文件字节未变。

### 循环 T22-d：未知兼容性阻止自动任务

- **测试**：manifest 声明 `hostApi: v1`，宿主是 `v2` 且不声明兼容 → 阻止自动任务，允许查看。
- **注意**：G1 §5.2b 明确 `hostApi` 与 `uiApi` 是两个独立主版本，远端兼容不蕴含本地兼容。

**T22 出口**：`cargo test --manifest-path "$R/plugins/superpowers-kanban/Cargo.toml" -p superpowers-kanban-runner --test dependencies` 全绿。

---

## T23：旧安装探测与受管接管

**前置：** T22。

**Files：** 新增 `crates/superpowers-kanban-runner/src/legacy.rs`、`tests/legacy.rs`；修改 `dispatch.rs`（`inspectLegacy` / `adoptLegacy` 的插件侧处理器）。

### 循环 T23-a：探测只查已登记项目与已知技能根

- **测试**：在临时 HOME 里放一个"用户磁盘深处"的伪造旧安装 → **不被发现**。
- **为什么重要**：设计 §11 明写"不递归扫描用户磁盘"。全盘扫描慢且会碰未授权目录。
- **断言**：探测结果是确定集合，不含深处那个。

### 循环 T23-b：探测列出五类来源与关联项目

- **测试**：PATH 程序、项目 supervisor、用户/项目 skill 副本、常驻登记、原功能开关 → 各一条结果，带来源路径与关联项目。

### 循环 T23-c：不执行未知旧程序

- **测试**：放一个会创建标记文件的伪装旧程序 → 探测后标记文件**不存在**。
- **断言**：`assert!(!marker.exists())`。这条证明"没有为了证明身份而去执行它"。

### 循环 T23-d：接管步骤顺序

- **测试**：用 `RecordingRuntime` 记录动作序列，断言顺序为
  `verify → backup → drain → stop_old → check_compat → confirm → commit → health`。
- **断言**：`commit` 之前没有任何新登记生效。

### 循环 T23-e：未知格式不原地猜迁移

- **测试**：旧数据格式为未知版本 → 接管被拒并给出明确错误，**旧数据字节未变**。

### 循环 T23-f：失败可恢复旧记录

- **测试**：健康检查失败 → 旧记录恢复、新登记不存在。

### 循环 T23-g：不复制活锁

- **测试**：旧共享配额目录有活锁 → 接管后新实例的配额目录里**没有**复制来的锁文件。
- **断言**：只保留指向同一共享域的路径，不复制。

### 循环 T23-h：不删用户文件

- **测试**：用户自定义 skill 与 PATH 上的旧程序在接管后仍存在（字节相同）。
- **断言**：只输出"已不再被托管"的说明。

### 循环 T23-i：接管后不能新旧同跑

- **测试**：接管完成后尝试启动旧程序 → 拒绝（或旧程序自行发现登记已切换）。

### 循环 T23-j：旧 install RPC 不冒充下载

- **测试**：旧客户端发来"注册并启用" → 返回明确的升级提示，**不**下载、**不**建立绑定。

**T23 出口**：`--test legacy` 全绿；每条断言都指向具体行为，不是"接管成功"。

---

## T25：桌面切换与远程宿主

**前置：** T21、T23。

**Files：** `desktop/src`（看板入口改为通用插件面板）、新增 `desktop/src/plugin-host/`；修改 `scripts/plugins/check-contracts.mjs`（发布物内容检查）。

### 循环 T25-a：缓存键包含宿主身份与激活代次

- **测试**：同一版本在两个不同执行宿主下 → 两个缓存条目；同宿主不同激活代次 → 也不同。
- **断言**：键里出现 host id、version、digest、activation。

### 循环 T25-b：远端激活但本地不兼容 → 升级提示

- **测试**：远端 `uiApi: v2`、本地 `v1` → 显示升级提示；**不回退**使用别的 UI 版本；**不妨碍**远端已有任务继续。

### 循环 T25-c：不运行包内远端二进制

- **测试**：从远端下载的包里带一个平台二进制 → 桌面**不执行**它（标记文件不存在），只加载静态 UI。

### 循环 T25-d：面板关闭不等于停止调度

- **测试**：关闭面板后，后端仍在跑、调度继续（除非用户显式停用）。

### 循环 T25-e：停用 ≠ 卸载

- **测试**：停用后数据仍在、页面通道关闭、托管 skill/命令准入撤销。

**T25 出口**：`cd "$R/desktop" && npm run build && npm test` 全绿；上述五条各有一个测试。

---

## 5. 跨任务门禁

每个任务提交前：

1. `cargo fmt --all`（对应 workspace）；
2. `just ci`（用 §0.4 的干净环境命令）；
3. 前端改动另跑 `npm ci && npm run build && npm test`；
4. 契约改动另跑 `node scripts/plugins/check-contracts.mjs && node --test scripts/plugins/check-contracts.test.mjs`；
5. 同一 commit 更新 `docs/project-management/`；
6. 只 `git add` 本任务文件。

## 6. 阻塞点

| 阻塞点 | 谁定 |
|---|---|
| `on`/`off` 归 QuotaWrite | G3 |
| `board.mark_terminal`/`release` 归 Write | G3 |
| `sessions.running` 聚合口径 | G3（实现细节归 T13） |
| 旧安装探测范围 | G3 |
