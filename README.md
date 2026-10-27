<p align="center">
  <img src="logos/export/logo-192.png" alt="y-sync logo" width="96" height="96" />
</p>

# y-sync

轻量级文件同步服务（类 Nextcloud 场景的核心 10% 能力）：client-server 架构，
单二进制服务端 + SQLite + 内容寻址 blob 存储，专为"散落在不同文件夹下的项目"设计。

需求与设计文档见 [REQUIREMENTS.md](REQUIREMENTS.md)。当前实现覆盖 **M1–M4 全部里程碑**。

---

## 功能总览

### 同步核心（M1/M2）

- **双向文件同步**：变更日志 + 游标增量同步，多文件夹独立游标（FR-S13/S14），互不干扰
- **移动与重命名**：以**移动语义**传播（稳定 node_id），不重传内容（FR-S6）
- **冲突处理**（FR-S7）：双向同时修改保留双方版本，输的一方落为 `name (conflict from 设备名).ext`，不静默覆盖
- **服务端回收站**（FR-S5/V2）：删除先入回收站，支持浏览、原位置恢复或彻底清理，默认 30 天自动清理（可配）
- **文件历史快照**（FR-V1/V3）：覆盖前自动保存旧版本，每文件保留最近 N 版（默认 10，可配），支持一键回滚
- **CDC 增量上传 + 断点续传**（FR-S11/P1-8）：大文件（≥100MB）按内容定义分块（gear 滚动哈希，min 256KB / avg 1MB / max 4MB），修改只重传变化块；清单哈希注册，服务端 GET 透明重组装（含 Range 支持）；块 blob 服务端去重；中断凭清单补传
- **.syncignore 机制**（FR-S8/S17）：gitignore 兼容子集；支持子目录嵌套逐层覆盖；`use-gitignore` 选项沿用已有 `.gitignore`；默认忽略 `.git/`、`.y-sync/` 等
- **崩溃安全恢复**（M2 验收）：元数据写入前记录恢复标记，崩溃（kill -9）重启后自动重建本地状态并全量对账；服务端 SQLite 事务 + 临时文件原子改名
- **元数据保留**：保留 mtime（FR-S10）；SHA-256 全局内容寻址 + 去重（相同内容全服只存一份）

### 原生交互与控制中心（M3）

- **macOS HIG 原生风格管理台**：默认监听 `127.0.0.1:8730`（随机 Token 认证，加载后地址栏自动洗净）。
  - **视觉体验**：遵循 macOS 人机界面指南，采用 `NavigationSplitView` 双栏布局、卡片流、系统 SF Pro 字体栈、纯 CSS 离线渲染并自动自适应明亮/暗黑模式。
  - **原生桌面交互**：支持“在访达中显示”快捷定位本地文件（`open -R`）、调用 macOS 原生系统文件夹选择器浏览接入目录、以及拖拽目录快捷接入。
  - **冲突可视化解决**：卡片式横向对比本机与云端版本大小、时间，一键决定保留本机或采用副本。
  - **历史版本时间轴**：类似 Time Machine 的覆盖历史节点树，单键回滚任意快照。
  - **凭据失效智能感知**：遇到 401 认证过期自动呼出横幅与重新登录窗口，凭据更新时自动保留已关联的全部目录。
- **FS 事件监听**：客户端采用 fsnotify 监听本地文件系统变更（2s 防抖聚合），5 分钟定时全量对账兜底。
- **WebSocket 准实时通知**（§4.2）：服务端推送变更日志游标事件，客户端准实时拉取增量，网络断开自动退化为短轮询。
- **选择性同步**（FR-S9）：支持 `ysync add --exclude <子路径>` 忽略特定子树（如 `node_modules`、`target`）。
- **带宽与分时限速**（FR-S12）：基于 Token Bucket 的上传/下载限速，支持分时段计划（如 `"9-18:512,0-7:10240"`）。
- **开机自启守护**：`ysync install` 一键注册为 macOS launchd 或 Linux systemd 用户服务。
- **一致性备份**（SR5）：`y-sync-server backup -out DIR` 输出 SQLite VACUUM 快照 + 完整 blob 存储。

### 扩展能力（M4）

- **安全分享链接**（FR-H1）：`ysync share <folder> <path> [-hours N] [-password pw]` 生成对外只读链接，支持有效期与密码保护，可在控制台随时撤销。
- **只读 WebDAV**：挂载在 `/dav/`，Basic Auth 复用用户密码，macOS Finder、Windows 资源管理器、NAS 可直接只读挂载。
- **Web 只读浏览页**：凭设备 Token 直开逐层浏览与下载文件。

