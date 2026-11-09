#!/usr/bin/env python3
"""接线守卫：确认门禁真的挂在 CI 与 just 上，而不是只存在于仓库里。

为什么需要静态守卫：一个没人调用的门禁等于没有门禁。而"CI 里出现过这个字符串"
又太弱——注释、别的 job、甚至被删掉依赖的 recipe 都能满足全局 grep。所以这里
按固定缩进切出具体的 just recipe 与 CI job，只认明确的结构；遇到看不懂的动态
写法返回错误，而不是猜测放行。

只用标准库（pathlib / re / sys），不引入 PyYAML：CI 文件的结构是稳定的，加一个
解析依赖反而会引入"解析器版本漂移"这类新故障。
"""

from __future__ import annotations

import pathlib
import re
import sys

JUSTFILE = "yi-agent-rs/justfile"
GITHUB_CI = ".github/workflows/ci.yml"
GITLAB_CI = ".gitlab-ci.yml"

GATE_RECIPE = "plugin-daemon-socket-check"
CONTRACTS_RECIPE = "plugin-contracts-check"
GATE_SCRIPT = "check-daemon-socket-contract.sh"
WIRING_SCRIPT = "check-daemon-socket-wiring.py"
GATE_TARGET = "cross_process_daemon_socket_contract"


def _lines(path: pathlib.Path) -> list[str]:
    try:
        return path.read_text().splitlines()
    except FileNotFoundError:
        return []


def _strip_comment(line: str) -> str:
    """去掉行尾注释；`#` 在引号里不算注释。"""
    in_single = in_double = False
    for index, char in enumerate(line):
        if char == "'" and not in_double:
            in_single = not in_single
        elif char == '"' and not in_single:
            in_double = not in_double
        elif char == "#" and not in_single and not in_double:
            return line[:index]
    return line


def just_recipe(root: pathlib.Path, name: str) -> tuple[list[str], list[str]] | None:
    """返回 (依赖列表, 主体行列表)；找不到返回 None。

    just 的 recipe 是顶层、零缩进的一行 `name: deps`，随后是缩进的命令行。
    这里只接受这种形态：写成变量拼接或 `{{...}}` 动态依赖的，直接判为"不认识"。
    """
    lines = _lines(root / JUSTFILE)
    header = re.compile(r"^([A-Za-z0-9_.-]+)\s*:\s*(.*)$")
    for index, raw in enumerate(lines):
        if raw.startswith((" ", "\t")) or not raw.strip():
            continue
        match = header.match(_strip_comment(raw).rstrip())
        if not match or match.group(1) != name:
            continue
        if "{{" in match.group(2) or "{{" in raw:
            return None
        deps = match.group(2).split()
        body: list[str] = []
        for follow in lines[index + 1 :]:
            if follow.strip() and not follow.startswith((" ", "\t")):
                break
            body.append(follow)
        return deps, body
    return None


def _just_command(recipe: tuple[list[str], list[str]] | None, needle: str) -> bool:
    if recipe is None:
        return False
    return any(needle in _strip_comment(line) for line in recipe[1])


def _top_level_blocks(lines: list[str], pattern: str) -> dict[str, list[str]]:
    """按零缩进切块：`pattern` 匹配的行是块头，后续缩进行属于该块。"""
    header = re.compile(pattern)
    blocks: dict[str, list[str]] = {}
    current: str | None = None
    for raw in lines:
        if not raw.strip() or raw.lstrip().startswith("#"):
            continue
        if not raw.startswith((" ", "\t")):
            match = header.match(_strip_comment(raw).rstrip())
            if match:
                current = match.group(1)
                blocks[current] = []
                continue
            if not raw.startswith((" ", "\t")):
                current = None
                continue
        if current is not None:
            blocks[current].append(raw)
    return blocks


def _github_job(root: pathlib.Path, job: str) -> list[str] | None:
    """GitHub 的 job 是 `jobs:` 下**两空格缩进**的块头，不是零缩进。

    因此不能复用零缩进的 `_top_level_blocks`；这里按"恰好两空格 + 名字 + 冒号"
    切块，遇到下一个同级块头或回到零缩进就结束。只认这种形态：写成锚点/复用
    workflow（`uses:`）等动态形式的 job 会被判为"不认识"。
    """
    lines = _lines(root / GITHUB_CI)
    if not lines:
        return None
    jobs_at = None
    for index, raw in enumerate(lines):
        if re.match(r"^jobs\s*:\s*$", raw):
            jobs_at = index
            break
    if jobs_at is None:
        return None
    header = re.compile(r"^  ([A-Za-z0-9_.-]+)\s*:\s*$")
    blocks: dict[str, list[str]] = {}
    current: str | None = None
    for raw in lines[jobs_at + 1 :]:
        if not raw.strip() or raw.lstrip().startswith("#"):
            if current is not None:
                blocks[current].append(raw)
            continue
        if not raw.startswith((" ", "\t")):
            current = None
            continue
        match = header.match(_strip_comment(raw).rstrip())
        if match:
            current = match.group(1)
            blocks[current] = []
            continue
        if current is not None:
            blocks[current].append(raw)
    return blocks.get(job)


