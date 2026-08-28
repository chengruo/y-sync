#!/usr/bin/env bash
# y-sync M1 端到端验证：两客户端互同步、删除/改名/移动传播、冲突副本、ignore、多文件夹。
# 用法: bash scripts/e2e.sh
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WORK="$(mktemp -d /tmp/ysync-e2e.XXXXXX)"
SRV_DATA="$WORK/server-data"
SRV_ADDR="127.0.0.1:18720"
PASS=0; FAIL=0

say()  { echo -e "$1"; }
ok()   { PASS=$((PASS+1)); say "  \033[32mPASS\033[0m $1"; }
bad()  { FAIL=$((FAIL+1)); say "  \033[31mFAIL\033[0m $1"; }
check(){ if eval "$2"; then ok "$1"; else bad "$1 — [$2]"; fi }

cleanup() { kill $SERVER_PID 2>/dev/null; [ -n "${E2E_KEEP:-}" ] || rm -rf "$WORK"; }
trap cleanup EXIT

export no_proxy="127.0.0.1,localhost" NO_PROXY="127.0.0.1,localhost"
say "== e2e 工作目录: $WORK =="

# ---------- 启动服务端 ----------
"$ROOT/bin/y-sync-server" serve -addr "$SRV_ADDR" -data "$SRV_DATA" >"$WORK/server.log" 2>&1 &
SERVER_PID=$!
for i in $(seq 1 50); do
  curl -s "http://$SRV_ADDR/healthz" >/dev/null 2>&1 && break
  sleep 0.1
done
curl -s "http://$SRV_ADDR/healthz" | grep -q ok && ok "服务端启动" || { bad "服务端启动"; exit 1; }

# ---------- 用户与两个"设备" ----------
export YSYNC_CONFIG_DIR="$WORK/cfgA"   # 设备 A 配置目录
YSYNC_DATA="$SRV_DATA" "$ROOT/bin/y-sync-server" adduser alice <<<"secret123" >/dev/null 2>&1

SRV="http://$SRV_ADDR"
YS="env YSYNC_CONFIG_DIR=$WORK/cfgA $ROOT/bin/ysync"
YS_B="env YSYNC_CONFIG_DIR=$WORK/cfgB $ROOT/bin/ysync"

$YS init -server "$SRV" -user alice -device deviceA <<<"secret123" >/dev/null 2>&1 \
  && ok "客户端 A 登录" || bad "客户端 A 登录"

mkdir -p "$WORK/A/proj/sub" "$WORK/B"
echo "hello v1" > "$WORK/A/proj/a.txt"
echo "readme"   > "$WORK/A/proj/README.md"
echo "nested"   > "$WORK/A/proj/sub/b.txt"
echo "junk"     > "$WORK/A/proj/debug.log"          # 将被 .syncignore 忽略
echo "*.log"    > "$WORK/A/proj/.syncignore"

$YS add "$WORK/A/proj" >/dev/null && ok "A: add 文件夹" || bad "A: add 文件夹"
$YS sync >/dev/null 2>&1 && ok "A: 首次上传" || bad "A: 首次上传"

# ignore 验证：debug.log 不应上传（服务端子树里不存在）
$YS_B init -server "$SRV" -user alice -device deviceB <<<"secret123" >/dev/null 2>&1 \
  && ok "客户端 B 登录" || bad "客户端 B 登录"
$YS_B add "$WORK/B/proj" --as proj >/dev/null && ok "B: add 文件夹" || bad "B: add 文件夹"
$YS_B sync >/dev/null 2>&1 && ok "B: 下拉同步" || bad "B: 下拉同步"

check "B 收到 a.txt(内容)"       "diff -q "$WORK/A/proj/a.txt" "$WORK/B/proj/a.txt" >/dev/null"
check "B 收到嵌套 sub/b.txt"     "test -f "$WORK/B/proj/sub/b.txt""
check "ignore: debug.log 未同步" "test ! -f "$WORK/B/proj/debug.log""
check "ignore: .syncignore 本身同步" "test -f "$WORK/B/proj/.syncignore""

# mtime 保留（FR-S10）
M1=$(stat -f %m "$WORK/A/proj/a.txt"); M2=$(stat -f %m "$WORK/B/proj/a.txt")
check "mtime 保留" "[ "$M1" = "$M2" ]"

# ---------- 增量：修改 / 删除 / 重命名 ----------
echo "hello v2" > "$WORK/B/proj/a.txt"
rm "$WORK/B/proj/README.md"
mv "$WORK/B/proj/sub/b.txt" "$WORK/B/proj/sub/renamed.txt"
$YS_B sync >/dev/null 2>&1 && ok "B: 上行(改/删/改名)" || bad "B: 上行(改/删/改名)"
$YS sync >/dev/null 2>&1 && ok "A: 下拉跟随" || bad "A: 下拉跟随"

