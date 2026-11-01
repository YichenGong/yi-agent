# web 设置页修复（settings.json 迁移后前端未跟上）Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让 `yi-agent web` 的设置页重新渲染出全部 9 个字段，并补上 label、min/max、恢复默认、下拉空选项四项缺口。

**Architecture:** 后端 `web/src/api.rs` 只做两件事——给每个字段补 `label`，把 `UpdateItem.value` 从 `String` 放宽为 `Option<String>` 以表达「删键」。前端 `assets/index.html` 把硬编码的分组列表改为从 `GET /api/config` 的 `data.groups` 派生，显示名用映射表兜底。`runtime/src/settings.rs` 完全不动。

**Tech Stack:** Rust（axum 0.8 + serde + serde_json）、单文件内嵌 HTML + 原生 JS（无构建链）。

## Global Constraints

- 工作目录：`.worktrees/fix-web-settings-ui`（已在分支 `fix/web-settings-ui` 上）
- `cargo` 不在默认 PATH，每条命令前先 `export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$HOME/.cargo/bin:/opt/homebrew/bin:/usr/bin:/bin"`
- 不得并行跑多个 `cargo`（会 OOM / 锁竞争）；跑前 `ps aux | grep -c "[c]argo"` 应为 0
- 测试按 crate 跑，不要 `--workspace`
- 不在 `main` 上提交；不写 `Co-Authored-By`
- 提交前必须 `cargo fmt --all`
- 前端**不引入**任何构建链或 Node 依赖（CI 镜像无 Node，见 `justfile:39-41`）
- 分组名固定为小写 `agent` / `compact` / `startup`，显示名 `Agent` / `压缩` / `启动`
- 前端所有 JSON 字段名（`label` / `min` / `max` / `group` / `name` / `vars` / `options` / `default`）必须与 `api.rs` 实际输出一致

---

## File Structure

| 文件 | 职责 | 本次改动 |
|---|---|---|
| `yi-agent-rs/crates/yi-agent-web/src/api.rs` | HTTP 外壳：把 `settings::FIELDS` 摊成 JSON、把 PUT 转成 `write_to` 调用 | 补 `label`；`UpdateItem.value` 改 `Option<String>` |
| `yi-agent-rs/crates/yi-agent-web/src/assets/index.html` | 单页前端，无构建 | 分组派生、label 显示、min/max、恢复默认、select 空选项 |
| `yi-agent-rs/crates/yi-agent-web/tests/api_test.rs` | web crate 的全部集成测试 | 新增 6 条 |
| `docs/project-management/yi-agent-web.md` | 模块进度与判据 | 补 2 条 feature；修正 1 条过时描述 |

---

### Task 1: API 补 `label` 字段

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-web/src/api.rs:42-58`（`json!` 块）
- Test: `yi-agent-rs/crates/yi-agent-web/tests/api_test.rs`

**Interfaces:**
- Consumes: `settings::FieldSpec::label: &'static str`（已存在于 `runtime/src/settings.rs:133`）
- Produces: `GET /api/config` 的每个 var 多一个 `"label": string` 字段。Task 5 的前端依赖它做 `label.textContent`。

- [ ] **Step 1: 写失败测试**

在 `tests/api_test.rs` 末尾追加：

```rust
/// 每个字段都必须带 `label`，且与 runtime 的 FIELDS 同源。
/// 漏一个键会让前端回退成显示机器键名（`agent.max_turns`），
/// 故按 FIELDS 遍历断言，而不是抽查几个。
#[tokio::test]
async fn get_config_reports_a_label_for_every_field() {
    use yi_agent_runtime::settings;

    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("settings.json");

    let response = app(path)
        .oneshot(Request::get("/api/config").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let value = body_json(response).await;

    let vars: Vec<serde_json::Value> = value["groups"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|g| g["vars"].as_array().unwrap().clone())
        .collect();

    assert_eq!(vars.len(), settings::FIELDS.len(), "每个字段都要出现");

    for spec in settings::FIELDS {
        let var = vars
            .iter()
            .find(|v| v["key"] == spec.key)
            .unwrap_or_else(|| panic!("missing field {}", spec.key));
        assert_eq!(
            var["label"], spec.label,
            "{} 的 label 必须来自 FIELDS，不得另写一份",
            spec.key
        );
    }
}
```

- [ ] **Step 2: 跑测试确认红**

```bash
export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$HOME/.cargo/bin:/opt/homebrew/bin:/usr/bin:/bin"
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/fix-web-settings-ui
cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-web --test api_test get_config_reports_a_label_for_every_field 2>&1 | tail -20
```

预期：FAIL，报 `missing field agent.max_turns`（`find` 返回 `None`）。**这必须是行为失败，不是编译失败**——若报编译错误，先修好测试代码再继续。

