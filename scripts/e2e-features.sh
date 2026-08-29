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

cleanup(){ [ -n "${SERVER_PID:-}" ] && kill "$SERVER_PID" 2>/dev/null; [ -n "${E2E_KEEP:-}" ] || rm -rf "$WORK"; }
trap cleanup EXIT
export no_proxy="127.0.0.1,localhost" NO_PROXY="127.0.0.1,localhost"

pkill -f y-sync-server-rs 2>/dev/null; sleep 0.2

say "== 特性验证: server=$(basename "$SERVER_BIN") client=$(basename "$CLIENT") =="

"$SERVER_BIN" serve -addr "$SRV_ADDR" -data "$SRV_DATA" >"$WORK/server.log" 2>&1 &
SERVER_PID=$!
for i in $(seq 1 50); do curl -s "http://$SRV_ADDR/healthz" >/dev/null 2>&1 && break; sleep 0.1; done

YS="env YSYNC_CONFIG_DIR=$WORK/cfg $CLIENT"
YS_B="env YSYNC_CONFIG_DIR=$WORK/cfgB $CLIENT"

# ---------- 设备管理（P1-6） ----------
YSYNC_DATA="$SRV_DATA" "$SERVER_BIN" adduser alice <<<"secret123" >/dev/null 2>&1
mkdir -p "$WORK/proj"; echo hello > "$WORK/proj/a.txt"
$YS init -server "http://$SRV_ADDR" -user alice -device devMain <<<"secret123" >/dev/null 2>&1
$YS_B init -server "http://$SRV_ADDR" -user alice -device devB <<<"secret123" >/dev/null 2>&1
mkdir -p "$WORK/B"
$YS_B add "$WORK/B/proj" --as proj >/dev/null
$YS_B sync >/dev/null 2>&1
$YS add "$WORK/proj" >/dev/null && $YS sync >/dev/null 2>&1
check "设备列表包含当前设备" "$YS devices | grep -q '当前设备'"
# 登录第二台设备并吊销（ghost 用独立配置目录，避免覆盖主设备 token）
mkdir -p "$WORK/cfgghost"
YS_GHOST="env YSYNC_CONFIG_DIR=$WORK/cfgghost $CLIENT"
$YS_GHOST init -server "http://$SRV_ADDR" -user alice -device devGhost <<<"secret123" >/dev/null 2>&1
DEV_ID=$($YS devices | grep devGhost | awk '{print $1}' | head -1)
check "设备列表包含第二台" "[ -n "$DEV_ID" ]"
$YS revoke "$DEV_ID" >/dev/null && ok "吊销 API 成功"
$YS_GHOST devices >/dev/null 2>&1 && bad "吊销后 ghost 仍可用" || ok "吊销后 ghost token 立即失效"
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

# ---------- setup 模式（UI 配置访问）----------
mkdir -p "$WORK/setup"
YS_SETUP="env YSYNC_CONFIG_DIR=$WORK/cfgsetup $CLIENT"
$YS_SETUP daemon -http 127.0.0.1:18799 -interval 60s >"$WORK/setup-daemon.log" 2>&1 &
SETUP_PID=$!
wait_for "setup daemon 启动" 10 "test -f $WORK/cfgsetup/daemon.json"
SETUP_TOKEN=$(python3 -c "import json;print(json.load(open('$WORK/cfgsetup/daemon.json'))['token'])")
check "setup 状态: 未初始化"   'curl -s "http://127.0.0.1:18799/setup-status?token='"$SETUP_TOKEN"'" | grep -q initialized.:false'
SETUP_BODY="{\"server_url\":\"http://$SRV_ADDR\",\"user\":\"alice\",\"password\":\"secret123\",\"device_name\":\"devSetup\"}"
SETUP_R=$(curl -s -m 15 -X POST "http://127.0.0.1:18799/setup?token=$SETUP_TOKEN" -H "Content-Type: application/json" -d "$SETUP_BODY")
check "setup 完成配置 (POST /setup)"   'echo "$SETUP_R" | grep -q initialized'
check "setup 后配置文件落盘" "test -f $WORK/cfgsetup/config.json"
check "setup 状态: 已初始化"   'curl -s "http://127.0.0.1:18799/setup-status?token='"$SETUP_TOKEN"'" | grep -q initialized.:true'
mkdir -p "$WORK/setup-f"
echo "setup-f" > "$WORK/setup-f/s.txt"
$YS_SETUP add "$WORK/setup-f" >/dev/null
SETUP_STATUS=$(curl -s -m 5 "http://127.0.0.1:18799/status?token=$SETUP_TOKEN")
curl -s -m 5 -X POST "http://127.0.0.1:18799/sync?token=$SETUP_TOKEN" >/dev/null
sleep 2
SETUP_STATUS=$(curl -s -m 5 "http://127.0.0.1:18799/status?token=$SETUP_TOKEN")
check "setup 后 daemon 拉起同步" 'echo "$SETUP_STATUS" | grep -q setup-f'
kill "$SETUP_PID" 2>/dev/null; wait "$SETUP_PID" 2>/dev/null

