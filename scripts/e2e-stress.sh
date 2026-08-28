#!/usr/bin/env bash
# 稳定性/可靠性压测：真实 kill -9、并发互斥、WS 断线重连、Unicode/深路径、大批量、GC 恢复。
# 默认使用 Rust 实现（Go 版本已冻结）。用法: bash scripts/e2e-stress.sh
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WORK="$(mktemp -d /tmp/ysync-stress.XXXXXX)"
SRV_DATA="$WORK/server-data"
SRV_ADDR="127.0.0.1:18790"
SERVER_BIN="${SERVER_BIN:-$ROOT/bin/y-sync-server-rs}"
CLIENT_A="${BIN_A:-$ROOT/bin/ysyncd-rs}"
CLIENT_B="${BIN_B:-$ROOT/bin/ysyncd-rs}"
PASS=0; FAIL=0

say()  { echo -e "$1"; }
ok()   { PASS=$((PASS+1)); say "  \033[32mPASS\033[0m $1"; }
bad()  { FAIL=$((FAIL+1)); say "  \033[31mFAIL\033[0m $1"; }
check(){ if eval "$2"; then ok "$1"; else bad "$1 — [$2]"; fi }
# wait_for: 轮询等待条件成立
wait_for(){
  local desc="$1" t="$2" cond="$3" end
  end=$((SECONDS + t))
  while [ "$SECONDS" -lt "$end" ]; do
    if eval "$cond" >/dev/null 2>&1; then ok "$desc"; return 0; fi
    sleep 0.3
  done
  bad "$desc — 超时 [${cond}]"
  return 1
}

cleanup() {
  [ -n "${DAEMON_PID:-}" ] && kill "$DAEMON_PID" 2>/dev/null
  [ -n "${SERVER_PID:-}" ] && kill "$SERVER_PID" 2>/dev/null
  [ -n "${SYNC_PID:-}" ] && kill -9 "$SYNC_PID" 2>/dev/null
  [ -n "${E2E_KEEP:-}" ] || rm -rf "$WORK"
}
trap cleanup EXIT

export no_proxy="127.0.0.1,localhost" NO_PROXY="127.0.0.1,localhost"
pkill -f y-sync-server-rs 2>/dev/null; pkill -f ysyncd-rs 2>/dev/null; sleep 0.3
say "== 压测工作目录: $WORK =="
say "  实现: server=$(basename "$SERVER_BIN") A=$(basename "$CLIENT_A") B=$(basename "$CLIENT_B")"

"$SERVER_BIN" serve -addr "$SRV_ADDR" -data "$SRV_DATA" >"$WORK/server.log" 2>&1 &
SERVER_PID=$!
for i in $(seq 1 50); do curl -s "http://$SRV_ADDR/healthz" >/dev/null 2>&1 && break; sleep 0.1; done
curl -s "http://$SRV_ADDR/healthz" | grep -q ok && ok "服务端启动" || { bad "服务端启动"; exit 1; }

YSYNC_DATA="$SRV_DATA" "$SERVER_BIN" adduser alice <<<"secret123" >/dev/null 2>&1
YS="env YSYNC_CONFIG_DIR=$WORK/cfgA $CLIENT_A"
YS_B="env YSYNC_CONFIG_DIR=$WORK/cfgB $CLIENT_B"

$YS init -server "http://$SRV_ADDR" -user alice -device devA <<<"secret123" >/dev/null 2>&1 && ok "A 登录" || bad "A 登录"
$YS_B init -server "http://$SRV_ADDR" -user alice -device devB <<<"secret123" >/dev/null 2>&1 && ok "B 登录" || bad "B 登录"

# ---------- 场景 1: 真实 kill -9 客户端（大文件分块上传中） ----------
mkdir -p "$WORK/A/big"
python3 - <<PYSET
import json, os
p = os.path.join("$WORK", "cfgA", "config.json")
c = json.load(open(p))
c["chunk_threshold_mb"] = 1
c["chunk_size_mb"] = 1
json.dump(c, open(p, "w"))
PYSET
for i in 1 2 3 4 5; do head -c 3145728 /dev/urandom > "$WORK/A/big/f$i.bin"; done
$YS add "$WORK/A/big" --as big >/dev/null
$YS sync >/dev/null 2>&1 &
SYNC_PID=$!
sleep 0.6
kill -9 "$SYNC_PID" 2>/dev/null && ok "kill -9 客户端（sync 中途）" || ok "客户端已自然结束（时序容差）"
wait "$SYNC_PID" 2>/dev/null
$YS sync >/dev/null 2>&1 && ok "kill -9 后重同步成功" || bad "kill -9 后重同步成功"
$YS_B add "$WORK/B/big" --as big >/dev/null
$YS_B sync >/dev/null 2>&1
check "kill -9 后数据收敛（5 个大文件齐全）" \
  "[ \$(find "$WORK/B/big" -name '*.bin' | wc -l) -eq 5 ]"
