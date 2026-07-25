use std::{
    path::Path,
    process::{Command, Stdio},
    sync::Arc,
};

use restic_browser::{
    AppError, cache::SessionCache, export::ExportService, model::PreviewArtifact,
    preview::PreviewService, repository::RepositoryReader, restic::ResticCliClient,
    rustic::RusticClient,
};
use secrecy::SecretString;
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

const PASSWORD: &str = "restic-browser-test-password";

#[tokio::test]
async fn real_repository_browse_search_dump_and_export() {
    if !integration_dependencies_available() {
        eprintln!("skipped: restic 0.19.x, ffmpeg, or ffprobe is not available");
        return;
    }

    let fixture = TempDir::new().expect("fixture directory");
    let repository = fixture.path().join("repository");
    let source = fixture.path().join("中文 空格 😀.txt");
    let image_path = fixture.path().join("preview image.png");
    let video_path = fixture.path().join("preview video.mp4");
    let folder = fixture.path().join("folder");
    let nested_path = folder.join("nested.txt");
    let expected = "restic-browser 集成测试\nsecond line\n".as_bytes();
    std::fs::write(&source, expected).expect("write fixture");
    std::fs::create_dir(&folder).expect("create nested fixture directory");
    std::fs::write(&nested_path, b"nested fixture").expect("write nested fixture");
    image::RgbImage::from_pixel(40, 30, image::Rgb([30, 120, 220]))
        .save(&image_path)
        .expect("write image fixture");
    make_video(&video_path);

    run_restic(&repository, &["init", "--repository-version", "2"], None);
    run_restic(
        &repository,
        &[
            "backup",
            source.file_name().unwrap().to_str().unwrap(),
            image_path.file_name().unwrap().to_str().unwrap(),
            video_path.file_name().unwrap().to_str().unwrap(),
            folder.file_name().unwrap().to_str().unwrap(),
        ],
        Some(fixture.path()),
    );
    let repository_before = repository_manifest(&repository);

    let wrong_password = RusticClient::open_with_cache_dir(
        &repository,
        "wrong password".to_owned(),
        Some(fixture.path().join("wrong-password-cache")),
    );
    assert!(matches!(wrong_password, Err(AppError::Authentication)));

    let cli_client = Arc::new(
        ResticCliClient::new(
            "restic",
            &repository,
            SecretString::from(PASSWORD.to_owned()),
        )
        .expect("client")
        .with_cache_dir(fixture.path().join("client-cache")),
    );
    let cli_snapshots = cli_client
        .list_snapshots(CancellationToken::new())
        .await
        .expect("list snapshots");
    assert_eq!(cli_snapshots.len(), 1);

    let rustic_client = Arc::new(
        RusticClient::open_with_cache_dir(
            &repository,
            PASSWORD.to_owned(),
            Some(fixture.path().join("rustic-cache")),
        )
        .expect("rustic client"),
    );
    let snapshots = rustic_client
        .list_snapshots(CancellationToken::new())
        .await
        .expect("rustic list snapshots");
    assert_eq!(snapshots.len(), 1);
    assert_eq!(snapshots[0].id, cli_snapshots[0].id);

    let cli_root = cli_client
        .list_directory(&snapshots[0].id, "/", CancellationToken::new())
        .await
        .expect("CLI list root");
    let root = rustic_client
        .list_directory(&snapshots[0].id, "/", CancellationToken::new())
        .await
        .expect("rustic list root");
    assert!(!root.is_empty());
    assert_eq!(entry_keys(&root), entry_keys(&cli_root));
    let folder_entry = root
        .iter()
        .find(|entry| entry.name == "folder")
        .expect("folder in root listing");
    let nested = rustic_client
        .list_directory(
            &snapshots[0].id,
            &folder_entry.path,
            CancellationToken::new(),
        )
        .await
        .expect("list nested directory");
    assert!(nested.iter().any(|entry| entry.name == "nested.txt"));

    let matches = rustic_client
        .find(&snapshots[0].id, "*.txt", CancellationToken::new())
        .await
        .expect("find text fixture");
    let found = matches
        .iter()
        .find(|result| result.entry.name == "中文 空格 😀.txt")
        .expect("unicode file in search results");

    assert!(!rustic_client.content_index_ready());
    let exported = fixture.path().join("exported.txt");
    ExportService
        .export_file(
            rustic_client.clone(),
            &snapshots[0].id,
            &found.entry.path,
            &exported,
            CancellationToken::new(),
        )
        .await
        .expect("export file");
    assert!(rustic_client.content_index_ready());
    let actual = std::fs::read(exported).expect("read exported file");
    assert_eq!(Sha256::digest(expected), Sha256::digest(actual));
    let existing_destination = fixture.path().join("exported.txt");
    let existing = ExportService
        .export_file(
            rustic_client.clone(),
            &snapshots[0].id,
            &found.entry.path,
            &existing_destination,
            CancellationToken::new(),
        )
        .await;
    assert!(matches!(existing, Err(AppError::DestinationExists(_))));
    assert_eq!(std::fs::read(existing_destination).unwrap(), expected);

    let preview_service = PreviewService::new(
        "ffmpeg",
        "ffprobe",
        SessionCache::new().expect("preview cache"),
    );
    preview_service
        .check_dependencies()
        .await
        .expect("media tools");

    let text_preview = preview_service
        .preview(
            rustic_client.clone(),
            &snapshots[0].id,
            &found.entry,
            std::time::Duration::ZERO,
            CancellationToken::new(),
        )
        .await
        .expect("text preview");
    assert!(matches!(text_preview, PreviewArtifact::Text { .. }));

    let image_match = find_named(
        &rustic_client,
        &snapshots[0].id,
        "*.png",
        "preview image.png",
    )
    .await;
    let image_preview = preview_service
        .preview(
            rustic_client.clone(),
            &snapshots[0].id,
            &image_match,
            std::time::Duration::ZERO,
            CancellationToken::new(),
        )
        .await
        .expect("image preview");
    assert!(matches!(image_preview, PreviewArtifact::Image { .. }));

    let video_match = find_named(
        &rustic_client,
        &snapshots[0].id,
        "*.mp4",
        "preview video.mp4",
    )
    .await;
    let video_preview = preview_service
        .preview(
            rustic_client.clone(),
            &snapshots[0].id,
            &video_match,
            std::time::Duration::from_secs(1),
            CancellationToken::new(),
        )
        .await
        .expect("video frame preview");
    match video_preview {
        PreviewArtifact::VideoFrame {
            metadata, position, ..
        } => {
            assert_eq!(metadata.width, Some(64));
            assert!(position.as_secs_f64() >= 1.0);
        }
        other => panic!("unexpected video preview: {other:?}"),
    }

    let cancelled_destination = fixture.path().join("cancelled.txt");
    let cancelled = rustic_client
        .dump_to_path(
            &snapshots[0].id,
            &found.entry.path,
            &cancelled_destination,
            {
                let token = CancellationToken::new();
                token.cancel();
                token
            },
        )
        .await;
    assert!(matches!(cancelled, Err(AppError::Cancelled)));
    assert!(!cancelled_destination.exists());

    assert_eq!(repository_before, repository_manifest(&repository));
}

