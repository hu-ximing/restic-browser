use std::{
    collections::{HashMap, HashSet},
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
};

use secrecy::{ExposeSecret, SecretString};
use serde_json::Value;
use tokio::process::{Child, Command};
use tokio_util::sync::CancellationToken;

use crate::{
    AppError, Result,
    model::{FileEntry, FileType, FileVersion, SearchResult, Snapshot},
    process::read_limited,
    repository::RepositoryReader,
};

const MAX_CAPTURE_STDOUT_BYTES: usize = 128 * 1024 * 1024;
const MAX_CAPTURE_STDERR_BYTES: usize = 4 * 1024 * 1024;
const MAX_DIRECTORY_ENTRIES: usize = 100_000;
const MAX_SEARCH_RESULTS: usize = 100_000;
const MAX_FILE_VERSIONS: usize = 100_000;

#[derive(Debug, Clone)]
pub struct ResticCliClient {
    executable: PathBuf,
    repository: PathBuf,
    password: Arc<SecretString>,
    cache_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy)]
enum Operation {
    Snapshots,
    List,
    Find,
    Dump,
    Restore,
}

impl Operation {
    fn command(self) -> Option<&'static str> {
        match self {
            Self::Snapshots => Some("snapshots"),
            Self::List => Some("ls"),
            Self::Find => Some("find"),
            Self::Dump => Some("dump"),
            Self::Restore => Some("restore"),
        }
    }
}

impl ResticCliClient {
    pub fn new(
        executable: impl Into<PathBuf>,
        repository: impl Into<PathBuf>,
        password: SecretString,
    ) -> Result<Self> {
        let repository = repository.into();
        if !repository.exists() {
            return Err(AppError::RepositoryNotFound);
        }
        Ok(Self {
            executable: executable.into(),
            repository,
            password: Arc::new(password),
            cache_dir: None,
        })
    }

    pub fn with_cache_dir(mut self, cache_dir: impl Into<PathBuf>) -> Self {
        self.cache_dir = Some(cache_dir.into());
        self
    }

    pub fn executable(&self) -> &Path {
        &self.executable
    }

    pub fn repository(&self) -> &Path {
        &self.repository
    }

    pub async fn check_version(executable: &Path) -> Result<String> {
        let output = Command::new(executable)
            .arg("version")
            .stdin(Stdio::null())
            .output()
            .await
            .map_err(|error| dependency_error(executable, error))?;
        if !output.status.success() {
            return Err(AppError::classify_stderr(
                &executable.display().to_string(),
                &output.stderr,
            ));
        }
        let version = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if !version.starts_with("restic 0.19.") {
            return Err(AppError::UnsupportedResticVersion(version));
        }
        Ok(version)
    }

    pub async fn list_snapshots(&self, token: CancellationToken) -> Result<Vec<Snapshot>> {
        let stdout = self
            .run_capture(Operation::Snapshots, ["--json"], token)
            .await?;
        let values: Vec<Value> = serde_json::from_slice(&stdout)?;
        let mut snapshots = values
            .iter()
            .map(parse_snapshot)
            .collect::<Result<Vec<_>>>()?;
        snapshots.sort_by(|left, right| right.time.cmp(&left.time));
        Ok(snapshots)
    }

    pub async fn list_directory(
        &self,
        snapshot: &str,
        path: &str,
        token: CancellationToken,
    ) -> Result<Vec<FileEntry>> {
        validate_snapshot_id(snapshot)?;
        let path = normalize_repo_path(path)?;
        let args = [
            OsString::from("--json"),
            OsString::from(snapshot),
            OsString::from(&path),
        ];
        let stdout = self.run_capture(Operation::List, args, token).await?;
        let mut entries = parse_json_lines(&stdout)?
            .into_iter()
            .filter_map(|value| parse_file_entry(&value).transpose())
            .collect::<Result<Vec<_>>>()?;
        if entries.len() > MAX_DIRECTORY_ENTRIES {
            return Err(AppError::InvalidResponse(format!(
                "directory contains more than {MAX_DIRECTORY_ENTRIES} entries"
            )));
        }

        entries.retain(|entry| {
            entry.path != path
                && parent_repo_path(&entry.path)
                    .map(|parent| parent == path)
                    .unwrap_or(false)
        });
        entries.sort_by(|left, right| {
            right
                .is_dir()
                .cmp(&left.is_dir())
                .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
        });
        Ok(entries)
    }

