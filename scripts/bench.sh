#!/usr/bin/env bash
# 10 万文件规模基准：生成 → 初次同步 → 增量同步 → 全量列举延迟。
# 用法: bash scripts/bench.sh [文件数，默认 100000]
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
N="${1:-100000}"
W="$(mktemp -d /tmp/ysync-bench.XXXXXX)"
SRV_ADDR="127.0.0.1:18799"
export no_proxy="127.0.0.1,localhost" NO_PROXY="127.0.0.1,localhost"
pkill -9 -f ysync-server-rs 2>/dev/null; sleep 0.3
ROOT="$ROOT" W="$W" N="$N" SRV_ADDR="$SRV_ADDR" bash -c '
pkill -9 -f ysync-server-rs 2>/dev/null; sleep 0.2
$ROOT/bin/y-sync-server-rs serve -addr $SRV_ADDR -data $W/server-data >/dev/null 2>&1 &
for i in $(seq 1 50); do curl -s $SRV_ADDR/healthz >/dev/null 2>&1 && break; sleep 0.1; done
printf "pw\n" | YSYNC_DATA=$W/server-data $ROOT/bin/y-sync-server-rs adduser bench >/dev/null 2>&1
mkdir -p $W/A/bench
YS="env YSYNC_CONFIG_DIR=$W/cfg $ROOT/bin/ysyncd-rs"
echo pw | $YS init -server http://$SRV_ADDR -user bench -device bench >/dev/null 2>&1
$YS add $W/A/bench --as bench >/dev/null

t0=$(date +%s)
for i in $(seq 1 $N); do echo "content-$i" > $W/A/bench/f$i.txt; done
t1=$(date +%s)
echo "生成 $N 文件: $((t1-t0))s"

t0=$(date +%s)
$YS sync 2>&1 | grep -E "synced|ERROR" | tail -1
t1=$(date +%s); echo "初次上传: $((t1-t0))s"

t0=$(date +%s)
$YS sync 2>&1 >/dev/null
t1=$(date +%s); echo "无变更对账: $((t1-t0))s"

echo mod-$RANDOM > $W/A/bench/f1.txt
t0=$(date +%s%N)
$YS sync >/dev/null 2>&1
t1=$(date +%s%N); echo "单文件修改传播: $(( (t1-t0)/1000000 ))ms"

T=$(python3 -c "import json;print(json.load(open(\"$W/cfg/config.json\"))[\"token\"])")
t0=$(date +%s%N)
curl -s -o /dev/null "http://$SRV_ADDR/api/v1/nodes?token=$T"
t1=$(date +%s%N); echo "nodes 全量列举: $(( (t1-t0)/1000000 ))ms"
kill %1 2>/dev/null
rm -rf $W
'
