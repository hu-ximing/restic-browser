use std::{path::PathBuf, sync::Arc, time::Duration};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::Line,
    widgets::{
        Block, Borders, Clear, List, ListItem, ListState, Paragraph, Row, Table, TableState, Wrap,
    },
};
use ratatui_image::{Resize, StatefulImage, picker::Picker, protocol::StatefulProtocol};

use crate::{
    AppError,
    export::ExportService,
    jobs::JobHandle,
    model::{FileEntry, PreviewArtifact, SearchResult, Snapshot},
    preview::PreviewService,
    repository::RepositoryHandle,
};

#[derive(Clone, Copy, Eq, PartialEq)]
enum Focus {
    Snapshots,
    Files,
}

enum InputMode {
    Normal,
    Search(String),
    Export(String),
}

enum ActiveJob {
    Directory(JobHandle<Vec<FileEntry>>),
    Search(JobHandle<Vec<SearchResult>>),
    Preview(JobHandle<PreviewArtifact>),
    Export(JobHandle<PathBuf>),
}

impl ActiveJob {
    fn is_finished(&self) -> bool {
        match self {
            Self::Directory(job) => job.is_finished(),
            Self::Search(job) => job.is_finished(),
            Self::Preview(job) => job.is_finished(),
            Self::Export(job) => job.is_finished(),
        }
    }

    fn cancel(&self) {
        match self {
            Self::Directory(job) => job.cancel(),
            Self::Search(job) => job.cancel(),
            Self::Preview(job) => job.cancel(),
            Self::Export(job) => job.cancel(),
        }
    }
}

pub struct App {
    repository: RepositoryHandle,
    preview_service: Arc<PreviewService>,
    export_service: ExportService,
    snapshots: Vec<Snapshot>,
    entries: Vec<FileEntry>,
    snapshot_index: usize,
    entry_index: usize,
    current_path: String,
    focus: Focus,
    input_mode: InputMode,
    status: String,
    active_job: Option<ActiveJob>,
    preview: Option<PreviewArtifact>,
    image_protocol: Option<StatefulProtocol>,
    picker: Picker,
    video_position: Duration,
    should_quit: bool,
}

