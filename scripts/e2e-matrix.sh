#!/usr/bin/env bash
# 差分验证矩阵：Go/Rust 客户端与服务端实现组合下，同一套 e2e 全部通过才算移植等价。
# 用法: bash scripts/e2e-matrix.sh [组合名...]
#   all        全部组合（默认）
#   go-client  仅 Go 客户端组合
#   rs-client  含 Rust 客户端组合
#   rs-server  含 Rust 服务端组合
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
which_comb="${1:-all}"

RUNS=()
add_run(){ RUNS+=("$1|$2|$3|$4|$5"); }

add_run "go-server + go-client(s)"     "$ROOT/bin/y-sync-server" "$ROOT/bin/ysync"      "$ROOT/bin/ysync"      "go-client"
add_run "go-server + rust-client(A)"   "$ROOT/bin/y-sync-server" "$ROOT/bin/ysyncd-rs"  "$ROOT/bin/ysync"      "rs-client"
add_run "go-server + rust-client(B)"   "$ROOT/bin/y-sync-server" "$ROOT/bin/ysync"      "$ROOT/bin/ysyncd-rs"  "rs-client"
add_run "go-server + rust-client(s)"   "$ROOT/bin/y-sync-server" "$ROOT/bin/ysyncd-rs"  "$ROOT/bin/ysyncd-rs"  "rs-client"
if [ -f "$ROOT/bin/y-sync-server-rs" ]; then
  add_run "rust-server + go-client(s)"   "$ROOT/bin/y-sync-server-rs" "$ROOT/bin/ysync"     "$ROOT/bin/ysync"     "rs-server"
  add_run "rust-server + rust-client(s)" "$ROOT/bin/y-sync-server-rs" "$ROOT/bin/ysyncd-rs" "$ROOT/bin/ysyncd-rs" "rs-server"
fi

TOTAL_PASS=0; TOTAL_FAIL=0
for r in "${RUNS[@]}"; do
  IFS='|' read -r name server a b tag <<<"$r"
  if [ "$which_comb" != "all" ] && [ "$which_comb" != "$tag" ]; then continue; fi
  echo ""
  echo "=============================================================="
  echo "=== 组合: $name"
  echo "=============================================================="
  if SERVER_BIN="$server" BIN_A="$a" BIN_B="$b" bash "$ROOT/scripts/e2e.sh"; then
    echo ">>> [PASS] $name"
    TOTAL_PASS=$((TOTAL_PASS+1))
  else
    echo ">>> [FAIL] $name"
    TOTAL_FAIL=$((TOTAL_FAIL+1))
  fi
done

echo ""
echo "=============================================================="
echo "=== 差分矩阵结果: 通过=$TOTAL_PASS 失败=$TOTAL_FAIL"
echo "=============================================================="
[ $TOTAL_FAIL -eq 0 ]
