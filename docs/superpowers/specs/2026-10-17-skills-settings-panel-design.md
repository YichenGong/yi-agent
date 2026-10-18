# 设计：设置页的 Skill 可视化面板

日期：2026-10-17
状态：待评审

## 1. 背景与问题

系统已有一套完整的 skill 机制，但**只对模型可见，对人不可见**：

- `yi-agent-skills` crate 负责发现、解析与编目：`SkillsService::snapshot()`
  枚举 `SkillMetadata { name, description, path, scope, body }`，三个根由
  `yi-agent-runtime/src/bootstrap.rs::build_skills_service` 装配——
  Project = `<workdir>/.yi-agent/skills`、User = `<home>/.yi-agent/skills`、
  System = `<home>/.yi-agent/skills/.system`（内置 `skill-creator` /
  `skill-installer` / `git-diff-review`，由 `install_system_skills` 从
  `include_dir!` 落盘）。
- 编目的唯一消费者是 **system prompt**：`SkillsCatalogHandle::current_system_prompt()`
  每轮开跑前重扫并渲染成 `## Skills` 列表（`server.rs:6622`）。模型据此决定要不要
  调 `Skill` 工具。
- app-server 的 JSON-RPC 方法表里**没有任何 skill 相关方法**（`plugins/list`、
  `model/*`、`onboarding/*` 那套通道里没有对应项），前端也无任何 skill 界面
  （`desktop/src` 下唯一的 skill 痕迹是 `lib/toolSummary.ts` 里把 `Skill` 工具的
  调用摘要显示成路径）。

后果：用户想知道「这台机器上现在装了哪些 skill」只能去 `~/.yi-agent/skills` 用
文件管理器看目录，既看不到 name/description，也分不清某个 skill 属于项目层还是
用户层还是系统内置；想读某个 skill 的正文得手动打开 SKILL.md；想自己加一个 skill
则完全没有入口说明。

## 2. 目标与非目标

### 目标

1. 设置页新增「技能」分类，**列出当前系统支持的 skill**，按三层分组
   （项目 / 用户 / 系统），显示 name、description 与所属层。
2. 每条 skill 可点击展开，**只展示主文件 `SKILL.md` 的正文**（去掉 YAML
   frontmatter）。
3. 在面板内用简短文字讲清「怎么给系统添加一个 skill」（目录、文件名、命名规则、
   frontmatter 格式、生效时机、三条添加渠道）。
4. Desktop 与 iOS 都可用：iOS 是 `Control` 档客户端，接口必须只读、非特权。
5. **桌面端现有观感与交互逐字节不变**（硬不变量，与设置页既有约定一致）。

### 非目标

- TUI 侧不做任何改动（用户明确只要求 Desktop 与 iOS）。
- 不做附属文件树：同目录下的 `references/`、`scripts/`、`assets/` 不列出、不读取。
- 不在设置页里创建 / 编辑 / 删除 skill（创建已有既有路径：`skill-creator`）。
- 不做「有 N 个 SKILL.md 解析失败」的提示。解析失败的条目由
  `discover_skills` 静默跳过（既有行为），维持不变。
- 不重排 skill 的排序规则或 catalog 渲染。
- 不调整手机端 Tab 条的尺寸（本次用导航形态改造取代它，见 §3.4）。

## 3. 设计

### 3.1 后端：新增 `skill/list` 与 `skill/read`

新增模块 `yi-agent-app-server/src/skill_rpc.rs`，形态对齐 `model_rpc.rs`：纯函数、
参数与宿主注入项都是入参，可直接单测，不读全局状态。

**`skill/list`**

| 参数 | 必填 | 说明 |
|---|---|---|
| `project` | 否 | 项目绝对路径。缺省回落宿主 workdir，语义与 `plugin_scope` 一致。 |

根目录推导（`home` 由注入项给出，见 §3.2）：

| 分组 | 路径 |
|---|---|
| `project` | `<project>/.yi-agent/skills` |
| `user` | `<home>/.yi-agent/skills` |
| `system` | `<home>/.yi-agent/skills/.system` |

响应：

```json
{
  "groups": [
    { "scope": "project", "skills": [ { "name": "...", "description": "...", "path": "/abs/.../SKILL.md" } ] },
    { "scope": "user",    "skills": [ ... ] },
    { "scope": "system",  "skills": [ ... ] }
  ],
  "roots": { "project": "/abs/...", "user": "/abs/...", "system": "/abs/..." }
}
```

两条实现要点：

