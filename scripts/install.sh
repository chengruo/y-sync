#!/bin/sh
# y-sync 一键安装与升级脚本：从 GitHub Release 下载对应平台的最新产物并安装。
# 用法:
#   curl -fsSL https://raw.githubusercontent.com/chengruo/y-sync/main/scripts/install.sh | bash
#   或: ./install.sh [client|server|both|desktop|all]      # 默认 both
set -eu

REPO="chengruo/y-sync"
MODE="${1:-both}"
OS="$(uname -s)"
ARCH="$(uname -m)"
case "$ARCH" in
  x86_64|amd64) A="amd64" ;;
  aarch64|arm64) A="arm64" ;;
  *) echo "不支持的架构: $ARCH"; exit 1 ;;
esac

# 路径自适应：若未显式指定 PREFIX，macOS 环境下若已存在 /opt/homebrew/bin（Apple Silicon 默认），优先安装到 /opt/homebrew
if [ -z "${PREFIX:-}" ]; then
  if [ "$OS" = "Darwin" ] && [ -d "/opt/homebrew/bin" ]; then
    PREFIX="/opt/homebrew"
  else
    PREFIX="/usr/local"
  fi
fi

echo "==> 查询最新版本..."
TAG="$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" \
  | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p')"
[ -n "$TAG" ] || { echo "无法获取最新版本号"; exit 1; }
echo "    最新版本: $TAG"

fetch() { # fetch <asset名> <输出路径>
  echo "    下载 $1"
  curl -fsSL "https://github.com/$REPO/releases/download/$TAG/$1" -o "$2"
}

mkdir_bin() {
  mkdir -p "$PREFIX/bin" 2>/dev/null || sudo mkdir -p "$PREFIX/bin"
}

install_one() { # install_one <asset名> <目标名>
  TMP="$(mktemp)"
  fetch "$1" "$TMP"
  chmod +x "$TMP"
  # 兜底：清除 Gatekeeper 隔离标记
  xattr -d com.apple.quarantine "$TMP" 2>/dev/null || true
  mkdir_bin
  if [ -w "$PREFIX/bin" ]; then
    mv "$TMP" "$PREFIX/bin/$2"
  else
    echo "    需要权限写入 $PREFIX/bin"
    sudo mv "$TMP" "$PREFIX/bin/$2"
  fi
  echo "    已安装命令行工具: $PREFIX/bin/$2"
}

install_macos_desktop() {
  if [ "$OS" != "Darwin" ]; then
    echo "    桌面安装包仅支持 macOS (当前: $OS)"
    return
  fi
  if [ "$A" != "arm64" ]; then
    echo "    当前 Release 提供 Apple Silicon (arm64) 桌面包，Intel 架构请使用命令行或源码构建"
    return
  fi
  echo "==> 下载并安装/更新 macOS 原生桌面端 (/Applications/y-sync.app)..."
  V="${TAG#v}"
  DMG="y-sync_${V}_aarch64.dmg"
  TMP_DMG="$(mktemp -t ysync_dmg).dmg"
  fetch "$DMG" "$TMP_DMG"
  MNT="/tmp/y-sync-mnt-$$"
  mkdir -p "$MNT"
  hdiutil attach "$TMP_DMG" -nobrowse -mountpoint "$MNT" >/dev/null
  # 停止运行中的桌面旧版
  killall y-sync ysync-desktop 2>/dev/null || true
  if [ -d "$MNT/y-sync.app" ]; then
    rm -rf /Applications/y-sync.app 2>/dev/null || sudo rm -rf /Applications/y-sync.app
    cp -R "$MNT/y-sync.app" /Applications/ 2>/dev/null || sudo cp -R "$MNT/y-sync.app" /Applications/
    xattr -rd com.apple.quarantine /Applications/y-sync.app 2>/dev/null || true
    echo "    已成功更新: /Applications/y-sync.app"
  fi
  hdiutil detach "$MNT" >/dev/null 2>&1 || true
  rm -rf "$MNT" "$TMP_DMG"
}

case "$MODE" in
  desktop|app)
    install_macos_desktop
    ;;
  client|both|all)
    [ "$OS" = "Linux" ] && ASSET="ysyncd-linux-$A"
    [ "$OS" = "Darwin" ] && ASSET="ysyncd-darwin-$A"
    [ -n "${ASSET:-}" ] || { echo "客户端暂不支持 $OS（可用桌面安装包或源码构建）"; }
    [ -n "${ASSET:-}" ] && { install_one "$ASSET" "ysync"; }
    # 智能探测：如果在 macOS 上已安装过 /Applications/y-sync.app 或指定了 all，自动联动更新桌面端
    if [ "$OS" = "Darwin" ] && { [ "$MODE" = "all" ] || [ -d "/Applications/y-sync.app" ]; }; then
      install_macos_desktop
    fi
    ;;
esac

case "$MODE" in
  server|both|all)
    [ "$OS" = "Linux" ] || { [ "$MODE" = "server" ] && echo "服务端仅支持 Linux（跳过 server）"; }
    [ "$OS" = "Linux" ] && { install_one "ysync-server-rs-linux-$A" "y-sync-server-rs"; }
    ;;
esac

# 自动终止旧进程，确保新版本立即被拉起加载
killall ysync ysyncd 2>/dev/null || true

echo ""
echo "== 安装/更新完成 ($TAG) =="
cat <<'NEXT'

下一步:
  客户端:  ysync init -server https://<你的服务地址> -user <用户名>
           ysync add <本地目录> && ysync daemon
  桌面端:  启动台或聚焦搜索打开「y-sync」
  服务端:  sudo y-sync-server-rs serve -addr 127.0.0.1:8720 -data /var/lib/y-sync
           （配合 deploy/ 下的 systemd 与 nginx 模板；首个用户:
            sudo YSYNC_DATA=/var/lib/y-sync y-sync-server-rs adduser <名字>）
NEXT