---

## 安装指南

### 1. macOS 快速安装

#### 方式 A：官方脚本一键安装与升级（推荐）
自动识别系统架构（Apple Silicon / Intel），从 GitHub Release 获取最新构建并覆盖安装，已有数据与配置完整保留：
```bash
# 一键安装/升级客户端命令行（若本地已有 /Applications/y-sync.app 也会自动联动更新桌面端）
curl -fsSL https://raw.githubusercontent.com/chengruo/y-sync/main/scripts/install.sh | bash -s client

# 仅安装/更新 macOS 桌面端应用（/Applications/y-sync.app）
curl -fsSL https://raw.githubusercontent.com/chengruo/y-sync/main/scripts/install.sh | bash -s desktop
```
> **自动优化特性**：
> - **路径自适应**：在 Apple Silicon Mac 上自动优先识别 `/opt/homebrew/bin`（存在时），解决 `/usr/local/bin` 被 PATH 优先级遮蔽的问题。
> - **桌面应用智能联动**：检测到系统中存在 `/Applications/y-sync.app` 时，会自动一并下载最新 DMG 替换，无需手动拖拽。
> - **无缝生效**：自动退出内存中的旧进程并自动移除 Gatekeeper 隔离标记，更新后立即生效。

#### 方式 B：从源码编译安装（包含最新 macOS 原生控制中心桌面应用 + CLI）
在仓库根目录直接执行：
```bash
# 1. 编译 Release
cargo build --release -p ysyncd -p ysync-desktop

# 2. 安装命令行到系统 PATH（自动兼容 Homebrew / usr/local）
BIN_DIR="$(brew --prefix 2>/dev/null)/bin" || BIN_DIR="/usr/local/bin"
sudo cp target/release/ysyncd "$BIN_DIR/ysync"
sudo cp target/release/ysyncd "$BIN_DIR/ysyncd"

# 3. 安装 macOS 桌面端应用到「访达」应用程序目录
sudo mkdir -p /Applications/y-sync.app/Contents/MacOS
sudo cp target/release/ysync-desktop /Applications/y-sync.app/Contents/MacOS/
sudo cp target/release/ysyncd /Applications/y-sync.app/Contents/MacOS/
sudo xattr -rd com.apple.quarantine /Applications/y-sync.app 2>/dev/null || true
```

#### 方式 C：macOS 首次打开提示“已损坏，无法打开”解决
桌面应用未加入付费 Apple 开发者公证时，从浏览器下载或第三方构建会被 Gatekeeper 拦截。在终端运行一次放行命令即可：
```bash
sudo xattr -rd com.apple.quarantine /Applications/y-sync.app
```

#### 方式 D：配置开机后台静默同步
```bash
ysync install
```
*(会自动在 `~/Library/LaunchAgents/` 下注册 launchd 守护并在登录时静默运行)*

---

### 2. Linux / 其他系统一键安装

```bash
# 安装客户端 + 服务端到 /usr/local/bin
curl -fsSL https://raw.githubusercontent.com/chengruo/y-sync/main/scripts/install.sh | sh

# 仅安装客户端
curl -fsSL https://raw.githubusercontent.com/chengruo/y-sync/main/scripts/install.sh | sh -s client

# 仅安装服务端
curl -fsSL https://raw.githubusercontent.com/chengruo/y-sync/main/scripts/install.sh | sh -s server
```

---

## 快速开始

桌面端首次登录后会自动打开“选择服务器上的项目”。勾选当前账号下要同步的项目，
选择本地父目录并确认，即可将项目同步到预览的目录；之后也可从顶部“从服务器选择”继续添加。
已有本地文件会参与双向合并，内容冲突时保留冲突副本。已接入项目可在主界面暂停或移除；
移除仅解除跟踪，保留本地和远端文件。第一版支持整个顶层项目，不提供项目内子目录勾选。

新版服务端通过项目列表和 `root` 范围分页避免下载其他项目的元数据；旧服务端仍可使用，
客户端会退回全账号节点列表并自行过滤。升级服务端后即可使用按项目查询。

