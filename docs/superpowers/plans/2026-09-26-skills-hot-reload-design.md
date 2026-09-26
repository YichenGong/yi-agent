# Skills 热重载设计

## 背景与问题

当前 skills 系统的 catalog(可用 skill 列表 + 元数据)在启动时渲染一次,拼进
system prompt(`bootstrap.rs:315-338`),之后整个会话不再变化。`Agent::run()`
开头 `let config = self.config.clone()`(`agent.rs:369`),`run_loop` 每轮读取
`config.system_prompt`(`agent.rs:474`),但 `Agent.config` 是私有字段且无 setter,
调用方无法在会话中途更新它。

结果是:**新增 / 删除 / 改名 skill 必须重启进程才能被 LLM 看到。**

注意 skill 的 **body** 并非如此:`SkillTool::call` 每次调用都从磁盘现读
(`service.rs:81-94`),所以编辑已有 skill 的正文本就"实时"生效。真正被冻结的
只有 catalog。

`SkillsService::refresh()`(`service.rs:46-51`)目前是死代码,设计文档原本预留
给未来的 CLI 命令,但该命令从未实现。

## 目标

- 让 catalog 在会话中途自动刷新,无需重启:
  - TUI:每条用户消息前
  - app-server:每轮对话前
  - daemon:每个 worker 任务开始前
- 刷新不打断会话(不重新弹预算询问)。
- catalog 未变化时 system prompt 逐字节不变,避免破坏 provider prompt cache。

## 非目标

- 不做文件系统 watcher(轮询式重扫已足够,见"变更检测")。
- 不做 skill 管理 UI。
- 不实现 `$name` 触发扩展点。
- 不补截断可见性日志。
- 不支持会话中途修改 base prompt(仍只随重启)。

## 架构

### 1. core:Agent setter(最小改动)

`yi-agent-core/src/agent.rs` 给 `Agent` 加:

```rust
pub fn set_system_prompt(&mut self, prompt: Option<String>) {
    self.config.system_prompt = prompt;
}
```

约 3 行,只写私有字段。因为 `run()` 开头 clone `self.config`,调用方在 `run()`
之前 set 即生效。不改动 `AgentConfig` 的 `#[derive(Debug, Clone)]`。

### 2. runtime:SkillsCatalogHandle

`yi-agent-runtime` 新增组件,持有刷新所需全部输入:

```rust
pub struct SkillsCatalogHandle {
    service: Arc<SkillsService>,
    base_prompt: Option<String>,  // 默认 prompt + 用户指令 + 日期(不含 catalog)
    budget: Option<usize>,        // None = 不限(启动时用户选了"纳入全部")
}

impl SkillsCatalogHandle {
    /// 重扫 + 重渲染 catalog,与 base_prompt 拼接。
    pub fn current_system_prompt(&self) -> Option<String>;
}
```

`current_system_prompt()` 调用 `refresh()`(赋予现有死代码用途)→
`render_catalog(budget)` → 与 `base_prompt` 拼接。`budget` 为 `None` 时传
`usize::MAX` 给 `render_catalog`,即不截断。

### 3. 装配层重构

`PromptSetup`(`bootstrap.rs:35-38`)、`ToolSetup`(`bootstrap.rs:56-59`)各加一个
`catalog: Option<SkillsCatalogHandle>` 字段;`AgentBootstrap` 同样加一个字段
透传。`build_prompt_setup` 内部计算一次 base prompt 与预算策略,构造 handle;
`build_tool_setup` 把 `prompt.catalog` 透传到 `ToolSetup`(当前该函数只返回
`tools` + `system_prompt`,会丢弃 skills service,需补上)。

这样三个运行面都能拿到 handle,base prompt 与预算决策逻辑只实现一处。

## 数据流与触发语义

### 触发点

