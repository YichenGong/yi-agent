//! 把插件目录打成**可复现**的发布包，并签出元数据。
//!
//! # 为什么确定性是硬要求，而不是"锦上添花"
//!
//! 同一个输入打两次得到不同字节，就没有人能回答"用户装的到底是哪个包"：
//! 摘要每次都不一样，回滚、比对、缓存、增量更新全部失效。所以这里对
//! 进归档的每一个可变因素都显式钉死：
//!
//! - 条目顺序按路径排序（不依赖文件系统返回顺序）；
//! - 条目的 mtime 固定为 [`FIXED_MTIME`]（`tar::Builder` 默认写当前时间）；
//! - uid/gid 固定为 0，uname/gname 清空；
//! - 权限只看"是不是可执行"，其余一律 0644/0755（不采纳 umask）；
//! - gzip 头部不写 mtime、不写文件名。
//!
//! 归档与签名用的是**宿主同一套**契约：清单由 [`Manifest::parse`] 校验，
//! 成员路径由 [`bundle::validate_entry_name`] 判定，签名 payload 由
//! [`trust::encode_release_payload`] 编码。打包器若自带一份规则，两边就会在
//! 某个边界上分叉，而那个边界只有用户装的时候才会撞上。
//!
//! # 这里为什么**不**签名
//!
//! 本 crate 被宿主链接进主程序。若签名能力也在这里，宿主二进制就带着
//! "能签发一个可安装插件"的代码，而宿主永远只需要**验证**。签名放在发布工具
//! （`src/bin/plugin-pack.rs`）里，那个二进制不进主程序。

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::bundle::validate_entry_name;
use crate::error::PluginError;
use crate::manifest::Manifest;

/// 归档里所有条目共用的 mtime。取 1980-01-01：zip 能表示的最早时间，
/// 也让"这个时间戳是谁的"一眼可辨——它不属于任何文件。
pub const FIXED_MTIME: u64 = 315_532_800;

/// 包根清单的文件名。宿主安装时只在这里找 `plugin.json`。
pub const PACKAGE_MANIFEST: &str = "plugin.json";

/// 一次打包的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packed {
    /// gzip 压缩后的 tar 字节。
    pub archive: Vec<u8>,
    pub sha256: String,
    pub plugin: String,
    pub version: String,
    pub target: String,
    pub files: usize,
}

/// 把 `directory` 整个打成确定性 tar.gz，清单取自目录根的 `plugin.json`。
///
/// 清单里的 `views[].digest` 与 `program.digest` 在打包时**重算**并与归档
/// 内容核对：清单说 A、包里是 B 时立刻失败，而不是等宿主安装到一半。
pub fn pack_directory(directory: &Path) -> Result<Packed, PluginError> {
    let manifest_path = directory.join(PACKAGE_MANIFEST);
    let manifest_bytes = std::fs::read(&manifest_path)
        .map_err(|e| PluginError::Io(format!("could not read {}: {e}", manifest_path.display())))?;
    let manifest = Manifest::parse(&manifest_bytes)?;

    let mut files = collect_files(directory)?;

    // 清单必须在包根，且只有一份：多份会让"宿主读哪一份"取决于解包顺序。
    if !files.contains_key(PACKAGE_MANIFEST) {
        return Err(PluginError::InvalidManifest(format!(
            "the package has no `{PACKAGE_MANIFEST}` at its root"
        )));
    }

    // 声明过的入口必须在包里真的存在，否则宿主装完才发现点开是空的。
    for view in &manifest.views {
        let bytes = files.get(&view.entry).ok_or_else(|| {
            PluginError::Integrity(format!(
                "views[{}].entry `{}` is not in the package",
                view.id, view.entry
            ))
        })?;
        let actual = hex(Sha256::digest(bytes));
        if actual != view.digest {
            return Err(PluginError::Integrity(format!(
                "views[{}].entry `{}` hashes to {actual}, but the manifest records {}; \
                 regenerate the manifest instead of shipping a stale digest",
                view.id, view.entry, view.digest
            )));
        }
    }
    if let Some(program) = &manifest.program {
        let bytes = files.get(&program.entry).ok_or_else(|| {
            PluginError::Integrity(format!(
                "program.entry `{}` is not in the package",
                program.entry
            ))
        })?;
        let actual = hex(Sha256::digest(bytes));
        if actual != program.digest {
            return Err(PluginError::Integrity(format!(
                "program.entry `{}` hashes to {actual}, but the manifest records {}",
                program.entry, program.digest
            )));
        }
    }
    for skill in &manifest.skills {
        // skill 是目录：包内必须有以它开头的若干文件，否则技能装了个空壳。
        let prefix = format!("{}/", skill.path);
        if !files.keys().any(|name| name.starts_with(&prefix)) {
            return Err(PluginError::Integrity(format!(
                "skills[{}].path `{}` has no files in the package",
                skill.name, skill.path
            )));
        }
    }

    // 清单本身可能因为重算而变——这里不改写它。打包器只验证，不生成：
    // 生成规则在 `scripts/plugins/kanban-manifest.py`，一处就够。
    files.insert(PACKAGE_MANIFEST.to_string(), manifest_bytes);

    let archive = build_archive(&files)?;
    Ok(Packed {
        sha256: hex(Sha256::digest(&archive)),
        archive,
        plugin: manifest.plugin.as_str().to_string(),
        version: manifest.version.clone(),
        target: manifest.target.clone(),
        files: files.len(),
    })
}