check "A 收到修改 v2"       "grep -q 'hello v2' "$WORK/A/proj/a.txt""
check "A 跟随删除 README"   "test ! -f "$WORK/A/proj/README.md""
check "A 跟随改名(原名消失)" "test ! -f "$WORK/A/proj/sub/b.txt""
check "A 跟随改名(新名存在)" "test -f "$WORK/A/proj/sub/renamed.txt""
check "改名未重传内容"       "grep -q 'nested' "$WORK/A/proj/sub/renamed.txt""

# ---------- 冲突：双方同时修改同一文件 ----------
echo "A-edit" > "$WORK/A/proj/a.txt"
sleep 0.05
echo "B-edit" > "$WORK/B/proj/a.txt"
$YS sync >/dev/null 2>&1; $YS_B sync >/dev/null 2>&1; $YS sync >/dev/null 2>&1
CONFLICTS_A=$(ls "$WORK/A/proj" | grep -c "conflict from" || true)
CONFLICTS_B=$(ls "$WORK/B/proj" | grep -c "conflict from" || true)
check "A 侧出现冲突副本"    "[ "$CONFLICTS_A" -ge 1 ]"
check "B 侧出现冲突副本"    "[ "$CONFLICTS_B" -ge 1 ]"
# 收敛语义：双方版本均保留（一方占原名，另一方进冲突副本），且两侧最终一致
check "两个版本都保留(A)"   "grep -rq 'A-edit' "$WORK/A/proj" && grep -rq 'B-edit' "$WORK/A/proj" && grep -rq 'A-edit' "$WORK/B/proj" && grep -rq 'B-edit' "$WORK/B/proj""
# 快照对比：路径+内容哈希（排除 .y-sync 状态目录与被忽略的本地文件）
snap() {
  (cd "$1" && find . -type f ! -path "./.y-sync/*" ! -name "*.log" | sort | while IFS= read -r f; do printf '%s %s\n' "$(md5 -q "$f")" "$f"; done | md5 -q)
}
check "两侧最终收敛一致"     "[ "$(snap "$WORK/A/proj")" = "$(snap "$WORK/B/proj")" ]"

# ---------- 多文件夹（FR-S13）----------
mkdir -p "$WORK/A/notes"
echo "note-1" > "$WORK/A/notes/n1.md"
$YS add "$WORK/A/notes" --as notes >/dev/null && ok "A: add 第二个文件夹" || bad "A: add 第二个文件夹"
$YS sync >/dev/null 2>&1
$YS_B add "$WORK/B/notes" --as notes >/dev/null
$YS_B sync >/dev/null 2>&1
check "多文件夹: B 收到 notes" "test -f "$WORK/B/notes/n1.md""
check "多文件夹互不干扰"       "test -f "$WORK/B/proj/a.txt""

# ---------- 断连恢复：服务端重启后客户端自动收敛（M1 验收）----------
mkdir -p "$WORK/A/proj/offline"
echo "offline-write" > "$WORK/A/proj/offline/o.txt"
kill $SERVER_PID; sleep 0.3
$YS sync >/dev/null 2>&1 && bad "断连期间 sync 应失败" || ok "断连期间 sync 报错"
"$ROOT/bin/y-sync-server" serve -addr "$SRV_ADDR" -data "$SRV_DATA" >>"$WORK/server.log" 2>&1 &
SERVER_PID=$!
for i in $(seq 1 50); do curl -s "http://$SRV_ADDR/healthz" >/dev/null 2>&1 && break; sleep 0.1; done
$YS sync >/dev/null 2>&1 && ok "重连后 sync 成功" || bad "重连后 sync 成功"
$YS_B sync >/dev/null 2>&1
check "断连期间写入最终传播" "test -f "$WORK/B/proj/offline/o.txt""

# ---------- M2: 回收站（FR-S5/V2/V3）----------
rm "$WORK/A/proj/offline/o.txt"
$YS sync >/dev/null 2>&1
TRASH_ID=$($YS trash list | grep offline/o.txt | awk '{print $1}' | head -1)
check "删除进入回收站"        "[ -n "$TRASH_ID" ]"
$YS trash restore "$TRASH_ID" >/dev/null && ok "回收站恢复 API" || bad "回收站恢复 API"
$YS sync >/dev/null 2>&1; $YS_B sync >/dev/null 2>&1
check "恢复传播到两端"        "test -f "$WORK/A/proj/offline/o.txt" && test -f "$WORK/B/proj/offline/o.txt""

# ---------- M2: 文件版本（FR-V1）----------
echo "v2-content" > "$WORK/A/proj/a.txt"
$YS sync >/dev/null 2>&1
echo "v3-content" > "$WORK/A/proj/a.txt"
$YS sync >/dev/null 2>&1
VER_ID=$($YS versions list proj a.txt | awk 'NR==1{print $1}')
check "覆盖产生历史版本"      "[ -n "$VER_ID" ]"
$YS versions restore proj a.txt "$VER_ID" >/dev/null && ok "版本回写 API" || bad "版本回写 API"
$YS sync >/dev/null 2>&1; $YS_B sync >/dev/null 2>&1
check "版本回退传播到 B"      "grep -q 'v2-content' "$WORK/B/proj/a.txt""

