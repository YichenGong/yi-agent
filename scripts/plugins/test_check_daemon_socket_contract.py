"""`check-daemon-socket-contract.sh` 的自测：用假 cargo 模拟每一阶段的 argv 与输出。

为什么必须自测这个 shell：门禁「永远 exit 1」和门禁「永远 exit 0」在 CI 上都是
绿的噪声。前者没人发现直到有人要靠它放行，后者更糟——它让真正的漂移通过。

做法：把待测脚本复制到临时仓库根的 `scripts/plugins/` 下，造必要的 manifest
占位文件，把 `cargo` 换成一个记录 argv 并按阶段分派输出的假程序。真实 cargo
一次都不跑。
"""

import json
import os
import pathlib
import shutil
import subprocess
import tempfile
import unittest

REPO_ROOT = pathlib.Path(__file__).resolve().parent.parent.parent
SCRIPT = REPO_ROOT / "scripts" / "plugins" / "check-daemon-socket-contract.sh"

# 门禁必须实际执行的 8 个 cargo 阶段：metadata、build、3 个 run_one、IPC、
# daemon_liveness、runner --lib。
EXPECTED_STAGES = 8

# 门禁在输出里逐个核对的关键测试名。
IPC_TESTS = [
    "status_io::tests::nul_is_rejected_before_connect",
    "status_io::tests::connect_backlog_obeys_deadline",
    "status_io::tests::connect_retries_keep_original_deadline",
    "status_io::tests::poll_eintr_keeps_deadline",
    "status_io::tests::failed_attempts_close_descriptors",
    "status_io::tests::short_writes_and_interrupted_io_complete_one_frame",
    "status_io::tests::write_stall_times_out_at_write_stage",
    "status_io::tests::broken_peer_does_not_raise_sigpipe",
    "silent_peer_times_out_during_read",
    "drip_feed_cannot_renew_budget",
    "hup_with_complete_frame_is_not_truncated",
    "valid_status_and_business_error_are_replies",
    "truncated_frame_is_rejected",
    "malformed_frame_is_rejected",
    "oversized_stream_is_bounded",
    "protocol_and_request_id_mismatches_are_rejected",
    "frame_limit_excludes_newline",
]

RUNNER_TESTS = [
    "new_socket_keeps_same_pid_and_old_sockets_are_untouched",
    "three_misses_exit_and_release_lock",
    "disabled_switch_still_probes",
    "success_resets_the_streak",
    "silent_status_peer_exits_under_empty_loop_budget",
    "missing_instance_home_fails_before_state_side_effects",
    "long_instance_home_fails_before_state_side_effects",
    "bound_card_state_is_not_changed_by_probing",
]

RUNNER_LIB_TESTS = [
    "client::tests::probe_preserves_error_and_accepts_business_error",
    "single_instance::tests::misses_accessor_tracks_only_consecutive_failures",
]

