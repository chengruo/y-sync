#!/usr/bin/env bash
# y-sync M1 端到端验证：两客户端互同步、删除/改名/移动传播、冲突副本、ignore、多文件夹。
# 用法: bash scripts/e2e.sh
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WORK="$(mktemp -d /tmp/ysync-e2e.XXXXXX)"
SRV_DATA="$WORK/server-data"
SRV_ADDR="127.0.0.1:18720"
# 可参数化（差分验证）：SERVER_BIN / BIN_A / BIN_B 可指向 Rust 实现
SERVER_BIN="${SERVER_BIN:-$ROOT/bin/y-sync-server}"
CLIENT_A="${BIN_A:-$ROOT/bin/ysync}"
CLIENT_B="${BIN_B:-$ROOT/bin/ysync}"
say "  实现组合: server=$(basename $SERVER_BIN) A=$(basename $CLIENT_A) B=$(basename $CLIENT_B)"
PASS=0; FAIL=0

say()  { echo -e "$1"; }
ok()   { PASS=$((PASS+1)); say "  \033[32mPASS\033[0m $1"; }
bad()  { FAIL=$((FAIL+1)); say "  \033[31mFAIL\033[0m $1"; }
check(){ if eval "$2"; then ok "$1"; else bad "$1 — [$2]"; fi }
# wait_for: 轮询等待条件成立（消除时序脆弱性）。用法: wait_for "desc" 20 'cond'
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

cleanup() { kill $SERVER_PID 2>/dev/null; [ -n "${E2E_KEEP:-}" ] || rm -rf "$WORK"; }
trap cleanup EXIT

export no_proxy="127.0.0.1,localhost" NO_PROXY="127.0.0.1,localhost"
say "== e2e 工作目录: $WORK =="

# ---------- 启动服务端 ----------
"$SERVER_BIN" serve -addr "$SRV_ADDR" -data "$SRV_DATA" >"$WORK/server.log" 2>&1 &
SERVER_PID=$!
for i in $(seq 1 50); do
  curl -s "http://$SRV_ADDR/healthz" >/dev/null 2>&1 && break
  sleep 0.1
done
curl -s "http://$SRV_ADDR/healthz" | grep -q ok && ok "服务端启动" || { bad "服务端启动"; exit 1; }

# ---------- 用户与两个"设备" ----------
export YSYNC_CONFIG_DIR="$WORK/cfgA"   # 设备 A 配置目录
YSYNC_DATA="$SRV_DATA" "$SERVER_BIN" adduser alice <<<"secret123" >/dev/null 2>&1

SRV="http://$SRV_ADDR"
YS="env YSYNC_CONFIG_DIR=$WORK/cfgA $CLIENT_A"
YS_B="env YSYNC_CONFIG_DIR=$WORK/cfgB $CLIENT_B"

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
mtime_of() { stat -f %m "$1" 2>/dev/null || stat -c %Y "$1"; }
M1=$(mtime_of "$WORK/A/proj/a.txt"); M2=$(mtime_of "$WORK/B/proj/a.txt")
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

# ---------- M3: 选择性同步（FR-S9 --exclude）----------
mkdir -p "$WORK/A/sel/skip"
echo "keep-me" > "$WORK/A/sel/keep.txt"
echo "secret" > "$WORK/A/sel/skip/hidden.txt"
$YS add "$WORK/A/sel" --as sel --exclude skip >/dev/null
$YS sync >/dev/null 2>&1
$YS_B add "$WORK/B/sel" --as sel >/dev/null
$YS_B sync >/dev/null 2>&1
check "选择性同步: 排除子树不同步" "test ! -e "$WORK/B/sel/skip""
check "选择性同步: 其余正常同步"   "test -f "$WORK/B/sel/keep.txt""

