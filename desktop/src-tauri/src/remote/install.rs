//! 安装编排：选路 + 拼远端解压命令。
//!
//! 刻意把**决策**（本文件上半部）与**执行**（下半部）分开：决策是纯函数，
//! 每条降级分支都能在单测里走一遍，不用真的下载十几 MB 或连一台没有 gzip 的
//! 机器；执行部分是 ssh/scp 编排，靠手工端到端验证。

use std::path::Path;

use crate::remote::probe::RemoteCaps;

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum InstallRoute {
    /// 本机下载（或命中缓存）→ scp → 远端解压。主路径。
    LocalDownload,
    /// 远端自己 curl/wget。远端有外网时用；也是「远端没有 tar」时的兜底。
    RemoteDownload,
    /// 用户用 `--from` 指定的本地文件。气隙环境与不支持架构的出路。
    FromFile,
}

/// 选路 + 解压命令的组合。给调用方一个「先算再执行」的显式中间结果，
/// 便于设置页在执行前展示将要发生什么。
#[allow(dead_code)]
pub struct Plan {
    pub route: InstallRoute,
    pub unpack: String,
}

/// 选路。顺序即优先级：
/// 1. `--from` 给了就用（显式动作不该被「我有缓存」覆盖）
/// 2. 远端连 `tar` 都没有 → 只能远端下载（解压不了，必须当场执行流水线）
/// 3. 否则本机下载（缓存命中与否只影响是否重新下载，不影响选路）
pub fn choose_route(caps: &RemoteCaps, _has_cache: bool, from: Option<&Path>) -> InstallRoute {
    if from.is_some() {
        return InstallRoute::FromFile;
    }
    if !caps.has_tar {
        return InstallRoute::RemoteDownload;
    }
    InstallRoute::LocalDownload
}

/// shell 单引号转义。本文件是全项目第二处拼 shell 的地方（第一处是
/// `remote/argv.rs`），同一套转义规则。
pub fn sh_quote(raw: &str) -> String {
    format!("'{}'", raw.replace('\'', r"'\''"))
}

/// 远端解压命令。
///
/// **`-z` 只在确认远端 gzip 可用时才加**：GNU tar 的 `-z` 会 fork 外部 `gzip`，
/// 极简镜像里没有它，`tar -xzf` 直接失败（spec §2.4.2）。没有 gzip 时上传的
/// 产物必须是**已解压的 `.tar`**（见 `needs_local_decompress`）。
pub fn unpack_command(caps: &RemoteCaps, tarball_remote: &str, dest: &str) -> String {
    let flag = if caps.tar_supports_gzip {
        "-xzf"
    } else {
        "-xf"
    };
    // 目录必须先建：`tar -C` 不会创建目标目录。
    format!(
        "mkdir -p {} && tar {} {} -C {}",
        sh_quote(dest),
        flag,
        sh_quote(tarball_remote),
        sh_quote(dest)
    )
}

/// 远端没有 gzip 时，上传前必须在本机把 `.tar.gz` 解成 `.tar`。
///
/// **这是计划里最容易漏的一处**：只用 `-xf` 去解一个 gzip 文件会立刻失败，
/// 而错误信息（`tar: Unrecognized archive format`）离真正原因很远。
pub fn needs_local_decompress(caps: &RemoteCaps) -> bool {
    caps.has_tar && !caps.tar_supports_gzip
}

/// 远端要接收的文件名。没有 gzip 时上传未压缩的 `.tar`。
pub fn upload_name(target: &str, version: &str, caps: &RemoteCaps) -> String {
    let suffix = if needs_local_decompress(caps) {
        "tar"
    } else {
        "tar.gz"
    };
    format!("yi-agent-{version}-{target}.{suffix}")
}

#[allow(dead_code)]
pub fn plan(
    caps: &RemoteCaps,
    has_cache: bool,
    from: Option<&Path>,
    tarball_remote: &str,
    dest: &str,
) -> Plan {
    Plan {
        route: choose_route(caps, has_cache, from),
        unpack: unpack_command(caps, tarball_remote, dest),
    }
}

// ---- 执行部分：ssh / scp 编排 ----

