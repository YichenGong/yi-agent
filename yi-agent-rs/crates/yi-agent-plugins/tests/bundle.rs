//! 有界安全解包的行为测试。
//!
//! 每个攻击都用**真实签名验证通过**的发布跑一遍，证明拒绝来自解包边界本身，
//! 而不是「摘要不符所以失败了」。后者是假绿：把解包检查整个删掉，测试仍会
//! 因为摘要错误而红。
//!
//! 同理，每条「必须拒绝」的断言都配一条正例，否则一个「任何包都失败」的
//! 实现也能让所有负例变绿。

mod common;

use std::os::unix::fs::PermissionsExt;

use common::{Entry, EntryKind, build_archive, file, file_with_mode, signed_release};
use yi_agent_plugins::bundle::{
    InstalledRelease, install_verified, validate_entry_name, verify_installed,
};
use yi_agent_plugins::{ExtractLimits, Manifest, PluginError, VerifiedRelease};

/// 一个最小的合法包。
fn good_entries() -> Vec<Entry> {
    vec![
        file("ui/index.html", "<html>main</html>"),
        file("ui/settings.html", "<html>settings</html>"),
    ]
}

/// 解一个归档到临时目录下的 `installed`。
fn install_with(
    archive: &[u8],
    program_entry: Option<&str>,
    limits: &ExtractLimits,
) -> (Result<InstalledRelease, PluginError>, std::path::PathBuf) {
    let (verified, manifest) = signed_release(archive, program_entry);
    let dir = tempfile::TempDir::new().expect("temp dir");
    // 目录必须在调用期间存活，所以泄漏它——测试进程结束即回收。
    let destination = dir.keep().join("installed");
    let result = install_verified(&verified, &manifest, archive, &destination, limits);
    (result, destination)
}

fn install(archive: &[u8]) -> Result<InstalledRelease, PluginError> {
    install_with(archive, None, &ExtractLimits::default()).0
}

/// 直接改 `VerifiedRelease` 的摘要——模拟「签名元数据与归档字节不符」。
fn install_with_forged_digest(
    archive: &[u8],
    mutate: impl FnOnce(&mut VerifiedRelease),
) -> Result<InstalledRelease, PluginError> {
    let (mut verified, manifest) = signed_release(archive, None);
    mutate(&mut verified);
    let dir = tempfile::TempDir::new().expect("temp dir");
    let destination = dir.keep().join("installed");
    install_verified(
        &verified,
        &manifest,
        archive,
        &destination,
        &ExtractLimits::default(),
    )
}

// ---------------------------------------------------------------------------
// 正例：先证明黄金路径能走通，否则所有负例都可能是"任何包都失败"。
// ---------------------------------------------------------------------------

#[test]
fn a_well_formed_archive_installs() {
    let archive = build_archive(&good_entries());
    let outcome = install(&archive).expect("a clean archive must install");
    assert_eq!(outcome.files_written, 2);
    assert!(outcome.destination.join("ui/index.html").exists());
    assert_eq!(
        std::fs::read_to_string(outcome.destination.join("ui/index.html")).unwrap(),
        "<html>main</html>"
    );
}

// ---------------------------------------------------------------------------
// 八类攻击，每类一条。
// ---------------------------------------------------------------------------

#[test]
fn a_parent_directory_entry_is_rejected() {
    let mut entries = good_entries();
    entries.push(file("../escaped.txt", "pwned"));
    let (result, destination) =
        install_with(&build_archive(&entries), None, &ExtractLimits::default());
    let error = result.expect_err("path traversal must be refused");
    assert_eq!(error.kind(), "Integrity", "got: {error}");
    assert!(!destination.exists());
}

#[test]
fn a_backslash_entry_name_is_rejected() {
    // 反斜杠在 macOS/Linux 上只是普通文件名字符，但它是 Windows 的路径
    // 分隔符，也会让「包内相对路径」与清单里的名字对不上。这里拒绝，
    // 保证一套归档在所有目标上含义一致。
    let mut entries = good_entries();
    entries.push(file("ui\\..\\escaped.txt", "pwned"));
    let error = install(&build_archive(&entries)).expect_err("backslash names must be refused");
    assert_eq!(error.kind(), "Integrity", "got: {error}");
}

