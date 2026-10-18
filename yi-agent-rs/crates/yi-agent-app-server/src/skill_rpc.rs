//! `skill/*` RPC：设置页「技能」面板的只读接缝。
//!
//! 与 `plugins/list` 同类：**按项目解析根目录**，缺省回落宿主 workdir。差别在于
//! 它读的是 skill 三层根（项目 / 用户 / 系统），而不是插件清单。
//!
//! 纯函数、无进程内状态：宿主 workdir 与 `home` 都由调用方注入。之所以不让测试
//! 改 `HOME`：cargo 并行跑测试，进程全局变量会让用例互相竞态（与 `model_rpc` 同源
//! 理由）。
//!
//! 权限档位：两个方法都只需 `Observe`——只读、不碰凭据、不落盘，与 `config/read`、
//! `plugins/list` 同档。iOS 设备默认 `Control`，故手机端可用。

use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use yi_agent_skills::{SkillMetadata, SkillScope, SkillsService};

use crate::protocol::RpcError;

/// 项目/用户/系统三层的固定顺序，响应与前端都按它渲染。
const SCOPES: [(&str, SkillScope); 3] = [
    ("project", SkillScope::Project),
    ("user", SkillScope::User),
    ("system", SkillScope::System),
];

/// 生产入口：宿主 workdir 与真实 `home`。测试一律用 [`handle_skill_request_at`]。
pub fn handle_skill_request(
    host_workdir: &Path,
    home: &Path,
    method: &str,
    params: &Value,
) -> Result<Value, RpcError> {
    handle_skill_request_at(host_workdir, home, method, params)
}

/// 可测核心：宿主 workdir 与 `home` 均由调用方注入。
pub fn handle_skill_request_at(
    host_workdir: &Path,
    home: &Path,
    method: &str,
    params: &Value,
) -> Result<Value, RpcError> {
    match method {
        "skill/list" => list(host_workdir, home, params),
        "skill/read" => read(host_workdir, home, params),
        _ => Err(RpcError::method_not_found(method)),
    }
}

/// 三层根。顺序即 `SCOPES` 的顺序。
///
/// 系统根放在用户根**之内**（`.system`）而不是并列，是既有布局
/// （`bootstrap::build_skills_service`）；`discover_skills` 会跳过以点开头的目录，
/// 故用户层不会把系统内置的重复扫一遍。
fn roots(project: &Path, home: &Path) -> Vec<(PathBuf, SkillScope)> {
    let user = home.join(".yi-agent/skills");
    vec![
        (project.join(".yi-agent/skills"), SkillScope::Project),
        (user.clone(), SkillScope::User),
        (user.join(".system"), SkillScope::System),
    ]
}

/// 调用方给了非空 `project` 就用它，否则回落宿主 workdir——语义与 `plugin_scope` 一致，
/// 理由也一致：桌面侧车的 workdir 是 home，设置页必须能把作用域指到它要看的项目。
fn project_scope(host_workdir: &Path, params: &Value) -> PathBuf {
    params
        .get("project")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| host_workdir.to_path_buf())
}

