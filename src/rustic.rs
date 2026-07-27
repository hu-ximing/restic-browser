use std::{
    collections::HashMap,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
};

use futures_util::future::BoxFuture;
use globset::GlobBuilder;
use rustic_backend::BackendOptions;
use rustic_core::{
    Credentials, IndexedFull, IndexedFullStatus, IndexedIdsStatus, IndexedTree, LsOptions,
    Repository, RepositoryOptions, RusticError,
};
use serde::Serialize;
use serde_json::Value;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use zeroize::Zeroize;

use crate::{
    AppError, Result,
    error::redact,
    model::{FileEntry, FileVersion, SearchResult, Snapshot},
    repository::RepositoryReader,
    restic::{normalize_repo_path, parse_file_entry, parse_snapshot},
};

type BlockingTask = Box<dyn FnOnce() + Send + 'static>;
const MAX_DIRECTORY_ENTRIES: usize = 100_000;
const MAX_SEARCH_RESULTS: usize = 100_000;
const MAX_FILE_VERSIONS: usize = 100_000;

#[derive(Clone)]
struct BlockingExecutor {
    sender: mpsc::Sender<BlockingTask>,
}

impl BlockingExecutor {
    fn new() -> Result<Self> {
        let (sender, receiver) = mpsc::channel::<BlockingTask>();
        std::thread::Builder::new()
            .name("restic-browser-repository".to_owned())
            .spawn(move || {
                while let Ok(task) = receiver.recv() {
                    task();
                }
            })
            .map_err(AppError::Io)?;
        Ok(Self { sender })
    }

    async fn run<T, F>(&self, token: CancellationToken, operation: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&CancellationToken) -> Result<T> + Send + 'static,
    {
        let (result_sender, result_receiver) = oneshot::channel();
        let task_token = token.clone();
        self.sender
            .send(Box::new(move || {
                let result = if task_token.is_cancelled() {
                    Err(AppError::Cancelled)
                } else {
                    operation(&task_token).and_then(|value| {
                        if task_token.is_cancelled() {
                            Err(AppError::Cancelled)
                        } else {
                            Ok(value)
                        }
                    })
                };
                let _ = result_sender.send(result);
            }))
            .map_err(|_| AppError::Other("repository worker is unavailable".to_owned()))?;
        result_receiver
            .await
            .map_err(|_| AppError::Other("repository worker stopped unexpectedly".to_owned()))?
    }
}

enum RusticIndex {
    Browse(Repository<IndexedIdsStatus>),
    Full(Repository<IndexedFullStatus>),
    Unavailable,
}

struct RusticState {
    index: RusticIndex,
}

/// A read-only adapter around the third-party rustic implementation.
pub struct RusticClient {
    repository: PathBuf,
    state: Arc<Mutex<RusticState>>,
    executor: BlockingExecutor,
    content_index_ready: Arc<AtomicBool>,
}

impl RusticClient {
    pub fn open(repository: impl Into<PathBuf>, password: String) -> Result<Self> {
        Self::open_with_cache_dir(repository, password, None)
    }

    pub fn open_with_cache_dir(
        repository: impl Into<PathBuf>,
        password: String,
        cache_dir: Option<PathBuf>,
    ) -> Result<Self> {
        let repository = repository.into();
        if !repository.exists() {
            return Err(AppError::RepositoryNotFound);
        }
        let repository_string = repository
            .to_str()
            .ok_or_else(|| {
                AppError::InvalidPath(
                    "rustic backend requires a UTF-8 repository path; use --backend restic-cli for this repository"
                        .to_owned(),
                )
            })?
            .to_owned();

        let backends = BackendOptions::default()
            .repository(repository_string)
            .to_backends()
            .map_err(map_rustic_error)?;
        let repository_options = if let Some(cache_dir) = cache_dir {
            RepositoryOptions::default().cache_dir(cache_dir)
        } else {
            RepositoryOptions::default()
        };

        let mut credentials = Credentials::Password(password);
        let opened = Repository::new(&repository_options, &backends)
            .and_then(|repository| repository.open(&credentials));
        let Credentials::Password(password) = &mut credentials else {
            unreachable!("this adapter only opens repositories with a password");
        };
        password.zeroize();
        let opened = opened.map_err(map_rustic_error)?;
        let browse = opened.to_indexed_ids().map_err(map_rustic_error)?;

        Ok(Self {
            repository,
            state: Arc::new(Mutex::new(RusticState {
                index: RusticIndex::Browse(browse),
            })),
            executor: BlockingExecutor::new()?,
            content_index_ready: Arc::new(AtomicBool::new(false)),
        })
    }

