use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use tempfile::{Builder, TempDir};

use crate::{AppError, Result};

const DEFAULT_MAX_BYTES: u64 = 512 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct SessionCache {
    inner: Arc<Mutex<CacheInner>>,
}

#[derive(Debug)]
struct CacheInner {
    dir: TempDir,
    entries: VecDeque<CacheEntry>,
    bytes: u64,
    max_bytes: u64,
    sequence: u64,
}

#[derive(Debug)]
struct CacheEntry {
    path: PathBuf,
    bytes: u64,
}

impl SessionCache {
    pub fn new() -> Result<Self> {
        Self::with_limit(DEFAULT_MAX_BYTES)
    }

    pub fn with_limit(max_bytes: u64) -> Result<Self> {
        let dir = Builder::new().prefix("restic-browser-").tempdir()?;
        Ok(Self {
            inner: Arc::new(Mutex::new(CacheInner {
                dir,
                entries: VecDeque::new(),
                bytes: 0,
                max_bytes,
                sequence: 0,
            })),
        })
    }

    pub fn max_bytes(&self) -> u64 {
        self.inner.lock().expect("cache mutex poisoned").max_bytes
    }

    pub fn allocate(&self, suffix: &str) -> Result<PathBuf> {
        let mut inner = self.inner.lock().expect("cache mutex poisoned");
        inner.sequence += 1;
        let suffix = sanitize_suffix(suffix);
        Ok(inner
            .dir
            .path()
            .join(format!("{:016x}{suffix}", inner.sequence)))
    }

    pub fn register(&self, path: PathBuf) -> Result<()> {
        let size = std::fs::metadata(&path)?.len();
        let mut inner = self.inner.lock().expect("cache mutex poisoned");
        inner.bytes = inner.bytes.saturating_add(size);
        inner.entries.push_back(CacheEntry { path, bytes: size });
        while inner.bytes > inner.max_bytes {
            let Some(entry) = inner.entries.pop_front() else {
                break;
            };
            let _ = std::fs::remove_file(&entry.path);
            inner.bytes = inner.bytes.saturating_sub(entry.bytes);
        }
        Ok(())
    }

    pub fn root(&self) -> PathBuf {
        self.inner
            .lock()
            .expect("cache mutex poisoned")
            .dir
            .path()
            .to_path_buf()
    }

    pub fn contains(&self, path: &Path) -> bool {
        path.starts_with(self.root())
    }
}

fn sanitize_suffix(suffix: &str) -> String {
    let value: String = suffix
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
        .collect();
    if value.is_empty() {
        ".bin".to_owned()
    } else if value.starts_with('.') {
        value
    } else {
        format!(".{value}")
    }
}

impl From<std::sync::PoisonError<std::sync::MutexGuard<'_, CacheInner>>> for AppError {
    fn from(_: std::sync::PoisonError<std::sync::MutexGuard<'_, CacheInner>>) -> Self {
        AppError::Other("preview cache lock was poisoned".to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocation_stays_in_cache() {
        let cache = SessionCache::with_limit(1024).unwrap();
        let path = cache.allocate("png").unwrap();
        assert!(cache.contains(&path));
        assert_eq!(path.extension().unwrap(), "png");
    }

    #[test]
    fn evicts_old_files() {
        let cache = SessionCache::with_limit(3).unwrap();
        let first = cache.allocate("bin").unwrap();
        std::fs::write(&first, b"123").unwrap();
        cache.register(first.clone()).unwrap();
        let second = cache.allocate("bin").unwrap();
        std::fs::write(&second, b"4").unwrap();
        cache.register(second).unwrap();
        assert!(!first.exists());
    }
}
