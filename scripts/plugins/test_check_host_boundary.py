#!/usr/bin/env python3
"""`check-host-boundary.py` 的测试。

判据有两面，缺一不可：

- 带旧 import / 硬编码注册 / 看板私有 flag 的**人工 fixture 必须失败**，
  否则这个检查对真实残留是瞎的；
- 把 id 当数据用（测试夹具、迁移声明、插件目录）**不能误报**，否则下一步
  就会有人给它加白名单，然后检查形同虚设。
"""

from __future__ import annotations

import sys
import tempfile
import textwrap
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import importlib.util

spec = importlib.util.spec_from_file_location(
    "check_host_boundary", Path(__file__).resolve().parent / "check-host-boundary.py"
)
check = importlib.util.module_from_spec(spec)
assert spec.loader is not None
spec.loader.exec_module(check)


class BoundaryCheckTest(unittest.TestCase):
    def scan(self, files: dict[str, str]) -> list[check.Finding]:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            for rel, text in files.items():
                path = root / rel
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(textwrap.dedent(text), encoding="utf-8")
            return check.scan(root, ["desktop/src", "yi-agent-rs/crates"])

    # --- 必须失败的人工 fixture -------------------------------------------

    def test_a_host_component_importing_the_board_view_fails(self):
        findings = self.scan(
            {
                "desktop/src/App.tsx": """
                    import { SuperpowersKanbanView } from "./components/SuperpowersKanbanView";
                    export function App() { return null; }
                """
            }
        )
        self.assertTrue(findings, "importing a board component must fail")

    def test_a_host_hook_importing_use_board_state_fails(self):
        findings = self.scan(
            {
                "desktop/src/App.tsx": """
                    import { useBoardState } from "./hooks/useBoardState";
                """
            }
        )
        self.assertEqual([f.rule for f in findings], ["forbidden-import"])

    def test_a_rust_slash_command_switch_naming_kanban_fails(self):
        findings = self.scan(
            {
                "yi-agent-rs/crates/yi-agent/src/tui/slash.rs": """
                    match name {
                        "kanban" => Command::Kanban,
                        _ => Command::Unknown,
                    }
                """
            }
        )
        self.assertTrue(findings, "a bespoke /kanban switch is a business path")

    def test_a_hardcoded_kanban_flag_registration_fails(self):
        findings = self.scan(
            {
                "yi-agent-rs/crates/yi-agent-flags/src/registry.rs": """
                    const KANBAN_PLUGIN: &str = "superpowers-kanban";
                """
            }
        )
        self.assertTrue(findings)

    # --- 不能误报 ---------------------------------------------------------

    def test_a_plain_string_in_a_test_fixture_does_not_fail(self):
        findings = self.scan(
            {
                "yi-agent-rs/crates/yi-agent-supervisors/tests/supervisor.rs": """
                    let manifest = json!({"name": "superpowers-kanban"});
                """
            }
        )
        self.assertEqual(findings, [])

    def test_the_legacy_probe_may_name_the_plugin(self):
        findings = self.scan(
            {
                "yi-agent-rs/crates/yi-agent-plugins/src/legacy.rs": """
                    //! 迁移探测要按清单自述找旧安装。
                    let binaries = ["superpowers-kanban"];
                """
            }
        )
        self.assertEqual(findings, [])

    def test_plugin_code_may_mention_itself(self):
        findings = self.scan(
            {
                "plugins/superpowers-kanban/ui/src/host.ts": """
                    import { SuperpowersKanbanView } from "./View";
                """
            }
        )
        self.assertEqual(findings, [])

    def test_a_test_file_is_not_a_loophole(self):
        # 豁免测试文件会让「在测试里 import 宿主的看板组件」变成一条暗门。
        # 纯夹具数据本来就不匹配任何规则（见下一条），所以不需要按路径豁免。
        findings = self.scan(
            {
                "desktop/src/components/SuperpowersKanbanBoard.test.tsx": """
                    import { SuperpowersKanbanView } from "./SuperpowersKanbanView";
                """,
                "yi-agent-rs/crates/yi-agent-app-server/src/server/plugins_tests.rs": """
                    const KANBAN_PLUGIN: &str = "superpowers-kanban";
                """,
            }
        )
        self.assertEqual(
            sorted({(f.path, f.rule) for f in findings}),
            [
                ("desktop/src/components/SuperpowersKanbanBoard.test.tsx", "forbidden-import"),
                ("yi-agent-rs/crates/yi-agent-app-server/src/server/plugins_tests.rs", "forbidden-registration"),
            ],
        )

    def test_the_legacy_compat_surface_may_name_the_plugin(self):
        # 旧登记表路径与旧 requester 名是**磁盘契约**：改名字等于让老机器上的
        # 看板凭空消失。它是唯一被逐项记账的兼容面，允许出现。
        findings = self.scan(
            {
                "yi-agent-rs/crates/yi-agent-app-server/src/legacy_compat.rs": """
                    pub const SUPERPOWERS_KANBAN_REQUESTER: &str = "superpowers-kanban";
                """
            }
        )
        self.assertEqual(findings, [])

    def test_a_second_plugin_needs_no_rule_change(self):
        # 检查的是"看板专属路径"，不是"任何插件"：另一个插件的名字不会
        # 因为出现在主程序里而被这条规则误伤（它的通用性由 T19/T22 保证）。
        findings = self.scan(
            {
                "desktop/src/App.tsx": """
                    const name = "example-counter";
                """
            }
        )
        self.assertEqual(findings, [])


if __name__ == "__main__":
    unittest.main()
