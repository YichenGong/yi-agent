#!/usr/bin/env python3
"""Measure `thread/resume` cost of a real app-server against real session logs.

Read-only: it spawns `yi-agent app-server --listen stdio://`, sends
`initialize` then `thread/resume` for the longest and a short thread, and prints
wall-clock numbers. It never writes to the repository or to any session.

Why this exists: `docs/superpowers/specs/2026-11-06-transcript-window-design.md`
separates the server half of "opening a long session" from the client half. This
script reproduces the server half so the split can be re-checked instead of
remembered.

Usage:
    python3 scripts/bench_resume.py [--project DIR] [--bin PATH] [--threads DIR]

A worktree usually has no thread logs of its own, so `--threads` points at a
real `.yi-agent/threads` (for example the main checkout's) while `--project`
stays the worktree being measured.

Defaults: project = the repository containing this script's parent, bin =
<project>/yi-agent-rs/target/release/yi-agent (falls back to target/debug).
"""

import argparse
import json
import os
import subprocess
import sys
import time


def parse_args():
    here = os.path.dirname(os.path.abspath(__file__))
    project = os.path.dirname(here)
    p = argparse.ArgumentParser()
    p.add_argument("--project", default=project)
    p.add_argument(
        "--threads",
        default=None,
        help="directory holding thread-*.jsonl (default: <project>/.yi-agent/threads)",
    )
    p.add_argument("--bin", default=None)
    return p.parse_args()


def resolve_bin(project, override):
    if override:
        return override
    for rel in ("yi-agent-rs/target/release/yi-agent", "yi-agent-rs/target/debug/yi-agent"):
        candidate = os.path.join(project, rel)
        if os.path.exists(candidate):
            return candidate
    sys.exit("no yi-agent binary found; build one or pass --bin")


class Server:
    def __init__(self, binary, project):
        env = dict(os.environ)
        env["YI_AGENT_DISABLE_WATCHMAN"] = "1"
        self.p = subprocess.Popen(
            [binary, "app-server", "--listen", "stdio://"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            cwd=project,
            env=env,
            text=True,
            bufsize=1,
        )
        self.next_id = 0

    def call(self, method, params):
        """Send one request; return (total_ms, first_replay_frame_ms, items, frames)."""
        self.next_id += 1
        rid = self.next_id
        t0 = time.perf_counter()
        self.p.stdin.write(
            json.dumps({"jsonrpc": "2.0", "id": rid, "method": method, "params": params}) + "\n"
        )
        self.p.stdin.flush()
        first_frame = None
        items = 0
        # Only `items/completed` frames: the reply also carries `thread/started`
        # and `token/usage` notifications, so counting every line overstates the
        # replay frame count (observed 26 chunks reported as 29).
        frames = 0
        while True:
            line = self.p.stdout.readline()
            if not line:
                raise RuntimeError("app-server closed before answering %s" % method)
            try:
                value = json.loads(line)
            except ValueError:
                continue
            if value.get("method") == "items/completed":
                frames += 1
                items += len(value["params"]["items"])
                if first_frame is None:
                    first_frame = time.perf_counter() - t0
            if value.get("id") == rid:
                return (
                    (time.perf_counter() - t0) * 1000,
                    (first_frame or 0) * 1000,
                    items,
                    frames,
                    value.get("error"),
                )

    def close(self):
        try:
            self.p.stdin.close()
        except Exception:
            pass
        try:
            self.p.wait(timeout=10)
        except Exception:
            self.p.kill()


def thread_logs(directory):
    d = directory
    if not os.path.isdir(d):
        sys.exit("no thread logs under %s (pass --threads)" % d)
    out = []
    for name in os.listdir(d):
        if not name.endswith(".jsonl"):
            continue
        path = os.path.join(d, name)
        out.append((name[: -len(".jsonl")], os.path.getsize(path)))
    out.sort(key=lambda x: -x[1])
    return out


def main():
    args = parse_args()
    binary = resolve_bin(args.project, args.bin)
    logs = thread_logs(args.threads or os.path.join(args.project, ".yi-agent", "threads"))
    if not logs:
        sys.exit("no thread logs")

    longest_id, longest_size = logs[0]
    # A short-but-non-empty thread stands in for "ordinary session".
    short = [x for x in logs if x[1] > 2000]
    short_id, short_size = short[-1] if short else logs[-1]

    print("binary : %s" % binary)
    print("project: %s" % args.project)
    print("longest: %s (%.1f MB)" % (longest_id, longest_size / 1048576))
    print("short  : %s (%.1f MB)" % (short_id, short_size / 1048576))
    print()

    server = Server(binary, args.project)
    try:
        total, _, _, _, err = server.call("initialize", {})
        print("initialize            %7.0f ms  err=%s" % (total, err))
        for label, tid in (("SHORT ", short_id), ("LONG  ", longest_id)):
            total, first, items, frames, err = server.call(
                "thread/resume", {"threadId": tid}
            )
            print(
                "%s thread/resume   %7.0f ms  first_frame=%6.0f ms  items=%5d frames=%2d err=%s"
                % (label, total, first, items, frames, err)
            )
            total, first, _, _, err = server.call("thread/resume", {"threadId": tid})
            print("%s resume (warm)   %7.0f ms  first_frame=%6.0f ms" % (label, total, first))
    finally:
        server.close()

    print()
    print("The client half of the same measurement lives in")
    print("  desktop/scripts/perf/open.mjs")


if __name__ == "__main__":
    main()