#[test]
fn an_absolute_path_entry_is_rejected() {
    let mut entries = good_entries();
    entries.push(file("/tmp/escaped.txt", "pwned"));
    let error = install(&build_archive(&entries)).expect_err("absolute paths must be refused");
    assert_eq!(error.kind(), "Integrity", "got: {error}");
}

#[test]
fn a_symlink_entry_is_rejected() {
    let mut entries = good_entries();
    entries.push(Entry {
        name: "ui/link".to_string(),
        kind: EntryKind::Symlink("/etc/passwd".to_string()),
        mode: 0o777,
    });
    let error = install(&build_archive(&entries)).expect_err("symlinks must be refused");
    assert_eq!(error.kind(), "Integrity", "got: {error}");
}

#[test]
fn a_hardlink_entry_is_rejected() {
    let mut entries = good_entries();
    entries.push(Entry {
        name: "ui/hard".to_string(),
        kind: EntryKind::Hardlink("ui/index.html".to_string()),
        mode: 0o644,
    });
    let error = install(&build_archive(&entries)).expect_err("hardlinks must be refused");
    assert_eq!(error.kind(), "Integrity", "got: {error}");
}

#[test]
fn a_device_entry_is_rejected() {
    let mut entries = good_entries();
    entries.push(Entry {
        name: "ui/dev".to_string(),
        kind: EntryKind::CharDevice,
        mode: 0o666,
    });
    let error = install(&build_archive(&entries)).expect_err("device files must be refused");
    assert_eq!(error.kind(), "Integrity", "got: {error}");
}

#[test]
fn a_duplicate_entry_name_is_rejected() {
    let mut entries = good_entries();
    entries.push(file("ui/index.html", "<html>second</html>"));
    let error = install(&build_archive(&entries)).expect_err("duplicate entries must be refused");
    assert_eq!(error.kind(), "Integrity", "got: {error}");
}

#[test]
fn a_case_collision_is_rejected() {
    // macOS 默认不区分大小写：`Index.html` 与 `index.html` 会落到同一个
    // 文件上，后写的悄悄覆盖前一个。必须按名字比较而不是靠文件系统。
    let mut entries = good_entries();
    entries.push(file("ui/Index.html", "<html>collide</html>"));
    let error = install(&build_archive(&entries)).expect_err("case collisions must be refused");
    assert_eq!(error.kind(), "Integrity", "got: {error}");
}

#[test]
fn a_nul_byte_in_an_entry_name_is_rejected() {
    // NUL 无法经 tar header 表达（名字在那之前就截断了），所以走不了归档
    // 夹具。但它仍可能来自清单或 registry 里存的名字，而 OS 会把它当成
    // 路径终止——"ui/index.html\0.png" 在 C 层看起来是 "ui/index.html"。
    // 因此这条直接测边界函数。
    let error = validate_entry_name("ui/index.html\0.png")
        .expect_err("a NUL byte in a path must be refused");
    assert_eq!(error.kind(), "Integrity", "got: {error}");
    validate_entry_name("ui/index.html").expect("an ordinary relative path is fine");
}

// ---------------------------------------------------------------------------
// 上限。每条都证明「上限收紧之后同一个包被拒」——不收紧的包能装上，
// 所以拒绝确实来自上限而不是包本身有问题。
// ---------------------------------------------------------------------------

#[test]
fn exceeding_the_total_size_limit_is_rejected() {
    let mut entries = good_entries();
    entries.push(file("ui/big.bin", &"x".repeat(4096)));
    let archive = build_archive(&entries);

    // 前提：默认上限下这个包是好包。
    install(&archive).expect("the archive is small enough by default");

    let tight = ExtractLimits {
        max_total_bytes: 1024,
        ..ExtractLimits::default()
    };
    let error = install_with(&archive, None, &tight)
        .0
        .expect_err("exceeding the total size limit must be refused");
    assert_eq!(error.kind(), "Integrity", "got: {error}");
}

#[test]
fn exceeding_the_file_count_limit_is_rejected() {
    let entries: Vec<Entry> = (0..50)
        .map(|i| file(&format!("ui/file{i}.html"), "x"))
        .collect();
    let archive = build_archive(&entries);
    install(&archive).expect("50 files is fine by default");

    let tight = ExtractLimits {
        max_files: 10,
        ..ExtractLimits::default()
    };
    let error = install_with(&archive, None, &tight)
        .0
        .expect_err("exceeding the file count limit must be refused");
    assert_eq!(error.kind(), "Integrity", "got: {error}");
}

