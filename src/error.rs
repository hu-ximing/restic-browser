use std::{io, path::PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("authentication failed")]
    Authentication,
    #[error("repository was not found or is not a restic repository")]
    RepositoryNotFound,
    #[error("restic repository format is not supported by this restic version")]
    RepositoryFormat,
    #[error("required dependency is unavailable: {0}")]
    DependencyMissing(String),
    #[error("unsupported restic version: {0}; restic 0.19.x is required")]
    UnsupportedResticVersion(String),
    #[error("operation was cancelled")]
    Cancelled,
    #[error("the selected file is too large to preview ({size} bytes; limit {limit} bytes)")]
    PreviewTooLarge { size: u64, limit: u64 },
    #[error("destination already exists: {0}")]
    DestinationExists(PathBuf),
    #[error("invalid repository response: {0}")]
    InvalidResponse(String),
    #[error("external command failed: {program}: {message}")]
    CommandFailed { program: String, message: String },
    #[error("invalid path: {0}")]
    InvalidPath(String),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Other(String),
}

impl AppError {
    pub fn classify_stderr(program: &str, stderr: &[u8]) -> Self {
        let raw = String::from_utf8_lossy(stderr);
        let lower = raw.to_ascii_lowercase();
        if lower.contains("wrong password")
            || lower.contains("no key found")
            || lower.contains("unable to open config file: stat")
                && lower.contains("incorrect password")
        {
            return Self::Authentication;
        }
        if lower.contains("is there a repository at the following location")
            || lower.contains("repository does not exist")
            || lower.contains("unable to open config file")
        {
            return Self::RepositoryNotFound;
        }
        if lower.contains("repository version") && lower.contains("not supported") {
            return Self::RepositoryFormat;
        }
        Self::CommandFailed {
            program: program.to_owned(),
            message: redact(&raw),
        }
    }
}

pub fn redact(input: &str) -> String {
    input
        .lines()
        .map(|line| {
            let lower = line.to_ascii_lowercase();
            if lower.contains("password")
                || lower.contains("secret")
                || lower.contains("access_key")
                || lower.contains("token")
            {
                "[redacted]"
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub type Result<T> = std::result::Result<T, AppError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_secret_lines() {
        let value = "normal\nRESTIC_PASSWORD=hunter2\nnext";
        assert_eq!(redact(value), "normal\n[redacted]\nnext");
    }
}