    pub fn repository(&self) -> &Path {
        &self.repository
    }
}

impl RepositoryReader for RusticClient {
    fn list_snapshots(&self, token: CancellationToken) -> BoxFuture<'_, Result<Vec<Snapshot>>> {
        let state = Arc::clone(&self.state);
        let executor = self.executor.clone();
        Box::pin(async move {
            executor
                .run(token, move |token| {
                    let state = lock_state(&state)?;
                    let snapshots = match &state.index {
                        RusticIndex::Browse(repository) => repository.get_all_snapshots(),
                        RusticIndex::Full(repository) => repository.get_all_snapshots(),
                        RusticIndex::Unavailable => {
                            return Err(AppError::Other(
                                "repository index is unavailable".to_owned(),
                            ));
                        }
                    }
                    .map_err(map_rustic_error)?;
                    let mut snapshots = snapshots
                        .into_iter()
                        .map(|snapshot| {
                            ensure_not_cancelled(token)?;
                            parse_snapshot(&serde_json::to_value(snapshot)?)
                        })
                        .collect::<Result<Vec<_>>>()?;
                    snapshots.sort_by(|left, right| right.time.cmp(&left.time));
                    Ok(snapshots)
                })
                .await
        })
    }

    fn list_directory(
        &self,
        snapshot: &str,
        path: &str,
        token: CancellationToken,
    ) -> BoxFuture<'_, Result<Vec<FileEntry>>> {
        let snapshot = snapshot.to_owned();
        let path = path.to_owned();
        let state = Arc::clone(&self.state);
        let executor = self.executor.clone();
        Box::pin(async move {
            executor
                .run(token, move |token| {
                    let state = lock_state(&state)?;
                    match &state.index {
                        RusticIndex::Browse(repository) => {
                            list_directory(repository, &snapshot, &path, token)
                        }
                        RusticIndex::Full(repository) => {
                            list_directory(repository, &snapshot, &path, token)
                        }
                        RusticIndex::Unavailable => Err(AppError::Other(
                            "repository index is unavailable".to_owned(),
                        )),
                    }
                })
                .await
        })
    }

    fn find(
        &self,
        snapshot: &str,
        pattern: &str,
        token: CancellationToken,
    ) -> BoxFuture<'_, Result<Vec<SearchResult>>> {
        let snapshot = snapshot.to_owned();
        let pattern = pattern.to_owned();
        let state = Arc::clone(&self.state);
        let executor = self.executor.clone();
        Box::pin(async move {
            executor
                .run(token, move |token| {
                    let state = lock_state(&state)?;
                    match &state.index {
                        RusticIndex::Browse(repository) => {
                            find(repository, &snapshot, &pattern, token)
                        }
                        RusticIndex::Full(repository) => {
                            find(repository, &snapshot, &pattern, token)
                        }
                        RusticIndex::Unavailable => Err(AppError::Other(
                            "repository index is unavailable".to_owned(),
                        )),
                    }
                })
                .await
        })
    }

    fn list_file_versions(
        &self,
        snapshots: Vec<Snapshot>,
        path: String,
        token: CancellationToken,
    ) -> BoxFuture<'_, Result<Vec<FileVersion>>> {
        let state = Arc::clone(&self.state);
        let executor = self.executor.clone();
        Box::pin(async move {
            executor
                .run(token, move |token| {
                    let state = lock_state(&state)?;
                    match &state.index {
                        RusticIndex::Browse(repository) => {
                            list_file_versions(repository, snapshots, &path, token)
                        }
                        RusticIndex::Full(repository) => {
                            list_file_versions(repository, snapshots, &path, token)
                        }
                        RusticIndex::Unavailable => Err(AppError::Other(
                            "repository index is unavailable".to_owned(),
                        )),
                    }
                })
                .await
        })
    }

    fn dump_to_path(
        &self,
        snapshot: &str,
        source: &str,
        destination: &Path,
        token: CancellationToken,
    ) -> BoxFuture<'_, Result<()>> {
        let snapshot = snapshot.to_owned();
        let source = source.to_owned();
        let destination = destination.to_path_buf();
        let state = Arc::clone(&self.state);
        let ready = Arc::clone(&self.content_index_ready);
        let executor = self.executor.clone();
        Box::pin(async move {
            executor
                .run(token, move |token| {
                    let result = dump_file(&state, &ready, &snapshot, &source, &destination, token);
                    if result.is_err() {
                        let _ = std::fs::remove_file(&destination);
                    }
                    result
                })
                .await
        })
    }

    fn content_index_ready(&self) -> bool {
        self.content_index_ready.load(Ordering::Acquire)
    }
}