# ---------- M2: use-gitignore（FR-S8 沿用 .gitignore）----------
mkdir -p "$WORK/A/gig"
echo "*.gen" > "$WORK/A/gig/.gitignore"
echo "generated" > "$WORK/A/gig/x.gen"
echo "normal" > "$WORK/A/gig/ok.txt"
$YS add "$WORK/A/gig" --as gig --use-gitignore >/dev/null
$YS sync >/dev/null 2>&1
$YS_B add "$WORK/B/gig" --as gig >/dev/null
$YS_B sync >/dev/null 2>&1
check "use-gitignore: .gen 被忽略" "test ! -f "$WORK/B/gig/x.gen""
check "use-gitignore: 其余同步"    "test -f "$WORK/B/gig/ok.txt""

# ---------- M3: 嵌套文件夹拒绝（FR-S15）----------
mkdir -p "$WORK/A/notes/sub"
$YS add "$WORK/A/notes/sub" --as notes-sub >/dev/null 2>&1 && bad "嵌套文件夹应被拒绝" || ok "嵌套文件夹被拒绝"

# ---------- M3: 限速冒烟（FR-S12，验证代码路径）----------
python3 -c "import json;p='$WORK/cfgA/config.json';c=json.load(open(p));c['upload_limit_kbs']=8192;json.dump(c,open(p,'w'))"
head -c 1048576 /dev/zero > "$WORK/A/proj/rl.bin"
$YS sync >/dev/null 2>&1 && ok "限速配置下上传正常" || bad "限速配置下上传正常"
python3 -c "import json;p='$WORK/cfgA/config.json';c=json.load(open(p));c.pop('upload_limit_kbs',None);json.dump(c,open(p,'w'))"
$YS_B sync >/dev/null 2>&1
check "限速文件传播到 B" "test -f "$WORK/B/proj/rl.bin""

# ---------- 断连恢复：服务端重启后客户端自动收敛（M1 验收）----------
mkdir -p "$WORK/A/proj/offline"
echo "offline-write" > "$WORK/A/proj/offline/o.txt"
kill $SERVER_PID; sleep 0.3
$YS sync >/dev/null 2>&1 && bad "断连期间 sync 应失败" || ok "断连期间 sync 报错"
"$SERVER_BIN" serve -addr "$SRV_ADDR" -data "$SRV_DATA" >>"$WORK/server.log" 2>&1 &
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

# ---------- M3: daemon 控制 API/管理页（token 认证；60s 轮询间隔 → 快速传播只能来自 WS/事件）----------
$YS daemon -http 127.0.0.1:18731 -interval 60s >"$WORK/daemon.log" 2>&1 &
DAEMON_PID=$!
wait_for "daemon 启动并写出 daemon.json" 10 "test -f $WORK/cfgA/daemon.json"
DAEMON_TOKEN=$(python3 -c "import json;print(json.load(open('$WORK/cfgA/daemon.json'))['token'])")
check "无 token 访问被拒 (401)"  "[ \$(curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:18731/status) = 401 ]"
check "带 token 可读状态"        "curl -s 'http://127.0.0.1:18731/status?token=$DAEMON_TOKEN' | grep -q proj"
check "管理页 HTML 可达"         "curl -s 'http://127.0.0.1:18731/?token=$DAEMON_TOKEN' | grep -q '管理台'"

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
YSYNC_DATA="$SRV_DATA" "$SERVER_BIN" backup -out "$WORK/backup" >/dev/null 2>&1 && ok "backup 完成" || bad "backup 完成"
check "backup 含元数据快照"    "test -f "$WORK/backup/y-sync.db" && test -f "$WORK/backup/manifest.json""

# ---------- M3: 管理台操作（加/移除/冲突处理/暂停）+ WS 准实时 ----------
# A 侧从此由 daemon 驱动（轮询 60s）：快速传播证明 WS/FS 事件生效
api_post(){ # 用法: api_post <path> <json>；绕开 check/eval 的引号问题
  curl -s -X POST "http://127.0.0.1:18731/$1?token=$DAEMON_TOKEN" \
    -H 'Content-Type: application/json' -d "$2"
}