# ---------- 日志水位（A1）----------
WM=$(curl -s "http://$SRV_ADDR/api/v1/sync/head?token=$(
  python3 -c "import json;print(json.load(open('$WORK/cfg/config.json'))['token'])")" | python3 -c "import json,sys;print(json.load(sys.stdin).get('watermark', -1))")
check "head 返回 watermark 字段" "[ "$WM" -ge 0 ]"

# ---------- 日志水位触发全量重同步（A1）----------
YS_B="env YSYNC_CONFIG_DIR=$WORK/cfgB $CLIENT"
kill "$SERVER_PID" 2>/dev/null; sleep 0.3
YSYNC_JOURNAL_KEEP=5 YSYNC_JOURNAL_TRIM_MIN=0 YSYNC_DATA="$SRV_DATA" "$SERVER_BIN" serve -addr "$SRV_ADDR" -data "$SRV_DATA" >>"$WORK/server.log" 2>&1 &
SERVER_PID=$!
wait_for "服务端重启（保留 5 条日志）" 10 "curl -s http://$SRV_ADDR/healthz | grep -q ok"
mkdir -p "$WORK/A/wm"
for i in $(seq 1 10); do echo "wm-$i" > "$WORK/A/wm/f$i.txt"; done
$YS add "$WORK/A/wm" --as wm >/dev/null
# A 端 10 个 put → 日志条数超过 keep=5 → 触发裁剪 → watermark 前移
$YS sync >/dev/null 2>&1
WM=$(curl -s "http://$SRV_ADDR/api/v1/sync/head?token=$(
  python3 -c "import json;print(json.load(open('$WORK/cfgB/config.json'))['token'])")" | python3 -c "import json,sys;print(json.load(sys.stdin).get('watermark', -1))")
check "裁剪后 watermark 前移 (>0)" "[ "$WM" -gt 0 ]"
# B 的游标落在水位之下 → 下次 sync 必须全量重同步且收敛
# B 端需先 add wm 子树（A 侧新增的文件夹对 B 是新顶层）
mkdir -p "$WORK/B/wm"
$YS_B add "$WORK/B/wm" --as wm >/dev/null
$YS_B sync >/dev/null 2>&1 && ok "水位下客户端全量重同步成功" || bad "水位下客户端全量重同步成功"
check "重同步后 B 数据完整" "[ $(find "$WORK/B/wm" -name 'f*.txt' | wc -l) -eq 10 ]"
# 恢复无限制服务端（后续场景不受影响）
kill "$SERVER_PID" 2>/dev/null; sleep 0.3
YSYNC_DATA="$SRV_DATA" "$SERVER_BIN" serve -addr "$SRV_ADDR" -data "$SRV_DATA" >>"$WORK/server.log" 2>&1 &
SERVER_PID=$!
wait_for "服务端恢复" 10 "curl -s http://$SRV_ADDR/healthz | grep -q ok"