/// 目录 → 包内路径 → 字节。路径用 `/` 分隔（不随平台变）。
fn collect_files(directory: &Path) -> Result<BTreeMap<String, Vec<u8>>, PluginError> {
    let mut out = BTreeMap::new();
    walk(directory, directory, &mut out)?;
    Ok(out)
}

fn walk(
    root: &Path,
    current: &Path,
    out: &mut BTreeMap<String, Vec<u8>>,
) -> Result<(), PluginError> {
    let entries = std::fs::read_dir(current)
        .map_err(|e| PluginError::Io(format!("could not list {}: {e}", current.display())))?;
    for entry in entries {
        let entry = entry.map_err(|e| {
            PluginError::Io(format!(
                "could not read an entry of {}: {e}",
                current.display()
            ))
        })?;
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        // 归档里不该出现的东西：VCS 元数据、构建产物与编辑器临时文件。
        // 它们会随机器不同而不同，正是确定性最直接的敌人。
        //
        // `target/` 尤其不能漏：它包含本机绝对路径派生的指纹目录名，既让
        // 同一输入产出不同字节，也会撑爆 tar 的单段路径上限。
        if name == ".git" || name == "node_modules" || name == "__pycache__" || name == "target" {
            continue;
        }
        if name.ends_with('~') || name.ends_with(".swp") {
            continue;
        }
        let metadata = std::fs::symlink_metadata(&path)
            .map_err(|e| PluginError::Io(format!("could not stat {}: {e}", path.display())))?;
        if metadata.is_dir() {
            walk(root, &path, out)?;
            continue;
        }
        if metadata.file_type().is_symlink() {
            return Err(PluginError::Integrity(format!(
                "{} is a symbolic link; a package may only contain regular files",
                path.display()
            )));
        }
        if !metadata.is_file() {
            return Err(PluginError::Integrity(format!(
                "{} is not a regular file",
                path.display()
            )));
        }
        let relative = path
            .strip_prefix(root)
            .map_err(|_| {
                PluginError::Integrity(format!("{} is not under the package root", path.display()))
            })?
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/");
        validate_entry_name(&relative)?;
        let bytes = std::fs::read(&path)
            .map_err(|e| PluginError::Io(format!("could not read {}: {e}", path.display())))?;
        out.insert(relative, bytes);
    }
    Ok(())
}

