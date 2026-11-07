#!/usr/bin/env python3
"""按分段统计 yi-agent 一次「互动」的延时。

数据源：app-server 已内建的 tracing JSONL（`~/.yi-agent/trace/session-*.jsonl`）。

每条链路的计时锚点（都是我代码里已有的 info! 日志）：

    sending request                       provider HTTP 请求发出
    response received                     HTTP 响应头到达（TTFB）
    accumulate_stream: received Stop      SSE 流读到 Stop（正文收完）
    tool call start / tool call done      单次工具执行
    think: calling provider               agent 回合开始（tool_call done 之后）

派生指标：
    ttfb          = response received - sending request        ← provider 首字节
    stream        = stop - response received                    ← 正文流式传输
    tool          = tool call done - tool call start            ← 工具执行
    overhead      = 下一次 sending request - 上一次 tool call done
                                                              ← 我方 agent 循环开销
    repeat        = 下一轮 think 的间隔 - 上一轮 stop           ← 工具已结束到再发请求

用法：
    python3 scripts/latency_profile.py                  # 今天的 trace
    python3 scripts/latency_profile.py 2026-11-06       # 指定日期
    python3 scripts/latency_profile.py 2026-11-06 top  # 附带最慢的工具调用
"""

import collections
import datetime
import glob
import json
import os
import statistics
import sys

TRACE_DIR = os.path.expanduser("~/.yi-agent/trace")


def to_ts(value):
    return datetime.datetime.fromisoformat(value.replace("Z", "+00:00")).timestamp()


def load(date_prefix, trace_dir=TRACE_DIR):
    """读入某天的全部 trace，按文件分组，组内按时间排序并去重。

    文件名用的是无分隔日期（`session-20261106-132340.jsonl`），所以传
    `2026-11-06` 与 `20261106` 都接受。
    """
    stamp = date_prefix.replace("-", "")
    sessions = []
    for path in sorted(glob.glob(os.path.join(trace_dir, f"session-{stamp}*.jsonl"))):
        events = []
        seen = set()
        with open(path) as handle:
            for line in handle:
                line = line.strip()
                if not line:
                    continue
                try:
                    record = json.loads(line)
                except json.JSONDecodeError:
                    continue
                # 同一进程可能重复初始化 tracing，同一行会被写两遍。
                key = (record.get("timestamp"), record.get("target"),
                       json.dumps(record.get("fields", {}), sort_keys=True),
                       json.dumps(record.get("span", {}), sort_keys=True))
                if key in seen:
                    continue
                seen.add(key)
                events.append((to_ts(record["timestamp"]),
                               record.get("target", ""),
                               record.get("fields", {}),
                               record.get("span") or {}))
        events.sort(key=lambda item: item[0])
        if len(events) > 50:
            sessions.append((os.path.basename(path), events))
    return sessions


def percentile(samples, q):
    ordered = sorted(samples)
    if not ordered:
        return 0.0
    return ordered[min(len(ordered) - 1, int(q * len(ordered)))]


def report(samples, label, width=20):
    ordered = sorted(samples)
    if not ordered:
        print(f"{label:<{width}} (no samples)")
        return
    print(
        f"{label:<{width}} n={len(ordered):5d} "
        f"p50={percentile(ordered, .5) * 1000:7.0f}ms "
        f"p90={percentile(ordered, .9) * 1000:7.0f}ms "
        f"p99={percentile(ordered, .99) * 1000:7.0f}ms "
        f"mean={statistics.mean(ordered) * 1000:7.0f}ms "
        f"total={sum(ordered):7.1f}s"
    )


def collect(sessions):
    ttfb, stream, overhead, repeats = [], [], [], []
    tools = {}
    tool_inputs = {}
    open_tools = {}
    names = {}
    for _, events in sessions:
        pending_send = None
        pending_response = None
        last_tool_done = None
        for ts, target, fields, span in events:
            message = fields.get("message", "")
            call_id = span.get("id") or fields.get("id")
            if target == "yi_agent_llm::openai::client" and message == "sending request":
                if last_tool_done is not None:
                    overhead.append(ts - last_tool_done)
                    last_tool_done = None
                pending_send = ts
            elif target == "yi_agent_llm::openai::client" and message == "response received":
                if pending_send is not None:
                    ttfb.append(ts - pending_send)
                    pending_send = None
                pending_response = ts
            elif target == "yi_agent_core::provider" and message.startswith("accumulate_stream: received Stop"):
                if pending_response is not None:
                    stream.append(ts - pending_response)
                    pending_response = None
            elif message == "tool call start":
                open_tools[call_id] = ts
                names[call_id] = span.get("tool") or fields.get("name")
                tool_inputs[call_id] = str(fields.get("input", ""))[:160]
            elif message == "tool call done":
                started = open_tools.pop(call_id, None)
                if started is not None:
                    duration = ts - started
                    tools.setdefault(names.get(call_id), []).append(duration)
                    tool_inputs[call_id] = (tool_inputs.get(call_id, ""), duration)
                    last_tool_done = ts
    return ttfb, stream, overhead, tools, tool_inputs


def main():
    date_prefix = sys.argv[1] if len(sys.argv) > 1 else datetime.date.today().strftime("%Y-%m-%d")
    want_slow = "top" in sys.argv
    sessions = load(date_prefix)
    if not sessions:
        print(f"no trace under {TRACE_DIR} for {date_prefix}")
        return 1
    total_events = sum(len(events) for _, events in sessions)
    print(f"trace: {len(sessions)} sessions / {total_events} events ({date_prefix})\n")
    ttfb, stream, overhead, tools, tool_inputs = collect(sessions)
    report(ttfb, "provider TTFB")
    report(stream, "stream consume")
    report(overhead, "agent loop overhead")
    print()
    print("=== tools (by total time) ===")
    for name, durations in sorted(tools.items(), key=lambda kv: -sum(kv[1])):
        report(durations, f"  {name}", width=22)
    if want_slow:
        print()
        print("=== slowest tool calls ===")
        ranked = [(meta[1], call_id, meta[0]) for call_id, meta in tool_inputs.items()
                  if isinstance(meta, tuple)]
        for duration, call_id, snippet in sorted(ranked, reverse=True)[:15]:
            print(f"{duration:8.1f}s {snippet[:140]}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
