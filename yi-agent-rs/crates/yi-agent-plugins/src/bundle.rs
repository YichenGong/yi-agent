//! 有界安全解包与不可变安装目录。
//!
//! # 设计要点
//!
//! **先把所有条目校验完，再落盘。** 一边解压一边写，会让"第 8 个条目非法"
//! 留下前 7 个文件的残骸，而清理残骸本身就是一处容易漏掉的路径。这里分两遍：
//! 第一遍只读条目头、累计计数、检查边界；第二遍才写。
//!
//! **边解边计。** 先全部解压再看总量，上限就退化成事后统计——磁盘已经被
//! 写满了，检查才发现超限。
//!
//! **归档权限不采纳。** uid/gid、setuid、原始 mode 全部丢弃。只有清单
//! `program.entry` 声明的那个文件由宿主设置执行位，其余一律只读。
//!
//! **输入必须是验证过的发布。** `install_verified` 接受 `&VerifiedRelease`
//! 而不是"摘要 + 字节"，这样调用方无法自己拼一个未经签名验证的包来解压。

use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::error::PluginError;
use crate::limits::ExtractLimits;
use crate::manifest::Manifest;
use crate::trust::VerifiedRelease;

const DIGEST_FILE: &str = ".yi-agent-digests";

/// 一次成功安装的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledRelease {
    pub destination: PathBuf,
    /// 安装的文件数与总字节数，用于日志与注册表。
    pub files_written: usize,
    pub total_bytes: u64,
    /// 清单声明的程序入口（包内相对路径），宿主据此启进程。
    pub program_entry: Option<String>,
}

/// 解包到 `destination`，成功时目录内是一个不可变安装。
///
/// 调用方必须保证 `destination` 尚不存在：同摘要重复安装应在 registry 层
/// 短路（T09），到这里就说明要么是首次安装，要么是冲突。
pub fn install_verified(
    verified: &VerifiedRelease,
    manifest: &Manifest,
    archive: &[u8],
    destination: &Path,
    limits: &ExtractLimits,
) -> Result<InstalledRelease, PluginError> {
    if limits.exceeds_hard_ceiling() {
        return Err(PluginError::PermissionDenied(
            "extract limits exceed the hard ceiling; a plugin may not raise them".to_string(),
        ));
    }

    // --- 摘要与大小先行：它们比解包便宜得多，失败得越早越好 ---
    if archive.len() as u64 != verified.size_bytes {
        return Err(PluginError::Integrity(format!(
            "archive is {} bytes but the signed metadata declares {}",
            archive.len(),
            verified.size_bytes
        )));
    }
    if archive.len() as u64 > limits.max_archive_bytes {
        return Err(PluginError::Integrity(format!(
            "archive is {} bytes, above the {} byte limit",
            archive.len(),
            limits.max_archive_bytes
        )));
    }
    let digest = hex(&Sha256::digest(archive));
    if digest != verified.release.sha256 {
        return Err(PluginError::Integrity(format!(
            "archive digest is {digest}, but the signed metadata declares {}",
            verified.release.sha256
        )));
    }

    // --- 第一遍：只读头，全部校验，不落盘 ---
    let program_entry = manifest.program.as_ref().map(|p| p.entry.clone());
    let plan = plan_extraction(archive, limits)?;

    // --- 第二遍：落盘 ---
    let staging = staging_path(destination);
    if staging.exists() {
        std::fs::remove_dir_all(&staging)?;
    }
    std::fs::create_dir_all(&staging)?;

    let write_result = write_plan(archive, &plan, &staging, program_entry.as_deref(), limits);
    if let Err(error) = write_result {
        // 失败不留残骸：staging 整个丢掉，destination 从未存在。
        let _ = std::fs::remove_dir_all(&staging);
        return Err(error);
    }

    // 摘要在 staging 内生成，再整体换名：rename 之后不再有任何可失败步骤，
    // 否则"写摘要失败"会留下一个已发布却没有指纹的目录。
    if let Err(error) = write_manifest_digests(&staging, &plan) {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(error);
    }

    // 原子换名。若 destination 已存在，rename 会失败——那正是我们要的
    // （幂等短路由 registry 负责，不在这一层静默覆盖）。
    if let Err(error) = std::fs::rename(&staging, destination) {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(PluginError::Io(format!(
            "could not publish the installation at {}: {error}",
            destination.display()
        )));
    }

    Ok(InstalledRelease {
        destination: destination.to_path_buf(),
        files_written: plan.entries.len(),
        total_bytes: plan.total_bytes,
        program_entry,
    })
}

struct PlannedEntry {
    name: String,
}

struct Plan {
    entries: Vec<PlannedEntry>,
    total_bytes: u64,
}

