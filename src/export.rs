use std::path::{Path, PathBuf};

use tempfile::Builder;
use tokio_util::sync::CancellationToken;

use crate::{AppError, Result, repository::RepositoryHandle};

#[derive(Debug, Default, Clone)]
pub struct ExportService;

impl ExportService {
    pub async fn export_file(
        &self,
        repository: RepositoryHandle,
        snapshot: &str,
        source: &str,
        destination: &Path,
        token: CancellationToken,
    ) -> Result<PathBuf> {
        if destination.exists() {
            return Err(AppError::DestinationExists(destination.to_path_buf()));
        }
        let parent = destination
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        if !parent.is_dir() {
            return Err(AppError::InvalidPath(format!(
                "export directory does not exist: {}",
                parent.display()
            )));
        }

        let temporary = Builder::new()
            .prefix(".restic-browser-")
            .suffix(".tmp")
            .tempfile_in(parent)?;
        let temporary = temporary.into_temp_path();
        repository
            .dump_to_path(snapshot, source, temporary.as_ref(), token)
            .await?;
        temporary
            .persist_noclobber(destination)
            .map_err(|error| match error.error.kind() {
                std::io::ErrorKind::AlreadyExists => {
                    AppError::DestinationExists(destination.to_path_buf())
                }
                _ => AppError::Io(error.error),
            })?;
        Ok(destination.to_path_buf())
    }
}