impl App {
    pub fn new(
        repository: RepositoryHandle,
        preview_service: Arc<PreviewService>,
        snapshots: Vec<Snapshot>,
    ) -> Self {
        let picker = if cfg!(test) {
            Picker::halfblocks()
        } else {
            Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks())
        };
        let mut app = Self {
            repository,
            preview_service,
            export_service: ExportService,
            snapshots,
            entries: Vec::new(),
            snapshot_index: 0,
            entry_index: 0,
            current_path: "/".to_owned(),
            focus: Focus::Snapshots,
            input_mode: InputMode::Normal,
            status: "就绪".to_owned(),
            active_job: None,
            preview: None,
            image_protocol: None,
            picker,
            video_position: Duration::ZERO,
            should_quit: false,
        };
        app.load_directory("/".to_owned());
        app
    }

    pub fn should_quit(&self) -> bool {
        self.should_quit
    }

    pub fn draw(&mut self, frame: &mut Frame<'_>) {
        render_app(frame, self);
    }

    pub async fn tick(&mut self) {
        if !self.active_job.as_ref().is_some_and(ActiveJob::is_finished) {
            return;
        }
        let Some(job) = self.active_job.take() else {
            return;
        };
        match job {
            ActiveJob::Directory(job) => match job.finish().await {
                Ok(entries) => {
                    self.entries = entries;
                    self.entry_index = 0;
                    self.status = format!("已加载 {} 个项目", self.entries.len());
                }
                Err(error) => self.show_error(error),
            },
            ActiveJob::Search(job) => match job.finish().await {
                Ok(results) => {
                    self.entries = results.into_iter().map(|result| result.entry).collect();
                    self.entry_index = 0;
                    self.status = format!("找到 {} 个项目", self.entries.len());
                }
                Err(error) => self.show_error(error),
            },
            ActiveJob::Preview(job) => match job.finish().await {
                Ok(preview) => {
                    self.set_preview(preview);
                    self.status = "预览已就绪".to_owned();
                }
                Err(error) => self.show_error(error),
            },
            ActiveJob::Export(job) => match job.finish().await {
                Ok(path) => self.status = format!("已导出到 {}", path.display()),
                Err(error) => self.show_error(error),
            },
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.should_quit = true;
            return;
        }
        match &mut self.input_mode {
            InputMode::Search(buffer) => match key.code {
                KeyCode::Esc => self.input_mode = InputMode::Normal,
                KeyCode::Enter => {
                    let pattern = std::mem::take(buffer);
                    self.input_mode = InputMode::Normal;
                    self.start_search(pattern);
                }
                KeyCode::Backspace => {
                    buffer.pop();
                }
                KeyCode::Char(character) => buffer.push(character),
                _ => {}
            },
            InputMode::Export(buffer) => match key.code {
                KeyCode::Esc => self.input_mode = InputMode::Normal,
                KeyCode::Enter => {
                    let destination = std::mem::take(buffer);
                    self.input_mode = InputMode::Normal;
                    self.start_export(PathBuf::from(destination));
                }
                KeyCode::Backspace => {
                    buffer.pop();
                }
                KeyCode::Char(character) => buffer.push(character),
                _ => {}
            },
            InputMode::Normal => self.handle_normal_key(key),
        }
    }

    pub fn cancel_active_job(&mut self) {
        if let Some(job) = &self.active_job {
            job.cancel();
            self.status = "正在取消…".to_owned();
        }
    }

    fn handle_normal_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('q') => {
                self.cancel_active_job();
                self.should_quit = true;
            }
            KeyCode::Esc => self.cancel_active_job(),
            KeyCode::Tab => {
                self.focus = match self.focus {
                    Focus::Snapshots => Focus::Files,
                    Focus::Files => Focus::Snapshots,
                };
            }
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::Enter => self.activate_selection(),
            KeyCode::Backspace | KeyCode::Left => self.go_parent(),
            KeyCode::Char('/') => self.input_mode = InputMode::Search(String::new()),
            KeyCode::Char('p') => self.start_preview(),
            KeyCode::Char('e') => {
                if let Some(entry) = self.selected_file() {
                    self.input_mode = InputMode::Export(entry.name.clone());
                }
            }
            KeyCode::Char('r') => self.load_directory(self.current_path.clone()),
            KeyCode::Right => self.enter_selection(),
            _ => {}
        }
    }

    fn move_selection(&mut self, delta: isize) {
        let (index, length) = match self.focus {
            Focus::Snapshots => (&mut self.snapshot_index, self.snapshots.len()),
            Focus::Files => (&mut self.entry_index, self.entries.len()),
        };
        if length > 0 {
            *index = (*index as isize + delta).clamp(0, length.saturating_sub(1) as isize) as usize;
        }
    }

    fn activate_selection(&mut self) {
        match self.focus {
            Focus::Snapshots => {
                self.current_path = "/".to_owned();
                self.clear_preview();
                self.load_directory("/".to_owned());
                self.focus = Focus::Files;
            }
            Focus::Files => {
                let Some(entry) = self.entries.get(self.entry_index).cloned() else {
                    return;
                };
                if entry.is_dir() {
                    self.current_path = entry.path.clone();
                    self.load_directory(entry.path);
                } else {
                    self.start_preview();
                }
            }
        }
    }

    fn enter_selection(&mut self) {
        if self.focus == Focus::Snapshots || self.selected_file().is_some_and(FileEntry::is_dir) {
            self.activate_selection();
        }
    }

    fn go_parent(&mut self) {
        if self.focus != Focus::Files {
            return;
        }
        if self.current_path == "/" {
            self.focus = Focus::Snapshots;
            return;
        }
        let trimmed = self.current_path.trim_end_matches('/');
        let parent = trimmed
            .rsplit_once('/')
            .map(|(parent, _)| if parent.is_empty() { "/" } else { parent })
            .unwrap_or("/")
            .to_owned();
        self.current_path = parent.clone();
        self.load_directory(parent);
    }

    fn load_directory(&mut self, path: String) {
        let Some(snapshot) = self.selected_snapshot().cloned() else {
            self.status = "仓库中没有快照".to_owned();
            return;
        };
        self.replace_job();
        self.status = format!("正在加载 {path}…");
        let client = Arc::clone(&self.repository);
        self.active_job = Some(ActiveJob::Directory(JobHandle::spawn_cancellable(
            move |token| async move { client.list_directory(&snapshot.id, &path, token).await },
        )));
    }

    fn start_search(&mut self, pattern: String) {
        if pattern.trim().is_empty() {
            self.status = "搜索模式不能为空".to_owned();
            return;
        }
        let Some(snapshot) = self.selected_snapshot().cloned() else {
            return;
        };
        self.replace_job();
        self.clear_preview();
        self.status = format!("正在搜索 {pattern}…");
        let client = Arc::clone(&self.repository);
        self.active_job = Some(ActiveJob::Search(JobHandle::spawn_cancellable(
            move |token| async move { client.find(&snapshot.id, &pattern, token).await },
        )));
    }

    fn start_preview(&mut self) {
        let Some(snapshot) = self.selected_snapshot().cloned() else {
            return;
        };
        let Some(entry) = self.selected_file().cloned() else {
            return;
        };
        if entry.is_dir() {
            self.status = "只能预览文件".to_owned();
            return;
        }
        self.replace_job();
        self.status = if self.repository.content_index_ready() {
            format!("正在预览 {}…", entry.name)
        } else {
            format!("首次读取：正在建立文件索引并预览 {}…", entry.name)
        };
        let service = Arc::clone(&self.preview_service);
        let client = Arc::clone(&self.repository);
        let position = self.video_position;
        self.active_job = Some(ActiveJob::Preview(JobHandle::spawn_cancellable(
            move |token| async move {
                service
                    .preview(client, &snapshot.id, &entry, position, token)
                    .await
            },
        )));
    }

    fn start_export(&mut self, destination: PathBuf) {
        let Some(snapshot) = self.selected_snapshot().cloned() else {
            return;
        };
        let Some(entry) = self.selected_file().cloned() else {
            return;
        };
        if destination.as_os_str().is_empty() {
            self.status = "导出目标不能为空".to_owned();
            return;
        }
        self.replace_job();
        self.status = if self.repository.content_index_ready() {
            format!("正在导出 {}…", entry.name)
        } else {
            format!("首次读取：正在建立文件索引并导出 {}…", entry.name)
        };
        let client = Arc::clone(&self.repository);
        let service = self.export_service.clone();
        self.active_job = Some(ActiveJob::Export(JobHandle::spawn_cancellable(
            move |token| async move {
                service
                    .export_file(client, &snapshot.id, &entry.path, &destination, token)
                    .await
            },
        )));
    }

    fn replace_job(&mut self) {
        if let Some(job) = self.active_job.take() {
            job.cancel();
        }
    }

    fn selected_snapshot(&self) -> Option<&Snapshot> {
        self.snapshots.get(self.snapshot_index)
    }

    fn selected_file(&self) -> Option<&FileEntry> {
        self.entries.get(self.entry_index)
    }

    fn set_preview(&mut self, preview: PreviewArtifact) {
        self.image_protocol = match &preview {
            PreviewArtifact::Image { image, .. } | PreviewArtifact::VideoFrame { image, .. } => {
                Some(self.picker.new_resize_protocol((**image).clone()))
            }
            _ => None,
        };
        self.preview = Some(preview);
    }

    fn clear_preview(&mut self) {
        self.preview = None;
        self.image_protocol = None;
        self.video_position = Duration::ZERO;
    }

    fn show_error(&mut self, error: AppError) {
        self.status = format!("错误：{error}");
    }
}

