# Skills 功能设计

## 概述

Skills 是分级披露的指令文档系统,借鉴 codex 的 skill 模型。LLM 先在 system prompt 里看到所有 skill 的简介列表(catalog);判断某个 skill 相关时调用 `Skill` tool,拿到完整 SKILL.md 内容;SKILL.md 里引用的 `references/`、`scripts/`、`assets/` 子目录文件,LLM 通过现有的 `ReadTool` / `BashTool` 按需读取。

### 设计决策摘要

| 决策点 | 选择 | 理由 |
|--------|------|------|
| Skill 模型 | 纯指令文档(prompt injection) | 核心是分级披露,不引入可执行代码注册 |
| 触发方式 | Tool-based(LLM 调 `Skill` tool),预留 `$name` 扩展点 | 同轮生效、无延迟、与现有 Tool 系统自然融合 |
| Skill roots | System / User / Project,不要 Admin | 三层覆盖大部分场景 |
| 同名冲突 | 两条都列出,用 path 区分 | 不 shadow,信息完整 |
| SKILL.md 格式 | YAML frontmatter(`name` + `description`)+ markdown body | 与 codex 一致 |
| Bundled skills | `skill-creator`, `skill-installer` | 自举 + 分发 |
| Catalog 预算 | 默认 8KB,可调,超预算时交互询问 | 对用户透明 |
| 配置 | 零配置,文件存在即启用 | 简单 |

---

## 1. Crate 结构

新增 crate `yi-agent-skills`,放在 `yi-agent-rs/crates/` 下,与 `yi-agent-tools` 平级。

```
yi-agent-rs/crates/yi-agent-skills/
  Cargo.toml
  src/
    lib.rs              # re-exports
    model.rs            # SkillMetadata, SkillScope, SkillError
    loader.rs           # SKILL.md 解析(YAML frontmatter + body)
    discovery.rs        # 扫描 skill roots,返回 Vec<SkillMetadata>
    service.rs          # SkillsService: 缓存 + snapshot + catalog 渲染
    system.rs           # bundled skill 安装(embed + 落盘到 .system/)
    assets/             # bundled SKILL.md 文件(include_dir!)
      skill-creator/SKILL.md
      skill-installer/SKILL.md
```

**依赖**:只依赖 `yi-agent-core`(用其 `Tool` trait 等)和常见 crate(`serde`, `serde_yaml`, `walkdir`, `include_dir`, `sha2`, `dirs`, `tracing`)。不依赖 `yi-agent-tools` 或主 crate。

`Skill` tool 本体放在 `yi-agent-tools` 里,构造时注入 `Arc<SkillsService>`。

---

## 2. 数据模型与 SKILL.md 格式

### SKILL.md 格式

YAML frontmatter + markdown body:

```markdown
---
name: skill-creator
description: Guide for creating effective skills. Use when users want to create a new skill...
---

# Skill Creator

This skill provides guidance for creating effective skills...
```

frontmatter 只解析两个必填字段:
- `name`:小写、连字符、数字,正则 `^[a-z0-9]+(-[a-z0-9]+)*$`,长度 <=64
- `description`:长度 <=1024,超出时在 discovery 阶段截断并加 `...`

markdown body 原样存储,作为 Level 2 的完整指令返回给 LLM。

### 核心类型(`yi-agent-skills/src/model.rs`)

```rust
pub struct SkillMetadata {
    pub name: String,
    pub description: String,
    pub path: PathBuf,       // SKILL.md 绝对路径
    pub scope: SkillScope,
    pub body: String,         // markdown body,延迟加载时可为空
}

pub enum SkillScope {
    System,   // ~/.yi-agent/skills/.system/
    User,     // ~/.yi-agent/skills/
    Project,  // <workdir>/.yi-agent/skills/
}

pub enum SkillError {
    NotFound(PathBuf),
    ParseError(PathBuf, String),
    InvalidName(String),
    Io(std::io::Error),
}
```

### 命名校验

正则 `^[a-z0-9]+(-[a-z0-9]+)*$`,长度 <=64。不合法的 skill 在 discovery 时跳过并记录 `tracing::warn!`,不会中断启动。

### skill 目录约定

