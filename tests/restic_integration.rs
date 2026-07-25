use std::{
    path::Path,
    process::{Command, Stdio},
    sync::Arc,
};

use restic_browser::{
    cache::SessionCache, export::ExportService, model::PreviewArtifact, preview::PreviewService,
    restic::ResticClient,
};
use secrecy::SecretString;
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

const PASSWORD: &str = "restic-browser-test-password";

#[tokio::test]
async fn real_repository_browse_search_dump_and_export() {
    if !has_supported_restic() {
        eprintln!("skipped: restic 0.19.x is not available");
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

    run_restic(&repository, &["init"], None);
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

    let client = Arc::new(
        ResticClient::new(
            "restic",
            &repository,
            SecretString::from(PASSWORD.to_owned()),
        )
        .expect("client")
        .with_cache_dir(fixture.path().join("client-cache")),
    );
    let snapshots = client
        .list_snapshots(CancellationToken::new())
        .await
        .expect("list snapshots");
    assert_eq!(snapshots.len(), 1);

    let root = client
        .list_directory(&snapshots[0].id, "/", CancellationToken::new())
        .await
        .expect("list root");
    assert!(!root.is_empty());
    let folder_entry = root
        .iter()
        .find(|entry| entry.name == "folder")
        .expect("folder in root listing");
    let nested = client
        .list_directory(
            &snapshots[0].id,
            &folder_entry.path,
            CancellationToken::new(),
        )
        .await
        .expect("list nested directory");
    assert!(nested.iter().any(|entry| entry.name == "nested.txt"));

    let matches = client
        .find(&snapshots[0].id, "*.txt", CancellationToken::new())
        .await
        .expect("find text fixture");
    let found = matches
        .iter()
        .find(|result| result.entry.name == "中文 空格 😀.txt")
        .expect("unicode file in search results");

    let exported = fixture.path().join("exported.txt");
    ExportService
        .export_file(
            Arc::clone(&client),
            &snapshots[0].id,
            &found.entry.path,
            &exported,
            CancellationToken::new(),
        )
        .await
        .expect("export file");
    let actual = std::fs::read(exported).expect("read exported file");
    assert_eq!(Sha256::digest(expected), Sha256::digest(actual));

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
            Arc::clone(&client),
            &snapshots[0].id,
            &found.entry,
            std::time::Duration::ZERO,
            CancellationToken::new(),
        )
        .await
        .expect("text preview");
    assert!(matches!(text_preview, PreviewArtifact::Text { .. }));

    let image_match = find_named(&client, &snapshots[0].id, "*.png", "preview image.png").await;
    let image_preview = preview_service
        .preview(
            Arc::clone(&client),
            &snapshots[0].id,
            &image_match,
            std::time::Duration::ZERO,
            CancellationToken::new(),
        )
        .await
        .expect("image preview");
    assert!(matches!(image_preview, PreviewArtifact::Image { .. }));

    let video_match = find_named(&client, &snapshots[0].id, "*.mp4", "preview video.mp4").await;
    let video_preview = preview_service
        .preview(
            Arc::clone(&client),
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
    client: &Arc<ResticClient>,
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
