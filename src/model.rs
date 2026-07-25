use std::{path::PathBuf, sync::Arc, time::Duration};

use image::DynamicImage;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Snapshot {
    pub id: String,
    pub short_id: String,
    pub time: String,
    pub hostname: String,
    pub username: Option<String>,
    pub paths: Vec<String>,
    pub tags: Vec<String>,
    pub total_bytes: Option<u64>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum FileType {
    File,
    Directory,
    Symlink,
    Other,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileEntry {
    pub name: String,
    pub path: String,
    pub file_type: FileType,
    pub size: u64,
    pub modified: Option<String>,
    pub mode: Option<u32>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub link_target: Option<String>,
}

impl FileEntry {
    pub fn is_dir(&self) -> bool {
        self.file_type == FileType::Directory
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SearchResult {
    pub snapshot_id: String,
    pub snapshot_time: Option<String>,
    pub entry: FileEntry,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct MediaMetadata {
    pub format_name: Option<String>,
    pub duration: Option<f64>,
    pub bit_rate: Option<u64>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
    pub sample_rate: Option<u32>,
    pub channels: Option<u32>,
}

#[derive(Debug)]
pub enum PreviewArtifact {
    Text {
        text: String,
        truncated: bool,
    },
    Image {
        image: Arc<DynamicImage>,
        source: PathBuf,
    },
    VideoFrame {
        image: Arc<DynamicImage>,
        source: PathBuf,
        frame: PathBuf,
        position: Duration,
        metadata: MediaMetadata,
    },
    MetadataOnly {
        reason: String,
        metadata: Option<MediaMetadata>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobKind {
    Directory,
    Search,
    Preview,
    Export,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobStatus {
    Queued,
    Running,
    Completed,
    Cancelled,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    Locked,
    Opening,
    Ready,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionStateMachine {
    state: SessionState,
}

impl Default for SessionStateMachine {
    fn default() -> Self {
        Self {
            state: SessionState::Locked,
        }
    }
}

impl SessionStateMachine {
    pub fn state(self) -> SessionState {
        self.state
    }

    pub fn begin_open(&mut self) -> bool {
        if matches!(self.state, SessionState::Locked | SessionState::Error) {
            self.state = SessionState::Opening;
            true
        } else {
            false
        }
    }

    pub fn opened(&mut self) -> bool {
        if self.state == SessionState::Opening {
            self.state = SessionState::Ready;
            true
        } else {
            false
        }
    }

    pub fn failed(&mut self) {
        self.state = SessionState::Error;
    }

    pub fn lock(&mut self) {
        self.state = SessionState::Locked;
    }
}

#[cfg(test)]
mod tests {
    use super::{SessionState, SessionStateMachine};

    #[test]
    fn session_state_transitions_are_explicit() {
        let mut session = SessionStateMachine::default();
        assert_eq!(session.state(), SessionState::Locked);
        assert!(session.begin_open());
        assert!(!session.begin_open());
        assert!(session.opened());
        assert_eq!(session.state(), SessionState::Ready);
        session.lock();
        assert_eq!(session.state(), SessionState::Locked);
        session.failed();
        assert!(session.begin_open());
    }
}
