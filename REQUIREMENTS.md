# y-sync 需求设计（轻量级文件同步服务）

> 定位：一个"只做好文件同步"的自托管服务，客户端-服务端架构，目标是把 Nextcloud 里真正被大多数人使用的 10% 功能做到足够可靠，同时把部署和运维成本压缩到"一个二进制 + 一个数据目录"。

## 1. 目标与非目标

### 1.1 目标

| # | 目标 | 衡量方式 |
|---|------|----------|
| G1 | 可靠的双向文件同步（多设备、多文件夹） | 移动/重命名/删除正确传播；冲突不丢数据 |
| G2 | 极轻量：单二进制服务端，SQLite 存储，无外部依赖 | 服务端空闲 RSS ≤ 50MB；一条命令启动 |
| G3 | 部署运维成本趋近于零 | 5 分钟内完成"下载 → 启动 → 客户端连上并同步" |
| G4 | 数据自持：存储格式透明、可备份、可迁移 | 数据目录可直接 rsync 备份；提供一致性备份命令 |
| G5 | 协议开放：API 有文档，第三方可写客户端 | v1 协议文档 + 冒烟测试脚本 |

### 1.2 非目标（轻量的来源，与 Nextcloud 的差异）

明确**不做**以下内容，这是整个项目"轻量"的根本保障：

- ❌ Web 应用生态（日历/联系人/相册/在线办公/Talk 等）
- ❌ 插件/应用市场系统
- ❌ 大而全的 Web 管理后台（用 CLI + 配置文件代替）
- ❌ 细粒度 ACL / 群组权限体系（MVP 阶段每用户一个私有空间）
- ❌ 端到端加密（协议复杂度指数级上升，且与版本/回收站/去重冲突；如确有需求未来以"加密文件夹"独立立项）
- ❌ P2P 同步（那是 Syncthing 的定位；本项目是中心化 server-client）
- ❌ git 的 commit/branch/staging 模型（同步应自动、持续发生，不要求用户"提交"；版本按文件自动保留，详见 §9）
- ❌ VFS 占位文件（按需下载，云端占位）——依赖各平台云文件 API（macOS File Provider / Windows Cloud Filter），复杂度极高，推迟到远期评估
- ⏸ WebDAV 兼容层（有价值，作为 M4 只读兼容层，不作为核心协议）
- ⏸ Web 浏览/上传页面（M4 提供极简只读页面，可选）

## 2. 核心概念

| 概念 | 定义 |
|------|------|
| **Node** | 服务端文件树中的一个节点（文件或目录），拥有全局唯一、跨移动/重命名稳定的 64 位 `node_id`。**路径只是树的属性，不是身份**——这是移动/重命名能正确传播的关键 |
| **Change Journal（变更日志）** | 服务端把每次元数据变更追加写入一张日志表，每条有一个单调递增 `cursor`。客户端凭"我同步到 cursor=N"做增量拉取，无需全量比对 |
| **Blob（内容块）** | 文件内容按 SHA-256 内容寻址存储。相同内容只存一份（天然去重），版本管理复用同一存储 |
| **Device** | 一个客户端实例 = 一个设备，持有一个独立 token，各自维护自己的同步 cursor |
| **Conflict（冲突副本）** | 同一文件在服务端和本地都被修改且无法自动合并时，保留两个版本，输的一方重命名保留 |

## 3. 功能需求

### 3.1 账户与设备（M1）

- FR-A1 服务端支持多用户；每个用户一个根目录空间（MVP 不做共享空间）。
- FR-A2 用户管理通过服务端 CLI 完成：`y-sync adduser / passwd / rmuser / list`。无 Web 管理界面。
- FR-A3 客户端用 用户名+密码 登录一次，换取**每设备独立 token**；token 服务端只存哈希，可单独吊销（丢一台设备不影响其他设备）。
- FR-A4 密码使用 Argon2id 哈希存储。
- FR-A5 TLS 由反向代理（推荐 Caddy）终结，服务端自身监听明文 HTTP（仅监听 127.0.0.1 或内网时可直接裸跑）。

### 3.2 文件同步（核心，M1–M2）

