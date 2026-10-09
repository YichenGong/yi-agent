# 模型清单权威化（模型来源显性化 + 一键导入）Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让设置界面如实显示当前实际生效的模型（来自清单条目还是 `.env` 兜底），并在为 `.env` 兜底时提供一键导入，把 `.env` 当前配置落成清单条目并设为全局默认。

**Architecture:** 不改既有解析链（会话覆盖 → 清单 `default_model` → 回退 cfg）。在 `model/list` 响应新增 `effective` 视图（服务端同时握有 catalog 与 cfg，只有它能算），新增一个原子写 RPC `model/importEnv`（只有服务端有明文 key，且避免前端两步半途失败）。桌面端在「全局默认模型」区域常驻一行状态，兜底时附「导入当前配置」按钮，写后重读（沿用既有「无乐观更新」约定）。

**Tech Stack:** Rust（`yi-agent-app-server` / `yi-agent-runtime`）、React + TypeScript + vitest（`desktop/`）。

## Global Constraints

- 设计依据：`docs/superpowers/specs/2026-10-09-model-catalog-authority-design.md`（本计划实现其 §4.2 / §4.3 / §4.4）。
- **明文 key 绝不外泄**：`effective` 视图与 `model/list` 一样，key 只出 `mask_key` 掩码 + `has_key` 布尔，绝不回传 `api_key` 明文。
- **不改 `.env`**：不删、不改 `.env` 文件里的任何模型字段；不做「力度三」（废掉 `.env` 模型字段）。
- **拒绝即零落盘**：任何校验失败必须在 `save_catalog_to` 之前返回 `Err`，磁盘一个字节都不写。
- **测试禁止 mutate 进程级 `HOME`**（cargo 并行测试会竞态）。一律走注入式：`handle_model_request_at(path, ...)` 用 `TempDir` 路径；桌面端用 `modelCall` 接缝。
- **不新增解析层**：复用 `yi_agent_runtime::models::effective_entry` / `mask_key`，不另写一套优先级逻辑。
- 权限：`model/list` 需 `Observe`；`model/importEnv` 需 `Control`（与 `model/upsert` 等同档）。
- Commit：conventional commits，首行 ≤72 字符，**不写 `Co-Authored-By`**；提交前在 `yi-agent-rs/` 下 `cargo fmt --all`；**禁止在 `main` 上直接提交**（全程在 worktree `feat/model-catalog-authority` 内）。
- 按 crate 跑测试，避免 `--workspace`（易 OOM/exit 137）；跑前 `ps aux | grep -v grep | grep -cE "cargo|rustc"` 须为 0。

---

### Task 1: `model/list` 增加「生效解析」视图 `effective`

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/model_rpc.rs`（`handle_model_request`、`handle_model_request_at`、新增 `effective_view`、tests）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs:2558` 与 `:2581`（两处 `handle_model_request_at` 调用补 `&cfg` 实参）

**Interfaces:**
- Consumes: `yi_agent_runtime::models::{effective_entry, mask_key, ModelCatalog, load_catalog_from}`；`yi_agent_runtime::config::RuntimeConfig`。
- Produces:
  - `pub fn handle_model_request(method: &str, params: &Value, fallback: &RuntimeConfig) -> Result<Value, RpcError>`
  - `pub fn handle_model_request_at(path: &Path, method: &str, params: &Value, fallback: &RuntimeConfig) -> Result<Value, RpcError>`（签名新增第 4 参）
  - `model/list` 响应新增字段 `effective`：`{ source: "catalog"|"env", model_ref: string|null, provider, api_url, model, has_key: bool, api_key_masked: string }`

- [ ] **Step 1: 写失败测试**

在 `model_rpc.rs` 的 `mod tests` 内，先给 `call` 补一个 `fallback` 参数并加一个测试用 cfg 构造器：

