//! 受管运行上下文：插件如何知道自己"属于哪个项目、由谁授权、能不能跑"。
//!
//! # 上下文只来自宿主
//!
//! 受管 launcher 用环境变量把上下文交给插件（G1 §12、`host-v1.json` 的
//! `reservedEnv`）：
//!
//! | 变量 | 含义 |
//! |---|---|
//! | `YI_AGENT_HOME` | 实例根 |
//! | `YI_AGENT_PLUGIN_INSTANCE` | 宿主实例 id |
//! | `YI_AGENT_PLUGIN_BINDING` | `项目:grant:activation:effect` |
//!
//! **`binding` 里的项目才是项目归属**，不是 `cwd`，也不是 worktree 目录
//! （设计 §7.3：状态目录不能默认成 worktree cwd，那会让会话写到另一块看板）。
//! 一个由宿主驱动的卡片会话跑在 `.worktrees/...` 里，若拿 cwd 当项目，它会
//! 在一块不存在的看板上工作。
//!
//! # 缺上下文时如实拒绝
//!
//! 独立 CLI（`superpowers-kanban add ...`）没有受管上下文也能工作——那是显式
//! 的 standalone 模式，`--state-dir` 由调用方给。但**受管入口**（宿主拉起的
//! `run`、以及任何声称"我在受管项目里"的调用）缺 binding 时必须失败：
//! 猜一个项目会让一次写入落到错误的地方，而且不会有任何报错。
//!
//! # 保留键不可覆盖
//!
//! 插件自己的环境项命中保留名即拒绝启动，理由与宿主侧
//! `yi-agent-supervisors::managed::build_command` 一致：悄悄丢弃会让插件
//! "看起来配好了、实际没生效"。两边都检查，因为插件可能不经过那个 launcher
//! 而被直接执行。

use std::path::{Path, PathBuf};

/// 宿主拥有的环境变量名。与 `plugins/contracts/fixtures/host-v1.json` 的
/// `reservedEnv` 及宿主侧 `RESERVED_ENV` **逐字一致**；三处任一漂移，
/// 插件就能覆盖一个本该保留的变量。
pub const RESERVED_ENV: [&str; 3] = [
    "YI_AGENT_HOME",
    "YI_AGENT_PLUGIN_INSTANCE",
    "YI_AGENT_PLUGIN_BINDING",
];

/// 插件从宿主拿到的一次运行上下文。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Context {
    /// 实例根（`YI_AGENT_HOME`）。
    pub instance_home: PathBuf,
    /// 宿主实例 id（`YI_AGENT_PLUGIN_INSTANCE`）。
    pub host: String,
    /// 受管绑定：项目归属与代次。
    pub binding: Binding,
}

/// 宿主授予的一次绑定。
///
/// `grant` 与 `activation` 递增即代表"授权变了"：旧会话不该继续用旧代次写入
/// （协议 §3.2 的双代次握手）。所以两者都留在上下文里，而不是只留项目。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    /// 授权项目。**项目归属的唯一来源。**
    pub project: PathBuf,
    pub grant: u64,
    pub activation: u64,
    /// 本次启动被授权的 effect（`Read` / `Write` / `Launch` / …）。
    pub effect: String,
}

/// 上下文错误。全部是"拒绝启动"级别的，没有"猜一个"的降级路径。
#[derive(Debug, PartialEq, Eq)]
pub enum ContextError {
    Missing(&'static str),
    Malformed {
        variable: &'static str,
        detail: String,
    },
    /// 插件试图设置一个宿主的保留变量。
    ReservedEnv(String),
}

impl std::fmt::Display for ContextError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ContextError::Missing(variable) => write!(
                f,
                "{variable} is not set: this process was not launched by the host as a managed \
                 plugin, so it has no project to act on"
            ),
            ContextError::Malformed { variable, detail } => {
                write!(f, "{variable} is malformed: {detail}")
            }
            ContextError::ReservedEnv(name) => write!(
                f,
                "may not set the reserved environment variable {name}; it belongs to the host, \
                 and honoring it would redirect the instance root or the project binding"
            ),
        }
    }
}

impl std::error::Error for ContextError {}

