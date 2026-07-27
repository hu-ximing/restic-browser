# `rustic_core` 迁移记录

## 决策状态

**当前已实现，发布验收尚未全部完成。** `rustic_core = 0.12.0` 和
`rustic_backend = 0.6.2` 已固定版本并成为默认本地仓库后端。原 restic 0.19.x CLI
后端保留，可通过 `--backend restic-cli` 显式选择；应用不会静默回退。

大型真实仓库的资源门槛和三平台 CI 实际运行结果仍是发布前待办。删除 CLI 后端、远程
backend 或目录缓存属于未来决策。

## 为什么迁移

CLI 后端的每次目录、搜索、预览和导出都会创建新进程、重新打开并解锁仓库。磁盘缓存能
减少部分 I/O，却不能跨进程保留解锁状态，因此未缓存目录常出现约 0.5–0.8 秒延迟。

`rustic_core` 在应用进程中持有同一个已解锁 Repository，使目录树读取复用会话，也避免
默认路径中通过子进程环境传递密码。迁移只替换读取实现，不增加备份或仓库管理能力。

## 项目身份

[`rustic_core`](https://github.com/rustic-rs/rustic_core) 是 `rustic-rs` 社区维护的
第三方纯 Rust restic 仓库实现：

- 它不是 restic 项目的官方 SDK；
- 它不是 Go restic 的绑定，也不嵌入 restic 可执行文件；
- 两者通过公开的 restic repository format 兼容，没有官方 API 兼容保证；
- 其 API 仍可能变化，因此当前依赖使用精确版本并提交 `Cargo.lock`；
- 库本身包含写能力，restic-browser 只通过自己的只读接口调用它。

## 保持不变的边界

- TUI 布局、键位、错误类别和一次输入密码的会话行为。
- `Snapshot`、`FileEntry`、`SearchResult`、`FileVersion` 等领域模型。
- 快照倒序、逐层浏览、当前快照搜索、预览和单文件无覆盖导出。
- 2 MiB 文本上限、512 MiB 媒体源上限和会话临时目录。
- ffmpeg/ffprobe 只处理本地临时文件。
- 不暴露或调用 init、backup、save、delete、repair、rewrite、copy 等写 API。
- Windows、Linux、macOS 的路径、Unicode、取消和终端恢复原则。

## 已实施的阶段

### 1. 兼容性探针

真实 restic 0.19.1 仓库测试已经验证：

- 打开格式 v1/v2 的本地仓库并一次解锁；
- 列快照、根目录和嵌套目录；
- 读取文件并以 SHA-256/逐字节结果对照；
- 中文、空格和 Emoji 路径；
- 错误密码分类；
- 操作前后仓库文件内容不变。

### 2. 只读后端抽象

`RepositoryReader` 统一四项领域能力：快照、目录、搜索、文件读取。`RusticClient` 和
`ResticCliClient` 均实现该接口，TUI、预览和文件导出不依赖具体后端类型。目录恢复不进入
该只读接口，始终由独立的 CLI 客户端执行。CLI 浏览后端继续作为行为对照和显式回退。

### 3. 完整读取流程

默认后端已接管浏览、搜索、文本/图片/视频帧预览和单文件导出。启动使用较小的
`to_indexed_ids()`；首次读取文件时一次升级为 `to_indexed()`，随后复用完整索引。
`rustic_core` 的同步调用在专用线程中串行运行，TUI 事件循环保持异步。

## 当前风险

| 风险 | 当前处理 |
| --- | --- |
| 第三方 API/格式兼容变化 | 精确固定 crate 版本和 lockfile；保留 CLI 对照 |
| 首次完整索引时间和内存 | 推迟到首次预览/导出；状态栏提示；仍需大型仓库测量 |
| 索引升级无法中途取消 | 升级前后检查取消；文档明确当前限制 |
| 搜索 pattern 语义差异 | 默认后端使用 glob 匹配路径/名称；CLI 回退用于精确行为对照 |
| 本地仓库根路径不是 UTF-8 | 返回明确错误；可显式选择 CLI 后端 |
| 同步 API 阻塞 | 单独工作线程串行执行，不阻塞终端事件循环 |
| crate 自带写 API | 应用只暴露 `RepositoryReader`；代码审查和仓库清单测试 |
| 跨平台差异 | 三平台 CI 矩阵、Unicode 夹具和 portable 路径/进程 API |

## 测试矩阵

| 维度 | 覆盖 |
| --- | --- |
| 仓库格式 | restic repository v1、v2 |
| 后端 | 默认 `rustic`、显式 `restic-cli` 对照 |
| 路径 | 中文、空格、Emoji、嵌套目录 |
| 内容 | 文本、PNG、短视频和单文件导出 |
| 错误 | 错误密码、预取消、已有目标、工具缺失的单元路径 |
| 只读 | 仓库操作前后递归内容摘要一致 |
| 平台 | Windows、Linux、macOS CI；真实媒体集成测试在依赖可用时运行 |

集成测试自行用 restic CLI 创建夹具，因此测试环境没有 restic 0.19.x 时会明确跳过；产品
默认运行不要求安装 restic。媒体闭环测试还要求 ffmpeg 和 ffprobe。

## 迁移完成条件

- 默认后端与 CLI 对照的快照、目录、Unicode 路径和文件内容一致。
- 浏览、搜索、三类预览和导出闭环通过自动化测试。
- 产品代码保持只读，操作前后仓库清单一致。
- 错误密码、取消和临时文件清理通过。
- Windows、Linux、macOS CI 实际通过，Windows x64 release artifact 可运行。
- 在代表性大型仓库上确认启动索引和首次完整索引的时间/内存可接受。
- 文档清楚说明第三方身份、显式 CLI 回退和已知限制。

前四项已由当前代码和本地测试覆盖；后三平台远程运行、Windows artifact 端到端和大型仓库
性能仍需在发布前确认。
