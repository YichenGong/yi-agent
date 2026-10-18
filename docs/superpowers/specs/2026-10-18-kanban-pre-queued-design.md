# 看板 pre-queued 列 Design

日期：2026-10-18
状态：设计已定稿，待用户 review 后进入 writing-plans。

## 1. 背景与问题

看板现在只有一条入口：投递**已经写好**的一对 spec + plan。卡片 id 由两个文件名
stem 派生（`plugins/superpowers-kanban/crates/superpowers-kanban-core/src/card_id.rs::card_id_for`），
投递时校验两份文件必须存在且互异（`promotion.rs::validate_promotion`），只有
`CardState::Queued` 的实现卡会被 claim 起来跑会话（`board.rs::next_startable`）。

缺一个位置放「已经决定要做、但 spec/plan 还没写出来」的需求。这类需求今天只能记在
脑子里或别处，需求一多就跟丢；等 spec/plan 写完再入队时，中间那段「我知道要做但还没
设计」的状态在看板上完全不可见。

## 2. 目标与非目标

**目标**

- 看板常驻一列 `PRE-QUEUED`，承载「已决定要做、spec/plan 待写」的需求，让它持续可见、
  不漏追踪。
- 预排队卡在**全生命周期内 id 不变**：创建时定下，提升为正式排队卡时不重算，因此从
  预排队一路到 `done` 都是同一张卡、同一条追踪线。
- 「什么时候算计划好了」由人显式拍板（`promote`），插件不猜、不自动探测文件出现。
- 不破坏现有架构不变量：`board.json` 的唯一写者仍是 runner 推进循环，外部投递一律走
  `inbox/`。

**非目标**

- 不做自动提升（不按「文件出现」自动开跑）。
- 不做 pre-queued 卡的内容编辑（描述正文、优先级标签等）；只存标题。
- 不改并发模型：预排队卡不占并发槽位、不参与 claim。
- 不重构看板其它列/状态的既有语义。

## 3. 数据模型

### 3.1 新状态 `PreQueued`

`plugins/superpowers-kanban/crates/superpowers-kanban-core/src/card.rs`：

- `CardState` 新增变体 `PreQueued`，serde `rename_all = "snake_case"` → 落盘为
  `"pre_queued"`。
- `occupies_slot()` **不**包含 `PreQueued`（仍只认 `Running`）。
- `is_terminal()` **不**包含它。
- `can_transition_to` 新增两条边：
  - `PreQueued → Queued`（提升）
  - `PreQueued → Cancelled`（取消；本就由 `next == Cancelled` 兜底，显式列出更清楚）
- 其余保持不动。`PreQueued` 不是 `Queued`，所以 `next_startable()` 天然不会挑到它，
  无需为「不跑会话」再写条件。

### 3.2 `Card.title`

- `Card` 新增 `pub title: Option<String>`，带 `#[serde(default)]`：旧 `board.json`
  没有该字段 → 反序列化为 `None`，不得整份解析失败。
- 语义：预排队卡的展示标题；正式卡由 spec 派生标题，此字段为 `None`。
- `Board::enqueue`（实现卡）与 `enqueue_merge`（合并卡）填 `title: None`。
- 新增 `Board::enqueue_prequeued(id, title, now)` 填 `state: PreQueued`、
  `spec_path`/`plan_path` 为空 `PathBuf`、`title: Some(title)`、其余字段同实现卡。

### 3.3 id 规则

- 预排队卡 id = `slug(title)`；撞名时追加 `-2`/`-3`…（复用 `board.rs::next_free_merge_id`
  的既有套路，新写一个 `next_free_prequeued_id(title)`）。
- `slug` 结果为空时回退 `"prequeued"` 再加后缀。
- 复用 `card_id.rs::slug`，不新造 slug 规则。
- **`promote` 绝不重算 id**：这是「追踪不断线」的核心保障。

## 4. 写入路径：全部走 inbox

架构不变量：`board.json` 的唯一写者是 runner 推进循环；外部一律通过 `inbox/` 投递
（见 `core/src/inbox.rs` 顶部注释，以及
`docs/superpowers/specs/2026-10-17-kanban-single-source-of-truth-design.md` 记录的
「CLI 直接写盘会被 daemon 内存快照覆盖」事故）。`promote` 需改一张已存在的卡，比投递
新卡复杂，但仍遵守这条不变量。

### 4.1 新投递形状

`core/src/inbox.rs` 新增两个投递函数：

- 建卡：`{"kind":"prequeued","id":...,"title":...}` → 文件 `<id>.json`
- 提升：`{"kind":"promote","id":...,"spec_path":...,"plan_path":...,"title"?:...}`
  → 文件 `<id>.promote.json`

两者都沿用 `temp + rename` 原子替换。

### 4.2 消费：两趟

`runner/src/inbox.rs::consume` 改为**两趟**：

1. 第一趟处理**建卡类**投递（implementation / merge / prequeued）。
2. 第二趟处理 `promote`。

理由：`add-prequeued` 与 `promote` 可能在同一 tick 的窗口内先后落盘，文件到达顺序不
可依赖；先建卡后提升，可保证「刚建完立刻提升」不会因顺序而失败。

`RawDelivery` 增加 `kind` 的 `"prequeued"` / `"promote"` 分支与 `title` 字段。

### 4.3 提升的处理

`promote` 投递被消费时：