```rust
    use yi_agent_runtime::config::RuntimeConfig;
    use yi_agent_runtime::models::ModelCatalog;

    /// 兜底层 cfg：全部字段可辨，便于断言「生效值来自 cfg」。
    ///
    /// `RuntimeConfig` **未实现** `Default`，必须显式构造全部字段（字段表见
    /// `yi-agent-runtime/src/config.rs:17-40`）。用 `#[cfg(test)]` 的
    /// `yi_agent_runtime::config::sample_config()` 不可行——它是 runtime crate 的
    /// `pub(crate)`，跨 crate 用不了。
    fn fallback_cfg() -> RuntimeConfig {
        RuntimeConfig {
            provider: "openai".to_string(),
            api_url: "https://env.example".to_string(),
            api_key: "env-secret-9999".to_string(),
            model: "env-model".to_string(),
            max_turns: 20,
            max_resident_subagents: 8,
            workdir: std::path::PathBuf::from("/tmp/import-env-test"),
            system_prompt: None,
            compact_threshold: 160_000,
            compact_user_budget_tokens: 20_000,
            compact_tool_budget_tokens: 12_000,
            yolo: false,
            sandbox_promotable: true,
            sandbox: yi_agent_tools::SandboxMode::WorkspaceWrite,
            sandbox_writable_roots: Vec::new(),
            skills_catalog_budget: 8192,
            skills_catalog_budget_explicit: false,
        }
    }
```

> 若 `yi_agent_tools` 不是 `yi-agent-app-server` 的直接依赖、或 `max_resident_subagents` 的默认常量不可见，就按编译错误调整：`sandbox` 取 `yi_agent_tools::SandboxMode::WorkspaceWrite`，`max_resident_subagents` 填字面量 `8`（测试只断言 provider/url/model/key 四个字段，其余值不影响断言）。

把现有 `call` 改为：

```rust
    fn call(path: &Path, method: &str, params: Value) -> Result<Value, RpcError> {
        handle_model_request_at(path, method, &params, &fallback_cfg())
    }
```

新增用例：

```rust
    #[test]
    fn effective_is_the_catalog_default_when_one_resolves() {
        let (_dir, path) = catalog_path();
        call(&path, "model/upsert", entry_json("A")).unwrap();
        call(&path, "model/setDefault", json!({ "name": "A" })).unwrap();
        let out = call(&path, "model/list", json!({})).unwrap();
        assert_eq!(out["effective"]["source"], "catalog");
        assert_eq!(out["effective"]["model_ref"], "A");
        assert_eq!(out["effective"]["model"], "m");
        assert_eq!(out["effective"]["api_key_masked"], "••••1234");
        // 明文绝不在任何字段里。
        assert!(!out.to_string().contains("sk-secret-1234"));
    }

    #[test]
    fn effective_is_the_env_fallback_when_the_catalog_cannot_resolve() {
        let (_dir, path) = catalog_path();
        // 清单为空 → 必须落到 cfg。
        let out = call(&path, "model/list", json!({})).unwrap();
        assert_eq!(out["effective"]["source"], "env");
        assert!(out["effective"]["model_ref"].is_null());
        assert_eq!(out["effective"]["model"], "env-model");
        assert_eq!(out["effective"]["api_url"], "https://env.example");
        assert_eq!(out["effective"]["provider"], "openai");
        assert_eq!(out["effective"]["api_key_masked"], "••••9999");
        assert!(!out.to_string().contains("env-secret-9999"));
    }

    #[test]
    fn a_dangling_default_falls_back_to_env() {
        let (_dir, path) = catalog_path();
        call(&path, "model/upsert", entry_json("A")).unwrap();
        call(&path, "model/setDefault", json!({ "name": "A" })).unwrap();
        call(&path, "model/delete", json!({ "name": "A" })).unwrap();
        // delete 会清掉悬空引用；再手动写一个悬空 default 覆盖该情形。
        let mut catalog = ModelCatalog::default();
        catalog.models.push(ModelEntry {
            name: "B".to_string(),
            provider: ModelProvider::parse("anthropic").unwrap(),
            api_url: "https://b".to_string(),
            model: "mb".to_string(),
            api_key: String::new(),
        });
        catalog.default_model = Some("gone".to_string());
        yi_agent_runtime::models::save_catalog_to(&path, &catalog).unwrap();
        let out = call(&path, "model/list", json!({})).unwrap();
        assert_eq!(out["effective"]["source"], "env");
        assert!(out["effective"]["model_ref"].is_null());
    }
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib model_rpc::tests::effective`
Expected: 编译失败（`handle_model_request_at` 参数个数不符 / `effective` 字段不存在）。

- [ ] **Step 3: 写最小实现**

在 `model_rpc.rs` 顶部导入补上：

```rust
use yi_agent_runtime::config::RuntimeConfig;
use yi_agent_runtime::models::{
    ModelEntry, ModelProvider, effective_entry, load_catalog_from, mask_key, models_path,
    save_catalog_to,
};
```

改入口签名：

```rust
/// 生产入口：读写 `~/.yi-agent/models.json`。
pub fn handle_model_request(
    method: &str,
    params: &Value,
    fallback: &RuntimeConfig,
) -> Result<Value, RpcError> {
    handle_model_request_at(&models_path(), method, params, fallback)
}