# 假 cargo 的行为开关，通过环境变量注入，避免为每个负例写一份假程序。
FAKE_CARGO = r"""#!/usr/bin/env python3
import json, os, sys

args = sys.argv[1:]
with open(os.environ["FAKE_LOG"], "a") as handle:
    handle.write(json.dumps(args) + "\n")

mode = os.environ.get("FAKE_MODE", "ok")
target_dir = os.environ["FAKE_TARGET_DIR"]

def emit(text):
    sys.stdout.write(text)

if args[0] == "metadata":
    if mode == "bad_metadata":
        emit("this is not json\n")
        sys.exit(0)
    emit(json.dumps({"target_directory": target_dir}) + "\n")
    sys.exit(0)

if args[0] == "build":
    if mode == "no_artifact":
        sys.exit(0)
    exe = os.path.join(target_dir, "debug", "examples", "daemon_socket_contract_probe")
    if mode == "cross_target":
        exe = os.path.join(target_dir, "other", "examples", "daemon_socket_contract_probe")
    # 真实 cargo 会写产物；门禁会检查它存在且可执行，所以假程序也必须写。
    os.makedirs(os.path.dirname(exe), exist_ok=True)
    with open(exe, "w") as handle:
        handle.write("#!/bin/sh\nexit 0\n")
    os.chmod(exe, 0o755)
    print(json.dumps({
        "reason": "compiler-artifact",
        "target": {"name": "daemon_socket_contract_probe", "kind": ["example"]},
        "executable": exe,
    }))
    sys.exit(0)

def single_test_name(args):
    # run_one 的形式是 `... <name> -- --exact [--ignored]`；整包回归是
    # `... -- --format pretty --color never`，`--` 前一个 token 是 flag。
    # run_one 一定带 --exact（可能再加 --ignored）；整包回归不带这两个。
    if "--exact" not in args:
        return None
    index = args.index("--")
    if index == 0:
        return None
    before = args[index - 1]
    if before.startswith("-"):
        return None
    return before

if args[0] == "test":
    name = single_test_name(args) or ""
    if mode == "zero_tests":
        emit("running 0 tests\n\ntest result: ok. 0 passed; 0 failed; 0 ignored;\n")
        sys.exit(0)
    if single_test_name(args):
        # 单测试阶段
        if mode == "missing_io_test" and name == "hup_with_complete_frame_is_not_truncated":
            emit("running 0 tests\n\ntest result: ok. 0 passed; 0 failed; 0 ignored;\n")
            sys.exit(0)
        emit("running 1 test\ntest %s ... ok\n\n"
             "test result: ok. 1 passed; 0 failed; 0 ignored;\n" % name)
        sys.exit(0)

    # 整包阶段：按 -p 与 --test 判断是哪个包
    if mode == "missing_io_test" and "--lib" in args:
        emit("running 1 test\ntest client::tests::probe_preserves_error_and_accepts_business_error ... ok\n"
             "test single_instance::tests::misses_accessor_tracks_only_consecutive_failures ... ok\n\n"
             "test result: ok. 2 passed; 0 failed; 0 ignored;\n")
        sys.exit(0)
    if mode == "fail_exit" and args.count("--lib") == 1:
        emit("running 1 test\ntest whatever ... ok\n\n"
             "test result: ok. 1 passed; 0 failed; 0 ignored;\n")
        sys.exit(3)
    if "--lib" in args:
        lines = ["running %d tests" % len(os.environ["FAKE_RUNNER_LIB_TESTS"].split(","))]
        for name in os.environ["FAKE_RUNNER_LIB_TESTS"].split(","):
            lines.append("test %s ... ok" % name)
        lines.append("")
        lines.append("test result: ok. %d passed; 0 failed; 0 ignored;"
                     % len(os.environ["FAKE_RUNNER_LIB_TESTS"].split(",")))
        if mode == "missing_io_test":
            lines = lines[:-1] + ["test result: ok. 1 passed; 0 failed; 0 ignored;"]
        emit("\n".join(lines) + "\n")
        sys.exit(0)
    if "daemon_liveness" in args:
        names = os.environ["FAKE_RUNNER_TESTS"].split(",")
    else:
        names = os.environ["FAKE_IPC_TESTS"].split(",")
    lines = ["running %d tests" % len(names)]
    for name in names:
        if mode == "missing_io_test" and name == "hup_with_complete_frame_is_not_truncated":
            continue
        lines.append("test %s ... ok" % name)
    lines.append("")
    lines.append("test result: ok. %d passed; 0 failed; 0 ignored;" % len(names))
    emit("\n".join(lines) + "\n")
    sys.exit(0)

sys.exit(2)
"""