/// 第一遍：只读头，检查所有边界，不落盘。
fn plan_extraction(archive: &[u8], limits: &ExtractLimits) -> Result<Plan, PluginError> {
    let mut entries = Vec::new();
    let mut total_bytes = 0u64;
    // 大小写折叠后的名字集合：macOS 默认不区分大小写，`Index.html` 与
    // `index.html` 会落到同一个文件上，后写的悄悄覆盖前一个。
    let mut seen_folded = BTreeSet::new();

    walk_archive(archive, |name, kind, _size, _reader| {
        validate_entry_name(&name)?;

        let folded = name.to_lowercase();
        if !seen_folded.insert(folded) {
            return Err(PluginError::Integrity(format!(
                "archive contains `{name}`, which collides with another entry \
                 (names are compared case-insensitively because some targets are)"
            )));
        }

        match kind {
            EntryKind::File(size) => {
                if size > limits.max_file_bytes {
                    return Err(PluginError::Integrity(format!(
                        "`{name}` is {size} bytes, above the {} byte per-file limit",
                        limits.max_file_bytes
                    )));
                }
                total_bytes = total_bytes.saturating_add(size);
                if total_bytes > limits.max_total_bytes {
                    return Err(PluginError::Integrity(format!(
                        "archive expands past the {} byte total limit",
                        limits.max_total_bytes
                    )));
                }
                entries.push(PlannedEntry { name });
            }
            EntryKind::Symlink => {
                return Err(PluginError::Integrity(format!(
                    "`{name}` is a symbolic link; archives may only contain regular files"
                )));
            }
            EntryKind::Hardlink => {
                return Err(PluginError::Integrity(format!(
                    "`{name}` is a hard link; archives may only contain regular files"
                )));
            }
            EntryKind::Device => {
                return Err(PluginError::Integrity(format!(
                    "`{name}` is a device or special file; archives may only contain regular files"
                )));
            }
            EntryKind::Directory => {
                // 目录条目本身无害，也不占文件名额。
            }
            EntryKind::Other(kind) => {
                return Err(PluginError::Integrity(format!(
                    "`{name}` is an unsupported entry type {kind:?}; archives may only contain \
                     regular files and directories"
                )));
            }
        }

        if entries.len() > limits.max_files {
            return Err(PluginError::Integrity(format!(
                "archive contains more than {} files",
                limits.max_files
            )));
        }
        Ok(())
    })?;

    Ok(Plan {
        entries,
        total_bytes,
    })
}

/// 包内相对路径：拒绝绝对路径、`..` 段、反斜杠、NUL 与空名。
///
/// 公开的理由：清单里的 `views[].entry` / `program.entry` 与归档条目名
/// 必须用**同一套**规则判定，否则一边允许、一边拒绝，同一个名字在两处
/// 含义不同。host 校验清单路径时应当调用这里，而不是自己写一份。
pub fn validate_entry_name(name: &str) -> Result<(), PluginError> {
    if name.is_empty() {
        return Err(PluginError::Integrity(
            "archive has an entry with an empty name".into(),
        ));
    }
    if name.starts_with('/') {
        return Err(PluginError::Integrity(format!(
            "archive entry `{name}` is an absolute path"
        )));
    }
    if name.contains('\\') {
        return Err(PluginError::Integrity(format!(
            "archive entry `{name}` contains a backslash"
        )));
    }
    if name.contains('\0') {
        return Err(PluginError::Integrity(format!(
            "archive entry `{name}` contains a NUL byte"
        )));
    }
    if name.split('/').any(|segment| segment == "..") {
        return Err(PluginError::Integrity(format!(
            "archive entry `{name}` contains a `..` segment"
        )));
    }
    Ok(())
}

/// 第二遍：写文件。
fn write_plan(
    archive: &[u8],
    plan: &Plan,
    staging: &Path,
    program_entry: Option<&str>,
    limits: &ExtractLimits,
) -> Result<(), PluginError> {
    let wanted: BTreeSet<String> = plan.entries.iter().map(|e| e.name.clone()).collect();
    let mut written_total = 0u64;

    walk_archive(archive, |name, kind, size, reader| {
        if !matches!(kind, EntryKind::File(_)) || !wanted.contains(&name) {
            return Ok(());
        }
        let Some(reader) = reader else {
            return Err(PluginError::Integrity(format!(
                "`{name}` yielded no content stream"
            )));
        };

        let target = staging.join(Path::new(&name));
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let mut buffer = Vec::with_capacity(size.min(1024 * 1024) as usize);
        reader.read_to_end(&mut buffer)?;
        if buffer.len() as u64 != size {
            return Err(PluginError::Integrity(format!(
                "`{name}` declared {size} bytes but yielded {}",
                buffer.len()
            )));
        }
        written_total += buffer.len() as u64;
        if written_total > limits.max_total_bytes {
            return Err(PluginError::Integrity(
                "archive expanded past the total limit while writing".to_string(),
            ));
        }

        let mut file = std::fs::File::create(&target)?;
        file.write_all(&buffer)?;
        file.sync_all()?;
        drop(file);

        // 权限：只保留读位（0444），执行位在下面单独处理。
        // 归档里的 mode 一律不采纳——setuid、组写、其他用户写全部丢弃。
        set_read_only(&target)?;
        Ok(())
    })?;

    if let Some(entry) = program_entry {
        let program = staging.join(entry);
        if !program.exists() {
            return Err(PluginError::Integrity(format!(
                "the manifest declares `{entry}` as the program entry, but the archive does not contain it"
            )));
        }
        set_executable(&program)?;
    }

    Ok(())
}