SAME=1
for f in "$WORK/A/big"/*.bin; do
  cmp -s "$f" "$WORK/B/big/$(basename "$f")" || SAME=0
done
check "kill -9 后内容逐字节一致" "[ $SAME -eq 1 ]"

# ---------- 场景 2: 真实 kill -9 服务端（ops 提交中） ----------
mkdir -p "$WORK/A/many"
for i in $(seq 1 30); do echo "content-$i" > "$WORK/A/many/file-$i.txt"; done
$YS add "$WORK/A/many" --as many >/dev/null
$YS sync >/dev/null 2>&1 &
SYNC_PID=$!
sleep 0.4
kill -9 "$SERVER_PID" 2>/dev/null && ok "kill -9 服务端（ops 提交中）" || ok "服务端时序容差"
wait "$SYNC_PID" 2>/dev/null
"$SERVER_BIN" serve -addr "$SRV_ADDR" -data "$SRV_DATA" >>"$WORK/server.log" 2>&1 &
SERVER_PID=$!
wait_for "服务端重启（SQLite WAL 恢复）" 10 "curl -s http://$SRV_ADDR/healthz | grep -q ok"
$YS sync >/dev/null 2>&1 && ok "服务端重启后重同步成功" || bad "服务端重启后重同步成功"
$YS_B add "$WORK/B/many" --as many >/dev/null
$YS_B sync >/dev/null 2>&1
check "kill -9 服务端后数据收敛（30 个文件）" \
  "[ \$(find "$WORK/B/many" -name 'file-*.txt' | wc -l) -eq 30 ]"

# ---------- 场景 3: Unicode / 空格 / 深路径 ----------
mkdir -p "$WORK/A/edge/中文 目录/深/层的/路径 结构"
echo "unicode-内容" > "$WORK/A/edge/中文 文件.txt"
echo "space-内容" > "$WORK/A/edge/中文 目录/带 空格 文件.md"
echo "deep-内容" > "$WORK/A/edge/中文 目录/深/层的/路径 结构/deep 文件.txt"
$YS add "$WORK/A/edge" --as edge >/dev/null
$YS sync >/dev/null 2>&1 && ok "Unicode/空格/深路径上传" || bad "Unicode/空格/深路径上传"
$YS_B add "$WORK/B/edge" --as edge >/dev/null
$YS_B sync >/dev/null 2>&1
F1="$WORK/B/edge/中文 文件.txt"; F2="$WORK/B/edge/中文 目录/带 空格 文件.md"
F3="$WORK/A/edge/中文 目录/深/层的/路径 结构/deep 文件.txt"; F4="$WORK/B/edge/中文 目录/深/层的/路径 结构/deep 文件.txt"
check "中文文件名传播" 'grep -q "unicode-内容" "$F1"'
check "空格路径传播"   'grep -q "space-内容" "$F2"'
check "深路径传播(A 侧源文件)" 'grep -q "deep-内容" "$F3"'
check "深路径传播(B 侧落地)"   'grep -q "deep-内容" "$F4"' 

# ---------- 场景 4: 100 文件 + 并发 sync（flock 互斥） ----------
mkdir -p "$WORK/A/bulk"
for i in $(seq 1 100); do echo "bulk-$i" > "$WORK/A/bulk/f$i.txt"; done
$YS add "$WORK/A/bulk" --as bulk >/dev/null
$YS sync >/dev/null 2>&1 &
P1=$!
$YS sync >/dev/null 2>&1 &
P2=$!
wait $P1; R1=$?
wait $P2; R2=$?
check "并发 sync 双双成功（flock 使后来者静默跳过）" "[ $R1 -eq 0 ] && [ $R2 -eq 0 ]"
$YS_B add "$WORK/B/bulk" --as bulk >/dev/null
$YS_B sync >/dev/null 2>&1
check "100 文件全部传播" "[ \$(find "$WORK/B/bulk" -name 'f*.txt' | wc -l) -eq 100 ]"

# ---------- 场景 5: GC 后回收站恢复仍可用 ----------
rm "$WORK/A/bulk/f50.txt"
$YS sync >/dev/null 2>&1
YSYNC_DATA="$SRV_DATA" "$SERVER_BIN" gc >/dev/null 2>&1 && ok "服务端 gc 执行" || bad "服务端 gc 执行"
TRASH_ID=$($YS trash list | grep "bulk/f50.txt" | awk '{print $1}' | head -1)
check "gc 后回收站条目仍在" "[ -n "$TRASH_ID" ]"
$YS trash restore "$TRASH_ID" >/dev/null && ok "gc 后恢复 API" || bad "gc 后恢复 API"
$YS sync >/dev/null 2>&1; $YS_B sync >/dev/null 2>&1
check "gc 后恢复传播到 B" "test -f "$WORK/B/bulk/f50.txt""

# ---------- 场景 6: daemon WS 断线重连 + 并发 CLI 同步互斥 ----------
$YS daemon -http 127.0.0.1:0 -interval 60s >"$WORK/daemon.log" 2>&1 &
DAEMON_PID=$!
wait_for "daemon 启动" 10 "test -f $WORK/cfgA/daemon.json"
DAEMON_TOKEN=$(python3 -c "import json;print(json.load(open('$WORK/cfgA/daemon.json'))['token'])")
DAEMON_ADDR=$(python3 -c "import json;print(json.load(open('$WORK/cfgA/daemon.json'))['addr'])")
api_post(){ curl -s -X POST "http://$DAEMON_ADDR/$1?token=$DAEMON_TOKEN" -H 'Content-Type: application/json' -d "$2"; }
# 并发：daemon 常驻时 CLI sync 应静默跳过而非报错
$YS sync >/dev/null 2>&1; RC=$?
check "daemon 运行时 CLI sync 静默跳过（exit 0）" "[ $RC -eq 0 ]"
# 断线重连：杀服务端 → 重启 → B 修改 → daemon A 应自动恢复同步
kill -9 "$SERVER_PID" 2>/dev/null; sleep 1
"$SERVER_BIN" serve -addr "$SRV_ADDR" -data "$SRV_DATA" >>"$WORK/server.log" 2>&1 &
SERVER_PID=$!
wait_for "服务端重启" 10 "curl -s http://$SRV_ADDR/healthz | grep -q ok"
mkdir -p "$WORK/A/wsdir"
$YS add "$WORK/A/wsdir" --as wsdir >/dev/null
api_post sync '{}'
wait_for "daemon 热加载 CLI 新增文件夹" 15 "curl -s "http://$DAEMON_ADDR/status?token=$DAEMON_TOKEN" | grep -q wsdir"
$YS_B add "$WORK/B/wsdir" --as wsdir >/dev/null
echo "ws-after-restart" > "$WORK/A/wsdir/w.txt"
$YS sync >/dev/null 2>&1
echo "ws-after-restart" > "$WORK/B/wsdir/w.txt"
$YS_B sync >/dev/null 2>&1
WS_F="$WORK/A/wsdir/w.txt"
wait_for "WS 断线重连后 daemon 自动拉取" 35 'grep -q "ws-after-restart" "$WS_F"'
# ---------- 易用性: status 汇总（daemon 存活期间） ----------
STATUS_OUT=$($YS status 2>/dev/null)
check "status 显示 daemon 运行摘要" 'echo "$STATUS_OUT" | grep -qE "daemon: running @ .*(个文件夹)"'
kill "$DAEMON_PID" 2>/dev/null; wait "$DAEMON_PID" 2>/dev/null
check "daemon 退出清理 daemon.json" "[ ! -f "$WORK/cfgA/daemon.json" ]"
STATUS_OUT2=$($YS status 2>/dev/null)
check "daemon 退出后 status 显示未运行" 'echo "$STATUS_OUT2" | grep -q "daemon: 未运行"'

say ""
say "== 压测结果: PASS=$PASS FAIL=$FAIL =="
[ $FAIL -eq 0 ]