class GateHarness:
    """在临时仓库根里跑门禁脚本。"""

    def __init__(self, tmp: pathlib.Path, mode: str = "ok"):
        self.root = tmp / "repo"
        self.plugins = self.root / "scripts" / "plugins"
        self.plugins.mkdir(parents=True)
        shutil.copy2(SCRIPT, self.plugins / "check-daemon-socket-contract.sh")
        # 门禁只要求这些 manifest 路径存在（真实解析交给 cargo）。
        for rel in [
            "yi-agent-rs/Cargo.toml",
            "plugins/superpowers-kanban/Cargo.toml",
        ]:
            path = self.root / rel
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text("[workspace]\n")

        self.bin = tmp / "bin"
        self.bin.mkdir()
        cargo = self.bin / "cargo"
        cargo.write_text(FAKE_CARGO)
        cargo.chmod(0o755)
        # 门禁开头 `command -v python3`。
        if not shutil.which("python3"):
            raise RuntimeError("python3 is required for the gate self-test")

        self.log = tmp / "cargo-argv.log"
        self.log.write_text("")
        self.target = tmp / "target"
        self.target.mkdir()
        self.mode = mode

    def run(self):
        env = {
            "PATH": "%s:%s" % (self.bin, os.environ["PATH"]),
            "FAKE_LOG": str(self.log),
            "FAKE_MODE": self.mode,
            "FAKE_TARGET_DIR": str(self.target),
            "FAKE_IPC_TESTS": ",".join(IPC_TESTS),
            "FAKE_RUNNER_TESTS": ",".join(RUNNER_TESTS),
            "FAKE_RUNNER_LIB_TESTS": ",".join(RUNNER_LIB_TESTS),
            "HOME": str(self.root),
        }
        return subprocess.run(
            ["/bin/bash", str(self.plugins / "check-daemon-socket-contract.sh")],
            env=env,
            capture_output=True,
            text=True,
            timeout=120,
        )

    def stages(self):
        rows = [json.loads(line) for line in self.log.read_text().splitlines() if line.strip()]
        return len(rows)


