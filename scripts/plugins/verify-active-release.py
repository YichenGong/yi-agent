#!/usr/bin/env python3
"""校验已激活的插件发布与其自身清单一致。

验收（T27 Step 4）要求「插件摘要、活跃版本/代次和 skill 来源」都对得上被激活的
那一代。只比较「UI 标记变了」不足以证明这一点：清单里的 digest 可以与磁盘内容
不一致，skill 路径可以指向空目录，纯 UI 插件的 `program` 也可能被悄悄写成别的
二进制。

用法：verify-active-release.py <installed-release-dir> <expected-version>
退出码 0 表示一致，1 表示有不一致项（逐条打印 FAIL）。
"""

import hashlib
import json
import sys
from pathlib import Path


def main(argv: list[str]) -> int:
    if len(argv) != 3:
        print(f"usage: {argv[0]} <release-dir> <expected-version>", file=sys.stderr)
        return 2
    root = Path(argv[1])
    expected = argv[2]

    manifest_path = root / "plugin.json"
    if not manifest_path.is_file():
        print(f"FAIL {manifest_path} is missing", file=sys.stderr)
        return 1
    manifest = json.loads(manifest_path.read_text())

    failures: list[str] = []

    if manifest.get("version") != expected:
        failures.append(
            f"version is {manifest.get('version')!r}, expected {expected!r}"
        )

    # 这个插件是纯 UI + skill，不带原生程序。`None` 是正确值；出现任何路径都
    # 说明包内容与「只发 UI」的说法不符。
    if manifest.get("program", "absent") is not None:
        failures.append(
            f"program is {manifest.get('program')!r}, but this plugin ships no program"
        )

    views = manifest.get("views", [])
    if not views:
        failures.append("manifest declares no views")
    for view in views:
        entry = root / view["entry"]
        if not entry.is_file():
            failures.append(f"view {view['id']} entry {view['entry']} is missing")
            continue
        digest = hashlib.sha256(entry.read_bytes()).hexdigest()
        if digest != view["digest"]:
            failures.append(
                f"view {view['id']} digest {view['digest'][:12]} != on-disk {digest[:12]}"
            )

    # skill 来源必须是包内真实存在的一份 SKILL.md，否则宿主找不到上游工作流。
    skills = manifest.get("skills", [])
    if not skills:
        failures.append("manifest declares no skills")
    for skill in skills:
        path = root / skill["path"] / "SKILL.md"
        if not path.is_file():
            failures.append(f"skill {skill['name']} has no SKILL.md under {skill['path']}")
        else:
            print(f"  skill {skill['name']} -> {skill['path']}/SKILL.md")

    print(
        f"  active version {manifest.get('version')}, "
        f"{len(views)} views and {len(skills)} skills verified"
    )
    for failure in failures:
        print(f"  FAIL {failure}", file=sys.stderr)
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