    pub async fn find(
        &self,
        snapshot: &str,
        pattern: &str,
        token: CancellationToken,
    ) -> Result<Vec<SearchResult>> {
        validate_snapshot_id(snapshot)?;
        if pattern.trim().is_empty() {
            return Err(AppError::InvalidPath(
                "search pattern cannot be empty".to_owned(),
            ));
        }
        let args = [
            OsString::from("--json"),
            OsString::from("--snapshot"),
            OsString::from(snapshot),
            OsString::from(pattern),
        ];
        let stdout = self.run_capture(Operation::Find, args, token).await?;
        parse_find_output(snapshot, &stdout)
    }

    pub async fn list_file_versions(
        &self,
        snapshots: Vec<Snapshot>,
        path: String,
        token: CancellationToken,
    ) -> Result<Vec<FileVersion>> {
        let path = normalize_repo_path(&path)?;
        if path == "/" {
            return Ok(Vec::new());
        }
        let pattern = literal_find_pattern(&path);
        let parse_token = token.clone();
        let stdout = self
            .run_capture(
                Operation::Find,
                [OsString::from("--json"), OsString::from(pattern)],
                token,
            )
            .await?;
        let matches = parse_find_output("", &stdout)?;
        let snapshots = snapshots
            .into_iter()
            .enumerate()
            .map(|(index, snapshot)| (snapshot.id.clone(), (index, snapshot)))
            .collect::<HashMap<_, _>>();
        let mut seen = HashSet::new();
        let mut versions = Vec::new();
        for result in matches {
            if parse_token.is_cancelled() {
                return Err(AppError::Cancelled);
            }
            if result.entry.path != path || !seen.insert(result.snapshot_id.clone()) {
                continue;
            }
            let Some((order, snapshot)) = snapshots.get(&result.snapshot_id) else {
                continue;
            };
            if versions.len() == MAX_FILE_VERSIONS {
                return Err(AppError::InvalidResponse(format!(
                    "file has more than {MAX_FILE_VERSIONS} snapshot records"
                )));
            }
            versions.push((
                *order,
                FileVersion {
                    snapshot: snapshot.clone(),
                    entry: result.entry,
                },
            ));
        }
        versions.sort_by_key(|(order, _)| *order);
        Ok(versions.into_iter().map(|(_, version)| version).collect())
    }

    pub async fn dump_to_path(
        &self,
        snapshot: &str,
        source: &str,
        destination: &Path,
        token: CancellationToken,
    ) -> Result<()> {
        validate_snapshot_id(snapshot)?;
        let source = normalize_repo_path(source)?;
        let file = std::fs::File::create(destination)?;
        let args = [OsString::from(snapshot), OsString::from(source)];
        let mut command = self.command(Operation::Dump);
        command
            .args(args)
            .stdout(Stdio::from(file))
            .stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .map_err(|error| dependency_error(&self.executable, error))?;
        let stderr = child.stderr.take().expect("stderr was piped");
        let stderr_task = tokio::spawn(read_limited(
            stderr,
            MAX_CAPTURE_STDERR_BYTES,
            "restic".to_owned(),
            "stderr",
        ));
        let status = wait_or_cancel(&mut child, token).await?;
        let stderr = stderr_task
            .await
            .map_err(|error| AppError::Other(format!("stderr reader failed: {error}")))??;
        if !status.success() {
            let _ = std::fs::remove_file(destination);
            return Err(AppError::classify_stderr("restic", &stderr));
        }
        Ok(())
    }

    pub async fn restore_directory(
        &self,
        snapshot: &str,
        source: &str,
        destination: &Path,
        token: CancellationToken,
    ) -> Result<PathBuf> {
        validate_snapshot_id(snapshot)?;
        let source = normalize_repo_path(source)?;
        if token.is_cancelled() {
            return Err(AppError::Cancelled);
        }
        if destination.exists() {
            return Err(AppError::DestinationExists(destination.to_path_buf()));
        }
        let parent = destination
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        if !parent.is_dir() {
            return Err(AppError::InvalidPath(format!(
                "restore parent directory does not exist: {}",
                parent.display()
            )));
        }
        Self::check_version(&self.executable).await?;
        if token.is_cancelled() {
            return Err(AppError::Cancelled);
        }
        std::fs::create_dir(destination).map_err(|error| {
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                AppError::DestinationExists(destination.to_path_buf())
            } else {
                AppError::Io(error)
            }
        })?;

