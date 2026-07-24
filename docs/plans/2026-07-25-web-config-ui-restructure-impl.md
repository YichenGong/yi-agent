# Web Config UI Restructure Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Restructure WebUI with vertical tabs, merge 3 provider groups into one "Model Provider" tab with collapsible sections, replace absolute `compact_threshold` with `context_length * ratio`.

**Architecture:** Metadata-driven — `config_meta.rs` changes cascade through API and env_file automatically. Frontend restructures to vertical tabs + collapsible sections with context-aware credential display. Config loading computes `compact_threshold` from new `model_context_length` and `compact_ratio` fields.

**Tech Stack:** Rust (axum, serde), vanilla JS/HTML/CSS

---

## Task 1: Update config_meta.rs — variable metadata

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-web/src/config_meta.rs`

**Step 1: Update ALL_VARS**

Replace the entire `ALL_VARS` static and remove the 3-group provider split. New structure has 15 vars in 3 groups:

```rust
/// 所有 15 个环境变量的元数据，按分组排列。
pub static ALL_VARS: &[VarMeta] = &[
    // === Model Provider ===
    VarMeta {
        key: "YI_AGENT_PROVIDER",
        default: Some("anthropic"),
        var_type: VarType::Select,
        group: "Model Provider",
        description: "LLM provider backend",
        options: &["anthropic", "openai"],
    },
    VarMeta {
        key: "YI_AGENT_MODEL",
        default: None,
        var_type: VarType::Text,
        group: "Model Provider",
        description: "Model identifier string",
        options: &[],
    },
    VarMeta {
        key: "YI_AGENT_MODEL_CONTEXT_LENGTH",
        default: None,
        var_type: VarType::Number,
        group: "Model Provider",
        description: "Model max context length in tokens (fallback: 200000)",
        options: &[],
    },
    VarMeta {
        key: "MODEL_API_KEY",
        default: None,
        var_type: VarType::Secret,
        group: "Model Provider",
        description: "API key (overrides provider-specific key)",
        options: &[],
    },
    VarMeta {
        key: "MODEL_API_URL",
        default: None,
        var_type: VarType::Text,
        group: "Model Provider",
        description: "API endpoint URL (overrides provider-specific URL)",
        options: &[],
    },
    VarMeta {
        key: "ANTHROPIC_API_KEY",
        default: None,
        var_type: VarType::Secret,
        group: "Model Provider",
        description: "Anthropic provider API key",
        options: &[],
    },
    VarMeta {
        key: "ANTHROPIC_BASE_URL",
        default: Some("https://api.anthropic.com"),
        var_type: VarType::Text,
        group: "Model Provider",
        description: "Anthropic API base URL",
        options: &[],
    },
    VarMeta {
        key: "OPENAI_API_KEY",
        default: None,
        var_type: VarType::Secret,
        group: "Model Provider",
        description: "OpenAI provider API key",
        options: &[],
    },
    VarMeta {
        key: "OPENAI_BASE_URL",
        default: Some("https://api.openai.com"),
        var_type: VarType::Text,
        group: "Model Provider",
        description: "OpenAI API base URL",
        options: &[],
    },
    // === Agent ===
    VarMeta {
        key: "YI_AGENT_MAX_TURNS",
        default: Some("20"),
        var_type: VarType::Number,
        group: "Agent",
        description: "Max agent turns per conversation",
        options: &[],
    },
    VarMeta {
        key: "YI_AGENT_WORKDIR",
        default: None,
        var_type: VarType::Path,
        group: "Agent",
        description: "Working directory for file tools",
        options: &[],
    },
    VarMeta {
        key: "YI_AGENT_SYSTEM_PROMPT",
        default: None,
        var_type: VarType::Text,
        group: "Agent",
        description: "Custom system prompt override",
        options: &[],
    },
    VarMeta {
        key: "YI_AGENT_COMPACT_RATIO",
        default: Some("80"),
        var_type: VarType::Number,
        group: "Agent",
        description: "Percentage of context length triggering auto-compact",
        options: &[],
    },
    VarMeta {
        key: "YI_AGENT_COMPACT_KEEP_TURNS",
        default: Some("4"),
        var_type: VarType::Number,
        group: "Agent",
        description: "Turns retained during compaction",
        options: &[],
    },
    // === Tools ===
    VarMeta {
        key: "BOCHA_API_KEY",
        default: None,
        var_type: VarType::Secret,
        group: "Tools",
        description: "Bocha web search API key",
        options: &[],
    },
];
```

**Step 2: Update tests**

Replace the test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_vars_count_is_15() {
        assert_eq!(ALL_VARS.len(), 15);
    }

    #[test]
    fn groups_count_is_3() {
        assert_eq!(groups().len(), 3);
    }

    #[test]
    fn groups_are_ordered() {
        let g = groups();
        assert_eq!(
            g,
            vec!["Model Provider", "Agent", "Tools"]
        );
    }

    #[test]
    fn find_returns_meta() {
        let m = find("YI_AGENT_PROVIDER").unwrap();
        assert_eq!(m.var_type, VarType::Select);
        assert_eq!(m.options, &["anthropic", "openai"]);
    }

    #[test]
    fn find_returns_none_for_unknown() {
        assert!(find("UNKNOWN_VAR").is_none());
    }

    #[test]
    fn select_vars_have_options() {
        for var in ALL_VARS {
            if var.var_type == VarType::Select {
                assert!(
                    !var.options.is_empty(),
                    "{} is Select but has no options",
                    var.key
                );
            }
        }
    }

    #[test]
    fn secret_vars_have_no_options() {
        for var in ALL_VARS {
            if var.var_type == VarType::Secret {
                assert!(
                    var.options.is_empty(),
                    "{} is Secret but has options",
                    var.key
                );
            }
        }
    }

    #[test]
    fn all_keys_are_unique() {
        let mut keys: Vec<&str> = ALL_VARS.iter().map(|v| v.key).collect();
        keys.sort();
        let before = keys.len();
        keys.dedup();
        assert_eq!(keys.len(), before, "duplicate keys found");
    }

    #[test]
    fn context_length_has_no_default() {
        let m = find("YI_AGENT_MODEL_CONTEXT_LENGTH").unwrap();
        assert!(m.default.is_none());
    }

    #[test]
    fn compact_ratio_default_is_80() {
        let m = find("YI_AGENT_COMPACT_RATIO").unwrap();
        assert_eq!(m.default, Some("80"));
    }
}
```