/// 执行安装，返回装好的远端二进制路径。
///
/// 三条路径的共同终点相同：远端出现 `<instance_root>/bin/<version>/yi-agent`
/// 且可执行。
pub fn install(
    fetcher: &dyn crate::remote::fetch::Fetcher,
    home: &Path,
    instance_root: &str,
    target: &str,
    version: &str,
    caps: &RemoteCaps,
    host: &str,
    from: Option<&Path>,
) -> Result<String, String> {
    let dest = crate::remote::cache::remote_install_dir(instance_root, version);
    let binary = crate::remote::cache::remote_binary_path(instance_root, version);

    match choose_route(caps, false, from) {
        InstallRoute::FromFile => {
            let source = from.expect("route implies a source");
            let remote_tmp = "/tmp/yi-agent-from";
            scp_to_remote(source, host, remote_tmp)?;
            run_remote(
                host,
                &format!(
                    "mkdir -p {dest} && cp {tmp} {binary} && chmod +x {binary} && rm -f {tmp}",
                    dest = sh_quote(&dest),
                    tmp = sh_quote(remote_tmp),
                    binary = sh_quote(&binary),
                ),
            )?;
        }
        InstallRoute::LocalDownload => {
            let cached = crate::remote::cache::cache_path(home, version, target);
            if !cached.exists() {
                crate::remote::fetch::download_verified(
                    fetcher,
                    crate::remote::fetch::RELEASE_BASE,
                    target,
                    version,
                    &cached,
                )?;
            }
            // 远端没有 gzip 时先在本机解压：`-xf` 解不了 gzip。
            let upload = if needs_local_decompress(caps) {
                let plain = cached.with_extension("tar");
                decompress_gzip(&cached, &plain)?;
                plain
            } else {
                cached
            };
            let remote_tmp = format!("/tmp/{}", upload_name(target, version, caps));
            scp_to_remote(&upload, host, &remote_tmp)?;
            run_remote(host, &unpack_command(caps, &remote_tmp, &dest))?;
            run_remote(
                host,
                &format!(
                    "chmod +x {} && rm -f {}",
                    sh_quote(&binary),
                    sh_quote(&remote_tmp)
                ),
            )?;
        }
        InstallRoute::RemoteDownload => {
            let url = format!(
                "{}/v{version}/yi-agent-{target}.tar.gz",
                crate::remote::fetch::RELEASE_BASE
            );
            let url = sh_quote(&url);
            // curl 缺席时退 wget；两者都没有则明确报错，而不是留一个空目录。
            run_remote(
                host,
                &format!(
                    "mkdir -p {dest} && \
                     if command -v curl >/dev/null 2>&1; then curl -fsSL {url} | tar -xz -C {dest}; \
                     elif command -v wget >/dev/null 2>&1; then wget -qO- {url} | tar -xz -C {dest}; \
                     else echo 'neither curl nor wget is available on the remote' >&2; exit 1; fi && \
                     chmod +x {binary}",
                    dest = sh_quote(&dest),
                    binary = sh_quote(&binary),
                ),
            )?;
        }
    }
    Ok(binary)
}

/// `.tar.gz` → `.tar`。只在本机用（`flate2` 已在依赖树里，经 tauri 的压缩支持）。
fn decompress_gzip(source: &Path, dest: &Path) -> Result<(), String> {
    use std::io::Write;

    let compressed =
        std::fs::File::open(source).map_err(|e| format!("{}: {e}", source.display()))?;
    let mut decoder = flate2::read::GzDecoder::new(compressed);
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let mut plain = std::fs::File::create(dest).map_err(|e| format!("{}: {e}", dest.display()))?;
    std::io::copy(&mut decoder, &mut plain).map_err(|e| format!("{}: {e}", source.display()))?;
    plain.flush().map_err(|e| e.to_string())
}