/// 用固定头部构造 tar.gz。条目按名字排序（`BTreeMap` 已保证）。
fn build_archive(files: &BTreeMap<String, Vec<u8>>) -> Result<Vec<u8>, PluginError> {
    let mut builder = tar::Builder::new(Vec::new());
    // 不用 `append_path`：它会从文件系统读 mtime/uid/mode，那正是要钉死的东西。
    for (name, bytes) in files {
        let mut header = tar::Header::new_gnu();
        header.set_path(name).map_err(|e| {
            PluginError::Integrity(format!("`{name}` is not a valid archive path: {e}"))
        })?;
        header.set_size(bytes.len() as u64);
        // 只区分"可执行"与"普通文件"：源目录的 umask 与权限位与发布无关。
        let executable = is_executable_name(name);
        header.set_mode(if executable { 0o755 } else { 0o644 });
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(FIXED_MTIME);
        header.set_entry_type(tar::EntryType::Regular);
        // GNU 扩展里的 uname/gname 会带出打包者的机器信息，清掉。
        header.set_cksum();
        builder
            .append(&header, bytes.as_slice())
            .map_err(|e| PluginError::Io(format!("could not append `{name}`: {e}")))?;
    }
    let tar_bytes = builder
        .into_inner()
        .map_err(|e| PluginError::Io(format!("could not finish the archive: {e}")))?;

    // gzip 头里默认带 mtime 与原名；两者都让同一输入产出不同字节。
    let mut encoder = flate2::GzBuilder::new()
        .mtime(0)
        .write(Vec::new(), flate2::Compression::new(6));
    encoder
        .write_all(&tar_bytes)
        .map_err(|e| PluginError::Io(format!("could not compress the archive: {e}")))?;
    encoder
        .finish()
        .map_err(|e| PluginError::Io(format!("could not finish the archive: {e}")))
}

/// 把一次打包的结果落到 `<out>/<stem>.tar.gz`。
///
/// 只写归档，不写元数据：元数据是签名的产物，属于发布工具，不属于宿主库。
pub fn write_release(packed: &Packed, out_dir: &Path, stem: &str) -> Result<PathBuf, PluginError> {
    std::fs::create_dir_all(out_dir)
        .map_err(|e| PluginError::Io(format!("could not create {}: {e}", out_dir.display())))?;
    let bundle = out_dir.join(format!("{stem}.tar.gz"));
    std::fs::write(&bundle, &packed.archive)
        .map_err(|e| PluginError::Io(format!("could not write {}: {e}", bundle.display())))?;
    Ok(bundle)
}