**Step 3: Run tests**

Run: `cargo test -p yi-agent-web config_meta`
Expected: 9 tests pass

**Step 4: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-web/src/config_meta.rs
git commit -m "refactor(web): merge provider groups, add context_length and compact_ratio vars"
```

---

## Task 2: Update config.rs — new fields and computed threshold

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/config.rs`

**Step 1: Update Config struct**

Remove `compact_threshold: u32` as a directly-loaded field. Add new fields. Keep `compact_threshold` as a computed field:

```rust
pub struct Config {
    pub provider: String,
    pub api_url: String,
    pub api_key: String,
    pub model: String,
    pub max_turns: u32,
    pub workdir: PathBuf,
    pub system_prompt: Option<String>,
    pub model_context_length: Option<u32>,
    pub compact_ratio: u32,
    pub compact_threshold: u32,  // computed: context_length * ratio / 100
    pub compact_keep_turns: u32,
}
```

**Step 2: Update Cli struct**

Remove `compact_threshold` arg, add two new args:

```rust
    /// Model max context length in tokens (fallback: 200000)
    #[arg(long)]
    pub model_context_length: Option<u32>,

    /// Percentage of context length triggering auto-compact (default: 80)
    #[arg(long)]
    pub compact_ratio: Option<u32>,
```

(Remove the old `--compact-threshold` arg.)

**Step 3: Update load() function**

Replace the `compact_threshold` loading block with:

