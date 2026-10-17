# Thread 颜色标签 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让桌面端右键、移动端长按会话行都能打一个 6 色之一的颜色标签，行内以色点呈现，标签显示名可在设置里自定义，变更在所有已连接客户端即时同步。

**Architecture:** 标签 id（`red`/`orange`/…）落在每个 thread 的 `.meta.json`（零迁移扩展，仿 `pin_seq`）；标签的**显示名**是全局宿主级偏好，落在 `preferences.json` 的 `tag_names`（逐键覆盖，默认名只在前端定义一份）。变更经 RPC 写入后广播列表层通知（`thread/tagChanged`）或全局通知（`ui/tagNames/updated`），前端就地落笔，不重拉全量列表。

**Tech Stack:** Rust（app-server：`thread_store` / `settings_store` / `protocol` / `server`）、React + TypeScript（`desktop/src`，Desktop 与移动端共享）、Tailwind v4、vitest、cargo test。

## Global Constraints

- **严禁在 `main` 上直接改代码或提交。** 工作在本计划的 worktree 内完成（`feat/thread-color-tags`）。
- 提交前必须跑 `cargo fmt --all`（在 `yi-agent-rs/` 下）。
- **不要**在 commit message 里写 `Co-Authored-By` 行；用 conventional commits（`feat:` / `fix:` / `docs:`）。
- **不要**在多个 shell 同时跑 `cargo test`（workspace 元数据锁竞争会导致卡死或 exit 137）。跑前 `ps aux | grep cargo` 确认无残留。
- 优先按 crate 跑测试，避免 `cargo test --workspace` 全量。
- 6 个颜色 id 固定为 `red` / `orange` / `yellow` / `green` / `blue` / `purple`，顺序即菜单顺序。**不引入灰色**。
- 默认显示名固定为 `红` / `橙` / `黄` / `绿` / `蓝` / `紫`，**仅在** `desktop/src/lib/threadTags.ts` 定义；Rust 侧不得出现同一套默认名。
- 标签名长度上限 **24** 字符（前端常量 `TAG_NAME_MAX_LEN` 与后端校验值必须一致）。
- 打标签**不得**刷新 `updated_at`。
- 无标签的会话行渲染必须与现状**逐像素一致**（不占位）。
- TUI 不涉及本功能。

### 运行环境（所有任务的测试命令都适用）

本机 `cargo` **不在默认 PATH**，`npm`/`npx` 在 `/opt/homebrew/bin`。每条 cargo 命令都这样写：

```bash
export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH" && cd yi-agent-rs && cargo test ...
```

给它**多个测试名过滤词**时必须用 `--` 分隔，否则 cargo 报 `unexpected argument`：

```bash
cargo test -p yi-agent-app-server --lib -- thread_store::tests::set_tag thread_store::tests::is_valid_tag
```

单个过滤词可省略 `--`（`cargo test -p yi-agent-app-server --lib thread_store` 可用），但为统一，计划中所有含多个过滤词或含 `::` 路径的命令一律用 `--`。前端命令：

```bash
export PATH="/opt/homebrew/bin:$PATH" && cd desktop && npx vitest run <file>
```

本机可能**同时有其它 worktree 在跑 cargo**（并发是常态，环境内存充裕）。不要因此改用 `cargo test --workspace`；始终按 crate 跑。

---

### Task 1: 存储层 — `ThreadMeta.tag` 与 `set_tag`

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs`（结构体 27 行起、`set_pin_seq` 附近、测试模块）

**Interfaces:**
- Consumes: 无（第一个任务）
- Produces:
  - `pub const TAG_IDS: [&str; 6]` = `["red","orange","yellow","green","blue","purple"]`
  - `pub fn is_valid_tag(id: &str) -> bool`
  - `ThreadMeta.tag: Option<String>`
  - `pub fn ThreadStore::set_tag(&self, id: &str, tag: Option<&str>) -> io::Result<bool>`

- [ ] **Step 1: 写失败的测试**

在 `thread_store.rs` 的测试模块内（紧跟 `set_pin_seq_unknown_id_returns_false` 之后）加入：

```rust
    #[test]
    fn set_tag_sets_value_without_bumping_updated_at() {
        let (_d, s) = store();
        let mut m = meta("thread-a");
        m.updated_at = 7;
        s.create(&m).unwrap();
        assert!(s.set_tag("thread-a", Some("red")).unwrap());
        let got = s.load("thread-a").unwrap().unwrap().meta;
        assert_eq!(got.tag.as_deref(), Some("red"));
        assert_eq!(got.updated_at, 7, "打标签不得刷新 updated_at");
    }

    #[test]
    fn set_tag_none_clears_it() {
        let (_d, s) = store();
        s.create(&meta("thread-a")).unwrap();
        s.set_tag("thread-a", Some("blue")).unwrap();
        assert!(s.set_tag("thread-a", None).unwrap());
        assert_eq!(s.load("thread-a").unwrap().unwrap().meta.tag, None);
    }

    #[test]
    fn set_tag_unknown_id_returns_false() {
        let (_d, s) = store();
        assert!(!s.set_tag("nope", Some("red")).unwrap());
    }

    #[test]
    fn legacy_meta_without_tag_deserializes_to_none() {
        let (_d, s) = store();
        std::fs::create_dir_all(&s.root).unwrap();
        // 旧格式：没有 tag 字段。
        let legacy = r#"{"thread_id":"thread-a","cwd":"/tmp","model":"m",
            "created_at":1,"updated_at":1,"title":null,"permission_mode":"normal"}"#;
        std::fs::write(s.meta_path("thread-a"), legacy).unwrap();
        assert_eq!(s.load("thread-a").unwrap().unwrap().meta.tag, None);
    }

    #[test]
    fn is_valid_tag_accepts_the_six_ids_only() {
        for id in TAG_IDS {
            assert!(is_valid_tag(id), "{id} 必须是合法标签");
        }
        assert!(!is_valid_tag("gray"), "不含灰色");
        assert!(!is_valid_tag("RED"), "大小写敏感");
        assert!(!is_valid_tag(""));
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH" && cd yi-agent-rs && cargo test -p yi-agent-app-server --lib -- thread_store::tests::set_tag thread_store::tests::legacy_meta_without_tag thread_store::tests::is_valid_tag`

Expected: 编译失败，报 `no field tag`、`no method named set_tag`、`cannot find value TAG_IDS`。

- [ ] **Step 3: 加字段与常量**

在 `thread_store.rs` 的 `ThreadMeta` 结构体内，`preserve_thinking` 字段**之前**插入：

```rust
    /// 会话的颜色标签 id（`TAG_IDS` 之一）。`None` = 无标签。
    /// 旧 meta 缺字段时默认 `None`（零迁移）。
    #[serde(default)]
    pub tag: Option<String>,
```

在 `ThreadMode` 定义之后、`ThreadMeta` 之前插入：

```rust
/// 合法的颜色标签 id。顺序即 UI 菜单顺序，**不含灰色**（深色主题下与「无标签」
/// 难以区分）。
pub const TAG_IDS: [&str; 6] = ["red", "orange", "yellow", "green", "blue", "purple"];

/// 该字符串是否是合法的颜色标签 id。
pub fn is_valid_tag(id: &str) -> bool {
    TAG_IDS.contains(&id)
}
```

- [ ] **Step 4: 加 `set_tag`**

紧跟 `set_pin_seq` 之后插入：

```rust
    /// 写入会话的颜色标签（`None` = 清除）。
    ///
    /// 有意**不**刷新 `updated_at`：打标签是用户视图标记而非 thread 活动，不应把
    /// 会话顶到「最近更新」最前面（与 `set_pin_seq` / `set_permission_mode` 同理）。
    /// 返回 false 表示 thread 不存在或 meta 不可读。
    pub fn set_tag(&self, id: &str, tag: Option<&str>) -> io::Result<bool> {
        Ok(self
            .update_meta(id, |meta| meta.tag = tag.map(str::to_string))?
            .is_some())
    }
```

- [ ] **Step 5: 修所有 `ThreadMeta` 字面量构造点**

新增字段会让所有不用 `..` 展开的字面量编译失败。逐个补 `tag: None,`（放在 `reasoning_effort` 行之后、`preserve_thinking` 之前）。共 13 处：

`thread_store.rs`：`rebuild_meta`（765 行）、`fn meta`（795）、`fn sample_meta`（1457）、1831、1869 —— 共 5 处。
`server.rs`：7504、13629、15140、15245、15338、16659、17110、17216 —— 共 8 处。

（`thread_store.rs:942` 的 `ThreadMeta { thread_id: bad.into(), ..meta("placeholder") }` 用了 `..` 展开，**不需要**改。）

编译失败时以编译器报错为准逐个补齐——上表是「预扫描」结果，可能因你实现时的行号漂移而有出入。

- [ ] **Step 6: 跑测试确认通过**

Run: `export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH" && cd yi-agent-rs && cargo test -p yi-agent-app-server --lib thread_store`

Expected: PASS（含既有 pin_seq 测试）。

- [ ] **Step 7: 提交**

```bash
cd yi-agent-rs && cargo fmt --all && cd ..
git add yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs yi-agent-rs/crates/yi-agent-app-server/src/server.rs
git commit -m "feat(store): thread meta 增加颜色标签字段与 set_tag"
```

---

### Task 2: 全局偏好 — 标签显示名的读写

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/settings_store.rs`

**Interfaces:**
- Consumes: Task 1 的 `TAG_IDS` / `is_valid_tag`（仅测试与校验语义参照）
- Produces:
  - `pub const TAG_NAMES_KEY: &str = "tag_names"`
  - `pub fn load_tag_names(workdir: &Path) -> BTreeMap<String, String>`
  - `pub fn save_tag_names(workdir: &Path, updates: &BTreeMap<String, String>) -> io::Result<()>`

**注意语义：** `load_tag_names` 返回**磁盘上实际的覆盖项**（可能只有 1 个键，也可能空），**不**补齐默认名——默认名只在前端定义（Global Constraints）。

**注意日志契约：** 读侧必须与同文件 `load()` 的既有契约一致——**缺文件是正常情形不打日志**；文件存在却读不出来 / 解析不了则 `tracing::warn!`（两类措辞不同）。理由是 `load()` 注释里写下的同一条：否则「我改的标签名总是复位」在日志里无从诊断。**不要**用裸 `.ok()` 链静默吞掉这两类错误（`load_watchman_enabled` / `load_relay_url` 是早期写法，本任务不跟）。值的形状不是对象（如 `{"tag_names": "not-an-object"}`）仍是**静默**当空映射，不告警。

- [ ] **Step 1: 写失败的测试**

在 `settings_store.rs` 测试模块内加入：

```rust
    #[test]
    fn load_tag_names_returns_empty_map_when_absent() {
        let dir = tempfile::TempDir::new().unwrap();
        assert!(load_tag_names(dir.path()).is_empty());
    }

    #[test]
    fn load_tag_names_returns_only_stored_overrides() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut updates = std::collections::BTreeMap::new();
        updates.insert("red".to_string(), "紧急".to_string());
        save_tag_names(dir.path(), &updates).unwrap();
        let got = load_tag_names(dir.path());
        assert_eq!(got.len(), 1, "只存了 1 个键就只该回 1 个: {got:?}");
        assert_eq!(got.get("red").map(String::as_str), Some("紧急"));
    }

    #[test]
    fn save_tag_names_merges_and_preserves_other_keys() {
        let dir = tempfile::TempDir::new().unwrap();
        save(dir.path(), Theme::Light).unwrap();
        save_relay_url(dir.path(), Some("wss://r/connect?session=x")).unwrap();

        let mut first = std::collections::BTreeMap::new();
        first.insert("red".to_string(), "紧急".to_string());
        save_tag_names(dir.path(), &first).unwrap();

        // 第二次只改 blue：red 必须还在（合并而非替换）。
        let mut second = std::collections::BTreeMap::new();
        second.insert("blue".to_string(), "参考".to_string());
        save_tag_names(dir.path(), &second).unwrap();

        let got = load_tag_names(dir.path());
        assert_eq!(got.get("red").map(String::as_str), Some("紧急"), "red 不得被抹掉");
        assert_eq!(got.get("blue").map(String::as_str), Some("参考"));

        // 同文件的其它键不受影响。
        assert_eq!(load(dir.path()), Theme::Light);
        assert_eq!(
            load_relay_url(dir.path()).as_deref(),
            Some("wss://r/connect?session=x")
        );
    }

    #[test]
    fn load_tag_names_is_empty_on_corrupt_value_shape() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join(".yi-agent")).unwrap();
        std::fs::write(
            preferences_path(dir.path()),
            r#"{"tag_names": "not-an-object"}"#,
        )
        .unwrap();
        assert!(load_tag_names(dir.path()).is_empty(), "坏形状当空映射");
    }
