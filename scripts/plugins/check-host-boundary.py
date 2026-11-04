#!/usr/bin/env python3
"""检查主程序（宿主）里没有看板专属的业务路径。

# 为什么需要这个检查

看板从"内置功能"变成"一个插件"之后，主程序里不该再有任何一处**只因为看板
存在**的代码：专属组件、专属 hook、硬编码的注册、只服务看板的 flag switch。
这类残留不会立刻报错，它只是让"主程序不含看板"这句话变成谎话，并且下一个
插件没法照抄。

# 判据是"结构"，不是"出现过这个词"

看板插件的 id 本身必须允许出现在几个正当位置，否则这个检查会与现实打架：

- **迁移声明**：`legacy.rs` 里按清单探测旧安装，必然要提到这些名字。
- **测试数据**：夹具里写 `superpowers-kanban` 是数据，不是注册。
- **插件目录**：`plugins/` 下面的一切都属于插件自己。

所以判据是"被 import 的路径 / 被硬编码的注册 / 该文件是否在插件目录里"，
而不是"这行里有没有出现 kanban"。一条 grep 就能做完的事不需要这个脚本；
它存在的意义是把**归类**写死成代码，让"新增命中也必须归类"可以执行。
"""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

# 主程序里**不允许**出现的引用形态。
#
# 每条都指向一个具体的、可修复的结构，而不是"这个词不好"：
# - import 一个看板专属模块：主程序依赖插件业务实现。
# - 硬编码一个看板专属注册：加第二个插件时这行必须再改一次。
# - 看板专属的 flag 名：主程序在替插件决定它叫什么。
FORBIDDEN_IMPORTS = [
    re.compile(r"""from\s+["'][^"']*SuperpowersKanban[^"']*["']"""),
    re.compile(r"""from\s+["'][^"']*useBoardState[^"']*["']"""),
    re.compile(r"""from\s+["'][^"']*superpowersKanban(Boards|Switch)[^"']*["']"""),
    re.compile(r"""^\s*import\s+.*\bSuperpowersKanban\w+""", re.MULTILINE),
    re.compile(r"""\buse\s+superpowers_kanban::"""),
    re.compile(r"""\bpub\s+mod\s+superpowers_kanban\b"""),
]

# 硬编码的看板专属注册（路由、flag 注册表、TUI 命令 switch）。
FORBIDDEN_REGISTRATIONS = [
    re.compile(r"""["']superpowers_kanban["']\s*=>"""),
    # 一个只认 "/kanban" 的 match 臂：主程序里出现 `"/kanban" => ...`，
    # 说明它替插件留了一个私有业务入口。
    re.compile(r"""^\s*["']/?kanban["']\s*=>""", re.MULTILINE),
    re.compile(r"""\bKANBAN_PLUGIN\s*:"""),
    re.compile(r"""\bconst\s+KANBAN_PLUGIN\b"""),
]

# 允许提到这些名字的目录：插件自己的代码、迁移探测、测试夹具。
ALLOWED_PATHS = [
    "plugins/",
    "docs/",
    "scripts/plugins/",
]

# 扫描范围：主程序的源码根。
DEFAULT_ROOTS = ["desktop/src", "yi-agent-rs/crates"]

# 明确允许的文件：迁移/探测按清单自述去找旧安装，必须提到具体名字。
ALLOWED_FILES = {
    "yi-agent-rs/crates/yi-agent-plugins/src/legacy.rs",
    "yi-agent-rs/crates/yi-agent-plugins/tests/legacy.rs",
    "yi-agent-rs/crates/yi-agent-supervisors/src/adopt.rs",
    # 旧环境兼容面的**唯一**载体：旧登记表路径、旧 requester 名都是磁盘契约，
    # 删掉它们不是清理而是破坏老机器上的用户数据。该文件自己逐项记账。
    "yi-agent-rs/crates/yi-agent-app-server/src/legacy_compat.rs",
}


class Finding:
    """一条残留。用普通类而不是 dataclass：这个文件会被测试以模块方式动态
    加载，而 Python 3.9 的 dataclass 在那种场景下会因为模块未注册而报错。"""

    __slots__ = ("path", "line", "rule", "text")

    def __init__(self, path: str, line: int, rule: str, text: str) -> None:
        self.path = path
        self.line = line
        self.rule = rule
        self.text = text

    def __eq__(self, other: object) -> bool:
        return (
            isinstance(other, Finding)
            and (self.path, self.line, self.rule, self.text)
            == (other.path, other.line, other.rule, other.text)
        )

    def __repr__(self) -> str:
        return f"Finding({self.path!r}, {self.line}, {self.rule!r}, {self.text!r})"

    def render(self) -> str:
        return f"{self.path}:{self.line}: [{self.rule}] {self.text.strip()}"


def _is_allowed(path: str) -> bool:
    """这个路径整体豁免吗。

    **测试文件不豁免。** 这是刻意的：`json!({"name": "superpowers-kanban"})`
    这类夹具数据本来就不匹配任何规则（规则只认 import、注册用的 `=>`、以及
    `KANBAN_PLUGIN` 常量），所以没必要豁免；而一旦按路径豁免，一个测试里的
    `import { SuperpowersKanbanView }` 就会悄悄通过——测试于是成了绕过检查的
    暗门。测试要放夹具数据，直接放即可。
    """
    if path in ALLOWED_FILES:
        return True
    return any(path.startswith(prefix) for prefix in ALLOWED_PATHS)


def scan_file(path: Path, rel: str) -> list[Finding]:
    try:
        text = path.read_text(encoding="utf-8")
    except (OSError, UnicodeDecodeError):
        return []

    findings: list[Finding] = []
    if _is_allowed(rel):
        return findings

    for rule in FORBIDDEN_IMPORTS:
        for match in rule.finditer(text):
            findings.append(
                Finding(rel, text[: match.start()].count("\n") + 1, "forbidden-import", match.group(0))
            )
    for rule in FORBIDDEN_REGISTRATIONS:
        for match in rule.finditer(text):
            findings.append(
                Finding(
                    rel,
                    text[: match.start()].count("\n") + 1,
                    "forbidden-registration",
                    match.group(0),
                )
            )
    return findings


def scan(root: Path, roots: list[str]) -> list[Finding]:
    findings: list[Finding] = []
    for rel_root in roots:
        base = root / rel_root
        if not base.is_dir():
            continue
        for path in sorted(base.rglob("*")):
            if not path.is_file():
                continue
            rel = path.relative_to(root).as_posix()
            findings.extend(scan_file(path, rel))
    return findings


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", default=".", help="repository root")
    parser.add_argument("--roots", nargs="*", default=DEFAULT_ROOTS)
    args = parser.parse_args(argv)

    root = Path(args.root).resolve()
    findings = scan(root, args.roots)
    if findings:
        for finding in findings:
            print(finding.render(), file=sys.stderr)
        print(
            f"\n{len(findings)} kanban-only path(s) remain in the host. "
            "Either remove them or record why they are legitimate data, not a registration.",
            file=sys.stderr,
        )
        return 1
    print("host boundary check passed: no kanban-only paths in the host")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
