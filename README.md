# y-sync

轻量级文件同步服务（类 Nextcloud 场景的核心 10% 能力）：client-server 架构，
单二进制服务端 + SQLite + 内容寻址 blob 存储，专为"散落在不同文件夹下的项目"设计。

需求与设计文档见 [REQUIREMENTS.md](REQUIREMENTS.md)。当前实现覆盖 **M1–M4 全部里程碑**
（例外见下文"实现状态"）。

## 功能总览

### 同步核心（M1/M2）

- 双向文件同步：变更日志 + 游标增量同步，多文件夹独立游标（FR-S13/S14），互不干扰
- 移动/重命名以**移动语义**传播（稳定 node_id），不重传内容（FR-S6）
- 冲突处理（FR-S7）：双方同时修改时保留两个版本，输的一方保存为
  `name (conflict from 设备名).ext`，不静默覆盖
- **回收站**（FR-S5/V2）：删除先入回收站，可列出/恢复/彻底删除，30 天自动清理（可配）
- **文件版本**（FR-V1/V3）：覆盖前自动保存旧版本，每文件保留最近 N 版（默认 10，可配）
- **分块上传 + 断点续传**（FR-S11）：超过阈值（默认 100MB）自动分块（8MB/块），
  中断后凭会话续传；下载支持 Range
- **.syncignore**（FR-S8）：gitignore 兼容子集；支持**子目录嵌套**逐层覆盖、
  `use-gitignore` 选项沿用 `.gitignore`；默认忽略 `.git/` `.y-sync/` 等（FR-S17）
- **崩溃恢复**（M2 验收：kill -9 不损坏数据）：元数据提交前写恢复标记，
  崩溃后自动重建本地状态并全量对账；服务端事务 + 写临时文件后原子改名
- mtime 保留（FR-S10）；SHA-256 内容寻址 + 去重（相同内容只存一份）

### 体验（M3）

- **FS 事件监听**：daemon 用 fsnotify 监听文件系统（2s 防抖），5 分钟全量对账兜底
- **WebSocket 准实时通知**（§4.2）：服务端只推"有新 cursor"事件，客户端立即拉增量；
  断线自动退化为轮询
- **选择性同步**（FR-S9）：`ysync add --exclude <子树>`（可多次）
- **限速**（FR-S12）：配置 `upload_limit_kbs` / `download_limit_kbs`（token bucket）
- **daemon 控制 API + Web 管理台**：默认 `127.0.0.1:8730`（token 认证，每次启动随机
  生成），支持：查看各文件夹状态/冲突/错误、暂停/恢复、立即同步、**从网页接入/移除
  文件夹**（含 exclude/use-gitignore）、**冲突处理**（保留当前 / 采用副本，处理为纯
  文件操作并自动同步传播）。`ysync ui` 一键在浏览器打开（`-http off` 关闭）
- **开机自启**：`ysync install` 生成 launchd（macOS）/ systemd user unit（Linux）
- **backup**（SR5）：`y-sync-server backup -out DIR` 输出一致性快照（SQLite VACUUM INTO
  + 全部 blob），放回数据目录即可恢复

### 扩展（M4）

- **只读分享链接**（FR-H1）：`ysync share <folder> <path> [-hours N] [-password pw]`
  → `GET /s/<token>`；文件直接下载，目录输出列表页；可撤销
- **只读 WebDAV 兼容层**：挂载在 `/dav/`，Basic Auth 复用用户口令，
  Finder/资源管理器/RAID 浏览器可直接只读挂载
- **Web 只读浏览页**：`GET /browse?token=<设备token>` 浏览器直开逐层浏览与下载

### 安全

- 密码 Argon2id 存储；每设备独立 token（服务端只存哈希），`passwd` 重置密码时全部吊销
- TLS 建议由反向代理（Caddy/Nginx）终结；服务端默认仅监听 loopback
- 分享密码/过期时间；路径穿越防护；用户数据全量 user_id 作用域隔离

## 构建

```bash
go build -o bin/y-sync-server ./cmd/y-sync-server
go build -o bin/ysync ./cmd/ysync
```

## 快速开始