```

另外在 `warning_tests` 模块内补齐标签名的日志契约测试（照该模块既有的三个 theme 测试写，同一个 `capture_logs` 辅助）：

```rust
    /// 缺文件是正常的首次运行,不能告警。
    #[test]
    fn a_missing_tag_names_file_is_not_logged() {
        let dir = tempfile::TempDir::new().unwrap();
        let (_names, logged) = capture_logs(|| load_tag_names(dir.path()));
        assert!(
            logged.is_empty(),
            "a genuinely missing file must not warn: {logged}"
        );
    }

    /// 存在却读不出来:必须 warn 且指明路径,与「解析失败」措辞可区分。
    #[test]
    fn an_unreadable_tag_names_file_is_logged_and_falls_back_to_empty() {
        let dir = tempfile::TempDir::new().unwrap();
        let prefs = dir.path().join(".yi-agent").join("preferences.json");
        std::fs::create_dir_all(&prefs).unwrap(); // 目录 → read_to_string 失败且非 NotFound
        let (names, logged) = capture_logs(|| load_tag_names(dir.path()));
        assert!(names.is_empty());
        assert!(logged.contains("WARN"), "must be a warning: {logged}");
        assert!(
            logged.contains("preferences.json"),
            "must name the file: {logged}"
        );
        assert!(
            logged.contains("could not read"),
            "must distinguish the read failure: {logged}"
        );
        assert!(
            !logged.contains("invalid"),
            "a read failure is not a parse failure: {logged}"
        );
    }

    /// 存在但损坏(非法 JSON):必须 warn 且与「读失败」措辞可区分。
    #[test]
    fn a_corrupt_tag_names_file_is_logged_and_falls_back_to_empty() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join(".yi-agent")).unwrap();
        std::fs::write(preferences_path(dir.path()), "not json at all").unwrap();
        let (names, logged) = capture_logs(|| load_tag_names(dir.path()));
        assert!(names.is_empty());
        assert!(logged.contains("WARN"), "must be a warning: {logged}");
        assert!(
            logged.contains("preferences.json"),
            "must name the file: {logged}"
        );
        assert!(
            logged.contains("invalid"),
            "must distinguish the parse failure: {logged}"
        );
        assert!(
            !logged.contains("could not read"),
            "a parse failure is not a read failure: {logged}"
        );
    }
```

- [ ] **Step 1b: 确认既有 warning 契约测试仍在**

Run: `export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH" && cd yi-agent-rs && cargo test -p yi-agent-app-server --lib -- settings_store::warning_tests`

Expected: PASS（主题侧 4 个测试不受影响）。

- [ ] **Step 2: 跑测试确认失败**

Run: `export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH" && cd yi-agent-rs && cargo test -p yi-agent-app-server --lib -- settings_store::tests::load_tag_names settings_store::tests::save_tag_names`

Expected: 编译失败，报 `cannot find function load_tag_names`。

- [ ] **Step 3: 实现**

在 `settings_store.rs` 的 `save_relay_url` 之后插入：

```rust
/// 颜色标签显示名覆盖（`preferences.json` 的 `tag_names` 键）。
pub const TAG_NAMES_KEY: &str = "tag_names";

/// 读用户自定义的颜色标签显示名（颜色 id → 名称）。
///
/// 返回磁盘上**实际存在的覆盖项**（可能只有 1 个键，也可能是空映射）。**刻意不**
/// 在此补齐 6 个默认名：默认名由前端 `threadTags.ts` 唯一持有，后端再定义一套就会
/// 变成两份必须手工保持一致的默认名。
///
/// 与 [`load`] 同一日志约定：缺文件是正常情形，不打日志；文件存在却读不出来或解析
/// 不了则 `tracing::warn!`（两类措辞不同），否则「我改的标签名总是复位」在日志里
/// 无从诊断。值的形状不是对象（`{"tag_names": "..."}`）静默当空映射——坏偏好绝不
/// 阻断 UI。
pub fn load_tag_names(workdir: &Path) -> std::collections::BTreeMap<String, String> {
    let path = preferences_path(workdir);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        // 缺文件是首次运行的正常情形,不是故障,不打日志。
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return std::collections::BTreeMap::new()
        }
        Err(error) => {
            tracing::warn!(
                error = %error,
                path = %path.display(),
                "could not read the tag names preference; using the defaults"
            );
            return std::collections::BTreeMap::new();
        }
    };
    match serde_json::from_str::<serde_json::Value>(&text) {
        Ok(value) => value
            .get(TAG_NAMES_KEY)
            .and_then(|v| v.as_object())
            .map(|object| {
                object
                    .iter()
                    .filter_map(|(key, value)| value.as_str().map(|s| (key.clone(), s.to_string())))
                    .collect()
            })
            .unwrap_or_default(),
        Err(error) => {
            tracing::warn!(
                error = %error,
                path = %path.display(),
                "invalid tag names preference; using the defaults"
            );
            std::collections::BTreeMap::new()
        }
    }
}

/// 合并写入颜色标签显示名：只覆盖 `updates` 提到的键，保留既有的其它标签键与
/// 同文件的无关顶层键（theme / relay_url / …）。
///
/// [`read_write_key`] 只写单个顶层键，故「按键合并」在它之上实现：先读出既有覆盖
/// 项、并入 `updates`，再把整个对象作为 `tag_names` 的值原子落盘。
pub fn save_tag_names(
    workdir: &Path,
    updates: &std::collections::BTreeMap<String, String>,
) -> std::io::Result<()> {
    let mut merged = load_tag_names(workdir);
    for (key, value) in updates {
        merged.insert(key.clone(), value.clone());
    }
    let object: serde_json::Map<String, serde_json::Value> = merged
        .into_iter()
        .map(|(key, value)| (key, serde_json::Value::String(value)))
        .collect();
    set_object_value(workdir, TAG_NAMES_KEY, serde_json::Value::Object(object))
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH" && cd yi-agent-rs && cargo test -p yi-agent-app-server --lib settings_store`

Expected: PASS（含既有 relay_url / theme 测试）。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all && cd ..
git add yi-agent-rs/crates/yi-agent-app-server/src/settings_store.rs
git commit -m "feat(settings): preferences.json 支持标签显示名覆盖"
```

---

### Task 3: 协议 — 两条新通知

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/protocol.rs`（`Notification` 枚举、`thread_key`、`delivery`、测试模块）

**Interfaces:**
- Consumes: 无
- Produces:
  - `Notification::TagChanged { thread_id: String, tag: Option<String> }` — wire `thread/tagChanged`，`Delivery::List`
  - `Notification::TagNamesUpdated { tag_names: BTreeMap<String, String> }` — wire `ui/tagNames/updated`，`Delivery::Global`

- [ ] **Step 1: 写失败的测试**

在 `protocol.rs` 测试模块内，紧跟 `title_changed_notification_carries_method_and_title` 之后加入：

```rust
    /// 标签变更通知的 wire 形状：`thread/tagChanged`，参数 snake_case。
    ///
    /// 与 `titleChanged` 同族：会话元数据、列表层恒推、按 thread 路由、直接携带
    /// 权威值（`null` = 已清除）。窗口外会话的标签变更同样要送达，否则手机端
    /// 「概率性不同步」。
    #[test]
    fn tag_changed_notification_carries_method_and_tag() {
        let n = Notification::TagChanged {
            thread_id: "t1".into(),
            tag: Some("red".into()),
        };
        let v: Value = serde_json::to_value(NotificationEnvelope::new(&n)).unwrap();
        assert_eq!(v["method"], "thread/tagChanged");
        assert_eq!(v["params"]["thread_id"], "t1");
        assert_eq!(v["params"]["tag"], "red");
        assert_eq!(n.delivery(), Delivery::List);
        assert_eq!(n.thread_key(), Some("t1"));
    }

    #[test]
    fn tag_changed_notification_serializes_a_cleared_tag_as_null() {
        let n = Notification::TagChanged {
            thread_id: "t1".into(),
            tag: None,
        };
        let v: Value = serde_json::to_value(NotificationEnvelope::new(&n)).unwrap();
        assert!(v["params"]["tag"].is_null(), "清除必须是显式 null: {v}");
    }

    /// 标签显示名变更通知：**全局**帧，不属于任何 thread。
    #[test]
    fn tag_names_updated_notification_is_global() {
        let mut names = std::collections::BTreeMap::new();
        names.insert("red".to_string(), "紧急".to_string());
        let n = Notification::TagNamesUpdated { tag_names: names };
        let v: Value = serde_json::to_value(NotificationEnvelope::new(&n)).unwrap();
        assert_eq!(v["method"], "ui/tagNames/updated");
        assert_eq!(v["params"]["tag_names"]["red"], "紧急");
        assert_eq!(n.delivery(), Delivery::Global);
        assert_eq!(n.thread_key(), None);
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH" && cd yi-agent-rs && cargo test -p yi-agent-app-server --lib protocol::tests::tag`

Expected: 编译失败，报 `no variant named TagChanged`。

- [ ] **Step 3: 加枚举变体**

在 `Notification::TitleChanged` 之后插入：

```rust
    /// 该会话的颜色标签已变（`thread/setTag`）。
    ///
    /// 与 `thread/titleChanged` 同族：标签是会话元数据，变更后必须让**所有**客户端
    /// 立即刷新侧栏色点。**直接携带权威值**（`None` = 已清除），客户端就地落笔，
    /// 不必重拉全量 `thread/listAll`。
    #[serde(rename = "thread/tagChanged")]
    TagChanged {
        thread_id: String,
        tag: Option<String>,
    },
```

在 `UiSettingsUpdated` 变体之后插入：

```rust
    /// 颜色标签的显示名（颜色 id → 名称）已变。
    ///
    /// **全局**帧：标签名是对所有会话生效的宿主级偏好，与任何 thread 上下文无关。
    /// **携带用户覆盖的权威映射**（可能只含被改的键），客户端与自己的默认名表合并
    /// 后即可渲染。
    ///
    /// 为什么独立成一条而不是挤进 `ui/settings/updated`：后者由主题 watcher 从
    /// 主题广播通道泵出（`server.rs` 的 `pump_theme_notifications`），语义是「主题
    /// 变了」；标签名只经 `ui/settings/write` 变更、没有 watcher。塞进去会让该通知
    /// 时而带 `tag_names` 时而不带（watcher 路径没有它），语义含混。
    #[serde(rename = "ui/tagNames/updated")]
    TagNamesUpdated {
        tag_names: std::collections::BTreeMap<String, String>,
    },
```

- [ ] **Step 4: 接进 `thread_key` 与 `delivery`**

