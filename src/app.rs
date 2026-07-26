use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::Line,
    widgets::{
        Block, Borders, Cell, Clear, List, ListItem, ListState, Paragraph, Row, Table, TableState,
        Wrap,
    },
};
use ratatui_image::{Resize, StatefulImage, picker::Picker, protocol::StatefulProtocol};

use crate::{
    AppError,
    export::ExportService,
    jobs::JobHandle,
    model::{FileEntry, FileType, PreviewArtifact, SearchResult, Snapshot},
    preview::PreviewService,
    repository::RepositoryHandle,
};

const WIDE_LAYOUT_MIN_WIDTH: u16 = 120;
const WIDE_LAYOUT_MIN_HEIGHT: u16 = 30;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Focus {
    Snapshots,
    Directories,
    Files,
}

enum InputMode {
    Normal,
    Search(String),
    Export(String),
}

#[derive(Clone, Copy)]
enum DirectoryLoadPurpose {
    Browse,
    Expand,
}

struct DirectoryLoad {
    path: String,
    entries: Vec<FileEntry>,
    purpose: DirectoryLoadPurpose,
}

struct PreviewResult {
    entry: FileEntry,
    artifact: PreviewArtifact,
}

enum ActiveJob {
    Directory(JobHandle<DirectoryLoad>),
    Search(JobHandle<Vec<SearchResult>>),
    Preview(JobHandle<PreviewResult>),
    Export(JobHandle<PathBuf>),
}

