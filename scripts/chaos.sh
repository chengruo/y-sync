#!/usr/bin/env bash
# 混沌长跑测试（P1-5）：随机文件操作 + 随机 kill -9（客户端/服务端），最终全树一致性校验。
# 用法: bash scripts/chaos.sh [轮数，默认 8]
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ROUNDS="${1:-8}"
WORK="$(mktemp -d /tmp/ysync-chaos.XXXXXX)"
SRV_DATA="$WORK/server-data"
SRV_ADDR="127.0.0.1:18797"
SERVER_BIN="${SERVER_BIN:-$ROOT/bin/y-sync-server-rs}"
CLIENT_A="${BIN_A:-$ROOT/bin/ysyncd-rs}"
CLIENT_B="${BIN_B:-$ROOT/bin/ysyncd-rs}"
PASS=0; FAIL=0
SYNC_KILLS=0; SRV_KILLS=0

say(){ echo -e "$1"; }
ok(){ PASS=$((PASS+1)); say "  \033[32mPASS\033[0m $1"; }
bad(){ FAIL=$((FAIL+1)); say "  \033[31mFAIL\033[0m $1"; }

cleanup(){
  [ -n "${SERVER_PID:-}" ] && kill -9 "$SERVER_PID" 2>/dev/null
  [ -n "${E2E_KEEP:-}" ] || rm -rf "$WORK"
}
trap cleanup EXIT
export no_proxy="127.0.0.1,localhost" NO_PROXY="127.0.0.1,localhost"
pkill -9 -f y-sync-server-rs 2>/dev/null; pkill -9 -f ysyncd-rs 2>/dev/null; pkill -9 -f "ysync-server-rs serve" 2>/dev/null; sleep 0.5
pkill -f y-sync-server-rs 2>/dev/null; sleep 0.2

say "== 混沌长跑: $ROUNDS 轮（Rust 全实现） =="

"$SERVER_BIN" serve -addr "$SRV_ADDR" -data "$SRV_DATA" >"$WORK/server.log" 2>&1 &
SERVER_PID=$!
for i in $(seq 1 50); do curl -s "http://$SRV_ADDR/healthz" >/dev/null 2>&1 && break; sleep 0.1; done

YSYNC_DATA="$SRV_DATA" "$SERVER_BIN" adduser alice <<<"secret123" >/dev/null 2>&1
YS_A="env YSYNC_CONFIG_DIR=$WORK/cfgA $CLIENT_A"
YS_B="env YSYNC_CONFIG_DIR=$WORK/cfgB $CLIENT_B"
$YS_A init -server "http://$SRV_ADDR" -user alice -device devA <<<"secret123" >/dev/null 2>&1
$YS_B init -server "http://$SRV_ADDR" -user alice -device devB <<<"secret123" >/dev/null 2>&1
mkdir -p "$WORK/A/chaos" "$WORK/B/chaos"
# A 端小分块阈值：保证每轮都有可击杀的分块上传窗口（顺带覆盖断点续传）
python3 - <<PYJSON
import json, os
p = os.path.join("$WORK", "cfgA", "config.json")
c = json.load(open(p))
c["chunk_threshold_mb"] = 1
c["chunk_size_mb"] = 1
c["upload_limit_kbs"] = 256  # 拉长上传窗口，保证 kill -9 落在分块上传中
json.dump(c, open(p, "w"))
PYJSON
$YS_A add "$WORK/A/chaos" >/dev/null
$YS_B add "$WORK/B/chaos" >/dev/null
$YS_A sync >/dev/null 2>&1; $YS_B sync >/dev/null 2>&1

r=$(date +%s)%100000

sync_a(){ $YS_A sync >/dev/null 2>&1 || $YS_A sync >/dev/null 2>&1; }
sync_b(){ $YS_B sync >/dev/null 2>&1 || $YS_B sync >/dev/null 2>&1; }

kill_client_mid_sync(){
  # 制造 2MB 上传负载（1MB 分块）→ 后台同步 → 随机时机 kill -9
  head -c $((2097152 + RANDOM * 1024)) /dev/urandom > "$WORK/A/chaos/blob-$RANDOM.bin"
  $YS_A sync >/dev/null 2>&1 &
  local pid=$!
  sleep "0.$((3 + RANDOM % 6))"
  kill -9 "$pid" 2>/dev/null && SYNC_KILLS=$((SYNC_KILLS+1))
  wait "$pid" 2>/dev/null
  return 0
}
kill_server(){
  kill -9 "$SERVER_PID" 2>/dev/null && SRV_KILLS=$((SRV_KILLS+1))
  sleep 0.3
  "$SERVER_BIN" serve -addr "$SRV_ADDR" -data "$SRV_DATA" >>"$WORK/server.log" 2>&1 &
  SERVER_PID=$!
  for i in $(seq 1 50); do curl -s "http://$SRV_ADDR/healthz" >/dev/null 2>&1 && break; sleep 0.1; done
}