每个 skill 是一个目录,内含 `SKILL.md`。可选子目录 `references/`、`scripts/`、`assets/` 不在 discovery 阶段解析,LLM 需要时自己通过 Read/Bash 访问。目录名不需要与 `name` frontmatter 字段一致,但建议一致。catalog 和 `Skill` tool 调用都用 `path` 定位,不用 `name`,所以同名冲突不影响加载。

---

## 3. 发现与加载

### Skill roots

| Scope | 路径 | 说明 |
|-------|------|------|
| Project | `<workdir>/.yi-agent/skills/` | 项目级,跟随 workdir |
| User | `~/.yi-agent/skills/` | 用户级,跨项目 |
| System | `~/.yi-agent/skills/.system/` | bundled 落盘位置 |

workdir 通过 `AgentConfig` 传入。`~` 用 `dirs::home_dir()` 解析。三个 root 任一不存在就跳过,不报错。

### 发现算法(`discovery.rs`)

```rust
pub fn discover_skills(roots: &[(PathBuf, SkillScope)]) -> Vec<SkillMetadata>;
```

对每个 root,用 `walkdir` 遍历(最大深度 4,跳过 hidden 目录即 `.` 开头),找出所有 `SKILL.md` 文件。对每个文件:

1. 读取全文,用 YAML frontmatter 分隔符 `---\n...\n---\n` 拆分 frontmatter 和 body
2. 用 `serde_yaml` 解析 frontmatter 为 `{name, description}`
3. 校验 `name`(正则 + 长度),不合法则 `tracing::warn!` 跳过
4. 截断超长 `description`(加 `...`)
5. 组装 `SkillMetadata`,scope 继承自 root

**性能约束**:每 root 最大 2000 个目录、20000 个条目。超出时停止扫描并 `tracing::warn!`。防止恶意或失控的 skill 目录拖慢启动。

**缓存**:`SkillsService` 内部持有 `Arc<RwLock<Vec<SkillMetadata>>>`,在 `snapshot()` 时第一次扫描并缓存。无文件系统 watcher,不热重载 -- skill 变更需要重启。提供 `refresh()` 方法手动重扫,预留给未来 CLI 命令调用。

### system skill 安装(`system.rs`)

```rust
pub fn install_system_skills(cache_root: &Path) -> Result<()>;
```

用 `include_dir!("$CARGO_MANIFEST_DIR/src/assets")` 把 `src/assets/` 下所有文件编译进二进制。启动时(主 crate `main.rs`),在 agent loop 开始前调用 `install_system_skills(&home.join(".yi-agent/skills/.system"))`。逻辑:

1. 确保 `~/.yi-agent/skills/.system/` 存在
2. 对每个 bundled skill(`skill-creator`, `skill-installer`):
   - 目标路径 `~/.yi-agent/skills/.system/<name>/SKILL.md`
   - 如果目标不存在,或存在但内容 hash(SHA-256)不同,写入新内容
   - 内容相同则跳过(避免无意义写入)
3. 不删除用户修改过的文件 -- 只在缺失或版本更新时覆盖

---

## 4. Skill Tool 与 Catalog 渲染

### `Skill` tool(放在 `yi-agent-tools`)

```rust
pub struct SkillTool {
    service: Arc<SkillsService>,
}

#[async_trait]
impl Tool for SkillTool {
    fn name(&self) -> &str { "Skill" }
    fn description(&self) -> &str {
        "Load full instructions for a skill. Call this when you need detailed guidance from a skill listed in the available-skills section."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Absolute path to the skill's SKILL.md file, as shown in the available-skills section."
                }
            },
            "required": ["path"]
        })
    }
    async fn call(&self, args: Value) -> ToolResult {
        let path = args["path"].as_str().context("missing path")?;
        match self.service.load_skill_body(path) {
            Ok(body) => ToolResult::success(format!(
                "<skill path=\"{}\">\n{}\n</skill>", path, body
            )),
            Err(e) => ToolResult::error(format!("skill load failed: {}", e)),
        }
    }
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata { read_only: true, ..Default::default() }
    }
}
```