/// canonicalize，失败则原样返回（路径不存在时仍要能比较出「不相等」）。
fn canonical_or_self(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn list(host_workdir: &Path, home: &Path, params: &Value) -> Result<Value, RpcError> {
    let project = project_scope(host_workdir, params);
    let roots = roots(&project, home);

    let project_root = canonical_or_self(&roots[0].0);
    let user_root = canonical_or_self(&roots[1].0);
    // 项目根与用户根是同一个目录 ⇒ 不存在项目层。桌面侧车（workdir = home）且未指定
    // project 时正是这种情况：不合并的话同一 skill 会出现两次，且用户级 skill 会被
    // 错标成项目级。
    let project_coincides_with_user = project_root == user_root;

    // 系统内置 skill 在 list 路径上幂等安装：全新机器、用户还没开过对话时也能列出，
    // 否则「当前系统支持了哪些 skill」答不出来。失败只告警——列表本身仍要可用，
    // 与 `build_skills_service` 的降级策略一致。
    let system_root = &roots[2].0;
    if let Err(error) = yi_agent_skills::install_system_skills(system_root) {
        tracing::warn!("skills: failed to install bundled skills: {error}");
    }

    let active: Vec<(PathBuf, SkillScope)> = roots
        .iter()
        .filter(|(_, scope)| !(project_coincides_with_user && *scope == SkillScope::Project))
        .cloned()
        .collect();

    // 直接构造 `SkillsService` 而不复用启动时那份：这里的根按**调用方的 project**
    // 解析，与进程启动时的 `cfg.workdir` 未必同一个。服务自带缓存，但本函数只调用
    // 一次，故一次性快照即可。
    let service = SkillsService::new(active.clone());
    let discovered = service
        .snapshot()
        .map_err(|error| RpcError::internal(format!("skill discovery failed: {error}")))?;

    let groups: Vec<Value> = active
        .iter()
        .filter_map(|(root, scope)| {
            let name = SCOPES.iter().find(|(_, s)| s == scope).map(|(n, _)| *n)?;
            let mut skills: Vec<&SkillMetadata> = discovered
                .iter()
                .filter(|s| s.scope == *scope && s.path.starts_with(root))
                .collect();
            if skills.is_empty() {
                return None; // 空分组整组不输出
            }
            skills.sort_by(|a, b| (&a.name, &a.path).cmp(&(&b.name, &b.path)));
            Some(json!({
                "scope": name,
                "skills": skills
                    .into_iter()
                    .map(|s| json!({
                        "name": s.name,
                        // 描述可能被 loader 截断（上限 1024B）；面板只做展示。
                        "description": s.description,
                        "path": s.path.to_string_lossy(),
                    }))
                    .collect::<Vec<_>>(),
            }))
        })
        .collect();

    Ok(json!({
        "groups": groups,
        "roots": {
            "project": roots[0].0.to_string_lossy(),
            "user": roots[1].0.to_string_lossy(),
            "system": roots[2].0.to_string_lossy(),
        },
    }))
}

fn read(host_workdir: &Path, home: &Path, params: &Value) -> Result<Value, RpcError> {
    let raw = params
        .get("path")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| RpcError::invalid_params("path required"))?;

    let project = project_scope(host_workdir, params);
    let path = resolve_within(raw, &roots(&project, home))?;

    // 复用既有的正文读取（它会剥掉 frontmatter）。这里只读单个文本文件，故不需要
    // `image/read` 那套「先按元数据设闸再分片」的内存放大防护。
    let service = SkillsService::new(Vec::new());
    let body = service
        .load_skill_body(&path.to_string_lossy())
        .map_err(|error| RpcError::invalid_params(error.to_string()))?;
    Ok(json!({ "body": body }))
}