        let snapshot_source = format!("{snapshot}:{source}");
        let args = [
            OsString::from("--overwrite"),
            OsString::from("never"),
            OsString::from("--target"),
            destination.as_os_str().to_owned(),
            OsString::from(snapshot_source),
        ];
        let result = self
            .run_capture(Operation::Restore, args, token)
            .await
            .map(|_| destination.to_path_buf());
        if let Err(error) = result {
            return match std::fs::remove_dir_all(destination) {
                Ok(()) => Err(error),
                Err(cleanup_error) => Err(AppError::Other(format!(
                    "{error}; failed to clean partial restore at {}: {cleanup_error}",
                    destination.display()
                ))),
            };
        }
        result
    }

    async fn run_capture<I, S>(
        &self,
        operation: Operation,
        args: I,
        token: CancellationToken,
    ) -> Result<Vec<u8>>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut command = self.command(operation);
        command
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .map_err(|error| dependency_error(&self.executable, error))?;
        let stdout = child.stdout.take().expect("stdout was piped");
        let stderr = child.stderr.take().expect("stderr was piped");
        let stdout_task = tokio::spawn(read_limited(
            stdout,
            MAX_CAPTURE_STDOUT_BYTES,
            "restic".to_owned(),
            "stdout",
        ));
        let stderr_task = tokio::spawn(read_limited(
            stderr,
            MAX_CAPTURE_STDERR_BYTES,
            "restic".to_owned(),
            "stderr",
        ));
        let status = wait_or_cancel(&mut child, token).await?;
        let stdout = stdout_task
            .await
            .map_err(|error| AppError::Other(format!("stdout reader failed: {error}")))??;
        let stderr = stderr_task
            .await
            .map_err(|error| AppError::Other(format!("stderr reader failed: {error}")))??;
        if !status.success() {
            return Err(AppError::classify_stderr("restic", &stderr));
        }
        Ok(stdout)
    }

    fn command(&self, operation: Operation) -> Command {
        let mut command = Command::new(&self.executable);
        command
            .arg("--repo")
            .arg(&self.repository)
            .env("RESTIC_PASSWORD", self.password.expose_secret())
            .env("RESTIC_PROGRESS_FPS", "0")
            .stdin(Stdio::null())
            .kill_on_drop(true);
        if let Some(cache_dir) = &self.cache_dir {
            command.arg("--cache-dir").arg(cache_dir);
        }
        if let Some(subcommand) = operation.command() {
            command.arg(subcommand);
        }
        command
    }
}

impl RepositoryReader for ResticCliClient {
    fn list_snapshots(
        &self,
        token: CancellationToken,
    ) -> futures_util::future::BoxFuture<'_, Result<Vec<Snapshot>>> {
        Box::pin(ResticCliClient::list_snapshots(self, token))
    }

    fn list_directory(
        &self,
        snapshot: &str,
        path: &str,
        token: CancellationToken,
    ) -> futures_util::future::BoxFuture<'_, Result<Vec<FileEntry>>> {
        let snapshot = snapshot.to_owned();
        let path = path.to_owned();
        Box::pin(
            async move { ResticCliClient::list_directory(self, &snapshot, &path, token).await },
        )
    }

    fn find(
        &self,
        snapshot: &str,
        pattern: &str,
        token: CancellationToken,
    ) -> futures_util::future::BoxFuture<'_, Result<Vec<SearchResult>>> {
        let snapshot = snapshot.to_owned();
        let pattern = pattern.to_owned();
        Box::pin(async move { ResticCliClient::find(self, &snapshot, &pattern, token).await })
    }

    fn list_file_versions(
        &self,
        snapshots: Vec<Snapshot>,
        path: String,
        token: CancellationToken,
    ) -> futures_util::future::BoxFuture<'_, Result<Vec<FileVersion>>> {
        Box::pin(ResticCliClient::list_file_versions(
            self, snapshots, path, token,
        ))
    }

    fn dump_to_path(
        &self,
        snapshot: &str,
        source: &str,
        destination: &Path,
        token: CancellationToken,
    ) -> futures_util::future::BoxFuture<'_, Result<()>> {
        let snapshot = snapshot.to_owned();
        let source = source.to_owned();
        let destination = destination.to_path_buf();
        Box::pin(async move {
            ResticCliClient::dump_to_path(self, &snapshot, &source, &destination, token).await
        })
    }
}

async fn wait_or_cancel(
    child: &mut Child,
    token: CancellationToken,
) -> Result<std::process::ExitStatus> {
    tokio::select! {
        status = child.wait() => Ok(status?),
        _ = token.cancelled() => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            Err(AppError::Cancelled)
        }
    }
}

