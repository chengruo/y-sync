#!/usr/bin/env bash
# 服务器一次性初始化（在服务器上以 sudo 执行；通常由 deploy.sh bootstrap 远程调用）。
# 用法: sudo bash bootstrap.sh <域名> <证书文件> <私钥文件> [服务端二进制]
#   - 创建 y-sync 系统用户与目录
#   - 安装 systemd 单元并开机自启
#   - 安装 nginx 站点（使用你已有的证书），nginx -t 通过后 reload
set -euo pipefail

DOMAIN="${1:?用法: bootstrap.sh <域名> <证书文件> <私钥文件> [二进制]}"
CERT="${2:?缺少证书文件路径}"
KEY="${3:?缺少私钥文件路径}"
BIN="${4:-}"

[ -f "$CERT" ] || { echo "证书不存在: $CERT"; exit 1; }
[ -f "$KEY" ]  || { echo "私钥不存在: $KEY";  exit 1; }

echo "== y-sync 服务器初始化 =="

# 1. 系统用户与目录
id y-sync >/dev/null 2>&1 || useradd --system --group --home-dir /var/lib/y-sync --shell /usr/sbin/nologin y-sync
mkdir -p /var/lib/y-sync/backups /opt/y-sync/releases /opt/y-sync/incoming /var/www/html
chown -R y-sync:y-sync /var/lib/y-sync
# /opt/y-sync 归部署用户（后续 deploy 无需 sudo 即可上传/切换），service 用户只读执行
chown -R "$(SUDO_USER:-ubuntu):$(SUDO_USER:-ubuntu)" /opt/y-sync

# 2. 首个二进制（可选：bootstrap 时直接带入）
if [ -n "$BIN" ] && [ -f "$BIN" ]; then
  V=bootstrap
  mkdir -p "/opt/y-sync/releases/$V"
  install -m 0755 "$BIN" "/opt/y-sync/releases/$V/y-sync-server-rs"
  chown y-sync:y-sync "/opt/y-sync/releases/$V/y-sync-server-rs"
  ln -sfn "releases/$V" /opt/y-sync/current
  echo "已安装初始二进制: releases/$V"
fi

# 3. systemd 单元
install -m 0644 y-sync-server.service /etc/systemd/system/y-sync-server.service
systemctl daemon-reload
systemctl enable y-sync-server.service
if [ -e /opt/y-sync/current/y-sync-server-rs ]; then
  systemctl restart y-sync-server
  echo "服务已启动（127.0.0.1:8720）"
else
  echo "注意: 尚无二进制，首次 deploy 后服务可用（已设置开机自启）"
fi

# 4. nginx 站点（使用你已有的证书）
CONF=/etc/nginx/sites-available/y-sync
sed -e "s|__DOMAIN__|${DOMAIN}|g" -e "s|__CERT__|${CERT}|g" -e "s|__KEY__|${KEY}|g" \
  nginx-y-sync.conf.example > "$CONF"
ln -sfn "$CONF" /etc/nginx/sites-enabled/y-sync
if nginx -t; then
  systemctl reload nginx
  echo "nginx 站点已启用: https://${DOMAIN}"
else
  echo "nginx 配置测试失败——请检查证书路径后手动执行 nginx -t" >&2
  exit 1
fi

echo "== 初始化完成 =="
echo "下一步（服务器上执行一次）: sudo -u y-sync env YSYNC_DATA=/var/lib/y-sync /opt/y-sync/current/y-sync-server-rs adduser <用户名>"
