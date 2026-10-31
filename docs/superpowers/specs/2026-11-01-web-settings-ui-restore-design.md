# Design: 修复 `yi-agent web` 设置页——迁移到 settings.json 后前端未跟上

## Context

`1adb7b46 feat(web): edit settings.json instead of .env`（2026-10-31 21:44）把 web
的存储后端从 `.env` 换成 `settings.json`，同时整体删除了
`yi-agent-web/src/config_meta.rs`（189 行）与 `env_file.rs`（247 行）。后端改动
本身质量高：

- `settings::write_to` 改成读-改-写，不再删掉不认识的键（`runtime/src/settings.rs:471`）
- `write_atomically` 用临时文件 + `rename` + `0600`（`runtime/src/settings.rs:615`）
- `validate_value` 按 `FieldType` 做 min/max 与枚举白名单校验（`runtime/src/settings.rs:525`）
- `tests/api_test.rs` 的 5 条测试覆盖了上述行为

但**前端页面没有跟着改**。后果是该页面打开后无法使用。

### 已实测的故障

用真实 `GET /api/config` 响应喂进 `index.html` 的渲染逻辑（逐行等价转写）：

```
侧栏 tab: ['Agent', 'Tools']
点击 'Agent' -> <h2>Agent</h2>, 其下字段数 = 0
点击 'Tools' -> <h2>Tools</h2>, 其下字段数 = 0
合计可见字段 = 0 / 9
```

页面不崩溃，是「成功渲染了一个空表单」。这比白屏更难察觉——用户会以为配置项
本来就没有。

### 根因（单点）

| 位置 | 内容 |
|---|---|
| `web/src/assets/index.html:284` | `const tabs = ['Agent', 'Tools'];`（硬编码、首字母大写） |
| `web/src/assets/index.html:311` | `if (v.group === tabName) {`（大小写敏感精确相等） |
| `runtime/src/settings.rs:148,206,239` | `group` 实际值是 `"agent"` / `"compact"` / `"startup"`（全小写） |

`'Agent' !== 'agent'`；`'Tools'` 这个分组在新模型里根本不存在（旧
`config_meta.rs` 里曾是 `"Agent"` / `"Tools"`，`git show 1adb7b46^:.../config_meta.rs`
可查）。

`git show 1adb7b46 -- .../index.html` 显示该提交只把 tab 列表从
`['Agent','Tools','Real LLM Tests']` 改成 `['Agent','Tools']`，**没有**同步分组
命名，也没跟上「2 组变 3 组」的变化。

### 为什么测试没拦住

`tests/api_test.rs` 的 5 条测试全部只测 JSON 接口。仓库中搜不到
`index_html`、`uri("/")` 或任何对 HTML 的断言。回归因此带着全绿发布。

### 同源缺口（读码确认）

这四条与根因同源：web 前端没有跟上 `settings.json` 的元数据模型。

1. **看不到字段中文名。** `api.rs:42-58` 的 `json!` 没有输出 `label`，而
   `runtime/src/settings.rs:133` 有该字段、桌面端 `app-server/src/settings_rpc.rs:28`
   也输出它。即使分组名修好，Web 仍只显示 `agent.max_turns` 这类机器键名。
2. **数字输入无范围约束。** `index.html` 全文搜不到 `v.min` / `v.max`。桌面端等价
   位置是 `min={field.min ?? undefined} max={field.max ?? undefined}`
   （`desktop/src/components/SettingsRuntimeTab.tsx:119-120`）。用户在 Web 上填
   `compact.ratio=999` 能点保存，后端才 400 拒绝。
3. **无法把字段恢复默认。** `write_to` 支持 `null` / 空串删键，但 UI 没有出口：
   无「恢复默认」按钮，`UpdateItem.value`（`api.rs:97`）是 `String`，
   `Value::from("")` 永远是字符串，前端无法表达 `null`。桌面端每行都有该按钮
   （`SettingsRuntimeTab.tsx:131-138`）。
4. **沙箱下拉未设置时展示错值。** `index.html:334` 只在 `opt === currentValue` 时
   设 `selected`；未设置时 `currentValue` 为 `null`，无任何 option 命中，浏览器自动
   选第一项 `read-only`。而运行时默认是 `workspace-write`（`tools/src/sandbox.rs:16`
   的 `#[default]`，`runtime/src/settings.rs:242` 的 `default_display`）。

## Scope

**做：**