class GateSelfTest(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.tmp = pathlib.Path(self._tmp.name)

    def tearDown(self):
        self._tmp.cleanup()

    def assert_rejected(self, mode, needle=None):
        harness = GateHarness(self.tmp, mode=mode)
        result = harness.run()
        self.assertNotEqual(result.returncode, 0, "mode %r must be rejected; stdout=%s stderr=%s"
                            % (mode, result.stdout, result.stderr))
        if needle:
            combined = result.stdout + result.stderr
            self.assertIn(needle, combined, "stderr must point at the cause")

    def test_valid_gate_executes_required_tests(self):
        harness = GateHarness(self.tmp)
        result = harness.run()
        self.assertEqual(result.returncode, 0, "stdout=%s stderr=%s" % (result.stdout, result.stderr))
        self.assertEqual(
            harness.stages(),
            EXPECTED_STAGES,
            "every required stage must actually run, argv log: %s" % harness.log.read_text(),
        )
        # 阴性对照：正常路径必须真的核对测试名，而不是只看 exit code。
        for name in ("cross_process_daemon_socket_contract", "root_vector_and_fixture_schema_are_pinned"):
            self.assertIn(name, result.stdout)

    def test_missing_probe_is_not_success(self):
        # build exit 0 但没有 example 产物：必须拒绝，不能因为「build 成功」就放行。
        self.assert_rejected("no_artifact", "missing or ambiguous host probe artifact")

    def test_stale_or_cross_target_probe_is_rejected(self):
        self.assert_rejected("cross_target", "cross-target or stale artifact path")

    def test_invalid_metadata_is_rejected(self):
        harness = GateHarness(self.tmp, mode="bad_metadata")
        result = harness.run()
        self.assertNotEqual(result.returncode, 0, "invalid metadata must fail")
        self.assertNotIn("running 1 test", result.stdout, "no test stage may run after bad metadata")

    def test_zero_cross_tests_is_rejected(self):
        # 单测试阶段报 "running 0 tests"：门禁的 `grep -Fqx 'running 1 test'` 必须失败。
        self.assert_rejected("zero_tests", "running 0 tests")

    def test_missing_required_io_test_is_rejected(self):
        # 整包 IPC 回归里少一条关键 IO 测试名：grep -Fqx 必须让门禁失败。
        self.assert_rejected("missing_io_test")

    def test_exit_zero_without_real_result_is_rejected(self):
        # 输出看起来成功但进程 exit 非零：不得被当成通过。
        harness = GateHarness(self.tmp, mode="fail_exit")
        result = harness.run()
        self.assertNotEqual(result.returncode, 0, "a non-zero test exit must fail the gate")


if __name__ == "__main__":
    unittest.main()


class WiringGuardTest(unittest.TestCase):
    """`check-daemon-socket-wiring.py` 的负例：每条接线都必须被点名拦下。

    用临时 root 造合法结构再逐项破坏，永远不动真实仓库文件。
    """

    JUSTFILE = """\
plugin-daemon-socket-check:
    bash ../scripts/plugins/check-daemon-socket-contract.sh

plugin-contracts-check: plugin-daemon-socket-check
    python3 ../scripts/plugins/check-daemon-socket-wiring.py
    python3 -m unittest discover -s ../scripts/plugins -p 'test_check_daemon_socket_contract.py'
"""

    GITHUB = """\
jobs:
  macos:
    runs-on: macos-15
    steps:
      - name: Install just
        uses: taiki-e/install-action@v2
        with:
          tool: just
      - name: Check
        working-directory: yi-agent-rs
        run: just plugin-daemon-socket-check
"""

    GITLAB = """\
ci-plugins:
  stage: check
  before_script:
    - apt-get update && apt-get install -y --no-install-recommends git python3 >/dev/null
  script:
    - just plugin-contracts-check
"""

    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.root = pathlib.Path(self._tmp.name)
        (self.root / "yi-agent-rs").mkdir()
        (self.root / ".github" / "workflows").mkdir(parents=True)
        (self.root / "scripts" / "plugins").mkdir(parents=True)
        (self.root / "yi-agent-rs" / "justfile").write_text(self.JUSTFILE)
        (self.root / ".github" / "workflows" / "ci.yml").write_text(self.GITHUB)
        (self.root / ".gitlab-ci.yml").write_text(self.GITLAB)
        (self.root / "scripts" / "plugins" / "check-daemon-socket-contract.sh").write_text(
            "run_one cross_process_daemon_socket_contract ignored\n"
            "flags=(--exact)\n"
            "flags+=(--ignored)\n"
        )

    def tearDown(self):
        self._tmp.cleanup()

    def check(self):
        module = _load_wiring()
        return module.check(self.root)

    def rewrite(self, relative, old, new):
        path = self.root / relative
        text = path.read_text()
        self.assertIn(old, text)
        path.write_text(text.replace(old, new, 1))

    def test_valid_layout_has_no_errors(self):
        self.assertEqual(self.check(), [])

    def test_missing_recipe_dependency_is_rejected(self):
        self.rewrite(
            "yi-agent-rs/justfile",
            "plugin-contracts-check: plugin-daemon-socket-check",
            "plugin-contracts-check:",
        )
        errors = "\n".join(self.check())
        self.assertIn("must depend on `plugin-daemon-socket-check`", errors)

    def test_missing_macos_gate_call_is_rejected(self):
        self.rewrite(
            ".github/workflows/ci.yml",
            "run: just plugin-daemon-socket-check",
            "run: just some-other-check",
        )
        errors = "\n".join(self.check())
        self.assertIn("must run `just plugin-daemon-socket-check`", errors)

    def test_missing_just_install_is_rejected(self):
        self.rewrite(
            ".github/workflows/ci.yml",
            "uses: taiki-e/install-action@v2",
            "uses: some/other-action@v1",
        )
        errors = "\n".join(self.check())
        self.assertIn("must install just", errors)

    def test_missing_gitlab_python3_is_rejected(self):
        self.rewrite(".gitlab-ci.yml", "git python3", "git")
        errors = "\n".join(self.check())
        self.assertIn("must install python3", errors)

    def test_missing_cross_target_name_is_rejected(self):
        self.rewrite(
            "scripts/plugins/check-daemon-socket-contract.sh",
            "cross_process_daemon_socket_contract",
            "some_other_test",
        )
        errors = "\n".join(self.check())
        self.assertIn("must name `cross_process_daemon_socket_contract`", errors)


def _load_wiring():
    import importlib.util

    path = pathlib.Path(__file__).resolve().parent / "check-daemon-socket-wiring.py"
    spec = importlib.util.spec_from_file_location("check_daemon_socket_wiring", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module
