//! 旧安装探测：只看用户点名的项目与已知技能根，**不扫盘、不执行**。
//!
//! # 为什么不能递归扫盘
//!
//! 设计 §7.1 的取舍（G3 已批）：探测范围限于"用户提供或实例已登记的项目，
//! 以及已知技能根"。全盘扫描慢、会碰用户没授权的目录，而且对一个想确认
//! "我是不是装着旧版"的问题给出过量信息。
//!
//! # 为什么不执行旧程序
//!
//! 让一个来路不明的二进制跑起来"证明自己是谁"，等于在确认信任之前先给它执行
//! 权限。旧程序能自报任何东西，包括一个假的版本号。所以身份只来自**文件事实**：
//! 路径上有没有、skill 根里有没有 `SKILL.md`、清单里登记了什么。
//!
//! # 旧开关不是新授权
//!
//! 旧安装的 `preferences.json` 里可能写着 `superpowers_kanban: true`。那是旧
//! supervisor 时代"推进开关是开的"，与受管协议下"用户批了 Launch/QuotaWrite/
//! Resident"是两件事（设计 §11）。这里把它当作**线索**报出来，绝不当授权。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// 一个被找到的旧痕迹。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Finding {
    /// 痕迹种类，稳定的短字符串（线协议用）。
    pub kind: FindingKind,
    /// 落在哪（绝对路径，供用户自己去看）。
    pub path: PathBuf,
    /// 关联的项目（如果这条痕迹属于某个项目）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<PathBuf>,
    /// 给用户看的一句话说明。
    pub detail: String,
}

/// 痕迹种类。新增种类只加不改，旧客户端遇到不认识的种类应当忽略而不是报错。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FindingKind {
    /// `PATH` 上的旧可执行文件。**只记录位置，不运行。**
    PathBinary,
    /// 项目里的旧 supervisor 清单。
    ProjectSupervisor,
    /// 用户级 skill 副本。
    UserSkill,
    /// 项目级 skill 副本。
    ProjectSkill,
    /// 宿主通用常驻登记里提到这个项目。
    ResidentEntry,
    /// 旧项目偏好里的推进开关。
    LegacySwitch,
    /// 旧看板数据目录（有 `board.json`，可被接管）。
    BoardData,
}

/// 一次探测的结果。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    pub findings: Vec<Finding>,
    /// 被拒绝探测的输入（例如一个不存在、因而无法确认归属的项目路径）。
    pub rejected_inputs: Vec<String>,
}

impl Report {
    pub fn has_board_data(&self) -> bool {
        self.findings
            .iter()
            .any(|finding| finding.kind == FindingKind::BoardData)
    }

    /// 能不能被接管：至少有一个可搬的看板数据目录。
    pub fn adoptable(&self) -> bool {
        self.has_board_data()
    }
}

/// 一个插件的旧安装"长什么样"。
///
/// 由**已验证包的清单**提供，不由这个 crate 硬编码：本模块是通用能力，认识
/// 某个具体插件的名字就违背了它存在的理由（加第二个插件要改这里 = 通用性
/// 是假的）。插件在清单里自述自己的旧二进制名、旧数据目录与旧开关键，
/// 宿主按这份自述去查。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LegacyRules {
    /// 旧可执行文件名（不带目录）。只用于在 `PATH` 的各段里拼路径。
    pub binaries: Vec<String>,
    /// 旧 supervisor 清单文件名，例如 `superpowers-kanban.json`。
    pub supervisor: Option<String>,
    /// 旧状态目录（相对项目根），例如 `.yi-agent/superpowers-kanban`。
    ///
    /// 受管模式与旧模式在这一点上通常相同，所以它同时描述"旧数据在哪"与
    /// "接管后的受管状态在哪"。
    pub state_dirs: Vec<String>,
    /// 旧项目偏好文件（相对项目根），例如 `.yi-agent/preferences.json`。
    pub preferences: Option<String>,
    /// 旧偏好里那个功能开关的键名。
    pub switch_key: Option<String>,
    /// 用户级 / 项目级 skill 名（`skills/<name>/SKILL.md`）。
    pub skill: Option<String>,
}

impl LegacyRules {
    /// 从一个插件 id 推导出"最常见的旧布局"。**只是兜底**，不是身份判据。
    ///
    /// 用在调用方没有清单可读的场合（例如实例里还没装任何版本）。真正的
    /// 探测应当传清单解析出来的 [`LegacyRules`]——那样加插件不用改代码。
    pub fn for_plugin(plugin: &crate::PluginId) -> LegacyRules {
        let name = plugin.as_str().to_string();
        LegacyRules {
            binaries: vec![name.clone()],
            supervisor: Some(format!("{name}.json")),
            state_dirs: vec![format!(".yi-agent/{name}")],
            preferences: Some(".yi-agent/preferences.json".to_string()),
            switch_key: Some(name.replace('-', "_")),
            skill: Some(name),
        }
    }
}