- 侧栏分组从 API 数据派生，不再硬编码；分组显示名与桌面端对齐
- API 补 `label` 字段
- 数字输入按 `min`/`max` 约束
- 新增「恢复默认」按钮，`UpdateItem.value` 改为可表达 `null`
- 沙箱下拉增加「未设置（默认 …）」显式选项
- 补 Rust 侧回归测试（HTML 契约 + 分组完整性 + null 删键）

**不做：**

- 不重做页面视觉设计，不改布局
- 不引入前端构建链、不引入 Node 依赖（理由见「验证策略」）
- 不动 `settings_rpc`（桌面端），它已正确
- 不处理本 spec 之外的问题（如 `/api/config` 无 Host 校验），单独开卡

## Architecture

改动集中在三个文件，边界清晰：

```
runtime/src/settings.rs          ← 不动（已是单一真相源）
        ↓ FIELDS
web/src/api.rs                   ← 补 label；UpdateItem.value 改 Option<String>
        ↓ JSON (groups[].vars[])
web/src/assets/index.html        ← 分组派生、min/max、恢复默认、select 显式空选项
```

### API 契约变更（`web/src/api.rs`）

**1. 每个 var 增加 `label`**

在 `api.rs:42` 的 `json!` 中加入 `"label": spec.label,`，与 `settings_rpc.rs:28`
保持一致。

**2. `UpdateItem.value` 改为 `Option<String>`**

现状（`api.rs:95-98`）：

```rust
pub struct UpdateItem {
    pub key: String,
    pub value: String,
}
```

`put_config` 在 `api.rs:113-116` 执行 `Value::from(item.value)`，所以 `""` 只能是
字符串。用户把文本框清空后，`write_to` 确实会把空串当删键
（`settings.rs:534-542`），但那是**巧合**而非契约，且无法区分「清空」与
「设成空串」。

改为：

```rust
pub struct UpdateItem {
    pub key: String,
    pub value: Option<String>,
}
```

`put_config` 里的映射改为：

```rust
let value = match item.value {
    Some(text) => Value::from(text),
    None => Value::Null,
};
patch.insert(item.key, value);
```

**关于「字段缺失」的语义**：serde 对 `Option<T>` 在 JSON 中缺该键时会解析成
`None`，与显式 `null` 不可区分。因此结论是：

- 显式 `null` → 删键（本 spec 需要的语义）
- 字段缺失 → 同样解析为 `None` → 删键

两者合并是**可接受**的，因为「没有值」在两种写法下含义一致，且 `write_to`
本来就认为 `null` 与空串同义（`settings.rs:534-539`）。旧代码下缺失字段会反序列化
失败返回 422，新行为是删键——这是一处行为放宽，但当前前端从不省略该字段，且
「省略 = 我没给值」按删键处理合理。在 `UpdateItem` 上写明这条 doc 注释，并用
一条测试固定（见「验证策略」第 5 条）。

**3. 空对象字段清理（现状保留，但有一处硬编码要记一笔）**

`write_to` 已有「删掉只剩空壳的分组对象」逻辑（`settings.rs:508-518`）：

```rust
for group in ["agent", "compact", "startup"] {
```

这是第三处硬编码分组名（另两处是 `index.html:284` 与本 spec 要引入的
`GROUP_LABELS`）。`FIELDS` 加新分组时这里会漏清，留下 `{"foo":{}}` 噪音。
**本次不改**（不属本 spec 的故障范围，改动需动 runtime crate），但要记进
`docs/bug-list.md` 以免丢失。web 侧不重复这份逻辑。

### 前端改造（`web/src/assets/index.html`）

**分组派生替代硬编码**

删除 `index.html:284` 的 `const tabs = [...]`，并把 `index.html:253` 的
`let activeTab = 'Agent'` 改成 `let activeTab = null`（真实值等数据到手再定，
避免留一个永远不存在的初始值）。改为：

```js
// 模块级
let activeTab = null;
const GROUP_LABELS = { agent: 'Agent', compact: '压缩', startup: '启动' };
const groupLabel = (name) => GROUP_LABELS[name] ?? name;

// loadConfig 内，拿到 data 之后
groupNames = data.groups.map((g) => g.name);          // 顺序跟随后端 FIELDS
if (!groupNames.includes(activeTab)) {                // 首载或该组已消失
  activeTab = groupNames[0] ?? null;
}
```

**三处必须一起改，否则仍会空白或显示英文：**