fn set_read_only(path: &Path) -> Result<(), PluginError> {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = std::fs::metadata(path)?.permissions();
    permissions.set_mode(0o444);
    std::fs::set_permissions(path, permissions)?;
    Ok(())
}

fn set_executable(path: &Path) -> Result<(), PluginError> {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = std::fs::metadata(path)?.permissions();
    permissions.set_mode(0o555);
    std::fs::set_permissions(path, permissions)?;
    Ok(())
}

/// staging 与 destination 同级，保证 `rename` 在同一个文件系统上（否则
/// 退化成 copy，就不再原子）。
fn staging_path(destination: &Path) -> PathBuf {
    let parent = destination.parent().unwrap_or_else(|| Path::new("."));
    let name = destination
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "installed".to_string());
    parent.join(format!(".{name}.staging"))
}

// ---------------------------------------------------------------------------
// 安装后完整性
// ---------------------------------------------------------------------------

fn write_manifest_digests(destination: &Path, plan: &Plan) -> Result<(), PluginError> {
    let mut lines = String::new();
    for entry in &plan.entries {
        let target = destination.join(&entry.name);
        let bytes = std::fs::read(&target)?;
        lines.push_str(&format!(
            "{}  {}\n",
            hex(&Sha256::digest(&bytes)),
            entry.name
        ));
    }
    let path = destination.join(DIGEST_FILE);
    std::fs::write(&path, lines)?;
    // 摘要文件自身不允许被改写，否则篡改者可以顺手更新它。
    set_read_only(&path)?;
    Ok(())
}

/// 启动前检查：每个文件仍与安装时记录的摘要一致。
///
/// 这条防的是"装上之后被替换"——签名验证只在安装时发生一次，之后
/// 磁盘上的字节没人再看。
pub fn verify_installed(destination: &Path) -> Result<(), PluginError> {
    let digest_path = destination.join(DIGEST_FILE);
    let recorded = std::fs::read_to_string(&digest_path).map_err(|e| {
        PluginError::Integrity(format!(
            "installation has no digest record at {}: {e}",
            digest_path.display()
        ))
    })?;

    for line in recorded.lines().filter(|l| !l.trim().is_empty()) {
        let Some((expected, name)) = line.split_once("  ") else {
            return Err(PluginError::Integrity(format!(
                "malformed digest record line: {line:?}"
            )));
        };
        let path = destination.join(name);
        let bytes = std::fs::read(&path).map_err(|e| {
            PluginError::Integrity(format!(
                "installed file `{name}` is missing or unreadable: {e}"
            ))
        })?;
        let actual = hex(&Sha256::digest(&bytes));
        if actual != expected {
            return Err(PluginError::Integrity(format!(
                "installed file `{name}` was modified after installation"
            )));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// tar 读取
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum EntryKind {
    File(u64),
    Symlink,
    Hardlink,
    Device,
    Directory,
    /// 其他类型（FIFO、未知）。
    Other(tar::EntryType),
}

/// 逐条遍历归档，把每条交给 `visit`。
///
/// 用回调而不是返回 `Vec<Reader>`：tar 的条目借用归档本身，把 reader
/// 带出这个作用域会让整个函数无法通过借用检查——而强行收集成 `Vec<u8>`
/// 就等于把整个归档解压到内存，正好抵消"边解边计"的意义。
fn walk_archive<F>(archive: &[u8], mut visit: F) -> Result<(), PluginError>
where
    F: FnMut(String, EntryKind, u64, Option<&mut dyn Read>) -> Result<(), PluginError>,
{
    let decoder = flate2::read::GzDecoder::new(archive);
    let mut tar = tar::Archive::new(decoder);

    let entries = tar
        .entries()
        .map_err(|e| PluginError::Integrity(format!("archive is not readable: {e}")))?;

    for entry in entries {
        let mut entry = entry
            .map_err(|e| PluginError::Integrity(format!("archive entry is unreadable: {e}")))?;
        let header = entry.header();
        let name = entry
            .path()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        let size = header.size().unwrap_or(0);
        let kind = match header.entry_type() {
            tar::EntryType::Regular | tar::EntryType::Continuous => EntryKind::File(size),
            // GNU 的长名字/长链接条目不是真实文件，跳过比当作未知类型拒绝更合适。
            tar::EntryType::GNULongName | tar::EntryType::GNULongLink => continue,
            tar::EntryType::Symlink => EntryKind::Symlink,
            tar::EntryType::Link => EntryKind::Hardlink,
            tar::EntryType::Char | tar::EntryType::Block => EntryKind::Device,
            tar::EntryType::Directory => EntryKind::Directory,
            other => EntryKind::Other(other),
        };
        // reader 只在 File 时给出：其他类型的头部已足够判断"必须拒绝"，
        // 读它们的内容没有意义。
        let reader: Option<&mut dyn Read> = if matches!(kind, EntryKind::File(_)) {
            Some(&mut entry)
        } else {
            None
        };
        visit(name, kind, size, reader)?;
    }
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