/// 跑一条远端命令，失败时带上 stderr。
///
/// `BatchMode=yes` 与 `ConnectTimeout=20` 与 `remote/argv.rs` 同口径：非交互、
/// 不可达时不要挂住。
pub fn run_remote(host: &str, command: &str) -> Result<String, String> {
    let out = std::process::Command::new("ssh")
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=20",
            host,
            command,
        ])
        .output()
        .map_err(|e| format!("ssh failed: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "remote command failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn scp_to_remote(local: &Path, host: &str, remote: &str) -> Result<(), String> {
    let out = std::process::Command::new("scp")
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=20",
            &local.display().to_string(),
            &format!("{host}:{remote}"),
        ])
        .output()
        .map_err(|e| format!("scp failed: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "scp failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::probe::RemoteCaps;

    fn caps(gzip: bool, has_tar: bool) -> RemoteCaps {
        RemoteCaps {
            target: "x86_64-unknown-linux-gnu",
            tar_supports_gzip: gzip,
            has_tar,
        }
    }

    #[test]
    fn from_file_wins_when_given() {
        // 显式动作优先：用户给了文件就别去下载。
        let route = choose_route(&caps(true, true), true, Some(Path::new("/tmp/yi-agent")));
        assert_eq!(route, InstallRoute::FromFile);
    }

    #[test]
    fn a_cache_hit_uses_the_local_download_route() {
        assert_eq!(
            choose_route(&caps(true, true), true, None),
            InstallRoute::LocalDownload
        );
    }

    #[test]
    fn no_tar_on_the_remote_falls_back_to_remote_download() {
        // 没有 tar 就解压不了，只能让远端自己拉并当场解压（路径 2）。
        assert_eq!(
            choose_route(&caps(false, false), false, None),
            InstallRoute::RemoteDownload
        );
    }

    #[test]
    fn gzip_unpack_uses_xzf() {
        let command = unpack_command(
            &caps(true, true),
            "/tmp/a.tar.gz",
            "/home/dev/.yi-agent/bin/1",
        );
        assert!(command.contains("tar -xzf"), "{command}");
        assert!(
            command.contains("-C '/home/dev/.yi-agent/bin/1'"),
            "{command}"
        );
        // `tar -C` 不会自己建目录。
        assert!(command.contains("mkdir -p"), "{command}");
    }

    #[test]
    fn plain_unpack_uses_xf_without_z() {
        // GNU tar 的 -z 要外部 gzip；没有就绝不能用 -z。
        let command = unpack_command(&caps(false, true), "/tmp/a.tar", "/home/dev/b");
        assert!(command.contains("tar -xf "), "{command}");
        assert!(!command.contains("-xzf"), "{command}");
    }

    /// 远端有 tar 无 gzip 时，上传的必须是已解压的 `.tar`。
    #[test]
    fn a_gzipless_remote_gets_a_plain_tarball() {
        assert!(needs_local_decompress(&caps(false, true)));
        assert!(!needs_local_decompress(&caps(true, true)));
        // 完全没有 tar 时走远端下载，与本机上传无关。
        assert!(!needs_local_decompress(&caps(false, false)));
        assert_eq!(
            upload_name("x86_64-unknown-linux-gnu", "0.1.10", &caps(false, true)),
            "yi-agent-0.1.10-x86_64-unknown-linux-gnu.tar"
        );
        assert_eq!(
            upload_name("x86_64-unknown-linux-gnu", "0.1.10", &caps(true, true)),
            "yi-agent-0.1.10-x86_64-unknown-linux-gnu.tar.gz"
        );
    }

    #[test]
    fn plain_route_tarball_is_not_gz_suffixed() {
        let plan = plan(
            &caps(false, true),
            false,
            None,
            "/tmp/yi-agent.tar",
            "/home/d/b",
        );
        assert!(plan.unpack.contains("/tmp/yi-agent.tar"), "{}", plan.unpack);
        assert!(!plan.unpack.contains(".tar.gz "), "{}", plan.unpack);
    }

    /// 单引号路径必须转义：远端路径既然由用户/配置决定，就不能假设它「正常」。
    #[test]
    fn single_quotes_in_paths_are_escaped() {
        let command = unpack_command(&caps(true, true), "/tmp/a.tar.gz", "/home/it's/bin");
        assert!(command.contains(r"/home/it'\''s/bin"), "{command}");
    }

    /// 真实 gzip 往返：`needs_local_decompress` 的降级必须真的能解出内容。
    #[test]
    fn decompressing_a_real_gzip_yields_the_original_bytes() {
        use std::io::Write;

        let dir = tempfile::TempDir::new().unwrap();
        let gz = dir.path().join("a.tar.gz");
        let payload = b"pretend this is a tar stream";
        {
            let file = std::fs::File::create(&gz).unwrap();
            let mut encoder = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
            encoder.write_all(payload).unwrap();
            encoder.finish().unwrap();
        }

        let plain = dir.path().join("a.tar");
        decompress_gzip(&gz, &plain).unwrap();

        assert_eq!(std::fs::read(&plain).unwrap(), payload);
    }

    #[test]
    fn decompressing_a_non_gzip_file_fails_loudly() {
        let dir = tempfile::TempDir::new().unwrap();
        let bogus = dir.path().join("a.tar.gz");
        std::fs::write(&bogus, b"not gzip at all").unwrap();
        assert!(decompress_gzip(&bogus, &dir.path().join("a.tar")).is_err());
    }
}
