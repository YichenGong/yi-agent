//! 旧安装探测的集成测试：一台"什么都装过"的机器上到底会报出什么。
//!
//! # 这些测试在防什么
//!
//! 探测最容易犯三个错误，每一个都会让用户做出错误决定：
//!
//! 1. **把旧程序跑起来"问它是谁"**——在确认信任之前先给了它执行权限，
//!    而且它自称的版本号毫无意义。这里的断言用一个会留下证据的假二进制。
//! 2. **顺手改东西**——一个叫"检测"的动作删了用户的 skill 或搬走了数据。
//! 3. **把旧开关当成新授权**——旧 `superpowers_kanban: true` 是"推进开关
//!    开着"，与受管协议里的 Launch / QuotaWrite / Resident 是两回事。
//!
//! 还有一个边界：**不扫盘**。只查显式项目 + 常驻登记 + 已知技能根。

use std::path::{Path, PathBuf};

use yi_agent_plugins::legacy::probe;
use yi_agent_plugins::legacy::{FindingKind, LegacyRules, Probe};

struct Machine {
    root: tempfile::TempDir,
    project: PathBuf,
    home: PathBuf,
    bin: PathBuf,
}

impl Machine {
    fn path(&self) -> &Path {
        &self.project
    }

    fn probe<'a>(&'a self, path: &'a str, rules: &'a LegacyRules) -> Probe<'a> {
        Probe {
            projects: vec![self.project.clone()],
            instance_dir: &self.home,
            path,
            rules,
        }
    }

    fn kinds(&self, findings: &[yi_agent_plugins::Finding]) -> Vec<FindingKind> {
        let repo = self.root.path().to_path_buf();
        findings
            .iter()
            .filter(|finding| finding.path.starts_with(&repo))
            .map(|finding| finding.kind)
            .collect()
    }
}

/// 看板插件的旧布局。真实路径里这份规则来自已验证包的清单；测试里直接写死，
/// 因为夹具本身就是"一份已知的清单"。
fn rules() -> LegacyRules {
    LegacyRules {
        binaries: vec![
            "superpowers-kanban".to_string(),
            "superpowers-kanban-runner".to_string(),
        ],
        supervisor: Some("superpowers-kanban.json".to_string()),
        state_dirs: vec![".yi-agent/superpowers-kanban".to_string()],
        preferences: Some(".yi-agent/preferences.json".to_string()),
        switch_key: Some("superpowers_kanban".to_string()),
        skill: Some("superpowers-kanban".to_string()),
    }
}

/// 一台"装过完整旧版"的机器：PATH 上二进制、supervisor 登记、两层 skill 副本、
/// 卡片数据、常驻引用、旧开关。
fn installed_machine() -> Machine {
    let root = tempfile::tempdir().unwrap();
    let project = root.path().join("proj");
    let home = root.path().join("home/.yi-agent");
    let bin = root.path().join("bin");
    for dir in [
        project.join(".yi-agent/supervisors"),
        project.join(".yi-agent/skills/superpowers-kanban"),
        project.join(".yi-agent/superpowers-kanban"),
        home.join("skills/superpowers-kanban"),
        bin.clone(),
    ] {
        std::fs::create_dir_all(dir).unwrap();
    }

    std::fs::write(bin.join("superpowers-kanban"), "#!/bin/sh\n").unwrap();
    std::fs::write(
        project.join(".yi-agent/supervisors/superpowers-kanban.json"),
        r#"{"name":"superpowers-kanban","argv":["superpowers-kanban","run"]}"#,
    )
    .unwrap();
    std::fs::write(
        project.join(".yi-agent/skills/superpowers-kanban/SKILL.md"),
        "---\nname: superpowers-kanban\n---\n# 项目副本\n",
    )
    .unwrap();
    std::fs::write(
        home.join("skills/superpowers-kanban/SKILL.md"),
        "---\nname: superpowers-kanban\n---\n# 用户副本\n",
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
        home.join("resident-daemons.json"),
        format!(
            r#"{{"projects":[{{"project":"{}","required_by":["superpowers-kanban"]}}]}}"#,
            project.display()
        ),
    )
    .unwrap();

    Machine {
        root,
        project,
        home,
        bin,
    }
}