- FR-S1 **初次同步**：客户端登录后拉取服务端全量元数据树，下载全部文件（支持选择性同步排除）；或本地上传到空服务端。
- FR-S2 **增量同步**：客户端持有 cursor，通过变更日志 API 拉取 `cursor` 之后的所有变更（新增/修改/删除/移动/重命名），逐条应用到本地。
- FR-S3 **上行**：客户端监视本地文件系统事件 + 定期全量对账（reconcile，防止事件丢失），将本地变更提交到服务端。
- FR-S4 **两阶段提交**：先 `PUT` 文件内容（按 SHA-256 寻址，服务端命中去重则直接返回），再通过批量元数据操作接口提交"哪个目录、叫什么名字、mtime 是多少"。保证文件树元数据操作原子化。
- FR-S5 **删除**：服务端删除进入回收站（保留期可配置，默认 30 天），变更日志写 tombstone，所有设备同步删除。
- FR-S6 **移动/重命名**：因 node_id 稳定，移动/重命名在设备间以"移动"语义传播，不产生"删除+重传"。
- FR-S7 **冲突处理**：客户端记录每个文件上次同步时的基线版本；若发现"服务端版本 ≠ 基线 且 本地也有改动"，保留双方：一方重命名为 `name (conflict from 设备名).ext`。默认不做 last-writer-wins（避免静默丢数据）。
- FR-S8 **忽略规则（gitignore 完全兼容）**：`.syncignore` 采用与 `.gitignore` 完全一致的匹配语义——支持子目录级 `.syncignore` 逐层覆盖、`!` 取反、`**`、`/` 锚定、结尾 `/` 仅匹配目录；另有客户端配置级全局排除。提供 `use-gitignore` 选项：对本身就是 git 仓库的项目，直接沿用其 `.gitignore`（叠加在 `.syncignore` 规则之后）。忽略判定双边一致：被忽略路径本端既不上传也不下载；已同步文件因新规则变为忽略时，本地解除跟踪，服务端与其他设备不受影响（不删除）。
- FR-S9 **选择性同步**：客户端可勾选同步服务端空间的哪些子目录。
- FR-S10 **mtime 保留**：文件的修改时间作为一等公民同步（下载后回设 mtime）。
- FR-S11 **大文件与断点续传**：超过阈值（默认 100MB）的文件分块上传（默认 8MB/块），中断后可从已传块续传；下载支持 HTTP Range。
- FR-S12 **带宽限制**：客户端可配置上/下行限速（全天或分时段）。

**多文件夹同步（散落的项目）**

- FR-S13 **多文件夹**：客户端支持同时同步任意多个本地目录（如 `~/work/api`、`~/notes`、`~/side/nav`），每个文件夹映射到用户空间中的一个子树（通常是顶层目录）。服务端仍是每用户一棵树（FR-A1）——多文件夹是客户端视角的拆分，不是服务端的多仓库。
- FR-S14 **文件夹级隔离**：每个同步文件夹拥有独立的本地状态库与同步 cursor（changes API 支持按子树过滤，见 §4.2），可单独暂停/恢复/移除；单个文件夹状态损坏不影响其他文件夹。
- FR-S15 **接入新项目**：`ysync add ~/code/foo [--as bar]` 一条命令把一个散落目录接入同步（服务端子树已存在则下拉、不存在则上传）；`ysync remove` 解除跟踪（可选保留服务端副本）。校验：文件夹之间不得嵌套或重叠；符号链接目录默认拒绝并提示。
- FR-S16 **跨文件夹移动**：因内容按哈希寻址，同一客户端把文件从一个同步文件夹移到另一个，只产生元数据操作（原子树 unlink + 新子树 put 复用已有 blob），内容不重传。
- FR-S17 **默认忽略清单**：所有文件夹默认忽略 `.git/`、`.svn/`、`.hg/`、`.y-sync/` 及常见临时文件（`*.tmp`、`*~`、`.DS_Store`、Office 锁文件），可在配置中增删。同步 `.git/` 会造成锁冲突与体积膨胀，必须默认排除。

### 3.3 版本与回收站（M2）

- FR-V1 文件每次内容被覆盖，旧内容自动保存为版本；默认每文件保留最近 10 个版本（可配置），超出的按时间梯度清理。
- FR-V2 回收站：可列出、恢复、彻底删除；保留期默认 30 天，到期自动清理。
- FR-V3 版本与回收站均通过同一套 blob 存储实现，无额外存储引擎。

### 3.4 分享（M4，可选模块）

- FR-H1 只读分享链接：对单个文件或目录生成带随机 token 的 URL，可设过期时间和密码。
- FR-H2 写共享/用户间共享明确**不在**路线图内（如未来需要，作为独立模块评估）。

### 3.5 客户端形态

- FR-C1 **M1–M2：CLI 守护进程**。`ysync init / sync / status / pause`，配合配置文件，可注册为 systemd / launchd / Windows 服务。
- FR-C3 **M3：桌面托盘应用**。托盘图标（同步状态）、简单设置界面。实现方式：复用同一守护进程 + 轻量 GUI 壳，不重写同步逻辑。
- FR-C4 客户端本地维护 SQLite 状态库（服务端文件树快照 + 本地 mtime/size），使得对账不需要重新哈希全部文件。
- FR-C5 移动端不在 MVP 范围；协议设计需对弱网友好（增量 + 断点续传已覆盖主要诉求），M4 后评估。