#[test]
fn exceeding_the_per_file_limit_is_rejected() {
    let entries = vec![file("ui/huge.bin", &"y".repeat(8192))];
    let archive = build_archive(&entries);
    install(&archive).expect("8 KiB is fine by default");

    let tight = ExtractLimits {
        max_file_bytes: 1024,
        ..ExtractLimits::default()
    };
    let error = install_with(&archive, None, &tight)
        .0
        .expect_err("exceeding the per-file limit must be refused");
    assert_eq!(error.kind(), "Integrity", "got: {error}");
}

#[test]
fn exceeding_the_archive_size_limit_is_rejected() {
    let archive = build_archive(&good_entries());
    install(&archive).expect("the archive is fine by default");

    let tight = ExtractLimits {
        max_archive_bytes: 8,
        ..ExtractLimits::default()
    };
    let error = install_with(&archive, None, &tight)
        .0
        .expect_err("exceeding the archive size limit must be refused");
    assert_eq!(error.kind(), "Integrity", "got: {error}");
}

#[test]
fn limits_cannot_be_raised_by_the_plugin() {
    // 宿主可以降低上限，插件不能提升。这条钉住"上限来自宿主传入"而不是
    // 从归档里读——否则一个恶意归档只要声明更大的上限就能绕过。
    let raised = ExtractLimits {
        max_archive_bytes: 10 * 1024 * 1024 * 1024,
        max_total_bytes: 10 * 1024 * 1024 * 1024,
        max_file_bytes: 10 * 1024 * 1024 * 1024,
        max_files: 10_000_000,
    };
    assert!(
        raised.exceeds_hard_ceiling(),
        "a raised limit must be detectable"
    );
    assert!(!ExtractLimits::default().exceeds_hard_ceiling());

    // 不只是"能被检测到"：真的用它安装必须被拒，且一个字节都不落盘。
    let archive = build_archive(&good_entries());
    let (result, destination) = install_with(&archive, None, &raised);
    let error = result.expect_err("a raised limit must be refused at install time");
    assert_eq!(error.kind(), "PermissionDenied", "got: {error}");
    assert!(!destination.exists());
}

// ---------------------------------------------------------------------------
// 失败不留垃圾。
// ---------------------------------------------------------------------------

#[test]
fn a_rejected_archive_leaves_no_destination_directory() {
    let mut entries = good_entries();
    entries.push(file("../escaped.txt", "pwned"));
    let archive = build_archive(&entries);

    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.keep();
    let destination = root.join("installed");
    let outside = root.join("escaped.txt");

    let (verified, manifest) = signed_release(&archive, None);
    assert!(
        install_verified(
            &verified,
            &manifest,
            &archive,
            &destination,
            &ExtractLimits::default()
        )
        .is_err()
    );
    assert!(
        !outside.exists(),
        "nothing may be written outside the destination"
    );
    assert!(
        !destination.exists(),
        "a failed install must not leave a directory"
    );

    // 失败后的 staging 也必须消失：留一个 `.installed.staging` 就等于
    // 每次失败都往磁盘里堆半个包。
    assert!(
        !root.join(".installed.staging").exists(),
        "a failed install must not leave a staging directory"
    );
}

#[test]
fn a_digest_mismatch_is_rejected_before_anything_is_written() {
    let archive = build_archive(&good_entries());
    let error = install_with_forged_digest(&archive, |verified| {
        verified.release.sha256 = "0".repeat(64);
    })
    .expect_err("a digest mismatch must be refused");
    assert_eq!(error.kind(), "Integrity", "got: {error}");
}

#[test]
fn a_size_mismatch_is_rejected() {
    let archive = build_archive(&good_entries());
    let error = install_with_forged_digest(&archive, |verified| {
        verified.size_bytes += 1;
    })
    .expect_err("a size mismatch must be refused");
    assert_eq!(error.kind(), "Integrity", "got: {error}");
}

// ---------------------------------------------------------------------------
// 权限：归档里的 mode 不采纳。
// ---------------------------------------------------------------------------