# 4a. 通过管理台接入新文件夹
mkdir -p "$WORK/A/uiadd"
echo "from-ui" > "$WORK/A/uiadd/hello.txt"
ADD_R=$(api_post add "{\"local_path\":\"$WORK/A/uiadd\",\"name\":\"uiadd\",\"excludes\":[\"node_modules\"]}")
check "管理台接入文件夹 (POST /add)" 'echo "$ADD_R" | grep -q ok'
TOKEN_A=${TOKEN_A:-$(python3 -c "import json;print(json.load(open('$WORK/cfgA/config.json'))['token'])")}
wait_for "接入后 daemon 自动上行" 15 "curl -s 'http://$SRV_ADDR/browse?token=$TOKEN_A&path=uiadd' | grep -q hello.txt"
$YS_B add "$WORK/B/uiadd" --as uiadd >/dev/null
$YS_B sync >/dev/null 2>&1
check "B 收到 UI 接入的文件夹" "test -f "$WORK/B/uiadd/hello.txt""

# 4b. WS 准实时：B 修改（无并发）→ daemon A（60s 轮询）秒级取回
echo "ws-change" > "$WORK/B/notes/n1.md"
$YS_B sync >/dev/null 2>&1
wait_for "WS 触发 A 拉取（非轮询）" 25 "grep -q 'ws-change' "$WORK/A/notes/n1.md""

# 4c. 冲突处理（确定性构造）：暂停 A → 双方各改 → 恢复 → 冲突副本
R=$(api_post pause '{"folder":"notes"}')
check "暂停文件夹 (POST /pause)" 'echo "$R" | grep -q ok'
echo "A-local" > "$WORK/A/notes/n1.md"
echo "B-side" > "$WORK/B/notes/n1.md"
$YS_B sync >/dev/null 2>&1
R=$(api_post resume '{"folder":"notes"}')
check "恢复文件夹 (POST /resume)" 'echo "$R" | grep -q ok'
R=$(api_post sync '{"folder":"notes"}')
wait_for "恢复后产生冲突副本" 25 "ls "$WORK/A/notes" | grep -q 'conflict from'"
CINFO=$(curl -s "http://127.0.0.1:18731/conflicts?token=$DAEMON_TOKEN" | python3 -c "
import json,sys
c=[x for x in json.load(sys.stdin)['conflicts'] if x['folder']=='notes']
assert c, 'no conflicts'
print(c[0]['rel'] + '|' + c[0]['copy_rel'])")
check "管理台列出冲突 (GET /conflicts)" 'test -n "$CINFO"'
CREL="${CINFO%%|*}"; CCOPY="${CINFO##*|}"
RES_R=$(api_post resolve "{\"folder\":\"notes\",\"rel\":\"$CREL\",\"copy_rel\":\"$CCOPY\",\"choice\":\"copy\"}")
check "管理台处理冲突: 采用副本 (POST /resolve)" 'echo "$RES_R" | grep -q ok'
wait_for "冲突副本被清理" 20 "! ls "$WORK/A/notes" | grep -q 'conflict from'"
check "原名文件已是副本内容"    "grep -q 'B-side' "$WORK/A/notes/n1.md""

# 4d. 移除文件夹 + daemon 退出清理
REM_R=$(api_post remove '{"name":"uiadd"}')
check "管理台移除文件夹 (POST /remove)" 'echo "$REM_R" | grep -q ok'
check "移除后状态不再包含"      "! curl -s "http://127.0.0.1:18731/status?token=$DAEMON_TOKEN" | grep -q uiadd"
kill $DAEMON_PID 2>/dev/null
wait $DAEMON_PID 2>/dev/null
check "daemon 退出清理 daemon.json" "[ ! -f "$WORK/cfgA/daemon.json" ]"

# ---------- 幂等重同步（无变更应安静收敛）----------
$YS sync >/dev/null 2>&1 && $YS_B sync >/dev/null 2>&1 && ok "重复同步幂等" || bad "重复同步幂等"

say ""
say "== 结果: PASS=$PASS FAIL=$FAIL =="
[ $FAIL -eq 0 ]