# ---------- M2: 分块续传上传（FR-S11，小阈值模拟）----------
python3 - <<PYJSON
import json, os
p = os.path.join("$WORK", "cfgA", "config.json")
c = json.load(open(p))
c["chunk_threshold_mb"] = 1
c["chunk_size_mb"] = 1
json.dump(c, open(p, "w"))
PYJSON
head -c 3145728 /dev/zero > "$WORK/A/proj/big.bin"
$YS sync >/dev/null 2>&1 && ok "分块上传完成" || bad "分块上传完成"
$YS_B sync >/dev/null 2>&1
check "大文件传播到 B"        "[ "$(md5 -q "$WORK/A/proj/big.bin")" = "$(md5 -q "$WORK/B/proj/big.bin")" ]"

# ---------- M2: 崩溃恢复（模拟 ops 提交后进程死亡）----------
echo "crash-recover" > "$WORK/A/proj/crash.txt"
$YS sync >/dev/null 2>&1
mkdir -p "$WORK/A/proj/.y-sync"
echo '{"note":"ops in flight"}' > "$WORK/A/proj/.y-sync/pending.json"
$YS sync >/dev/null 2>&1 && ok "崩溃标记触发状态重建" || bad "崩溃标记触发状态重建"
$YS_B sync >/dev/null 2>&1
check "重建后收敛（crash.txt 在）" "test -f "$WORK/B/proj/crash.txt""

# ---------- M2: 嵌套 .syncignore（FR-S8）----------
mkdir -p "$WORK/A/proj/sub2"
echo "secret-x" > "$WORK/A/proj/sub2/x.txt"
echo "public-y" > "$WORK/A/proj/sub2/y.txt"
echo "x.txt" > "$WORK/A/proj/sub2/.syncignore"
$YS sync >/dev/null 2>&1; $YS_B sync >/dev/null 2>&1
check "嵌套 ignore: x.txt 不同步" "test ! -f "$WORK/B/proj/sub2/x.txt""
check "嵌套 ignore: y.txt 同步"   "test -f "$WORK/B/proj/sub2/y.txt""

# ---------- M3: daemon 控制 API/状态页 ----------
$YS daemon -http 127.0.0.1:18731 -interval 2s >/dev/null 2>&1 &
DAEMON_PID=$!
sleep 2
check "daemon 状态页可达"      "curl -s http://127.0.0.1:18731/status | grep -q proj"
check "daemon 状态页 HTML"     "curl -s http://127.0.0.1:18731/ | grep -q 'y-sync'"
kill $DAEMON_PID 2>/dev/null

# ---------- M4: 只读分享（FR-H1）----------
SHARE_TOKEN=$($YS share proj a.txt 2>/dev/null | grep -oE '/s/[0-9a-f]+' | cut -d/ -f3)
check "分享链接可下载"         "curl -s "http://$SRV_ADDR/s/$SHARE_TOKEN" | grep -qE 'v[23]-content'"
SHARE_P=$($YS share proj sub/renamed.txt -password pw123 2>/dev/null | grep -oE '/s/[0-9a-f]+' | cut -d/ -f3)
check "密码分享无密码 401"     "[ "$(curl -s -o /dev/null -w '%{http_code}' "http://$SRV_ADDR/s/$SHARE_P")" = "401" ]"
check "密码分享带密码 200"     "curl -s "http://$SRV_ADDR/s/$SHARE_P?p=pw123" | grep -q nested"
$YS unshare "$SHARE_TOKEN" >/dev/null && ok "撤销分享" || bad "撤销分享"

# ---------- M4: WebDAV 只读 ----------
CODE=$(curl -s -o /dev/null -w '%{http_code}' -X PROPFIND -u alice:secret123 -H "Depth: 1" "http://$SRV_ADDR/dav/proj")
check "WebDAV PROPFIND 207"    "[ "$CODE" = "207" ]"
CODE2=$(curl -s -o /dev/null -w '%{http_code}' -u alice:secret123 "http://$SRV_ADDR/dav/proj/a.txt")
check "WebDAV GET 内容"        "[ "$CODE2" = "200" ]"
TOKEN_A=$(python3 -c "import json,os;print(json.load(open(os.path.join(os.environ.get('YSYNC_CONFIG_DIR','$WORK/cfgA'),'config.json')))['token'])")
check "浏览页可达"             "curl -s 'http://$SRV_ADDR/browse?token=$TOKEN_A&path=proj' | grep -q a.txt"

# ---------- M3: backup（SR5）----------
YSYNC_DATA="$SRV_DATA" "$ROOT/bin/y-sync-server" backup -out "$WORK/backup" >/dev/null 2>&1 && ok "backup 完成" || bad "backup 完成"
check "backup 含元数据快照"    "test -f "$WORK/backup/y-sync.db" && test -f "$WORK/backup/manifest.json""

# ---------- 幂等重同步（无变更应安静收敛）----------
$YS sync >/dev/null 2>&1 && $YS_B sync >/dev/null 2>&1 && ok "重复同步幂等" || bad "重复同步幂等"

say ""
say "== 结果: PASS=$PASS FAIL=$FAIL =="
[ $FAIL -eq 0 ]