fn dependency_error(path: &Path, error: std::io::Error) -> AppError {
    if error.kind() == std::io::ErrorKind::NotFound {
        AppError::DependencyMissing(path.display().to_string())
    } else {
        AppError::Io(error)
    }
}

pub(crate) fn parse_snapshot(value: &Value) -> Result<Snapshot> {
    let id = string(value, "id")?;
    Ok(Snapshot {
        short_id: value
            .get("short_id")
            .and_then(Value::as_str)
            .unwrap_or_else(|| id.get(..8).unwrap_or(&id))
            .to_owned(),
        id,
        time: string(value, "time")?,
        hostname: value
            .get("hostname")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_owned(),
        username: value
            .get("username")
            .and_then(Value::as_str)
            .map(str::to_owned),
        paths: string_array(value.get("paths")),
        tags: string_array(value.get("tags")),
        total_bytes: value
            .pointer("/summary/total_bytes_processed")
            .and_then(Value::as_u64),
    })
}

fn parse_json_lines(bytes: &[u8]) -> Result<Vec<Value>> {
    let text =
        std::str::from_utf8(bytes).map_err(|error| AppError::InvalidResponse(error.to_string()))?;
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).map_err(AppError::from))
        .collect()
}

pub(crate) fn parse_file_entry(value: &Value) -> Result<Option<FileEntry>> {
    let object_type = value
        .get("struct_type")
        .or_else(|| value.get("object_type"))
        .or_else(|| value.get("type"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    if matches!(object_type, "snapshot" | "message" | "summary") {
        return Ok(None);
    }
    let Some(path) = value.get("path").and_then(Value::as_str) else {
        return Ok(None);
    };
    let name = value
        .get("name")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| {
            path.trim_end_matches('/')
                .rsplit('/')
                .next()
                .unwrap_or(path)
                .to_owned()
        });
    let kind = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or(object_type);
    let file_type = match kind {
        "dir" | "directory" => FileType::Directory,
        "file" => FileType::File,
        "symlink" => FileType::Symlink,
        _ => FileType::Other,
    };
    Ok(Some(FileEntry {
        name,
        path: normalize_repo_path(path)?,
        file_type,
        size: value.get("size").and_then(Value::as_u64).unwrap_or(0),
        modified: value
            .get("mtime")
            .or_else(|| value.get("mod_time"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        mode: value
            .get("mode")
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok()),
        uid: value
            .get("uid")
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok()),
        gid: value
            .get("gid")
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok()),
        link_target: value
            .get("linktarget")
            .or_else(|| value.get("link_target"))
            .and_then(Value::as_str)
            .map(str::to_owned),
    }))
}

fn parse_find_output(snapshot: &str, bytes: &[u8]) -> Result<Vec<SearchResult>> {
    let values = parse_json_lines(bytes).or_else(|_| {
        serde_json::from_slice::<Value>(bytes).map(|value| match value {
            Value::Array(values) => values,
            value => vec![value],
        })
    })?;
    let mut results = Vec::new();
    for value in values {
        collect_find_results(snapshot, None, &value, &mut results)?;
    }
    Ok(results)
}

fn collect_find_results(
    default_snapshot: &str,
    inherited_time: Option<&str>,
    value: &Value,
    output: &mut Vec<SearchResult>,
) -> Result<()> {
    match value {
        Value::Array(values) => {
            for value in values {
                collect_find_results(default_snapshot, inherited_time, value, output)?;
            }
        }
        Value::Object(map) => {
            let snapshot_id = map
                .get("snapshot_id")
                .or_else(|| map.get("snapshot"))
                .or_else(|| map.get("id"))
                .and_then(Value::as_str)
                .unwrap_or(default_snapshot);
            let snapshot_time = map
                .get("snapshot_time")
                .or_else(|| map.get("time"))
                .and_then(Value::as_str)
                .or(inherited_time);
            if let Some(matches) = map.get("matches") {
                collect_find_results(snapshot_id, snapshot_time, matches, output)?;
            } else if let Some(entry) = parse_file_entry(value)? {
                if output.len() == MAX_SEARCH_RESULTS {
                    return Err(AppError::InvalidResponse(format!(
                        "search returned more than {MAX_SEARCH_RESULTS} results"
                    )));
                }
                output.push(SearchResult {
                    snapshot_id: snapshot_id.to_owned(),
                    snapshot_time: snapshot_time.map(str::to_owned),
                    entry,
                });
            }
        }
        _ => {}
    }
    Ok(())
}