**设计要点**:
- `load_skill_body(path)` 直接 `std::fs::read_to_string(path)`,不查缓存(文件可能变化,且 LLM 一次只用几个 skill,开销可忽略)
- path 不在已发现列表里也照样读 -- 不做严格校验,因为 LLM 可能拼错但文件确实存在,严格校验反而挡住合法调用
- `SkillTool` 在主 crate `main.rs` 里和其他 builtin tool 一起 `register` 到 `ToolRegistry`,`ToolSource` 标记为 `Plugin { name: "skills".into() }`
- 不在 `yi-agent-tools::register_builtin_tools` 里注册(它需要 `SkillsService`,跟无状态 builtin 不同),由主 crate 显式注册

### Catalog 渲染(`service.rs`)

```rust
pub fn render_catalog(&self, budget_bytes: usize) -> String;
```

从缓存的 `Vec<SkillMetadata>` 生成 markdown 列表:

```markdown
## Skills

A skill is a set of instructions. Each skill is listed below with its name, a brief description, and the path to its SKILL.md file. To load the full instructions for a skill, call the Skill tool with that path.

### Available skills
- skill-creator: Guide for creating effective skills. (path: /home/user/.yi-agent/skills/.system/skill-creator/SKILL.md)
- skill-installer: Guide for installing skills. (path: /home/user/.yi-agent/skills/.system/skill-installer/SKILL.md)
- my-custom: ... (path: /home/user/.yi-agent/skills/my-custom/SKILL.md)
```

**预算分配顺序**:Project → User → System(最相关的先入),这样:
- Project skill 一定在 catalog 里(除非自身超预算)
- 剩余预算给 User
- 最后用 System 兜底

**System prompt 拼接**:catalog 字符串在主 crate 构造 `Agent` 时拼到 `AgentConfig.system_prompt` 末尾。如果用户没传 system prompt,catalog 就是全部 system prompt。

---

## 5. 启动流程与集成点

主 crate `main.rs` 启动流程(在现有 tool 注册之后,agent loop 之前):

```rust
// 1. 安装 bundled system skills(幂等,内容 hash 比对)
let system_root = dirs::home_dir().unwrap().join(".yi-agent/skills/.system");
yi_agent_skills::install_system_skills(&system_root)?;

// 2. 构造 skill roots
let workdir = config.workdir.clone();
let home = dirs::home_dir().unwrap();
let roots = vec![
    (workdir.join(".yi-agent/skills"), SkillScope::Project),
    (home.join(".yi-agent/skills"), SkillScope::User),
    (home.join(".yi-agent/skills/.system"), SkillScope::System),
];

// 3. 构造 SkillsService 并触发首次扫描
let skills_service = Arc::new(SkillsService::new(roots));
let snapshot = skills_service.snapshot()?;

// 4. 超预算交互(见第 7 节)
let total_bytes = skills_service.full_catalog_size();
let budget = resolve_catalog_budget(total_bytes, &config)?;

// 5. 渲染 catalog,拼到 system prompt
let catalog = skills_service.render_catalog(budget);
let system_prompt = match config.system_prompt {
    Some(p) => format!("{p}\n\n{catalog}"),
    None => catalog,
};

// 6. 注册 Skill tool
let skill_tool = Arc::new(SkillTool::new(skills_service.clone()));
registry.register(skill_tool);

// 7. 用更新后的 system_prompt 构造 Agent
let agent = Agent::new(provider, Arc::new(registry), session, agent_config_with_prompt);
```

**错误容忍**:skill 系统是辅助功能,任何错误都不应中断 agent 启动:
- `install_system_skills` 失败 → `tracing::warn!` + 继续
- `snapshot` 失败 → warn + 返回空,让 agent 在无 skill 状态下继续跑
- `SkillsService` 用 `Arc` 共享给 `SkillTool` 和主流程

### `SkillsService` API

```rust
pub struct SkillsService { /* roots + RwLock<Vec<SkillMetadata>> */ }

impl SkillsService {
    pub fn new(roots: Vec<(PathBuf, SkillScope)>) -> Self;
    pub fn snapshot(&self) -> Result<&[SkillMetadata]>;  // 懒加载
    pub fn refresh(&self) -> Result<()>;                 // 手动重扫
    pub fn render_catalog(&self, budget_bytes: usize) -> String;
    pub fn load_skill_body(&self, path: &str) -> Result<String>;
    pub fn full_catalog_size(&self) -> usize;            // 不截断的总字节数
}
```

### `AgentConfig` 新增字段

```rust
pub struct AgentConfig {
    // ... 现有字段
    pub skills_catalog_budget: usize,
    pub skills_catalog_budget_explicit: bool,  // 用户是否显式设置
}
```

