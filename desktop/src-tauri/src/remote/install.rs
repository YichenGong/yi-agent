//! 安装编排：选路 + 拼远端解压命令。
//!
//! 刻意把**决策**（本文件上半部）与**执行**（下半部）分开：决策是纯函数，
//! 每条降级分支都能在单测里走一遍，不用真的下载十几 MB 或连一台没有 gzip 的
//! 机器；执行部分是 ssh/scp 编排，靠手工端到端验证。

use std::path::{Path, PathBuf};
use std::time::Duration;

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
/// 极简镜像里没有它，`tar -xzf` 直接失败（spec §2.4.2）。没有 gzip 的远端
/// 根本不走这条命令（见 `needs_local_extract`）。
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

/// 远端有 `tar` 但解不了 gzip 时，安装改走「本机解出二进制，只传那一个文件」。
///
/// **这是计划里最容易漏的一处**：只用 `-xf` 去解一个 gzip 文件会立刻失败，
/// 而错误信息（`tar: Unrecognized archive format`）离真正原因很远。
///
/// 为什么不是「本机解成 `.tar` 再传」：那要 37MB 上传，还得让远端的 `tar`
/// 能用——可远端恰恰是那台「tar 在、gzip 不在」的机器，多传一层只是把同一
/// 个风险换了个位置。只传二进制只有几十 MB 里的一小份，且远端一条 `cp` 就
/// 完事，**远端连 tar 都不再需要**。
///
/// 解压在**本机用系统 `tar`** 做（macOS 的 bsdtar 自带 zlib，Linux 发行版
/// 必有 gzip 二进制），因此 Rust 侧不引入任何解压依赖——spec 判据 10 要求
/// `cargo tree` 里没有 `xz2`/`flate2`，为的就是不让 App 为一个可选降级路径
/// 背一个解压库。
pub fn needs_local_extract(caps: &RemoteCaps) -> bool {
    caps.has_tar && !caps.tar_supports_gzip
}

/// 远端要接收的文件名。
///
/// 本机解出二进制时传的是裸二进制（无后缀）；否则传整个 `.tar.gz`。
pub fn upload_name(target: &str, version: &str, caps: &RemoteCaps) -> String {
    if needs_local_extract(caps) {
        "yi-agent".to_string()
    } else {
        format!("yi-agent-{version}-{target}.tar.gz")
    }
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

// ---- 进度回推 ----

/// 安装阶段。UI 据此显示「正在做什么」，而不是一个静止的「连接中」。
///
/// spec §8.2：慢网下这一步要等十几秒，没有反馈就是最容易被投诉的体验。
/// 阶段划分按**用户能理解的动作**切，不按函数边界切：用户在意的不是
/// 「正在解压」，而是「在下 16MB」还是「在传到机器上」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallStage {
    /// 正在从发布渠道下 `.tar.gz`（含 sha256 校验）。
    Downloading,
    /// 正在 scp 到远端。
    Uploading,
    /// 正在远端解压 / 落位。
    Unpacking,
    /// 装完了。
    Done,
}

impl InstallStage {
    pub fn as_str(self) -> &'static str {
        match self {
            InstallStage::Downloading => "downloading",
            InstallStage::Uploading => "uploading",
            InstallStage::Unpacking => "unpacking",
            InstallStage::Done => "done",
        }
    }
}

/// 进度回调。`None` 表示调用方不关心（单测与 CLI 路径）。
pub type Progress<'a> = &'a dyn Fn(InstallStage);

// ---- 执行部分：ssh / scp 编排 ----