fn render_app(frame: &mut Frame<'_>, app: &mut App) {
    let sections = Layout::vertical([
        Constraint::Length(1),
        Constraint::Percentage(58),
        Constraint::Min(7),
        Constraint::Length(2),
    ])
    .split(frame.area());
    frame.render_widget(
        Paragraph::new(Line::from(format!(" restic-browser  {}", app.status))),
        sections[0],
    );
    let browser = Layout::horizontal([Constraint::Percentage(32), Constraint::Percentage(68)])
        .split(sections[1]);
    render_snapshots(frame, app, browser[0]);
    render_files(frame, app, browser[1]);
    render_preview(frame, app, sections[2]);
    frame.render_widget(
        Paragraph::new(
            " Tab 切换  Enter 打开  ← 返回  → 进入  / 搜索  p 预览  e 导出  r 刷新  q 退出",
        )
        .style(Style::default().fg(Color::DarkGray)),
        sections[3],
    );
    match &app.input_mode {
        InputMode::Search(buffer) => render_prompt(frame, "搜索 restic pattern", buffer),
        InputMode::Export(buffer) => render_prompt(frame, "导出目标（不覆盖）", buffer),
        InputMode::Normal => {}
    }
}

fn render_snapshots(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let items = app.snapshots.iter().map(|snapshot| {
        ListItem::new(vec![
            Line::from(snapshot.time.clone()),
            Line::styled(
                format!("{}  {}", snapshot.short_id, snapshot.hostname),
                Style::default().fg(Color::DarkGray),
            ),
        ])
    });
    let border = if app.focus == Focus::Snapshots {
        Color::Cyan
    } else {
        Color::DarkGray
    };
    let list = List::new(items)
        .block(
            Block::default()
                .title(" 快照 ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(border)),
        )
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("› ");
    let mut state = ListState::default()
        .with_selected((!app.snapshots.is_empty()).then_some(app.snapshot_index));
    frame.render_stateful_widget(list, area, &mut state);
}

fn render_files(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let rows = app.entries.iter().map(|entry| {
        Row::new([
            if entry.is_dir() { "DIR" } else { "FILE" }.to_owned(),
            entry.name.clone(),
            format_size(entry.size),
        ])
    });
    let border = if app.focus == Focus::Files {
        Color::Cyan
    } else {
        Color::DarkGray
    };
    let table = Table::new(
        rows,
        [
            Constraint::Length(5),
            Constraint::Min(12),
            Constraint::Length(10),
        ],
    )
    .header(Row::new(["类型", "名称", "大小"]).style(Style::default().fg(Color::Yellow)))
    .block(
        Block::default()
            .title(format!(" {} ", app.current_path))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border)),
    )
    .row_highlight_style(Style::default().bg(Color::DarkGray))
    .highlight_symbol("› ");
    let mut state =
        TableState::default().with_selected((!app.entries.is_empty()).then_some(app.entry_index));
    frame.render_stateful_widget(table, area, &mut state);
}