```bash
# 1. 启动服务端（可选 TOML：-config，支持 addr/data_dir/max_versions/trash_retention_days）
./bin/y-sync-server serve -addr 127.0.0.1:8720 -data ./y-sync-data

# 2. 创建用户（脚本场景用 YSYNC_DATA 指定数据目录）
YSYNC_DATA=./y-sync-data ./bin/y-sync-server adduser alice

# 3. 设备 A：登录并接入散落项目
./bin/ysync init -server http://127.0.0.1:8720 -user alice
./bin/ysync add ~/code/my-project --use-gitignore --exclude node_modules
./bin/ysync daemon          # 或 ./bin/ysync sync 单次同步

# 4. 设备 B（另一台机器）
./bin/ysync init -server http://server:8720 -user alice
./bin/ysync add ~/work/my-project --as my-project
./bin/ysync install         # 开机自启（launchd/systemd）

# 5. 日常操作
./bin/ysync status                        # 各文件夹状态
open http://127.0.0.1:8730/               # 状态页（冲突/错误可见、暂停/恢复）
./bin/ysync trash list && ./bin/ysync trash restore 3
./bin/ysync versions list my-project src/main.go && ./bin/ysync versions restore my-project src/main.go 12
./bin/ysync share my-project docs -hours 72 -password pw
./bin/ysync unshare <token>
./bin/y-sync-server backup -out ~/backups/2026-08-28   # 服务端
```

环境变量：`YSYNC_DATA`（服务端数据目录）、`YSYNC_CONFIG_DIR`（客户端配置目录，
默认 `~/.config/y-sync/`（macOS: `~/Library/Application Support/y-sync/`））。

## 架构

```
cmd/y-sync-server    服务端入口（serve/adduser/passwd/list-users/gc/backup）
cmd/ysync            客户端 CLI（init/add/sync/daemon/status/trash/versions/share/install）
internal/protocol    协议类型（两端共享，单一事实来源）
internal/server      store.go（SQLite 元数据 + 变更日志 + blob 引用计数）
                     versions.go（回收站/版本/恢复/GC）、blob.go（内容寻址存储）
                     upload.go（分块上传会话）、hub.go（WS 通知）
                     share.go（分享）、dav.go（WebDAV/浏览页）、http.go（API 层）
internal/client      engine.go（同步引擎：reconcile/冲突/移动语义/崩溃恢复）
                     state.go（本地状态库）、ignore.go（gitignore 兼容）
                     daemon.go（FS 监听/WS 订阅/控制 API/状态页）
                     rate.go（限速）、config.go、api.go
scripts/e2e.sh       端到端验证（53 项断言）
```

协议要点（详见 REQUIREMENTS.md §4）：元数据（JSON）与内容（字节流）分离；上传两阶段
（先内容后元数据，服务端按 SHA-256 去重）；服务端 `changes` 为全局变更日志，客户端凭
`(cursor, root)` 按子树增量拉取，journal 带设备归属避免自重放；路径不是身份，
节点以稳定 `node_id` 标识。

## 测试

```bash
go test ./...          # ignore 匹配器等单元测试
bash scripts/e2e.sh    # 76 项端到端断言（同步/冲突/移动/回收站/版本/分块/
                       # 崩溃恢复/嵌套 ignore/选择性同步/use-gitignore/WS 准实时/
                       # 管理台 API/冲突处理/暂停恢复/分享/WebDAV/backup）
```

e2e 提供 `wait_for` 轮询助手消除时序脆弱性；`E2E_KEEP=1` 保留现场目录供排查。

## 实现状态对照（REQUIREMENTS.md §8）

| 里程碑 | 状态 |
|--------|------|
| M0 协议定稿 | ✅ protocol 包 + e2e 脚本即协议一致性验证 |
| M1 最小闭环 | ✅ 双向同步/多文件夹/冲突副本/断连恢复 |
| M2 可靠性 | ✅ 移动传播/回收站/版本/嵌套 ignore/分块续传/FS 事件/崩溃恢复 |
| M3 体验 | ✅ 选择性同步/限速/WS 通知/自启/backup/Web 管理台（状态监控 + 接入文件夹/处理冲突/暂停恢复；原生托盘壳以 Web 管理台代替，Tauri 留待评估） |
| M4 扩展 | ✅ 分享链接/只读 WebDAV/浏览页（移动端评估：协议已具备弱网要素——增量+续传，原生 App 另行立项） |

## 已知限制

- 分块上传会话在服务端重启后失效（客户端自动重建会话重传，内容去重降低代价）
- 冲突副本命名中的设备名为"解决冲突的设备"（需要设备名解析 API 可升级为"内容来源设备"）
- 大文件 sha256 全量哈希无缓存复用（本地状态库仅记录 mtime/size/哈希，未哈希内容需重算）
- Linux 下 WebDAV 挂载建议只读使用；写入操作被服务端拒绝