/// 可测核心：清单文件路径与兜底 cfg 均由调用方注入。
pub fn handle_model_request_at(
    path: &Path,
    method: &str,
    params: &Value,
    fallback: &RuntimeConfig,
) -> Result<Value, RpcError> {
    match method {
        "model/list" => {
            let catalog = load_catalog_from(path);
            Ok(json!({
                "models": catalog.models.iter().map(entry_view).collect::<Vec<_>>(),
                "default_model": catalog.default_model,
                "subagent_model": catalog.subagent_model,
                "effective": effective_view(&catalog, fallback),
            }))
        }
        "model/upsert" => upsert(path, params),
        "model/delete" => delete(path, params),
        "model/setDefault" => set_reference(path, params, Reference::Default),
        "model/setSubagent" => set_reference(path, params, Reference::Subagent),
        _ => Err(RpcError::method_not_found(method)),
    }
}
```

新增 `effective_view`：

```rust
/// 当前默认实际解析到哪里：命中清单条目就用条目，否则回退 cfg（`.env`）。
/// key 一律只出掩码——与 [`entry_view`] 同一条安全约定。
fn effective_view(catalog: &ModelCatalog, cfg: &RuntimeConfig) -> Value {
    match effective_entry(catalog, None) {
        Some(entry) => json!({
            "source": "catalog",
            "model_ref": entry.name,
            "provider": entry.provider.as_str(),
            "api_url": entry.api_url,
            "model": entry.model,
            "has_key": !entry.api_key.is_empty(),
            "api_key_masked": mask_key(&entry.api_key),
        }),
        None => json!({
            "source": "env",
            "model_ref": Value::Null,
            "provider": cfg.provider,
            "api_url": cfg.api_url,
            "model": cfg.model,
            "has_key": !cfg.api_key.is_empty(),
            "api_key_masked": mask_key(&cfg.api_key),
        }),
    }
}
```

（`ModelCatalog` 需在导入里可见：补 `use yi_agent_runtime::models::ModelCatalog;`，或把它并入上面的 `use` 列表。）

在 `server.rs` 两处调用补 `&cfg`：

```rust
                        match crate::model_rpc::handle_model_request_at(
                            &models_path,
                            method.as_str(),
                            &req.params,
                            &cfg,
                        ) {
```

（`:2558` 的 `model/list` 臂与 `:2581` 的写臂都要改。`cfg` 是 `serve` 的局部 `RuntimeConfig`，在循环里可借用，未被移动。）

- [ ] **Step 4: 运行测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib model_rpc`
Expected: PASS（含新增 3 例 + 既有例全部通过）。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git checkout -- .   # 清掉 rustfmt 对无关文件的 churn，只留本次改的两个文件（见下方核对）
git add crates/yi-agent-app-server/src/model_rpc.rs crates/yi-agent-app-server/src/server.rs
git commit -m "feat(app-server): report the effective model source on model/list"
```

> **fmt churn 处置**：仓库既有 rustfmt 1.98.1 drift 会让 `cargo fmt --all` 改动大量无关文件。执行 `git status --short`，对**非** `model_rpc.rs`/`server.rs` 的改动逐一 `git checkout -- <file>` 还原，确保 commit 只含这两个文件。

---

### Task 2: 新增原子写 RPC `model/importEnv`

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/model_rpc.rs`（新增 `import_env`、dispatch 臂、tests）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs:2581`（把 `model/importEnv` 并入 `Control` 写臂的方法名列表）

**Interfaces:**
- Consumes: Task 1 的 `handle_model_request_at(path, method, params, fallback)`；`RuntimeConfig`；`save_catalog_to`。
- Produces:
  - `model/importEnv`（无参）→ `{ ok: true, name: <新条目名>, default_model: <新条目名> }`
  - 内部 `fn import_env(path: &Path, cfg: &RuntimeConfig) -> Result<Value, RpcError>`

- [ ] **Step 1: 写失败测试**

在 `model_rpc.rs` 的 `mod tests` 内新增：

```rust
    #[test]
    fn import_env_lands_the_fallback_and_makes_it_the_default() {
        let (_dir, path) = catalog_path();
        let out = call(&path, "model/importEnv", json!({})).unwrap();
        assert_eq!(out["ok"], true);
        assert_eq!(out["name"], "env-model");
        assert_eq!(out["default_model"], "env-model");

        let list = call(&path, "model/list", json!({})).unwrap();
        assert_eq!(list["models"].as_array().unwrap().len(), 1);
        assert_eq!(list["models"][0]["name"], "env-model");
        assert_eq!(list["models"][0]["api_url"], "https://env.example");
        assert_eq!(list["models"][0]["model"], "env-model");
        assert_eq!(list["models"][0]["api_key_masked"], "••••9999");
        // 导入后生效来源翻成清单，且明文不外泄。
        assert_eq!(list["effective"]["source"], "catalog");
        assert_eq!(list["effective"]["model_ref"], "env-model");
        assert!(!list.to_string().contains("env-secret-9999"));
    }

    #[test]
    fn import_env_dedupes_a_name_that_already_exists() {
        let (_dir, path) = catalog_path();
        // 清单里先有一条同名 "env-model"。
        call(
            &path,
            "model/upsert",
            json!({ "name": "env-model", "provider": "anthropic",
                    "api_url": "https://x", "model": "mx" }),
        )
        .unwrap();
        let out = call(&path, "model/importEnv", json!({})).unwrap();
        assert_eq!(out["name"], "env-model-2");
        let list = call(&path, "model/list", json!({})).unwrap();
        let names: Vec<&str> = list["models"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"env-model"));
        assert!(names.contains(&"env-model-2"));
        // 不覆盖既有条目，且默认指向新条目。
        assert_eq!(list["default_model"], "env-model-2");
    }

    #[test]
    fn import_env_rejects_an_invalid_fallback_without_writing() {
        let (_dir, path) = catalog_path();
        let mut cfg = fallback_cfg();
        cfg.api_url = String::new(); // 非法：url 为空
        let err = handle_model_request_at(&path, "model/importEnv", &json!({}), &cfg).unwrap_err();
        assert_eq!(err.data.unwrap()["code"], "invalid_model");
        assert!(!path.exists(), "a rejected import must not write a catalog");
    }
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib model_rpc::tests::import_env`
Expected: FAIL，`method_not_found`（`model/importEnv` 未实现）。

- [ ] **Step 3: 写最小实现**

在 `handle_model_request_at` 的 `match` 中加臂：

```rust
        "model/importEnv" => import_env(path, fallback),