fn render_preview(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let block = Block::default()
        .title(" 预览 / 元数据 ")
        .borders(Borders::ALL);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if let Some(protocol) = &mut app.image_protocol {
        frame.render_stateful_widget(
            StatefulImage::new().resize(Resize::Fit(None)),
            inner,
            protocol,
        );
        return;
    }
    let text = match &app.preview {
        Some(PreviewArtifact::Text { text, truncated }) => {
            if *truncated {
                format!("{text}\n\n[预览已截断到 2 MiB]")
            } else {
                text.clone()
            }
        }
        Some(PreviewArtifact::MetadataOnly { reason, metadata }) => metadata
            .as_ref()
            .map(|metadata| format!("{reason}\n{}", format_metadata(metadata)))
            .unwrap_or_else(|| reason.clone()),
        Some(PreviewArtifact::VideoFrame { metadata, .. }) => format_metadata(metadata),
        Some(PreviewArtifact::Image { .. }) => "图片预览".to_owned(),
        None => app
            .selected_file()
            .map(|entry| {
                format!(
                    "{}\n大小：{}\n修改时间：{}",
                    entry.path,
                    format_size(entry.size),
                    entry.modified.as_deref().unwrap_or("-")
                )
            })
            .unwrap_or_else(|| "选择文件后按 p 预览".to_owned()),
    };
    frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }), inner);
}

fn render_prompt(frame: &mut Frame<'_>, title: &str, buffer: &str) {
    let area = centered_rect(70, 3, frame.area());
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(buffer).block(
            Block::default()
                .title(format!(" {title} "))
                .borders(Borders::ALL),
        ),
        area,
    );
    let cursor = buffer
        .chars()
        .count()
        .min(area.width.saturating_sub(3) as usize) as u16;
    frame.set_cursor_position((area.x + 1 + cursor, area.y + 1));
}

fn centered_rect(percent_x: u16, height: u16, area: Rect) -> Rect {
    let vertical = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(height),
        Constraint::Fill(1),
    ])
    .split(area);
    Layout::horizontal([
        Constraint::Percentage((100 - percent_x) / 2),
        Constraint::Percentage(percent_x),
        Constraint::Percentage((100 - percent_x) / 2),
    ])
    .split(vertical[1])[1]
}

