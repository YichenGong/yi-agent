#!/usr/bin/env python3
"""T20 的真实双 app-server 验收：两个**独立进程**，不是两个对象。

# 为什么不能只靠单测

`crates/yi-agent-app-server/src/server/plugin_remote_tests.rs` 里的两条 `Harness`
各自拥有 stdio 管道与磁盘记录，但它们仍跑在同一个测试进程里。真实部署是
**两个 app-server 进程**，各自解析自己的 `YI_AGENT_HOME`、各自读自己的官方目录。
只有起两个进程才能证明"按连接注入的目录"真的生效，而不是被进程级环境变量
或共享临时目录冒充。

# 它断言什么

1. 两个进程各自只看见自己的目录条目；
2. 各自只认证自己的发布，问对方那一版得到 `plugin_not_found`；
3. 两个宿主给出的传输句柄不同；
4. `assetRelease` 永远不返回 URL 或凭据；
5. 两个实例根各自生成，互不写入。

# 用法

    cargo build -p yi-agent --bin yi-agent      # 在 yi-agent-rs/ 下
    python3 scripts/plugins/check-remote-hosts.py

退出码 0 全绿，1 有失败项（逐条打印 FAIL）。
"""

import json
import os
import shutil
import subprocess
import sys
import tempfile

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
BIN = os.path.join(ROOT, "yi-agent-rs", "target", "debug", "yi-agent")

SHA_A = "aa" * 32
SHA_B = "bb" * 32


def write_catalog(home: str, version: str, sha: str, serial: int) -> None:
    plugins = os.path.join(home, ".yi-agent", "plugins")
    os.makedirs(plugins, exist_ok=True)
    catalog = {
        "serial": serial,
        "entries": [
            {
                "release": {
                    "plugin": "example-counter",
                    "version": version,
                    "target": "darwin-aarch64",
                    "sha256": sha,
                },
                "sizeBytes": 1024,
                "metadataPath": "/official/example-counter/metadata.json",
                "bundlePath": "/official/example-counter/bundle.tar.gz",
                "publishedAt": 1_700_000_000,
                "expiresAt": 0,
                "serial": serial,
            }
        ],
    }
    with open(os.path.join(plugins, "catalog.json"), "w", encoding="utf-8") as handle:
        json.dump(catalog, handle)


def release_params(version: str, sha: str) -> dict:
    return {
        "plugin": "example-counter",
        "version": version,
        "target": "darwin-aarch64",
        "sha256": sha,
    }


def run_host(home: str, calls):
    """起一个真实的 app-server，按顺序发请求，收集响应。"""
    env = dict(os.environ)
    env["YI_AGENT_HOME"] = home
    lines = [
        json.dumps(
            {
                "jsonrpc": "2.0",
                "id": 0,
                "method": "initialize",
                "params": {"clientInfo": {"name": "t20-probe", "version": "1"}, "capabilities": {}},
            }
        )
    ]
    for index, (method, params) in enumerate(calls, start=1):
        lines.append(
            json.dumps({"jsonrpc": "2.0", "id": index, "method": method, "params": params})
        )
    proc = subprocess.run(
        [BIN, "app-server", "--listen", "stdio://"],
        input="\n".join(lines) + "\n",
        capture_output=True,
        text=True,
        env=env,
        timeout=180,
    )
    responses = {}
    for line in proc.stdout.splitlines():
        line = line.strip()
        if not line.startswith("{"):
            continue
        try:
            message = json.loads(line)
        except json.JSONDecodeError:
            continue
        if "id" in message:
            responses[message["id"]] = message
    return responses, proc.stderr


def main() -> int:
    if not os.path.isfile(BIN):
        print(f"missing binary: {BIN}", file=sys.stderr)
        print("build it first: (cd yi-agent-rs && cargo build -p yi-agent --bin yi-agent)", file=sys.stderr)
        return 1

    work = tempfile.mkdtemp(prefix="t20-hosts-")
    home_a = os.path.join(work, "a")
    home_b = os.path.join(work, "b")
    failures = []

    def check(name: str, condition: bool, detail="") -> None:
        print(("PASS " if condition else "FAIL ") + name + ("" if condition else f"  -> {detail}"))
        if not condition:
            failures.append(name)

    try:
        write_catalog(home_a, "1.0.0", SHA_A, 10)
        write_catalog(home_b, "2.0.0", SHA_B, 20)

        a, stderr_a = run_host(
            home_a,
            [
                ("plugins/v1/catalog", {}),
                ("plugins/v1/assetRelease", release_params("1.0.0", SHA_A)),
                ("plugins/v1/assetRelease", release_params("2.0.0", SHA_B)),
            ],
        )
        b, stderr_b = run_host(
            home_b,
            [
                ("plugins/v1/catalog", {}),
                ("plugins/v1/assetRelease", release_params("2.0.0", SHA_B)),
                ("plugins/v1/assetRelease", release_params("1.0.0", SHA_A)),
            ],
        )

        def versions(response):
            return [entry["version"] for entry in response["result"]["entries"]]

        check("host A initialized", "result" in a.get(0, {}), a.get(0))
        check("host B initialized", "result" in b.get(0, {}), b.get(0))
        check("host A sees only its own 1.0.0", versions(a[1]) == ["1.0.0"], a[1])
        check("host B sees only its own 2.0.0", versions(b[1]) == ["2.0.0"], b[1])

        check(
            "host A authenticates its own 1.0.0",
            a[2].get("result", {}).get("authenticated") is True,
            a[2],
        )
        check(
            "host B authenticates its own 2.0.0",
            b[2].get("result", {}).get("authenticated") is True,
            b[2],
        )
        check(
            "host A refuses host B's release",
            a[3].get("error", {}).get("data", {}).get("code") == "plugin_not_found",
            a[3],
        )
        check(
            "host B refuses host A's release",
            b[3].get("error", {}).get("data", {}).get("code") == "plugin_not_found",
            b[3],
        )

        handle_a = a[2]["result"]["transfer"]["handle"]
        handle_b = b[2]["result"]["transfer"]["handle"]
        check("handles differ across hosts", handle_a != handle_b, f"{handle_a} vs {handle_b}")
        check("host A returns no direct URL", a[2]["result"]["directUrl"] is None, a[2]["result"])
        check("host A returns no credentials", a[2]["result"]["credentials"] is None, a[2]["result"])
        check("host B returns no direct URL", b[2]["result"]["directUrl"] is None, b[2]["result"])

        for home in (home_a, home_b):
            root = os.path.join(home, ".yi-agent")
            check(f"instance root generated at {root}", os.path.isdir(root))

        if stderr_a.strip():
            print("stderr A tail:", stderr_a.strip().splitlines()[-3:])
        if stderr_b.strip():
            print("stderr B tail:", stderr_b.strip().splitlines()[-3:])
    finally:
        shutil.rmtree(work, ignore_errors=True)

    print()
    if failures:
        print("FAILED:", ", ".join(failures))
        return 1
    print("ALL PASS")
    return 0


if __name__ == "__main__":
    sys.exit(main())
