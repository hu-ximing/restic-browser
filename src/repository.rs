use std::{path::Path, sync::Arc};

use futures_util::future::BoxFuture;
use tokio_util::sync::CancellationToken;

use crate::{
    Result,
    model::{FileEntry, SearchResult, Snapshot},
};

pub type RepositoryHandle = Arc<dyn RepositoryReader>;

/// The complete read-only boundary used by the application.
pub trait RepositoryReader: Send + Sync {
    fn list_snapshots(&self, token: CancellationToken) -> BoxFuture<'_, Result<Vec<Snapshot>>>;

    fn list_directory(
        &self,
        snapshot: &str,
        path: &str,
        token: CancellationToken,
    ) -> BoxFuture<'_, Result<Vec<FileEntry>>>;

    fn find(
        &self,
        snapshot: &str,
        pattern: &str,
        token: CancellationToken,
    ) -> BoxFuture<'_, Result<Vec<SearchResult>>>;

    fn dump_to_path(
        &self,
        snapshot: &str,
        source: &str,
        destination: &Path,
        token: CancellationToken,
    ) -> BoxFuture<'_, Result<()>>;

    /// Whether reading file content can start without upgrading an index.
    fn content_index_ready(&self) -> bool {
        true
    }
}
