use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use image::ImageReader;
use serde_json::Value;
use tokio::{io::AsyncReadExt, process::Command};
use tokio_util::sync::CancellationToken;

use crate::{
    AppError, Result,
    cache::SessionCache,
    model::{FileEntry, MediaMetadata, PreviewArtifact},
    restic::ResticClient,
};

const TEXT_LIMIT: usize = 2 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct PreviewService {
    ffmpeg: PathBuf,
    ffprobe: PathBuf,
    cache: SessionCache,
}

impl PreviewService {
    pub fn new(
        ffmpeg: impl Into<PathBuf>,
        ffprobe: impl Into<PathBuf>,
        cache: SessionCache,
    ) -> Self {
        Self {
            ffmpeg: ffmpeg.into(),
            ffprobe: ffprobe.into(),
            cache,
        }
    }

    pub async fn check_dependencies(&self) -> Result<()> {
        check_tool(&self.ffmpeg).await?;
        check_tool(&self.ffprobe).await
    }

    pub async fn preview(
        &self,
        restic: Arc<ResticClient>,
        snapshot: &str,
        entry: &FileEntry,
        position: Duration,
        token: CancellationToken,
    ) -> Result<PreviewArtifact> {
        if entry.is_dir() {
            return Ok(PreviewArtifact::MetadataOnly {
                reason: "directories cannot be previewed".to_owned(),
                metadata: None,
            });
        }
        if entry.size > self.cache.max_bytes() {
            return Ok(PreviewArtifact::MetadataOnly {
                reason: AppError::PreviewTooLarge {
                    size: entry.size,
                    limit: self.cache.max_bytes(),
                }
                .to_string(),
                metadata: None,
            });
        }

        let extension = Path::new(&entry.name)
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or("bin")
            .to_ascii_lowercase();
        let source = self.cache.allocate(&extension)?;
        restic
            .dump_to_path(snapshot, &entry.path, &source, token.child_token())
            .await?;

        let result = if is_text_extension(&extension) {
            preview_text(&source).await?
        } else if is_image_extension(&extension) {
            let image = load_image(&source)?;
            PreviewArtifact::Image {
                image: Arc::new(image),
                source: source.clone(),
            }
        } else if is_video_extension(&extension) {
            self.preview_video(&source, position, token.child_token())
                .await?
        } else if is_audio_extension(&extension) {
            PreviewArtifact::MetadataOnly {
                reason: "audio playback is not supported".to_owned(),
                metadata: Some(self.probe(&source, token.child_token()).await?),
            }
        } else if looks_like_text(&source).await? {
            preview_text(&source).await?
        } else {
            let metadata = self.probe(&source, token.child_token()).await.ok();
            PreviewArtifact::MetadataOnly {
                reason: "no built-in preview for this file type".to_owned(),
                metadata,
            }
        };
        self.cache.register(source)?;
        Ok(result)
    }

    async fn preview_video(
        &self,
        source: &Path,
        requested: Duration,
        token: CancellationToken,
    ) -> Result<PreviewArtifact> {
        let metadata = self.probe(source, token.child_token()).await?;
        let duration = metadata.duration.unwrap_or_default().max(0.0);
        let position = requested.as_secs_f64().min(duration.max(0.0));
        let frame = self.cache.allocate("png")?;
        let args = vec![
            OsString::from("-hide_banner"),
            OsString::from("-loglevel"),
            OsString::from("error"),
            OsString::from("-nostdin"),
            OsString::from("-ss"),
            OsString::from(format!("{position:.3}")),
            OsString::from("-i"),
            source.as_os_str().to_owned(),
            OsString::from("-frames:v"),
            OsString::from("1"),
            OsString::from("-vf"),
            OsString::from("scale=1280:-2:force_original_aspect_ratio=decrease"),
            OsString::from("-y"),
            frame.as_os_str().to_owned(),
        ];
        run_external(&self.ffmpeg, args, token).await?;
        let image = load_image(&frame)?;
        self.cache.register(frame.clone())?;
        Ok(PreviewArtifact::VideoFrame {
            image: Arc::new(image),
            source: source.to_path_buf(),
            frame,
            position: Duration::from_secs_f64(position),
            metadata,
        })
    }

    async fn probe(&self, source: &Path, token: CancellationToken) -> Result<MediaMetadata> {
        let args = vec![
            OsString::from("-v"),
            OsString::from("error"),
            OsString::from("-show_format"),
            OsString::from("-show_streams"),
            OsString::from("-of"),
            OsString::from("json"),
            source.as_os_str().to_owned(),
        ];
        let stdout = run_external(&self.ffprobe, args, token).await?;
        parse_probe(&stdout)
    }
}

async fn check_tool(path: &Path) -> Result<()> {
    let output = Command::new(path)
        .arg("-version")
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                AppError::DependencyMissing(path.display().to_string())
            } else {
                AppError::Io(error)
            }
        })?;
    if output.status.success() {
        Ok(())
    } else {
        Err(AppError::classify_stderr(
            &path.display().to_string(),
            &output.stderr,
        ))
    }
}