---

## 6. Bundled Skills 内容大纲

### `skill-creator/SKILL.md`

frontmatter:
```yaml
---
name: skill-creator
description: >-
  Guide for creating effective skills. Use when users want to create a new
  skill or update an existing one. Covers naming, directory structure,
  SKILL.md format, and the references/scripts/assets subdirectories.
---
```

正文要点:
- 什么是 skill、分级披露的三层结构
- 命名规则:`^[a-z0-9]+(-[a-z0-9]+)*$`,<=64 字符
- 目录结构:`<name>/SKILL.md` + 可选 `references/`、`scripts/`、`assets/`
- SKILL.md 格式:YAML frontmatter(`name` + `description`) + markdown body
- description 写作要点:要写明"什么场景下用这个 skill",因为这是 LLM 判断是否调用的唯一依据
- 放置位置:`~/.yi-agent/skills/`(跨项目)或 `<project>/.yi-agent/skills/`(项目级)
- 建议先写最小可用 SKILL.md,再补 references/scripts

### `skill-installer/SKILL.md`

frontmatter:
```yaml
---
name: skill-installer
description: >-
  Guide for installing skills from external sources like GitHub repositories.
  Use when users want to install, download, or share skills from outside
  the local filesystem.
---
```

正文要点:
- skill 就是一个目录,拷贝到 skill root 即完成安装
- 从 GitHub 安装:`git clone` 到 `~/.yi-agent/skills/<name>/`,确认有 `SKILL.md`
- 验证:检查 frontmatter 的 `name` 和 `description` 字段
- 不需要注册、重启即生效(yi-agent 当前是启动时扫描,实际需要重启 -- 如实写明)

这两个 skill 在实现阶段写,设计阶段只定方向。内容会参考 codex 的对应 skill 但精简,去掉 codex 特有的 plugin/orchestrator 概念。

---

## 7. Catalog 超预算交互

### 触发条件(三个都满足才问)

1. 用户**未**显式设置 `--skills-catalog-budget` 或 `YI_AGENT_SKILLS_CATALOG_BUDGET`(显式设置视为已决策,不再问)
2. 实际 catalog 总字节数 > 默认预算(8192)
3. 当前为**交互模式**(stdin 是 TTY 且非 `--yolo` / 非 piped 输入)

### 交互流程(在 `main.rs`,`snapshot()` 之后、`render_catalog()` 之前)

```rust
fn resolve_catalog_budget(total: usize, config: &Config) -> Result<usize> {
    const DEFAULT_BUDGET: usize = 8192;

    if config.skills_catalog_budget_explicit {
        return Ok(config.skills_catalog_budget);
    }

    if total <= DEFAULT_BUDGET || !is_interactive() {
        return Ok(DEFAULT_BUDGET);
    }

    prompt_catalog_budget(total, DEFAULT_BUDGET)
}

fn prompt_catalog_budget(total: usize, default: usize) -> Result<usize> {
    let total_kb = total / 1024;
    let default_kb = default / 1024;
    eprintln!(
        "Skills catalog is {} KB ({} skills), exceeds default {} KB budget.\n\
         Include all skills? [Y/n]",
        total_kb, skills_count, default_kb
    );
    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    match input.trim().to_lowercase().as_str() {
        "" | "y" | "yes" => Ok(total),    // 全部包含
        "n" | "no" => Ok(default),        // 保持 8KB,截断
        _ => Ok(default),                 // 无法识别,保守用默认
    }
}
```

### 非交互模式的回退

- stdin 不是 TTY(isatty)、`--yolo`、或通过管道传入 prompt:跳过询问,用默认 8KB 截断,并 `tracing::warn!` 告知有多少 skill 被截断
- 用户显式设置 budget:直接用,不问

### 截断时的可见性

- 被截断的 skill 仍能通过 `Skill` tool 用 path 调用,只是不在 catalog 里露面
- 如果发生了截断,启动后在 TUI/inline 里 `tracing::info!` 一条:"X skills loaded, Y shown in catalog, Z omitted (run with --skills-catalog-budget=<bytes> to adjust)"

### 可调参数配置来源

