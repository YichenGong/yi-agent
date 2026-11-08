#!/usr/bin/env python3
"""「运行参数」全字段消费覆盖检查。

判据不是"源码里出现过这个键"，而是：把它交给 cargo，**真的跑一次**那个测试，
并且确认这次运行有非零用例、没有被 skip、全部通过。

为什么每一步都要显式失败而不是"尽量继续"：
这个脚本的唯一价值是当某个字段悄悄失去消费时变红。任何一步退化成
"跳过/警告后继续"，就会得到一个永远绿、什么也没验的脚本——比没有更糟。

用法：
    python3 scripts/check-live-settings-coverage.py [--list-only] [--jobs N]

--list-only 只做清单与测试名的存在性校验，不逐项执行（调试用）。
"""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
MANIFEST = ROOT / "scripts" / "runtime-settings-consumers.json"
FIELDS_SOURCE = ROOT / "crates" / "yi-agent-runtime" / "src" / "settings.rs"

# 根目录下没有 .env 也能跑；-p 与 --test 都显式指定，不依赖当前目录。
CARGO = os.environ.get("CARGO", "cargo")


def fail(message: str) -> None:
    print(f"FAIL: {message}", file=sys.stderr)
    raise SystemExit(1)


def read_manifest() -> list[dict]:
    try:
        document = json.loads(MANIFEST.read_text(encoding="utf-8"))
    except FileNotFoundError:
        fail(f"缺少消费清单：{MANIFEST}")
    except json.JSONDecodeError as error:
        fail(f"消费清单不是合法 JSON：{error}")

    entries = document.get("fields")
    if not isinstance(entries, list) or not entries:
        fail("消费清单的 fields 必须是非空数组")

    required = {"key", "package", "target_kind", "target", "test_name", "consumes"}
    for index, entry in enumerate(entries):
        if not isinstance(entry, dict):
            fail(f"fields[{index}] 不是对象")
        missing = required - set(entry)
        if missing:
            fail(f"fields[{index}] 缺少字段：{sorted(missing)}")
        if entry["target_kind"] not in {"lib", "bin", "test"}:
            fail(
                f"fields[{index}].target_kind 必须是 lib/bin/test，"
                f"实际 {entry['target_kind']!r}"
            )
    return entries


def read_source_field_keys() -> list[str]:
    """从 `FIELDS` 静态表里取出真实键集合。

    只解析 `FIELDS` 块：源码别处也有 `key: "..."`（例如迁移用的旧键字面量），
    全域正则会把它们混进来，让"全覆盖"变成一句空话。
    """
    text = FIELDS_SOURCE.read_text(encoding="utf-8")
    start = text.find("pub static FIELDS")
    if start < 0:
        fail("settings.rs 里找不到 `pub static FIELDS`")
    end = text.find("\n];", start)
    if end < 0:
        fail("`FIELDS` 定义没有终止的 `];`")
    block = text[start:end]

    keys = re.findall(r'key:\s*(?:[A-Z_]+|"([a-z_.]+)")', block)
    # 常量形式的键（PREVENT_IDLE_SLEEP_KEY）单独解析，否则会漏掉一个字段。
    literals = [k for k in keys if k]
    constants = re.findall(r"key:\s*([A-Z_]+),", block)
    for constant in constants:
        match = re.search(
            rf'pub const {constant}: &str = "([a-z_.]+)";', text
        )
        if not match:
            fail(f"常量 {constant} 找不到对应的字符串定义")
        literals.append(match.group(1))

    if not literals:
        fail("从 `FIELDS` 解析出零个键——解析逻辑已失效，不能当成通过")
    if len(set(literals)) != len(literals):
        fail(f"`FIELDS` 里有重复键：{sorted(literals)}")
    return sorted(literals)


def check_manifest_covers_every_field(field_keys: list[str], entries: list[dict]) -> None:
    mapped = sorted({entry["key"] for entry in entries})
    missing = sorted(set(field_keys) - set(mapped))
    extra = sorted(set(mapped) - set(field_keys))
    if missing:
        fail(f"以下字段在 FIELDS 里但没有消费测试映射：{missing}")
    if extra:
        fail(f"消费清单里有 FIELDS 中不存在的键：{extra}")
    if len(mapped) != len(field_keys):
        fail(
            f"字段数与映射数不一致：FIELDS {len(field_keys)} 个，"
            f"清单 {len(mapped)} 个"
        )

    duplicated = {
        key for key in mapped if sum(1 for e in entries if e["key"] == key) > 1
    }
    # 同一字段映射到多个测试是允许的（更严），但要显式：这里只拒绝
    # 完全重复的行（同键 + 同测试名），那通常是复制粘贴没改。
    seen: set[tuple[str, str]] = set()
    for entry in entries:
        identity = (entry["key"], entry["test_name"])
        if identity in seen:
            fail(f"消费清单有重复行：{entry['key']} -> {entry['test_name']}")
        seen.add(identity)
    if duplicated:
        print(f"note: 这些键映射了多个测试：{sorted(duplicated)}")