fn literal_find_pattern(path: &str) -> String {
    path.chars()
        .flat_map(|character| match character {
            '*' => "[*]".chars().collect::<Vec<_>>(),
            '?' => "[?]".chars().collect(),
            '[' => "[[]".chars().collect(),
            character => vec![character],
        })
        .collect()
}

fn string(value: &Value, key: &str) -> Result<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| AppError::InvalidResponse(format!("missing string field {key}")))
}

fn string_array(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

pub fn normalize_repo_path(path: &str) -> Result<String> {
    if path.contains('\0') {
        return Err(AppError::InvalidPath("path contains NUL".to_owned()));
    }
    let value = path.replace('\\', "/");
    let mut components = Vec::new();
    for component in value.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                components.pop();
            }
            component => components.push(component),
        }
    }
    Ok(if components.is_empty() {
        "/".to_owned()
    } else {
        format!("/{}", components.join("/"))
    })
}

pub fn parent_repo_path(path: &str) -> Option<String> {
    let path = normalize_repo_path(path).ok()?;
    if path == "/" {
        return None;
    }
    let parent = path
        .rsplit_once('/')
        .map(|(parent, _)| parent)
        .unwrap_or("");
    Some(if parent.is_empty() {
        "/".to_owned()
    } else {
        parent.to_owned()
    })
}

fn validate_snapshot_id(snapshot: &str) -> Result<()> {
    if snapshot.is_empty()
        || !snapshot
            .chars()
            .all(|character| character.is_ascii_hexdigit())
    {
        return Err(AppError::InvalidPath(
            "invalid snapshot identifier".to_owned(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_repository_paths() {
        assert_eq!(
            normalize_repo_path(r"\home\me\..\docs").unwrap(),
            "/home/docs"
        );
        assert_eq!(normalize_repo_path("/").unwrap(), "/");
    }

    #[test]
    fn computes_parent_paths() {
        assert_eq!(parent_repo_path("/home/file"), Some("/home".to_owned()));
        assert_eq!(parent_repo_path("/home"), Some("/".to_owned()));
        assert_eq!(parent_repo_path("/"), None);
    }

    #[test]
    fn parses_snapshot_json() {
        let value = serde_json::json!({
            "id": "1234567890abcdef",
            "short_id": "12345678",
            "time": "2026-07-22T00:00:00Z",
            "hostname": "host",
            "paths": ["/home"],
            "tags": ["daily"],
            "summary": {"total_bytes_processed": 42}
        });
        let snapshot = parse_snapshot(&value).unwrap();
        assert_eq!(snapshot.short_id, "12345678");
        assert_eq!(snapshot.total_bytes, Some(42));
    }

    #[test]
    fn parses_ls_node() {
        let value = serde_json::json!({
            "struct_type": "node",
            "name": "hello.txt",
            "type": "file",
            "path": "/home/hello.txt",
            "size": 5,
            "mtime": "2026-07-22T00:00:00Z"
        });
        let entry = parse_file_entry(&value).unwrap().unwrap();
        assert_eq!(entry.name, "hello.txt");
        assert_eq!(entry.file_type, FileType::File);
    }

    #[test]
    fn escapes_literal_paths_for_find() {
        assert_eq!(
            literal_find_pattern("/notes/[draft]*?.txt"),
            "/notes/[[]draft][*][?].txt"
        );
    }

    #[test]
    fn parses_find_results_grouped_by_snapshot() {
        let snapshot = "a".repeat(64);
        let output = serde_json::to_vec(&serde_json::json!([{
            "snapshot": snapshot,
            "matches": [{
                "path": "/notes/[draft]*?.txt",
                "name": "[draft]*?.txt",
                "type": "file",
                "size": 42,
                "mtime": "2026-07-22T00:00:00Z"
            }]
        }]))
        .unwrap();

        let results = parse_find_output("", &output).unwrap();

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].snapshot_id, "a".repeat(64));
        assert_eq!(results[0].entry.path, "/notes/[draft]*?.txt");
    }

    #[test]
    fn production_operation_whitelist_is_bounded() {
        let commands = [
            Operation::Snapshots.command(),
            Operation::List.command(),
            Operation::Find.command(),
            Operation::Dump.command(),
            Operation::Restore.command(),
        ];
        assert_eq!(
            commands,
            [
                Some("snapshots"),
                Some("ls"),
                Some("find"),
                Some("dump"),
                Some("restore")
            ]
        );
    }
}