- 卡不存在 → 移入 `inbox/rejected/`，记原因。
- 卡存在但 state 不是 `PreQueued` → 移入 `inbox/rejected/`，记原因（不猜、不错改）。
- 校验：复用 `validate_promotion(spec, plan)`；不过 → 移入 `inbox/rejected/`。
- 通过：`Board` 新增 `promote_prequeued(id, spec, plan, title)`，一步完成
  - 填 `spec_path` / `plan_path`；
  - 可选覆盖 `title`；
  - **重排 order 到队尾**（取其 `next_order` 并递增）——`order` 表达「何时变得可跑」，
    而非「何时记下想法」，否则一张三周前记的想法今天补完 plan，会插到所有昨天排队的
    卡前面；
  - 状态 `PreQueued → Queued`（经 `transition`，受迁移表约束）。
- 成功后删除投递文件；任何拒绝分支都留档 `inbox/rejected/`，绝不静默丢。

## 5. 入口

### 5.1 CLI（`runner/src/main.rs`）

- `superpowers-kanban add-prequeued <title> [--state-dir <d>]` —— 定 id、投递、打印 id。
- `superpowers-kanban promote <card-id> <spec> <plan> [--title <新标题>] [--state-dir <d>]`
  —— 提升前先 `validate_promotion`，拒绝即报错退出（不落盘）。
- 两者都进 `USAGE` 文本。

### 5.2 IPC（`runner/src/dispatch.rs`）

- `enqueue_prequeued`（params `title`）→ 返回 `{"id": ...}`。
- `promote`（params `id`/`spec_path`/`plan_path`/可选 `title`）→ 返回 `{"id": ...}`。
- 校验先做、拒绝不落盘，与既有 `enqueue` 同构。

### 5.3 TUI（`yi-agent-rs/crates/yi-agent/src/tui/superpowers_kanban.rs`）

- 新增 verb：`/superpowers-kanban add-prequeued <title>`、
  `/superpowers-kanban promote <id> <spec> <plan> [<title>]`。
- `render_board` 逐卡那行：无 `plan_path` 时回退显示 `title`，避免预排队卡显示为一行空。
- `/superpowers-kanban status` 计数加「预排队 N」：`BoardStatus` 加
  `pre_queued: usize`（`yi-agent-boards/src/lifecycle.rs`，其 `count` 按 state 字符串
  数 `pre_queued`）。

### 5.4 桌面 UI

- `desktop/src/lib/superpowersKanbanBoard.ts`：
  - `BoardColumnKey` 加 `"preQueued"`；`BoardColumns` 加 `preQueued`。
  - `columnForState` 把 `"pre_queued"` 映到 `"preQueued"`。
  - `boardColumns` 初始化并入该列；排序沿用 `order` 升序（与 queued 同规则）。
- `desktop/src/components/SuperpowersKanbanBoard.tsx`：列序改为
  `["preQueued","queued","doing","needDecision","done"]`，最左、常驻（空列显示「空」）。
- `desktop/src/components/SuperpowersKanbanColumn.tsx`：`COLUMN_LABEL` 加
  `preQueued: "PRE-QUEUED"`。
- `desktop/src/lib/superpowersKanbanState.ts`：`normalizeCard` 的 `title` 优先取显式
  `title` 字段，回退现有 `deriveTitle(specPath, id)`。
- 「加入看板」入口扩展（`SuperpowersKanbanEnqueue.tsx`）：新增「预排队」输入框（输
  标题）→ 走 `enqueue_prequeued`；预排队卡上提供「补 spec/plan 提升」动作 → 走 `promote`。

### 5.5 布局风险

宽屏从四列并排变五列并排，每列更窄；标题已有 `truncate`，窄屏本就纵向堆叠，可接受。
在 spec 实现时以 vitest 快照/断言覆盖五列渲染，人工过一眼宽度。

## 6. 文档与 skill 同步

- 插件 `README.md` 与 `plugins/superpowers-kanban/skills/superpowers-kanban/SKILL.md`
  增加前植入队一节：「spec/plan 还没写时用 `add-prequeued <title>` 追踪，写完用
  `promote <id> <spec> <plan>` 提升」，修正当前「必须先有 spec+plan」的绝对表述。
- 同步 `docs/project-management/` 对应模块状态与 README 计数。
- 新增 spec 之外不改 `docs/ideas.md`。

## 7. 测试

- **core**（`superpowers-kanban-core`）：`PreQueued` 迁移表两条边、`occupies_slot` 为
  false、`is_terminal` 为 false、`title` 前向兼容（旧 json → `None`）、
  `enqueue_prequeued`、`next_free_prequeued_id` 撞名后缀、`promote_prequeued`
  重排 order 且不换 id、`slug` 为空回退。
- **runner**：`consume` 两趟顺序（同一 tick 内建卡 + 提升都生效）、promote 各失败分支
  落 `rejected/`、幂等、CLI 参数解析（`add-prequeued`/`promote`）、dispatch 两个新方法。
- **desktop**（vitest）：`columnForState("pre_queued") === "preQueued"`、`boardColumns`
  含五列且列序正确、`normalizeCard` 标题优先显式 `title`、Enqueue 新输入框与提升动作。
- **TUI**：新 verb 的 usage 与投递、`render_board` 回退 `title`、status 计数含预排队。

## 8. 验收判据（可执行）

- `cargo test -p superpowers-kanban-core -p superpowers-kanban-runner`（在
  `plugins/superpowers-kanban/` 下）通过。
- `cargo test -p yi-agent`（TUI 相关）通过。
- `cd desktop && npm test` 通过。
- 手工路径：`add-prequeued "示例需求"` → 桌面看板最左出现一张 PRE-QUEUED 卡 →
  `promote <id> <spec> <plan>` → 卡片移到 QUEUED 且 id 不变、排到队尾。
- 回归：既有四列行为不变；旧 `board.json` 仍能反序列化。