#[tokio::test]
async fn rustic_reads_repository_format_v1() {
    if !has_supported_restic() {
        eprintln!("skipped: restic 0.19.x is not available");
        return;
    }

    let fixture = TempDir::new().expect("fixture directory");
    let repository = fixture.path().join("repository-v1");
    let source = fixture.path().join("旧格式 😀.txt");
    let expected = b"repository format v1";
    std::fs::write(&source, expected).expect("write v1 fixture");
    run_restic(&repository, &["init", "--repository-version", "1"], None);
    run_restic(
        &repository,
        &["backup", source.file_name().unwrap().to_str().unwrap()],
        Some(fixture.path()),
    );
    let repository_before = repository_manifest(&repository);

    let client = RusticClient::open_with_cache_dir(
        &repository,
        PASSWORD.to_owned(),
        Some(fixture.path().join("rustic-v1-cache")),
    )
    .expect("open repository format v1");
    let snapshots = client
        .list_snapshots(CancellationToken::new())
        .await
        .expect("list v1 snapshots");
    let root = client
        .list_directory(&snapshots[0].id, "/", CancellationToken::new())
        .await
        .expect("list v1 root");
    let entry = root
        .iter()
        .find(|entry| entry.name == "旧格式 😀.txt")
        .expect("unicode v1 entry");
    let destination = fixture.path().join("v1-export.txt");
    client
        .dump_to_path(
            &snapshots[0].id,
            &entry.path,
            &destination,
            CancellationToken::new(),
        )
        .await
        .expect("read v1 file");
    assert_eq!(std::fs::read(destination).unwrap(), expected);
    assert_eq!(repository_before, repository_manifest(&repository));
}