```rust
    let model_context_length = cli
        .model_context_length
        .or_else(|| {
            std::env::var("YI_AGENT_MODEL_CONTEXT_LENGTH")
                .ok()
                .and_then(|s| s.parse().ok())
        });

    let compact_ratio = cli
        .compact_ratio
        .or_else(|| {
            std::env::var("YI_AGENT_COMPACT_RATIO")
                .ok()
                .and_then(|s| s.parse().ok())
        })
        .unwrap_or(80);

    let effective_context_length = model_context_length.unwrap_or(200_000);
    let compact_threshold = effective_context_length * compact_ratio / 100;
```

Update the `Ok(Config { ... })` construction:

```rust
    Ok(Config {
        provider,
        api_url,
        api_key,
        model,
        max_turns,
        workdir,
        system_prompt,
        model_context_length,
        compact_ratio,
        compact_threshold,
        compact_keep_turns,
    })
```

**Step 4: Update all existing test Cli literals**

In all 10 existing `Cli { ... }` test literals, replace:
```rust
            compact_threshold: None,
            compact_keep_turns: None,
```
with:
```rust
            model_context_length: None,
            compact_ratio: None,
            compact_keep_turns: None,
```

**Step 5: Update existing compact test**

Replace `load_includes_compact_defaults`:

```rust
    #[test]
    fn load_includes_compact_defaults() {
        let cli = Cli {
            command: None,
            provider: None,
            api_url: None,
            api_key: Some("test-key".into()),
            model: None,
            max_turns: None,
            workdir: Some(PathBuf::from(".")),
            system_prompt: None,
            model_context_length: None,
            compact_ratio: None,
            compact_keep_turns: None,
        };
        let config = load(&cli).unwrap();
        assert_eq!(config.compact_ratio, 80);
        assert_eq!(config.model_context_length, None);
        assert_eq!(config.compact_threshold, 160_000); // 200000 * 80 / 100
    }
```

**Step 6: Add new tests**

```rust
    #[test]
    fn load_computes_threshold_from_context_and_ratio() {
        let cli = Cli {
            command: None,
            provider: None,
            api_url: None,
            api_key: Some("test-key".into()),
            model: None,
            max_turns: None,
            workdir: Some(PathBuf::from(".")),
            system_prompt: None,
            model_context_length: Some(100_000),
            compact_ratio: Some(50),
            compact_keep_turns: None,
        };
        let config = load(&cli).unwrap();
        assert_eq!(config.compact_threshold, 50_000); // 100000 * 50 / 100
    }

    #[test]
    fn load_falls_back_to_default_context_length() {
        let cli = Cli {
            command: None,
            provider: None,
            api_url: None,
            api_key: Some("test-key".into()),
            model: None,
            max_turns: None,
            workdir: Some(PathBuf::from(".")),
            system_prompt: None,
            model_context_length: None,
            compact_ratio: Some(80),
            compact_keep_turns: None,
        };
        let config = load(&cli).unwrap();
        assert_eq!(config.compact_threshold, 160_000); // 200000 * 80 / 100
    }
```

**Step 7: Run tests**

Run: `cargo test -p yi-agent config`
Expected: all pass

**Step 8: Commit**

```bash
git add yi-agent-rs/crates/yi-agent/src/config.rs
git commit -m "refactor(config): compute compact_threshold from context_length and ratio"
```

---

## Task 3: Update .env.example

**Files:**
- Modify: `.env.example`

**Step 1: Replace content**

```env
# === Model Provider ===
YI_AGENT_PROVIDER=anthropic
YI_AGENT_MODEL=claude-sonnet-4-20250514
YI_AGENT_MODEL_CONTEXT_LENGTH=
MODEL_API_KEY=
MODEL_API_URL=https://api.anthropic.com
ANTHROPIC_API_KEY=
ANTHROPIC_BASE_URL=https://api.anthropic.com
OPENAI_API_KEY=
OPENAI_BASE_URL=https://api.openai.com

# === Agent ===
YI_AGENT_MAX_TURNS=20
YI_AGENT_WORKDIR=.
YI_AGENT_SYSTEM_PROMPT=
YI_AGENT_COMPACT_RATIO=80
YI_AGENT_COMPACT_KEEP_TURNS=4

# === Tools ===
BOCHA_API_KEY=
```