/// 从环境读上下文。`lookup` 注入以便测试不碰真实环境。
///
/// 三项全缺 → `None`（standalone 模式，不是错误）。部分缺失 → `Missing`：
/// 半套上下文通常意味着宿主的启动脚本改了而插件没跟上，继续跑会用错项目
/// 或用错实例根。
pub fn context_from_env(
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<Option<Context>, ContextError> {
    let home = lookup("YI_AGENT_HOME").filter(|value| !value.is_empty());
    let host = lookup("YI_AGENT_PLUGIN_INSTANCE").filter(|value| !value.is_empty());
    let binding = lookup("YI_AGENT_PLUGIN_BINDING").filter(|value| !value.is_empty());

    if home.is_none() && host.is_none() && binding.is_none() {
        return Ok(None);
    }

    let instance_home = PathBuf::from(home.ok_or(ContextError::Missing("YI_AGENT_HOME"))?);
    let host = host.ok_or(ContextError::Missing("YI_AGENT_PLUGIN_INSTANCE"))?;
    let binding = binding.ok_or(ContextError::Missing("YI_AGENT_PLUGIN_BINDING"))?;

    Ok(Some(Context {
        instance_home,
        host,
        binding: parse_binding(&binding)?,
    }))
}

/// 解析 `项目:grant:activation:effect`。
///
/// 项目路径从**右侧**切分：macOS 的路径可以合法包含 `:`（`:` 只在不含 `/`
/// 的 HFS 文件名里被禁止，而路径里带 `/` 的段可以含 `:`）。固定切 4 段会让
/// 这种路径被截断成一个不存在的项目，而它看起来仍然"解析成功"。
pub fn parse_binding(raw: &str) -> Result<Binding, ContextError> {
    let parts: Vec<&str> = raw.rsplitn(4, ':').collect();
    if parts.len() != 4 {
        return Err(ContextError::Malformed {
            variable: "YI_AGENT_PLUGIN_BINDING",
            detail: format!("expected `project:grant:activation:effect`, got {raw:?}"),
        });
    }
    // rsplitn 从右往左，所以顺序要倒回来。
    let effect = parts[0];
    let activation = parts[1];
    let grant = parts[2];
    let project = parts[3];

    let parse_number = |value: &str, which: &str| -> Result<u64, ContextError> {
        value.parse::<u64>().map_err(|_| ContextError::Malformed {
            variable: "YI_AGENT_PLUGIN_BINDING",
            detail: format!("{which} must be a number, got {value:?}"),
        })
    };

    if project.is_empty() {
        return Err(ContextError::Malformed {
            variable: "YI_AGENT_PLUGIN_BINDING",
            detail: "the project segment is empty".to_string(),
        });
    }
    if effect.is_empty() {
        return Err(ContextError::Malformed {
            variable: "YI_AGENT_PLUGIN_BINDING",
            detail: "the effect segment is empty".to_string(),
        });
    }

    Ok(Binding {
        project: PathBuf::from(project),
        grant: parse_number(grant, "grant")?,
        activation: parse_number(activation, "activation")?,
        effect: effect.to_string(),
    })
}

/// 校验插件想要设置的环境项里没有占用的保留名。
///
/// 返回被判为保留项的名字（调用方据此拒绝启动并说明是哪一个），空表表示
/// 全部可用。**不修改**输入：拒绝，而不是过滤。
pub fn rejected_env<'a>(requested: impl IntoIterator<Item = (&'a str, &'a str)>) -> Vec<String> {
    requested
        .into_iter()
        .map(|(name, _)| name)
        .filter(|name| RESERVED_ENV.contains(name))
        .map(str::to_string)
        .collect()
}

/// `workdir` 是否就是受管项目本身（或它的子目录）。
///
/// 用 canonical 路径比较，理由与 `ProjectResolver` 相同：`/a/repo` 与
/// `/a/repo/.` 是同一个项目，字面比较会让同一个项目被判成两个。
///
/// 用途是**诊断**而不是授权：授权只看 binding。当两者不一致时，说明这次
/// 调用是在别的项目目录里跑受管看板，值得明确报出来——但那由调用方决定
/// 是否当作错误，这里只回答一个事实。
pub fn workdir_belongs_to(binding_project: &Path, workdir: &Path) -> bool {
    let project = canonical(binding_project);
    let dir = canonical(workdir);
    dir == project || dir.starts_with(&project)
}

fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let owned: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |name: &str| {
            owned
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.clone())
        }
    }

    #[test]
    fn no_managed_variables_means_standalone_not_an_error() {
        assert_eq!(context_from_env(env(&[])).unwrap(), None);
    }

    #[test]
    fn a_half_set_context_is_refused_rather_than_guessed() {
        let result = context_from_env(env(&[("YI_AGENT_HOME", "/home/x")]));
        assert_eq!(
            result,
            Err(ContextError::Missing("YI_AGENT_PLUGIN_INSTANCE"))
        );
    }

    #[test]
    fn a_complete_context_names_the_project_from_the_binding() {
        let context = context_from_env(env(&[
            ("YI_AGENT_HOME", "/home/x"),
            ("YI_AGENT_PLUGIN_INSTANCE", "local-abc"),
            ("YI_AGENT_PLUGIN_BINDING", "/p/a:3:7:Write"),
        ]))
        .unwrap()
        .expect("a complete context");
        assert_eq!(context.instance_home, PathBuf::from("/home/x"));
        assert_eq!(context.host, "local-abc");
        assert_eq!(context.binding.project, PathBuf::from("/p/a"));
        assert_eq!(context.binding.grant, 3);
        assert_eq!(context.binding.activation, 7);
        assert_eq!(context.binding.effect, "Write");
    }

    #[test]
    fn a_project_path_containing_a_colon_is_not_truncated() {
        // 从右侧切分：项目段可以含 `:`，否则会被截成 `/p`——一个看起来合法、
        // 实际不存在的项目。
        let binding = parse_binding("/p/a:b:1:2:Read").expect("parses");
        assert_eq!(binding.project, PathBuf::from("/p/a:b"));
        assert_eq!(binding.grant, 1);
        assert_eq!(binding.activation, 2);
        assert_eq!(binding.effect, "Read");
    }

    #[test]
    fn a_malformed_binding_names_the_field_that_is_wrong() {
        let error = parse_binding("/p/a:x:7:Write").unwrap_err();
        assert_eq!(
            error,
            ContextError::Malformed {
                variable: "YI_AGENT_PLUGIN_BINDING",
                detail: "grant must be a number, got \"x\"".to_string(),
            }
        );
    }

    #[test]
    fn a_binding_with_too_few_segments_is_refused() {
        assert!(matches!(
            parse_binding("/p/a:1:2"),
            Err(ContextError::Malformed { .. })
        ));
    }

    #[test]
    fn the_plugin_may_not_claim_a_reserved_variable() {
        let rejected = rejected_env([("YI_AGENT_HOME", "/evil"), ("PATH", "/bin")]);
        assert_eq!(rejected, vec!["YI_AGENT_HOME".to_string()]);
    }

    #[test]
    fn ordinary_variables_pass_the_reserved_check() {
        assert!(rejected_env([("SUPERPOWERS_KANBAN_CARD", "card-1")]).is_empty());
    }

    #[test]
    fn the_reserved_list_matches_the_published_contract() {
        let contract = include_str!("../../../../contracts/fixtures/host-v1.json");
        let value: serde_json::Value = serde_json::from_str(contract).expect("fixture parses");
        let declared: Vec<String> = value["reservedEnv"]
            .as_array()
            .expect("reservedEnv is an array")
            .iter()
            .map(|item| item.as_str().expect("names are strings").to_string())
            .collect();
        assert_eq!(declared, RESERVED_ENV);
    }

    #[test]
    fn a_worktree_under_the_project_still_belongs_to_it() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("proj");
        let worktree = project.join(".worktrees/card");
        std::fs::create_dir_all(&worktree).unwrap();
        assert!(workdir_belongs_to(&project, &worktree));
    }

    #[test]
    fn a_sibling_directory_is_a_different_project_even_with_the_same_name() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("x/a");
        let b = dir.path().join("y/a");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        // 同名不是同一项目：按 basename 比较会让两个项目共享授权。
        assert!(!workdir_belongs_to(&a, &b));
    }
}
