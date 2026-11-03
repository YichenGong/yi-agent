//! 通用假插件：一个能区分「自述」与「正常运行」的测试替身。
//!
//! 这里**不引用本 crate 的任何类型**，所以 `yi-agent-runtime` 的源码级测试可以
//! 通过 `#[path]` 直接复用同一份文件，而不必给 supervisors 开一个生产测试 API。
//!
//! 每个项目一个临时目录：目录消失就是所有 helper 进程的回收 backstop。

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// 假插件的行为脚本。`run`（无 verb）走正常运行分支并监听 query socket；
/// `manifest` verb 只自述，不写 run marker——两条路径必须能分开观测。
const HELPER: &str = r#"# coding: utf-8
import json, os, pathlib, socket, subprocess, sys, time
cfg = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
root = pathlib.Path(cfg["root"])
def mark(key):
    p = pathlib.Path(cfg[key])
    t = p.with_name(p.name + ".tmp")
    t.write_text(str(os.getpid()), encoding="ascii")
    os.replace(t, p)
def hold():
    end = time.monotonic() + 60
    while root.exists() and time.monotonic() < end:
        time.sleep(0.01)
verb = sys.argv[2] if len(sys.argv) > 2 else "run"
if verb == "hold":
    mark("descendant_marker")
    hold()
    sys.exit(0)
if verb == "manifest":
    mark("manifest_marker")
    mode = cfg["mode"]
    if mode == "hang":
        hold()
        sys.exit(0)
    if mode in ("inherited", "inherited_closed"):
        sinks = {} if mode == "inherited" else {
            "stdout": subprocess.DEVNULL, "stderr": subprocess.DEVNULL,
        }
        subprocess.Popen([sys.executable, __file__, sys.argv[1], "hold"], **sinks)
        until = time.monotonic() + 20
        while not pathlib.Path(cfg["descendant_marker"]).exists():
            if not root.exists() or time.monotonic() >= until:
                sys.exit(72)
            time.sleep(0.01)
    if mode == "eof_live":
        os.close(1)
        os.close(2)
        hold()
        sys.exit(0)
    if mode == "noisy":
        sys.stderr.buffer.write(b"x" * (2 * 1024 * 1024))
        sys.stderr.flush()
    if mode == "continuous":
        end = time.monotonic() + 60
        while root.exists() and time.monotonic() < end:
            sys.stderr.buffer.write(b"x" * 4096)
            sys.stderr.flush()
        sys.exit(0)
    if mode == "nonzero":
        sys.stderr.write("cancelled is only plugin text\nsecond line\n")
        sys.exit(cfg["exit_code"])
    if mode == "invalid_utf8":
        sys.stdout.buffer.write(b"\xff")
        sys.exit(0)
    print(cfg["fresh"], flush=True)
    sys.exit(cfg["exit_code"])
mark("run_marker")
s = socket.socket(socket.AF_UNIX)
s.bind(cfg["query_socket"])
s.listen(16)
s.settimeout(0.1)
try:
    end = time.monotonic() + 60
    while root.exists() and time.monotonic() < end:
        try:
            c, _ = s.accept()
        except socket.timeout:
            continue
        with c:
            c.settimeout(1)
            try:
                data = c.makefile("rb").readline()
                if data:
                    c.sendall(b'{"ok":true}\n')
            except (OSError, socket.timeout):
                pass
finally:
    s.close()
"#;

/// 一个假项目：临时目录 + 若干可安装的插件。
pub(crate) struct PluginFixture {
    root: tempfile::TempDir,
}

/// 一个已安装的假插件，以及测试需要观测/断言的路径。
pub(crate) struct FakePlugin {
    pub command: PathBuf,
    pub stored: PathBuf,
    pub body: PathBuf,
    pub manifest_marker: PathBuf,
    pub descendant_marker: PathBuf,
    pub run_marker: PathBuf,
    pub query_socket: PathBuf,
}

/// 把路径安全地塞进 `sh` 的单引号里。
fn shell_quote(path: &Path) -> String {
    format!(
        "'{}'",
        path.to_str()
            .expect("fixture path is UTF-8")
            .replace('\'', "'\\''")
    )
}

impl PluginFixture {
    pub(crate) fn new() -> Self {
        Self {
            root: tempfile::tempdir().expect("fixture tempdir"),
        }
    }

    pub(crate) fn workdir(&self) -> &Path {
        self.root.path()
    }

    /// 写一份「开关打开」的 flags，供 supervisor 测试注入。
    pub(crate) fn arm_flags(&self) -> PathBuf {
        let path = self.workdir().join("flags.json");
        std::fs::write(
            &path,
            r#"{"version":1,"flags":{"superpowers_kanban":true}}"#,
        )
        .unwrap();
        path
    }