/// 探测的输入。**全部由调用方显式给出**，没有"默认扫一遍"的分支。
#[derive(Debug, Clone)]
pub struct Probe<'a> {
    /// 用户点名或实例已登记的项目根。
    pub projects: Vec<PathBuf>,
    /// 实例的通用目录（`$HOME/.yi-agent`）：常驻登记与用户级 skill 在它下面。
    pub instance_dir: &'a Path,
    /// `PATH` 的内容。注入便于测试，不读真实环境。
    pub path: &'a str,
    /// 这个插件的旧安装长什么样（来自已验证包的清单）。
    pub rules: &'a LegacyRules,
}

/// 执行一次探测。**只读**：不执行、不删除、不修改任何东西。
pub fn probe(probe: &Probe<'_>) -> Report {
    let mut report = Report::default();
    let rules = probe.rules;

    // 1. PATH：只看有没有那个名字，不 `stat` 出可执行位就宣称"能跑"。
    for dir in probe.path.split(':').filter(|dir| !dir.is_empty()) {
        for name in &rules.binaries {
            let candidate = Path::new(dir).join(name);
            if candidate.is_file() {
                report.findings.push(Finding {
                    kind: FindingKind::PathBinary,
                    path: candidate,
                    project: None,
                    detail: format!(
                        "`{name}` is on PATH. It will not be run to identify itself; \
                         once a managed release is active this copy is no longer maintained."
                    ),
                });
            }
        }
    }

    // 2. 逐个项目：supervisor 清单、项目级 skill、回退边界。
    for project in &probe.projects {
        if !project.is_dir() {
            // 不存在就无法确认归属——报出来，但不去猜一个相近路径。
            report
                .rejected_inputs
                .push(format!("{}: not a directory", project.display()));
            continue;
        }
        if let Some(name) = &rules.supervisor {
            let supervisor = project.join(".yi-agent").join("supervisors").join(name);
            if supervisor.is_file() {
                report.findings.push(Finding {
                    kind: FindingKind::ProjectSupervisor,
                    path: supervisor,
                    project: Some(project.clone()),
                    detail: "the project still registers a legacy supervisor for this plugin"
                        .to_string(),
                });
            }
        }
        if let Some(skill) = &rules.skill {
            let path = project
                .join(".yi-agent")
                .join("skills")
                .join(skill)
                .join("SKILL.md");
            if path.is_file() {
                report.findings.push(Finding {
                    kind: FindingKind::ProjectSkill,
                    path,
                    project: Some(project.clone()),
                    detail: "a project-local copy of the plugin's skill exists".to_string(),
                });
            }
        }
        // 旧开关：只作为线索。读不懂就不报，别把一次解析失败说成"开关关着"。
        if let (Some(preferences), Some(key)) = (&rules.preferences, &rules.switch_key) {
            let path = project.join(preferences);
            if let Some(detail) = legacy_switch(&path, key) {
                report.findings.push(Finding {
                    kind: FindingKind::LegacySwitch,
                    path,
                    project: Some(project.clone()),
                    detail,
                });
            }
        }
        // 旧数据：这是唯一"可搬"的东西。
        for relative in &rules.state_dirs {
            let candidate = project.join(relative);
            if candidate.join("board.json").is_file() {
                report.findings.push(Finding {
                    kind: FindingKind::BoardData,
                    path: candidate,
                    project: Some(project.clone()),
                    detail: "a legacy state dir with board.json exists and can be adopted"
                        .to_string(),
                });
            }
        }
    }

    // 3. 已知技能根（用户级）。
    if let Some(skill) = &rules.skill {
        let user_skill = probe
            .instance_dir
            .join("skills")
            .join(skill)
            .join("SKILL.md");
        if user_skill.is_file() {
            report.findings.push(Finding {
                kind: FindingKind::UserSkill,
                path: user_skill,
                project: None,
                detail: "a user-level copy of the plugin's skill exists; the managed package \
                         ships its own and does not copy into this root"
                    .to_string(),
            });
        }
    }

    // 4. 常驻登记：宿主与插件共享的那份文件契约。
    let resident = probe.instance_dir.join("resident-daemons.json");
    if let Some(projects) = resident_projects(&resident) {
        for project in projects {
            report.findings.push(Finding {
                kind: FindingKind::ResidentEntry,
                path: resident.clone(),
                project: Some(project),
                detail: "the resident registry still asks for a board daemon in this project"
                    .to_string(),
            });
        }
    }

    report
}

/// 读旧偏好里的推进开关。返回一句可展示的说明，`None` 表示没有可说的。
///
/// **只报告**，不把 `true` 解释成授权。
fn legacy_switch(path: &Path, key: &str) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    let on = value.get(key)?.as_bool()?;
    Some(if on {
        "the legacy board switch is on. That is not a grant in the managed protocol: \
         launching tasks, changing quotas and running resident all need fresh approval."
            .to_string()
    } else {
        "the legacy board switch is off".to_string()
    })
}