## 4. 同步协议设计（v1 草案）

### 4.1 数据模型（SQLite）

```sql
users     (id, name, pass_hash, quota_bytes, created)
devices   (id, user_id, name, token_hash, last_seen_at)
nodes     (id, user_id, parent_id, name, type,          -- file | dir
           content_hash, size, mtime, created_at)       -- 目录仅 parent/name 有意义
blobs     (hash PK, size, refcount)                     -- 内容寻址，引用计数
changes   (cursor INTEGER PRIMARY KEY AUTOINCREMENT,    -- 全局变更日志
           user_id, node_id, op)                        -- op: put | unlink | move | mkdir
versions  (node_id, version_no, content_hash, size, mtime, created_at)
trash     (id, user_id, orig_parent, name, content_hash, size, mtime, deleted_at)
```

- 每用户一棵树，所有查询强制 `user_id` 作用域（隔离即安全）。
- 变更日志按 cursor 裁剪：最老设备的 cursor 之前的条目可删；落后超过保留窗口的设备强制触发全量重同步。

### 4.2 API 草案（JSON over HTTPS，文件内容为原始字节流）

```
POST /api/v1/auth/login            {user, password, device_name} → {token}
GET  /api/v1/sync/changes?cursor=&limit=&root=<node_id>
                                   → {cursor, changes:[{node_id, op, parent_id, name,
                                        type, size, mtime, content_hash}]}
                                   （root 可选：按子树过滤，支撑多文件夹各自独立 cursor）
PUT  /api/v1/content               body=文件字节; 请求头 X-Content-SHA256
                                   → {hash, dedup: bool}        （两阶段：先传内容）
GET  /api/v1/content/{hash}        支持 Range（两阶段：按哈希取内容）
POST /api/v1/ops                   批量元数据操作（原子应用）：
                                   [{op:"mkdir|put|move|unlink|setmtime",
                                     node_id?, parent_id, name, content_hash?, size?, mtime?}]
                                   → 每条操作的 {ok|error, new_cursor}
POST /api/v1/uploads               大文件分块会话：创建会话/上传块/完成
GET  /api/v1/nodes/{id}/versions   列出版本
GET  /api/v1/versions/{...}/content
GET  /api/v1/trash  / POST /api/v1/trash/{id}/restore  / DELETE /api/v1/trash/{id}
WS   /api/v1/notify                服务端变更推送（只推"有新 cursor"，客户端再拉 changes）
GET  /healthz /metrics             健康检查 / Prometheus 指标（可选开关）
```

- **通知机制**：WebSocket 推送变更提示实现"准实时"；客户端断线或未连接时退化为默认 60s（±随机抖动）轮询。
- **协议原则**：元数据（小 JSON）与内容（大字节流）彻底分离；所有操作幂等（以 node_id + content_hash 为幂等键），客户端崩溃重放安全。

### 4.3 客户端同步算法（概述）

```
循环（对每个同步文件夹独立执行，互不阻塞）:
  1. reconcile: 全量扫描本地树，与本地状态库比对 → 得出本地变更集
  2. 上行: 本地新增/修改 → PUT content + ops 提交
  3. 下行: GET changes?cursor=本地cursor → 逐条应用（下载内容、回设 mtime）
  4. 冲突检测: 应用下行时对比"基线版本"，按 FR-S7 处理
  5. 持久化新 cursor 与新基线
触发: 启动时 / FS 事件(防抖 2s) / 每 N 分钟兜底 / 收到 notify
```

### 4.4 客户端边界情况要求