    /// 装一个插件：磁盘上放**旧**清单（`stop_when_disabled=false`），插件自述
    /// 返回**新**清单（`true`）。两者 command 一致，只有那一处不同。
    pub(crate) fn plugin(
        &self,
        name: &str,
        mode: &str,
        payload_bytes: usize,
        exit_code: i32,
    ) -> FakePlugin {
        assert!(
            name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'),
            "fixture plugin names stay shell-safe"
        );
        let root = self.workdir();
        let manifests = root.join(".yi-agent/supervisors");
        std::fs::create_dir_all(&manifests).unwrap();
        let plugin = FakePlugin {
            command: root.join(format!("{name}.sh")),
            stored: manifests.join(format!("{name}.json")),
            body: root.join(format!("{name}.fresh.json")),
            manifest_marker: root.join(format!("{name}.manifest.pid")),
            descendant_marker: root.join(format!("{name}.descendant.pid")),
            run_marker: root.join(format!("{name}.run.pid")),
            query_socket: root.join(format!("{name}.sock")),
        };
        // Unix socket 路径有硬上限；这里不复制生产的哈希回退，太长就直接失败。
        assert!(
            plugin.query_socket.as_os_str().len() <= 103,
            "fixture socket path is too long for a Unix socket"
        );

        let fresh = serde_json::json!({
            "name": name,
            "command": plugin.command,
            "args": ["run", plugin.query_socket.to_str().unwrap(), "x".repeat(payload_bytes)],
            "switch_key": "superpowers_kanban",
            "stop_when_disabled": true,
            "restart_backoff_ms": 50,
            "restart_backoff_max_ms": 200,
            "query_socket": plugin.query_socket,
        });
        let mut stored = fresh.clone();
        stored["stop_when_disabled"] = serde_json::json!(false);
        std::fs::write(&plugin.stored, serde_json::to_vec(&stored).unwrap()).unwrap();
        std::fs::write(&plugin.body, serde_json::to_vec(&fresh).unwrap()).unwrap();

        let helper = root.join(format!("{name}.py"));
        let config = root.join(format!("{name}.config.json"));
        std::fs::write(&helper, HELPER).unwrap();
        let cfg = serde_json::json!({
            "root": root,
            // 完整 JSON **字符串**：helper 直接 print 它，得到原清单正文。
            "fresh": serde_json::to_string(&fresh).unwrap(),
            "mode": mode,
            "exit_code": exit_code,
            "manifest_marker": plugin.manifest_marker,
            "descendant_marker": plugin.descendant_marker,
            "run_marker": plugin.run_marker,
            "query_socket": plugin.query_socket,
        });
        std::fs::write(&config, serde_json::to_vec(&cfg).unwrap()).unwrap();
        // exec 之后 python 就是直接 child，中间不留 `sh`。
        std::fs::write(
            &plugin.command,
            format!(
                "#!/bin/sh\nexec python3 {} {} \"$@\"\n",
                shell_quote(&helper),
                shell_quote(&config)
            ),
        )
        .unwrap();
        std::fs::set_permissions(&plugin.command, std::fs::Permissions::from_mode(0o755)).unwrap();
        plugin
    }

    /// 把自述正文补空白到精确的 stdout 字节数，再改回 config。
    ///
    /// JSON 尾部空白合法；这样能精确控制「恰好等于上限」与「超一字节」。
    /// helper 的 `print` 还会追加一个换行，所以补齐到 `stdout_bytes - 1` 才是
    /// 恰好 `stdout_bytes`。
    pub(crate) fn set_manifest_stdout_bytes(&self, plugin: &FakePlugin, stdout_bytes: usize) {
        let fresh = std::fs::read(&plugin.body).expect("fixture body");
        let mut text = String::from_utf8(fresh).expect("fixture body is UTF-8");
        let target = stdout_bytes.saturating_sub(1);
        if text.len() < target {
            text.reserve(target - text.len());
            text.extend(std::iter::repeat_n(' ', target - text.len()));
        }
        assert_eq!(
            text.len(),
            target,
            "padding must hit the byte target exactly"
        );
        let config = plugin.command.parent().unwrap().join(format!(
            "{}.config.json",
            plugin.command.file_stem().unwrap().to_str().unwrap()
        ));
        let raw = std::fs::read(&config).expect("fixture config");
        let mut cfg: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        cfg["fresh"] = serde_json::json!(text);
        std::fs::write(&config, serde_json::to_vec(&cfg).unwrap()).unwrap();
    }
}