1. **System 组在 list 路径上做一次幂等安装**（`install_system_skills(&system_root)`，
   按 SHA-256 跳过同内容文件）。否则全新机器上、用户还没开过任何对话时，
   「系统内置的 3 个」列不出来——而「当前系统支持了哪些 skill」正是本面板要回答的
   第一个问题。安装失败只 `tracing::warn!`，不使整个 RPC 失败（与
   `build_skills_service` 的降级策略一致）。
2. **根重合时视作没有项目层。** 桌面侧车 app-server 的 workdir 被锚定到用户 home
   （`bridge.rs::spawn_once` 的 `current_dir(home)`），此时「未指定 project」推出的
   project 根与 user 根是**同一目录**。若照常两组都扫，同一个 skill 会在两组里各出现
   一次，看起来像装了两份；若简单按路径去重并「保留更具体的一层」，又会把 15 个用户
   级 skill 全部错标成「项目」级，同时让「用户」组空掉。故规则定为：**当 project 根与
   user 根 canonicalize 后为同一路径时，判定不存在项目层——`project` 组不输出，这些
   skill 只在 `user` 组出现一次**（这更诚实：该目录对所有项目都生效，本来就是用户级）。
   反之，用户显式选了项目（如当前对话的 cwd）时，project 根与 user 根不同，两组照常
   各自渲染。

   注意这里**只**在「根重合」这一种情况下合并。两个**不同**目录里同名 skill
   （`/proj/.yi-agent/skills/foo` 与 `~/.yi-agent/skills/foo`）应当各自出现——这与
   `render_catalog` 的既有语义一致（其 `same_name_different_scope_both_listed` 测试
   钉住了这一点），面板不擅自合并。

`roots` 字段是给 §3.3 的说明文案用的：面板显示**后端真实解析出的路径**，而不是前端
硬编码 `~/.yi-agent/skills`，避免后续路径规则变动后文案与事实脱节。

**`skill/read`**

| 参数 | 必填 | 说明 |
|---|---|---|
| `path` | 是 | 某个 `SKILL.md` 的绝对路径 |
| `project` | 否 | 同 `skill/list`，用于确定合法根集合 |

响应 `{ "body": "<去掉 frontmatter 的正文>" }`（复用
`SkillsService::load_skill_body`）。

**安全边界（iOS 是 `Control` 档，这条必须有）。** 与既有的 `image/read` 同源问题：
这个接口若不设限，就成了一台「可读任意文件」的远程读取原语。必须同时满足两个条件，
否则 `-32602`：

1. 文件名恰为 `SKILL.md`；
2. `canonicalize(path)` 落在三个合法根之一的 `canonicalize()` 结果之下。

用 canonical 路径比较可同时挡住 `..` 穿越与指向根外的符号链接（链接会被解析出来），
也挡住「传绝对路径进来被 `join` 整体替换」这类情况。**这一手法直接照抄既有先例**
`server.rs:2940` 的 `image/read`：它同样 `canonicalize` 后要求结果 `starts_with`
某个根（错误文案 `"path escapes the workspace"`），且同为 `Observe` 档、不设
`Control` 闸。本设计沿用同一模式，不为 skill 另造一套。

注意 `SkillsService::load_skill_body` 自身**刻意不做路径校验**（它的既定理由是
「模型可能拼错路径，路径穿越风险与 ReadTool 等同」），那是**进程内工具**的语境；
RPC 是网络面，必须另加上面这层校验。同理，`skill/read` 只读 `SKILL.md`，不读附属
文件，故不需要 `image/read` 那条「先按元数据长度设闸再分片」的内存放大防护——
单个 SKILL.md 由 discovery 的读取路径自然受限，且不含二进制。

**权限档位**：两者都只需 `Observe`。它们只读、不碰凭据、不写盘，与 `config/read`、
`ui/settings/read`、`plugins/list` 同档。这是 iOS 可用的前提——手机端设备默认是
`Control`。同理，**不**加入 `ADMIN_METHODS`。

### 3.2 后端：home 的注入

skill 根里有两处来自 `$HOME`，测试必须能替换掉它们，否则 `skill/list` 会读到开发者
真实的 `~/.yi-agent/skills`。

在 `RuntimeAttachments` 增加 `skills_home: PathBuf`，生产入口填 `home_dir()`，测试
harness 填 tempdir——与 `board_dir`、`models_path`、`onboarding_env_path`、
`watchman_home` 完全同一既有约定与同一理由。

`RuntimeAttachments` 是结构体字面量构造，新增字段会让所有构造点编译失败，因此不会
有测试被漏掉而误触真实 home。

### 3.3 前端：`SettingsSkillsTab`