fn hex(bytes: impl AsRef<[u8]>) -> String {
    bytes.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

/// 包内只有程序入口可执行。判定按清单里的 program 更准确，但这里在归档层
/// 没有清单上下文；用路径前缀认出 `bin/` 是唯一稳定且不撒谎的信号。
fn is_executable_name(name: &str) -> bool {
    name.starts_with("bin/")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, bytes: &[u8]) {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, bytes).unwrap();
    }

    fn digest_of(bytes: &[u8]) -> String {
        hex(Sha256::digest(bytes))
    }

    /// 一个最小但完整的插件目录。
    fn plugin_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "ui/index.html", b"<html>main</html>");
        write(root, "ui/settings.html", b"<html>settings</html>");
        write(root, "skills/demo/SKILL.md", b"# demo");
        let manifest = serde_json::json!({
            "schemaVersion": 1,
            "packageFormat": 1,
            "plugin": "demo",
            "version": "1.0.0",
            "target": "darwin-aarch64",
            "views": [
                {
                    "id": "main",
                    "kind": "main",
                    "entry": "ui/index.html",
                    "digest": digest_of(b"<html>main</html>"),
                },
                {
                    "id": "settings",
                    "kind": "settings",
                    "entry": "ui/settings.html",
                    "digest": digest_of(b"<html>settings</html>"),
                }
            ],
            "program": null,
            "skills": [{ "name": "demo", "path": "skills/demo" }],
            "methods": [{ "name": "demo/read", "effect": "Read" }],
            "cli": [{ "verb": "demo:read", "effect": "Read" }],
            "capabilities": { "required": ["Read"], "optional": [] },
            "dependencies": { "workflows": [] },
            "data": { "format": 1, "migrations": [] },
            "compatibility": { "hostApi": "v1", "uiApi": "v1" }
        });
        write(
            root,
            PACKAGE_MANIFEST,
            serde_json::to_vec_pretty(&manifest).unwrap().as_slice(),
        );
        dir
    }

    #[test]
    fn packing_the_same_directory_twice_is_byte_identical() {
        // 确定性不是"通常一样"：同一输入必须逐字节相同，否则摘要没有意义。
        let dir = plugin_dir();
        let first = pack_directory(dir.path()).unwrap();
        let second = pack_directory(dir.path()).unwrap();
        assert_eq!(first.archive, second.archive);
        assert_eq!(first.sha256, second.sha256);
    }

    #[test]
    fn the_archive_ignores_mtime_and_writes_a_fixed_one() {
        // 改文件时间不该改变包：打包器不采纳文件系统的时间。
        let dir = plugin_dir();
        let before = pack_directory(dir.path()).unwrap();

        let file = dir.path().join("ui/index.html");
        let handle = std::fs::File::options().write(true).open(&file).unwrap();
        handle
            .set_modified(
                std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(2_000_000_000),
            )
            .unwrap();

        let after = pack_directory(dir.path()).unwrap();
        assert_eq!(
            before.archive, after.archive,
            "mtime must not leak into the package"
        );
    }

    #[test]
    fn build_outputs_and_vcs_metadata_never_enter_the_package() {
        // `target/` / `node_modules/` / `.git/` 随机器而变，进包就毁掉确定性；
        // `target/` 还会带出本机绝对路径派生的长文件名。
        let dir = plugin_dir();
        write(dir.path(), "target/debug/artifact.bin", b"x");
        write(dir.path(), "node_modules/pkg/index.js", b"x");
        write(dir.path(), ".git/HEAD", b"x");

        let packed = pack_directory(dir.path()).unwrap();
        let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(&packed.archive[..]));
        let names: Vec<String> = tar
            .entries()
            .unwrap()
            .map(|e| e.unwrap().path().unwrap().to_string_lossy().to_string())
            .collect();
        for name in &names {
            assert!(
                !name.starts_with("target/")
                    && !name.starts_with("node_modules/")
                    && !name.starts_with(".git/"),
                "`{name}` must not be packaged"
            );
        }
        assert!(names.iter().any(|n| n == "ui/index.html"));
    }

    #[test]
    fn a_stale_view_digest_is_refused() {
        // 清单说 A、包里是 B：必须在这里失败，而不是等宿主装到一半。
        let dir = plugin_dir();
        let manifest_path = dir.path().join(PACKAGE_MANIFEST);
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
        manifest["views"][0]["digest"] = "0".repeat(64).into();
        std::fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();

        let error = pack_directory(dir.path()).unwrap_err();
        assert!(
            matches!(error, PluginError::Integrity(_)),
            "expected an integrity error, got {error:?}"
        );
    }

    #[test]
    fn a_declared_view_that_is_missing_is_refused() {
        let dir = plugin_dir();
        std::fs::remove_file(dir.path().join("ui/settings.html")).unwrap();
        let error = pack_directory(dir.path()).unwrap_err();
        assert!(
            matches!(error, PluginError::Integrity(_)),
            "expected an integrity error, got {error:?}"
        );
    }

    #[test]
    fn the_package_has_a_manifest_at_its_root() {
        let dir = plugin_dir();
        let packed = pack_directory(dir.path()).unwrap();
        let bytes =
            crate::bundle::read_package_file(&packed.archive, PACKAGE_MANIFEST, 1024 * 1024)
                .expect("the manifest must be readable from the archive root");
        let manifest = Manifest::parse(&bytes).unwrap();
        assert_eq!(manifest.plugin.as_str(), "demo");
        assert_eq!(manifest.version, "1.0.0");
    }
}