**Step 2: Commit**

```bash
git add .env.example
git commit -m "docs: update .env.example for restructured config"
```

---

## Task 4: Update api_test.rs — group/var counts

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-web/tests/api_test.rs`

**Step 1: Update get_config_returns_all_groups**

```rust
    let groups = json["groups"].as_array().unwrap();
    assert_eq!(groups.len(), 3); // Model Provider, Agent, Tools

    // 验证包含所有 15 个变量
    let total_vars: usize = groups
        .iter()
        .map(|g| g["vars"].as_array().unwrap().len())
        .sum();
    assert_eq!(total_vars, 15);
```

**Step 2: Run tests**

Run: `cargo test -p yi-agent-web --test api_test`
Expected: 5 tests pass

**Step 3: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-web/tests/api_test.rs
git commit -m "test(web): update group/var counts for restructured config"
```

---

## Task 5: Rewrite frontend — vertical tabs and collapsible sections

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-web/src/assets/index.html`

**Step 1: Replace entire file with new layout**

```html
<!DOCTYPE html>
<html lang="zh">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<title>yi-agent 配置</title>
<style>
  :root {
    --bg: #1a1b26;
    --surface: #24283b;
    --surface-hover: #2f334d;
    --border: #3b4261;
    --text: #c0caf5;
    --text-dim: #565f89;
    --accent: #7aa2f7;
    --accent-hover: #89b4fa;
    --success: #9ece6a;
    --warning: #e0af68;
    --error: #f7768e;
    --radius: 8px;
  }
  * { margin: 0; padding: 0; box-sizing: border-box; }
  body {
    font-family: -apple-system, "Segoe UI", Roboto, sans-serif;
    background: var(--bg);
    color: var(--text);
    line-height: 1.6;
    padding: 1.5rem;
    max-width: 900px;
    margin: 0 auto;
  }
  h1 { font-size: 1.5rem; margin-bottom: 0.5rem; }
  .env-path {
    font-family: "SF Mono", "Fira Code", monospace;
    font-size: 0.85rem;
    color: var(--text-dim);
    margin-bottom: 1.5rem;
    padding: 0.5rem 0.75rem;
    background: var(--surface);
    border-radius: var(--radius);
    border: 1px solid var(--border);
    word-break: break-all;
  }
  .layout {
    display: flex;
    gap: 1.5rem;
    min-height: 400px;
  }
  .sidebar {
    width: 180px;
    flex-shrink: 0;
    display: flex;
    flex-direction: column;
    gap: 0.25rem;
  }
  .tab {
    padding: 0.6rem 0.9rem;
    background: var(--surface);
    border: 1px solid var(--border);
    border-radius: var(--radius);
    color: var(--text-dim);
    cursor: pointer;
    font-size: 0.9rem;
    text-align: left;
    transition: all 0.15s;
  }
  .tab:hover { color: var(--text); border-color: var(--accent); }
  .tab.active {
    color: var(--accent);
    border-color: var(--accent);
    background: var(--surface-hover);
  }
  .main {
    flex: 1;
    min-width: 0;
  }
  .group {
    background: var(--surface);
    border: 1px solid var(--border);
    border-radius: var(--radius);
    padding: 1.25rem;
    margin-bottom: 1.5rem;
  }
  .group h2 {
    font-size: 1rem;
    color: var(--accent);
    margin-bottom: 1rem;
    padding-bottom: 0.5rem;
    border-bottom: 1px solid var(--border);
  }
  .section { margin-bottom: 1rem; }
  .section:last-child { margin-bottom: 0; }
  .section-header {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    cursor: pointer;
    padding: 0.5rem 0;
    font-size: 0.9rem;
    color: var(--text);
    font-weight: 600;
    user-select: none;
  }
  .section-header:hover { color: var(--accent); }
  .section-arrow {
    display: inline-block;
    transition: transform 0.15s;
    font-size: 0.75rem;
    color: var(--text-dim);
  }
  .section.collapsed .section-arrow { transform: rotate(-90deg); }
  .section.collapsed .section-body { display: none; }
  .section-body {
    padding-top: 0.5rem;
  }
  .field { margin-bottom: 1rem; }
  .field:last-child { margin-bottom: 0; }
  .field label {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    font-family: "SF Mono", "Fira Code", monospace;
    font-size: 0.85rem;
    color: var(--text);
    margin-bottom: 0.35rem;
  }
  .field .desc {
    font-family: -apple-system, sans-serif;
    font-size: 0.75rem;
    color: var(--text-dim);
    font-weight: normal;
  }
  .field input, .field select {
    width: 100%;
    padding: 0.5rem 0.75rem;
    background: var(--bg);
    border: 1px solid var(--border);
    border-radius: var(--radius);
    color: var(--text);
    font-family: "SF Mono", "Fira Code", monospace;
    font-size: 0.85rem;
    outline: none;
    transition: border-color 0.15s;
  }
  .field input:focus, .field select:focus {
    border-color: var(--accent);
  }
  .field input.modified, .field select.modified {
    border-color: var(--warning);
  }
  .secret-wrapper {
    display: flex;
    gap: 0.5rem;
    align-items: center;
  }
  .secret-wrapper input { flex: 1; }
  .toggle-btn {
    padding: 0.5rem 0.75rem;
    background: var(--bg);
    border: 1px solid var(--border);
    border-radius: var(--radius);
    color: var(--text-dim);
    cursor: pointer;
    font-size: 0.8rem;
    white-space: nowrap;
    transition: all 0.15s;
  }
  .toggle-btn:hover { color: var(--text); border-color: var(--accent); }
  .actions {
    position: sticky;
    bottom: 0;
    background: var(--bg);
    padding: 1rem 0;
    display: flex;
    align-items: center;
    gap: 1rem;
  }
  .save-btn {
    padding: 0.6rem 2rem;
    background: var(--accent);
    color: var(--bg);
    border: none;
    border-radius: var(--radius);
    font-size: 0.9rem;
    font-weight: 600;
    cursor: pointer;
    transition: all 0.15s;
  }
  .save-btn:hover:not(:disabled) { background: var(--accent-hover); }
  .save-btn:disabled { opacity: 0.4; cursor: not-allowed; }
  .status {
    font-size: 0.85rem;
    color: var(--text-dim);
  }
  .status.unsaved { color: var(--warning); }
  .status.saved { color: var(--success); }
  .toast {
    position: fixed;
    top: 1rem;
    right: 1rem;
    padding: 0.75rem 1.25rem;
    background: var(--success);
    color: var(--bg);
    border-radius: var(--radius);
    font-size: 0.85rem;
    font-weight: 600;
    opacity: 0;
    transform: translateY(-10px);
    transition: all 0.3s;
    pointer-events: none;
  }
  .toast.show { opacity: 1; transform: translateY(0); }
  .toast.error { background: var(--error); }