```

新增函数：

```rust
/// `model/importEnv`：把兜底层 `cfg`（`.env`）当前的 provider/api_url/api_key/model
/// 原子地落成清单里的一条，并把它设为 `default_model`。
///
/// 一次性完成而不是前端两步（upsert + setDefault）：只有服务端握有明文 key，
/// 且两步之间可能半途失败留下「条目落了、默认没设」的中间态。
fn import_env(path: &Path, cfg: &RuntimeConfig) -> Result<Value, RpcError> {
    if cfg.api_url.trim().is_empty() {
        return Err(RpcError::invalid_model("api_url must not be empty"));
    }
    if cfg.model.trim().is_empty() {
        return Err(RpcError::invalid_model("model must not be empty"));
    }
    let Some(provider) = ModelProvider::parse(&cfg.provider) else {
        return Err(RpcError::invalid_model(format!(
            "unknown provider: {}",
            cfg.provider
        )));
    };

    let mut catalog = load_catalog_from(path);
    let base = cfg.model.trim().to_string();
    // 重名自动加后缀，静默成功（收边是「零摩擦固化」，不该被重名卡住）。
    let mut name = base.clone();
    let mut n = 2u32;
    while catalog.models.iter().any(|m| m.name == name) {
        name = format!("{base}-{n}");
        n += 1;
    }
    catalog.models.push(ModelEntry {
        name: name.clone(),
        provider,
        api_url: cfg.api_url.clone(),
        model: cfg.model.clone(),
        api_key: cfg.api_key.clone(),
    });
    catalog.default_model = Some(name.clone());
    save_catalog_to(path, &catalog).map_err(|error| RpcError::internal(error.to_string()))?;
    Ok(json!({ "ok": true, "name": name, "default_model": name }))
}
```

在 `server.rs` 把 `model/importEnv` 并入 `Control` 写臂的方法名列表：

```rust
                    "model/upsert" | "model/delete" | "model/setDefault" | "model/setSubagent"
                    | "model/importEnv" => {
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib model_rpc`
Expected: PASS（含新增 3 例）。

再跑全 crate 确认无回归：

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git status --short   # 还原非本次两个文件的 fmt churn
git add crates/yi-agent-app-server/src/model_rpc.rs crates/yi-agent-app-server/src/server.rs
git commit -m "feat(app-server): add model/importEnv to adopt the env fallback"
```

---

### Task 3: 桌面端显示生效来源 + 「导入当前配置」

**Files:**
- Modify: `desktop/src/lib/models.ts`（`EffectiveModel` 类型、`ModelList.effective`、`importEnvModel`）
- Modify: `desktop/src/lib/models.test.ts`
- Modify: `desktop/src/components/SettingsModelsTab.tsx`（状态行 + 导入按钮 + `importCurrent`）
- Modify: `desktop/src/components/SettingsModelsTab.test.tsx`

**Interfaces:**
- Consumes: 宿主 `model/list` 的 `effective` 字段、`model/importEnv`（Task 1/2）。
- Produces:
  - `export type EffectiveModel = { source: "catalog" | "env"; model_ref: string | null; provider: string; api_url: string; model: string; has_key: boolean; api_key_masked: string }`
  - `ModelList` 新增 `effective: EffectiveModel | null`
  - `export async function importEnvModel(rpc: ModelRpc): Promise<{ name: string }>`

- [ ] **Step 1: 写失败测试**

在 `desktop/src/lib/models.test.ts` 追加：

```ts
import { describe, expect, it, vi } from "vitest";
import { importEnvModel, listModels } from "./models";

describe("listModels.effective", () => {
  it("carries the effective view through", async () => {
    const rpc = vi.fn().mockResolvedValue({
      models: [],
      default_model: null,
      subagent_model: null,
      effective: {
        source: "env", model_ref: null, provider: "openai",
        api_url: "https://env", model: "env-model",
        has_key: true, api_key_masked: "••••9999",
      },
    });
    const list = await listModels(rpc);
    expect(list.effective?.source).toBe("env");
    expect(list.effective?.model).toBe("env-model");
  });

  it("defaults a missing effective to null", async () => {
    const rpc = vi.fn().mockResolvedValue({ models: [], default_model: null, subagent_model: null });
    expect((await listModels(rpc)).effective).toBeNull();
  });
});

describe("importEnvModel", () => {
  it("calls model/importEnv with empty params and returns the new name", async () => {
    const rpc = vi.fn().mockResolvedValue({ ok: true, name: "env-model", default_model: "env-model" });
    const out = await importEnvModel(rpc);
    expect(rpc).toHaveBeenCalledWith("model/importEnv", {});
    expect(out).toEqual({ name: "env-model" });
  });
});
```

在 `desktop/src/components/SettingsModelsTab.test.tsx`：先把 `payload()` 补上 `effective`（默认 catalog 来源），再加两个用例：

```tsx
function payload(overrides: Partial<ModelList> = {}): ModelList {
  return {
    models: [ENTRY],
    default_model: "A",
    subagent_model: null,
    effective: {
      source: "catalog", model_ref: "A", provider: "anthropic",
      api_url: "u", model: "m", has_key: true, api_key_masked: "••••1234",
    },
    ...overrides,
  };
}
```

```tsx
  it("says so and offers an import when the model comes from .env", async () => {
    const call = vi.fn(async (method: string, _params: unknown) =>
      method === "model/list"
        ? payload({
            models: [],
            default_model: null,
            effective: {
              source: "env", model_ref: null, provider: "openai",
              api_url: "https://env", model: "env-model",
              has_key: true, api_key_masked: "••••9999",
            },
          })
        : { ok: true },
    );
    render(<SettingsModelsTab call={call} />);

    // 如实告知「在用 .env 的那个模型」，而不是干说「还没有配置任何模型」。
    expect(await screen.findByText(/来自 \.env/)).toBeTruthy();
    expect(screen.getByText(/env-model/)).toBeTruthy();

    fireEvent.click(screen.getByRole("button", { name: "导入当前配置" }));
    await waitFor(() => expect(call).toHaveBeenCalledWith("model/importEnv", {}));
    // 写后重读：model/list 至少被调两次（首载 + 导入后）。
    await waitFor(() => expect(callsTo(call, "model/list").length).toBeGreaterThanOrEqual(2));
  });

  it("does not offer an import when the model resolves from the catalog", async () => {
    const call = vi.fn(async (method: string, _params: unknown) =>
      method === "model/list" ? payload() : { ok: true },
    );
    render(<SettingsModelsTab call={call} />);
    await screen.findByText("••••1234");
    expect(screen.queryByRole("button", { name: "导入当前配置" })).toBeNull();
  });
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd desktop && npx vitest run src/lib/models.test.ts src/components/SettingsModelsTab.test.tsx`
Expected: FAIL（`effective` / `importEnvModel` 不存在、按钮找不到）。

- [ ] **Step 3: 写最小实现**

`desktop/src/lib/models.ts`：类型与函数

```ts
/** 当前默认实际解析到哪里：清单条目（catalog）还是回退 `.env`（env）。key 只出掩码。 */
export type EffectiveModel = {
  source: "catalog" | "env";
  model_ref: string | null;
  provider: string;
  api_url: string;
  model: string;
  has_key: boolean;
  api_key_masked: string;
};

export type ModelList = {
  models: ModelEntryView[];
  default_model: string | null;
  subagent_model: string | null;
  /** 宿主未提供时回 null，面板只渲染已知信息。 */
  effective: EffectiveModel | null;
};
```

`listModels` 返回值补一行：

```ts
    effective: result?.effective ?? null,
```

> `Partial<ModelList>` 已含可选 `effective`；若 TS 报错，就把 `rpc<Partial<ModelList>>` 保留并确保 `effective` 在 `ModelList` 上是必需字段——`result?.effective ?? null` 在 `Partial` 下类型为 `EffectiveModel | null | undefined`，赋给 `EffectiveModel | null` 需 `?? null` 收口，已满足。

新增包装：

```ts
/**
 * 把 `.env`（兜底层）当前的模型配置导入清单并设为全局默认。
 *
 * 由宿主原子完成：只有它握有明文 key，且避免前端「落条目 / 设默认」两步半途失败。
 */
export async function importEnvModel(rpc: ModelRpc): Promise<{ name: string }> {
  const result = await rpc<{ name?: unknown }>("model/importEnv", {});
  return { name: typeof result?.name === "string" ? result.name : "" };
}
```

`SettingsModelsTab.tsx`：导入 `importEnvModel`，加处理函数与状态行：

```tsx
import {
  deleteModel,
  importEnvModel,   // 新增
  isModelNotFound,
  listModels,
  setDefaultModel,
  setSubagentModel,
  upsertModel,
  type ModelEntryView,
  type ModelList,
  type ModelRpc,
  type ModelUpsertInput,
} from "../lib/models";
```

```tsx
const EMPTY: ModelList = {
  models: [],
  default_model: null,
  subagent_model: null,
  effective: null,
};
```

```tsx
  /** 收边：把 `.env` 当前配置导入清单并设为默认；写后重读（无乐观更新）。 */
  const importCurrent = async () => {
    if (!rpc) return;
    setBusy(true);
    setActionError(null);
    try {
      await importEnvModel(rpc);
      await reload();
    } catch (error) {
      setActionError(saveErrorText(error));
    } finally {
      setBusy(false);
    }
  };
```

在「全局默认模型 / 子 agent 模型」那个 `flex flex-wrap gap-4` 容器**之前**插入状态行：

```tsx
      {catalog.effective !== null && (
        <p className="mt-4 text-xs text-fg-subtle">
          {catalog.effective.source === "catalog" ? (
            <>当前生效：{catalog.effective.model}（清单条目「{catalog.effective.model_ref}」）</>
          ) : (
            <>
              当前实际在用 {catalog.effective.model}（来自 .env，尚未纳入清单）
              <button
                type="button"
                onClick={() => void importCurrent()}
                disabled={!rpc || busy}
                className="ml-2 rounded border border-line-strong px-2 py-0.5 text-xs text-fg-muted hover:text-fg disabled:opacity-50"
              >
                导入当前配置
              </button>
            </>
          )}
        </p>
      )}
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cd desktop && npx vitest run src/lib/models.test.ts src/components/SettingsModelsTab.test.tsx src/components/SettingsDialog.test.tsx`
Expected: PASS。

Run: `cd desktop && npx tsc --noEmit`
Expected: 无输出（exit 0）。

- [ ] **Step 5: 提交**

```bash
git add desktop/src/lib/models.ts desktop/src/lib/models.test.ts \
        desktop/src/components/SettingsModelsTab.tsx desktop/src/components/SettingsModelsTab.test.tsx
git commit -m "feat(desktop): show the effective model source and import it"
```

---

### Task 4: 文档与项目进度登记

**Files:**
- Modify: `docs/project-management/yi-agent-app-server.md`
- Modify: `docs/project-management/desktop.md`
- Modify: `docs/project-management/README.md`、`README.md`（模块索引计数，若表格追踪）

**Interfaces:** 无。

- [ ] **Step 1: 登记新项**

按各文件既有格式（`- [x] 标题 — 判据（`路径:行` 或可执行命令）— 验证：<命令>`）追加，三态只用 `[x]/[ ]/[-]`（**禁 `[~]`**）：

- app-server：`model/list` 的 `effective` 视图（`crates/yi-agent-app-server/src/model_rpc.rs`）+ 新增 `model/importEnv`（需 `Control`，原子落条目并设默认）。验证：`cargo test -p yi-agent-app-server --lib model_rpc`。
- desktop：「模型」设置页显示当前生效来源 + 兜底时「导入当前配置」（`desktop/src/components/SettingsModelsTab.tsx`、`desktop/src/lib/models.ts`）。验证：`cd desktop && npx vitest run src/components/SettingsModelsTab.test.tsx`。

- [ ] **Step 2: 同步 README 计数**

若 `README.md` / `docs/project-management/README.md` 的模块索引表追踪「完成/总计」，按各模块文件内 `[x]` 与条目总数的实测值更新对应行；不发明新格式，不动未涉及模块的既有（可能已知漂移的）计数。

- [ ] **Step 3: 提交**

```bash
git add docs/project-management README.md
git commit -m "docs: register the model source view and import in project management"
```

---

## Self-Review

**1. Spec coverage：**
- §4.2 `model/list` 增加 `effective` 视图 → Task 1。✓
- §4.3 新增 `model/importEnv`（Control、原子、重名加后缀、拒绝即零落盘）→ Task 2。✓
- §4.4 桌面端常驻生效来源 + 兜底时「导入当前配置」+ 写后重读、子 agent 不另做 → Task 3。✓
- §6 测试策略（服务端 catalog/env/悬空、importEnv 落盘/重名/非法、key 掩码；桌面 catalog/env/点击重读/失败）→ Task 1/2/3 的测试步骤。✓
- 文档登记 → Task 4。✓
- 非目标（不改 `.env`、不做力度三、不动 ModelPicker/子 agent 解析）→ Global Constraints 与各任务未触碰相应代码。✓

**2. Placeholder scan：** 各代码步骤均含可运行代码；无 TBD/TODO；测试步骤含实际断言与命令。`catalog.default_model = Some(...)` 处用局部变量 `catalog` 的写法已在 Task 1 测试中显式给出（避免临时值生命周期问题）。

**3. Type consistency：**
- `handle_model_request_at` 的第 4 参 `fallback: &RuntimeConfig` 在 Task 1 定义，Task 2 沿用，签名一致。
- `effective` 字段名（`source`/`model_ref`/`provider`/`api_url`/`model`/`has_key`/`api_key_masked`）在服务端 `effective_view`、`models.ts` 的 `EffectiveModel`、组件与测试中一致。
- `importEnvModel` 返回 `{ name: string }`，组件只用其副作用（重读），测试断言 `model/importEnv` 调用参数为 `{}`。
- Rust 导入符号在两处 `use` 中保持一致（`effective_entry`/`mask_key`/`RuntimeConfig`/`ModelCatalog`）。

**4. 依赖顺序：** Task 2 依赖 Task 1 的新签名；Task 3 依赖 Task 1/2 的协议；Task 4 依赖前三者落地。执行须按序。