/// 读常驻登记里登记过的项目。
///
/// 与插件侧 `resident.rs` 同一份文件契约，但这里只需要"有谁"，并且**读不懂就
/// 返回 `None`**：一份坏登记表不该让整个探测失败，也不该被当成空表（那会漏报）。
fn resident_projects(path: &Path) -> Option<Vec<PathBuf>> {
    let text = std::fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    let projects = value.get("projects")?.as_array()?;
    Some(
        projects
            .iter()
            .filter_map(|entry| entry.get("project")?.as_str())
            .map(PathBuf::from)
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一份"完整旧安装"：PATH 上二进制、supervisor 清单、skill 副本、
    /// 卡片数据、常驻引用、旧开关。
    struct Legacy {
        root: tempfile::TempDir,
        project: PathBuf,
        instance: PathBuf,
        bin_dir: PathBuf,
    }

    fn legacy_install() -> Legacy {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("proj");
        let instance = root.path().join("home/.yi-agent");
        let bin_dir = root.path().join("bin");

        std::fs::create_dir_all(project.join(".yi-agent/supervisors")).unwrap();
        std::fs::create_dir_all(project.join(".yi-agent/skills/superpowers-kanban")).unwrap();
        std::fs::create_dir_all(project.join(".yi-agent/superpowers-kanban")).unwrap();
        std::fs::create_dir_all(&instance).unwrap();
        std::fs::create_dir_all(&bin_dir).unwrap();

        std::fs::write(bin_dir.join("superpowers-kanban"), "#!/bin/sh\n").unwrap();
        std::fs::write(
            project.join(".yi-agent/supervisors/superpowers-kanban.json"),
            r#"{"name":"superpowers-kanban"}"#,
        )
        .unwrap();
        std::fs::write(
            project.join(".yi-agent/skills/superpowers-kanban/SKILL.md"),
            "---\nname: superpowers-kanban\n---\n",
        )
        .unwrap();
        std::fs::write(
            project.join(".yi-agent/superpowers-kanban/board.json"),
            r#"{"cards":[],"next_order":0}"#,
        )
        .unwrap();
        std::fs::write(
            project.join(".yi-agent/preferences.json"),
            r#"{"superpowers_kanban":true}"#,
        )
        .unwrap();
        std::fs::write(
            instance.join("resident-daemons.json"),
            format!(
                r#"{{"projects":[{{"project":"{}","required_by":["superpowers-kanban"]}}]}}"#,
                project.display()
            ),
        )
        .unwrap();

        Legacy {
            root,
            project,
            instance,
            bin_dir,
        }
    }

    fn rules() -> LegacyRules {
        LegacyRules {
            binaries: vec![
                "superpowers-kanban".to_string(),
                "superpowers-kanban-runner".to_string(),
            ],
            supervisor: Some("superpowers-kanban.json".to_string()),
            state_dirs: vec![
                ".yi-agent/superpowers-kanban".to_string(),
                ".superpowers-kanban".to_string(),
            ],
            preferences: Some(".yi-agent/preferences.json".to_string()),
            switch_key: Some("superpowers_kanban".to_string()),
            skill: Some("superpowers-kanban".to_string()),
        }
    }

    fn probe_of<'a>(legacy: &'a Legacy, path: &'a str, rules: &'a LegacyRules) -> Probe<'a> {
        Probe {
            projects: vec![legacy.project.clone()],
            instance_dir: &legacy.instance,
            path,
            rules,
        }
    }

    #[test]
    fn a_complete_legacy_install_is_reported_field_by_field() {
        let legacy = legacy_install();
        let report = probe(&probe_of(
            &legacy,
            legacy.bin_dir.to_str().unwrap(),
            &rules(),
        ));

        let kinds: Vec<FindingKind> = report.findings.iter().map(|f| f.kind).collect();
        for expected in [
            FindingKind::PathBinary,
            FindingKind::ProjectSupervisor,
            FindingKind::ProjectSkill,
            FindingKind::LegacySwitch,
            FindingKind::BoardData,
            FindingKind::ResidentEntry,
        ] {
            assert!(
                kinds.contains(&expected),
                "{expected:?} missing from {kinds:?}"
            );
        }
        assert!(report.adoptable());
        assert!(report.rejected_inputs.is_empty());
    }

    #[test]
    fn probing_never_executes_anything() {
        // 一个会留下证据的可执行文件：跑过就会创建 `ran`。
        let legacy = legacy_install();
        let marker = legacy.root.path().join("ran");
        let bin = legacy.bin_dir.join("superpowers-kanban");
        std::fs::write(&bin, format!("#!/bin/sh\ntouch {}\n", marker.display())).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();

        probe(&probe_of(
            &legacy,
            legacy.bin_dir.to_str().unwrap(),
            &rules(),
        ));

        assert!(!marker.exists(), "the probe must not run the legacy binary");
    }

    #[test]
    fn probing_does_not_delete_or_modify_anything() {
        let legacy = legacy_install();
        let before = std::fs::read(
            legacy
                .project
                .join(".yi-agent/superpowers-kanban/board.json"),
        )
        .unwrap();
        let supervisor = legacy
            .project
            .join(".yi-agent/supervisors/superpowers-kanban.json");
        let supervisor_before = std::fs::read(&supervisor).unwrap();

        probe(&probe_of(
            &legacy,
            legacy.bin_dir.to_str().unwrap(),
            &rules(),
        ));

        assert_eq!(
            std::fs::read(
                legacy
                    .project
                    .join(".yi-agent/superpowers-kanban/board.json")
            )
            .unwrap(),
            before
        );
        assert!(supervisor.is_file());
        assert_eq!(std::fs::read(&supervisor).unwrap(), supervisor_before);
    }

    #[test]
    fn the_legacy_switch_is_reported_as_a_clue_not_a_grant() {
        let legacy = legacy_install();
        let rules = rules();
        let report = probe(&probe_of(&legacy, "", &rules));
        let switch = report
            .findings
            .iter()
            .find(|f| f.kind == FindingKind::LegacySwitch)
            .expect("the switch should be reported");
        assert!(switch.detail.contains("not a grant"), "{}", switch.detail);
    }

    #[test]
    fn a_project_that_does_not_exist_is_rejected_not_guessed() {
        let legacy = legacy_install();
        let rules = rules();
        let mut probe = probe_of(&legacy, "", &rules);
        probe.projects = vec![legacy.root.path().join("typo-project")];

        let report = probe_report(&probe);

        // 拼错的项目路径一个痕迹都不能产出——不去找"相近的那个"。
        assert!(
            report.findings.iter().all(|finding| !finding
                .path
                .starts_with(legacy.root.path().join("typo-project"))),
            "{:?}",
            report.findings
        );
        assert_eq!(report.rejected_inputs.len(), 1);
        assert!(report.rejected_inputs[0].contains("typo-project"));
        // 常驻登记是**实例已登记**的来源（设计 §8.1 允许），所以真实项目那条
        // 仍会被报出来；它不因为本次只点名了拼错路径就消失。
        assert!(
            report
                .findings
                .iter()
                .any(|finding| finding.kind == FindingKind::ResidentEntry)
        );
    }

    fn probe_report(probe: &Probe<'_>) -> Report {
        // 与 `probe` 同名冲突会遮蔽函数，所以包一层（测试里更清楚）。
        super::probe(probe)
    }

    #[test]
    fn an_empty_machine_reports_nothing_and_is_not_adoptable() {
        let root = tempfile::tempdir().unwrap();
        let instance = root.path().join(".yi-agent");
        std::fs::create_dir_all(&instance).unwrap();
        let project = root.path().join("proj");
        std::fs::create_dir_all(&project).unwrap();
        let rules = rules();

        let report = probe(&Probe {
            projects: vec![project],
            instance_dir: &instance,
            path: "",
            rules: &rules,
        });

        assert!(report.findings.is_empty(), "{:?}", report.findings);
        assert!(!report.adoptable());
    }

    #[test]
    fn a_project_without_board_data_is_not_adoptable() {
        let root = tempfile::tempdir().unwrap();
        let instance = root.path().join(".yi-agent");
        std::fs::create_dir_all(&instance).unwrap();
        let project = root.path().join("proj");
        std::fs::create_dir_all(project.join(".superpowers-kanban")).unwrap();
        let rules = rules();

        let report = probe(&Probe {
            projects: vec![project],
            instance_dir: &instance,
            path: "",
            rules: &rules,
        });

        assert!(!report.adoptable(), "{:?}", report.findings);
    }

    #[test]
    fn a_corrupt_resident_registry_is_ignored_rather_than_read_as_empty() {
        let root = tempfile::tempdir().unwrap();
        let instance = root.path().join(".yi-agent");
        std::fs::create_dir_all(&instance).unwrap();
        std::fs::write(instance.join("resident-daemons.json"), "{broken").unwrap();
        let project = root.path().join("proj");
        std::fs::create_dir_all(&project).unwrap();
        let rules = rules();

        // 不 panic、不误报：坏登记表既不能让探测失败，也不能被当作"没有登记"而
        // 让上层据此宣布"干净"——它只是这一项没有结论。
        let report = probe(&Probe {
            projects: vec![project],
            instance_dir: &instance,
            path: "",
            rules: &rules,
        });
        assert!(report.findings.is_empty());
    }
}