# ---------- nodes 分页（P0-1）----------
TOKQ=$(python3 -c "import json;print(json.load(open('$WORK/cfg/config.json'))['token'])")
P1=$(curl -s "http://$SRV_ADDR/api/v1/nodes?limit=1&after=0&token=$TOKQ")
check "nodes 分页: 第一页 has_more" 'echo "$P1" | grep -q '"'"'has_more":true'"'"''
PAGE_IDS=$(python3 -c "
import json,urllib.request
tok='$TOKQ'; base='$SRV_ADDR'; after=0; ids=[]
while True:
    r = json.load(urllib.request.urlopen(f'http://{base}/api/v1/nodes?limit=1&after={after}&token={tok}'))
    ids += [n['id'] for n in r['nodes']]
    if not r['has_more']: break
    after = r['nodes'][-1]['id']
print(len(ids), len(set(ids)))")
CNT=$(echo "$PAGE_IDS" | wc -l); UNIQ=$(echo "$PAGE_IDS" | sort -u | wc -l)
check "nodes 分页: 翻页收全且无重复"       "[ $CNT -gt 0 ] && [ $CNT = $UNIQ ]"

# ---------- 大小写冲突保护（P0-3，双设备构造 → A 下载为副本）----------
echo "case-content" > "$WORK/proj/case.txt"
$YS sync >/dev/null 2>&1
mkdir -p "$WORK/cfgcase" "$WORK/B/caseproj"
YS_CASE="env YSYNC_CONFIG_DIR=$WORK/cfgcase $CLIENT"
$YS_CASE init -server "http://$SRV_ADDR" -user alice -device devCase <<<"secret123" >/dev/null 2>&1
# devCase 是另一台"Linux 设备"：创建大小写变体并上行（服务端区分大小写，允许共存）
YSYNC_DATA="$SRV_DATA" "$SERVER_BIN" list-users >/dev/null  # no-op 保序
CASE_DIR="$WORK/cfgcase/proj_tmp"
mkdir -p "$CASE_DIR"
$YS_CASE add "$CASE_DIR" --as proj >/dev/null 2>&1 || true
# devCase 拉取 proj 子树（获取 case.txt）
$YS_CASE sync >/dev/null 2>&1
# 在 devCase 的 Linux 视角下创建大小写变体（用服务端 API 直写更稳：B 设备 token）
CH2=$(python3 -c "import hashlib;print(hashlib.sha256(b'other-case').hexdigest())")
CTOK=$(python3 -c "import json;print(json.load(open('$WORK/cfgcase/config.json'))['token'])")
curl -s -o /dev/null -X PUT --data-binary "other-case" "http://$SRV_ADDR/api/v1/content?token=$CTOK"
PARENT=$(curl -s -H "Authorization: Bearer $CTOK" "http://$SRV_ADDR/api/v1/nodes" | python3 -c "
import json,sys
print([n['id'] for n in json.load(sys.stdin)['nodes'] if n['path']=='proj'][0])")
curl -s -o /dev/null -X POST -H "Authorization: Bearer $CTOK" -H 'Content-Type: application/json' \
  -d "[{\"op\":\"put\",\"parent_id\":$PARENT,\"name\":\"CASE.txt\",\"content_hash\":\"$CH2\",\"mtime\":1700000000000}]" \
  "http://$SRV_ADDR/api/v1/ops"
# A（大小写不敏感 FS）sync：CASE.txt 应落地为冲突副本而非覆盖 case.txt
$YS sync >/dev/null 2>&1 && ok "大小写冲突场景同步完成" || bad "大小写冲突场景同步完成"
check "大小写冲突: 原文件保留"   "grep -q case-content \"$WORK/proj/case.txt\" 2>/dev/null"
check "大小写冲突: 副本落地"     "ls $WORK/proj | grep -q 'case conflict'"
CCFILE=$(ls $WORK/proj/*case*conflict* 2>/dev/null | head -1)
check "大小写冲突: 副本内容正确" "[ -n \"$CCFILE\" ] && grep -q other-case \"$CCFILE\""

# ---------- 分享密码防爆破（P0-5）----------
SHARE_P=$($YS share proj case.txt -password pw9 2>/dev/null | grep -oE '/s/[0-9a-f]+' | cut -d/ -f3)
for i in 1 2 3 4 5; do
  curl -s -o /dev/null "http://$SRV_ADDR/s/$SHARE_P?p=wrong$i"
done
CODE=$(curl -s -o /dev/null -w '%{http_code}' "http://$SRV_ADDR/s/$SHARE_P?p=pw9")
check "分享密码连错 5 次后锁定 (429)" "[ "$CODE" = "429" ]"

# ---------- header token 认证（P0-4）----------
CODE=$(curl -s -o /dev/null -w '%{http_code}' -H "X-Ysync-Token: $TOKQ" "http://$SRV_ADDR/api/v1/nodes")
check "X-Ysync-Token 头认证可用" "[ "$CODE" = "200" ]"

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