#[test]
fn a_full_legacy_install_is_described_item_by_item_with_its_project() {
    let machine = installed_machine();
    let report = probe(&machine.probe(machine.bin.to_str().unwrap(), &rules()));

    let kinds = machine.kinds(&report.findings);
    for expected in [
        FindingKind::PathBinary,
        FindingKind::ProjectSupervisor,
        FindingKind::ProjectSkill,
        FindingKind::UserSkill,
        FindingKind::ResidentEntry,
        FindingKind::LegacySwitch,
        FindingKind::BoardData,
    ] {
        assert!(
            kinds.contains(&expected),
            "{expected:?} missing from {kinds:?}"
        );
    }

    // 每条痕迹都要说清"它在哪、属于哪个项目、为什么报出来"。
    for finding in &report.findings {
        assert!(finding.path.is_absolute(), "{finding:?}");
        assert!(!finding.detail.trim().is_empty(), "{finding:?}");
    }
    // 项目级痕迹必须能回答"这是哪个项目的"，否则用户无法决定要不要接管。
    for finding in &report.findings {
        if matches!(
            finding.kind,
            FindingKind::ProjectSupervisor | FindingKind::ProjectSkill | FindingKind::BoardData
        ) {
            assert_eq!(
                finding.project.as_deref(),
                Some(machine.path()),
                "{finding:?}"
            );
        }
    }
    assert!(report.adoptable());
}

#[test]
fn the_legacy_binary_is_never_run_to_identify_itself() {
    let machine = installed_machine();
    let marker = machine.root.path().join("was-run");
    let binary = machine.bin.join("superpowers-kanban");
    std::fs::write(
        &binary,
        format!(
            "#!/bin/sh\ntouch {}\necho '{{\"version\":\"9.9.9\"}}'\n",
            marker.display()
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();

    let report = probe(&machine.probe(machine.bin.to_str().unwrap(), &rules()));

    assert!(!marker.exists(), "probing executed the legacy binary");
    // 报出来的是"它在这儿"，不是它自称的任何东西。
    let finding = report
        .findings
        .iter()
        .find(|f| f.kind == FindingKind::PathBinary)
        .expect("the binary should be reported");
    assert!(!finding.detail.contains("9.9.9"), "{}", finding.detail);
}

#[test]
fn probing_changes_nothing_on_disk() {
    let machine = installed_machine();
    let snapshot = |root: &Path| -> Vec<(PathBuf, Vec<u8>)> {
        let mut files = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else {
                    files.push((path.clone(), std::fs::read(&path).unwrap_or_default()));
                }
            }
        }
        files.sort();
        files
    };
    let before = snapshot(machine.root.path());

    probe(&machine.probe(machine.bin.to_str().unwrap(), &rules()));

    assert_eq!(snapshot(machine.root.path()), before);
}

#[test]
fn user_skills_and_the_old_binary_are_reported_but_never_removed() {
    let machine = installed_machine();
    let user_skill = machine.home.join("skills/superpowers-kanban/SKILL.md");
    let binary = machine.bin.join("superpowers-kanban");

    probe(&machine.probe(machine.bin.to_str().unwrap(), &rules()));

    // §8.3：用户自定义 skill 与 PATH 上用户装的旧程序"不动"。
    assert!(user_skill.is_file());
    assert!(binary.is_file());
}

#[test]
fn the_old_switch_is_a_clue_and_says_so() {
    let machine = installed_machine();
    let report = probe(&machine.probe("", &rules()));

    let switch = report
        .findings
        .iter()
        .find(|f| f.kind == FindingKind::LegacySwitch)
        .expect("the switch should be reported");
    // "不是授权"这句话必须在给用户看的文本里，而不是只活在注释里。
    assert!(switch.detail.contains("not a grant"), "{}", switch.detail);
}

#[test]
fn a_machine_with_no_legacy_install_reports_nothing() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join(".yi-agent");
    let project = root.path().join("proj");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&project).unwrap();

    let rules = rules();
    let report = probe(&Probe {
        projects: vec![project],
        instance_dir: &home,
        path: "",
        rules: &rules,
    });

    assert!(report.findings.is_empty(), "{:?}", report.findings);
    assert!(report.rejected_inputs.is_empty());
    assert!(!report.adoptable());
}

#[test]
fn scanning_is_bounded_to_named_projects() {
    // 一个"隔壁项目"也装着旧版，但没人点名、也没登记：不该出现在报告里。
    let machine = installed_machine();
    let neighbour = machine.root.path().join("neighbour");
    std::fs::create_dir_all(neighbour.join(".yi-agent/superpowers-kanban")).unwrap();
    std::fs::write(
        neighbour.join(".yi-agent/superpowers-kanban/board.json"),
        r#"{"cards":[],"next_order":0}"#,
    )
    .unwrap();

    let report = probe(&machine.probe(machine.bin.to_str().unwrap(), &rules()));

    assert!(
        report
            .findings
            .iter()
            .all(|finding| !finding.path.starts_with(&neighbour)),
        "{:?}",
        report.findings
    );
}

#[test]
fn a_path_that_cannot_be_confirmed_is_rejected_rather_than_replaced() {
    let machine = installed_machine();
    let typo = machine.root.path().join("proj-typo");

    let rules = rules();
    let report = probe(&Probe {
        projects: vec![typo],
        instance_dir: &machine.home,
        path: "",
        rules: &rules,
    });

    assert_eq!(report.rejected_inputs.len(), 1, "{report:?}");
    assert!(report.rejected_inputs[0].contains("proj-typo"));
}