```bash
# -----------------------------
# 1. 服务端（单二进制即可运行）
# -----------------------------
# 启动本地服务（默认 8720 端口，数据存放于 ./y-sync-data）
./bin/y-sync-server-rs serve -addr 127.0.0.1:8720 -data ./y-sync-data

# 创建第一个同步账号（交互输入密码）
./bin/y-sync-server-rs adduser alice

# ⚠️ 若在生产服务器通过 systemd 运行（以 y-sync 身份隔离），管理命令需保持相同权限：
sudo -u y-sync YSYNC_DATA=/var/lib/y-sync y-sync-server-rs adduser alice


# -----------------------------
# 2. 客户端 A：登录并接入项目目录
# -----------------------------
# 客户端初始化登录
ysync init -server https://sync.example.com -user alice

# 接入想要同步的本地项目
ysync add ~/code/my-project --use-gitignore --exclude node_modules

# 启动后台同步守护（或运行 ysync sync 进行单次同步）
ysync daemon


# -----------------------------
# 3. 客户端 B（另一台设备）
# -----------------------------
ysync init -server https://sync.example.com -user alice
ysync add ~/work/my-project --as my-project
ysync install      # 设为开机自启


# -----------------------------
# 4. 日常操作
# -----------------------------
ysync status                               # 查看各文件夹同步状态
ysync update                               # 检查并一键自动升级客户端（支持 --check 仅检查）
open http://127.0.0.1:8730/                # 打开 macOS 原生控制台（或直接启动 y-sync.app）
ysync trash list                           # 查看服务端回收站
ysync trash restore 3                      # 恢复已删除的项目
ysync versions list my-project src/main.rs # 查看指定文件的历史覆盖版本
ysync share my-project docs -hours 72      # 生成对外分享链接
```

> 💡 **客户端自动更新**：守护进程在启动后检查版本，成功检查后每 24 小时再次检查 GitHub Release；桌面端与管理台共用检查结果。可从托盘或管理台“检查更新”查看版本、更新说明和下载进度，也可关闭自动检查或跳过某个版本。点击“下载并重新启动”后，后台任务校验 SHA256，等待当前同步结束，再安装并重新启动 macOS 应用及随包守护进程；新守护进程健康检查失败时恢复旧版本。Unix CLI 守护也支持自动重新启动，Windows CLI 安装后需手动重新启动。配置和同步数据保留。
>
> 发布流程会生成 `SHA256SUMS` 并为桌面壳和 sidecar 注入同一版本号。更新仅安装更高版本和匹配架构的包；缺少有效校验信息的旧发布包需通过发布页手动安装。更新备份和 `update.log` 可用于排障。

---

## 服务端运维与安全机制

### 1. 忘记密码与密码重置

出于安全性考虑，`y-sync` **不在公网暴露任何“忘记密码/邮件重置”接口**，以杜绝线上撞库与邮箱劫持风险。

如管理员或用户遗忘密码，需通过 SSH 登录服务器主机，直接在服务端命令行执行密码重置：

```bash
# 本地或直接部署环境：
y-sync-server-rs passwd <用户名>

# 生产环境服务隔离部署（以运行服务的账号执行）：
sudo -u y-sync YSYNC_DATA=/var/lib/y-sync y-sync-server-rs passwd <用户名>
```
根据提示输入两遍新密码即可完成重置。

### 2. 客户端 401 凭据失效处理

服务端一旦通过 `passwd` 修改了密码，该用户所有已发放的设备 Token 会**立即全局吊销**。
此时各客户端同步日志会收到 `HTTP 401 Unauthorized`，Web 控制台也会弹出橙色告警横幅。

**处理方式**：
- 在控制台横幅中点击 **“立即重新登录”**，输入服务端新密码即可完成认证。
- 或在客户端终端执行：
  ```bash
  ysync init -server https://sync.example.com -user <用户名>
  ```
- **配置安全保证**：重新认证只会刷新 Token，**原有已配置并跟踪的全部同步目录会自动完整保留**，无需重新添加。

### 3. 登录防爆破与频控

- 按 `IP + 用户名` 记录失败次数，连续 5 次错误密码即触发指数退避锁定（60s 起，上限 12h），锁定期间正确凭证也会返回 429。成功登录后计数自动清零。
- 分享链接的提取密码同样具备防爆破机制。

### 4. 设备管理与吊销

- 服务端为每个接入设备分配独立 Token（服务端仅存 SHA-256 哈希）。
- 随时可通过控制台查看已授权设备列表，或在终端执行 `ysync devices` / `ysync revoke <id>` 立即踢出遗失或废弃的设备。

### 5. 存储配额与审计