在 `thread_key()` 中，把 `Notification::TagChanged { thread_id, .. }` 加进返回 `Some(thread_id)` 的那一臂（紧邻 `TitleChanged`）；并把 `Notification::TagNamesUpdated { .. }` 加进返回 `None` 的那一臂（与 `UiSettingsUpdated` 同列）。

在 `delivery()` 中，`Delivery::List` 那一臂加 `Notification::TagChanged { .. }`（紧邻 `TitleChanged`）；`Delivery::Global` 那一臂加 `Notification::TagNamesUpdated { .. }`（紧邻 `UiSettingsUpdated`）。

- [ ] **Step 5: 跑测试确认通过**

Run: `export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH" && cd yi-agent-rs && cargo test -p yi-agent-app-server --lib protocol`

Expected: PASS。

- [ ] **Step 6: 提交**

```bash
cd yi-agent-rs && cargo fmt --all && cd ..
git add yi-agent-rs/crates/yi-agent-app-server/src/protocol.rs
git commit -m "feat(protocol): 新增 thread/tagChanged 与 ui/tagNames/updated 通知"
```

---

### Task 4: 服务层 — `thread/setTag` RPC 与列表输出

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`（`thread/setPinned` 与 `thread/reorderPinned` 之间、`thread_summary_json`、测试模块）

**Interfaces:**
- Consumes: Task 1 的 `ThreadStore::set_tag` / `is_valid_tag`；Task 3 的 `Notification::TagChanged`
- Produces: RPC `thread/setTag`（`params: { threadId, tag: string | null }` → `{}`）；`thread_summary_json` 输出增加 `"tag"` 字段

- [ ] **Step 1: 写失败的测试**

在 `server.rs` 测试模块内（`set_pinned_*` 测试附近）加入：

```rust
    #[tokio::test(flavor = "multi_thread")]
    async fn set_tag_persists_and_reports_in_list() {
        let dir = tempfile::TempDir::new().unwrap();
        let cfg = pin_test_config(dir.path());
        let mut h = Harness::with_config(cfg, build_test_agent, Duration::from_secs(5));
        initialize(&mut h).await;

        h.send(r#"{"jsonrpc":"2.0","id":2,"method":"thread/start","params":{}}"#)
            .await;
        let tid = read_thread_start_response(&mut h, 2).await;

        let set = serde_json::json!({"jsonrpc":"2.0","id":3,"method":"thread/setTag",
            "params":{"threadId": tid, "tag": "red"}});
        h.send(&set.to_string()).await;
        let resp = read_response(&mut h, 3).await;
        assert!(resp.get("error").is_none(), "setTag 应成功: {resp}");

        h.send(r#"{"jsonrpc":"2.0","id":4,"method":"thread/listAll","params":{}}"#)
            .await;
        let v = read_response(&mut h, 4).await;
        let me = v["result"]["groups"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|g| g["threads"].as_array().unwrap().iter())
            .find(|t| t["thread_id"].as_str() == Some(tid.as_str()))
            .expect("in group");
        assert_eq!(me["tag"], "red");
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn set_tag_null_clears() {
        let dir = tempfile::TempDir::new().unwrap();
        let cfg = pin_test_config(dir.path());
        let mut h = Harness::with_config(cfg, build_test_agent, Duration::from_secs(5));
        initialize(&mut h).await;

        h.send(r#"{"jsonrpc":"2.0","id":2,"method":"thread/start","params":{}}"#)
            .await;
        let tid = read_thread_start_response(&mut h, 2).await;

        for (id, tag) in [(3u64, serde_json::json!("green")), (4, serde_json::Value::Null)] {
            let req = serde_json::json!({"jsonrpc":"2.0","id": id, "method":"thread/setTag",
                "params":{"threadId": tid, "tag": tag}});
            h.send(&req.to_string()).await;
            let resp = read_response(&mut h, id).await;
            assert!(resp.get("error").is_none(), "setTag 应成功: {resp}");
        }

        h.send(r#"{"jsonrpc":"2.0","id":5,"method":"thread/listAll","params":{}}"#)
            .await;
        let v = read_response(&mut h, 5).await;
        let me = v["result"]["groups"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|g| g["threads"].as_array().unwrap().iter())
            .find(|t| t["thread_id"].as_str() == Some(tid.as_str()))
            .expect("in group");
        assert!(me["tag"].is_null(), "清除后应为 null: {me}");
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn set_tag_rejects_unknown_color_and_unknown_thread() {
        let dir = tempfile::TempDir::new().unwrap();
        let cfg = pin_test_config(dir.path());
        let mut h = Harness::with_config(cfg, build_test_agent, Duration::from_secs(5));
        initialize(&mut h).await;

        // 先校验颜色：非法值一律 -32602，无论 thread 是否存在。
        h.send(r#"{"jsonrpc":"2.0","id":2,"method":"thread/setTag","params":{"threadId":"thread-x","tag":"chartreuse"}}"#).await;
        let v = read_response(&mut h, 2).await;
        assert_eq!(v["error"]["code"], serde_json::json!(-32602), "{v}");

        h.send(r#"{"jsonrpc":"2.0","id":3,"method":"thread/setTag","params":{"threadId":"thread-x","tag":"red"}}"#).await;
        let v = read_response(&mut h, 3).await;
        assert_eq!(
            v["error"]["code"],
            serde_json::json!(-32011),
            "合法颜色 + 未知 thread 应为 unknown_thread: {v}"
        );
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn set_tag_broadcasts_tag_changed() {
        let dir = tempfile::TempDir::new().unwrap();
        let cfg = pin_test_config(dir.path());
        let mut h = Harness::with_config(cfg, build_test_agent, Duration::from_secs(5));
        initialize(&mut h).await;

        h.send(r#"{"jsonrpc":"2.0","id":2,"method":"thread/start","params":{}}"#)
            .await;
        let tid = read_thread_start_response(&mut h, 2).await;

        let set = serde_json::json!({"jsonrpc":"2.0","id":3,"method":"thread/setTag",
            "params":{"threadId": tid, "tag": "purple"}});
        h.send(&set.to_string()).await;
        let _ = read_response(&mut h, 3).await;

        let ev = read_until_method(&mut h, "thread/tagChanged").await;
        assert_eq!(ev["params"]["thread_id"], tid);
        assert_eq!(ev["params"]["tag"], "purple");
        h.shutdown().await;
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH" && cd yi-agent-rs && cargo test -p yi-agent-app-server --lib server::tests::set_tag`

Expected: 失败——`thread/setTag` 是未知方法（测试会读到错误回包或超时断言不符）。

- [ ] **Step 3: 加 RPC 分支**

在 `server.rs` 的 `"thread/setPinned" => { … }` 之后、`"thread/reorderPinned" => {` 之前插入：

```rust
                    "thread/setTag" => {
                        let Some(thread_id) =
                            require_thread_id(&hub, &client, &req.params, id.clone()).await?
                        else {
                            continue;
                        };
                        // 参数校验先于 thread 存在性：非法颜色一律 `-32602`，
                        // 与 setPinned 的「非布尔先报错」同一顺序。
                        let raw = req
                            .params
                            .get("tag")
                            .cloned()
                            .unwrap_or(serde_json::Value::Null);
                        let tag: Option<String> = match raw {
                            serde_json::Value::Null => None,
                            serde_json::Value::String(s)
                                if crate::thread_store::is_valid_tag(&s) =>
                            {
                                Some(s)
                            }
                            other => {
                                write_response(
                                    &hub,
                                    &client,
                                    err_response(
                                        id,
                                        RpcError::invalid_params(format!(
                                            "tag must be null or one of the 6 palette ids, got {other}"
                                        )),
                                    ),
                                )
                                .await?;
                                continue;
                            }
                        };
                        match store_lookup(&threads, &workspaces, &cfg, &thread_id)
                            .set_tag(&thread_id, tag.as_deref())
                        {
                            Ok(true) => {
                                write_response(&hub, &client, ok_response(id, json!({}))).await?;
                                // 广播给所有客户端（含本端）：列表层恒推，就在地
                                // 更新色点，无需各自重拉 thread/listAll。
                                let _ = write_notification(
                                    &hub,
                                    &Notification::TagChanged {
                                        thread_id: thread_id.clone(),
                                        tag: tag.clone(),
                                    },
                                )
                                .await;
                            }
                            Ok(false) => {
                                write_response(
                                    &hub,
                                    &client,
                                    err_response(id, RpcError::unknown_thread(&thread_id)),
                                )
                                .await?;
                            }
                            Err(e) => {
                                write_response(
                                    &hub,
                                    &client,
                                    err_response(id, RpcError::internal(e.to_string())),
                                )
                                .await?;
                            }
                        }
                    }
```

若 `Notification` 尚未在 `server.rs` 顶部导入，把它加进第 26 行的 `use crate::protocol::{…}` 列表。

- [ ] **Step 4: 加列表输出字段**

在 `thread_summary_json` 里，`"card_id": m.card_id,` 之后插入：

```rust
        "tag": m.tag,
```

- [ ] **Step 5: 跑测试确认通过**

Run: `export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH" && cd yi-agent-rs && cargo test -p yi-agent-app-server --lib server::tests::set_tag`

Expected: PASS（4 个测试）。

- [ ] **Step 6: 提交**

```bash
cd yi-agent-rs && cargo fmt --all && cd ..
git add yi-agent-rs/crates/yi-agent-app-server/src/server.rs
git commit -m "feat(app-server): thread/setTag RPC 与列表 tag 字段"
```

---

### Task 5: 服务层 — `ui/settings/*` 读写标签名

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`（`ui/settings/read` 分支、`ui/settings/write` 分支、测试模块）

**Interfaces:**
- Consumes: Task 2 的 `load_tag_names` / `save_tag_names` / `TAG_NAMES_KEY`；Task 1 的 `is_valid_tag`；Task 3 的 `Notification::TagNamesUpdated`
- Produces: `ui/settings/read` 回包增加 `tag_names`（对象）；`ui/settings/write` 接受可选 `tag_names` 并广播 `ui/tagNames/updated`

- [ ] **Step 1: 写失败的测试**

在 `server.rs` 测试模块内加入：

```rust
    #[tokio::test(flavor = "multi_thread")]
    async fn ui_settings_read_returns_tag_names_overrides() {
        let dir = tempfile::TempDir::new().unwrap();
        let cfg = pin_test_config(dir.path());
        let mut h = Harness::with_config(cfg, build_test_agent, Duration::from_secs(5));
        initialize(&mut h).await;

        // 先写 1 个覆盖，再读回：应恰有这 1 个键（默认名不由后端补齐）。
        h.send(r#"{"jsonrpc":"2.0","id":2,"method":"ui/settings/write","params":{"tag_names":{"red":"紧急"}}}"#).await;
        let _ = read_response(&mut h, 2).await;

        h.send(r#"{"jsonrpc":"2.0","id":3,"method":"ui/settings/read","params":{}}"#)
            .await;
        let v = read_response(&mut h, 3).await;
        let names = v["result"]["tag_names"]
            .as_object()
            .expect("tag_names 必须是对象");
        assert_eq!(names.len(), 1, "只回用户覆盖项: {v}");
        assert_eq!(names["red"], "紧急");
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn ui_settings_write_rejects_invalid_tag_name_key_and_empty_value() {
        let dir = tempfile::TempDir::new().unwrap();
        let cfg = pin_test_config(dir.path());
        let mut h = Harness::with_config(cfg, build_test_agent, Duration::from_secs(5));
        initialize(&mut h).await;

        // 非法颜色 id。
        h.send(r#"{"jsonrpc":"2.0","id":2,"method":"ui/settings/write","params":{"tag_names":{"gray":"灰"}}}"#).await;
        let v = read_response(&mut h, 2).await;
        assert_eq!(v["error"]["code"], serde_json::json!(-32602), "{v}");

        // 空白名字。
        h.send(r#"{"jsonrpc":"2.0","id":3,"method":"ui/settings/write","params":{"tag_names":{"red":"   "}}}"#).await;
        let v = read_response(&mut h, 3).await;
        assert_eq!(v["error"]["code"], serde_json::json!(-32602), "{v}");

        // 超长名字（>24）。
        let long = "长".repeat(25);
        let req = serde_json::json!({"jsonrpc":"2.0","id":4,"method":"ui/settings/write",
            "params":{"tag_names":{"red": long}}});
        h.send(&req.to_string()).await;
        let v = read_response(&mut h, 4).await;
        assert_eq!(v["error"]["code"], serde_json::json!(-32602), "{v}");
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn ui_settings_write_broadcasts_tag_names_updated() {
        let dir = tempfile::TempDir::new().unwrap();
        let cfg = pin_test_config(dir.path());
        let mut h = Harness::with_config(cfg, build_test_agent, Duration::from_secs(5));
        initialize(&mut h).await;

        h.send(r#"{"jsonrpc":"2.0","id":2,"method":"ui/settings/write","params":{"tag_names":{"blue":"参考"}}}"#).await;
        let v = read_response(&mut h, 2).await;
        assert!(v.get("error").is_none(), "{v}");

        let ev = read_until_method(&mut h, "ui/tagNames/updated").await;
        assert_eq!(ev["params"]["tag_names"]["blue"], "参考");
        h.shutdown().await;
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH" && cd yi-agent-rs && cargo test -p yi-agent-app-server --lib -- server::tests::ui_settings_read_returns_tag_names server::tests::ui_settings_write_broadcasts`

Expected: 失败（`tag_names` 尚未出现在 read 回包 / 未广播）。

- [ ] **Step 3: 扩展 `ui/settings/read`**

在 `"ui/settings/read" => { … }` 分支里，`let relay_url = …;` 之后加入：

```rust
                        // 标签显示名：与 theme/值守/中继同处一个 preferences.json，
                        // 故同样从 workdir 读。只回**用户覆盖项**，默认名由前端持有。
                        let tag_names = crate::settings_store::load_tag_names(workdir);
```

并把回包改成（在 `"relay_url": relay_url,` 之后加一行）：

```rust
                                    "tag_names": tag_names,
```

- [ ] **Step 4: 扩展 `ui/settings/write`**

在 `ui/settings/write` 分支内、`relay_url` 处理块**之前**插入（先校验后落盘，避免半个写入）：

```rust
                        // 标签显示名：键必须是 6 色之一，值 trim 后非空且不超 24 字符。
                        // 校验先于落盘——非法名字不该把一次有效的 relay_url 写了一半。
                        let tag_updates: Option<
                            std::collections::BTreeMap<String, String>,
                        > = match req.params.get("tag_names") {
                            None => None,
                            Some(value) => {
                                let Some(object) = value.as_object() else {
                                    write_response(
                                        &hub,
                                        &client,
                                        err_response(
                                            id,
                                            RpcError::invalid_params(
                                                "tag_names must be an object of color id -> name",
                                            ),
                                        ),
                                    )
                                    .await?;
                                    continue;
                                };
                                let mut updates = std::collections::BTreeMap::new();
                                let mut error: Option<String> = None;
                                for (key, value) in object {
                                    if !crate::thread_store::is_valid_tag(key) {
                                        error = Some(format!("unknown tag id '{key}'"));
                                        break;
                                    }
                                    let Some(name) = value.as_str() else {
                                        error = Some(format!("tag name for '{key}' must be a string"));
                                        break;
                                    };
                                    let name = name.trim();
                                    if name.is_empty() {
                                        error = Some(format!("tag name for '{key}' must not be blank"));
                                        break;
                                    }
                                    if name.chars().count() > TAG_NAME_MAX_LEN {
                                        error = Some(format!(
                                            "tag name for '{key}' exceeds {TAG_NAME_MAX_LEN} characters"
                                        ));
                                        break;
                                    }
                                    updates.insert(key.clone(), name.to_string());
                                }
                                match error {
                                    Some(message) => {
                                        write_response(
                                            &hub,
                                            &client,
                                            err_response(id, RpcError::invalid_params(message)),
                                        )
                                        .await?;
                                        continue;
                                    }
                                    None => Some(updates),
                                }
                            }
                        };
                        if let Some(updates) = &tag_updates {
                            let workdir = theme_handle.workdir();
                            if let Err(error) =
                                crate::settings_store::save_tag_names(workdir, updates)
                            {
                                write_response(
                                    &hub,
                                    &client,
                                    err_response(id, RpcError::internal(error.to_string())),
                                )
                                .await?;
                                continue;
                            }
                        }
```

在文件里 `ui/settings/write` 分支的收尾（`write_response(&hub, &client, ok_response(id, json!({ "ok": true })))` 之前）插入广播：

```rust
                        // 名字落盘成功后广播权威覆盖映射（全局帧），让其它设备
                        // 立即刷新菜单与搜索。放在最后：主题/中继的写入都已生效。
                        if tag_updates.is_some() {
                            let names = crate::settings_store::load_tag_names(
                                theme_handle.workdir(),
                            );
                            let _ = write_notification(
                                &hub,
                                &Notification::TagNamesUpdated { tag_names: names },
                            )
                            .await;
                        }
```

在 `server.rs` 顶部（`thread_store` 引用附近）定义常量：

```rust
/// 标签显示名的长度上限。必须与前端 `desktop/src/lib/threadTags.ts` 的
/// `TAG_NAME_MAX_LEN` 一致——超出会撑破菜单。
const TAG_NAME_MAX_LEN: usize = 24;
```

- [ ] **Step 5: 跑测试确认通过**

Run: `export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH" && cd yi-agent-rs && cargo test -p yi-agent-app-server --lib server::tests::ui_settings`

Expected: PASS（含既有 ui_settings 测试）。

- [ ] **Step 6: 提交**

```bash
cd yi-agent-rs && cargo fmt --all && cd ..
git add yi-agent-rs/crates/yi-agent-app-server/src/server.rs
git commit -m "feat(app-server): ui/settings 读写标签显示名并广播"
```

---

### Task 6: 前端 — 标签常量与纯函数模块

**Files:**
- Create: `desktop/src/lib/threadTags.ts`
- Test: `desktop/src/lib/threadTags.test.ts`

**Interfaces:**
- Consumes: 无
- Produces:
  - `TAG_IDS: readonly ["red","orange","yellow","green","blue","purple"]`，`type TagId`
  - `DEFAULT_TAG_NAMES: Record<TagId, string>`、`TAG_DOT_CLASS: Record<TagId, string>`
  - `TAG_NAME_MAX_LEN = 24`
  - `isValidTagId(v: unknown): v is TagId`
  - `resolveTagNames(overrides?: Record<string, string> | null): Record<TagId, string>`
  - `tagLabel(id: TagId, names: Record<TagId, string>): string`

- [ ] **Step 1: 写失败的测试**

创建 `desktop/src/lib/threadTags.test.ts`：

```ts
import { describe, expect, it } from "vitest";
import {
  DEFAULT_TAG_NAMES,
  TAG_DOT_CLASS,
  TAG_IDS,
  TAG_NAME_MAX_LEN,
  isValidTagId,
  resolveTagNames,
  tagLabel,
} from "./threadTags";

describe("threadTags", () => {
  it("has exactly six ids in the fixed menu order", () => {
    expect(TAG_IDS).toEqual(["red", "orange", "yellow", "green", "blue", "purple"]);
  });

  it("defines a name and a dot class for every id", () => {
    expect(Object.keys(DEFAULT_TAG_NAMES).sort()).toEqual([...TAG_IDS].sort());
    expect(Object.keys(TAG_DOT_CLASS).sort()).toEqual([...TAG_IDS].sort());
    for (const id of TAG_IDS) {
      expect(DEFAULT_TAG_NAMES[id].length).toBeGreaterThan(0);
      expect(DEFAULT_TAG_NAMES[id].length).toBeLessThanOrEqual(TAG_NAME_MAX_LEN);
      expect(TAG_DOT_CLASS[id]).toMatch(/^bg-/);
    }
  });

  it("accepts the six ids and rejects everything else", () => {
    for (const id of TAG_IDS) expect(isValidTagId(id)).toBe(true);
    for (const bad of ["gray", "RED", "", null, undefined, 1, {}, [], true]) {
      expect(isValidTagId(bad)).toBe(false);
    }
  });

  it("resolves missing overrides back to the defaults", () => {
    expect(resolveTagNames({})).toEqual(DEFAULT_TAG_NAMES);
    expect(resolveTagNames(null)).toEqual(DEFAULT_TAG_NAMES);
    expect(resolveTagNames(undefined)).toEqual(DEFAULT_TAG_NAMES);

    const partial = resolveTagNames({ red: "紧急" });
    expect(partial.red).toBe("紧急");
    // 部分覆盖不得丢掉其它 5 个键。
    expect(partial.orange).toBe(DEFAULT_TAG_NAMES.orange);
    expect(Object.keys(partial).sort()).toEqual([...TAG_IDS].sort());
  });

  it("ignores blank and unknown-key overrides", () => {
    const names = resolveTagNames({ red: "   ", gray: "灰" } as Record<string, string>);
    expect(names.red).toBe(DEFAULT_TAG_NAMES.red);
    expect("gray" in names).toBe(false);
  });

  it("trims overrides", () => {
    expect(resolveTagNames({ blue: "  参考  " }).blue).toBe("参考");
  });

  it("labels a tag, falling back to the default name", () => {
    expect(tagLabel("red", resolveTagNames({ red: "紧急" }))).toBe("紧急");
    expect(tagLabel("red", resolveTagNames({}))).toBe(DEFAULT_TAG_NAMES.red);
    // 名字表缺键也不抛。
    expect(tagLabel("blue", {} as Record<never, never> & Record<string, string>)).toBe(
      DEFAULT_TAG_NAMES.blue,
    );
  });
});
```

- [ ] **Step 2: 跑测试确认失败**

Run: `export PATH="/opt/homebrew/bin:$PATH" && cd desktop && npx vitest run src/lib/threadTags.test.ts`

Expected: FAIL，报找不到模块 `./threadTags`。

- [ ] **Step 3: 实现**

创建 `desktop/src/lib/threadTags.ts`：

```ts
/**
 * Thread 颜色标签的常量与纯函数。
 *
 * 纯模块：不碰 React、不碰 RPC。
 *
 * **这里是标签默认显示名的唯一权威定义处。** Rust 侧刻意不内置同一套默认名
 * （`settings_store::load_tag_names` 只回用户覆盖项），否则两份中文默认名必须
 * 手工保持同步，迟早漂移。
 */

/** 6 个固定颜色 id，顺序即菜单顺序。**不含灰色**：深色主题下灰点与「无标签」难以区分。 */
export const TAG_IDS = ["red", "orange", "yellow", "green", "blue", "purple"] as const;

export type TagId = (typeof TAG_IDS)[number];

/** 颜色 id → 默认显示名。用户可在设置里覆盖，未覆盖的键回退到这里。 */
export const DEFAULT_TAG_NAMES: Record<TagId, string> = {
  red: "红",
  orange: "橙",
  yellow: "黄",
  green: "绿",
  blue: "蓝",
  purple: "紫",
};

/** 颜色 id → 色点 class。与既有的未读点同尺寸（`size-2 rounded-full`）。 */
export const TAG_DOT_CLASS: Record<TagId, string> = {
  red: "bg-red-400",
  orange: "bg-orange-400",
  yellow: "bg-yellow-400",
  green: "bg-emerald-400",
  blue: "bg-blue-400",
  purple: "bg-violet-400",
};

/** 标签显示名的长度上限，与后端 `TAG_NAME_MAX_LEN` 一致。 */
export const TAG_NAME_MAX_LEN = 24;

/** 类型守卫：服务端字段或手改的 meta 可能是任意值，渲染前必须先过这一关。 */
export function isValidTagId(value: unknown): value is TagId {
  return typeof value === "string" && (TAG_IDS as readonly string[]).includes(value);
}

/**
 * 把服务端给的**部分覆盖**合成为完整的 6 键映射：未提及或空白的键回退默认名。
 * 未知键被丢弃（不透传脏数据到渲染层）。
 */
export function resolveTagNames(
  overrides?: Record<string, string> | null,
): Record<TagId, string> {
  const resolved: Record<TagId, string> = { ...DEFAULT_TAG_NAMES };
  if (!overrides) return resolved;
  for (const id of TAG_IDS) {
    const value = overrides[id];
    if (typeof value === "string" && value.trim() !== "") {
      resolved[id] = value.trim();
    }
  }
  return resolved;
}

/** 取某颜色的显示名；名字表缺键时回退默认名（不抛）。 */
export function tagLabel(id: TagId, names: Record<TagId, string>): string {
  const name = names[id];
  return typeof name === "string" && name.trim() !== "" ? name : DEFAULT_TAG_NAMES[id];
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `export PATH="/opt/homebrew/bin:$PATH" && cd desktop && npx vitest run src/lib/threadTags.test.ts`

Expected: PASS（7 个用例）。

- [ ] **Step 5: 提交**

```bash
git add desktop/src/lib/threadTags.ts desktop/src/lib/threadTags.test.ts
git commit -m "feat(desktop): 标签颜色常量与显示名解析纯函数"
```

---

### Task 7: 前端 — 协议类型与搜索匹配标签名

**Files:**
- Modify: `desktop/src/lib/protocol.ts`（`ThreadSummary`、`Notification` 联合）
- Modify: `desktop/src/lib/threadSearch.ts`（`haystack` 与 `filterThreadTree`）
- Test: `desktop/src/lib/threadSearch.test.ts`

**Interfaces:**
- Consumes: Task 6 的 `isValidTagId` / `tagLabel` / `DEFAULT_TAG_NAMES` / `TagId`
- Produces:
  - `ThreadSummary.tag?: string | null`
  - `filterThreadTree(groups, pinned, query, tagNames?: Record<TagId, string>)`
  - `Notification` 增加 `thread/tagChanged` 与 `ui/tagNames/updated` 两臂

- [ ] **Step 1: 写失败的测试**

在 `desktop/src/lib/threadSearch.test.ts` 末尾（`describe` 内）加入：

```ts
  it("matches a thread by its tag's default name", () => {
    const withTag: WorkspaceGroup[] = [
      {
        workspace: "/work/projA",
        exists: true,
        threads: [
          { thread_id: "1", cwd: "/work/projA", model: "m", created_at: 0, updated_at: 0, title: "alpha", tag: "red" },
          { thread_id: "2", cwd: "/work/projA", model: "m", created_at: 0, updated_at: 0, title: "beta", tag: "blue" },
        ],
      },
    ];
    // 默认名「红」命中，且只命中打了 red 的那条。
    expect(idsOf(filterThreadTree(withTag, [], "红"))).toEqual(["1"]);
  });

  it("matches a thread by a customized tag name, not the default", () => {
    const withTag: WorkspaceGroup[] = [
      {
        workspace: "/work/projA",
        exists: true,
        threads: [
          { thread_id: "1", cwd: "/work/projA", model: "m", created_at: 0, updated_at: 0, title: "alpha", tag: "red" },
        ],
      },
    ];
    const names = resolveTagNames({ red: "紧急" });
    expect(idsOf(filterThreadTree(withTag, [], "紧急", names))).toEqual(["1"]);
    // 改过名后旧名不再命中。
    expect(filterThreadTree(withTag, [], "红", names).empty).toBe(true);
  });

  it("ignores an invalid tag when searching", () => {
    const withTag: WorkspaceGroup[] = [
      {
        workspace: "/work/projA",
        exists: true,
        threads: [
          { thread_id: "1", cwd: "/work/projA", model: "m", created_at: 0, updated_at: 0, title: "alpha", tag: "chartreuse" },
        ],
      },
    ];
    // 脏 tag 不并入匹配文本，也不崩；按标题仍能命中。
    expect(idsOf(filterThreadTree(withTag, [], "alpha"))).toEqual(["1"]);
    expect(filterThreadTree(withTag, [], "chartreuse").empty).toBe(true);
  });
```

在该文件顶部 import 补上 `resolveTagNames`：

```ts
import { resolveTagNames } from "./threadTags";
```

- [ ] **Step 2: 跑测试确认失败**

Run: `export PATH="/opt/homebrew/bin:$PATH" && cd desktop && npx vitest run src/lib/threadSearch.test.ts`

Expected: FAIL（"红" 搜不到，因为 `haystack` 还没并入标签名）。

- [ ] **Step 3: 加协议字段**

在 `desktop/src/lib/protocol.ts` 的 `ThreadSummary` 内，`model_ref` 之后加入：

```ts
  /**
   * 颜色标签 id（`threadTags.ts` 的 `TAG_IDS` 之一）。缺省 / `null` = 无标签。
   * 旧服务端不发送此字段；读取时先过 `isValidTagId` 再渲染，脏值按无标签处理。
   */
  tag?: string | null;
```

在 `Notification` 联合类型里，`thread/titleChanged` 一臂之后加入两臂：

```ts
  /**
   * 该会话的颜色标签已变。与 `thread/titleChanged` 同族：元数据变更、列表层恒推、
   * 直接携带权威值（`tag: null` = 已清除），客户端就地落笔。
   */
  | { method: "thread/tagChanged"; params: { thread_id: string; tag: string | null } }
  /**
   * 标签显示名（颜色 id → 名称）已变。**全局**帧，携带用户覆盖的权威映射
   * （可能只含被改的键），客户端与自己的默认名表合并后再渲染。
   */
  | { method: "ui/tagNames/updated"; params: { tag_names: Record<string, string> } }
```

- [ ] **Step 4: 搜索并入标签名**

修改 `desktop/src/lib/threadSearch.ts`：import 与 `haystack`、`filterThreadTree`。

```ts
import { DEFAULT_TAG_NAMES, isValidTagId, tagLabel, type TagId } from "./threadTags";
```

把 `haystack` 整体替换为：

```ts
/** 一条会话在界面上可被搜索到的全部文本：渲染标题 + 标签名 + 所属目录名与完整路径。 */
function haystack(
  thread: ThreadSummary,
  workspaceOfThread: string,
  tagNames: Record<TagId, string>,
): string {
  // 标题渲染的就是这个回退值（ThreadSidebar 的 `t.title ?? "(untitled)"`），
  // 所以搜「untitled」能命中无标题的会话。
  // 标签名并入匹配文本：搜「紧急」即筛出所有打了该标签的会话——这是「不做筛选
  // UI」方案下几乎零成本的收益。非法 tag 不并入（脏数据不该被搜到）。
  const tagText = isValidTagId(thread.tag) ? tagLabel(thread.tag, tagNames) : "";
  return [
    thread.title ?? "(untitled)",
    tagText,
    basename(workspaceOfThread),
    workspaceOfThread,
  ]
    .join("\n")
    .toLowerCase();
}
```

把 `filterThreadTree` 的签名与两处 `haystack` 调用改为：

```ts
export function filterThreadTree(
  groups: WorkspaceGroup[],
  pinned: ThreadSummary[],
  query: string,
  tagNames: Record<TagId, string> = DEFAULT_TAG_NAMES,
): ThreadSearchResult {
```

```ts
    const threads = g.threads.filter((t) =>
      tokens.every((tok) => haystack(t, g.workspace, tagNames).includes(tok)),
    );
```

```ts
  const pinned2 = pinned.filter((t) =>
    tokens.every((tok) => haystack(t, t.cwd, tagNames).includes(tok)),
  );
```

- [ ] **Step 5: 跑测试确认通过**

Run: `export PATH="/opt/homebrew/bin:$PATH" && cd desktop && npx vitest run src/lib/threadSearch.test.ts`

Expected: PASS（含既有搜索用例）。

- [ ] **Step 6: 提交**

```bash
git add desktop/src/lib/protocol.ts desktop/src/lib/threadSearch.ts desktop/src/lib/threadSearch.test.ts
git commit -m "feat(desktop): ThreadSummary 加 tag 字段，搜索并入标签名"
```

---

### Task 8: 前端 — 侧栏色点、右键菜单与标签菜单区

**Files:**
- Modify: `desktop/src/components/ThreadSidebar.tsx`
- Test: `desktop/src/components/ThreadSidebar.test.tsx`

**Interfaces:**
- Consumes: Task 6 的 `TAG_IDS` / `TAG_DOT_CLASS` / `isValidTagId` / `tagLabel` / `TagId`；Task 7 的 `ThreadSummary.tag` 与 `filterThreadTree` 新签名
- Produces: `ThreadSidebar` 新增 props `tagNames: Record<TagId, string>`、`onSetTag: (id: string, tag: TagId | null) => void`

- [ ] **Step 1: 写失败的测试**

在 `desktop/src/components/ThreadSidebar.test.tsx` 中：

(a) `thread()` 辅助函数支持 tag——改为可选第三参之后追加第四参：

```ts
function thread(id: string, title: string, cwd: string, tag?: string): ThreadSummary {
  return { thread_id: id, cwd, model: "m", created_at: 0, updated_at: 0, title, tag };
}
```

(b) `sidebarProps` 的默认值里加入 `tagNames: DEFAULT_TAG_NAMES,` 与 `onSetTag: vi.fn(),`，并在文件顶部加：

```ts
import { DEFAULT_TAG_NAMES, resolveTagNames } from "../lib/threadTags";
```

(c) 新增测试（放在 `describe("ThreadSidebar", …)` 内）：

```ts
  it("renders a colour dot for a tagged thread and nothing for an untagged one", () => {
    const tagged: WorkspaceGroup[] = [
      {
        workspace: "/work/projA",
        exists: true,
        threads: [thread("1", "alpha-thread", "/work/projA", "red")],
      },
      {
        workspace: "/work/projB",
        exists: true,
        threads: [thread("2", "beta-thread", "/work/projB")],
      },
    ];
    const { container } = renderSidebar({ groups: tagged, tagNames: resolveTagNames({ red: "紧急" }) });
    const dots = container.querySelectorAll("[data-thread-tag]");
    expect(dots.length).toBe(1);
    expect(dots[0].getAttribute("data-thread-tag")).toBe("red");
    expect(dots[0].getAttribute("aria-label")).toBe("紧急");
    // 无标签的行不渲染任何色点节点。
    const row2 = container.querySelector('[data-thread-row="2"]')!;
    expect(row2.querySelector("[data-thread-tag]")).toBeNull();
    // 标题仍然渲染（色点不能顶掉标题）。
    expect(screen.getByText("beta-thread")).toBeTruthy();
  });

  it("does not render a dot for an invalid tag id", () => {
    const dirty: WorkspaceGroup[] = [
      {
        workspace: "/work/projA",
        exists: true,
        threads: [thread("1", "alpha-thread", "/work/projA", "chartreuse")],
      },
    ];
    const { container } = renderSidebar({ groups: dirty });
    expect(container.querySelector("[data-thread-tag]")).toBeNull();
  });

  it("opens the thread menu on right-click and applies a tag", () => {
    const onSetTag = vi.fn();
    const { container } = renderSidebar({ onSetTag });
    const row = container.querySelector<HTMLElement>('[data-thread-row="1"]')!;
    fireEvent.contextMenu(row);
    const menu = container.querySelector("[data-thread-menu]");
    expect(menu).toBeTruthy();
    fireEvent.click(screen.getByRole("menuitemradio", { name: "红" }));
    expect(onSetTag).toHaveBeenCalledWith("1", "red");
  });

  it("clears the tag from the menu", () => {
    const onSetTag = vi.fn();
    const { container } = renderSidebar({ onSetTag });
    fireEvent.contextMenu(container.querySelector<HTMLElement>('[data-thread-row="1"]')!);
    fireEvent.click(screen.getByRole("menuitemradio", { name: "无标签" }));
    expect(onSetTag).toHaveBeenCalledWith("1", null);
  });

  it("marks the current tag as checked", () => {
    const tagged: WorkspaceGroup[] = [
      {
        workspace: "/work/projA",
        exists: true,
        threads: [thread("1", "alpha-thread", "/work/projA", "blue")],
      },
    ];
    const { container } = renderSidebar({ groups: tagged });
    fireEvent.contextMenu(container.querySelector<HTMLElement>('[data-thread-row="1"]')!);
    expect(screen.getByRole("menuitemradio", { name: "蓝" }).getAttribute("aria-checked")).toBe("true");
    expect(screen.getByRole("menuitemradio", { name: "红" }).getAttribute("aria-checked")).toBe("false");
    expect(screen.getByRole("menuitemradio", { name: "无标签" }).getAttribute("aria-checked")).toBe("false");
  });

  it("still opens the same menu on long-press in mobile", async () => {
    vi.useFakeTimers();
    try {
      const { container } = renderSidebar({ isMobile: true });
      const row = container.querySelector<HTMLElement>('[data-thread-row="1"]')!;
      fireEvent.pointerDown(row, { pointerId: 1, button: 0, clientX: 0, clientY: 0 });
      act(() => {
        vi.advanceTimersByTime(LONG_PRESS_MS + 10);
      });
      expect(container.querySelector("[data-thread-menu]")).toBeTruthy();
      // 同一份菜单：标签项也在。
      expect(screen.getByRole("menuitemradio", { name: "红" })).toBeTruthy();
    } finally {
      vi.useRealTimers();
    }
  });
```

- [ ] **Step 2: 跑测试确认失败**

Run: `export PATH="/opt/homebrew/bin:$PATH" && cd desktop && npx vitest run src/components/ThreadSidebar.test.tsx`

Expected: FAIL（找不到 `menuitemradio`、无 `data-thread-tag`）。

- [ ] **Step 3: 加 props 与 import**

在 `ThreadSidebar.tsx` 顶部 import 区加入：

```ts
import { TAG_DOT_CLASS, TAG_IDS, isValidTagId, tagLabel, type TagId } from "../lib/threadTags";
```

在解构参数里（`onReorderPinned,` 之后）加入 `onSetTag,` 与 `tagNames,`；在类型声明里（`onReorderPinned` 的类型之后）加入：

```ts
  /** 给会话打 / 清颜色标签（`TagId` 或 null=清除）。 */
  onSetTag: (id: string, tag: TagId | null) => void;
  /** 已解析的标签显示名（6 键齐全，由 App 经 `resolveTagNames` 得到）。 */
  tagNames: Record<TagId, string>;
```

- [ ] **Step 4: 搜索调用传入标签名**

把 `const filtered = filterThreadTree(groups, pinned, query);` 改为：

```ts
  const filtered = filterThreadTree(groups, pinned, query, tagNames);
```

- [ ] **Step 5: 行内色点 + 右键 + 键盘**

在 `renderThread` 里，`<StatusBadge status={st} stale={statusStale} />` 之后插入色点：

```tsx
                {isValidTagId(t.tag) && (
                  <span
                    data-thread-tag={t.tag}
                    aria-label={tagLabel(t.tag, tagNames)}
                    title={tagLabel(t.tag, tagNames)}
                    className={`size-2 shrink-0 rounded-full ${TAG_DOT_CLASS[t.tag]}`}
                  />
                )}
```

在会话行那个最外层 `<div key={t.thread_id} …>` 上补右键与键盘（紧跟既有的 `onClick` 之后）：

```tsx
        onContextMenu={(e) => {
          // 桌面端打标签的入口：右键会话行打开与手机长按**同一份**菜单。
          e.preventDefault();
          if (editingId === t.thread_id) return;
          setNewMenuOpen(false);
          setMenu({ kind: "thread", id: t.thread_id });
        }}
        tabIndex={0}
        aria-haspopup="menu"
        aria-expanded={menu?.kind === "thread" && menu.id === t.thread_id}
        onKeyDown={(e) => {
          // 键盘等价于右键：Enter/Space 开关本行的菜单。
          if (e.target !== e.currentTarget) return;
          if (e.key !== "Enter" && e.key !== " ") return;
          e.preventDefault();
          setNewMenuOpen(false);
          setMenu((cur) =>
            cur?.kind === "thread" && cur.id === t.thread_id
              ? null
              : { kind: "thread", id: t.thread_id },
          );
        }}
```

- [ ] **Step 6: 菜单加标签区**

在会话菜单（`menu?.kind === "thread" && menu.id === t.thread_id` 那个 `<div role="menu" data-thread-menu>` 内）的**最前面**插入标签区：

```tsx
              <div
                role="group"
                aria-label="标签"
                className="flex items-center gap-1.5 border-b border-line px-2 py-1.5"
              >
                {TAG_IDS.map((id) => {
                  const active = t.tag === id;
                  const name = tagLabel(id, tagNames);
                  return (
                    <button
                      key={id}
                      type="button"
                      role="menuitemradio"
                      aria-checked={active}
                      aria-label={name}
                      title={name}
                      onClick={() => {
                        closeMenus();
                        onSetTag(t.thread_id, id);
                      }}
                      className={`size-3.5 rounded-full ${TAG_DOT_CLASS[id]} ${
                        active ? "ring-2 ring-fg ring-offset-1 ring-offset-raised" : ""
                      }`}
                    />
                  );
                })}
                <button
                  type="button"
                  role="menuitemradio"
                  aria-checked={!isValidTagId(t.tag)}
                  onClick={() => {
                    closeMenus();
                    onSetTag(t.thread_id, null);
                  }}
                  className="ml-auto px-1 text-xs whitespace-nowrap text-fg-muted hover:text-fg"
                >
                  无标签
                </button>
              </div>
```

- [ ] **Step 7: 收紧既有的元素选择器（消除与新行属性的巧合依赖）**

会话行现在也带 `tabIndex` 与 `aria-haspopup="menu"`，这会让既有测试里**按这些
属性定位分组头**的选择器变成「靠文档顺序侥幸命中」。分组头本来就有稳定的
`data-ws-header` 属性，改用它。

在 `desktop/src/components/ThreadSidebar.test.tsx` 中，把两处

```ts
    const header = container.querySelectorAll<HTMLElement>("div[tabindex]")[0];
```

改为

```ts
    const header = container.querySelector<HTMLElement>("[data-ws-header]")!;
```

在 `desktop/src/App.test.tsx` 中，把 `openBoardFor` 里的

```ts
  const header = document.querySelector<HTMLElement>('div[tabindex][aria-haspopup="menu"]')!;
```

改为

```ts
  const header = document.querySelector<HTMLElement>("[data-ws-header]")!;
```

（`ThreadSidebar.test.tsx` 里 `title.closest("div[tabindex]")` 那处**不用改**：
它从组头内部的标题元素向上找，与文档顺序无关。）

- [ ] **Step 8: 跑测试确认通过**

Run: `export PATH="/opt/homebrew/bin:$PATH" && cd desktop && npx vitest run src/components/ThreadSidebar.test.tsx`

Expected: PASS（含既有侧栏用例）。

- [ ] **Step 9: 跑全量前端测试确认无回归**

Run: `export PATH="/opt/homebrew/bin:$PATH" && cd desktop && npm test`

Expected: PASS——含 `App.test.tsx`（含刚改过选择器的 `openBoardFor`）。

- [ ] **Step 10: 提交**

```bash
git add desktop/src/components/ThreadSidebar.tsx desktop/src/components/ThreadSidebar.test.tsx desktop/src/App.test.tsx
git commit -m "feat(desktop): 侧栏色点、右键菜单与标签菜单区"
```

---

### Task 9: 前端 — App 接线（状态、通知、RPC）

**Files:**
- Modify: `desktop/src/lib/threadListUpdate.ts`（新增 `applyTagChange`）
- Test: `desktop/src/lib/threadListUpdate.test.ts`
- Modify: `desktop/src/App.tsx`
- Test: `desktop/src/App.test.tsx`

**Interfaces:**
- Consumes: Task 6 的 `resolveTagNames` / `DEFAULT_TAG_NAMES`；Task 7 的通知类型；Task 8 的 `ThreadSidebar` 新 props
- Produces:
  - `applyTagChange(groups, pinned, threadId, tag: string | null)` 纯函数
  - App 内部：`tagNames` 状态、`onSetTag`、两条通知的处理

- [ ] **Step 1: 写失败的测试（纯函数）**

在 `desktop/src/lib/threadListUpdate.test.ts` 末尾加入：

```ts
describe("applyTagChange", () => {
  const groups: WorkspaceGroup[] = [
    {
      workspace: "/w",
      exists: true,
      threads: [
        { thread_id: "t1", cwd: "/w", model: "m", created_at: 0, updated_at: 0, title: "one" },
        { thread_id: "t2", cwd: "/w", model: "m", created_at: 0, updated_at: 0, title: "two" },
      ],
    },
  ];

  it("writes the tag into the matching thread only", () => {
    const next = applyTagChange(groups, [], "t1", "red");
    expect(next.groups[0].threads[0].tag).toBe("red");
    expect(next.groups[0].threads[1].tag).toBeUndefined();
  });

  it("clears the tag with null", () => {
    const tagged = applyTagChange(groups, [], "t1", "red");
    const cleared = applyTagChange(tagged.groups, [], "t1", null);
    expect(cleared.groups[0].threads[0].tag).toBeNull();
  });

  it("updates both the group copy and the pinned copy", () => {
    const pinned = [{ ...groups[0].threads[0], pinned: true }];
    const next = applyTagChange(groups, pinned, "t1", "blue");
    expect(next.groups[0].threads[0].tag).toBe("blue");
    expect(next.pinned[0].tag).toBe("blue");
  });

  it("is a no-op for an unknown thread", () => {
    const next = applyTagChange(groups, [], "nope", "red");
    expect(next.groups).toBe(groups);
  });
});
```

在文件顶部 import 补上 `applyTagChange` 与 `WorkspaceGroup` 类型（若未导入）。

- [ ] **Step 2: 跑测试确认失败**

Run: `export PATH="/opt/homebrew/bin:$PATH" && cd desktop && npx vitest run src/lib/threadListUpdate.test.ts`

Expected: FAIL，报 `applyTagChange is not a function`。

- [ ] **Step 3: 实现 `applyTagChange`**

在 `threadListUpdate.ts` 末尾追加：

```ts
/** 把一条 thread 的颜色标签改写进分组与置顶两份列表（`null` = 清除）。 */
export function applyTagChange(
  groups: WorkspaceGroup[],
  pinned: ThreadSummary[],
  threadId: string,
  tag: string | null,
): { groups: WorkspaceGroup[]; pinned: ThreadSummary[] } {
  const hit =
    groups.some((g) => g.threads.some((t) => t.thread_id === threadId)) ||
    pinned.some((t) => t.thread_id === threadId);
  if (!hit) return { groups, pinned };

  // 置顶项同时存在于分组里（服务端为向后兼容不摘除），故两份都要更新，
  // 与 applyTitleChange 同理。
  const retag = (t: ThreadSummary): ThreadSummary =>
    t.thread_id === threadId ? { ...t, tag } : t;

  return {
    groups: groups.map((g) =>
      g.threads.some((t) => t.thread_id === threadId)
        ? { ...g, threads: g.threads.map(retag) }
        : g,
    ),
    pinned: pinned.some((t) => t.thread_id === threadId) ? pinned.map(retag) : pinned,
  };
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `export PATH="/opt/homebrew/bin:$PATH" && cd desktop && npx vitest run src/lib/threadListUpdate.test.ts`

Expected: PASS。

- [ ] **Step 5: 写失败的测试（App 通知路径）**

在 `App.test.tsx` 的 `describe` 内（`reflects a thread/titleChanged…` 之后）加入：

```tsx
  it("reflects a thread/tagChanged in the sidebar without re-listing", async () => {
    state.threads = [
      { thread_id: "t1", title: "one", permission_mode: "normal" },
      { thread_id: "t2", title: "two", permission_mode: "normal" },
    ];
    render(<App />);
    await settle();
    await waitFor(() => expect(screen.getByText("one")).toBeTruthy());
    const listCallsBefore = clients[0].requests.filter((r) => r.method === "thread/listAll").length;

    notify("thread/tagChanged", { thread_id: "t1", tag: "red" });
    await waitFor(() =>
      expect(document.querySelector('[data-thread-tag="red"]')).toBeTruthy(),
    );
    // 就地把权威值落笔，不该为此再拉一次全量 thread/listAll。
    expect(clients[0].requests.filter((r) => r.method === "thread/listAll").length).toBe(
      listCallsBefore,
    );
  });

  it("reflects ui/tagNames/updated in the existing dot's label", async () => {
    state.threads = [{ thread_id: "t1", title: "one", permission_mode: "normal" }];
    render(<App />);
    await settle();
    await waitFor(() => expect(screen.getByText("one")).toBeTruthy());

    notify("thread/tagChanged", { thread_id: "t1", tag: "red" });
    await waitFor(() => expect(document.querySelector('[data-thread-tag="red"]')).toBeTruthy());

    notify("ui/tagNames/updated", { tag_names: { red: "紧急" } });
    await waitFor(() =>
      expect(
        document.querySelector('[data-thread-tag="red"]')?.getAttribute("aria-label"),
      ).toBe("紧急"),
    );
  });
```

- [ ] **Step 6: 跑测试确认失败**

Run: `export PATH="/opt/homebrew/bin:$PATH" && cd desktop && npx vitest run src/App.test.tsx -t "tagChanged"`

Expected: FAIL（无 `data-thread-tag` 节点）。

- [ ] **Step 7: App 里加状态与接线**

在 `App.tsx` import 区加入：

```ts
import { DEFAULT_TAG_NAMES, resolveTagNames, type TagId } from "./lib/threadTags";
import { applyTagChange, applyTitleChange } from "./lib/threadListUpdate";
```

（若 `applyTitleChange` 已在同一行导入，把 `applyTagChange` 并入该 import。）

在 `const [pinned, setPinned] = …` 附近加状态：

```ts
  /** 标签显示名（6 键齐全）。默认名来自 threadTags.ts，服务端只给覆盖项。 */
  const [tagNames, setTagNames] = useState<Record<TagId, string>>(DEFAULT_TAG_NAMES);
```

在握手序列里，`ui/settings/read` 的泛型参数中加入 `tag_names?: unknown;`，并在读取 theme/watchman 的同一段之后加：

```ts
        // 标签显示名：服务端只回用户覆盖项，与默认名合并成完整 6 键。
        setTagNames(
          resolveTagNames(
            settings.tag_names && typeof settings.tag_names === "object"
              ? (settings.tag_names as Record<string, string>)
              : null,
          ),
        );
```

在 `notify` 的监听闭包里，`thread/titleChanged` 分支之后加两个分支：

```ts
        if (n.method === "thread/tagChanged") {
          // 服务端推来的权威标签（null = 已清除）。就地落笔即可，不必重拉列表。
          const next = applyTagChange(
            groupsRef.current,
            pinnedRef.current,
            n.params.thread_id,
            n.params.tag,
          );
          setGroups(next.groups);
          setPinned(next.pinned);
          force((v) => v + 1);
          return;
        }
        if (n.method === "ui/tagNames/updated") {
          // 标签显示名变更（全局帧）：合成完整映射后重渲染，色点 title / 菜单
          // aria-label / 搜索匹配全部随之更新。
          setTagNames(resolveTagNames(n.params.tag_names));
          force((v) => v + 1);
          return;
        }
```

在 `onTogglePin` 附近加操作函数：

```ts
  /** 设置 / 清除某会话的颜色标签；成功后重拉列表，以服务端为权威。 */
  const onSetTag = async (id: string, tag: TagId | null) => {
    const c = clientRef.current;
    if (!c) return;
    try {
      await c.request("thread/setTag", { threadId: id, tag });
      await refreshThreads();
      force((v) => v + 1);
    } catch (e) {
      setCurrentError(formatError(e));
      // 失败也要重拉：本地不做乐观更新，UI 不能停在「看起来打上了」的状态。
      await refreshThreads();
      force((v) => v + 1);
    }
  };
```

把两个 props 传给侧栏（`<ThreadSidebar … />` 里）：

```tsx
          onSetTag={onSetTag}
          tagNames={tagNames}
```

- [ ] **Step 8: 跑测试确认通过**

Run: `export PATH="/opt/homebrew/bin:$PATH" && cd desktop && npx vitest run src/App.test.tsx`

Expected: PASS（含既有 App 用例）。

- [ ] **Step 9: 提交**

```bash
git add desktop/src/lib/threadListUpdate.ts desktop/src/lib/threadListUpdate.test.ts desktop/src/App.tsx desktop/src/App.test.tsx
git commit -m "feat(desktop): App 接线标签状态、通知与 setTag RPC"
```

---

### Task 10: 前端 — 设置页「标签」Tab

**Files:**
- Create: `desktop/src/components/SettingsTagsTab.tsx`
- Test: `desktop/src/components/SettingsTagsTab.test.tsx`
- Modify: `desktop/src/components/SettingsDialog.tsx`
- Test: `desktop/src/components/SettingsDialog.test.tsx`

**Interfaces:**
- Consumes: Task 6 的 `TAG_IDS` / `TAG_DOT_CLASS` / `TAG_NAME_MAX_LEN` / `TagId`；Task 9 的 `tagNames` 状态 与 `ui/settings/write` 协议
- Produces:
  - `SettingsTagsTab({ tagNames, onSave })`，`onSave: (tagId: TagId, name: string) => Promise<void> | void`
  - `SettingsDialog` 新 props `tagNames?: Record<TagId, string>`、`saveTagName?: (tagId: TagId, name: string) => Promise<void> | void`，新增 Tab `{ id: "tags", label: "标签" }`

- [ ] **Step 1: 写失败的测试**

创建 `desktop/src/components/SettingsTagsTab.test.tsx`：

```tsx
/** @vitest-environment jsdom */
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { SettingsTagsTab } from "./SettingsTagsTab";
import { DEFAULT_TAG_NAMES, resolveTagNames } from "../lib/threadTags";

afterEach(cleanup);

/**
 * 按颜色 id 取那一行的输入框。
 *
 * 刻意**不**用 `getByLabelText("红")`：输入框的 `aria-label` 是**当前显示名**，
 * 一旦该颜色被改过名（如 red → 紧急），按默认名就再也查不到它了。用稳定的
 * `data-tag-name-input` 才与名字解耦。
 */
const inputFor = (id: string) =>
  document.querySelector<HTMLInputElement>(`[data-tag-name-input="${id}"]`)!;

describe("SettingsTagsTab", () => {
  it("renders one row per colour with its current name", () => {
    render(
      <SettingsTagsTab tagNames={resolveTagNames({ red: "紧急" })} onSave={vi.fn()} />,
    );
    expect(document.querySelectorAll("[data-tag-name-input]").length).toBe(6);
    expect(inputFor("red").value).toBe("紧急");
    // 未覆盖的颜色显示默认名。
    expect(inputFor("blue").value).toBe(DEFAULT_TAG_NAMES.blue);
  });

  it("saves on blur with the edited name", () => {
    const onSave = vi.fn();
    render(<SettingsTagsTab tagNames={DEFAULT_TAG_NAMES} onSave={onSave} />);
    const input = inputFor("red");
    fireEvent.change(input, { target: { value: "紧急" } });
    fireEvent.blur(input);
    expect(onSave).toHaveBeenCalledWith("red", "紧急");
  });

  it("saves on Enter", () => {
    const onSave = vi.fn();
    render(<SettingsTagsTab tagNames={DEFAULT_TAG_NAMES} onSave={onSave} />);
    const input = inputFor("blue");
    fireEvent.change(input, { target: { value: "参考" } });
    fireEvent.keyDown(input, { key: "Enter" });
    expect(onSave).toHaveBeenCalledWith("blue", "参考");
  });

  it("does not save a blank name and shows an inline error", () => {
    const onSave = vi.fn();
    render(<SettingsTagsTab tagNames={DEFAULT_TAG_NAMES} onSave={onSave} />);
    const input = inputFor("green");
    fireEvent.change(input, { target: { value: "   " } });
    fireEvent.blur(input);
    expect(onSave).not.toHaveBeenCalled();
    expect(screen.getByRole("alert")).toBeTruthy();
  });

  it("does not save on the Enter that confirms an IME candidate", () => {
    const onSave = vi.fn();
    render(<SettingsTagsTab tagNames={DEFAULT_TAG_NAMES} onSave={onSave} />);
    const input = inputFor("purple");
    fireEvent.change(input, { target: { value: "重要" } });
    fireEvent.compositionStart(input);
    fireEvent.keyDown(input, { key: "Enter" });
    expect(onSave).not.toHaveBeenCalled();
    fireEvent.compositionEnd(input);
    fireEvent.keyDown(input, { key: "Enter" });
    expect(onSave).toHaveBeenCalledWith("purple", "重要");
  });

  it("rejects a name longer than the max length", () => {
    const onSave = vi.fn();
    render(<SettingsTagsTab tagNames={DEFAULT_TAG_NAMES} onSave={onSave} />);
    const input = inputFor("orange");
    fireEvent.change(input, { target: { value: "长".repeat(25) } });
    fireEvent.blur(input);
    expect(onSave).not.toHaveBeenCalled();
    expect(screen.getByRole("alert")).toBeTruthy();
  });
});
```

- [ ] **Step 2: 跑测试确认失败**

Run: `export PATH="/opt/homebrew/bin:$PATH" && cd desktop && npx vitest run src/components/SettingsTagsTab.test.tsx`

Expected: FAIL，报找不到模块 `./SettingsTagsTab`。

- [ ] **Step 3: 实现 `SettingsTagsTab`**

创建 `desktop/src/components/SettingsTagsTab.tsx`：

```tsx
import { useState } from "react";
import { isImeEnter, useImeGuard } from "../lib/imeEnter";
import {
  DEFAULT_TAG_NAMES,
  TAG_DOT_CLASS,
  TAG_IDS,
  TAG_NAME_MAX_LEN,
  type TagId,
} from "../lib/threadTags";

/** 一行的编辑态：受控值 + 上一次已保存值（用于失败回填与「是否已改动」判定）。 */
interface RowState {
  value: string;
  error: string | null;
}

/**
 * 设置里的「标签」Tab：改 6 个颜色标签的显示名。
 *
 * 名字是**全局**的（所有会话共用同一套定义），故这里改的是宿主级偏好，而非某条
 * 会话的属性。只改名、不能增删颜色、不能改色值——那是本功能的非目标。
 */
export function SettingsTagsTab({
  tagNames,
  onSave,
}: {
  tagNames: Record<TagId, string>;
  onSave: (tagId: TagId, name: string) => Promise<void> | void;
}) {
  const ime = useImeGuard();
  const [rows, setRows] = useState<Record<TagId, RowState>>(() => {
    const initial = {} as Record<TagId, RowState>;
    for (const id of TAG_IDS) initial[id] = { value: tagNames[id], error: null };
    return initial;
  });

  const commit = (id: TagId) => {
    const raw = rows[id].value;
    const name = raw.trim();
    if (name === "") {
      setRows((prev) => ({ ...prev, [id]: { ...prev[id], error: "名字不能为空" } }));
      return;
    }
    if (name.length > TAG_NAME_MAX_LEN) {
      setRows((prev) => ({
        ...prev,
        [id]: { ...prev[id], error: `名字最多 ${TAG_NAME_MAX_LEN} 个字符` },
      }));
      return;
    }
    // 未改动就直接清掉可能残留的错误，不发请求。
    if (name === tagNames[id]) {
      setRows((prev) => ({ ...prev, [id]: { value: name, error: null } }));
      return;
    }
    setRows((prev) => ({ ...prev, [id]: { value: name, error: null } }));
    void onSave(id, name);
  };

  return (
    <section className="p-5">
      <h2 className="text-sm font-medium text-fg">标签</h2>
      <p className="mt-2 text-xs text-fg-subtle">
        6 个固定颜色，名字由你定义；改名字对所有会话立即生效，已打的标签颜色不变。
      </p>
      <ul className="mt-4 space-y-2">
        {TAG_IDS.map((id) => (
          <li key={id} className="flex items-center gap-3">
            <span
              aria-hidden="true"
              className={`size-3.5 shrink-0 rounded-full ${TAG_DOT_CLASS[id]}`}
            />
            <input
              aria-label={`${DEFAULT_TAG_NAMES[id]}标签名`}
              data-tag-name-input={id}
              value={rows[id].value}
              maxLength={TAG_NAME_MAX_LEN}
              onChange={(e) =>
                setRows((prev) => ({
                  ...prev,
                  [id]: { value: e.target.value, error: prev[id].error },
                }))
              }
              onCompositionStart={ime.onCompositionStart}
              onCompositionEnd={ime.onCompositionEnd}
              onBlur={() => {
                ime.resetComposition();
                commit(id);
              }}
              onKeyDown={(e) => {
                // 中文输入法确认候选的 Enter 不能提交（否则会吞掉半个词）。
                if (e.key === "Enter" && !isImeEnter(e, ime.composing.current)) commit(id);
              }}
              className="w-48 rounded border border-line bg-surface px-2 py-1 text-sm text-fg outline-none focus:border-line-strong"
            />
            {rows[id].error && (
              <span role="alert" className="text-xs text-red-400">
                {rows[id].error}
              </span>
            )}
          </li>
        ))}
      </ul>
    </section>
  );
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `export PATH="/opt/homebrew/bin:$PATH" && cd desktop && npx vitest run src/components/SettingsTagsTab.test.tsx`

Expected: PASS（6 个用例）。

- [ ] **Step 5: 在 `SettingsDialog` 里加 Tab**

在 `SettingsDialog.tsx` 的 `TABS` 表中，`models` 之后加入：

```ts
  { id: "tags", label: "标签" },
```

在 import 区加入：

```ts
import { SettingsTagsTab } from "./SettingsTagsTab";
import type { TagId } from "../lib/threadTags";
```

在参数解构里加入 `tagNames,` 与 `saveTagName,`，类型声明里加入：

```ts
  /** 已解析的标签显示名（6 键齐全）。缺省时「标签」Tab 只读回退默认名。 */
  tagNames?: Record<TagId, string>;
  /** 「标签」Tab 保存单个颜色显示名的接缝（写 ui/settings/write）。 */
  saveTagName?: (tagId: TagId, name: string) => Promise<void> | void;
```

把面板渲染的末尾 `) : ( <SettingsModelsTab … /> )` 改成多一臂：

```tsx
            ) : active === "models" ? (
              <SettingsModelsTab call={modelCall} isMobile={isMobile} />
            ) : (
              <SettingsTagsTab
                tagNames={tagNames ?? DEFAULT_TAG_NAMES}
                onSave={saveTagName ?? (() => {})}
              />
            )}
```

（`DEFAULT_TAG_NAMES` 从 `../lib/threadTags` 一并导入。）

- [ ] **Step 6: 加对话框测试**

在 `SettingsDialog.test.tsx` 内加入：

```tsx
  it("shows the 标签 tab and forwards a name edit", async () => {
    const saveTagName = vi.fn();
    render(
      <SettingsDialog
        open
        theme="dark"
        onThemeChange={() => {}}
        onClose={() => {}}
        saveTagName={saveTagName}
      />,
    );
    fireEvent.click(screen.getByRole("tab", { name: "标签" }));
    const input = document.querySelector<HTMLInputElement>('[data-tag-name-input="red"]')!;
    fireEvent.change(input, { target: { value: "紧急" } });
    fireEvent.blur(input);
    expect(saveTagName).toHaveBeenCalledWith("red", "紧急");
  });
```

- [ ] **Step 7: 跑测试确认通过**

Run: `export PATH="/opt/homebrew/bin:$PATH" && cd desktop && npx vitest run src/components/SettingsDialog.test.tsx src/components/SettingsTagsTab.test.tsx`

Expected: PASS。

- [ ] **Step 8: 把接缝从 App 接到对话框**

在 `App.tsx` 的 `<SettingsDialog … />` 上加入：

```tsx
        tagNames={tagNames}
        saveTagName={saveTagName}
```

并在 App 里实现（放在 `onSetTag` 附近）：

```ts
  /**
   * 保存单个颜色标签的显示名。
   *
   * 只提交改动的那一个键（服务端按键合并），成功后用**服务端**读回的覆盖项重建
   * 本地映射——服务端是权威，本端不自行猜测合并结果。广播随后到达，两端一致。
   */
  const saveTagName = async (tagId: TagId, name: string) => {
    const c = clientRef.current;
    if (!c) return;
    try {
      await c.request("ui/settings/write", { tag_names: { [tagId]: name } });
      force((v) => v + 1);
    } catch (e) {
      setCurrentError(formatError(e));
    }
  };
```

- [ ] **Step 9: 跑全量前端测试**

Run: `export PATH="/opt/homebrew/bin:$PATH" && cd desktop && npm test`

Expected: PASS（全绿）。

- [ ] **Step 10: 提交**

```bash
git add desktop/src/components/SettingsTagsTab.tsx desktop/src/components/SettingsTagsTab.test.tsx desktop/src/components/SettingsDialog.tsx desktop/src/components/SettingsDialog.test.tsx desktop/src/App.tsx
git commit -m "feat(desktop): 设置页新增标签改名 Tab"
```

---

### Task 11: 全量校验与文档同步

**Files:**
- Modify: `docs/project-management/desktop.md`、`docs/project-management/yi-agent-app-server.md`

**Interfaces:**
- Consumes: 前 10 个任务的全部产出
- Produces: 无新接口

- [ ] **Step 1: 后端全量测试**

Run: `ps aux | grep -v grep | grep -E "cargo|yi_agent" || true`（确认无残留进程）

然后在 `yi-agent-rs/` 下：

Run: `cargo test -p yi-agent-app-server`

Expected: PASS，无回归。

- [ ] **Step 2: fmt 与 lint**

Run: `export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH" && cd yi-agent-rs && cargo fmt --all && cargo fmt --all -- --check`

Expected: 无 diff、无报错。

- [ ] **Step 3: 前端全量测试与类型检查**

Run: `export PATH="/opt/homebrew/bin:$PATH" && cd desktop && npm test && npx tsc --noEmit`

Expected: 测试全绿、类型无错。

- [ ] **Step 4: 更新项目进度文档**

在 `docs/project-management/desktop.md` 追加一节（沿用该文件既有格式）：

```markdown
## Thread 颜色标签

- [x] 侧栏会话行行内色点（6 色，无标签不占位）
- [x] 桌面右键 / 移动长按会话行 → 菜单内选色或清除
- [x] 设置页「标签」Tab：自定义 6 个颜色的显示名（全局生效）
- [x] 标签变更经 `thread/tagChanged` 即时同步所有客户端
- [x] 标签名经 `ui/tagNames/updated` 即时同步所有客户端
- [x] 搜索并入标签名（搜「紧急」即筛出该标签的会话）
- [ ] 按标签筛选的侧栏 UI（非目标，留待后续）
```

在 `docs/project-management/yi-agent-app-server.md` 追加：

```markdown
## Thread 颜色标签（元数据扩展）

- [x] `ThreadMeta.tag: Option<String>`（`#[serde(default)]`，零迁移）
- [x] `ThreadStore::set_tag`（不刷新 `updated_at`）
- [x] RPC `thread/setTag`（`tag` 为 null 或 6 色之一，否则 -32602）
- [x] `thread_summary_json` 输出 `tag`
- [x] 通知 `thread/tagChanged`（列表层）
- [x] `preferences.json` 的 `tag_names`（逐键覆盖）+ `ui/settings/read|write` 扩展
- [x] 通知 `ui/tagNames/updated`（全局层）
```

- [ ] **Step 5: 提交**

```bash
git add docs/project-management/desktop.md docs/project-management/yi-agent-app-server.md
git commit -m "docs: 同步 Thread 颜色标签的项目进度"
```

---

### Task 12: 合并回 main

**Files:** 无

- [ ] **Step 1: 确认分支干净且测试全绿**

Run: `git status --porcelain && git log --oneline main..HEAD`

Expected: 无未提交改动；列出本分支的全部 commit。

- [ ] **Step 2: 回 main 合并**

```bash
cd <主仓库根>
git merge --no-ff feat/thread-color-tags -m "merge: Thread 颜色标签（右键打标签 + 6 色 + 可自定义名）"
```

Expected: 合并成功。若 `main` 上 `docs/superpowers/specs/2026-10-15-cot-round-trip-and-anthropic-thinking-design.md` 或
`yi-agent-rs/crates/yi-agent/src/main.rs` 的未提交改动与本次冲突：**不要**动那两个文件，先 `git stash` 再合并、合并后 `git stash pop`。

- [ ] **Step 3: 合并后再验一次**

Run: `export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH" && cd yi-agent-rs && cargo test -p yi-agent-app-server && cd ../desktop && npm test`

Expected: PASS（合并结果与分支一致）。

- [ ] **Step 4: 清理 worktree**

```bash
git worktree remove .worktrees/feat-thread-color-tags
git branch -d feat/thread-color-tags
```

Expected: 两者都成功（分支已合并，`-d` 安全删除）。

---

## 附：手工端到端验收（Step 4 后执行）

1. 启动桌面端，右键一个会话 → 选「红」→ 行内立刻出红点。
2. 改成「蓝」→ 红点变蓝点；选「无标签」→ 色点消失。
3. 重启 app → 标签仍在。
4. 手机连同一 app-server：桌面改标签，手机侧栏**无需重连**即变色；反向亦然。
5. 设置 → 标签：把「红」改成「紧急」→ 两端菜单悬停提示变「紧急」。
6. 搜「紧急」→ 只列出打了红色的会话。
7. 打标签后，该会话**不**跳到「最近更新」最前面。