fn has_supported_restic() -> bool {
    Command::new("restic")
        .arg("version")
        .stdin(Stdio::null())
        .output()
        .is_ok_and(|output| {
            output.status.success()
                && String::from_utf8_lossy(&output.stdout).starts_with("restic 0.19.")
        })
}

fn integration_dependencies_available() -> bool {
    has_supported_restic() && has_tool("ffmpeg") && has_tool("ffprobe")
}

fn has_tool(program: &str) -> bool {
    Command::new(program)
        .arg("-version")
        .stdin(Stdio::null())
        .output()
        .is_ok_and(|output| output.status.success())
}

fn run_restic(repository: &Path, args: &[&str], current_dir: Option<&Path>) {
    let mut command = Command::new("restic");
    command
        .arg("--repo")
        .arg(repository)
        .arg("--no-cache")
        .args(args)
        .env("RESTIC_PASSWORD", PASSWORD)
        .stdin(Stdio::null());
    if let Some(directory) = current_dir {
        command.current_dir(directory);
    }
    let output = command.output().expect("run fixture restic");
    assert!(
        output.status.success(),
        "restic fixture command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

async fn find_named(
    client: &Arc<RusticClient>,
    snapshot: &str,
    pattern: &str,
    name: &str,
) -> restic_browser::model::FileEntry {
    client
        .find(snapshot, pattern, CancellationToken::new())
        .await
        .expect("find preview fixture")
        .into_iter()
        .find(|result| result.entry.name == name)
        .expect("named preview fixture")
        .entry
}

fn entry_keys(entries: &[restic_browser::model::FileEntry]) -> Vec<(String, String, u64)> {
    entries
        .iter()
        .map(|entry| {
            (
                entry.path.clone(),
                format!("{:?}", entry.file_type),
                entry.size,
            )
        })
        .collect()
}

fn make_video(destination: &Path) {
    let output = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=64x48:rate=2",
            "-t",
            "2",
            "-pix_fmt",
            "yuv420p",
            "-y",
        ])
        .arg(destination)
        .stdin(Stdio::null())
        .output()
        .expect("generate video fixture");
    assert!(
        output.status.success(),
        "ffmpeg fixture failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn repository_manifest(repository: &Path) -> Vec<(String, Vec<u8>)> {
    fn visit(root: &Path, directory: &Path, files: &mut Vec<(String, Vec<u8>)>) {
        for entry in std::fs::read_dir(directory).expect("read repository directory") {
            let entry = entry.expect("read repository entry");
            let path = entry.path();
            if path.is_dir() {
                visit(root, &path, files);
            } else {
                let relative = path
                    .strip_prefix(root)
                    .expect("repository-relative path")
                    .to_string_lossy()
                    .replace('\\', "/");
                files.push((
                    relative,
                    Sha256::digest(std::fs::read(path).unwrap()).to_vec(),
                ));
            }
        }
    }

    let mut files = Vec::new();
    visit(repository, repository, &mut files);
    files.sort_by(|left, right| left.0.cmp(&right.0));
    files
}