def cargo_args(entry: dict, extra: list[str]) -> list[str]:
    args = [CARGO, "test", "-p", entry["package"], "--offline"]
    if entry["target_kind"] == "lib":
        args += ["--lib"]
    elif entry["target_kind"] == "bin":
        args += ["--bin", entry["target"]]
    else:
        args += ["--test", entry["target"]]
    return args + extra


def run_capture(args: list[str]) -> subprocess.CompletedProcess:
    return subprocess.run(
        args, cwd=ROOT, text=True, capture_output=True, check=False
    )


def list_tests(entry: dict, cache: dict[tuple[str, str, str], set[str]]) -> set[str]:
    """`--list` 拿到该 target 里真实存在的测试名（完整路径）。"""
    identity = (entry["package"], entry["target_kind"], entry["target"])
    if identity in cache:
        return cache[identity]

    result = run_capture(cargo_args(entry, ["--", "--list"]))
    if result.returncode != 0:
        fail(
            "无法列出测试（这条命令本身就失败了，不能当成'没有测试'）：\n"
            f"  {' '.join(cargo_args(entry, ['--', '--list']))}\n"
            f"{result.stdout}\n{result.stderr}"
        )
    names = set()
    for line in result.stdout.splitlines():
        match = re.match(r"^(\S+): test$", line.strip())
        if match:
            names.add(match.group(1))
    if not names:
        fail(
            f"target {identity} 列出零个测试——target 名写错或清单为空，"
            "不能当成通过"
        )
    cache[identity] = names
    return names


def run_one(entry: dict) -> None:
    """精确执行一个测试，并要求：真的运行、不被 skip、通过。"""
    args = cargo_args(entry, ["--exact", entry["test_name"], "--", "--exact"])
    # cargo test 的过滤参数必须放在 `--` 之后：`--exact` 是 test harness 的参数。
    args = cargo_args(entry, ["--", "--exact", entry["test_name"]])
    result = run_capture(args)
    output = f"{result.stdout}\n{result.stderr}"

    if result.returncode != 0:
        fail(
            "测试执行失败：\n"
            f"  {' '.join(args)}\n{output}"
        )

    # 必须看到 "N passed" 且 N >= 1：0 个用例是"名字匹配不上"，
    # 这时 cargo 的退出码是 0，只看退出码会把空跑当成功。
    passed = re.search(r"(\d+) passed", output)
    if not passed or int(passed.group(1)) < 1:
        fail(
            "测试没有真正运行（0 个用例通过，通常是名字匹配不上）：\n"
            f"  {' '.join(args)}\n{output}"
        )

    ignored = re.search(r"(\d+) ignored", output)
    if ignored and int(ignored.group(1)) > 0:
        fail(
            "测试被 ignored 跳过，不能作为消费证据：\n"
            f"  {' '.join(args)}\n{output}"
        )


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--list-only",
        action="store_true",
        help="只校验清单与测试名存在性，不逐项执行",
    )
    parser.parse_args()

    entries = read_manifest()
    field_keys = read_source_field_keys()
    check_manifest_covers_every_field(field_keys, entries)

    cache: dict[tuple[str, str, str], set[str]] = {}
    for entry in entries:
        names = list_tests(entry, cache)
        if entry["test_name"] not in names:
            fail(
                f"{entry['key']} 映射的测试不存在："
                f"{entry['package']}/{entry['target_kind']} {entry['target']} "
                f"-> {entry['test_name']}"
            )

    if not parser.parse_args().list_only:
        for entry in entries:
            print(f"run {entry['key']} -> {entry['test_name']}")
            run_one(entry)

    print(
        f"OK: {len(field_keys)} 个字段全部有可执行的消费测试"
        + ("（仅校验存在性）" if parser.parse_args().list_only else "（已逐项执行）")
    )


if __name__ == "__main__":
    main()
