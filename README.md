# restic-browser

[ English | [中文](README_cn.md) ]

`restic-browser` is a read-only TUI for browsing local restic repositories, searching snapshots,
previewing text, images, and video frames, and safely exporting files or restoring directories.
Windows x64 is the current release priority; the code and CI also target Linux and macOS.

## Current implementation

- The default backend is the third-party Rust implementation `rustic_core`, which reuses one
  unlocked session after opening the repository.
- `--backend restic-cli` explicitly enables the restic 0.19.x CLI comparison/fallback backend.
  The application never falls back silently.
- ffmpeg and ffprobe are external dependencies used for media metadata and single-frame video
  previews.
- The password remains only in process memory. Repository operations are read-only, and file
  exports and directory restores refuse to overwrite existing content.
- Only local repositories are officially supported. Backup, deletion, repair, and migration are
  outside the current scope.
- The interface is in English by default. Pass `--cn` to use Chinese.

```powershell
restic-browser.exe -r D:\backup\restic-repo
restic-browser.exe -r D:\backup\restic-repo --backend restic-cli
restic-browser.exe -r D:\backup\restic-repo --cn
```

You can also set the repository with `RESTIC_REPOSITORY`. Use `--ffmpeg` and `--ffprobe` to
override the media tool paths. The CLI fallback backend also supports `--restic`. Use
`--log-file` to enable redacted diagnostic logging.

## Documentation

- [v0.1 product goals, workflow, key bindings, and acceptance criteria](docs/product-v0.1.md)
- [Current architecture, security boundaries, and performance characteristics](docs/architecture.md)
- [`rustic_core` migration status, risks, and completion criteria](docs/rustic-core-migration.md)

## Build

A stable Rust toolchain is required. The Windows MSVC target also requires Visual Studio C++
Build Tools.

```powershell
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --locked
cargo build --release --locked
```
