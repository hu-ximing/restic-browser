# restic-browser

`restic-browser` 是一个 Windows x64 优先的只读 TUI，用来浏览本地 restic
仓库、搜索快照中的文件、预览文本/图片/视频帧，并安全导出单个文件。

v0.1 只会调用以下 restic 命令：

- `version`
- `snapshots`
- `ls`
- `find`
- `dump`

它不会执行备份、删除、修复、迁移或其他可能修改仓库的操作。

## 依赖

- Windows x64
- restic 0.19.x
- ffmpeg 和 ffprobe

这些工具必须已经安装并可从 `PATH` 找到，也可以使用命令行参数指定路径。
发布包只包含 `restic-browser.exe`，不会捆绑上述外部工具。

## 使用

```powershell
restic-browser.exe -r D:\backup\restic-repo
```

也可以设置 `RESTIC_REPOSITORY`，或显式指定工具：

```powershell
restic-browser.exe `
  --repository D:\backup\restic-repo `
  --restic C:\tools\restic.exe `
  --ffmpeg C:\tools\ffmpeg.exe `
  --ffprobe C:\tools\ffprobe.exe
```

程序启动后只询问一次密码。密码仅存在于当前进程内存中，并通过当前 restic
子进程的环境传递；不会写入配置、日志或命令参数。

可选的 `--log-file <path>` 会写入脱敏诊断日志。

## 按键

| 按键 | 操作 |
| --- | --- |
| `Tab` | 在快照和文件列表之间切换 |
| `Enter` | 打开快照、进入目录或预览文件 |
| `Backspace` | 返回上级目录 |
| `/` | 在当前快照中按 restic pattern 搜索 |
| `p` | 预览当前文件 |
| `←` / `→` | 视频帧后退/前进 5 秒 |
| `e` | 导出当前单文件；目标存在时拒绝覆盖 |
| `r` | 刷新当前目录 |
| `Esc` | 取消当前任务 |
| `q` | 安全退出并恢复终端 |

图片显示会自动尝试终端图形协议；不可用时降级为 Unicode half-block 或元数据。
文本预览上限为 2 MiB。媒体源超过 512 MiB 时只显示元数据。

## 构建与检查

```powershell
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo build --release
```

真实仓库集成测试会在系统存在 restic 0.19.x 时临时执行 `init` 和 `backup`
来生成测试夹具；产品代码仍然只使用只读命令。

## v0.1 边界

本版本不支持远程仓库配置、目录或多选导出、跨快照比较、PDF 页面、音频播放、
系统凭据库、仓库健康检查和安装器。
