#!/usr/bin/env bash
# 自动部署：backup → 上传新二进制 → 原子切换 → 健康检查（版本比对）→ 失败自动回滚。
#
# 用法:
#   scripts/deploy.sh                                    # 部署当前/指定版本
#   scripts/deploy.sh bootstrap <域名> <证书路径*> <私钥路径*> [二进制]
#       * 证书/私钥为「服务器上的路径」；服务器一次性初始化（nginx 站点/systemd/用户）
#
# 环境变量:
#   DEPLOY_HOST      服务器地址（必填；CI 配 Secrets，本地放 deploy/deploy.env）
#   DEPLOY_PORT      SSH 端口（默认 22）
#   DEPLOY_USER      SSH 用户（默认 ubuntu）
#   DEPLOY_KEY_FILE  SSH 私钥路径；或 DEPLOY_KEY（私钥内容，CI Secrets 用）
#   DEPLOY_VERSION   版本标识（默认 git describe；CI 传 tag）
#   BIN              待部署的 linux 服务端二进制（缺省找预构建产物或本地构建）
#   DEPLOY_DRY_RUN=1 只打印将执行的命令，不实际执行
#
# 前提: 服务器上 ubuntu 账号有免密 sudo（systemctl restart 等）。
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

DRY_RUN="${DEPLOY_DRY_RUN:-0}"
[ -f deploy/deploy.env ] && source deploy/deploy.env
DEPLOY_HOST="${DEPLOY_HOST:?需要 DEPLOY_HOST（服务器地址）}"
DEPLOY_PORT="${DEPLOY_PORT:-22}"
DEPLOY_USER="${DEPLOY_USER:-ubuntu}"
DEPLOY_VERSION="${DEPLOY_VERSION:-$(git -C "$ROOT" describe --tags --always 2>/dev/null || echo dev)}"
DEPLOY_VERSION="${DEPLOY_VERSION#v}"

# SSH 密钥
SSH_KEY_ARGS=()
if [ -n "${DEPLOY_KEY:-}" ]; then
  KEY_FILE="$(mktemp)"; printf '%s\n' "$DEPLOY_KEY" > "$KEY_FILE"; chmod 600 "$KEY_FILE"
  SSH_KEY_ARGS=(-i "$KEY_FILE")
elif [ -n "${DEPLOY_KEY_FILE:-}" ]; then
  SSH_KEY_ARGS=(-i "$DEPLOY_KEY_FILE")
fi
# bash 3.2 兼容：空数组在 set -u 下的展开需要 guard
# 注意 ssh 端口是小写 -p，scp 是大写 -P（两者选项集不同，必须分开传）
BASE_SSH_OPTS=(${SSH_KEY_ARGS[@]+"${SSH_KEY_ARGS[@]}"} -o StrictHostKeyChecking=accept-new -o ConnectTimeout=10)
SSH_PORT_OPTS=(-p "$DEPLOY_PORT")
SCP_PORT_OPTS=(-P "$DEPLOY_PORT")

ssh_run() { # 单条远程命令
  if [ "$DRY_RUN" = "1" ]; then
    echo "[dry-run] ssh $DEPLOY_USER@$DEPLOY_HOST -- $1"
  else
    ssh "${BASE_SSH_OPTS[@]}" "${SSH_PORT_OPTS[@]}" "$DEPLOY_USER@$DEPLOY_HOST" "$1"
  fi
}
ssh_script() { # 远程脚本：stdin 传入（quoted heredoc，远端变量不与本地混淆）；$1=远程环境变量
  local envs="$1"
  if [ "$DRY_RUN" = "1" ]; then
    echo "[dry-run] ssh $DEPLOY_USER@$DEPLOY_HOST env $envs bash -s <<'REMOTE_SCRIPT'"
    cat
    echo "REMOTE_SCRIPT"
  else
    ssh "${SSH_OPTS[@]}" "$DEPLOY_USER@$DEPLOY_HOST" "env $envs bash -s"
  fi
}
scp_to() {
  if [ "$DRY_RUN" = "1" ]; then
    echo "[dry-run] scp $1 → $DEPLOY_USER@$DEPLOY_HOST:$2"
  else
    scp -q "${BASE_SSH_OPTS[@]}" "${SCP_PORT_OPTS[@]}" "$1" "$DEPLOY_USER@$DEPLOY_HOST:$2"
  fi
}

cmd="${1:-deploy}"