fn lock_state(state: &Mutex<RusticState>) -> Result<std::sync::MutexGuard<'_, RusticState>> {
    state
        .lock()
        .map_err(|_| AppError::Other("repository state lock was poisoned".to_owned()))
}

fn ensure_not_cancelled(token: &CancellationToken) -> Result<()> {
    if token.is_cancelled() {
        Err(AppError::Cancelled)
    } else {
        Ok(())
    }
}

fn list_directory<S: IndexedTree>(
    repository: &Repository<S>,
    snapshot: &str,
    path: &str,
    token: &CancellationToken,
) -> Result<Vec<FileEntry>> {
    ensure_snapshot_id(snapshot)?;
    let path = normalize_repo_path(path)?;
    let snapshot = repository
        .get_snapshot_from_str(snapshot, |_| true)
        .map_err(map_rustic_error)?;
    let node = repository
        .node_from_snapshot_and_path(&snapshot, &path)
        .map_err(map_rustic_error)?;
    let options = LsOptions::default().recursive(false);
    let mut entries = Vec::new();
    for item in repository.ls(&node, &options).map_err(map_rustic_error)? {
        ensure_not_cancelled(token)?;
        if entries.len() == MAX_DIRECTORY_ENTRIES {
            return Err(AppError::InvalidResponse(format!(
                "directory contains more than {MAX_DIRECTORY_ENTRIES} entries"
            )));
        }
        let (relative, node) = item.map_err(map_rustic_error)?;
        entries.push(node_to_entry(&path, &relative, &node)?);
    }
    sort_entries(&mut entries);
    Ok(entries)
}

fn find<S: IndexedTree>(
    repository: &Repository<S>,
    snapshot_id: &str,
    pattern: &str,
    token: &CancellationToken,
) -> Result<Vec<SearchResult>> {
    ensure_snapshot_id(snapshot_id)?;
    if pattern.trim().is_empty() {
        return Err(AppError::InvalidPath(
            "search pattern cannot be empty".to_owned(),
        ));
    }
    let matcher = GlobBuilder::new(pattern)
        .literal_separator(false)
        .backslash_escape(true)
        .build()
        .map_err(|error| AppError::InvalidPath(format!("invalid search pattern: {error}")))?
        .compile_matcher();
    let snapshot = repository
        .get_snapshot_from_str(snapshot_id, |_| true)
        .map_err(map_rustic_error)?;
    let root = repository
        .node_from_snapshot_and_path(&snapshot, "/")
        .map_err(map_rustic_error)?;
    let mut results = Vec::new();
    for item in repository
        .ls(&root, &LsOptions::default())
        .map_err(map_rustic_error)?
    {
        ensure_not_cancelled(token)?;
        let (relative, node) = item.map_err(map_rustic_error)?;
        let portable = relative.to_string_lossy().replace('\\', "/");
        let name_matches = relative
            .file_name()
            .is_some_and(|name| matcher.is_match(Path::new(name)));
        if matcher.is_match(&portable) || matcher.is_match(format!("/{portable}")) || name_matches {
            if results.len() == MAX_SEARCH_RESULTS {
                return Err(AppError::InvalidResponse(format!(
                    "search returned more than {MAX_SEARCH_RESULTS} results"
                )));
            }
            results.push(SearchResult {
                snapshot_id: snapshot_id.to_owned(),
                snapshot_time: Some(snapshot.time.to_string()),
                entry: node_to_entry("/", &relative, &node)?,
            });
        }
    }
    Ok(results)
}