#[test]
fn setuid_is_not_preserved() {
    let archive = build_archive(&[file_with_mode("ui/index.html", "<html/>", 0o4755)]);
    let outcome = install(&archive).expect("install");
    let mode = std::fs::metadata(outcome.destination.join("ui/index.html"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o4000, 0, "setuid must be stripped, got {mode:o}");
}

#[test]
fn a_non_entry_file_is_not_executable() {
    // 只有清单里 `program.entry` 声明的那个文件由宿主设置执行位；
    // 其余静态文件必须只读不可执行。
    let archive = build_archive(&[file_with_mode("ui/index.html", "<html/>", 0o755)]);
    let outcome = install(&archive).expect("install");
    let mode = std::fs::metadata(outcome.destination.join("ui/index.html"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(
        mode & 0o111,
        0,
        "static files must not be executable, got {mode:o}"
    );
    assert_eq!(
        mode & 0o222,
        0,
        "installed files must be read-only, got {mode:o}"
    );
}

#[test]
fn a_declared_program_entry_becomes_executable() {
    let archive = build_archive(&[
        file("ui/index.html", "<html/>"),
        file_with_mode("bin/example-counter", "#!/bin/sh\n", 0o644),
    ]);
    let (result, _destination) = install_with(
        &archive,
        Some("bin/example-counter"),
        &ExtractLimits::default(),
    );
    let outcome = result.expect("install");
    assert_eq!(
        outcome.program_entry.as_deref(),
        Some("bin/example-counter")
    );
    let mode = std::fs::metadata(outcome.destination.join("bin/example-counter"))
        .unwrap()
        .permissions()
        .mode();
    assert_ne!(
        mode & 0o100,
        0,
        "the declared program entry must be executable, got {mode:o}"
    );
    assert_eq!(
        mode & 0o022,
        0,
        "the program entry must not be group/other writable"
    );
}

#[test]
fn a_declared_program_entry_missing_from_the_archive_is_rejected() {
    // 清单声明了入口但归档里没有：装上之后宿主会在启动时才发现，
    // 那时已经是一个"安装成功"的坏包。所以要在解包阶段拒绝。
    let archive = build_archive(&good_entries());
    let (result, destination) = install_with(
        &archive,
        Some("bin/example-counter"),
        &ExtractLimits::default(),
    );
    let error = result.expect_err("a missing program entry must be refused");
    assert_eq!(error.kind(), "Integrity", "got: {error}");
    assert!(!destination.exists());
}

// ---------------------------------------------------------------------------
// 安装后完整性。
// ---------------------------------------------------------------------------

#[test]
fn an_unmodified_installation_passes_the_integrity_check() {
    let archive = build_archive(&good_entries());
    let outcome = install(&archive).expect("install");
    verify_installed(&outcome.destination).expect("an untouched installation must verify");
}

#[test]
fn a_tampered_installed_file_fails_the_integrity_check() {
    let archive = build_archive(&good_entries());
    let outcome = install(&archive).expect("install");

    let target = outcome.destination.join("ui/index.html");
    // 安装后文件是 0444。篡改者当然会先解锁再写——测试也要先解锁，
    // 否则这里红的是"写不进去"，而不是"篡改被检出"，等于什么也没证明。
    let mut permissions = std::fs::metadata(&target).unwrap().permissions();
    permissions.set_mode(0o644);
    std::fs::set_permissions(&target, permissions).unwrap();
    std::fs::write(&target, "<html>tampered</html>").unwrap();

    let error = verify_installed(&outcome.destination)
        .expect_err("a modified installation must be detected");
    assert_eq!(error.kind(), "Integrity", "got: {error}");
    assert!(
        error.to_string().contains("index.html"),
        "the error must name the tampered file, got: {error}"
    );
}

#[test]
fn a_payload_that_does_not_match_the_manifest_is_caught_by_the_signed_digest() {
    // 交叉检查：`VerifiedRelease` 与 `Manifest` 是两份独立输入，签名只覆盖
    // 前者。如果宿主编排时把 A 版的 verified 配上 B 版的 manifest，
    // 归档摘要仍会不符，安装被拒。
    let archive_a = build_archive(&good_entries());
    let archive_b = build_archive(&[file("ui/index.html", "<html>other</html>")]);

    let (verified, manifest) = signed_release(&archive_a, None);
    let dir = tempfile::TempDir::new().unwrap();
    let destination = dir.keep().join("installed");
    let error = install_verified(
        &verified,
        &manifest,
        &archive_b,
        &destination,
        &ExtractLimits::default(),
    )
    .expect_err("mismatched archive must be refused");
    assert_eq!(error.kind(), "Integrity", "got: {error}");
}

/// 让未使用的导入在只跑部分测试时也不报警。
#[allow(dead_code)]
fn _type_check(_: Manifest) {}