</style>
</head>
<body>

<h1>yi-agent 配置</h1>
<div class="env-path" id="envPath">加载中...</div>

<div class="layout">
  <div class="sidebar" id="sidebar"></div>
  <div class="main" id="main"></div>
</div>

<div class="actions">
  <button class="save-btn" id="saveBtn" disabled>保存</button>
  <span class="status" id="status"></span>
</div>

<div class="toast" id="toast"></div>

<script>
  let allVars = [];
  let originalValues = {};
  let currentValues = {};
  let activeTab = 'Model Provider';
  let collapsedSections = { 'Provider Credentials': true };

  // 定义 sections：仅 Model Provider tab 使用
  const modelProviderSections = [
    { name: 'General', vars: ['YI_AGENT_PROVIDER', 'YI_AGENT_MODEL', 'YI_AGENT_MODEL_CONTEXT_LENGTH', 'MODEL_API_KEY', 'MODEL_API_URL'] },
    { name: 'Provider Credentials', dynamic: true },
  ];

  function getProviderCredentialVars(provider) {
    if (provider === 'openai') return ['OPENAI_API_KEY', 'OPENAI_BASE_URL'];
    return ['ANTHROPIC_API_KEY', 'ANTHROPIC_BASE_URL'];
  }

  async function loadConfig() {
    const resp = await fetch('/api/config');
    const data = await resp.json();
    document.getElementById('envPath').textContent = data.envPath;

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
    renderMain();
    updateSaveButton();
  }

  function renderSidebar() {
    const sidebar = document.getElementById('sidebar');
    sidebar.innerHTML = '';
    const tabs = ['Model Provider', 'Agent', 'Tools'];
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
  }

  function renderMain() {
    const main = document.getElementById('main');
    main.innerHTML = '';

    if (activeTab === 'Model Provider') {
      renderModelProviderTab(main);
    } else {
      renderFlatTab(main, activeTab);
    }
  }

  function renderModelProviderTab(main) {
    const groupEl = document.createElement('div');
    groupEl.className = 'group';
    groupEl.innerHTML = '<h2>Model Provider</h2>';

    for (const section of modelProviderSections) {
      const sectionEl = document.createElement('div');
      sectionEl.className = 'section' + (collapsedSections[section.name] ? ' collapsed' : '');
      sectionEl.dataset.section = section.name;

      const header = document.createElement('div');
      header.className = 'section-header';
      header.innerHTML = `<span class="section-arrow">▼</span> ${section.name}`;
      header.addEventListener('click', () => {
        collapsedSections[section.name] = !collapsedSections[section.name];
        sectionEl.classList.toggle('collapsed');
      });

      const body = document.createElement('div');
      body.className = 'section-body';

      let varsToRender;
      if (section.dynamic) {
        const provider = currentValues['YI_AGENT_PROVIDER'] || 'anthropic';
        varsToRender = getProviderCredentialVars(provider);
      } else {
        varsToRender = section.vars;
      }

      for (const key of varsToRender) {
        const v = allVars.find(x => x.key === key);
        if (v) renderField(body, v);
      }

      sectionEl.appendChild(header);
      sectionEl.appendChild(body);
      groupEl.appendChild(sectionEl);
    }

    main.appendChild(groupEl);
  }

  function renderFlatTab(main, tabName) {
    const groupEl = document.createElement('div');
    groupEl.className = 'group';
    groupEl.innerHTML = `<h2>${tabName}</h2>`;

    for (const v of allVars) {
      if (v.group === tabName) {
        renderField(groupEl, v);
      }
    }

    main.appendChild(groupEl);
  }

  function renderField(container, v) {
    const field = document.createElement('div');
    field.className = 'field';

    const label = document.createElement('label');
    label.innerHTML = `${v.key} <span class="desc">${v.description}</span>`;

    let inputEl;
    if (v.type === 'select') {
      inputEl = document.createElement('select');
      for (const opt of v.options) {
        const o = document.createElement('option');
        o.value = opt;
        o.textContent = opt;
        if (opt === v.value) o.selected = true;
        inputEl.appendChild(o);
      }
    } else {
      inputEl = document.createElement('input');
      inputEl.type = v.type === 'secret' ? 'password' : (v.type === 'number' ? 'number' : 'text');
      inputEl.value = v.value;
      inputEl.placeholder = v.default || '(none)';
    }

    inputEl.dataset.key = v.key;
    inputEl.dataset.type = v.type;
    inputEl.dataset.masked = v.masked;
    inputEl.addEventListener('input', onFieldChange);
    inputEl.addEventListener('change', onFieldChange);

    if (v.type === 'secret') {
      const wrapper = document.createElement('div');
      wrapper.className = 'secret-wrapper';

      inputEl.addEventListener('focus', () => {
        if (inputEl.dataset.masked === 'true' && inputEl.value.includes('***')) {
          inputEl.value = '';
          inputEl.dataset.masked = 'false';
        }
      });

      const toggle = document.createElement('button');
      toggle.className = 'toggle-btn';
      toggle.textContent = '显示';
      toggle.type = 'button';
      toggle.addEventListener('click', () => {
        if (inputEl.type === 'password') {
          inputEl.type = 'text';
          toggle.textContent = '隐藏';
        } else {
          inputEl.type = 'password';
          toggle.textContent = '显示';
        }
      });

      wrapper.appendChild(inputEl);
      wrapper.appendChild(toggle);
      field.appendChild(wrapper);
    } else {
      field.appendChild(inputEl);
    }

    field.insertBefore(label, field.firstChild);
    container.appendChild(field);
  }

  function onFieldChange(e) {
    const key = e.target.dataset.key;
    const newValue = e.target.value;
    currentValues[key] = newValue;

    const original = originalValues[key];
    if (newValue !== original) {
      e.target.classList.add('modified');
    } else {
      e.target.classList.remove('modified');
    }

    // 若 provider 变化，重渲染 Model Provider tab 的 credentials section
    if (key === 'YI_AGENT_PROVIDER') {
      renderMain();
    }

    updateSaveButton();
  }

  function updateSaveButton() {
    let hasChanges = false;
    for (const key in currentValues) {
      if (currentValues[key] !== originalValues[key]) {
        hasChanges = true;
        break;
      }
    }
    const btn = document.getElementById('saveBtn');
    const status = document.getElementById('status');
    btn.disabled = !hasChanges;
    if (hasChanges) {
      status.className = 'status unsaved';
      status.textContent = '有未保存的修改';
    } else {
      status.className = 'status';
      status.textContent = '';
    }
  }

  async function save() {
    const updates = [];
    for (const key in currentValues) {
      if (currentValues[key] !== originalValues[key]) {
        updates.push({ key, value: currentValues[key] });
      }
    }
    if (updates.length === 0) return;

    const resp = await fetch('/api/config', {
      method: 'PUT',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ updates }),
    });

    if (resp.ok) {
      showToast('已保存', false);
      await loadConfig();
    } else {
      const data = await resp.json().catch(() => ({}));
      showToast(data.error || '保存失败', true);
    }
  }

  function showToast(msg, isError) {
    const toast = document.getElementById('toast');
    toast.textContent = msg;
    toast.className = 'toast show' + (isError ? ' error' : '');
    setTimeout(() => toast.className = 'toast', 2500);
  }

  document.getElementById('saveBtn').addEventListener('click', save);
  loadConfig();