| 运行面 | 位置 | 时机 |
|--------|------|------|
| TUI | driver loop `main.rs:1326` 之前 | 每条用户消息 `run()` 前 `agent.set_system_prompt(handle.current_system_prompt())`。`/clear`(`main.rs:1166`)、`/compact`(`main.rs:1188`)、runtime-start(`main.rs:1243`)三处重建 Agent 的路径同样从 handle 取 prompt,而非 `rebuild_config.clone()` 里的旧值 |
| app-server | `run_thread_driver`(`server.rs:482`) | 每轮 `agent.run()` 前 set |
| daemon | `subagent_runtime.rs:466` | `DaemonAgentWorkerFactory` 改存 `SkillsCatalogHandle` + AgentConfig 的非 prompt 部分;在每个 worker 任务开始时用 `current_system_prompt()` 现算 AgentConfig(替代当前在 daemon 启动时烘焙的 `main.rs:515-516`) |

### 预算策略:启动时定策略,刷新时不追问

现有逻辑(`bootstrap.rs:340-367`)在 catalog 超预算、未显式指定、且 stdin 是 TTY
时询问 "Include all? [Y/n]"。热重载若每次重问会打断会话,因此:

**启动时把决策固化成 `Option<usize>`:**

| 启动情形 | 固化值 |
|----------|--------|
| 显式 `--skills-catalog-budget n` | `Some(n)` |
| 交互式询问,用户答 "纳入全部" | `None`(不限) |
| 其余(total ≤ default,或非 TTY,或用户答默认) | `Some(default)` |

刷新时只用该值渲染,永不重新 prompt。

### 变更检测:靠确定性渲染

`render_catalog` 的排序是确定的(scope → name,`service.rs:97-103`)。catalog 未
变化时,`current_system_prompt()` 产出的字符串逐字节相同,provider prompt cache
不受影响。因此不需要 mtime 追踪或额外 diff 机制。

这也意味着每条消息全量重扫是可接受的:`discover_skills` 有
`MAX_DIRS=2000` / `MAX_ENTRIES=20000` 上限(`discovery.rs:8-10`),典型 skills
目录是毫秒级。

### 失败处理

- `SkillsService` 为 `None`(启动装配失败)时 handle 为 `None`,调用方跳过 set,
  prompt 保持启动时的值。
- 刷新时 `discover_skills` 对坏条目只 warn 跳过、不返回错误(`discovery.rs:76-79`),
  不会把 prompt 清空。

### 与 Skill body 的关系

body 本就每次工具调用现读(`service.rs:86`),这条链路无需改动。

## 测试

**core(`yi-agent-core`)**
- `set_system_prompt` 后 `run()` 使用新 prompt:用 fake provider 断言收到的
  `ProviderRequest.system`。

**skills crate(`yi-agent-skills`)** — 现有 `refresh()` 无测试,补:
- `refresh_picks_up_new_skill`:snapshot 后新建 `SKILL.md`,`refresh()` 返回新条目,
  而 `snapshot()` 仍返回旧缓存(与现有 `snapshot_caches` 对照)。

**runtime(`yi-agent-runtime`)**
- `SkillsCatalogHandle::current_system_prompt` 在 catalog 变化后返回新字符串、
  未变化时返回逐字节相同的字符串(验证零 churn)。
- 预算策略:`None`(纳入全部)在刷新后仍不截断。

## 文档

- 内置 `assets/skill-creator/SKILL.md:261-263` 与
  `assets/skill-installer/SKILL.md:74-76` 的 "Restart to Reload / no hot-reload"
  段落改写为:TUI / app-server 每条消息、daemon 每个任务自动重扫;`Skill` body
  始终现读。
- `docs/project-management/yi-agent-skills.md` 增补一条热重载 feature,并同步
  `README.md` 索引计数。
- 顺带修 `docs/project-management/yi-agent-skills.md:25,31` 与
  `yi-agent-tools.md:32` 的失效链接(`../plans/` → `../superpowers/plans/`)。
  仅修 skills 相关这几处,其余失效链接不在本次范围。

## 边界情况

- 会话进行中 catalog 变大导致超预算:按启动策略静默截断,不打断。
- daemon 的 handle 在 daemon 进程启动时构造一次,worker 任务按需读取;handle
  内的 `SkillsService` 缓存由 `refresh()` 更新,跨任务共享同一 `Arc`。
