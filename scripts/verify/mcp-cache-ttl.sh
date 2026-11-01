#!/usr/bin/env bash
# MCP 缓存 TTL 自愈的手工验收脚本。
#
# 用法:
#   scripts/verify/mcp-cache-ttl.sh expire          # 把缓存改成 25h 前(已过期)
#   scripts/verify/mcp-cache-ttl.sh status          # 看当前是新鲜还是过期
#   scripts/verify/mcp-cache-ttl.sh backoff         # 把 failed_at 改成 1 分钟前(退避中)
#   scripts/verify/mcp-cache-ttl.sh restore <备份>  # 恢复备份
#
# 完整验收流程见 docs/superpowers/specs/2026-10-31-mcp-cache-ttl-selfheal-design.md
set -euo pipefail

CACHE=".yi-agent/mcp-cache.json"
TTL=86400        # 24h,与 cache.rs 的 TTL_SECS 一致
RETRY=300        # 5min,与 cache.rs 的 RETRY_AFTER_SECS 一致

need_cache() {
  [ -f "$CACHE" ] || { echo "找不到 $CACHE,先在项目里跑一次 yi-agent 让缓存生成" >&2; exit 1; }
}

case "${1:-status}" in
  status)
    need_cache
    python3 - "$CACHE" "$TTL" "$RETRY" <<'PY'
import json, sys, time, datetime
path, ttl, retry = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
d = json.load(open(path))
now = int(time.time())
print(f"now = {datetime.datetime.fromtimestamp(now)}")
for name, e in d.get("servers", {}).items():
    pa, fa = e.get("probed_at"), e.get("failed_at")
    print(f"\n[{name}]")
    if pa:
        age = now - pa
        state = "新鲜" if 0 <= age < ttl else "已过期"
        print(f"  probed_at = {datetime.datetime.fromtimestamp(pa)}  ({age/3600:.2f}h 前, {state})")
        if age >= ttl:
            print(f"  -> 下次发消息前应触发一次重探")
    else:
        print("  probed_at = None  (旧格式,启动时会被补成 now)")
    if fa:
        fage = now - fa
        print(f"  failed_at = {datetime.datetime.fromtimestamp(fa)}  ({fage/60:.1f} 分钟前)")
        print(f"  -> " + ("退避中,不会探测" if 0 <= fage < retry else "退避已过,可以重试"))
    else:
        print("  failed_at = None")
    print(f"  tools = {len(e.get('tools', []))} 个")
PY
    ;;

  expire)
    need_cache
    python3 - "$CACHE" "$TTL" <<'PY'
import json, sys, time
path, ttl = sys.argv[1], int(sys.argv[2])
d = json.load(open(path))
now = int(time.time())
old = now - ttl - 3600          # 24h + 1h = 25h 前
for name, e in d["servers"].items():
    e["probed_at"] = old
    e["failed_at"] = None
    print(f"{name}: probed_at -> {old} (25h 前)")
json.dump(d, open(path, "w"), indent=2, ensure_ascii=False)
print("\n已改成过期。现在启动 yi-agent 发一条消息,再看 `status`。")
PY
    ;;

  backoff)
    need_cache
    python3 - "$CACHE" "$TTL" "$RETRY" <<'PY'
import json, sys, time
path, ttl, retry = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
d = json.load(open(path))
now = int(time.time())
for name, e in d["servers"].items():
    e["probed_at"] = now - ttl - 60   # 过期
    e["failed_at"] = now - 60         # 1 分钟前失败,仍在 5min 窗口内
    print(f"{name}: 已过期 + failed_at 1 分钟前(退避中)")
json.dump(d, open(path, "w"), indent=2, ensure_ascii=False)
print("\n现在发消息:不应探测(probed_at 不变)。")
print("再跑 `yi-agent` 敲 /mcp reload:应提示「N 个因退避窗口跳过」。")
PY
    ;;

  restore)
    [ $# -ge 2 ] || { echo "用法: $0 restore <备份文件>" >&2; exit 1; }
    cp "$2" "$CACHE"
    echo "已恢复 $CACHE <- $2"
    ;;

  *)
    sed -n '2,14p' "$0" | sed 's/^# \{0,1\}//'
    exit 1
    ;;
esac