- [ ] **Step 3: 加 `label`**

`src/api.rs` 的 `json!({` 块里，在 `"key": spec.key,` 之后加一行：

```rust
                    "key": spec.key,
                    "label": spec.label,
                    "value": value,
```

- [ ] **Step 4: 跑测试确认绿**

```bash
cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-web --test api_test get_config_reports_a_label_for_every_field 2>&1 | tail -8
```

预期：`test result: ok. 1 passed`

- [ ] **Step 5: 跑全量确认无回归**

```bash
cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-web 2>&1 | grep -E "test result|FAILED"
```

预期：6 passed（原 5 + 新 1）

- [ ] **Step 6: 提交**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/fix-web-settings-ui/yi-agent-rs && cargo fmt --all
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/fix-web-settings-ui
git add yi-agent-rs/crates/yi-agent-web/src/api.rs yi-agent-rs/crates/yi-agent-web/tests/api_test.rs
git commit -m "feat(web): GET /api/config 补 label 字段

settings.rs:133 的 FieldSpec.label 早已有值、桌面端 settings_rpc.rs:28 也
输出它，只有 web 漏抄。缺 label 时前端只能显示 agent.max_turns 这类机器
键名，用户看不出这是「最大轮次」。

测试按 FIELDS 遍历断言，漏一个键就红。"
```

---

### Task 2: `UpdateItem.value` 改 `Option<String>`

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-web/src/api.rs:95-98`（`UpdateItem`）、`:113-116`（`put_config` 映射）
- Test: `yi-agent-rs/crates/yi-agent-web/tests/api_test.rs`

**Interfaces:**
- Consumes: 无
- Produces: `PUT /api/config` 接受 `{"updates":[{"key":string,"value":string|null}]}`。`value: null` 或字段缺失 → 删键。Task 6 的前端依赖这一点发送恢复默认。

- [ ] **Step 1: 写失败测试**

```rust
/// 显式 `value: null` 必须表示「删掉这个键」，而不是报错或写成 null 字面量。
#[tokio::test]
async fn put_config_with_null_value_deletes_the_key() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("settings.json");
    std::fs::write(
        &path,
        r#"{"agent":{"max_turns":500,"system_prompt":"keep"},"my_note":"keep me"}"#,
    )
    .unwrap();

    let response = app(path.clone())
        .oneshot(
            Request::put("/api/config")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"updates":[{"key":"agent.max_turns","value":null}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let text = std::fs::read_to_string(&path).unwrap();
    let value: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert!(
        value["agent"].get("max_turns").is_none(),
        "null 必须删键，实际内容: {text}"
    );
    assert_eq!(value["agent"]["system_prompt"], "keep", "同组其它键必须保留");
    assert_eq!(value["my_note"], "keep me", "未知顶层键必须保留");
}

/// 省略 `value` 字段等价于发 null（serde 对 Option 缺字段即 None）。
/// 这条固定该行为，避免日后被误当 bug 改来改去。
#[tokio::test]
async fn put_config_treats_a_missing_value_as_null() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("settings.json");
    std::fs::write(&path, r#"{"agent":{"max_turns":500}}"#).unwrap();

    let response = app(path.clone())
        .oneshot(
            Request::put("/api/config")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"updates":[{"key":"agent.max_turns"}]}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let value: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert!(value.get("agent").is_none(), "空壳分组应被清掉: {value}");
}
```

- [ ] **Step 2: 跑测试确认红**

```bash
export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$HOME/.cargo/bin:/opt/homebrew/bin:/usr/bin:/bin"
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/fix-web-settings-ui
cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-web --test api_test put_config_with_null_value_deletes_the_key 2>&1 | tail -20
```

预期：FAIL，状态码是 422（`null` 无法反序列化成 `String`），断言 `left: 422, right: 200`。

- [ ] **Step 3: 改 `UpdateItem` 与映射**

`src/api.rs`，把

```rust
#[derive(Deserialize)]
pub struct UpdateItem {
    pub key: String,
    pub value: String,
}
```

改为

```rust
#[derive(Deserialize)]
pub struct UpdateItem {
    pub key: String,
    /// `None` 与 `Some("")` 都表示删键（由 `settings::write_to` 统一判定）。
    ///
    /// 用 `Option` 而非 `String` 是为了让前端能显式表达「恢复默认」：直接发
    /// JSON `null`，不必借用「空串恰好被当成删键」这个巧合。
    ///
    /// 注意 serde 对 `Option<T>` 在字段缺失时也解析成 `None`，因此**省略
    /// `value` 等价于发 `null`**。这是有意接受的行为，有测试固定
    /// （`put_config_treats_a_missing_value_as_null`）。
    pub value: Option<String>,
}
```

再把 `put_config` 的映射：