def _gitlab_job(root: pathlib.Path, job: str) -> list[str] | None:
    lines = _lines(root / GITLAB_CI)
    if not lines:
        return None
    blocks = _top_level_blocks(lines, r"^([A-Za-z0-9_.-]+)\s*:\s*$")
    return blocks.get(job)


def _gitlab_before_script(job_lines: list[str] | None) -> list[str]:
    if not job_lines:
        return []
    body: list[str] = []
    inside = False
    for raw in job_lines:
        if re.match(r"^\s*before_script\s*:\s*$", raw):
            inside = True
            continue
        if inside:
            if re.match(r"^\s*[A-Za-z_]+\s*:\s*$", raw) and not raw.lstrip().startswith("-"):
                break
            body.append(raw)
    return body


def check(root: pathlib.Path) -> list[str]:
    """返回错误列表；空列表表示接线完整。"""
    errors: list[str] = []

    gate = just_recipe(root, GATE_RECIPE)
    if gate is None:
        errors.append(
            f"{JUSTFILE}: recipe `{GATE_RECIPE}` is missing or uses a dynamic form this guard cannot verify"
        )
    else:
        if not _just_command(gate, GATE_SCRIPT):
            errors.append(
                f"{JUSTFILE}: `{GATE_RECIPE}` must invoke scripts/plugins/{GATE_SCRIPT}"
            )
        if gate[0]:
            errors.append(
                f"{JUSTFILE}: `{GATE_RECIPE}` must not declare dependencies; it is the leaf gate"
            )

    contracts = just_recipe(root, CONTRACTS_RECIPE)
    if contracts is None:
        errors.append(f"{JUSTFILE}: recipe `{CONTRACTS_RECIPE}` is missing")
    else:
        if GATE_RECIPE not in contracts[0]:
            errors.append(
                f"{JUSTFILE}: `{CONTRACTS_RECIPE}` must depend on `{GATE_RECIPE}`"
            )
        if not _just_command(contracts, WIRING_SCRIPT):
            errors.append(
                f"{JUSTFILE}: `{CONTRACTS_RECIPE}` must run scripts/plugins/{WIRING_SCRIPT}"
            )
        if not _just_command(contracts, "test_check_daemon_socket_contract.py"):
            errors.append(
                f"{JUSTFILE}: `{CONTRACTS_RECIPE}` must run the gate self-test"
            )

    # 门禁脚本本身：必须真的以 ignored + exact 跑那条跨进程对照。
    script = root / "scripts" / "plugins" / GATE_SCRIPT
    script_text = script.read_text() if script.exists() else ""
    if not script_text:
        errors.append(f"scripts/plugins/{GATE_SCRIPT} is missing")
    else:
        if GATE_TARGET not in script_text:
            errors.append(f"scripts/plugins/{GATE_SCRIPT} must name `{GATE_TARGET}`")
        if "ignored" not in script_text:
            errors.append(
                f"scripts/plugins/{GATE_SCRIPT} must run the cross-process test with --ignored"
            )
        if "--exact" not in script_text:
            errors.append(
                f"scripts/plugins/{GATE_SCRIPT} must run single tests with --exact"
            )

    # macOS 上必须真的调用 Rust-only 门禁，并且先把 just 装上。
    macos = _github_job(root, "macos")
    if macos is None:
        errors.append(f"{GITHUB_CI}: job `macos` is missing")
    else:
        text = "\n".join(macos)
        if GATE_RECIPE not in text:
            errors.append(
                f"{GITHUB_CI}: job `macos` must run `just {GATE_RECIPE}`"
            )
        if "taiki-e/install-action" not in text:
            errors.append(
                f"{GITHUB_CI}: job `macos` must install just via taiki-e/install-action"
            )
        if "yi-agent-rs" not in text:
            errors.append(
                f"{GITHUB_CI}: job `macos` must run the gate from `yi-agent-rs`"
            )

    # GitLab 侧：contracts 走现有调用链，但 before_script 必须自带 python3。
    gitlab = _gitlab_job(root, "ci-plugins")
    if gitlab is None:
        errors.append(f"{GITLAB_CI}: job `ci-plugins` is missing")
    else:
        text = "\n".join(gitlab)
        if CONTRACTS_RECIPE not in text:
            errors.append(
                f"{GITLAB_CI}: job `ci-plugins` must run `just {CONTRACTS_RECIPE}`"
            )
        before = "\n".join(_gitlab_before_script(gitlab))
        if "python3" not in before:
            errors.append(
                f"{GITLAB_CI}: job `ci-plugins` must install python3 in before_script"
            )

    return errors


def main(argv: list[str]) -> int:
    root = pathlib.Path(argv[1]).resolve() if len(argv) > 1 else pathlib.Path(__file__).resolve().parent.parent.parent
    errors = check(root)
    for error in errors:
        print(error, file=sys.stderr)
    return 1 if errors else 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