fn list_file_versions<S: IndexedTree>(
    repository: &Repository<S>,
    snapshots: Vec<Snapshot>,
    path: &str,
    token: &CancellationToken,
) -> Result<Vec<FileVersion>> {
    let path = normalize_repo_path(path)?;
    if path == "/" {
        return Ok(Vec::new());
    }
    if snapshots.len() > MAX_FILE_VERSIONS {
        return Err(AppError::InvalidResponse(format!(
            "file has more than {MAX_FILE_VERSIONS} snapshot records"
        )));
    }
    let ids = snapshots
        .iter()
        .map(|snapshot| snapshot.id.as_str())
        .collect::<Vec<_>>();
    let raw_snapshots = repository.get_snapshots(&ids).map_err(map_rustic_error)?;
    let mut trees = raw_snapshots
        .into_iter()
        .map(|snapshot| (snapshot.id.into_inner().to_hex().to_string(), snapshot.tree))
        .collect::<HashMap<_, _>>();
    let mut ordered_snapshots = Vec::with_capacity(snapshots.len());
    let mut root_trees = Vec::with_capacity(snapshots.len());
    for snapshot in snapshots {
        ensure_not_cancelled(token)?;
        let tree = trees.remove(&snapshot.id).ok_or_else(|| {
            AppError::InvalidResponse(format!(
                "rustic did not return snapshot {}",
                snapshot.short_id
            ))
        })?;
        ordered_snapshots.push(snapshot);
        root_trees.push(tree);
    }
    let found = repository
        .find_nodes_from_path(root_trees, Path::new(&path))
        .map_err(map_rustic_error)?;
    let relative = Path::new(path.trim_start_matches('/'));
    let mut versions = Vec::new();
    for (snapshot, matched) in ordered_snapshots.into_iter().zip(found.matches) {
        ensure_not_cancelled(token)?;
        let Some(node_index) = matched else {
            continue;
        };
        let node = found.nodes.get(node_index).ok_or_else(|| {
            AppError::InvalidResponse("rustic returned an invalid version node index".to_owned())
        })?;
        versions.push(FileVersion {
            snapshot,
            entry: node_to_entry("/", relative, node)?,
        });
    }
    Ok(versions)
}

fn dump_file(
    state: &Mutex<RusticState>,
    ready: &AtomicBool,
    snapshot: &str,
    source: &str,
    destination: &Path,
    token: &CancellationToken,
) -> Result<()> {
    ensure_snapshot_id(snapshot)?;
    let source = normalize_repo_path(source)?;
    ensure_not_cancelled(token)?;
    let mut state = lock_state(state)?;
    if matches!(state.index, RusticIndex::Browse(_)) {
        let current = std::mem::replace(&mut state.index, RusticIndex::Unavailable);
        let RusticIndex::Browse(repository) = current else {
            unreachable!("the index state was checked above");
        };
        match repository.to_indexed() {
            Ok(full) => {
                state.index = RusticIndex::Full(full);
                ready.store(true, Ordering::Release);
            }
            Err(error) => return Err(map_rustic_error(error)),
        }
    }
    ensure_not_cancelled(token)?;
    let RusticIndex::Full(repository) = &state.index else {
        unreachable!("the full index was installed above");
    };
    dump_with_repository(repository, snapshot, &source, destination, token)
}

