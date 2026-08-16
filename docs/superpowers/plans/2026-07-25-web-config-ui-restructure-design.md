# Web Config UI Restructure Design

## Goal

Restructure the WebUI: merge 3 provider groups into one "Model Provider" tab with vertical tabs and collapsible sections; replace absolute `compact_threshold` with `context_length * ratio` (default 80%).

## Variable Changes

### Removed
- `YI_AGENT_COMPACT_THRESHOLD` (absolute token count, default 100000)

### Added
- `YI_AGENT_MODEL_CONTEXT_LENGTH` — Model max context length in tokens (user-written, e.g. 200000). Group: "Model Provider". Type: Number. Default: None (fallback 200000).
- `YI_AGENT_COMPACT_RATIO` — Percentage of context length triggering auto-compact (default 80). Group: "Agent". Type: Number.

### Renamed Groups
- "Provider" + "Anthropic Provider" + "OpenAI Provider" → "Model Provider"
- "Agent" stays
- "Tools" stays

### New Var Count: 15

## Compact Threshold Computation

In `config::load()`:
```
effective_context_length = model_context_length.unwrap_or(200_000)
compact_threshold = effective_context_length * compact_ratio / 100
```

`compact_threshold` stays as a computed field in `Config` (passed to `AgentConfig` as before — `yi-agent-core` doesn't change).

Fallback when `YI_AGENT_MODEL_CONTEXT_LENGTH` unset: 200000. Default ratio: 80. So default threshold = 160000.

## UI Layout

Vertical tabs on left, content panel on right:

```
┌─────────────┬─────────────────────────────┐
│ Model       │                             │
│ Provider    │  (active tab content)       │
│             │                             │
│ Agent       │                             │
│             │                             │
│ Tools       │                             │
└─────────────┴─────────────────────────────┘
                                        [Save]
```

### Tab 1: Model Provider

**Section "General" (always expanded):**
- `YI_AGENT_PROVIDER` (select: anthropic/openai)
- `YI_AGENT_MODEL` (text)
- `YI_AGENT_MODEL_CONTEXT_LENGTH` (number)
- `MODEL_API_KEY` (secret) — label note: "overrides provider-specific key"
- `MODEL_API_URL` (text) — label note: "overrides provider-specific URL"

**Section "Provider Credentials" (collapsed by default):**
- Context-aware: only shows fields for the currently selected provider
  - `provider=anthropic`: `ANTHROPIC_API_KEY` + `ANTHROPIC_BASE_URL`
  - `provider=openai`: `OPENAI_API_KEY` + `OPENAI_BASE_URL`
- Re-renders live when provider select changes

### Tab 2: Agent (flat, 5 fields)
- `YI_AGENT_MAX_TURNS` (number)
- `YI_AGENT_WORKDIR` (path)
- `YI_AGENT_SYSTEM_PROMPT` (text)
- `YI_AGENT_COMPACT_RATIO` (number 0-100)
- `YI_AGENT_COMPACT_KEEP_TURNS` (number)

### Tab 3: Tools (flat, 1 field)
- `BOCHA_API_KEY` (secret)

## Config Loading Changes

### `Config` struct
- Remove field `compact_threshold: u32` (as loaded value)
- Add field `model_context_length: Option<u32>`
- Add field `compact_ratio: u32`
- Keep `compact_threshold: u32` as **computed** field

### `Cli` struct
- Remove `--compact-threshold` arg
- Add `--model-context-length <N>` arg (Option<u32>)
- Add `--compact-ratio <N>` arg (Option<u32>)

### `load()` computation
```rust
let model_context_length = cli.model_context_length
    .or_else(|| env::var("YI_AGENT_MODEL_CONTEXT_LENGTH").ok().and_then(|s| s.parse().ok()));

let compact_ratio = cli.compact_ratio
    .or_else(|| std::env::var("YI_AGENT_COMPACT_RATIO").ok().and_then(|s| s.parse().ok()))
    .unwrap_or(80);

let effective_context_length = model_context_length.unwrap_or(200_000);
let compact_threshold = effective_context_length * compact_ratio / 100;
```

## API Changes

- `get_config` response: same shape, `groups` array now has 3 entries (was 5), 15 vars total
- `put_config` request: unchanged shape (`{ updates: [{ key, value }] }`)
- `env_file::write()` and `get_config` driven by `ALL_VARS` metadata — no per-var code changes beyond metadata update

## Frontend Changes

### Layout
- `.layout` — flex container, sidebar + main
- `.sidebar` — fixed-width left column (~180px)
- `.tab` — tab button style, `.tab.active` highlighted
- `.section` — collapsible container
- `.section-header` — clickable header with arrow indicator (▼/▶)
- `.section.collapsed .section-body` — `display: none`

### JS
- Tab state variable (`activeTab`), default "Model Provider"
- Collapsible state map (`collapsedSections`), default "Provider Credentials" collapsed
- `renderTab(name)` — renders right panel for active tab
- `renderField(varMeta)` — reusable field renderer
- Provider select `change` event → re-render Provider Credentials section
- Save bar stays global, unchanged

### No New API Calls
Frontend reads all 15 vars from existing `/api/config`, filters by group for each tab.

## Testing

### `config_meta.rs` unit tests
- `all_vars_count_is_15`
- `groups_count_is_3`
- `groups_are_ordered` → `["Model Provider", "Agent", "Tools"]`
- `context_length_has_no_default`
- `compact_ratio_default_is_80`

### `config.rs` tests
- `load_includes_compact_defaults` — assert `compact_ratio == 80`, `compact_threshold == 160000`
- `load_computes_threshold_from_context_and_ratio` — context=100000, ratio=50 → threshold=50000
- `load_falls_back_to_default_context_length` — unset context, ratio=80 → threshold=160000
- `load_reads_context_length_from_env`
- `load_reads_compact_ratio_from_env`
- Update all 10 `Cli { ... }` test literals

### `api_test.rs` integration tests
- `get_config_returns_all_groups` — 3 groups, 15 vars
- Other 4 tests unchanged

### Verification
```bash
cargo test --workspace
cargo clippy --workspace --all-targets
cargo fmt --all -- --check
```

## `.env.example` Updates

- Remove `YI_AGENT_COMPACT_THRESHOLD=100000`
- Add `YI_AGENT_MODEL_CONTEXT_LENGTH=` (under Model Provider)
- Add `YI_AGENT_COMPACT_RATIO=80` (under Agent)
- Merge 3 provider sections into one "Model Provider" section

## Files to Change

- `yi-agent-rs/crates/yi-agent-web/src/config_meta.rs` — metadata: new vars, merged group
- `yi-agent-rs/crates/yi-agent-web/src/assets/index.html` — vertical tabs, collapsible sections, context-aware credentials
- `yi-agent-rs/crates/yi-agent-web/tests/api_test.rs` — update group/var counts
- `yi-agent-rs/crates/yi-agent/src/config.rs` — new fields, computed threshold, updated tests
- `yi-agent-rs/crates/yi-agent/src/main.rs` — pass new config fields to AgentConfig (if needed)
- `.env.example` — update template