/// 执行安装，返回装好的远端二进制路径。
///
/// 三条路径的共同终点相同：远端出现 `<instance_root>/bin/<version>/yi-agent`
/// 且可执行。
///
/// 九个参数全部是独立输入（取字节的方式、本地 home、远端根、目标三元组、
/// 版本、远端能力、主机、可选本地包、进度回调）。折成结构体会让"这是哪两个"
/// 隐式化，而这里每一项都决定一次 ssh/scp 的自由度。
#[allow(clippy::too_many_arguments)]
pub fn install(
    fetcher: &dyn crate::remote::fetch::Fetcher,
    home: &Path,
    instance_root: &str,
    target: &str,
    version: &str,
    caps: &RemoteCaps,
    host: &str,
    from: Option<&Path>,
    progress: Option<Progress<'_>>,
) -> Result<String, String> {
    let report = |stage: InstallStage| {
        if let Some(callback) = progress {
            callback(stage);
        }
    };
    let dest = crate::remote::cache::remote_install_dir(instance_root, version);
    let binary = crate::remote::cache::remote_binary_path(instance_root, version);

    match choose_route(caps, false, from) {
        InstallRoute::FromFile => {
            let source = from.expect("route implies a source");
            let remote_tmp = "/tmp/yi-agent-from";
            report(InstallStage::Uploading);
            scp_to_remote(source, host, remote_tmp)?;
            report(InstallStage::Unpacking);
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
                report(InstallStage::Downloading);
                crate::remote::fetch::download_verified(
                    fetcher,
                    crate::remote::fetch::RELEASE_BASE,
                    target,
                    version,
                    &cached,
                )?;
            }
            if needs_local_extract(caps) {
                // 远端解不了 gzip：只把二进制解出来传过去，远端不必有 tar。
                let staging = Staging::create()?;
                let local_bin = extract_binary(&cached, staging.path())?;
                let remote_tmp = "/tmp/yi-agent-upload";
                report(InstallStage::Uploading);
                scp_to_remote(&local_bin, host, remote_tmp)?;
                report(InstallStage::Unpacking);
                run_remote(
                    host,
                    &format!(
                        "mkdir -p {dest} && cp {tmp} {binary} && chmod +x {binary} && rm -f {tmp}",
                        dest = sh_quote(&dest),
                        tmp = sh_quote(remote_tmp),
                        binary = sh_quote(&binary),
                    ),
                )?;
            } else {
                let remote_tmp = format!("/tmp/{}", upload_name(target, version, caps));
                report(InstallStage::Uploading);
                scp_to_remote(&cached, host, &remote_tmp)?;
                report(InstallStage::Unpacking);
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
        }
        InstallRoute::RemoteDownload => {
            let url = format!(
                "{}/v{version}/yi-agent-{target}.tar.gz",
                crate::remote::fetch::RELEASE_BASE
            );
            let url = sh_quote(&url);
            report(InstallStage::Downloading);
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
    report(InstallStage::Done);
    Ok(binary)
}

/// 解压用的临时目录，离开作用域即删。
///
/// 不引 `tempfile`：它当前只是 dev-dependency，为一次解压把它提成生产依赖会
/// 为三个函数多带一棵依赖树。这里要的语义很窄——建一个唯一目录、退出时删掉。
struct Staging {
    path: PathBuf,
}

impl Staging {
    fn create() -> Result<Self, String> {
        // 进程 id + 纳秒时间戳：同一进程连装两次也不会撞。
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path =
            std::env::temp_dir().join(format!("yi-agent-extract-{}-{stamp}", std::process::id()));
        std::fs::create_dir_all(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(Self { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// 从 `.tar.gz` 里解出 `yi-agent` 这一个成员，放在 `staging` 下。
///
/// 用**系统 `tar`** 而不是 Rust 解压库：判据 10 要求本机依赖树里不出现
/// `gz`/`xz` 解压 crate。macOS 的 bsdtar 内置 zlib，Linux 发行版都有 `gzip`，
/// 所以这条降级路径在两端都能走通。发布产物是 `tar -czf ... yi-agent` 打的，
/// 成员名固定为 `yi-agent`（见 `desktop/scripts/release-dmg.sh`）。
fn extract_binary(archive: &Path, staging: &Path) -> Result<PathBuf, String> {
    let out = std::process::Command::new("tar")
        .args([
            "-xzf",
            &archive.display().to_string(),
            "-C",
            &staging.display().to_string(),
            "yi-agent",
        ])
        .output()
        .map_err(|e| format!("tar failed: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "tar failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let binary = staging.join("yi-agent");
    if !binary.exists() {
        return Err(format!(
            "{} does not contain a yi-agent member",
            archive.display()
        ));
    }
    Ok(binary)
}

/// 跑一条远端命令，失败时带上 stderr。
///
/// `BatchMode=yes` 与 `ConnectTimeout=20` 与 `remote/argv.rs` 同口径：非交互、
/// 不可达时不要挂住。
pub fn run_remote(host: &str, command: &str) -> Result<String, String> {
    run_remote_within(host, command, PROBE_TIMEOUT)
}

/// 探测类命令的超时（spec §4.1 的表：10~20s）。
///
/// `ConnectTimeout` 只盖住**建连**：host 可达但远端卡住（磁盘满、NFS 挂死、
/// `test -d` 撞上一个失效挂载点）时，ssh 会一直等下去，UI 就永远停在「连接中」。
/// 那是这类功能最常见的坏体验，所以命令本身另有一道 watchdog。
const PROBE_TIMEOUT: Duration = Duration::from_secs(20);

/// 带 watchdog 的 `run_remote`。
///
/// 实现方式：子进程 + 等待线程 + `timeout` 上的 `recv_timeout`。做不到「先
/// kill 再等」的话，超时后就把它留成孤儿——所以超时分支显式 `kill`。
pub fn run_remote_within(host: &str, command: &str, timeout: Duration) -> Result<String, String> {
    let mut command_builder = std::process::Command::new("ssh");
    command_builder.args([
        "-o",
        "BatchMode=yes",
        "-o",
        "ConnectTimeout=20",
        host,
        command,
    ]);
    let label = format!("ssh `{host}`");
    run_guarded(command_builder, &label, timeout)
}

/// 同一套 watchdog，命令由调用方给。测试用它跑本地 `/bin/sh`。
#[cfg(test)]
fn run_local_within(command: &str, timeout: Duration) -> Result<String, String> {
    let mut builder = std::process::Command::new("/bin/sh");
    builder.args(["-c", command]);
    run_guarded(builder, "`/bin/sh -c`", timeout)
}

/// 起子进程、等 stdout/stderr 收尾线程、按 `timeout` 决断。
///
/// **必须真的 kill**：超时后留着的 ssh 会在 ssh 层堆积，下一次重连又多一个
/// 同样卡住的进程，越积越多。
fn run_guarded(
    mut builder: std::process::Command,
    label: &str,
    timeout: Duration,
) -> Result<String, String> {
    let mut child = builder
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to start {label}: {e}"))?;

    let mut stdout = child.stdout.take().expect("stdout piped");
    let mut stderr = child.stderr.take().expect("stderr piped");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        use std::io::Read;
        let mut out = String::new();
        let mut err = String::new();
        let _ = stdout.read_to_string(&mut out);
        let _ = stderr.read_to_string(&mut err);
        // 收尾失败意味着主线程已放弃（超时），忽略即可。
        let _ = tx.send((out, err));
    });

    match rx.recv_timeout(timeout) {
        Ok((out, err)) => {
            let status = child.wait().map_err(|e| format!("{label} failed: {e}"))?;
            // 非零退出必须带上 stderr：只说「失败」，用户无从下手。
            if !status.success() {
                return Err(format!("{label} failed ({status}): {}", err.trim()));
            }
            Ok(out)
        }
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            Err(format!("{label} timed out after {}s", timeout.as_secs()))
        }
    }
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

    /// 远端有 tar 无 gzip 时，只上传解出来的那一个二进制。
    #[test]
    fn a_gzipless_remote_gets_only_the_binary() {
        assert!(needs_local_extract(&caps(false, true)));
        assert!(!needs_local_extract(&caps(true, true)));
        // 完全没有 tar 时走远端下载，与本机上传无关。
        assert!(!needs_local_extract(&caps(false, false)));
        assert_eq!(
            upload_name("x86_64-unknown-linux-gnu", "0.1.10", &caps(false, true)),
            "yi-agent"
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

    /// 阶段名是**外部契约**（前端按字符串匹配）：改名会让 UI 静默失去进度。
    #[test]
    fn stage_names_are_stable_strings() {
        assert_eq!(InstallStage::Downloading.as_str(), "downloading");
        assert_eq!(InstallStage::Uploading.as_str(), "uploading");
        assert_eq!(InstallStage::Unpacking.as_str(), "unpacking");
        assert_eq!(InstallStage::Done.as_str(), "done");
    }

    /// 无缓存时必须先报「下载」再报「上传」：顺序反了会让 UI 显示
    /// 「正在上传」十几秒而实际在下载。
    #[test]
    fn a_cold_install_reports_download_before_upload() {
        let dir = tempfile::TempDir::new().unwrap();
        let seen = std::sync::Mutex::new(Vec::new());
        let record = |stage: InstallStage| seen.lock().unwrap().push(stage);

        // 用一个必然失败的 host：我们只关心失败前报过的阶段。
        struct EmptyFetch;
        impl crate::remote::fetch::Fetcher for EmptyFetch {
            fn get(&self, url: &str) -> Result<Vec<u8>, String> {
                Err(format!("test fetcher refuses {url}"))
            }
        }
        let _ = install(
            &EmptyFetch,
            dir.path(),
            "/home/dev/.yi-agent",
            "x86_64-unknown-linux-gnu",
            "0.1.10",
            &caps(true, true),
            "127.0.0.1",
            None,
            Some(&record),
        );

        let stages = seen.lock().unwrap().clone();
        let first = stages.first().copied();
        assert_eq!(
            first,
            Some(InstallStage::Downloading),
            "a cold install must announce the download first: {stages:?}"
        );
    }

    /// 走路径 3 时不该出现「下载」阶段：用户给的文件是本地的，报下载会
    /// 让人以为系统还在联网取东西（对气隙环境尤其误导）。
    #[test]
    fn a_from_file_install_never_reports_downloading() {
        let dir = tempfile::TempDir::new().unwrap();
        let source = dir.path().join("yi-agent");
        std::fs::write(&source, b"#!/bin/sh\n").unwrap();
        let seen = std::sync::Mutex::new(Vec::new());
        let record = |stage: InstallStage| seen.lock().unwrap().push(stage);

        let _ = install(
            &crate::remote::fetch::HttpFetcher,
            dir.path(),
            "/home/dev/.yi-agent",
            "x86_64-unknown-linux-gnu",
            "0.1.10",
            &caps(true, true),
            "127.0.0.1",
            Some(&source),
            Some(&record),
        );

        let stages = seen.lock().unwrap().clone();
        assert!(
            !stages.contains(&InstallStage::Downloading),
            "path 3 must not claim to download: {stages:?}"
        );
        assert_eq!(stages.first().copied(), Some(InstallStage::Uploading));
    }

    /// 判据 25：卡住的远端命令必须超时，而不是让 UI 永远停在「连接中」。
    ///
    /// 只测 watchdog 本身：`sleep` 作为「远端命令」交给本地 `/bin/sh`，不经过
    /// ssh。这样无论本机有没有 sshd、网络是否可达，测的都是同一条路径——超时
    /// 分支。经 ssh 测会让这条断言在一台没有 sshd 的机器上变成「ssh 立刻失败」，
    /// 根本没走到被测代码。
    #[test]
    fn a_stuck_command_times_out_and_is_killed() {
        let started = std::time::Instant::now();
        let result = run_local_within("sleep 30", Duration::from_millis(300));
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(5),
            "the watchdog did not fire; elapsed {elapsed:?}"
        );
        let error = result.unwrap_err();
        assert!(error.contains("timed out"), "{error}");
        assert!(error.contains("0s") || error.contains("300"), "{error}");
    }

    /// 正常退出的命令要把 stdout 原样交回，且不因为 watchdog 而被误杀。
    #[test]
    fn a_fast_command_returns_its_stdout() {
        let out = run_local_within("printf YI_OK", Duration::from_secs(10)).unwrap();
        assert_eq!(out, "YI_OK");
    }

    /// 非零退出必须失败，并把 stderr 带出来——否则用户只看到「失败」两个字。
    #[test]
    fn a_failing_command_surfaces_stderr() {
        let error = run_local_within("echo boom >&2; exit 3", Duration::from_secs(10)).unwrap_err();
        assert!(error.contains("boom"), "{error}");
    }

    /// 真实 gzip 往返：`needs_local_extract` 的降级必须真的解出可执行文件。
    ///
    /// 用系统 `tar` 打包（与 `release-dmg.sh` 同一命令），再用同一套系统 `tar`
    /// 解回来——被测的正是那条真实路径，而不是一个模拟实现。
    #[test]
    fn extracting_a_real_tarball_yields_the_binary() {
        let dir = tempfile::TempDir::new().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        let payload = b"#!/bin/sh\necho hi\n";
        std::fs::write(src.join("yi-agent"), payload).unwrap();
        let gz = dir.path().join("a.tar.gz");
        let status = std::process::Command::new("tar")
            .args([
                "-czf",
                &gz.display().to_string(),
                "-C",
                &src.display().to_string(),
                "yi-agent",
            ])
            .status()
            .unwrap();
        assert!(status.success());

        let staging = dir.path().join("out");
        std::fs::create_dir_all(&staging).unwrap();
        let binary = extract_binary(&gz, &staging).unwrap();

        assert_eq!(std::fs::read(&binary).unwrap(), payload);
    }

    #[test]
    fn extracting_a_non_gzip_file_fails_loudly() {
        let dir = tempfile::TempDir::new().unwrap();
        let bogus = dir.path().join("a.tar.gz");
        std::fs::write(&bogus, b"not gzip at all").unwrap();
        let staging = dir.path().join("out");
        std::fs::create_dir_all(&staging).unwrap();
        assert!(extract_binary(&bogus, &staging).is_err());
    }

    /// 压缩包里没有 `yi-agent` 成员时必须报错，而不是返回一个不存在的路径。
    #[test]
    fn a_tarball_without_the_binary_fails_loudly() {
        let dir = tempfile::TempDir::new().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("something-else"), b"x").unwrap();
        let gz = dir.path().join("a.tar.gz");
        let status = std::process::Command::new("tar")
            .args([
                "-czf",
                &gz.display().to_string(),
                "-C",
                &src.display().to_string(),
                "something-else",
            ])
            .status()
            .unwrap();
        assert!(status.success());

        let staging = dir.path().join("out");
        std::fs::create_dir_all(&staging).unwrap();
        assert!(extract_binary(&gz, &staging).is_err());
    }
}