fn dump_with_repository<S: IndexedFull>(
    repository: &Repository<S>,
    snapshot: &str,
    source: &str,
    destination: &Path,
    token: &CancellationToken,
) -> Result<()> {
    let snapshot = repository
        .get_snapshot_from_str(snapshot, |_| true)
        .map_err(map_rustic_error)?;
    let node = repository
        .node_from_snapshot_and_path(&snapshot, source)
        .map_err(map_rustic_error)?;
    let file = std::fs::File::create(destination)?;
    let mut writer = CancellationWriter { file, token };
    repository.dump(&node, &mut writer).map_err(|error| {
        if token.is_cancelled() {
            AppError::Cancelled
        } else {
            map_rustic_error(error)
        }
    })?;
    writer.flush()?;
    ensure_not_cancelled(token)
}

struct CancellationWriter<'a> {
    file: std::fs::File,
    token: &'a CancellationToken,
}

impl Write for CancellationWriter<'_> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if self.token.is_cancelled() {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
        }
        self.file.write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.token.is_cancelled() {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
        }
        self.file.flush()
    }
}

fn node_to_entry(base: &str, relative: &Path, node: &impl Serialize) -> Result<FileEntry> {
    let portable = relative.to_string_lossy().replace('\\', "/");
    let full_path = normalize_repo_path(&format!("{base}/{portable}"))?;
    let mut value = serde_json::to_value(node)?;
    let Value::Object(object) = &mut value else {
        return Err(AppError::InvalidResponse(
            "rustic node did not serialize as an object".to_owned(),
        ));
    };
    object.insert("path".to_owned(), Value::String(full_path));
    if let Some(name) = relative.file_name().and_then(|name| name.to_str()) {
        object.insert("name".to_owned(), Value::String(name.to_owned()));
    }
    parse_file_entry(&value)?.ok_or_else(|| {
        AppError::InvalidResponse("rustic returned a non-node directory entry".to_owned())
    })
}

fn sort_entries(entries: &mut [FileEntry]) {
    entries.sort_by(|left, right| {
        right
            .is_dir()
            .cmp(&left.is_dir())
            .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
    });
}

fn ensure_snapshot_id(snapshot: &str) -> Result<()> {
    if snapshot.is_empty()
        || !snapshot
            .chars()
            .all(|character| character.is_ascii_hexdigit())
    {
        Err(AppError::InvalidPath(
            "invalid snapshot identifier".to_owned(),
        ))
    } else {
        Ok(())
    }
}

fn map_rustic_error(error: impl AsRef<RusticError>) -> AppError {
    let error = error.as_ref();
    if error.is_incorrect_password() {
        return AppError::Authentication;
    }
    let message = error.display_log();
    let lower = message.to_ascii_lowercase();
    if lower.contains("no repository config")
        || lower.contains("repository config file")
        || lower.contains("not found")
    {
        AppError::RepositoryNotFound
    } else if lower.contains("repository version")
        || lower.contains("unsupported repository")
        || lower.contains("unsupported format")
    {
        AppError::RepositoryFormat
    } else {
        AppError::Other(redact(&message))
    }
}

#[cfg(test)]
mod tests {
    use super::{BlockingExecutor, ensure_snapshot_id};
    use crate::AppError;
    use tokio_util::sync::CancellationToken;

    #[test]
    fn validates_snapshot_identifiers() {
        assert!(ensure_snapshot_id(&"a".repeat(64)).is_ok());
        assert!(ensure_snapshot_id("latest").is_err());
    }

    #[tokio::test]
    async fn blocking_executor_skips_cancelled_work() {
        let executor = BlockingExecutor::new().unwrap();
        let token = CancellationToken::new();
        token.cancel();
        let result = executor.run(token, |_| Ok(42)).await;
        assert!(matches!(result, Err(AppError::Cancelled)));
    }
}