- Windows：长路径（`\\?\` 前缀）、文件被占用（重试策略）、大小写不敏感冲突检测。
- macOS/Linux：NFC/NFD Unicode 规范化差异处理。
- 所有平台：符号链接默认不跟随（作为链接文本或忽略，可配置）；不同步权限/xattr（明确声明）。
- 写入采用"临时文件 + 原子 rename"，保证任意时刻断电不损坏已有文件。

## 5. 服务端需求

- SR1 单个静态编译二进制；子命令：`serve / adduser / passwd / list-users / backup / gc / version`。
- SR2 配置：单个 TOML 文件（监听地址、数据目录、保留策略、配额、限速），支持环境变量覆盖。
- SR3 存储布局：
  ```
  data/
    y-sync.db          # SQLite（WAL 模式），全部元数据
    blobs/ab/cd/ef...  # 内容寻址 blob，两层目录散列
    trash/             # 回收站（或直接复用 blobs + 引用）
  ```
- SR4 写入安全：blob 写临时文件后 rename；先写内容后提交元数据；refcount 与 blob 的不一致由 `gc` 子命令清理。
- SR5 备份：`y-sync backup` 输出一致性快照（SQLite backup API + 引用到的 blob 集合），数据目录本身可直接 rsync。
- SR6 优雅退出：处理 SIGTERM，完成进行中的写操作，未完成的上传会话可恢复。

## 6. 非功能需求

| 维度 | 指标 |
|------|------|
| 资源占用 | 服务端空闲 RSS ≤ 50MB；10 万文件规模下元数据常驻内存可控（走 SQLite，不整树载入内存） |
| 元数据性能 | changes API 单页 1000 条 < 50ms；10 万文件全量列举 < 2s |
| 吞吐 | 大文件传输能跑满 1Gbps 链路（瓶颈在磁盘/网络，不在进程） |
| 小文件 | 端到端 ≥ 50 文件/秒（SSD，单客户端顺序场景） |
| 并发 | 50 台设备常驻轮询 + notify，CPU 无明显尖峰 |
| 可靠性 | 任意时刻 kill -9 服务端/客户端，重启后自动收敛到一致状态，不丢已确认数据 |
| 安全 | TLS（反代）、Argon2id、per-device token 吊销、路径穿越防护、user_id 全量作用域隔离 |
| 可观测 | 结构化 JSON 日志（可配级别）、`/healthz`、同步审计日志（谁在何时改了什么） |

## 7. 技术选型建议

**推荐 Rust**（资源占用与单二进制分发最优）；若追求开发速度，Go 是合理替代。客户端与服务端同语言、同一仓库，核心同步逻辑沉淀为共享 crate/package：

```
y-sync/
  protocol/    # 协议类型 + API 定义（两端共享，单一事实来源）
  engine/      # 同步引擎：纯逻辑，FS/网络通过 trait 抽象，可单测
  server/      # axum(actix) + tokio + rusqlite
  client/      # CLI 守护进程：clap + notify(FS事件) + engine
  gui/         # (M3) 托盘壳，复用 client
  docs/        # 协议文档、本文件