- **存储配额**：通过 `y-sync-server-rs adduser <name> --quota <字节数>` 指定配额上限（0 表示不限），上传超出配额时给出明确拒绝。
- **可观测性**：`GET /metrics` 输出 Prometheus 格式的设备数、文件数、Blob 用量及请求统计（仅 loopback 可达）。
- **审计日志**：自动记录 `audit.log`（JSONL 格式），记录用户登录、设备授权及元数据操作历史。

---

## 多客户端 × 多文件夹同步原理

一台机器可同时接入 N 个独立项目目录（`ysync add`），同一项目可被多台客户端并发同步：

```
                ┌── 客户端 A（MacBook）                   ┌── 客户端 B（Workstation）
 ~/code/api  ───┤ folder "api"    cursor=120              ─┤ folder "api"    cursor=118
 ~/notes     ───┤ folder "notes"  cursor=95   ── 服务端 ── ─┤ folder "notes"  cursor=120
                 └──────────── y-sync-server ──────────────┘
                       （每用户一棵节点树 + 全局变更日志）
```

- **服务端为单一事实源**：每用户独立一棵稳定节点树（`node_id` 保持不变，路径作为可变属性），搭配单调递增的 `changes` 变更日志。
- **每文件夹独立游标**：客户端针对每个跟踪的文件夹维护独立的日志游标，各目录进度互不影响。
- **增量拉取与应用**：客户端凭 `(cursor, root)` 获取增量日志，自动执行重命名、内容下载与冲突分支。
- **两阶段原子上行**：客户端先上传内容 Blob（`PUT /content`，服务端 SHA-256 去重），再原子提交元数据变动（`POST /ops`）。
- **自动收敛保障**：游标落后于服务端清理水位线时自动触发全量对账；网络中断或进程强制退出（kill -9）重启后自动恢复。

---

## 自动化生产部署

Rust 服务端支持一条命令部署到 Linux 云服务器（Nginx 反向代理 + TLS 证书）：

```bash
# 1. 首次初始化：在服务器配置服务运行用户、数据目录、systemd 与 nginx 站点
DEPLOY_HOST=124.x.x.x BIN=bin/y-sync-server-rs-linux-amd64 \
  bash scripts/deploy.sh bootstrap sync.example.com /etc/nginx/ssl/fullchain.pem /etc/nginx/ssl/privkey.pem

# 2. 日常无缝更新：自动备份数据 → 上传新版本 → 原子切换 → 健康检查 → 异常回滚
DEPLOY_HOST=124.x.x.x bash scripts/deploy.sh
```

---

## 测试与验证

```bash
cargo test                   # 运行单元测试与属性测试（100% Rust 实现）
bash scripts/e2e.sh          # 76 项端到端断言测试（同步/冲突/移动/回收站/版本/分块/崩溃恢复）
bash scripts/e2e-stress.sh   # 30 项高压稳定性测试（真实 kill -9、WS 频繁断连、百级并发互斥）
bash scripts/e2e-features.sh # 14 项安全特性测试（设备吊销、防爆破退避、配额校验、审计日志）
python3 scripts/e2e-updater.py # 隔离验证更新接口、重启与失败回滚（先构建 Rust debug 客户端）
python3 scripts/e2e-project-selection.py # 远端项目选择、账号隔离、独立同步和双向合并（先构建 Rust debug 二进制）
bash scripts/chaos.sh 8       # 混沌工程长跑（随机文件变动 + 随机击杀，逐字节校验最终一致性）
bash scripts/bench.sh 100000 # 规模基准压测（10 万文件索引与传播性能）
```

协议跨语言契约细节见 [docs/PROTOCOL.md](docs/PROTOCOL.md)。

---

## 架构组成

```
rust/ysync-core       核心协议定义、配置管理与控制端 Client
rust/ysyncd           Rust 客户端（同步引擎、CDC 分块、监听器、WS 客户端、macOS HIG 控制台）
rust/ysync-server-rs  Rust 服务端（SQLite 存储引擎、Blob 去重存储、回收站、历史版本、WebDAV、WS 推送）
desktop/src-tauri     macOS / Linux / Windows 原生桌面外壳（菜单栏托盘、视窗控制、系统集成）
```

---

## 已知限制

- 分块上传会话在服务端重启后失效（客户端会自动重建会话重传，内容去重降低网络代价）。
- 冲突副本命名中的设备名为“解决冲突的设备”。
- 大文件 SHA-256 全量哈希暂无缓存复用（仅比对本地状态库中记录的 mtime/size/哈希）。
- Linux 下 WebDAV 建议以只读方式挂载。