new_file(){ # new_file <设备A|B> <相对路径>
  local side="$1" rel="$2"
  local dir="$WORK/A/chaos"
  [ "$side" = "B" ] && dir="$WORK/B/chaos"
  mkdir -p "$(dirname "$dir/$rel")"
  echo "r$RANDOM-$rel" > "$dir/$rel"
}
mod_file(){ # 随机改一个现存文件
  local side="$1"
  local dir="$WORK/A/chaos"
  [ "$side" = "B" ] && dir="$WORK/B/chaos"
  local f
  f=$(find "$dir" -type f ! -path "*/.y-sync/*" 2>/dev/null | shuf -n 1 2>/dev/null || find "$dir" -type f ! -path "*/.y-sync/*" | head -1)
  [ -n "${f:-}" ] && echo "mod-r$RANDOM" > "$f"
}
del_file(){
  local side="$1"
  local dir="$WORK/A/chaos"
  [ "$side" = "B" ] && dir="$WORK/B/chaos"
  local f
  f=$(find "$dir" -type f ! -path "*/.y-sync/*" 2>/dev/null | shuf -n 1 2>/dev/null || true)
  [ -n "${f:-}" ] && rm -f "$f"
}
move_file(){
  local side="$1"
  local dir="$WORK/A/chaos"
  [ "$side" = "B" ] && dir="$WORK/B/chaos"
  local f
  f=$(find "$dir" -type f ! -path "*/.y-sync/*" 2>/dev/null | shuf -n 1 2>/dev/null || true)
  [ -n "${f:-}" ] && mkdir -p "$(dirname "$dir/sub$RANDOM")" && mv "$f" "$dir/sub$RANDOM/moved-$(basename "$f")" 2>/dev/null
}

say "  -- 随机操作与击杀阶段 --"
for round in $(seq 1 "$ROUNDS"); do
  side=$((RANDOM % 2)); S=A; [ $side -eq 1 ] && S=B
  op=$((RANDOM % 5))
  case $op in
    0) new_file "$S" "dir$RANDOM/f$RANDOM.txt" ;;
    1) new_file "$S" "f$RANDOM.txt"; mod_file "$S" ;;
    2) mod_file "$S"; del_file "$S" ;;
    3) move_file "$S" ;;
    4) new_file "$S" "deep/a/b/f$RANDOM.txt"; del_file "$S" ;;
  esac
  # 随机混沌事件
  roll=$((RANDOM % 4))
  case $roll in
    0) sync_a; sync_b ;;
    1) kill_client_mid_sync; sync_a; sync_b ;;
    2) kill_server; sync_a; sync_b ;;
    3) sync_a; kill_server; sync_a; sync_b ;;
  esac
  say "  轮 $round/$ROUNDS 完成（客户端击杀 ${SYNC_KILLS} / 服务端击杀 ${SRV_KILLS}）"
done

say "  -- 收敛与一致性校验 --"
# 双端各同步至多 3 轮到稳定
for i in 1 2 3; do sync_a; sync_b; done

A_SNAP=$(cd "$WORK/A/chaos" && find . -type f ! -path "./.y-sync/*" ! -name "*.lock" | sort | while IFS= read -r f; do printf '%s %s\n' "$(md5 -q "$f")" "$f"; done)
B_SNAP=$(cd "$WORK/B/chaos" && find . -type f ! -path "./.y-sync/*" ! -name "*.lock" | sort | while IFS= read -r f; do printf '%s %s\n' "$(md5 -q "$f")" "$f"; done)
if [ "$A_SNAP" = "$B_SNAP" ]; then
  ok "终态一致性：A 与 B 全树逐字节一致（$(echo "$A_SNAP" | wc -l | tr -d ' ') 个文件）"
else
  bad "终态一致性：A 与 B 不一致"
  diff <(echo "$A_SNAP") <(echo "$B_SNAP") | head -10
fi
[ "$SYNC_KILLS" -gt 0 ] && ok "混沌覆盖：客户端 kill -9 ×$SYNC_KILLS" || bad "混沌覆盖：未触发客户端击杀"
[ "$SRV_KILLS" -gt 0 ] && ok "混沌覆盖：服务端 kill -9 ×$SRV_KILLS" || bad "混沌覆盖：未触发服务端击杀"

say ""
say "== 混沌结果: PASS=$PASS FAIL=$FAIL =="
[ $FAIL -eq 0 ]
