#!/bin/sh
# y-sync 一键安装（P0-4）：从 GitHub Release 下载对应平台的 Rust 实现并安装。
# 用法:
#   curl -fsSL https://raw.githubusercontent.com/chengruo/y-sync/main/scripts/install.sh | sh
#   或: ./install.sh [client|server|both]      # 默认 both
set -eu

REPO="chengruo/y-sync"
MODE="${1:-both}"
PREFIX="${PREFIX:-/usr/local}"

OS="$(uname -s)"
ARCH="$(uname -m)"
case "$ARCH" in
  x86_64|amd64) A="amd64" ;;
  aarch64|arm64) A="arm64" ;;
  *) echo "不支持的架构: $ARCH"; exit 1 ;;
esac

echo "==> 查询最新版本..."
TAG="$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" \
  | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p')"
[ -n "$TAG" ] || { echo "无法获取最新版本号"; exit 1; }
echo "    最新版本: $TAG"

fetch() { # fetch <asset名> <输出路径>
  echo "    下载 $1"
  curl -fsSL "https://github.com/$REPO/releases/download/$TAG/$1" -o "$2"
}

install_one() { # install_one <asset名> <目标名>
  TMP="$(mktemp)"
  fetch "$1" "$TMP"
  chmod +x "$TMP"
  # 兜底：curl 下载本身不会被 Gatekeeper 隔离，但复用浏览器下载的缓存/路径时会带上
  xattr -d com.apple.quarantine "$TMP" 2>/dev/null || true
  mkdir_bin
  if [ -w "$PREFIX/bin" ]; then
    mv "$TMP" "$PREFIX/bin/$2"
  else
    echo "    需要 sudo 写入 $PREFIX/bin"
    sudo mv "$TMP" "$PREFIX/bin/$2"
  fi
  echo "    已安装: $PREFIX/bin/$2"
}

mkdir_bin() {
  mkdir -p "$PREFIX/bin" 2>/dev/null || sudo mkdir -p "$PREFIX/bin"
}

case "$MODE" in
  client|both)
    [ "$OS" = "Linux" ] && ASSET="ysyncd-linux-$A"
    [ "$OS" = "Darwin" ] && ASSET="ysyncd-darwin-$A"
    [ -n "${ASSET:-}" ] || { echo "客户端暂不支持 $OS（可用桌面安装包或源码构建）"; }
    [ -n "${ASSET:-}" ] && { install_one "$ASSET" "ysync"; }
    ;;
esac
case "$MODE" in
  server|both)
    [ "$OS" = "Linux" ] || { echo "服务端仅支持 Linux（跳过 server）"; }
    [ "$OS" = "Linux" ] && { install_one "ysync-server-rs-linux-$A" "y-sync-server-rs"; }
    ;;
esac

echo ""
echo "== 安装完成 =="
cat <<'NEXT'

下一步:
  客户端:  ysync init -server https://<你的服务地址> -user <用户名>
           ysync add <本地目录> && ysync daemon
  服务端:  sudo y-sync-server-rs serve -addr 127.0.0.1:8720 -data /var/lib/y-sync
           （配合 deploy/ 下的 systemd 与 nginx 模板；首个用户:
            sudo YSYNC_DATA=/var/lib/y-sync y-sync-server-rs adduser <名字>）
NEXT
