#!/usr/bin/env bash
# Self-test for scripts/test-magpie-agent-yi-agent.sh: pins down the paths where the
# check itself must fail, above all "no Node" having to exit 2 (an
# infrastructure failure) rather than pass green.
#
# Run it with bash, by absolute path if you like — it does not depend on
# being found through PATH:
#
#   bash scripts/test_test_magpie_agent_yi_agent.sh
#
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCRIPT="$HERE/test-magpie-agent-yi-agent.sh"
[ -x "$SCRIPT" ] || { echo "FAIL: $SCRIPT not executable"; exit 1; }

pass=0
failn=0
check() { # check <name> <expected_exit> <actual_exit>
  if [ "$2" = "$3" ]; then echo "ok   - $1"; pass=$((pass+1));
  else echo "FAIL - $1 (expected exit $2, got $3)"; failn=$((failn+1)); fi
}

# The "no Node" case has to be built, not guessed at: PATH=/bin:/usr/bin is
# not an environment without Node — Debian and Ubuntu package it at
# /usr/bin/node, so the old test would report a false failure on exactly the
# CI image it guards.
#
# A PATH holding only a symlink to bash does not work either, for the same
# reason in reverse: on Linux bash is /usr/bin/bash, so putting bash's
# directory on PATH brings node along. So the check is run by absolute path
# with `env -i` instead of through its `#!/usr/bin/env bash` shebang, which
# leaves PATH free to point at an empty directory — provably no node. The
# script needs nothing but shell builtins before its node check (`command`
# is a builtin), so an empty PATH is enough to reach the exit 2 it must
# report.
tmp="$(mktemp -d "${HERE}/.selftest.XXXXXX")" || { echo "FAIL: mktemp"; exit 1; }
trap 'rm -rf "$tmp"' EXIT
emptydir="$tmp/empty"
mkdir -p "$emptydir"

bashbin="$(command -v bash)"
[ -x "$bashbin" ] || { echo "FAIL: bash not found"; exit 1; }

# Prove the PATH really has no node before trusting the result: otherwise a
# machine where that directory does hold node would turn this into a silent
# "expected 2, got 0" mystery.
if env -i PATH="$emptydir" "$bashbin" -c 'command -v node' >/dev/null 2>&1; then
  echo "FAIL - the no-node PATH still finds node at $(env -i PATH="$emptydir" "$bashbin" -c 'command -v node')"
  failn=$((failn+1))
else
  env -i PATH="$emptydir" "$bashbin" "$SCRIPT" >/dev/null 2>&1
  check "missing node is an infra failure (exit 2)" 2 $?
fi

# A normal environment: exit 0 (the syntax check and the behavior tests
# really ran). Called by absolute path, with the caller's PATH intact.
{
  bash "$SCRIPT"
} >/dev/null 2>&1
check "a full run passes (exit 0)" 0 $?

echo
if [ "$failn" -ne 0 ]; then echo "$failn failed, $pass passed"; exit 1; fi
echo "$pass passed"