```rust
    let mut patch = serde_json::Map::new();
    for item in request.updates {
        patch.insert(item.key, Value::from(item.value));
    }
```

改为

```rust
    let mut patch = serde_json::Map::new();
    for item in request.updates {
        let value = match item.value {
            Some(text) => Value::from(text),
            None => Value::Null,
        };
        patch.insert(item.key, value);
    }
```

- [ ] **Step 4: 跑测试确认绿**

```bash
cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-web --test api_test 2>&1 | grep -E "test result|FAILED|^test "
```

预期：8 passed（原 5 + Task 1 的 1 + 本条 2）

- [ ] **Step 5: 提交**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/fix-web-settings-ui/yi-agent-rs && cargo fmt --all
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/fix-web-settings-ui
git add yi-agent-rs/crates/yi-agent-web/src/api.rs yi-agent-rs/crates/yi-agent-web/tests/api_test.rs
git commit -m "feat(web): PUT /api/config 支持 value:null 删键

原本 UpdateItem.value 是 String，前端无法表达「恢复默认」——只能发空串，
依赖 write_to「空串恰好被当删键」这个巧合。改成 Option<String> 后前端直接
发 JSON null，语义显式。

serde 对 Option 缺字段也解析成 None，故省略 value 等价于 null；该行为有
专门测试固定，避免日后被当成 bug。"
```

---

### Task 3: 分组完整性测试（钉死「后端加组、前端不认」）

**Files:**
- Test: `yi-agent-rs/crates/yi-agent-web/tests/api_test.rs`

**Interfaces:**
- Consumes: Task 1 的 `label`；`GET /api/config` 的 `groups[].name`
- Produces: 无（纯测试）

**说明：** 这条测试不检查 HTML（那是 Task 4），只钉死 API 的分组输出与 `FIELDS` 一致。它是 Task 7 前端改动的安全网——前端改为派生后，若 API 漏组，这条会红。

- [ ] **Step 1: 写测试**

```rust
/// `groups` 必须覆盖 FIELDS 里出现的每个 group，且不重复、不遗漏。
/// 前端改为从这份数据派生侧栏，任何一侧漏组都会让整组字段消失。
#[tokio::test]
async fn get_config_groups_match_every_group_in_fields() {
    use std::collections::BTreeSet;
    use yi_agent_runtime::settings;

    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("settings.json");

    let response = app(path)
        .oneshot(Request::get("/api/config").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let value = body_json(response).await;

    let returned: Vec<&str> = value["groups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g["name"].as_str().unwrap())
        .collect();

    let expected: BTreeSet<&str> = settings::FIELDS.iter().map(|f| f.group).collect();

    assert_eq!(
        returned.len(),
        expected.len(),
        "分组数不符；返回 {returned:?}，期望 {expected:?}"
    );
    assert_eq!(
        returned.iter().copied().collect::<BTreeSet<_>>(),
        expected,
        "分组集合不符"
    );
    assert!(
        !returned.contains(&"Tools"),
        "Tools 分组已不存在，出现它说明硬编码残留"
    );
}
```

- [ ] **Step 2: 跑测试确认绿（这条是纯安全网，当前实现应当已满足）**

```bash
export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$HOME/.cargo/bin:/opt/homebrew/bin:/usr/bin:/bin"
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/fix-web-settings-ui
cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-web --test api_test get_config_groups_match_every_group_in_fields 2>&1 | tail -8
```

预期：PASS。**如果红，说明后端也有问题**，先停下来查，不要为了让测试绿而改断言。

- [ ] **Step 3: 自查这条测试真的能拦住漂移**

临时把断言里的 `expected` 改成只含 `"agent"`，确认测试红，再改回来：

```bash
# 手工验证：把 BTreeSet::from(["agent"]) 塞进 expected，跑一次应 FAIL，然后还原
```

目的：确认测试不是「怎么写都绿」。还原后用 `git diff` 确认无残留。

- [ ] **Step 4: 提交**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/fix-web-settings-ui/yi-agent-rs && cargo fmt --all
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/fix-web-settings-ui
git add yi-agent-rs/crates/yi-agent-web/tests/api_test.rs
git commit -m "test(web): 钉死分组集合与 FIELDS 一致

前端即将改为从 groups 派生侧栏，这条测试是那一步的安全网：API 少给一组，
整组字段就会从页面消失，且不会有任何报错。同时断言 Tools 不再出现，
防止硬编码残留。"
```

---

### Task 4: HTML 契约测试

**Files:**
- Test: `yi-agent-rs/crates/yi-agent-web/tests/api_test.rs`

**Interfaces:**
- Consumes: `GET /` 返回的 HTML
- Produces: 无

**说明：** 纯文本断言，不跑 JS（CI 无 Node）。它只能挡住「又写死一行」，无法证明渲染正确——渲染正确性由 Task 7 的人工验收兜底。写清这一点，避免后人误以为它是强保证。

- [ ] **Step 1: 写测试**

```rust
/// HTML 契约：挡住「又加一行硬编码分组」这类回归。
///
/// 注意这是**弱保证**——纯文本断言，不执行 JS，无法证明渲染正确。
/// 真正的渲染正确性靠人工浏览器验收（见 plan 的人工验收清单）。
#[tokio::test]
async fn index_html_derives_groups_instead_of_hardcoding_them() {
    let response = app(tempfile::TempDir::new().unwrap().path().join("settings.json"))
        .oneshot(Request::get("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_json_text(response).await;

    assert!(
        !html.contains("const tabs = ["),
        "禁止再把分组写死；应从 data.groups 派生"
    );
    assert!(
        html.contains("data.groups"),
        "分组必须来自 API 数据"
    );
    assert!(
        html.contains("GROUP_LABELS"),
        "显示名需要映射表，否则侧栏显示英文名"
    );
    assert!(
        html.contains("v.min") && html.contains("v.max"),
        "数字输入必须带上后端给的上下限"
    );
    assert!(
        html.contains("reset-btn"),
        "必须有「恢复默认」按钮"
    );
    assert!(
        html.contains("currentValues[v.key] = null"),
        "恢复默认的核心是把当前值置 null（save 靠差分检测收进 updates）"
    );
}

/// 把响应体当纯文本读出来（HTML 不走 JSON）。
async fn body_json_text(response: axum::response::Response) -> String {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8(bytes.to_vec()).unwrap()
}
```

- [ ] **Step 2: 跑测试确认红**

```bash
export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$HOME/.cargo/bin:/opt/homebrew/bin:/usr/bin:/bin"
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/fix-web-settings-ui
cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-web --test api_test index_html_derives_groups 2>&1 | tail -20
```

预期：FAIL 在第一条断言（`const tabs = [` 还在）。这是**预期中的红**——Task 7 会把它转绿。

- [ ] **Step 3: 提交（带着一条红测试）**

这条测试在 Task 7 完成前必然是红的。为了让每个 commit 都自洽，**不单独提交**，而是把这一步并入 Task 7 的提交：写完后先停在这里，跳到 Task 5/6/7，Task 7 结束时连同实现一起提交。

若坚持逐步提交，用：

```bash
git add yi-agent-rs/crates/yi-agent-web/tests/api_test.rs
git commit -m "test(web): HTML 契约测试（预期红，由下一步实现转绿）"
```

**推荐做法：并入 Task 7 提交。**

---

### Task 5: 前端分组派生 + label 显示

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-web/src/assets/index.html:250-316`
- Test: Task 4 的契约测试

**Interfaces:**
- Consumes: Task 1 的 `label`；`data.groups[].name`、`data.groups[].vars[]`
- Produces: 模块级 `groupNames: string[]`、`groupLabel(name)`、`activeTab`（初始 `null`）。Task 6 的恢复默认按钮会调用 `updateSaveButton()`。

- [ ] **Step 1: 改模块级状态与映射表**

把 `index.html:250-253`：

```js
  let allVars = [];
  let originalValues = {};
  let currentValues = {};
  let activeTab = 'Agent';
```

改为：

```js
  let allVars = [];
  let originalValues = {};
  let currentValues = {};
  // null 表示「还没加载到数据」。绝不能写死一个分组名——那正是本次故障的根因
  // （写死的 'Agent' 与后端的小写 'agent' 永远不相等，页面渲染出 0 个字段）。
  let activeTab = null;
  // 分组顺序跟随后端 FIELDS 的出现顺序，不在前端另排。
  let groupNames = [];
  // 显示名映射。缺映射时回退成原始 name（见 groupLabel），
  // 这样后端新增分组不会被前端吞掉。
  const GROUP_LABELS = { agent: 'Agent', compact: '压缩', startup: '启动' };
  const groupLabel = (name) => GROUP_LABELS[name] ?? name;
```

- [ ] **Step 2: 在 `loadConfig` 里派生分组**

把 `index.html:267-279` 的扁平化之后、`renderSidebar()` 之前：

```js
    // 扁平化所有变量
    allVars = [];
    for (const group of data.groups) {
      for (const v of group.vars) {
        allVars.push(v);
        originalValues[v.key] = v.value;
        currentValues[v.key] = v.value;
      }
    }

    renderSidebar();
```

改为：

```js
    // 扁平化所有变量
    allVars = [];
    for (const group of data.groups) {
      for (const v of group.vars) {
        allVars.push(v);
        originalValues[v.key] = v.value;
        currentValues[v.key] = v.value;
      }
    }

    // 分组顺序由后端决定。activeTab 只在「当前选中的组已不存在」时才重置，
    // 避免每次保存后跳回第一个 tab。
    groupNames = data.groups.map((g) => g.name);
    if (!groupNames.includes(activeTab)) {
      activeTab = groupNames[0] ?? null;
    }

    renderSidebar();
```

- [ ] **Step 3: 改 `renderSidebar`**

把 `index.html:284` 那句 `const tabs = ['Agent', 'Tools'];` 与循环：

```js
    const tabs = ['Agent', 'Tools'];
    for (const tab of tabs) {
      const btn = document.createElement('button');
      btn.className = 'tab' + (tab === activeTab ? ' active' : '');
      btn.textContent = tab;
      btn.addEventListener('click', () => {
        activeTab = tab;
        renderSidebar();
        renderMain();
      });
      sidebar.appendChild(btn);
    }
```

改为：

```js
    for (const tab of groupNames) {
      const btn = document.createElement('button');
      btn.className = 'tab' + (tab === activeTab ? ' active' : '');
      // 显示中文名，但 activeTab 里存的必须是原始 name（agent），
      // 否则下面的 v.group === tabName 匹配不上。
      btn.textContent = groupLabel(tab);
      btn.addEventListener('click', () => {
        activeTab = tab;
        renderSidebar();
        renderMain();
      });
      sidebar.appendChild(btn);
    }
```

- [ ] **Step 4: 改 `renderMain` 处理空数据**

把 `index.html:298-303`：

```js
  function renderMain() {
    const main = document.getElementById('main');
    main.innerHTML = '';

    renderFlatTab(main, activeTab);
  }
```

改为：

```js
  function renderMain() {
    const main = document.getElementById('main');
    main.innerHTML = '';

    if (activeTab === null) {
      const empty = document.createElement('p');
      empty.className = 'desc';
      empty.textContent = '没有可配置项。';
      main.appendChild(empty);
      return;
    }

    renderFlatTab(main, activeTab);
  }
```

- [ ] **Step 5: 改 `renderFlatTab` 的标题**

把 `index.html:308`：

```js
    groupEl.innerHTML = `<h2>${tabName}</h2>`;
```

改为：

```js
    groupEl.innerHTML = `<h2>${groupLabel(tabName)}</h2>`;
```

- [ ] **Step 6: 改 `renderField` 的标签行**

把 `index.html:324`：

```js
    label.innerHTML = `${v.key} <span class="desc">${v.description}</span> ${sourceLabel(v.source)}`;
```

改为（用 `textContent` 拼，避免把用户可配置的 `system_prompt` 之类内容当 HTML）：

```js
    const labelText = document.createElement('span');
    labelText.textContent = v.label || v.key;
    labelText.title = v.key; // 悬停可看到机器键名，便于对照文档
    const desc = document.createElement('span');
    desc.className = 'desc';
    desc.textContent = v.description;
    label.append(labelText, desc);
    label.insertAdjacentHTML('beforeend', sourceLabel(v.source));
```

`sourceLabel` 返回的是固定字符串（不含用户数据），用 `insertAdjacentHTML` 安全。

- [ ] **Step 7: 在浏览器里确认分组出现**

```bash
export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$HOME/.cargo/bin:/opt/homebrew/bin:/usr/bin:/bin"
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/fix-web-settings-ui
cargo run --manifest-path yi-agent-rs/Cargo.toml -p yi-agent -- web --port 7399
```

另开一个终端：

```bash
curl -s http://127.0.0.1:7399/api/config | python3 -c "import json,sys; d=json.load(sys.stdin); print([(g['name'], len(g['vars'])) for g in d['groups']])"
```

预期输出：`[('agent', 5), ('compact', 3), ('startup', 1)]`

浏览器打开 `http://127.0.0.1:7399`，确认：

- 侧栏出现三个标签：`Agent` / `压缩` / `启动`
- 点每个标签，右侧显示字段，且标题是中文名
- 字段标签显示中文（如「最大轮次」），鼠标悬停能看到 `agent.max_turns`

按 `Ctrl+C` 停掉服务。

- [ ] **Step 8: 提交**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/fix-web-settings-ui/yi-agent-rs && cargo fmt --all
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/fix-web-settings-ui
git add yi-agent-rs/crates/yi-agent-web/src/assets/index.html
git commit -m "fix(web): 分组从 API 数据派生，修复页面渲染 0 个字段

1adb7b46 把后端分组从 Agent/Tools 改成小写的 agent/compact/startup，前端
却仍硬编码 ['Agent','Tools']，且用 v.group === tabName 精确比较。结果是两个
tab 都匹配不到任何分组，9 个字段一个都不渲染——页面不报错，只是空白，比
白屏更难察觉。

改为从 data.groups 派生侧栏；显示名走 GROUP_LABELS 映射，缺映射时回退成
原始 name，保证后端加组不被吞。同时字段标签改用 FIELDS 的 label（如「最大
轮次」），不再只显示 agent.max_turns。"
```

---

### Task 6: 数字上下限 + 恢复默认 + 下拉空选项

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-web/src/assets/index.html:326-352`（`renderField`）、`:118-160`（CSS 补按钮样式）
- Test: Task 4 的契约测试

**Interfaces:**
- Consumes: Task 2 的 `value: null` 删键；`v.min` / `v.max` / `v.default` / `v.options`
- Produces: 无

- [ ] **Step 1: 数字输入加 min/max**

`renderField` 里，把 `index.html:337-342` 的 else 分支：

```js
    } else {
      inputEl = document.createElement('input');
      inputEl.type = v.type === 'number' ? 'number' : 'text';
      inputEl.value = currentValue;
      inputEl.placeholder = v.default || '(none)';
    }
```

改为：

```js
    } else {
      inputEl = document.createElement('input');
      inputEl.type = v.type === 'number' ? 'number' : 'text';
      inputEl.value = currentValue;
      inputEl.placeholder = v.default || '(none)';
      // 后端给了 min/max 就套上去。JSON null 在 JS 里是 null，
      // 用 != null 同时排除 null 与 undefined（否则会写成属性 "null"）。
      if (v.type === 'number') {
        if (v.min != null) inputEl.min = v.min;
        if (v.max != null) inputEl.max = v.max;
      }
    }
```

- [ ] **Step 2: select 加显式空选项**

把 `index.html:328-336` 的 select 分支：

```js
    if (v.type === 'select') {
      inputEl = document.createElement('select');
      for (const opt of v.options) {
        const o = document.createElement('option');
        o.value = opt;
        o.textContent = opt;
        if (opt === currentValue) o.selected = true;
        inputEl.appendChild(o);
      }
    }
```

改为：

```js
    if (v.type === 'select') {
      inputEl = document.createElement('select');
      // 显式空选项：未设置时浏览器否则会自动选第一项（read-only），
      // 而运行时默认其实是 workspace-write，用户会以为默认是只读。
      const blank = document.createElement('option');
      blank.value = '';
      blank.textContent = `未设置（默认 ${v.default || '—'}）`;
      inputEl.appendChild(blank);
      for (const opt of v.options) {
        const o = document.createElement('option');
        o.value = opt;
        o.textContent = opt;
        if (opt === (currentValue ?? '')) o.selected = true;
        inputEl.appendChild(o);
      }
    }
```

- [ ] **Step 3: 加「恢复默认」按钮**

在 `renderField` 里，`inputEl.dataset.type = v.type;`（`index.html:345`）之后、
`field.insertBefore(label, field.firstChild);`（`index.html:350`）之前插入：

```js
    const reset = document.createElement('button');
    reset.type = 'button';
    reset.className = 'reset-btn';
    reset.textContent = '恢复默认';
    // 已经是「未设置」时没有可恢复的东西。
    reset.disabled = currentValues[v.key] === null;
    reset.addEventListener('click', () => {
      // 置 null 即可：save() 用 currentValues !== originalValues 做差异检测，
      // null 与任何字符串都不等，会自动收进 updates 并发成 JSON null。
      // 不需要额外的「待删除集合」。
      currentValues[v.key] = null;
      inputEl.value = '';
      reset.disabled = true;
      updateSaveButton();
    });
    field.appendChild(reset);
    inputEl.addEventListener('change', () => {
      reset.disabled = currentValues[v.key] === null;
    });
```

- [ ] **Step 4: 补 CSS**

在 `.field input.modified, .field select.modified { ... }` 之后加：

```css
  .field .reset-btn {
    margin-top: 0.4rem;
    padding: 0.25rem 0.7rem;
    background: transparent;
    border: 1px solid var(--border);
    border-radius: var(--radius);
    color: var(--text-dim);
    font-size: 0.75rem;
    cursor: pointer;
    transition: all 0.15s;
  }
  .field .reset-btn:hover:not(:disabled) {
    border-color: var(--accent);
    color: var(--accent);
  }
  .field .reset-btn:disabled {
    opacity: 0.4;
    cursor: not-allowed;
  }
```

- [ ] **Step 5: 转绿 Task 4 的契约测试**

```bash
export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$HOME/.cargo/bin:/opt/homebrew/bin:/usr/bin:/bin"
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/fix-web-settings-ui
cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-web 2>&1 | grep -E "test result|FAILED|^test "
```

预期：全部 PASS，共 9 条。

- [ ] **Step 6: 浏览器验证三件事**

```bash
cargo run --manifest-path yi-agent-rs/Cargo.toml -p yi-agent -- web --port 7399
```

浏览器打开 `http://127.0.0.1:7399`，确认：

1. **上下限**：`压缩触发占比（%）` 的输入框点上下箭头，最大停在 `100`、最小 `1`
2. **下拉**：`默认沙箱档位` 未设置时显示「未设置（默认 workspace-write）」，展开后第一项是它，后面才是 `read-only` 等
3. **恢复默认**：给「最大轮次」填 `500` → 保存 → 刷新后显示 `500` 且「恢复默认」可点；点它，输入框清空，再点保存，然后：

```bash
python3 -c "
import json,os
p=os.path.expanduser('~/.yi-agent/settings.json')
d=json.load(open(p)) if os.path.exists(p) else {}
print('agent.max_turns 存在:', 'max_turns' in d.get('agent', {}))
"
```

预期：`agent.max_turns 存在: False`

**注意：这一步会真实修改 `~/.yi-agent/settings.json`。** 开始前先备份：

```bash
cp ~/.yi-agent/settings.json ~/.yi-agent/settings.json.bak-plan 2>/dev/null || echo "无原文件"
```

验证完还原：

```bash
mv ~/.yi-agent/settings.json.bak-plan ~/.yi-agent/settings.json 2>/dev/null || rm -f ~/.yi-agent/settings.json
```

- [ ] **Step 7: 提交**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/fix-web-settings-ui/yi-agent-rs && cargo fmt --all
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/fix-web-settings-ui
git add yi-agent-rs/crates/yi-agent-web/src/assets/index.html yi-agent-rs/crates/yi-agent-web/tests/api_test.rs
git commit -m "feat(web): 数字上下限、恢复默认、下拉空选项

三条都是「后端有元数据、前端没用上」的同类缺口：

1. min/max：后端 FIELDS 给了范围，前端从没读过。用户填 compact.ratio=999
   能点保存，到后端才 400，错误发现得晚。
2. 恢复默认：write_to 支持 null 删键，但 UI 没有出口。配合上一步的
   UpdateItem.value: Option<String>，现在点一下就能清空。
3. select 空选项：未设置时无 option 命中，浏览器自动选第一项 read-only，
   而运行时默认是 workspace-write，用户会以为默认是只读。

恢复默认不需要额外的待删除集合：save() 的 differential 检测里 null 与任何
字符串都不等，会自动收进 updates。"
```

---

### Task 7: 记录 runtime 侧的空壳清理硬编码

**Files:**
- Modify: `docs/bug-list.md`

- [ ] **Step 1: 追加条目**

在 `docs/bug-list.md` 末尾追加（沿用文件现有格式，`[ ]` 表示未修复）：

```markdown
## settings.json 空壳分组清理写死了组名

- [ ] `write_to` 清理空分组时遍历的是硬编码数组 `["agent","compact","startup"]`
      （`yi-agent-runtime/src/settings.rs:509`）。`FIELDS` 新增分组后，该组被
      清空时会留下 `{"foo":{}}` 噪音。修法：从 `FIELDS` 派生组名集合，不另写一份。
      影响面：仅丑，不影响读写。发现于 2026-11-01 web 设置页修复。
```

- [ ] **Step 2: 提交**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/fix-web-settings-ui
git add docs/bug-list.md
git commit -m "docs(bug-list): 记录 settings.json 空壳清理的硬编码分组

settings.rs:509 遍历 ["agent","compact","startup"] 而非从 FIELDS 派生，
新增分组后清空会留下 {"foo":{}} 噪音。本次 web 修复不涉及 runtime，单独记账。"
```

**2. Placeholder scan**

已检查：无 TBD / TODO / 「稍后实现」/「类似 Task N」。每条代码步骤都给了可直接粘贴的完整代码。

**3. Type consistency**

- `UpdateItem.value: Option<String>` — Task 2 定义，Task 6 的前端发 `null`，一致
- `groupNames: string[]`、`groupLabel(name)`、`activeTab: string|null` — Task 5 定义并在同一 Task 内使用
- `v.label` — Task 1 产出，Task 5 Step 6 消费，字段名一致
- `v.min` / `v.max` — `api.rs` 早已输出（`json!` 里的 `"min": spec.min`），Task 6 消费，一致
- 测试辅助 `body_json_text` — Task 4 定义，仅在该 Task 使用

**4. 风险复核**

- Task 6 Step 6 会真实修改用户的 `~/.yi-agent/settings.json`，plan 里已写明备份与还原命令
- Task 4 的测试在 Task 6 之前是红的，plan 明确建议并入 Task 6 提交而非单独提交

---

### Task 8: 全量回归、文档更新、人工验收记录

**Files:**
- Modify: `docs/project-management/yi-agent-web.md`

**Interfaces:**
- Consumes: Task 1-6 全部
- Produces: 无

- [ ] **Step 1: 确认没有其他 cargo 在跑**

```bash
ps aux | grep -c "[c]argo"
```

预期：`0`。若有残留，先 `pkill` 再继续。

- [ ] **Step 2: 跑 web crate 全量**

```bash
export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$HOME/.cargo/bin:/opt/homebrew/bin:/usr/bin:/bin"
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/fix-web-settings-ui
cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-web 2>&1 | grep -E "test result|FAILED"
```

预期：全绿，`api_test` 9 条通过。

- [ ] **Step 3: 跑 runtime crate 确认无连带**

```bash
cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-runtime --lib 2>&1 | grep -E "test result|FAILED"
```

预期：全绿（本 plan 未改该 crate）。

- [ ] **Step 4: `cargo fmt` 与 clippy**

```bash
cd yi-agent-rs && cargo fmt --all && cargo fmt --all -- --check && echo "FMT OK"
cargo clippy -p yi-agent-web --all-targets 2>&1 | grep -E "^(warning|error)" | head -20
```

预期：`FMT OK`；clippy 无 error（warning 若与本次改动无关可忽略，但要贴出原文）。

- [ ] **Step 5: 更新模块文档**

`docs/project-management/yi-agent-web.md`：

1. 删掉「范围边界 → 做什么」里的过时行：

```
- Secret 值掩码显示；未修改的掩码值不回写
```

该能力随 `env_file.rs` 一起删除（`1adb7b46`），现在 settings.json 不含凭据。

2. 在「Features」末尾追加：

```markdown
- [x] 侧栏分组从 `GET /api/config` 的 `data.groups` 派生，不硬编码；显示名走 `GROUP_LABELS` 映射 — `assets/index.html::renderSidebar`
  - 判据：`cargo test -p yi-agent-web --test api_test get_config_groups_match_every_group_in_fields`
  - 判据：`cargo test -p yi-agent-web --test api_test index_html_derives_groups_instead_of_hardcoding_them`
- [x] 字段显示 `FIELDS.label`（中文名）而非机器键名；数字框套用 `min`/`max`；支持「恢复默认」删键；select 有显式空选项 — `api.rs::get_config` + `assets/index.html::renderField`
  - 判据：`cargo test -p yi-agent-web --test api_test get_config_reports_a_label_for_every_field`
  - 判据：`cargo test -p yi-agent-web --test api_test put_config_with_null_value_deletes_the_key`
```

3. 更新 `docs/project-management/README.md` 里 yi-agent-web 的「完成 / 总计」计数（按实际 `[x]` 条数重新数）。

- [ ] **Step 6: 人工验收清单逐条记录**

按下面清单实际跑一遍，把观察结果写进提交说明（不是"已通过"，而是具体看到什么）：

| # | 检查项 | 预期 | 实际 |
|---|---|---|---|
| 1 | 侧栏标签 | 三个：Agent / 压缩 / 启动 | （填） |
| 2 | 每组字段数 | 5 / 3 / 1 | （填） |
| 3 | 字段标签 | 中文名如「最大轮次」 | （填） |
| 4 | 数字上下限 | ratio 上限 100 | （填） |
| 5 | 下拉未设置 | 「未设置（默认 workspace-write）」 | （填） |
| 6 | 恢复默认 | 保存后键从 settings.json 消失 | （填） |

- [ ] **Step 7: 提交**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/fix-web-settings-ui
git add docs/project-management/yi-agent-web.md docs/project-management/README.md
git commit -m "docs(web): 记录分组派生与字段元数据落地

补两条 feature 及可执行判据；删掉「Secret 值掩码显示」——该能力随
env_file.rs 在 1adb7b46 一并删除，settings.json 不含凭据，留着会误导读者。

人工验收 6 项的具体观察结果见本 commit 说明。"
```

---

## Self-Review

**1. Spec coverage**

| Spec 章节 | 对应 Task |
|---|---|
| 根因（分组硬编码） | Task 5 |
| 同源缺口 1（缺 label） | Task 1 |
| 同源缺口 2（无 min/max） | Task 6 Step 1 |
| 同源缺口 3（无恢复默认） | Task 2 + Task 6 Step 3 |
| 同源缺口 4（select 误选） | Task 6 Step 2 |
| 不做项：`settings.rs` 空壳清理硬编码 | 仅记 `bug-list.md`，本 plan 无代码改动 |
| 验证策略：分组完整性 | Task 3 |
| 验证策略：label 全覆盖 | Task 1 |
| 验证策略：HTML 契约 | Task 4 |
| 验证策略：null 删键 | Task 2 |
| 验证策略：省略 value | Task 2 |
| 验证策略：人工验收 | Task 7 Step 6 |
| 完成判据 1-4 | Task 7 Step 2/3/4/6 |

**一处 spec 要求需单独任务：** spec 要求把 `settings.rs:508-518` 的硬编码分组
记进 `docs/bug-list.md`。对应 Task 8（在 Task 7 之后执行）。

---

---