1. 侧栏遍历 `groupNames`（不是硬编码数组），按钮文本用 `groupLabel(tab)`，
   但 `click` 里赋给 `activeTab` 的必须是**原始 name**（`agent`），不是显示名。
   这是最容易写错的一步。
2. `renderFlatTab` 的标题（`index.html:308`）现在是
   ``groupEl.innerHTML = `<h2>${tabName}</h2>` ``，要改成
   `` `<h2>${groupLabel(tabName)}</h2>` ``，否则标题显示 `agent`。
3. `renderMain`（`index.html:298-303`）在 `activeTab` 为 `null` 时不要调
   `renderFlatTab`，改为渲染一行「没有可配置项」。

显示名映射与桌面端 `SettingsRuntimeTab.tsx:9-13` 对齐（同为
`agent/compact/startup` → `Agent/压缩/启动`）。

`?? group` 的兜底是必须的：`settings.rs` 未来加分组时，web 不能因为缺少映射
就把整组吞掉。桌面端的等价实现见 `SettingsRuntimeTab.tsx:55-65`：
`GROUP_ORDER.filter(...)` 先排已知组，再用 `extra` 收集映射表里没有的组，
两者拼接后渲染。web 只做简化版（数组派生 + 标签兜底），不引入 `GROUP_ORDER`
排序表——顺序直接跟随后端 `FIELDS` 的出现顺序。

**渲染条件改为按 group 名精确匹配（保留）**

`index.html:311` 的 `v.group === tabName` 本身是对的，只要 `tabName` 来自数据。
不改比较逻辑，只改 `tabName` 的来源。

**数字输入约束**

`index.html:339` 之后补：

```js
if (v.type === 'number') {
  if (v.min != null) inputEl.min = v.min;
  if (v.max != null) inputEl.max = v.max;
}
```

注意 `v.min` 来自 JSON `null` 时是 `null`，用 `!= null` 同时排除 `null` 与
`undefined`。

**「恢复默认」按钮**

每个 field 追加一个按钮。点击时依次做三件事：

1. `currentValues[key] = null`
2. 清空输入框显示（`inputEl.value = ''`；`select` 则选中 `""` 那个空选项）
3. 调 `updateSaveButton()`，让「保存」按钮立刻变为可用（否则用户点了恢复默认
   却看不到任何反馈）

第 3 步不能省：`updateSaveButton` 只由 `onFieldChange` 与 `loadConfig` 触发，
恢复默认按钮不经过 `onFieldChange`，不显式调用就会漏掉。

**不需要额外的「待删除集合」**：现有 `save()`（`index.html:390-396`）已经用
`currentValues[key] !== originalValues[key]` 做差异检测，`null` 与任何字符串都
不等，因此置 `null` 后会被自动收进 `updates`，发出去就是
`{ key, value: null }`。再加一个 Set 是重复状态，删掉。

行为验证（三条都要在人工验收里过）：

| 原值 | 操作 | `updates` 内容 |
|---|---|---|
| `500` | 点恢复默认 | `{ key, value: null }` → 删键 |
| `null`（未设置） | 按钮禁用，点不到 | — |
| `500` | 点恢复默认后再输入 `300` | `{ key, value: "300" }` |

按钮禁用条件用 `currentValues[key] === null`（当前已是「无值」，再点无意义），
与桌面端 `disabled={saving || isUnset}`（`SettingsRuntimeTab.tsx:132-135`）语义一致。

**沙箱下拉显式空选项**

`select` 分支在遍历 `v.options` 之前插入一个空选项：

```js
const blank = document.createElement('option');
blank.value = '';
blank.textContent = `未设置（默认 ${v.default || '—'}）`;
inputEl.appendChild(blank);
```

同时把选中判定（`index.html:334`）从 `opt === currentValue` 改成
`opt === (currentValue ?? '')`，让 `null` 命中空选项。

已用 Node 验证三种输入都落在正确选项上：

| `currentValue` | 选中项 |
|---|---|
| `null`（未设置） | `""` → 显示「未设置（默认 workspace-write）」 |
| `"workspace-write"` | `workspace-write` |
| `"read-only"` | `read-only` |

**这一改同时解决「恢复默认」的显示问题**：点恢复默认把 `currentValues[key]` 置
`null` 之后，`select` 需要重新选中空选项。因为「恢复默认」的点击处理里已经要
「清空输入框显示」，`select` 分支就写 `inputEl.value = ''`——这与空选项的
`value` 一致，浏览器会同步选中它。两条规则不是各写一套，而是同一个动作。

**保存逻辑**