</script>

</body>
</html>
```

**Step 2: Build and run tests**

Run: `cargo test -p yi-agent-web`
Expected: all pass

**Step 3: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-web/src/assets/index.html
git commit -m "feat(web): vertical tabs with collapsible sections and context-aware credentials"
```

---

## Task 6: Final verification

**Step 1: Run full test suite**

Run: `cargo test --workspace`
Expected: all pass

**Step 2: Run clippy**

Run: `cargo clippy --workspace --all-targets`
Expected: no warnings

**Step 3: Run fmt check**

Run: `cargo fmt --all -- --check`
Expected: clean

**Step 4: Fix any fmt issues**

If fmt check fails: `cargo fmt --all`

**Step 5: Commit any fixes**

```bash
git add -A
git commit -m "style: apply cargo fmt"
```

---

## Critical Files

- `yi-agent-rs/crates/yi-agent-web/src/config_meta.rs` — variable metadata
- `yi-agent-rs/crates/yi-agent-web/src/assets/index.html` — frontend
- `yi-agent-rs/crates/yi-agent-web/tests/api_test.rs` — integration tests
- `yi-agent-rs/crates/yi-agent/src/config.rs` — config loading
- `.env.example` — template

## Verification

1. `cargo test --workspace` — all pass
2. `cargo clippy --workspace --all-targets` — no warnings
3. `cargo fmt --all -- --check` — clean
4. Manual: `cargo run -p yi-agent -- web` → open http://localhost:7292 → verify:
   - Vertical tabs on left (Model Provider, Agent, Tools)
   - Model Provider tab has "General" (expanded) and "Provider Credentials" (collapsed)
   - Switching provider select toggles credential fields
   - Save works with new vars (context_length, compact_ratio)