/// 把调用方给的路径收到合法 skill 根之内。
///
/// 网络面必须自己设限：`SkillsService::load_skill_body` 刻意不做校验（它的既定理由
/// 是「模型可能拼错路径，路径穿越风险与 ReadTool 等同」），那是**进程内工具**的语境。
/// 手法照抄既有先例 `server.rs` 的 `image/read`：`canonicalize` 后要求 `starts_with`
/// 某个根——`canonicalize` 会解掉 `..` 与符号链接，故逃逸一定现形。
fn resolve_within(raw: &str, roots: &[(PathBuf, SkillScope)]) -> Result<PathBuf, RpcError> {
    let candidate = PathBuf::from(raw);
    if candidate.file_name().and_then(|n| n.to_str()) != Some("SKILL.md") {
        return Err(RpcError::invalid_params(
            "path must name a SKILL.md file".to_string(),
        ));
    }
    let canonical = candidate
        .canonicalize()
        .map_err(|error| RpcError::invalid_params(format!("cannot read skill: {error}")))?;
    let allowed = roots
        .iter()
        .any(|(root, _)| canonical.starts_with(canonical_or_self(root)));
    if !allowed {
        return Err(RpcError::invalid_params(
            "path escapes the skill roots".to_string(),
        ));
    }
    Ok(canonical)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// 造一个 skill 目录：`<root>/<name>/SKILL.md`。
    fn make_skill(root: &Path, name: &str, desc: &str) -> PathBuf {
        let dir = root.join(name);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("SKILL.md");
        fs::write(
            &path,
            format!("---\nname: {name}\ndescription: {desc}\n---\n# {name}\n\n正文"),
        )
        .unwrap();
        path
    }

    /// 隔离的「家目录」：`<home>/.yi-agent/skills`（用户层）与 `.system`（系统层）。
    struct Fixture {
        _tmp: TempDir,
        home: PathBuf,
        project: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let tmp = TempDir::new().unwrap();
            let home = tmp.path().join("home");
            let project = tmp.path().join("proj");
            fs::create_dir_all(home.join(".yi-agent/skills")).unwrap();
            fs::create_dir_all(project.join(".yi-agent/skills")).unwrap();
            Self {
                _tmp: tmp,
                home,
                project,
            }
        }

        fn user_root(&self) -> PathBuf {
            self.home.join(".yi-agent/skills")
        }

        fn project_root(&self) -> PathBuf {
            self.project.join(".yi-agent/skills")
        }

        fn system_root(&self) -> PathBuf {
            self.user_root().join(".system")
        }

        /// 装上系统内置 skill（走真实安装路径，与 `skill/list` 同一条）。
        fn install_system(&self) {
            yi_agent_skills::install_system_skills(&self.system_root()).unwrap();
        }

        fn list(&self, project: Option<&Path>) -> Value {
            let params = match project {
                Some(p) => json!({ "project": p.to_string_lossy() }),
                None => json!({}),
            };
            handle_skill_request_at(&self.project, &self.home, "skill/list", &params).unwrap()
        }

        /// 某一层里某个 skill 是否在（按 scope 找组，不按下标——空组会被丢掉，
        /// 下标不稳；系统组还总有内置的 3 个）。
        fn has_skill(&self, project: Option<&Path>, scope: &str, name: &str) -> bool {
            self.list(project)["groups"]
                .as_array()
                .unwrap()
                .iter()
                .find(|g| g["scope"] == scope)
                .map(|g| {
                    g["skills"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|s| s["name"] == name)
                })
                .unwrap_or(false)
        }

        /// 某层是否存在（用于断言「根重合时不输出项目组」）。
        fn has_scope(&self, project: Option<&Path>, scope: &str) -> bool {
            self.list(project)["groups"]
                .as_array()
                .unwrap()
                .iter()
                .any(|g| g["scope"] == scope)
        }
    }

    #[test]
    fn lists_all_three_scopes() {
        let f = Fixture::new();
        make_skill(&f.project_root(), "proj-skill", "项目级。");
        make_skill(&f.user_root(), "user-skill", "用户级。");
        make_skill(&f.system_root(), "sys-skill", "系统级。");

        let listed = f.list(Some(&f.project));
        let scopes: Vec<&str> = listed["groups"]
            .as_array()
            .unwrap()
            .iter()
            .map(|g| g["scope"].as_str().unwrap())
            .collect();
        assert_eq!(scopes, vec!["project", "user", "system"]);
        assert!(f.has_skill(Some(&f.project), "project", "proj-skill"));
        assert!(f.has_skill(Some(&f.project), "user", "user-skill"));
        // 系统组里除自造的这一个，还有 list 路径幂等装上的内置 3 个，故只断言包含。
        assert!(f.has_skill(Some(&f.project), "system", "sys-skill"));
    }

    #[test]
    fn entry_carries_name_description_and_path() {
        let f = Fixture::new();
        let path = make_skill(&f.user_root(), "user-skill", "用户级。");
        let listed = f.list(Some(&f.project));
        // 按 scope 找组，不按下标：空组会被丢掉，下标不稳。
        let entry = listed["groups"]
            .as_array()
            .unwrap()
            .iter()
            .find(|g| g["scope"] == "user")
            .expect("user group")["skills"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["name"] == "user-skill")
            .expect("seeded skill");
        assert_eq!(entry["name"], "user-skill");
        assert_eq!(entry["description"], "用户级。");
        assert_eq!(entry["path"], path.to_string_lossy().as_ref());
        // 正文绝不随列表下发：点开才读（见 `skill/read`）。
        assert!(entry.get("body").is_none());
    }

    #[test]
    fn missing_project_falls_back_to_host_workdir() {
        let f = Fixture::new();
        make_skill(&f.project_root(), "proj-skill", "项目级。");
        // 不传 project → 用宿主 workdir（fixture 里就是 `project`）。
        assert!(f.has_skill(None, "project", "proj-skill"));
    }

    #[test]
    fn system_root_is_not_double_counted_by_the_user_scope() {
        // 依赖 `discover_skills` 跳过隐藏目录（`.system` 以点开头）：这条钉住该依赖，
        // 一旦 discovery 的 `is_hidden` 行为变化，用户层会把系统内置的 3 个重复列一遍。
        let f = Fixture::new();
        f.install_system();
        assert!(f.has_scope(Some(&f.project), "system"));
        assert!(!f.has_skill(Some(&f.project), "user", "skill-creator"));
    }

    #[test]
    fn a_fresh_home_lists_the_bundled_system_skills() {
        // 全新机器上用户还没开过任何对话：`skill/list` 自己必须把内置 skill 装上，
        // 否则「当前系统支持了哪些 skill」这个首要问题答不出来。
        let f = Fixture::new();
        assert!(f.has_skill(Some(&f.project), "system", "skill-creator"));
        assert!(f.has_skill(Some(&f.project), "system", "skill-installer"));
        assert!(f.has_skill(Some(&f.project), "system", "git-diff-review"));
    }

    #[test]
    fn reinstalling_system_skills_is_idempotent() {
        let f = Fixture::new();
        f.install_system();
        let path = f.system_root().join("skill-creator/SKILL.md");
        let first = fs::metadata(&path).unwrap().modified().unwrap();
        // 再列一次：安装按 SHA-256 跳过同内容文件，mtime 不变。
        let _ = f.list(Some(&f.project));
        assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), first);
    }

    #[test]
    fn identical_roots_do_not_produce_a_project_group() {
        // 桌面侧车的 workdir 是 home：未指定 project 时三层里的「项目根」与「用户根」
        // 是同一个目录。此时必须判定「不存在项目层」——否则同一 skill 出现两次，
        // 且用户级 skill 会被错标成项目级。
        let f = Fixture::new();
        make_skill(&f.user_root(), "user-skill", "用户级。");
        let listed = handle_skill_request_at(&f.home, &f.home, "skill/list", &json!({})).unwrap();
        let scopes: Vec<&str> = listed["groups"]
            .as_array()
            .unwrap()
            .iter()
            .map(|g| g["scope"].as_str().unwrap())
            .collect();
        assert!(!scopes.contains(&"project"), "{scopes:?}");
        // 只在用户层出现一次。（系统层仍有内置的 3 个——`skill/list` 会幂等安装它们，
        // 那是另一回事，与这里要钉的重合无关。）
        let names: Vec<&str> = listed["groups"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|g| g["skills"].as_array().unwrap().iter())
            .map(|s| s["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names.iter().filter(|n| **n == "user-skill").count(),
            1,
            "{names:?}"
        );
    }

    #[test]
    fn same_name_in_two_different_roots_stays_in_both_groups() {
        // 两个**不同**目录里的同名 skill 各归各层，不擅自合并——与 `render_catalog`
        // 的既有语义一致（其 `same_name_different_scope_both_listed` 用例钉住了这点）。
        let f = Fixture::new();
        make_skill(&f.project_root(), "dup", "项目级。");
        make_skill(&f.user_root(), "dup", "用户级。");
        assert!(f.has_skill(Some(&f.project), "project", "dup"));
        assert!(f.has_skill(Some(&f.project), "user", "dup"));
    }

    #[test]
    fn roots_are_reported_for_the_help_copy() {
        let f = Fixture::new();
        let listed = f.list(Some(&f.project));
        assert_eq!(
            listed["roots"]["user"],
            f.user_root().to_string_lossy().as_ref()
        );
        assert_eq!(
            listed["roots"]["system"],
            f.system_root().to_string_lossy().as_ref()
        );
        assert_eq!(
            listed["roots"]["project"],
            f.project_root().to_string_lossy().as_ref()
        );
    }

    #[test]
    fn read_returns_the_body_without_frontmatter() {
        let f = Fixture::new();
        let path = make_skill(&f.user_root(), "user-skill", "用户级。");
        let read = handle_skill_request_at(
            &f.project,
            &f.home,
            "skill/read",
            &json!({ "path": path.to_string_lossy() }),
        )
        .unwrap();
        assert_eq!(read["body"], "# user-skill\n\n正文");
    }

    #[test]
    fn read_refuses_a_file_that_is_not_skill_md() {
        let f = Fixture::new();
        let path = make_skill(&f.user_root(), "user-skill", "用户级。");
        let other = path.parent().unwrap().join("notes.md");
        fs::write(&other, "x").unwrap();
        let err = handle_skill_request_at(
            &f.project,
            &f.home,
            "skill/read",
            &json!({ "path": other.to_string_lossy() }),
        )
        .unwrap_err();
        assert_eq!(err.code, -32602);
    }

    #[test]
    fn read_refuses_a_path_outside_every_root() {
        let f = Fixture::new();
        let outside = f.home.join("secrets/SKILL.md");
        fs::create_dir_all(outside.parent().unwrap()).unwrap();
        fs::write(&outside, "---\nname: x\ndescription: y\n---\nsecret").unwrap();
        let err = handle_skill_request_at(
            &f.project,
            &f.home,
            "skill/read",
            &json!({ "path": outside.to_string_lossy() }),
        )
        .unwrap_err();
        assert_eq!(err.code, -32602);
    }

    #[test]
    fn read_refuses_a_dotdot_traversal() {
        let f = Fixture::new();
        make_skill(&f.user_root(), "user-skill", "用户级。");
        let sneaky = f
            .user_root()
            .join("user-skill/../../../../etc/SKILL.md")
            .to_string_lossy()
            .to_string();
        let err = handle_skill_request_at(
            &f.project,
            &f.home,
            "skill/read",
            &json!({ "path": sneaky }),
        )
        .unwrap_err();
        assert_eq!(err.code, -32602);
    }

    #[test]
    fn read_reports_a_missing_file_as_invalid_params() {
        let f = Fixture::new();
        let missing = f.user_root().join("nope/SKILL.md");
        let err = handle_skill_request_at(
            &f.project,
            &f.home,
            "skill/read",
            &json!({ "path": missing.to_string_lossy() }),
        )
        .unwrap_err();
        assert_eq!(err.code, -32602);
    }

    #[test]
    fn unknown_method_is_method_not_found() {
        let f = Fixture::new();
        let err =
            handle_skill_request_at(&f.project, &f.home, "skill/nope", &json!({})).unwrap_err();
        assert_eq!(err.code, -32601);
    }
}