`index.html:394` 的 `updates.push({ key, value: currentValues[key] })` **不需要改**。
`currentValues[key]` 为 `null` 时，`JSON.stringify` 会把它序列化成 JSON `null`，
配合上面 `UpdateItem.value: Option<String>` 正好走删键分支。一行不动。

## 验证策略

这是本次设计最关键的选择，先说明约束：

- `just ci`（`yi-agent-rs/justfile:36`）跑 `test` = `cargo test --all-features --workspace`
- CI 镜像**没有 Node**。`justfile:39-41` 明确记录了这一点（magpie agent 检查刻意
  不进 `ci`，否则拖垮流水线）
- 仓库没有 jsdom / happy-dom 之类的 DOM shim，也没有前端测试先例

因此**不能**把「真跑 JS 断言 DOM」作为门禁。采用两层：

### 第一层：Rust 测试（进 CI，作为门禁）

全部放在 `web/tests/api_test.rs`：

1. **分组完整性**：断言 `GET /api/config` 的每个 `group.name` 与
   `settings::FIELDS` 里出现过的 `group` 值集合完全相同。这条直接钉死
   「后端加组、前端不认」的漂移。
2. **`label` 全覆盖**：遍历返回的 9 个 var，断言每个都有非空 `label`，
   且等于 `FIELDS` 中对应 `spec.label`。结构同
   `settings_rpc.rs:116` 那条「漏一个键会红」的测试。
3. **HTML 契约**（纯文本断言，不跑 JS）：
   - 断言 `index.html` 不含 `const tabs = [`（禁止再写死分组）
   - 断言含 `data.groups` 派生的命名（如 `group.name`）
   - 断言含 `v.min` 与 `v.max`
   - 断言含恢复默认的 `null` 路径
   这类断言弱，但能以零成本挡住「有人又加一行硬编码」。
4. **`null` 删键**：PUT `{"key":"agent.max_turns","value":null}` 后断言该键从
   文件消失、且其余键保留。补上现有测试未覆盖的显式 null 路径。
5. **省略 `value`**：PUT `{"key":"agent.max_turns"}` 断言与显式 `null` 等价，
   把上面「API 契约变更」里承认的行为固定下来，避免日后被当成 bug 反复改。

### 第二层：人工浏览器验收（不进 CI）

改完后我本地起 `yi-agent web`，用真实浏览器确认：

- 三个分组 tab 都出现，标签为「Agent / 压缩 / 启动」
- 每组字段数与 `FIELDS` 一致（5 / 3 / 1）
- 字段显示中文名，不再是机器键名
- 数字框上下限生效（`compact.ratio` 无法通过 spinner 超过 100）
- 点「恢复默认」后保存，键从 `settings.json` 消失
- 沙箱下拉未设置时显示「未设置（默认 workspace-write）」，选中后保存值正确

这一步的结论会写进交付说明，但它不是门禁。

### 为什么不用 Node 测

可选方案是加 `web/tests/index.test.mjs`（用 `node --test` 解析 HTML 里的函数并
注入桩 DOM），走 magpie 的路子单独挂一个 workflow。否决理由：

- 需要一个手写的最小 DOM 桩，维护成本高于被测代码本身
- 纯函数部分（如分组派生）已被第一层的「分组完整性」覆盖
- 真正的渲染正确性靠人工验收一次即可，它是低频改动

## 风险

| 风险 | 处置 |
|---|---|
| `UpdateItem.value` 改 `Option` 可能影响既有测试 | 现有 5 条测试全部用字符串 value，`Some(text)` 路径不变；跑全量确认 |
| 分组派生后，`data.groups` 为空时页面空白 | `activeTab` 置空串，主区显示「无可配置项」提示，明确而非静默 |
| `v.min` 为 JSON `null` 时被误设成 `"null"` 字符串 | 用 `!= null` 判断；在 HTML 契约测试里断言取值为 `null` 的字段使用 `!= null` 守卫。真实 DOM 行为靠人工验收 |
| 恢复默认与普通编辑同批提交时状态错乱 | `null` 与修改值共用 `currentValues` 单一状态源，差异检测一次性收全；人工验收三条组合路径 |

## 完成判据

1. `cargo test -p yi-agent-web` 全绿，且新增 ≥ 5 条测试
2. `cargo test -p yi-agent-runtime` 仍全绿（未改该 crate，确认无连带）
3. `cargo fmt --all` 无 diff
4. 人工验收 6 项全部通过，逐条记录实际观察结果
