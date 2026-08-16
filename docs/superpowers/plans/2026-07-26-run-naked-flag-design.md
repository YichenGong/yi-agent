# `yi-agent run --naked` 设计

## 背景

`yi-agent run` (headless 模式) 当前与 TUI `run_agent` 的能力不对齐:

- **不加载 skills 服务** — `setup_skills` 未被调用,`SkillTool` 未注册,skills catalog 不在 system prompt 里
- **system prompt 不补全** — 直接用 `config.system_prompt.clone()`,既不补默认 prompt,也不补 "Current date: YYYY-MM-DD",也不补 skills catalog
- **compact 参数不传** — 但经确认 `agent loop` 根本没读 `compact_threshold` 字段,目前无自动 compact,超过上下文直接报错(现状行为)

用户需求:
1. 默认 `yi-agent run 'hi'` 应具备 TUI 的完整能力(skills + 完整 system prompt)
2. 新增 `--naked` flag,走"裸模型"路径:无工具、无 skills、无 system prompt

## CLI 变更

`Command::Run` 加一个 `#[arg(long)] naked: bool` 字段。

```rust
Run {
    prompt: Option<String>,
    #[arg(long)]
    json: bool,
    #[arg(long)]
    stdin: bool,
    #[arg(long)]
    naked: bool,  // 新增:裸模型模式,不注册工具/不加载 skills/不补 system prompt
},
```

`main.rs` 里 `Command::Run { ref prompt, json, stdin, naked } => run_headless(cli, prompt, json, stdin, *naked)`。

## `run_headless` 实现

两条路径共用前置逻辑:`config::load`、prompt 解析、permission checker 构造、provider 构造。

```rust
fn run_headless(cli: Cli, prompt: Option<String>, json: bool, from_stdin: bool, naked: bool) -> Result<()> {
    let config = config::load(&cli)?;
    // ... prompt 解析、permission checker、provider 构造(两条路径共用)...

    let mut registry = yi_agent_core::ToolRegistry::new();
    let system_prompt = if naked {
        // 裸模型:不注册任何工具,不加载 skills,不补 system prompt
        None
    } else {
        // 和 run_agent(TUI)完全一致
        yi_agent_tools::register_builtin_tools(&mut registry, config.workdir.clone());
        let skills_service = setup_skills(&config)?;
        let sp = resolve_system_prompt_with_skills(
            config.system_prompt.clone(),
            &skills_service,
            config.skills_catalog_budget,
            config.skills_catalog_budget_explicit,
        );
        if let Some(svc) = &skills_service {
            registry.register(Arc::new(yi_agent_tools::SkillTool::new(svc.clone())));
        }
        sp
    };

    let tools = Arc::new(registry);
    let agent_config = yi_agent_core::AgentConfig {
        model: config.model.clone(),
        system_prompt,
        max_turns: Some(config.max_turns),
        ..Default::default()  // compact 参数用默认值,headless 不自动 compact,超上下文直接报错
    };

    // ... 后面 agent.run + drain_stream 逻辑不变 ...
}
```

### naked 路径行为

- **不注册任何工具**:`ToolRegistry::new()` 后直接 `Arc::new(registry)`,不调用 `register_builtin_tools`,不注册 `SkillTool`
- **不加载 skills 服务**:不调用 `setup_skills`
- **system prompt = `None`**:直接传 `None` 给 provider,纯靠模型自身能力

### 默认 run 路径行为(对齐 TUI `run_agent`)

- 注册内置工具(`read`/`write`/`bash`/`grep` 等)
- 调用 `setup_skills(&config)?` 加载 skills 服务
- 注册 `SkillTool`
- `resolve_system_prompt_with_skills(...)` 拼接:默认 prompt + 当前日期 + skills catalog

### compact 参数

两条路径都沿用 `AgentConfig::default()` 的 `compact_threshold` / `compact_keep_turns` 默认值。`agent loop` 当前不读这两个字段(headless 不自动 compact,超上下文直接报错)。agent loop 自动 compact 是独立 missing feature,本次不做。

## 测试计划

按 TDD,先写失败测试,再实现。

### 测试基础设施

在 `yi-agent` crate 的 `tests` module 写一个 `RecordingProvider`,实现 `yi_agent_core::Provider` trait:
- 记录每次 `call_stream` 收到的 `ProviderRequest` 到 `Arc<Mutex<Vec<ProviderRequest>>>`
- 返回 scripted events(从预设 vec 里 pop)
- 不修改 `yi-agent-core` 的 `ScriptedProvider`(它是 `#[cfg(test)]` 私有类型,不该跨 crate 用)

### 单元测试

1. `cli_parses_run_naked_flag` — `Cli::parse_from(["yi-agent","run","--naked","hi"])` 解析出 `naked: true`
2. `cli_parses_run_default_naked_false` — 不带 `--naked` 时 `naked: false`
3. `run_headless_naked_uses_empty_tools_and_no_system_prompt` — 用 `RecordingProvider` 跑 naked 路径,断言:
   - provider 收到的 `ProviderRequest.tools` 是空 vec
   - provider 收到的 `ProviderRequest.system` 是 `None`
4. `run_headless_default_loads_builtin_tools_and_system_prompt` — 用 `RecordingProvider` 跑默认路径,断言:
   - provider 收到的 `tools` 非空(含内置工具)
   - provider 收到的 `system` 非空且含 "Current date:"

### 手动 / 集成验证

5. `yi-agent run --naked 'hi 你是谁'` — 纯文本回复,无 tool call、无 skills 痕迹
6. `yi-agent run 'hi 你是谁'`(不带 `--naked`)— 有 skills + 日期在 system prompt 里

## 范围

**本次做:**
- `--naked` flag
- 默认 `run` 补齐 skills + system prompt(对齐 TUI)

**本次不做(单开 work):**
- agent loop 自动 compact(目前 `compact_threshold` 字段定义了但 loop 不读,headless 超上下文直接报错)
