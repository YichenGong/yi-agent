#!/usr/bin/env bash
# nightly workflow 的静态守卫。
#
# 为什么需要它：`.github/workflows/nightly.yml` 不会在 PR/push 上跑（只有
# cron 和 workflow_dispatch），所以它**坏了没人会立刻知道**——最典型的是
# 「justfile 里某个 recipe 改了名，workflow 还在调旧名」，或者
# 「models.json 字段名改了，写进去的配置读不回来」。这类失效是静默的：
# workflow 每次定时跑都红，但没人看夜间结果，等真要它兜底时才发现一直没工作。
#
# 这个脚本把「workflow 依赖的所有名字」逐个与**真实定义**对账：
#   1. 调用的 just recipe 是否存在于 justfile
#   2. 调用的 scripts/*.sh 是否存在且可执行
#   3. workflow 写 models.json 用的字段名，是否与 export-model-env.sh 读取的
#      字段名一致（不一致 → 写进去了但读不出来 → 测试静默 SKIPPED）
#   4. YAML 是否可解析、三个 job 是否都在
#
# 它**不**验证 workflow 真能跑通（那需要真实 runner + secrets）。它只保证
# 「名字对不上」这一类静默失效被挡住。真实跑通靠 workflow_dispatch 手动验一次。
#
# 进 `ci`：零依赖、毫秒级、不需要网络。

set -uo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
wf="$root/.github/workflows/nightly.yml"
justfile="$root/yi-agent-rs/justfile"
loader="$root/yi-agent-rs/scripts/export-model-env.sh"

pass=0
fail=0
ok() { printf 'ok   - %s\n' "$1"; pass=$((pass + 1)); }
no() {
  printf 'FAIL - %s\n' "$1" >&2
  fail=$((fail + 1))
}

for f in "$wf" "$justfile" "$loader"; do
  if [ ! -f "$f" ]; then
    echo "FAIL: 缺文件 $f" >&2
    exit 1
  fi
done

# ---- 1. YAML 可解析 + 三个 job 齐全 ----
if command -v ruby >/dev/null 2>&1; then
  jobs="$(ruby -ryaml -e '
    begin
      d = YAML.load_file(ARGV[0])
    rescue => e
      warn "YAML parse error: #{e}"; exit 3
    end
    puts (d["jobs"] || {}).keys.join(",")
  ' "$wf" 2>/dev/null)"
  if [ $? -eq 0 ]; then
    ok "nightly.yml 可被 YAML 解析"
    for want in audit mutants real-llm; do
      case ",$jobs," in
        *",$want,"*) ok "job 存在：$want" ;;
        *) no "缺 job：$want（实际 jobs: $jobs）" ;;
      esac
    done
  else
    no "nightly.yml 解析失败"
  fi
else
  printf 'skip - 无 ruby，跳过 YAML 解析检查\n'
fi

# ---- 2. workflow 调用的 just recipe 必须存在 ----
# 抓 `run: just <recipe>` 与 `run: just <recipe> <arg>` 形式
recipes="$(grep -oE 'just [a-z][a-z0-9-]*' "$wf" | awk '{print $2}' | sort -u)"
if [ -z "$recipes" ]; then
  no "workflow 里没找到任何 just 调用（是不是被改写了？）"
else
  for r in $recipes; do
    # justfile 里 recipe 定义形如 `name:` 或 `name arg=...:`
    if grep -qE "^${r}([[:space:]][^:]*)?:" "$justfile"; then
      ok "just recipe 存在：$r"
    else
      no "workflow 调用了不存在的 recipe：$r"
    fi
  done
fi

# ---- 3. workflow 引用的 scripts/*.sh 必须存在且可执行 ----
scripts="$(grep -oE 'scripts/[a-z0-9_-]+\.sh' "$wf" | sort -u)"
if [ -z "$scripts" ]; then
  no "workflow 里没找到 scripts/*.sh 调用（是不是被改写了？）"
else
  for s in $scripts; do
    # workflow 里 working-directory 是 yi-agent-rs，故相对该目录解析
    if [ -f "$root/yi-agent-rs/$s" ]; then
      if [ -x "$root/yi-agent-rs/$s" ]; then
        ok "script 存在且可执行：$s"
      else
        no "script 不可执行：$s（chmod +x）"
      fi
    else
      no "workflow 引用了不存在的脚本：$s"
    fi
  done
fi

# ---- 4. models.json 字段名对账 ----
# workflow 的 python 片段写 entry = {"name","provider","model","api_url","api_key"}
# loader 读的字段是 provider/model/api_url/api_key。任何一边改名都会导致
# 「写进去了但读不出来」，测试静默 SKIPPED（exit 0，看起来是绿的）。
# 归一化成一整行空格分隔：下面用 `case " $wf_fields " in *" $f "*` 匹配，
# 多行会让换行顶掉分隔空格，匹配必然失败（实测踩过）。
wf_fields="$(grep -oE '"(provider|model|api_url|api_key|name)"[[:space:]]*:' "$wf" | tr -d '" :' | sort -u | tr '\n' ' ')"
loader_fields="$(grep -oE '\("[A-Z_]+", "(provider|model|api_url|api_key)"\)' "$loader" | grep -oE '"(provider|model|api_url|api_key)"' | tr -d '"' | sort -u | tr '\n' ' ')"

if [ -z "$wf_fields" ] || [ -z "$loader_fields" ]; then
  no "无法从 workflow / loader 中提取 models.json 字段名（格式变了？）"
else
  for f in $loader_fields; do
    case " $wf_fields " in
      *" $f "*) ok "models.json 字段闭环：$f" ;;
      *) no "workflow 未写入字段 \"$f\"，但 loader 会读它（读到空 → 测试静默跳过）" ;;
    esac
  done
fi

# ---- 5. 缺 secret 时必须失败，而不是写出空条目 ----
if grep -q "missing = \[k for k in" "$wf" && grep -q "sys.exit" "$wf"; then
  ok "缺 secret 时 workflow 显式失败（不写空条目）"
else
  no "缺 secret 时 workflow 可能写出空条目让测试静默跳过"
fi

# ---- 6. eval job 必须检查 key 非空，避免「0 条 case 退出 0」 ----
if grep -q 'if \[ -z "${MODEL_API_KEY:-}" \]' "$wf"; then
  ok "eval 步骤在无 key 时显式提示（而非静默跑 0 条）"
else
  no "eval 步骤缺少无 key 判断，可能静默跑 0 条还退出 0"
fi

printf '\n%d passed, %d failed\n' "$pass" "$fail"
[ "$fail" -eq 0 ]
