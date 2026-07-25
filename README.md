# restic-browser

`restic-browser` 是一个只读 TUI，用来浏览本地 restic 仓库、搜索快照、预览文本、图片
和视频帧，并安全导出单个文件。Windows x64 是当前发布优先级；代码遵循 Windows、Linux、
macOS 的跨平台约束。

## 当前实现

- 后端：外部 restic 0.19.x CLI，只允许 `version`、`snapshots`、`ls`、`find`、`dump`。
- 预览：文本、常见图片、视频指定时间帧；ffmpeg 和 ffprobe 是外部依赖。
- 安全：一次运行输入一次密码；不持久化密码；导出单文件且拒绝覆盖。
- 范围：仅本地仓库，不提供备份、删除、修复或迁移仓库的功能。

运行当前版本需要预先安装 restic 0.19.x、ffmpeg 和 ffprobe，或通过参数指定它们的路径。

```powershell
restic-browser.exe -r D:\backup\restic-repo
```

也可通过 `RESTIC_REPOSITORY` 提供仓库，并使用 `--restic`、`--ffmpeg`、
`--ffprobe` 指定工具路径；`--log-file` 启用脱敏诊断日志。

## 计划中的迁移

项目已决定在独立阶段验证 `rustic_core`，以复用一次打开的仓库会话并改善目录加载延迟。
该迁移**尚未实现**；`rustic_core` 是第三方 Rust 实现，不是 restic 官方 SDK。验证期间
保留当前 CLI 后端作为对照和回退。

## 文档

- [v0.1 产品目标、流程、键位和验收](docs/product-v0.1.md)
- [当前架构、安全边界和已知目录延迟](docs/architecture.md)
- [`rustic_core` 分阶段迁移方案](docs/rustic-core-migration.md)

## 构建

```powershell
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --locked
cargo build --release --locked
```