新增 `desktop/src/components/SettingsSkillsTab.tsx`，结构照 `SettingsPluginsTab`：

- 顶部项目选择器，与「插件」Tab 完全同款。为此把 `SettingsPluginsTab` 里那段选择器
  抽成共享组件 `SettingsProjectPicker`（`aria-label` 由 prop 传入）。插件 Tab 渲染出的
  标记与可查询元素保持不变，其既有测试**不改动**且必须继续全绿——这是「抽取没抽坏」
  的回归证明。
- 按 `groups` 顺序渲染三层；**空分组整组不渲染**（不出现空标题）。
- 每行是一个 `aria-expanded` 按钮，前缀 `▸` / `▾`，显示 `name` 与 `description`
  ——与 `ChatView` 的「思考」折叠块同一范式。
- 展开时才发 `skill/read`（列表响应里**不含正文**）；正文按 path 缓存进 state，
  同一行反复折叠不重复拉取。加载态「正在读取…」、失败态如实显示错误，均用
  `text-fg-subtle` 的既有写法。
- 正文用既有 `MarkdownText` 渲染。它自带三处宽度收口（表格进 `overflow-x-auto`
  盒子、图片 `max-w-full`、长词 `break-words`）、按 text 的 `memo`、以及
  react-markdown 默认丢弃原始 HTML 的防注入——不另写 markdown 渲染器，也不重蹈
  手机端长行溢出的坑。
- 面板底部一节「如何添加 Skill」（见 §2 目标 3），纯说明文字：
  - 三层目录，路径取自响应的 `roots`（真实解析值）。
  - 规则：把 `<技能名>/SKILL.md` 放进对应目录即生效；名字须匹配
    `^[a-z0-9]+(-[a-z0-9]+)*$` 且 ≤ 64 字符；frontmatter 必须含 `name` 与
    `description`（附三行示例）。
  - 生效时机：下一轮对话（每轮开跑前重扫，热重载）。
  - 三条渠道：手动放文件 / 对话让 Yi-Agent 用内置 `skill-creator` 生成 /
    用内置 `skill-installer` 从外部仓库安装。

宿主接缝 `skillsCall` 与既有 `pluginCall` / `modelCall` 逐字同形：只闭合
`clientRef`、空依赖、身份稳定（否则面板读取 effect 会随父级重渲染反复重跑）。

### 3.4 前端：手机端改为两级导航

**动机**：设置页要加第 5 个分类，而手机端 Tab 条是被测试钉死的等宽横条
（`[data-mobile="true"] .settings-nav > [role="tab"] { flex: 1 1 0 }`，
`index.css` 注释明确写着「320pt 上每格 80px，最长的『远程访问』四字单行放得下」）。
5 格后 320pt 每格只剩 64px，四字标签放不下。与其每加一个分类就重调一次尺寸，不如
让手机端改用更贴合触屏的层级结构。

`SettingsDialog` 增加一个「手机端子页」状态：

- 手机端且未选中分类 → 渲染**分类列表**（通用 / 远程访问 / 插件 / 模型 / 技能），
  每项一行、右侧 `›`、触控高度 ≥ 44px（Apple HIG，与设置页关闭按钮 `size-11`、
  Tab 条 12px 内边距同一标准）。
- 手机端选中分类 → 顶部「‹ 返回」+ 该面板；「返回」回到列表。
- 焦点管理对齐既有做法：进入子页时焦点落到「返回」，返回时焦点还给刚才那一行
  （与 `SettingsDialog` 既有的「关闭时把焦点还给来处」同源）。
- **桌面端路径一字不动**：仍需 `.settings-nav` 竖栏 `role="tablist"`、roving
  tabIndex、方向键/Home/End 导航，标记与类名不变。

要如实登记代价：手机端因此**不再渲染 tablist**，以下既有断言必然改写（这是导航
改造的直接后果，不是顺手改测试）：

| 位置 | 现状 | 改为 |
|---|---|---|
| `SettingsDialog.test.tsx`（phone layout hooks） | 手机端 `aria-orientation` 为 `horizontal` | 手机端渲染分类列表、无 tablist；桌面仍是 `vertical` 的 tablist |
| `mobileLayout.test.ts`（「turns the phone settings into a full-screen page with a top tab bar」） | 断言 `.settings-nav` 横排、每格 `flex:1 1 0`、12px 内边距 | 断言分类列表每行触控高度 ≥44px、无横向溢出；删掉已失效的横条规则 |
| `index.css` | `[data-mobile] .settings-nav` 横排与 `.settings-nav > [role="tab"]` 等宽那几条 | 换成分类列表样式 |