fn format_size(size: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = size as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{size} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn format_metadata(metadata: &crate::model::MediaMetadata) -> String {
    format!(
        "格式：{}\n尺寸：{} × {}\n时长：{}\n视频编码：{}\n音频编码：{}",
        metadata.format_name.as_deref().unwrap_or("-"),
        metadata
            .width
            .map_or_else(|| "-".to_owned(), |v| v.to_string()),
        metadata
            .height
            .map_or_else(|| "-".to_owned(), |v| v.to_string()),
        metadata
            .duration
            .map_or_else(|| "-".to_owned(), |v| format!("{v:.2} 秒")),
        metadata.video_codec.as_deref().unwrap_or("-"),
        metadata.audio_codec.as_deref().unwrap_or("-"),
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{App, Focus, centered_rect, format_size};
    use crate::{
        cache::SessionCache,
        model::{FileEntry, FileType, Snapshot},
        preview::PreviewService,
        restic::ResticCliClient,
    };
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::{
        Terminal,
        backend::{Backend, TestBackend},
        layout::Rect,
    };
    use secrecy::SecretString;

    #[test]
    fn formats_sizes() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(1536), "1.5 KiB");
    }

    #[test]
    fn prompt_fits_small_terminal() {
        let outer = Rect::new(0, 0, 80, 24);
        let prompt = centered_rect(70, 3, outer);
        assert!(prompt.right() <= outer.right());
        assert!(prompt.bottom() <= outer.bottom());
    }

    #[tokio::test]
    async fn renders_supported_terminal_sizes_and_resize() {
        let repository = tempfile::tempdir().unwrap();
        let client = Arc::new(
            ResticCliClient::new(
                "restic",
                repository.path(),
                SecretString::from("test".to_owned()),
            )
            .unwrap(),
        );
        let preview = Arc::new(PreviewService::new(
            "ffmpeg",
            "ffprobe",
            SessionCache::new().unwrap(),
        ));
        let snapshot = Snapshot {
            id: "a".repeat(64),
            short_id: "aaaaaaaa".to_owned(),
            time: "2026-01-01T00:00:00Z".to_owned(),
            hostname: "test-host".to_owned(),
            username: None,
            paths: vec!["/".to_owned()],
            tags: Vec::new(),
            total_bytes: Some(0),
        };
        let mut app = App::new(client, preview, vec![snapshot]);

        for (width, height) in [(80, 24), (120, 40), (92, 28)] {
            let backend = TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal.draw(|frame| app.draw(frame)).unwrap();
            assert_eq!(terminal.backend().size().unwrap().width, width);
        }
        app.cancel_active_job();
    }

    #[tokio::test]
    async fn horizontal_arrows_navigate_directories_but_do_not_preview_files() {
        let repository = tempfile::tempdir().unwrap();
        let client = Arc::new(
            ResticCliClient::new(
                "restic",
                repository.path(),
                SecretString::from("test".to_owned()),
            )
            .unwrap(),
        );
        let preview = Arc::new(PreviewService::new(
            "ffmpeg",
            "ffprobe",
            SessionCache::new().unwrap(),
        ));
        let snapshot = Snapshot {
            id: "a".repeat(64),
            short_id: "aaaaaaaa".to_owned(),
            time: "2026-01-01T00:00:00Z".to_owned(),
            hostname: "test-host".to_owned(),
            username: None,
            paths: vec!["/".to_owned()],
            tags: Vec::new(),
            total_bytes: Some(0),
        };
        let mut app = App::new(client, preview, vec![snapshot]);
        app.replace_job();
        app.focus = Focus::Files;

        app.current_path = "/parent/child".to_owned();
        app.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        assert_eq!(app.current_path, "/parent");
        app.replace_job();

        app.entries = vec![FileEntry {
            name: "directory".to_owned(),
            path: "/parent/directory".to_owned(),
            file_type: FileType::Directory,
            size: 0,
            modified: None,
            mode: None,
            uid: None,
            gid: None,
            link_target: None,
        }];
        app.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert_eq!(app.current_path, "/parent/directory");
        app.replace_job();

        app.entries = vec![FileEntry {
            name: "file.txt".to_owned(),
            path: "/parent/directory/file.txt".to_owned(),
            file_type: FileType::File,
            size: 1,
            modified: None,
            mode: None,
            uid: None,
            gid: None,
            link_target: None,
        }];
        app.status = "unchanged".to_owned();
        app.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert_eq!(app.status, "unchanged");
        assert!(app.active_job.is_none());

        app.current_path = "/".to_owned();
        app.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        assert!(app.focus == Focus::Snapshots);
    }
}