| 来源 | 名字 | 默认 |
|------|------|------|
| CLI | `--skills-catalog-budget <BYTES>` | 8192 |
| Env | `YI_AGENT_SKILLS_CATALOG_BUDGET` | 8192 |

优先级:CLI > env > 默认。加到 `AgentConfig` 里一个 `skills_catalog_budget: usize` 字段。`render_catalog` 的调用方传这个值进去。

---

## 8. 错误处理与边界情况

### 启动阶段(错误不中断 agent 启动)

| 错误点 | 处理 |
|--------|------|
| `install_system_skills` 失败(写权限等) | `tracing::warn!` + 继续。用户 skill 仍可用,只是 bundled 没装上 |
| `snapshot()` discovery 中某个 root 不存在 | 跳过该 root,不报错(正常情况) |
| 某个 `SKILL.md` 解析失败 | `tracing::warn!("skipping skill at {}: {}", path, err)`,跳过这一个,继续扫其余 |
| `snapshot()` 全部 root 扫描失败 | warn + 返回空 `Vec`,catalog 为空字符串,`Skill` tool 仍注册但调用会返回 error |
| `render_catalog` 生成空 catalog(无 skill) | 返回空字符串,system prompt 不追加任何 skill 内容 |

### 运行阶段

| 错误点 | 处理 |
|--------|------|
| `Skill` tool 调用时 path 不存在 | `ToolResult::error("skill not found: <path>")`,LLM 看到错误信息可自行修正 |
| `Skill` tool 调用时 path 是目录外文件(路径穿越) | 不做路径校验。`Skill` tool 是 read-only,LLM 调用时只能读文件内容,无副作用。路径穿越风险等于 LLM 自己用 `ReadTool` 读任意文件,不增加新攻击面 |

### 边界情况

1. **workdir 不存在或不可读**:`Project` root 跳过,不报错
2. **`~/.yi-agent/skills/` 是文件不是目录**:该 root 跳过,warn
3. **SKILL.md 是空文件**:frontmatter 解析失败,跳过,warn
4. **frontmatter 缺 `name` 或 `description`**:跳过,warn
5. **`name` 含非法字符**:跳过,warn
6. **symlink 循环**:`walkdir` 默认不 follow symlink,安全。当前设计**不 follow**,与 codex 对 system root 的保守策略一致
7. **同一目录下两个 SKILL.md**:不可能,`walkdir` 找的是文件名,一个目录只有一个
8. **skill 目录里嵌套 skill 目录**:`<skill-a>/sub/SKILL.md` 会被当成两个独立 skill 扫到。这是合理行为,不阻止

---

## 9. 测试策略

### 单元测试(在 `yi-agent-skills` crate 内)

**`loader.rs` 测试**:
- 解析正常 frontmatter + body
- 缺 `name` 字段 → 返回 `SkillError`
- 缺 `description` 字段 → 返回 `SkillError`
- `name` 含大写/下划线/空格 → `InvalidName`
- `name` 超过 64 字符 → `InvalidName`
- `description` 超过 1024 字符 → 截断 + 加 `...`
- 无 frontmatter(纯 markdown)→ 返回 `SkillError`
- frontmatter 格式错误(YAML 语法错)→ `ParseError`
- body 为空 → 合法,返回空 body

**`discovery.rs` 测试**(用 `tempfile::TempDir` 构造 mock 目录树):
- 单 root 单 skill → 找到 1 个
- 单 root 多 skill → 找到全部
- 嵌套目录(`<root>/a/b/SKILL.md`)→ 找到
- hidden 目录(`.hidden/SKILL.md`)→ 跳过
- 无 `SKILL.md` 的目录 → 返回空
- 不存在的 root → 返回空,不报错
- 混合合法/非法 SKILL.md → 合法的返回,非法的跳过

**`service.rs` 测试**:
- `snapshot()` 缓存:调用两次,第二次不重新扫描(通过 mock 计数验证)
- `render_catalog(budget)`:
  - skill 总量 < budget → 全部列出
  - skill 总量 > budget → 按 Project→User→System 顺序截断
  - 无 skill → 返回空字符串
  - 同名 skill(不同 scope)→ 两条都列出,带不同 path
- `load_skill_body(path)`:
  - 合法 path → 返回 body
  - 不存在 path → `NotFound`

**`system.rs` 测试**:
- 首次安装 → 文件落盘
- 已存在且内容相同 → 不写入
- 已存在且内容不同 → 覆盖
- 目标目录不存在 → 自动创建