struct TreeRow {
    path: String,
    name: String,
    depth: usize,
    expanded: bool,
    has_children: bool,
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
    active_snapshot_index: usize,
    snapshot_offset: usize,
    snapshot_page_len: usize,
    entry_index: usize,
    entry_offset: usize,
    entry_page_len: usize,
    current_path: String,
    directory_cache: HashMap<String, Vec<FileEntry>>,
    expanded_directories: HashSet<String>,
    tree_path: String,
    tree_offset: usize,
    tree_page_len: usize,
    focus: Focus,
    last_browser_focus: Focus,
    wide_layout: bool,
    input_mode: InputMode,
    status: String,
    active_job: Option<ActiveJob>,
    preview: Option<PreviewArtifact>,
    preview_entry: Option<FileEntry>,
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
            active_snapshot_index: 0,
            snapshot_offset: 0,
            snapshot_page_len: 0,
            entry_index: 0,
            entry_offset: 0,
            entry_page_len: 0,
            current_path: "/".to_owned(),
            directory_cache: HashMap::new(),
            expanded_directories: HashSet::new(),
            tree_path: "/".to_owned(),
            tree_offset: 0,
            tree_page_len: 0,
            focus: Focus::Snapshots,
            last_browser_focus: Focus::Files,
            wide_layout: false,
            input_mode: InputMode::Normal,
            status: "就绪".to_owned(),
            active_job: None,
            preview: None,
            preview_entry: None,
            image_protocol: None,
            picker,
            video_position: Duration::ZERO,
            should_quit: false,
        };
        app.browse_directory("/".to_owned());
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
                Ok(load) => self.finish_directory_load(load),
                Err(error) => self.show_error(error),
            },
            ActiveJob::Search(job) => match job.finish().await {
                Ok(results) => {
                    self.entries = results.into_iter().map(|result| result.entry).collect();
                    self.entry_index = 0;
                    self.entry_offset = 0;
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
            KeyCode::Char('`') => self.toggle_snapshots_focus(),
            KeyCode::Tab => self.toggle_browser_focus(),
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::PageUp => self.page_selection(false),
            KeyCode::PageDown => self.page_selection(true),
            KeyCode::Enter => self.activate_selection(),
            KeyCode::Backspace | KeyCode::Left => match self.focus {
                Focus::Directories => self.collapse_tree_selection(),
                Focus::Files => self.go_parent(),
                Focus::Snapshots => {}
            },
            KeyCode::Char('/') => self.input_mode = InputMode::Search(String::new()),
            KeyCode::Char('p') => self.start_preview(),
            KeyCode::Char('e') => {
                if let Some(entry) = self.selected_file() {
                    self.input_mode = InputMode::Export(entry.name.clone());
                }
            }
            KeyCode::Char('r') => self.start_directory_load(
                self.current_path.clone(),
                DirectoryLoadPurpose::Browse,
                false,
            ),
            KeyCode::Right => match self.focus {
                Focus::Snapshots => self.activate_snapshot(),
                Focus::Directories => self.expand_tree_selection(),
                Focus::Files => self.enter_file_selection(),
            },
            _ => {}
        }
    }

    fn toggle_snapshots_focus(&mut self) {
        if self.focus == Focus::Snapshots {
            self.restore_browser_focus();
        } else {
            self.last_browser_focus = self.focus;
            self.focus = Focus::Snapshots;
        }
    }

    fn toggle_browser_focus(&mut self) {
        if self.focus == Focus::Snapshots || !self.wide_layout {
            return;
        }
        self.focus = match self.focus {
            Focus::Directories => Focus::Files,
            Focus::Files => Focus::Directories,
            Focus::Snapshots => unreachable!(),
        };
        self.last_browser_focus = self.focus;
    }

    fn restore_browser_focus(&mut self) {
        self.focus = if self.wide_layout && self.last_browser_focus == Focus::Directories {
            Focus::Directories
        } else {
            Focus::Files
        };
    }

    fn move_selection(&mut self, delta: isize) {
        match self.focus {
            Focus::Snapshots => {
                move_index(&mut self.snapshot_index, self.snapshots.len(), delta);
            }
            Focus::Directories => {
                let rows = self.tree_rows();
                let current = rows
                    .iter()
                    .position(|row| row.path == self.tree_path)
                    .unwrap_or(0);
                if !rows.is_empty() {
                    let next = (current as isize + delta)
                        .clamp(0, rows.len().saturating_sub(1) as isize)
                        as usize;
                    self.tree_path = rows[next].path.clone();
                }
            }
            Focus::Files => move_index(&mut self.entry_index, self.entries.len(), delta),
        }
    }

    fn page_selection(&mut self, down: bool) {
        match self.focus {
            Focus::Snapshots => page_index(
                &mut self.snapshot_index,
                &mut self.snapshot_offset,
                self.snapshots.len(),
                self.snapshot_page_len,
                down,
            ),
            Focus::Directories => {
                let rows = self.tree_rows();
                let mut index = rows
                    .iter()
                    .position(|row| row.path == self.tree_path)
                    .unwrap_or(0);
                page_index(
                    &mut index,
                    &mut self.tree_offset,
                    rows.len(),
                    self.tree_page_len,
                    down,
                );
                if let Some(row) = rows.get(index) {
                    self.tree_path = row.path.clone();
                }
            }
            Focus::Files => page_index(
                &mut self.entry_index,
                &mut self.entry_offset,
                self.entries.len(),
                self.entry_page_len,
                down,
            ),
        }
    }

    fn activate_selection(&mut self) {
        match self.focus {
            Focus::Snapshots => self.activate_snapshot(),
            Focus::Directories => self.browse_directory(self.tree_path.clone()),
            Focus::Files => {
                let Some(entry) = self.entries.get(self.entry_index).cloned() else {
                    return;
                };
                if entry.is_dir() {
                    self.browse_directory(entry.path);
                } else {
                    self.start_preview();
                }
            }
        }
    }

    fn activate_snapshot(&mut self) {
        if self.snapshots.is_empty() {
            self.status = "仓库中没有快照".to_owned();
            return;
        }
        if self.snapshot_index != self.active_snapshot_index {
            self.active_snapshot_index = self.snapshot_index;
            self.entries.clear();
            self.entry_index = 0;
            self.entry_offset = 0;
            self.current_path = "/".to_owned();
            self.directory_cache.clear();
            self.expanded_directories.clear();
            self.tree_path = "/".to_owned();
            self.tree_offset = 0;
            self.clear_preview();
            self.start_directory_load("/".to_owned(), DirectoryLoadPurpose::Browse, false);
        }
        self.restore_browser_focus();
    }

    fn enter_file_selection(&mut self) {
        if self.selected_file().is_some_and(FileEntry::is_dir) {
            self.activate_selection();
        }
    }

    fn expand_tree_selection(&mut self) {
        let path = self.tree_path.clone();
        if self.expanded_directories.contains(&path) {
            let rows = self.tree_rows();
            if let Some(index) = rows.iter().position(|row| row.path == path)
                && let Some(child) = rows.get(index + 1)
                && child.depth > rows[index].depth
            {
                self.tree_path = child.path.clone();
            }
            return;
        }
        self.start_directory_load(path, DirectoryLoadPurpose::Expand, true);
    }

    fn collapse_tree_selection(&mut self) {
        if self.expanded_directories.remove(&self.tree_path) {
            return;
        }
        if let Some(parent) = parent_repository_path(&self.tree_path) {
            self.tree_path = parent;
        }
    }

    fn go_parent(&mut self) {
        if self.current_path == "/" {
            return;
        }
        if let Some(parent) = parent_repository_path(&self.current_path) {
            self.browse_directory(parent);
        }
    }

    fn browse_directory(&mut self, path: String) {
        self.start_directory_load(path, DirectoryLoadPurpose::Browse, true);
    }

    fn start_directory_load(
        &mut self,
        path: String,
        purpose: DirectoryLoadPurpose,
        use_cache: bool,
    ) {
        if matches!(purpose, DirectoryLoadPurpose::Browse) {
            self.replace_job();
            if path != self.current_path {
                self.clear_preview();
            }
            self.current_path = path.clone();
            self.tree_path = path.clone();
            if let Some(parent) = parent_repository_path(&path) {
                self.reveal_tree_path(&parent);
            }
        }
        if use_cache && let Some(entries) = self.directory_cache.get(&path).cloned() {
            self.finish_directory_load(DirectoryLoad {
                path,
                entries,
                purpose,
            });
            return;
        }
        let Some(snapshot) = self.selected_snapshot().cloned() else {
            self.status = "仓库中没有快照".to_owned();
            return;
        };
        if matches!(purpose, DirectoryLoadPurpose::Expand) {
            self.replace_job();
        }
        self.status = format!("正在加载 {path}…");
        let client = Arc::clone(&self.repository);
        self.active_job = Some(ActiveJob::Directory(JobHandle::spawn_cancellable(
            move |token| async move {
                let entries = client.list_directory(&snapshot.id, &path, token).await?;
                Ok(DirectoryLoad {
                    path,
                    entries,
                    purpose,
                })
            },
        )));
    }

    fn finish_directory_load(&mut self, load: DirectoryLoad) {
        let count = load.entries.len();
        self.directory_cache
            .insert(load.path.clone(), load.entries.clone());
        match load.purpose {
            DirectoryLoadPurpose::Browse => {
                self.current_path = load.path.clone();
                self.tree_path = load.path.clone();
                self.reveal_tree_path(&load.path);
                self.entries = load.entries;
                if let Some(parent) = parent_repository_path(&load.path) {
                    self.entries.insert(0, parent_entry(parent));
                }
                self.entry_index = 0;
                self.entry_offset = 0;
                self.status = format!("已加载 {count} 个项目");
            }
            DirectoryLoadPurpose::Expand => {
                self.expanded_directories.insert(load.path);
                self.status = format!("已加载 {count} 个项目");
            }
        }
    }

    fn reveal_tree_path(&mut self, path: &str) {
        let mut cursor = Some(path.to_owned());
        while let Some(current) = cursor {
            self.expanded_directories.insert(current.clone());
            cursor = parent_repository_path(&current);
        }
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
                let artifact = service
                    .preview(client, &snapshot.id, &entry, position, token)
                    .await?;
                Ok(PreviewResult { entry, artifact })
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
        self.snapshots.get(self.active_snapshot_index)
    }

    fn selected_file(&self) -> Option<&FileEntry> {
        self.entries.get(self.entry_index)
    }

    fn set_preview(&mut self, preview: PreviewResult) {
        self.image_protocol = match &preview.artifact {
            PreviewArtifact::Image { image, .. } | PreviewArtifact::VideoFrame { image, .. } => {
                Some(self.picker.new_resize_protocol((**image).clone()))
            }
            _ => None,
        };
        self.preview_entry = Some(preview.entry);
        self.preview = Some(preview.artifact);
    }

    fn clear_preview(&mut self) {
        self.preview = None;
        self.preview_entry = None;
        self.image_protocol = None;
        self.video_position = Duration::ZERO;
    }

    fn set_wide_layout(&mut self, wide_layout: bool) {
        self.wide_layout = wide_layout;
        if !wide_layout && self.focus == Focus::Directories {
            self.focus = Focus::Files;
        }
    }

    fn tree_rows(&self) -> Vec<TreeRow> {
        let mut rows = Vec::new();
        self.collect_tree_rows("/", "/", 0, &mut rows);
        rows
    }

    fn collect_tree_rows(&self, path: &str, name: &str, depth: usize, rows: &mut Vec<TreeRow>) {
        let children = self.directory_cache.get(path);
        let has_children = children
            .map(|entries| entries.iter().any(FileEntry::is_dir))
            .unwrap_or(true);
        let expanded = self.expanded_directories.contains(path);
        rows.push(TreeRow {
            path: path.to_owned(),
            name: name.to_owned(),
            depth,
            expanded,
            has_children,
        });
        if !expanded {
            return;
        }
        let Some(children) = children else {
            return;
        };
        for child in children.iter().filter(|entry| entry.is_dir()) {
            self.collect_tree_rows(&child.path, &child.name, depth + 1, rows);
        }
    }

    fn show_error(&mut self, error: AppError) {
        self.status = format!("错误：{error}");
    }
}

