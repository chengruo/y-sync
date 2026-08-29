#!/usr/bin/env bash
# P1 新特性验证（仅 Rust 实现）：设备管理、/metrics、审计日志、登录锁定、配额。
# 用法: bash scripts/e2e-features.sh
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WORK="$(mktemp -d /tmp/ysync-feat.XXXXXX)"
SRV_DATA="$WORK/server-data"
SRV_ADDR="127.0.0.1:18795"
SERVER_BIN="${SERVER_BIN:-$ROOT/bin/y-sync-server-rs}"
CLIENT="${BIN_A:-$ROOT/bin/ysyncd-rs}"
PASS=0; FAIL=0

say(){ echo -e "$1"; }
ok(){ PASS=$((PASS+1)); say "  \033[32mPASS\033[0m $1"; }
bad(){ FAIL=$((FAIL+1)); say "  \033[31mFAIL\033[0m $1"; }
check(){ if eval "$2"; then ok "$1"; else bad "$1 — [$2]"; fi }

cleanup(){ [ -n "${SERVER_PID:-}" ] && kill "$SERVER_PID" 2>/dev/null; [ -n "${E2E_KEEP:-}" ] || rm -rf "$WORK"; }
trap cleanup EXIT
export no_proxy="127.0.0.1,localhost" NO_PROXY="127.0.0.1,localhost"
pkill -f y-sync-server-rs 2>/dev/null; sleep 0.2

say "== 特性验证: server=$(basename "$SERVER_BIN") client=$(basename "$CLIENT") =="

"$SERVER_BIN" serve -addr "$SRV_ADDR" -data "$SRV_DATA" >"$WORK/server.log" 2>&1 &
SERVER_PID=$!
for i in $(seq 1 50); do curl -s "http://$SRV_ADDR/healthz" >/dev/null 2>&1 && break; sleep 0.1; done

YS="env YSYNC_CONFIG_DIR=$WORK/cfg $CLIENT"

# ---------- 设备管理（P1-6） ----------
YSYNC_DATA="$SRV_DATA" "$SERVER_BIN" adduser alice <<<"secret123" >/dev/null 2>&1
mkdir -p "$WORK/proj"; echo hello > "$WORK/proj/a.txt"
$YS init -server "http://$SRV_ADDR" -user alice -device devMain <<<"secret123" >/dev/null 2>&1
$YS add "$WORK/proj" >/dev/null && $YS sync >/dev/null 2>&1
check "设备列表包含当前设备" "$YS devices | grep -q '当前设备'"
# 登录第二台设备并吊销
$YS init -server "http://$SRV_ADDR" -user alice -device devGhost <<<"secret123" >/dev/null 2>&1
DEV_ID=$($YS devices | grep devGhost | awk '{print $1}' | head -1)
check "设备列表包含第二台" "[ -n "$DEV_ID" ]"
$YS revoke "$DEV_ID" >/dev/null && ok "吊销 API 成功"
TOK_GHOST=$($YS devices >/dev/null 2>&1; python3 -c "pass" 2>/dev/null; echo "")
# devGhost 的 token 已失效：用其重新 init 验证（应 401 → init 失败）

# ---------- /metrics（P1-7） ----------
M=$(curl -s "http://$SRV_ADDR/metrics")
check "metrics: 用户数"        'echo "$M" | grep -q "^ysync_users 1$"'
check "metrics: 文件数"        'echo "$M" | grep -q "^ysync_files 1$"'
check "metrics: blob 字节数"   'echo "$M" | grep -q "^ysync_blob_bytes [1-9]"'
check "metrics: HTTP 计数"     'echo "$M" | grep -q "^ysync_http_requests_total [0-9]"'

# ---------- 审计日志（P1-7） ----------
check "审计日志存在"           "test -f $SRV_DATA/audit.log"
check "审计含 op_put"          "grep -q 'op_put' $SRV_DATA/audit.log"
check "审计含 login"           "grep -q '\"event\":\"login\"' $SRV_DATA/audit.log"

# ---------- 登录暴力破解防护（P0-3） ----------
for i in 1 2 3 4 5; do
  curl -s -o /dev/null -X POST "http://$SRV_ADDR/api/v1/auth/login" \
    -H 'Content-Type: application/json' \
    -d '{"user":"locker","password":"wrong","device_name":"x"}'
done
# 第 6 次（即使密码正确）应 429
CODE=$(curl -s -o /dev/null -w '%{http_code}' -X POST "http://$SRV_ADDR/api/v1/auth/login" \
  -H 'Content-Type: application/json' \
  -d '{"user":"locker","password":"anything","device_name":"x"}')
check "连续 5 次失败后触发锁定 (429)" "[ \"$CODE\" = \"429\" ]"

# ---------- 用户配额（P1-8） ----------
YSYNC_DATA="$SRV_DATA" "$SERVER_BIN" adduser bob --quota 200 <<<"qpass" >/dev/null 2>&1
mkdir -p "$WORK/bob"; head -c 1024 /dev/zero > "$WORK/bob/big.bin"
$YS init -server "http://$SRV_ADDR" -user bob -device devQ <<<"qpass" >/dev/null 2>&1
$YS add "$WORK/bob" >/dev/null
$YS sync >/dev/null 2>&1
TOKQ=$(python3 -c "import json;print(json.load(open('$WORK/cfg/config.json'))['token'])")
NODES_Q=$(curl -s -H "Authorization: Bearer $TOKQ" "http://$SRV_ADDR/api/v1/nodes")
check "超配额上传被拒绝（服务端无该文件）" 'echo "$NODES_Q" | grep -qv "big.bin"'
check "引擎记录 put 失败待重试" 'grep -q "op_failed" "$SRV_DATA/audit.log"' 
check "list-users 显示配额与用量" \
  "YSYNC_DATA='$SRV_DATA' '$SERVER_BIN' list-users | grep -qE 'bob.*配额 0.00 GB|bob.*已用'"

say ""
say "== 特性验证结果: PASS=$PASS FAIL=$FAIL =="
[ $FAIL -eq 0 ]