# ---------- 服务器一次性初始化 ----------
if [ "$cmd" = "bootstrap" ]; then
  DOMAIN="${2:?需要域名}"
  CERT="${3:?需要证书文件路径（服务器上的路径）}"
  KEY_PATH="${4:?需要私钥文件路径（服务器上的路径）}"
  scp_to "$ROOT/deploy/bootstrap.sh" "/tmp/y-sync-bootstrap.sh"
  scp_to "$ROOT/deploy/nginx-y-sync.conf.example" "/tmp/y-sync-nginx.conf.example"
  scp_to "$ROOT/deploy/y-sync-server.service" "/tmp/y-sync-server.service"
  if [ -n "${BIN:-}" ] && [ -f "$BIN" ] && [ "$DRY_RUN" != "1" ]; then
    scp_to "$BIN" "/tmp/y-sync-first"
    ssh_run "sudo bash /tmp/y-sync-bootstrap.sh $DOMAIN $CERT $KEY_PATH /tmp/y-sync-first"
  else
    ssh_run "sudo bash /tmp/y-sync-bootstrap.sh $DOMAIN $CERT $KEY_PATH"
  fi
  echo "== 初始化完成 =="
  exit 0
fi

# ---------- 部署 ----------
if [ -z "${BIN:-}" ]; then
  for cand in "bin/y-sync-server-rs-linux-amd64-$DEPLOY_VERSION" \
              "bin/y-sync-server-rs-linux-amd64" \
              "target/x86_64-unknown-linux-gnu/release/ysync-server-rs" \
              "target/release/ysync-server-rs"; do
    if [ -f "$ROOT/$cand" ]; then BIN="$ROOT/$cand"; break; fi
  done
  if [ -z "${BIN:-}" ]; then
    echo "未找到预构建 linux 二进制，尝试本地构建（要求当前平台为 linux 或已装交叉工具链）…"
    cargo build --release -p ysync-server-rs
    BIN="$ROOT/target/release/ysync-server-rs"
  fi
fi
[ -f "$BIN" ] || { echo "二进制不存在: $BIN"; exit 1; }

echo "== 部署 $DEPLOY_VERSION → $DEPLOY_USER@$DEPLOY_HOST =="
[ "$DRY_RUN" = "1" ] && echo "[dry-run] BIN=$BIN"

# 0. 连通性
ssh_run "echo ok" >/dev/null

# 1. 部署前数据快照（首次部署时服务未装，自动跳过）
ssh_run "sudo mkdir -p /var/lib/y-sync/backups && \
  (sudo env YSYNC_DATA=/var/lib/y-sync /opt/y-sync/current/y-sync-server-rs backup \
     -out /var/lib/y-sync/backups/pre-$DEPLOY_VERSION-$(date +%Y%m%d%H%M%S) 2>/dev/null \
   || echo '[warn] 服务尚未安装，跳过 backup（首次部署）')"

# 2. 上传新二进制（不停服）
ssh_run "mkdir -p /opt/y-sync/releases/$DEPLOY_VERSION /opt/y-sync/incoming"
scp_to "$BIN" "/opt/y-sync/incoming/y-sync-server-rs.$DEPLOY_VERSION"

# 3. 远程：原子切换 + 健康检查（版本比对）+ 失败回滚（全部变量在远端展开）
ssh_script "VERSION=$DEPLOY_VERSION" <<'REMOTE'
set -euo pipefail
install -m 0755 /opt/y-sync/incoming/y-sync-server-rs.$VERSION \
  /opt/y-sync/releases/$VERSION/y-sync-server-rs
chown y-sync:y-sync /opt/y-sync/releases/$VERSION/y-sync-server-rs
PREV=$(readlink -f /opt/y-sync/current || true)
ln -sfn "releases/$VERSION" /opt/y-sync/current.new
mv -T /opt/y-sync/current.new /opt/y-sync/current
sudo systemctl restart y-sync-server

BODY=""
for i in $(seq 1 10); do
  BODY=$(curl -fsS http://127.0.0.1:8720/healthz 2>/dev/null || true)
  [ -n "$BODY" ] && break
  sleep 1
done
if echo "$BODY" | grep -q "\"version\":\"$VERSION\""; then
  echo "健康检查通过（版本 $VERSION）"
  rm -f /opt/y-sync/incoming/y-sync-server-rs.$VERSION
else
  echo "健康检查失败（body=$BODY），回滚到 ${PREV:-<无旧版>}"
  if [ -n "${PREV:-}" ]; then
    ln -sfn "$PREV" /opt/y-sync/current.new
    mv -T /opt/y-sync/current.new /opt/y-sync/current
    sudo systemctl restart y-sync-server
    for i in $(seq 1 10); do
      curl -fsS http://127.0.0.1:8720/healthz 2>/dev/null && break
      sleep 1
    done
  fi
  exit 1
fi
REMOTE

echo "== 部署完成: 版本 $DEPLOY_VERSION =="