async fn run_external(
    program: &Path,
    args: Vec<OsString>,
    token: CancellationToken,
) -> Result<Vec<u8>> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                AppError::DependencyMissing(program.display().to_string())
            } else {
                AppError::Io(error)
            }
        })?;
    let mut stdout = child.stdout.take().expect("stdout was piped");
    let mut stderr = child.stderr.take().expect("stderr was piped");
    let stdout_task = tokio::spawn(async move {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).await.map(|_| bytes)
    });
    let stderr_task = tokio::spawn(async move {
        let mut bytes = Vec::new();
        stderr.read_to_end(&mut bytes).await.map(|_| bytes)
    });
    let status = tokio::select! {
        status = child.wait() => status?,
        _ = token.cancelled() => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            return Err(AppError::Cancelled);
        }
    };
    let stdout = stdout_task
        .await
        .map_err(|error| AppError::Other(format!("stdout reader failed: {error}")))??;
    let stderr = stderr_task
        .await
        .map_err(|error| AppError::Other(format!("stderr reader failed: {error}")))??;
    if !status.success() {
        return Err(AppError::classify_stderr(
            &program.display().to_string(),
            &stderr,
        ));
    }
    Ok(stdout)
}

async fn preview_text(path: &Path) -> Result<PreviewArtifact> {
    let file = tokio::fs::File::open(path).await?;
    let mut bytes = Vec::with_capacity(TEXT_LIMIT.min(64 * 1024));
    file.take((TEXT_LIMIT + 1) as u64)
        .read_to_end(&mut bytes)
        .await?;
    let truncated = bytes.len() > TEXT_LIMIT;
    bytes.truncate(TEXT_LIMIT);
    let text = String::from_utf8_lossy(&bytes).into_owned();
    Ok(PreviewArtifact::Text { text, truncated })
}

async fn looks_like_text(path: &Path) -> Result<bool> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut bytes = vec![0; 8192];
    let read = file.read(&mut bytes).await?;
    bytes.truncate(read);
    Ok(!bytes.contains(&0) && std::str::from_utf8(&bytes).is_ok())
}

fn load_image(path: &Path) -> Result<image::DynamicImage> {
    ImageReader::open(path)
        .map_err(AppError::Io)?
        .with_guessed_format()
        .map_err(AppError::Io)?
        .decode()
        .map_err(|error| AppError::Other(format!("image decode failed: {error}")))
}

fn parse_probe(bytes: &[u8]) -> Result<MediaMetadata> {
    let value: Value = serde_json::from_slice(bytes)?;
    let format = value.get("format").unwrap_or(&Value::Null);
    let streams = value
        .get("streams")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let video = streams
        .iter()
        .find(|stream| stream.get("codec_type").and_then(Value::as_str) == Some("video"));
    let audio = streams
        .iter()
        .find(|stream| stream.get("codec_type").and_then(Value::as_str) == Some("audio"));
    Ok(MediaMetadata {
        format_name: format
            .get("format_name")
            .and_then(Value::as_str)
            .map(str::to_owned),
        duration: format
            .get("duration")
            .and_then(Value::as_str)
            .and_then(|value| value.parse().ok()),
        bit_rate: format
            .get("bit_rate")
            .and_then(Value::as_str)
            .and_then(|value| value.parse().ok()),
        width: video
            .and_then(|value| value.get("width"))
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok()),
        height: video
            .and_then(|value| value.get("height"))
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok()),
        video_codec: video
            .and_then(|value| value.get("codec_name"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        audio_codec: audio
            .and_then(|value| value.get("codec_name"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        sample_rate: audio
            .and_then(|value| value.get("sample_rate"))
            .and_then(Value::as_str)
            .and_then(|value| value.parse().ok()),
        channels: audio
            .and_then(|value| value.get("channels"))
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok()),
    })
}

fn is_text_extension(extension: &str) -> bool {
    matches!(
        extension,
        "txt"
            | "md"
            | "json"
            | "toml"
            | "yaml"
            | "yml"
            | "xml"
            | "csv"
            | "log"
            | "rs"
            | "go"
            | "py"
            | "js"
            | "ts"
            | "css"
            | "html"
            | "sh"
            | "ps1"
    )
}

fn is_image_extension(extension: &str) -> bool {
    matches!(extension, "jpg" | "jpeg" | "png" | "webp" | "gif" | "bmp")
}

fn is_video_extension(extension: &str) -> bool {
    matches!(extension, "mp4" | "mkv" | "mov" | "avi" | "webm" | "m4v")
}

fn is_audio_extension(extension: &str) -> bool {
    matches!(extension, "mp3" | "m4a" | "flac" | "ogg" | "wav")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ffprobe_json() {
        let data = br#"{
          "streams": [
            {"codec_type":"video","codec_name":"h264","width":1920,"height":1080},
            {"codec_type":"audio","codec_name":"aac","sample_rate":"48000","channels":2}
          ],
          "format": {"format_name":"mov,mp4","duration":"12.5","bit_rate":"1000"}
        }"#;
        let metadata = parse_probe(data).unwrap();
        assert_eq!(metadata.duration, Some(12.5));
        assert_eq!(metadata.width, Some(1920));
        assert_eq!(metadata.audio_codec.as_deref(), Some("aac"));
    }
}