```

- Rust 栈参考：axum + tokio + rusqlite(SQLite bundled) + notify + clap。
- Go 栈参考：net/http + mattn/go-sqlite3 + fsnotify + cobra；代价是二进制与内存略大，跨平台 GUI 壳选择更少。
- 判据：如果你写 Rust 熟练，选 Rust；如果这是第一个 Rust 项目、希望 M1 一个月内可用，选 Go。

## 8. 里程碑

| 里程碑 | 内容 | 验收标准 |
|--------|------|----------|
| **M0 协议定稿** | 本文件 4.x 节细化为完整协议文档；起一个协议一致性测试脚本（可先用 curl/python 实现假客户端） | 测试脚本能对任何符合协议的实现跑通基本流程 |
| **M1 最小闭环** | 服务端：auth + content 上传下载 + changes/ops；CLI 客户端：init/add/sync/status（多文件夹，按文件夹隔离状态与 cursor）；双向同步基本可用；冲突副本机制 | 两台"客户端"（可用两个目录模拟）互同步；断网重连后收敛；多个散落目录同时接入、独立同步 |
| **M2 可靠性** | 删除/移动/重命名传播、回收站、版本、gitignore 兼容的 .syncignore（含子目录嵌套与 use-gitignore）、分块续传、FS 事件 + 定时对账、mtime 保留 | 混合操作（改名+编辑+删除）在多端正确收敛；kill -9 不损坏数据 |
| **M3 体验** | 选择性同步、限速、WebSocket 通知、托盘 GUI、开机自启、`backup` 命令 | 非技术用户可在 5 分钟完成部署+装客户端 |
| **M4 扩展（可选）** | 只读分享链接、只读 WebDAV 兼容层、极简 Web 只读浏览页、移动端评估 | 按需裁剪 |

## 9. 与 git 的关系：借鉴什么，不借鉴什么

**结论：借 git 的思想，不用 git 做引擎。** "每个项目一个 bare git 仓库当存储"这类方案被评估并否决：仓库锁与并发写冲突、二进制反复修改导致仓库无限膨胀、packfile 格式不利于按需流式下载、无法实现按文件的版本保留策略。正确做法是自建内容寻址存储 + SQLite 元数据——而这本身就是 git 最值得借鉴的思想。具体取舍：

### 借鉴

| git 概念 | y-sync 中的对应 |
|----------|----------------|
| `.gitignore` 匹配语义 | `.syncignore` 完全兼容 gitignore 语法（嵌套逐层覆盖、`!` 取反、`**`、锚定），心智零成本，可直接用现成实现（Rust `ignore` crate 等）（FR-S8） |
| 内容寻址存储（objects） | blobs 按 SHA-256 存储 + 引用计数：天然去重，版本/回收站零额外引擎，跨文件夹移动不重传内容（§4.1、FR-S16） |
| `git gc` | `y-sync gc` 清理无引用 blob，同样的"先标记再清扫"思路（SR4） |
| 每仓库自包含 | 多文件夹模型：每个散落的项目是一个自包含的同步单元，独立状态、独立 cursor、可独立暂停（FR-S13–S15） |

### 不借鉴

| git 概念 | 不采用的理由 |
|----------|--------------|
| commit / branch / staging | 同步必须自动、持续发生，引入"提交"动作等于要求用户改变使用习惯；版本按文件自动保留（FR-V1）即可 |
| 重命名检测（内容启发式猜测） | git 在展示层靠相似度猜 rename；我们用稳定 node_id 结构化记录移动，比 git 更可靠（§2） |
| packfile + delta 压缩 | 对同步负载收益有限、实现复杂。升级路径：对大文件引入内容定义分块（CDC，restic/borg 路线），可后置到远期 |
| autocrlf / 行尾转换 | 同步工具永远原样传输字节，绝不做任何内容转换 |

## 10. 待确认问题（影响后续细化）

1. **单用户自用，还是多用户小团队？** 决定配额、分享、甚至是否可以砍掉多用户（单用户能再砍掉一半账户复杂度）。
2. **技术栈倾向 Rust 还是 Go？**（见 §7 判据）
3. **部署环境：公网 VPS 还是家庭内网/NAS？** 决定 TLS 与域名假设，以及是否需要考虑不可靠网络下的更激进重连策略。
4. **是否需要 Web 上传/浏览页面？** 若"手机临时取个文件"是高频场景，M4 的优先级应提前。
5. **预计规模？**（文件数、总量、设备数）10 万文件与 1000 文件的实现取舍不同（比如 changes 分页策略、索引设计）。

---

## 实现状态（2026-08-28 更新）

M1–M4 已全部实现并通过 53 项端到端断言（`scripts/e2e.sh`）+ 单元测试。细节见 README
"实现状态对照"。与本文档的有意偏差：

1. 技术栈：Go 与 Rust 双实现，协议等价由 e2e 差分矩阵保证（6 组合 × 76 断言）。
   客户端：Go（cmd/ysync）与 Rust（rust/ysyncd）均为完整实现；桌面壳 Tauri v2
   （desktop/src-tauri）。服务端：Go（cmd/y-sync-server）与 Rust（rust/ysync-server-rs）
   均为完整实现。协议类型两端镜像（internal/protocol ↔ rust/ysync-core/src/protocol.rs）。
   迁移策略：协议冻结 v1，任一实现的问题不阻塞另一端；e2e 即协议一致性测试。
   **2026-08-29 更新：Go 实现已冻结并最终移除**，Rust 为唯一实现；
   稳定性保障新增：跨进程同步互斥（.y-sync/sync.lock flock）、进程内 syncing 标记、
   mkdir 按深度分层批次、daemon 配置热重载（CLI add/remove 运行期生效）、
   压测脚本 scripts/e2e-stress.sh（真实 kill -9 / WS 重连 / Unicode 深路径 / 百文件并发）。
   **P0/P1 交付（2026-08-29）**：Rust 产物进 Release 流水线（server/client 多平台 +
   Tauri 桌面包 dmg/msi/deb）、一键安装脚本、登录防爆破（IP+用户指数锁定）、
   设备管理（列表/单台吊销）、/metrics（Prometheus）+ 审计日志（JSONL 轮转）、
   用户配额（增量强制）、混沌长跑测试 scripts/chaos.sh（随机操作+随机击杀，
   终态逐字节一致性）。限速器修复：分段发放避免"单次请求量 > 速率"的活锁。
2. M3 的"桌面托盘 GUI"以 daemon 本地控制 API + Web 状态页（127.0.0.1:8730）代替
   （§3.5 的"引擎常驻、GUI 是薄壳"架构不变；原生托盘壳留待 Tauri 评估）。
3. 版本保留策略实现了"每文件最近 N 版"（时间梯度清理未做）。
4. 分块上传会话不跨服务端重启持久化（客户端自动重建会话，代价由内容去重兜底）。