### 集成测试(`yi-agent-skills/tests/`)

- 构造完整目录树(System + User + Project),跑 `discover_skills` + `render_catalog`,断言 catalog 内容和顺序
- 超预算截断:放 20 个 skill 各 ~500 字节 description,预算 2KB,断言只有前几个进 catalog,且顺序正确

### `SkillTool` 测试(在 `yi-agent-tools` 内)

- `call` 合法 path → 成功,返回 `<skill path="...">...</skill>` 包裹的 body
- `call` 不存在 path → 返回 error result
- `call` 缺 `path` 参数 → 返回 error result

### 不测试的部分

- 不测 `main.rs` 的启动流程(需要 mock `Agent` 全部依赖,成本高收益低)
- 不测 catalog 超预算的交互式 prompt(涉及 stdin,用 `is_interactive()` 守卫后跳过询问,难测)
- 不测 `Skill` tool 与 LLM 的端到端交互(需要真调 LLM)

---

## 10. 实现分阶段与依赖

### Phase 1: 核心骨架(可独立编译运行)

1. 创建 `yi-agent-skills` crate,`Cargo.toml` 依赖 `serde`, `serde_yaml`, `walkdir`, `dirs`, `include_dir`, `sha2`, `tracing`
2. `model.rs`:`SkillMetadata`, `SkillScope`, `SkillError`
3. `loader.rs`:frontmatter 解析 + 校验 + 单元测试
4. `discovery.rs`:`discover_skills` + 单元测试
5. `service.rs`:`SkillsService::new/snapshot/render_catalog/load_skill_body` + 单元测试
6. `system.rs`:`install_system_skills` + 单元测试

### Phase 2: Tool 与集成

1. `yi-agent-tools` 里加 `SkillTool`,依赖 `yi-agent-skills`
2. `yi-agent-tools/src/lib.rs` 的 `register_builtin_tools` 不注册 `SkillTool`(它需要 `SkillsService`,跟无状态 builtin 不同),由主 crate 显式注册
3. 主 crate `main.rs`:加载 env/CLI config(`skills_catalog_budget`),构造 roots,`install_system_skills`,构造 `SkillsService`,`snapshot`,超预算交互,`render_catalog`,拼 system prompt,注册 `SkillTool`,构造 `Agent`

### Phase 3: Bundled Skills

1. 写 `src/assets/skill-creator/SKILL.md`
2. 写 `src/assets/skill-installer/SKILL.md`
3. `include_dir` 是编译期宏,直接在 `system.rs` 里 `include_dir!("$CARGO_MANIFEST_DIR/src/assets")`

### Phase 4: 配置与 CLI

1. `Conf` 结构加 `skills_catalog_budget` 字段 + `--skills-catalog-budget` CLI flag
2. env 加载 `YI_AGENT_SKILLS_CATALOG_BUDGET`
3. `AgentConfig` 加 `skills_catalog_budget` 字段
4. 超预算交互的 `is_interactive()` 判断 + `prompt_catalog_budget()` 函数

### 依赖关系

Phase 1 无外部依赖。Phase 2 依赖 Phase 1。Phase 3 依赖 Phase 1 的 `system.rs`。Phase 4 依赖 Phase 2(因为交互在 `main.rs` 里调用,需要 `SkillsService` 已构造)。

### 改动文件清单

- 新增:`yi-agent-rs/crates/yi-agent-skills/` 整个 crate
- 新增:`yi-agent-rs/crates/yi-agent-tools/src/skill_tool.rs`
- 修改:`yi-agent-rs/crates/yi-agent-tools/src/lib.rs`(re-export `SkillTool`)
- 修改:`yi-agent-rs/Cargo.toml`(workspace 加 `yi-agent-skills`)
- 修改:`yi-agent-rs/crates/yi-agent/Cargo.toml`(依赖 `yi-agent-skills`)
- 修改:`yi-agent-rs/crates/yi-agent/src/main.rs`(启动流程)
- 修改:`yi-agent-rs/crates/yi-agent/src/config.rs` 或对应 config 文件(CLI/env 字段)
- 修改:`yi-agent-rs/crates/yi-agent-core/src/agent.rs`(`AgentConfig` 字段,如有需要)