fn render_app(frame: &mut Frame<'_>, app: &mut App) {
    let wide_layout = frame.area().width >= WIDE_LAYOUT_MIN_WIDTH
        && frame.area().height >= WIDE_LAYOUT_MIN_HEIGHT;
    app.set_wide_layout(wide_layout);
    let sections = Layout::vertical([
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(3),
    ])
    .split(frame.area());
    frame.render_widget(
        Paragraph::new(Line::from(format!(" restic-browser  {}", app.status))),
        sections[0],
    );
    if wide_layout {
        let columns = Layout::horizontal([
            Constraint::Percentage(24),
            Constraint::Percentage(38),
            Constraint::Percentage(38),
        ])
        .split(sections[1]);
        let navigation = Layout::vertical([Constraint::Percentage(42), Constraint::Percentage(58)])
            .split(columns[0]);
        render_snapshots(frame, app, navigation[0], true);
        render_directory_tree(frame, app, navigation[1]);
        render_files(frame, app, columns[1]);
        render_preview(frame, app, columns[2]);
    } else {
        let content = Layout::vertical([Constraint::Percentage(58), Constraint::Percentage(42)])
            .split(sections[1]);
        let browser = Layout::horizontal([Constraint::Percentage(32), Constraint::Percentage(68)])
            .split(content[0]);
        render_snapshots(frame, app, browser[0], false);
        render_files(frame, app, browser[1]);
        render_preview(frame, app, content[1]);
    }
    let shortcuts = if wide_layout {
        " Tab 目录树/文件  ` 快照  PgUp/PgDn 翻页  Enter 打开  ←/→ 导航  / 搜索  p 预览  e 导出  r 刷新  q 退出"
    } else {
        " ` 快照  PgUp/PgDn 翻页  Enter 打开  ←/→ 导航  / 搜索  p 预览  e 导出  r 刷新  q 退出"
    };
    frame.render_widget(
        Paragraph::new(shortcuts)
            .style(Style::default().fg(Color::DarkGray))
            .alignment(Alignment::Center)
            .block(Block::default().borders(Borders::ALL)),
        sections[2],
    );
    match &app.input_mode {
        InputMode::Search(buffer) => render_prompt(frame, "搜索 restic pattern", buffer),
        InputMode::Export(buffer) => render_prompt(frame, "导出目标（不覆盖）", buffer),
        InputMode::Normal => {}
    }
}

