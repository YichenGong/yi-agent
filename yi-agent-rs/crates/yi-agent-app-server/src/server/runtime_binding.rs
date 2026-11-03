//! 运行期绑定与进程级路径解析。
//!
//! 从 `server.rs` 逐字搬出（wave2 C2 batch 1）。函数体未改，只把可见性
//! 收紧到 `pub(crate)`，并由 `server.rs` 统一 re-export 回 `crate::server::X`，
//! 使 `rpc/*` 与 `server/tests/*` 的既有路径继续解析。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use yi_agent_runtime::config::RuntimeConfig;

// 这些项仍定义在 `server.rs`（或后续搬到别的 server 子模块后由其 re-export），
// 一律走 `crate::server::` 路径引用，避免 `super::` 在后续搬移中静默改变含义。
use crate::server::{
    BuiltAgent, current_session_extra_roots, current_session_model_ref,
    current_session_reasoning_effort, rebuild_thread_agent_with_theme, skills_home_from,
    watchman_home_from,
};

/// 生产环境的 `$HOME`。未设置时回退空路径——与 `resident_dir` 同一策略：宁可把
/// 值守装进一个无人读取的目录，也不要因为环境缺失让整个服务起不来。
pub(crate) fn home_dir() -> PathBuf {
    watchman_home_from(std::env::var_os("HOME").as_deref())
}
/// 实例状态根。preferences、skills 等随实例走。
pub(crate) fn instance_home_dir() -> PathBuf {
    skills_home_from(
        std::env::var_os("YI_AGENT_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
    )
}
/// `skill/*` 需要一个「home 形状」的目录（它自己会拼 `.yi-agent/skills`）。
/// 传实例状态根的父目录，结果与 `paths::skills_dir()` 一致。
pub(crate) fn skills_home_dir() -> PathBuf {
    yi_agent_store::paths::instance_root().unwrap_or_default()
}
/// The key that identifies one project directory across this process.
///
/// Runtime attach and every later lookup must agree on this value, so it is
/// computed in exactly one place. A path that exists canonicalizes whole, which
/// resolves symlinks and, on macOS, `/tmp` -> `/private/tmp`. A path that does
/// not exist yet still has to key the *same* before and after it is created, so
/// the deepest existing ancestor is canonicalized and the remaining components
/// are appended unchanged. Without that second half an absent directory keys as
/// its literal spelling while the same directory keys as its canonical path once
/// it exists -- two keys for one project, which makes delegation silently
/// disappear (attach stores one key, the lookup misses it) whenever the two
/// spellings differ.
pub(crate) fn project_key(path: &Path) -> PathBuf {
    if let Ok(canonical) = std::fs::canonicalize(path) {
        return canonical;
    }
    let mut suffix: Vec<std::ffi::OsString> = Vec::new();
    let mut cursor = path;
    loop {
        let Some(parent) = cursor.parent() else {
            return path.to_path_buf();
        };
        if let Some(name) = cursor.file_name() {
            suffix.push(name.to_os_string());
        }
        if let Ok(canonical) = std::fs::canonicalize(parent) {
            let mut key = canonical;
            for part in suffix.iter().rev() {
                key.push(part);
            }
            return key;
        }
        cursor = parent;
    }
}
thread_local! {
    /// 本次 `build_agent` 调用所属会话的 `preserve_thinking`（跨轮 CoT 保留）。
    /// 与 [`SESSION_REASONING_EFFORT`] 同一理由：工厂闭包签名带不进这个逐会话
    /// 参数，而 `build_agent_config` 只认 `RuntimeConfig`（其中没有该字段）。
    ///
    /// `Some(v)` = 本次会话显式指定的值（`thread/resume` 传
    /// `meta.preserve_thinking`）；`None` = 未指定（直接构建与测试），此时按默认
    /// `true` 解释，与 `AgentConfig::default()` 一致 ⇒ 零回归。
    pub(crate) static SESSION_PRESERVE_THINKING: std::cell::RefCell<Option<bool>> =
        const { std::cell::RefCell::new(None) };
}
/// 在调用工厂(`build_agent`)前设定本次会话的 `preserve_thinking`，供工厂解析。
///
/// 调用返回后恢复调用前的值，故嵌套调用不会泄漏。`thread/start` 传 `true`
/// （新会话默认保留）；`thread/resume` 传 `meta.preserve_thinking`——让重开的
/// 会话沿用落盘的开关，否则被关闭保留的会话重开后会静默恢复为「保留」，与
/// 磁盘和已广播给各端的值相悖。
pub(crate) fn with_session_preserve_thinking<T>(preserve: bool, f: impl FnOnce() -> T) -> T {
    SESSION_PRESERVE_THINKING.with(|slot| {
        let prev = slot.replace(Some(preserve));
        let out = f();
        slot.replace(prev);
        out
    })
}
/// 工厂内部读取当前会话的 `preserve_thinking`；未设定时按默认 `true` 解释。
pub(crate) fn current_session_preserve_thinking() -> bool {
    SESSION_PRESERVE_THINKING.with(|slot| slot.borrow().unwrap_or(true))
}
/// 生产环境的 agent 工厂:按 thread 的 cwd 覆盖 workdir 与 yolo 后引导一个 agent。
///
/// `catalog` 在进程启动时经 `load_catalog()` 读入一次并共享;每次调用再按本次会话
/// 的 `model_ref`(由 [`with_session_model_ref`] 设定)解析 provider/api_url/
/// api_key/model 四个字段,使**整条会话**跑在清单条目的 url/key/model 上,而非
/// 全局 `cfg`(解析不中则原样回退 `cfg`)。
pub(crate) fn production_factory(
    cfg: RuntimeConfig,
    theme: crate::theme_tool::ThemeHandle,
    catalog: Arc<yi_agent_runtime::models::CatalogWatcher>,
) -> impl Fn(
    Option<yi_agent_core::Session>,
    &Path,
    crate::thread_store::ThreadMode,
) -> anyhow::Result<BuiltAgent>
+ Send
+ Sync
+ 'static {
    move |session, cwd, mode| {
        let mut thread_cfg = cfg.clone();
        thread_cfg.workdir = cwd.to_path_buf();
        thread_cfg.yolo = mode == crate::thread_store::ThreadMode::Yolo;
        // 本会话的关联根（逐调用传入，不共享）。空 = 单根会话，与改动前一致。
        // **不得**把根集留在共享 `cfg` 里：本工厂按项目目录构建、被同目录多个会话
        // 共享，那样会让 A 会话的关联根泄漏给同目录的其它会话。
        thread_cfg.sandbox_writable_roots = current_session_extra_roots();
        let model_ref = current_session_model_ref();
        // 取「当前」清单:运行期新增/修改的条目立刻可见(原先持启动快照)。
        let catalog = catalog.current();
        // 按本次会话解析 provider/api_url/api_key/model;不中则原样回退 cfg。
        let mut thread_cfg = yi_agent_runtime::models::resolve_effective(
            &thread_cfg,
            &catalog,
            model_ref.as_deref(),
        );
        // 会话 effort 覆盖（若有）优先于条目默认：`resolve_effective` 已带出条目
        // 的 `reasoning_effort`，仅当槽位**确有覆盖**（`Some(tier)`）时才覆盖它。
        // 槽位为 `None` 时一律保留条目默认 —— 这同时覆盖「新会话无覆盖」与「显式
        // 清除覆盖」：清除 = 回落条目默认（解析链第 2 级），而非不发该字段。二者
        // 必须一致，否则同一个 meta 在「活线程清除」（provider 自补条目档）与
        // 「resume 重建」两条路径上会有两种线上行为。
        if let Some(effort) = current_session_reasoning_effort() {
            thread_cfg.reasoning_effort = Some(effort);
        }
        let built = yi_agent_runtime::bootstrap::bootstrap_agent(
            &thread_cfg,
            yi_agent_runtime::bootstrap::PermissionMode::Interactive,
        )?;
        // 主题工具必须进每个 thread 的工具集;委派随后可能用
        // `wrap_for_delegation` 换掉 registry,那条路径([`build_runtime_tooling`])
        // 同样注册。`Agent::new` 会顺带清掉权限检查器,故重建后必须重新装上
        // (见 [`rebuild_thread_agent_with_theme`],否则非 git 目录会退化成不弹审批)。
        Ok(rebuild_thread_agent_with_theme(
            built,
            session,
            theme.clone(),
        ))
    }
}
/// 从 `turn/start` 的 params 提取用户文本:`input:[{type:"text",text}]` 拼接。
///
/// 非 text 的 block（含 attachment）由 `crate::attachments::parse_input` 另行处理；
/// 这里只看文本，空文本返回 `None`。**注意**：附件-only 的消息在这里是 `None`，
/// 是否放行由调用方结合附件列表决定（见 `prepare_turn_core`）。
pub(crate) fn extract_prompt(params: &serde_json::Value) -> Option<String> {
    let (text, _paths) = crate::attachments::parse_input(params);
    if text.trim().is_empty() {
        None
    } else {
        Some(text)
    }
}