顺带收益：分类数量不再受横向宽度限制。

## 4. 测试

先写测试，再实现。

**后端单测**（`skill_rpc.rs`，tempdir，不碰真实 home）：

- 三层分组齐全、各自内容正确。
- `project` 缺省回落宿主 workdir；显式 `project` 时使用该项目。
- `.system` 不被 User 层重复发现（`discover_skills` 跳过隐藏目录，`is_hidden` 对
  `.system` 为真——用测试钉住这条依赖）。
- 去重：project 根与 user 根 canonicalize 后指向同一目录时，**不输出 project 组**，
  这些 skill 只在 user 组出现一次（钉住「未指定项目时不会把用户级 skill 错标成项目级」）。
- 两个**不同**目录的同名 skill 各自出现在各自组里（不擅自合并，与 `render_catalog`
  的既有语义一致）。
- 全新 tempdir home 上 `skill/list` 能列出系统内置 3 个（证明幂等安装跑在 list 路径上，
  且重复调用不会改变文件 mtime）。
- `skill/read`：成功剥离 frontmatter；非 `SKILL.md` 文件名 → `-32602`；根目录之外
  的路径（含 `..` 穿越）→ `-32602`。

**后端集成测试**（`server.rs`，复用既有 `Harness`）：`skill/list` 返回三层分组、
`skill/read` 读回正文、越界路径回 `-32602`、两者在 `Observe` 档可用（不被
`-32014` 拒）。

**前端测试**（vitest）：

- `SettingsSkillsTab`：stub 接缝 → 三层渲染、空分组不渲染、点行展开**只发一次**
  `skill/read`、正文经 `MarkdownText` 渲染、`skill/read` 失败显示错误而不崩、
  无接缝时空态。
- `SettingsDialog`：桌面 5 个 Tab 且点「技能」渲染面板；手机端渲染分类列表
  （**不**渲染 tablist）、点进子页、点「返回」回列表、焦点归还到原行。
- `SettingsPluginsTab` 既有测试全绿（共享选择器抽取的回归证明）。
- `mobileLayout.test.ts`：改写后的分类列表门禁。

**验证命令**：

```
cd desktop && npx vitest run && npx tsc --noEmit
cd yi-agent-rs && cargo fmt --all && cargo test -p yi-agent-app-server skill && cargo test -p yi-agent-skills
```

按 `CLAUDE.md`：跑 cargo 前先确认没有其它 cargo 进程，不并行跑多个 worktree 的测试。

## 5. 风险

1. **手机端不再有 tablist**（3 处断言改写，见 §3.4 表）——本次唯一的破坏性变更，
   已在计划中显式列出。
2. **桌面端硬不变量**：竖栏导航、roving tabIndex、方向键、以及所有桌面 CSS 规则
   一律不动。桌面 tablist 只多出「技能」一项，属新功能本身。
3. **`RuntimeAttachments` 加字段**会让所有构造点编译失败（含 harness），由编译器
   兜住；这正是「不会有测试误触真实 `~/.yi-agent`」的保证方式。
4. **手机上 44px 行高需要真机/窄屏确认**：现有测试是静态断言（读 CSS 与产物），
   能钉住规则存在，但钉不住真机观感。落地时按既有做法在窄屏上量一次。
5. **iOS 的 `project` 候选**与「插件」Tab 同源（宿主传 `pluginProjects` /
   `pluginDefaultProject`），行为一致，无需特殊处理。

## 6. 影响面

| 位置 | 变更 |
|---|---|
| `yi-agent-rs/crates/yi-agent-app-server/src/skill_rpc.rs` | 新增 |
| `yi-agent-app-server/src/server.rs` | 两个方法分支、`RuntimeAttachments.skills_home`、集成测试 |
| `desktop/src/components/SettingsSkillsTab.tsx` | 新增（+ 测试） |
| `desktop/src/components/SettingsProjectPicker.tsx` | 新增（从插件 Tab 抽出） |
| `desktop/src/components/SettingsDialog.tsx` | 加「技能」Tab、手机端两级导航、`skillsCall` 接缝（+ 测试） |
| `desktop/src/components/SettingsPluginsTab.tsx` | 改用共享选择器（行为不变） |
| `desktop/src/App.tsx` | `skillsCall` 稳定接缝、传 `skillsProjects` / `skillsDefaultProject` |
| `desktop/src/index.css` | 手机端分类列表样式，删除已失效的横条规则 |
| `desktop/src/lib/*` | 新增 `skills.ts`（列表/读取的类型与解析） |
| `desktop/src/mobileLayout.test.ts` | 改写手机端设置页门禁 |