fn render_snapshots(frame: &mut Frame<'_>, app: &mut App, area: Rect, condensed: bool) {
    let items = app.snapshots.iter().enumerate().map(|(index, snapshot)| {
        let active_style = if index == app.active_snapshot_index {
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        if condensed {
            let size = snapshot
                .total_bytes
                .map(format_size)
                .unwrap_or_else(|| "-".to_owned());
            ListItem::new(Line::styled(
                format!(
                    "{}  {}  {}",
                    snapshot.short_id,
                    format_snapshot_time(&snapshot.time),
                    size
                ),
                active_style,
            ))
        } else {
            let secondary_style = if index == app.active_snapshot_index {
                active_style
            } else {
                Style::default().fg(Color::DarkGray)
            };
            ListItem::new(vec![
                Line::styled(snapshot.time.clone(), active_style),
                Line::styled(
                    format!("{}  {}", snapshot.short_id, snapshot.hostname),
                    secondary_style,
                ),
            ])
        }
    });
    let border = if app.focus == Focus::Snapshots {
        Color::Cyan
    } else {
        Color::DarkGray
    };
    let list = List::new(items)
        .block(
            Block::default()
                .title(" 快照（最新优先） ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(border)),
        )
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("› ");
    app.snapshot_page_len = usize::from(
        area.height
            .saturating_sub(2)
            .checked_div(if condensed { 1 } else { 2 })
            .unwrap_or(0),
    );
    let mut state = ListState::default()
        .with_selected((!app.snapshots.is_empty()).then_some(app.snapshot_index))
        .with_offset(app.snapshot_offset);
    frame.render_stateful_widget(list, area, &mut state);
    app.snapshot_offset = state.offset();
}

fn render_directory_tree(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let rows = app.tree_rows();
    let items = rows.iter().map(|row| {
        let marker = if row.expanded {
            "▾"
        } else if row.has_children {
            "▸"
        } else {
            " "
        };
        let style = if row.path == app.current_path {
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD)
        } else if is_repository_ancestor(&row.path, &app.current_path) {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default()
        };
        ListItem::new(Line::styled(
            format!("{}{} {}", "  ".repeat(row.depth), marker, row.name),
            style,
        ))
    });
    let border = if app.focus == Focus::Directories {
        Color::Cyan
    } else {
        Color::DarkGray
    };
    let list = List::new(items)
        .block(
            Block::default()
                .title(" 目录树 ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(border)),
        )
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("› ");
    let selected = rows
        .iter()
        .position(|row| row.path == app.tree_path)
        .or((!rows.is_empty()).then_some(0));
    app.tree_page_len = usize::from(area.height.saturating_sub(2));
    let mut state = ListState::default()
        .with_selected(selected)
        .with_offset(app.tree_offset);
    frame.render_stateful_widget(list, area, &mut state);
    app.tree_offset = state.offset();
}

fn render_files(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let rows = app.entries.iter().map(|entry| {
        let style = file_entry_style(entry);
        Row::new([
            Cell::from(entry.name.clone()).style(style),
            Cell::from(format_size(entry.size)),
            Cell::from(format_modified(entry.modified.as_deref())),
            Cell::from(format_entry_type(entry)).style(style),
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
            Constraint::Percentage(45),
            Constraint::Percentage(18),
            Constraint::Percentage(25),
            Constraint::Percentage(12),
        ],
    )
    .header(
        Row::new(["名称", "大小", "修改时间", "类型"]).style(Style::default().fg(Color::Yellow)),
    )
    .block(
        Block::default()
            .title(format!(
                " 文件：{}（{} 个项目） ",
                app.current_path,
                app.entries
                    .len()
                    .saturating_sub(usize::from(has_parent_entry(&app.entries)))
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border)),
    )
    .row_highlight_style(Style::default().bg(Color::DarkGray))
    .highlight_symbol("› ");
    app.entry_page_len = usize::from(area.height.saturating_sub(3));
    let mut state = TableState::default()
        .with_selected((!app.entries.is_empty()).then_some(app.entry_index))
        .with_offset(app.entry_offset);
    frame.render_stateful_widget(table, area, &mut state);
    app.entry_offset = state.offset();
}

fn render_preview(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let preview_name = app
        .preview_entry
        .as_ref()
        .or_else(|| app.selected_file())
        .map(|entry| entry.name.as_str());
    let title = preview_name
        .map(|name| format!(" 预览：{name} "))
        .unwrap_or_else(|| " 预览 / 元数据 ".to_owned());
    let block = Block::default().title(title).borders(Borders::ALL);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let entry_details = app
        .preview_entry
        .as_ref()
        .map(|entry| format_entry_details(entry, app.selected_snapshot()))
        .unwrap_or_default();
    let visual_details = match &app.preview {
        Some(PreviewArtifact::VideoFrame { metadata, .. }) => {
            format!("{entry_details}\n{}", format_metadata(metadata))
        }
        Some(PreviewArtifact::Image { .. }) => entry_details.clone(),
        _ => String::new(),
    };
    if matches!(
        &app.preview,
        Some(PreviewArtifact::Image { .. } | PreviewArtifact::VideoFrame { .. })
    ) {
        let (preview_area, details_area) = split_preview_details(inner, &visual_details);
        if let Some(protocol) = &mut app.image_protocol {
            frame.render_stateful_widget(
                StatefulImage::new().resize(Resize::Fit(None)),
                preview_area,
                protocol,
            );
        }
        if let Some(details_area) = details_area {
            render_preview_details(frame, &visual_details, details_area);
        }
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
        Some(PreviewArtifact::MetadataOnly { reason, metadata }) => {
            let metadata = metadata.as_ref().map(format_metadata).unwrap_or_default();
            [reason.as_str(), entry_details.as_str(), metadata.as_str()]
                .into_iter()
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>()
                .join("\n")
        }
        Some(PreviewArtifact::VideoFrame { metadata, .. }) => format_metadata(metadata),
        Some(PreviewArtifact::Image { .. }) => "图片预览".to_owned(),
        None => app
            .selected_file()
            .map(|entry| format_entry_details(entry, app.selected_snapshot()))
            .unwrap_or_default(),
    };
    if matches!(&app.preview, Some(PreviewArtifact::Text { .. })) && !entry_details.is_empty() {
        let (preview_area, details_area) = split_preview_details(inner, &entry_details);
        frame.render_widget(
            Paragraph::new(text).wrap(Wrap { trim: false }),
            preview_area,
        );
        if let Some(details_area) = details_area {
            render_preview_details(frame, &entry_details, details_area);
        }
    } else {
        frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }), inner);
    }
}

fn split_preview_details(area: Rect, details: &str) -> (Rect, Option<Rect>) {
    if details.is_empty() || area.height < 5 {
        return (area, None);
    }
    let details_height = (details.lines().count() as u16 + 1)
        .min(area.height.saturating_sub(3))
        .max(2);
    let sections =
        Layout::vertical([Constraint::Fill(1), Constraint::Length(details_height)]).split(area);
    (sections[0], Some(sections[1]))
}

fn render_preview_details(frame: &mut Frame<'_>, details: &str, area: Rect) {
    frame.render_widget(
        Paragraph::new(details).wrap(Wrap { trim: false }).block(
            Block::default()
                .borders(Borders::TOP)
                .border_style(Style::default().fg(Color::DarkGray)),
        ),
        area,
    );
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

fn move_index(index: &mut usize, length: usize, delta: isize) {
    if length > 0 {
        *index = (*index as isize + delta).clamp(0, length.saturating_sub(1) as isize) as usize;
    }
}

fn page_index(index: &mut usize, offset: &mut usize, length: usize, page_len: usize, down: bool) {
    if length == 0 || page_len == 0 {
        return;
    }
    *index = (*index).min(length - 1);
    *offset = (*offset).min(length.saturating_sub(page_len));
    let bottom = (*offset + page_len - 1).min(length - 1);
    if down {
        if *index < bottom {
            *index = bottom;
        } else {
            *offset = (*offset + page_len).min(length.saturating_sub(page_len));
            *index = (*offset + page_len - 1).min(length - 1);
        }
    } else if *index > *offset {
        *index = *offset;
    } else {
        *offset = offset.saturating_sub(page_len);
        *index = *offset;
    }
}

fn parent_repository_path(path: &str) -> Option<String> {
    if path == "/" {
        return None;
    }
    let trimmed = path.trim_end_matches('/');
    Some(
        trimmed
            .rsplit_once('/')
            .map(|(parent, _)| if parent.is_empty() { "/" } else { parent })
            .unwrap_or("/")
            .to_owned(),
    )
}

fn parent_entry(path: String) -> FileEntry {
    FileEntry {
        name: "..".to_owned(),
        path,
        file_type: FileType::Directory,
        size: 0,
        modified: None,
        mode: None,
        uid: None,
        gid: None,
        link_target: None,
    }
}

fn has_parent_entry(entries: &[FileEntry]) -> bool {
    entries
        .first()
        .is_some_and(|entry| entry.name == ".." && entry.is_dir())
}

fn is_repository_ancestor(candidate: &str, path: &str) -> bool {
    candidate != path
        && (candidate == "/"
            || path
                .strip_prefix(candidate)
                .is_some_and(|suffix| suffix.starts_with('/')))
}

fn file_entry_style(entry: &FileEntry) -> Style {
    let color = match entry.file_type {
        FileType::Directory => Some(Color::Blue),
        FileType::Symlink => Some(Color::Cyan),
        FileType::Other => Some(Color::Yellow),
        FileType::File if entry.mode.is_some_and(|mode| mode & 0o111 != 0) => Some(Color::Green),
        FileType::File => None,
    };
    color.map_or_else(Style::default, |color| Style::default().fg(color))
}

fn format_snapshot_time(time: &str) -> String {
    compact_datetime(time)
}

fn format_modified(modified: Option<&str>) -> String {
    modified
        .map(compact_datetime)
        .unwrap_or_else(|| "-".to_owned())
}

fn compact_datetime(value: &str) -> String {
    value.chars().take(16).collect::<String>().replace('T', " ")
}

fn format_entry_type(entry: &FileEntry) -> String {
    match entry.file_type {
        FileType::Directory => "DIR".to_owned(),
        FileType::Symlink => "LINK".to_owned(),
        FileType::Other => "OTHER".to_owned(),
        FileType::File => Path::new(&entry.name)
            .extension()
            .and_then(|extension| extension.to_str())
            .filter(|extension| !extension.is_empty())
            .map(|extension| extension.to_uppercase())
            .unwrap_or_else(|| "FILE".to_owned()),
    }
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

fn format_entry_details(entry: &FileEntry, snapshot: Option<&Snapshot>) -> String {
    let snapshot = snapshot
        .map(|snapshot| {
            format!(
                "{} ({})",
                snapshot.short_id,
                format_snapshot_time(&snapshot.time)
            )
        })
        .unwrap_or_else(|| "-".to_owned());
    format_detail_rows(&[
        ("文件", entry.name.clone()),
        ("快照", snapshot),
        ("路径", entry.path.clone()),
        (
            "大小",
            format!("{} ({} bytes)", format_size(entry.size), entry.size),
        ),
        (
            "修改时间",
            entry.modified.as_deref().unwrap_or("-").to_owned(),
        ),
        ("类型", format_entry_type(entry)),
    ])
}

fn format_metadata(metadata: &crate::model::MediaMetadata) -> String {
    format_detail_rows(&[
        (
            "格式",
            metadata.format_name.as_deref().unwrap_or("-").to_owned(),
        ),
        (
            "尺寸",
            format!(
                "{} × {}",
                metadata
                    .width
                    .map_or_else(|| "-".to_owned(), |v| v.to_string()),
                metadata
                    .height
                    .map_or_else(|| "-".to_owned(), |v| v.to_string())
            ),
        ),
        (
            "时长",
            metadata
                .duration
                .map_or_else(|| "-".to_owned(), |v| format!("{v:.2} 秒")),
        ),
        (
            "视频编码",
            metadata.video_codec.as_deref().unwrap_or("-").to_owned(),
        ),
        (
            "音频编码",
            metadata.audio_codec.as_deref().unwrap_or("-").to_owned(),
        ),
    ])
}

fn format_detail_rows(rows: &[(&str, String)]) -> String {
    let label_width = rows
        .iter()
        .map(|(label, _)| terminal_text_width(label))
        .max()
        .unwrap_or(0);
    rows.iter()
        .map(|(label, value)| {
            format!(
                "{label}: {}{value}",
                " ".repeat(label_width.saturating_sub(terminal_text_width(label)) + 1)
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn terminal_text_width(text: &str) -> usize {
    text.chars()
        .map(|character| if character.is_ascii() { 1 } else { 2 })
        .sum()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{
        App, Focus, centered_rect, file_entry_style, format_detail_rows, format_size, page_index,
        terminal_text_width,
    };
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
        style::Color,
    };
    use secrecy::SecretString;

    fn test_snapshot(id: char) -> Snapshot {
        Snapshot {
            id: id.to_string().repeat(64),
            short_id: id.to_string().repeat(8),
            time: "2026-01-01T00:00:00Z".to_owned(),
            hostname: "test-host".to_owned(),
            username: None,
            paths: vec!["/".to_owned()],
            tags: Vec::new(),
            total_bytes: Some(0),
        }
    }

    fn test_app(snapshots: Vec<Snapshot>) -> App {
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
        let mut app = App::new(client, preview, snapshots);
        app.replace_job();
        app
    }

    fn test_entry(name: &str, path: &str, file_type: FileType) -> FileEntry {
        FileEntry {
            name: name.to_owned(),
            path: path.to_owned(),
            file_type,
            size: 1,
            modified: None,
            mode: None,
            uid: None,
            gid: None,
            link_target: None,
        }
    }

    fn rendered_text(backend: &TestBackend) -> String {
        backend
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

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

    #[test]
    fn page_navigation_uses_view_edges_before_changing_pages() {
        let (mut index, mut offset) = (3, 0);
        page_index(&mut index, &mut offset, 30, 10, true);
        assert_eq!((index, offset), (9, 0));

        page_index(&mut index, &mut offset, 30, 10, true);
        assert_eq!((index, offset), (19, 10));

        page_index(&mut index, &mut offset, 30, 10, false);
        assert_eq!((index, offset), (10, 10));

        page_index(&mut index, &mut offset, 30, 10, false);
        assert_eq!((index, offset), (0, 0));
    }

    #[test]
    fn metadata_values_start_in_the_same_terminal_column() {
        let details = format_detail_rows(&[
            ("文件", "photo.jpg".to_owned()),
            ("修改时间", "2026-01-01".to_owned()),
            ("类型", "JPEG".to_owned()),
        ]);
        let value_columns = details
            .lines()
            .map(|line| {
                let (label, _) = line.split_once(':').unwrap();
                terminal_text_width(label)
                    + line[label.len() + 1..]
                        .chars()
                        .take_while(|character| *character == ' ')
                        .count()
                    + 1
            })
            .collect::<Vec<_>>();
        assert!(value_columns.windows(2).all(|pair| pair[0] == pair[1]));
    }

    #[test]
    fn file_colors_follow_common_ls_conventions() {
        let directory = test_entry("photos", "/photos", FileType::Directory);
        let symlink = test_entry("latest", "/latest", FileType::Symlink);
        let mut executable = test_entry("backup", "/backup", FileType::File);
        executable.mode = Some(0o755);
        let regular = test_entry("notes.txt", "/notes.txt", FileType::File);

        assert_eq!(file_entry_style(&directory).fg, Some(Color::Blue));
        assert_eq!(file_entry_style(&symlink).fg, Some(Color::Cyan));
        assert_eq!(file_entry_style(&executable).fg, Some(Color::Green));
        assert_eq!(file_entry_style(&regular).fg, None);
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

        for (width, height, wide) in [(80, 24, false), (120, 40, true), (92, 28, false)] {
            let backend = TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal.draw(|frame| app.draw(frame)).unwrap();
            assert_eq!(terminal.backend().size().unwrap().width, width);
            assert_eq!(app.wide_layout, wide);
            let rendered = rendered_text(terminal.backend());
            assert!(rendered.contains("文 件 ："));
            assert!(rendered.contains("预 览"));
            assert_eq!(rendered.contains("目 录 树"), wide);
            if wide {
                app.focus = Focus::Directories;
            } else {
                assert_ne!(app.focus, Focus::Directories);
            }
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
        assert_eq!(app.focus, Focus::Files);
    }

    #[tokio::test]
    async fn tab_only_switches_tree_and_files_and_backtick_restores_browser_state() {
        let mut app = test_app(vec![test_snapshot('a')]);
        app.set_wide_layout(true);
        app.focus = Focus::Files;
        app.current_path = "/home/minato/Pictures".to_owned();
        app.tree_path = "/home/minato".to_owned();
        app.entries = vec![
            FileEntry {
                name: "first.txt".to_owned(),
                path: "/home/minato/Pictures/first.txt".to_owned(),
                file_type: FileType::File,
                size: 1,
                modified: None,
                mode: None,
                uid: None,
                gid: None,
                link_target: None,
            },
            FileEntry {
                name: "second.txt".to_owned(),
                path: "/home/minato/Pictures/second.txt".to_owned(),
                file_type: FileType::File,
                size: 2,
                modified: None,
                mode: None,
                uid: None,
                gid: None,
                link_target: None,
            },
        ];
        app.entry_index = 1;

        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.focus, Focus::Directories);
        app.handle_key(KeyEvent::new(KeyCode::Char('`'), KeyModifiers::NONE));
        assert_eq!(app.focus, Focus::Snapshots);
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.focus, Focus::Snapshots);
        app.handle_key(KeyEvent::new(KeyCode::Char('`'), KeyModifiers::NONE));

        assert_eq!(app.focus, Focus::Directories);
        assert_eq!(app.current_path, "/home/minato/Pictures");
        assert_eq!(app.tree_path, "/home/minato");
        assert_eq!(app.entry_index, 1);
    }

    #[tokio::test]
    async fn moving_snapshot_cursor_does_not_switch_the_browsed_snapshot_until_enter() {
        let mut app = test_app(vec![test_snapshot('a'), test_snapshot('b')]);
        app.set_wide_layout(true);
        app.focus = Focus::Files;
        app.current_path = "/kept/directory".to_owned();
        app.entry_index = 3;

        app.handle_key(KeyEvent::new(KeyCode::Char('`'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char('`'), KeyModifiers::NONE));

        assert_eq!(app.active_snapshot_index, 0);
        assert_eq!(app.selected_snapshot().unwrap().short_id, "aaaaaaaa");
        assert_eq!(app.current_path, "/kept/directory");
        assert_eq!(app.entry_index, 3);

        app.handle_key(KeyEvent::new(KeyCode::Char('`'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(app.active_snapshot_index, 1);
        assert_eq!(app.selected_snapshot().unwrap().short_id, "bbbbbbbb");
        assert_eq!(app.current_path, "/");
        assert_eq!(app.entry_index, 0);
        assert_eq!(app.focus, Focus::Files);
        app.replace_job();
    }

    #[tokio::test]
    async fn directory_tree_expands_lazily_and_collapses_to_its_parent() {
        let mut app = test_app(vec![test_snapshot('a')]);
        app.set_wide_layout(true);
        app.finish_directory_load(super::DirectoryLoad {
            path: "/".to_owned(),
            entries: vec![FileEntry {
                name: "photos".to_owned(),
                path: "/photos".to_owned(),
                file_type: FileType::Directory,
                size: 0,
                modified: None,
                mode: None,
                uid: None,
                gid: None,
                link_target: None,
            }],
            purpose: super::DirectoryLoadPurpose::Browse,
        });
        app.directory_cache.insert(
            "/photos".to_owned(),
            vec![FileEntry {
                name: "trips".to_owned(),
                path: "/photos/trips".to_owned(),
                file_type: FileType::Directory,
                size: 0,
                modified: None,
                mode: None,
                uid: None,
                gid: None,
                link_target: None,
            }],
        );
        app.focus = Focus::Directories;
        app.tree_path = "/photos".to_owned();

        app.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert!(app.expanded_directories.contains("/photos"));
        assert!(
            app.tree_rows()
                .iter()
                .any(|row| row.path == "/photos/trips")
        );

        app.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        assert!(!app.expanded_directories.contains("/photos"));
        assert_eq!(app.tree_path, "/photos");
        app.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        assert_eq!(app.tree_path, "/");
    }

    #[tokio::test]
    async fn changing_directories_clears_preview_and_adds_parent_entry() {
        let mut app = test_app(vec![test_snapshot('a')]);
        app.current_path = "/photos".to_owned();
        app.preview_entry = Some(test_entry("old.jpg", "/photos/old.jpg", FileType::File));
        app.preview = Some(crate::model::PreviewArtifact::MetadataOnly {
            reason: "test".to_owned(),
            metadata: None,
        });
        app.directory_cache
            .insert("/photos/trips".to_owned(), Vec::new());

        app.browse_directory("/photos/trips".to_owned());

        assert!(app.preview.is_none());
        assert!(app.preview_entry.is_none());
        assert_eq!(app.entries[0].name, "..");
        assert_eq!(app.entries[0].path, "/photos");
    }

    #[tokio::test]
    async fn file_view_keeps_its_scroll_offset_while_selection_moves_inside_the_view() {
        let mut app = test_app(vec![test_snapshot('a')]);
        app.set_wide_layout(true);
        app.focus = Focus::Files;
        app.entries = (0..100)
            .map(|index| {
                test_entry(
                    &format!("file-{index:03}.txt"),
                    &format!("/file-{index:03}.txt"),
                    FileType::File,
                )
            })
            .collect();
        app.entry_index = 60;
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal.draw(|frame| app.draw(frame)).unwrap();
        let offset = app.entry_offset;
        assert!(offset > 0);

        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        terminal.draw(|frame| app.draw(frame)).unwrap();
        assert_eq!(app.entry_offset, offset);
    }
}
