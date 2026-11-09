use crate::domain::{
    LeftPanelWidthCols, MAX_LEFT_TITLES, NoteDetail, NoteGuid, NoteSummary, Notebook, NotebookGuid,
    SearchPage, Tag,
};
use crate::enml::PreparedNoteDocument;
use crate::preferences::Preferences;
use crate::search::ParsedSearch;
use crate::vim_pane::VimSession;
use crate::worker::{WorkerCommand, WorkerEvent};
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::Rect;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

const MIN_SAVE_ANIMATION_DURATION: Duration = Duration::from_millis(400);
const SAVED_BADGE_DURATION: Duration = Duration::from_secs(2);
const ERROR_BADGE_DURATION: Duration = Duration::from_secs(6);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FocusTarget {
    LeftTitles,
    SearchInput,
    RightVim,
}

#[derive(Clone, Debug)]
pub enum SaveIndicator {
    Idle,
    Saving {
        label: String,
        started_at: Instant,
        pending_completion: Option<String>,
    },
    Saved {
        label: String,
        until: Instant,
    },
    Failed {
        message: String,
        until: Instant,
    },
}

impl SaveIndicator {
    #[must_use]
    pub fn is_animating(&self) -> bool {
        matches!(
            self,
            Self::Saving { .. } | Self::Saved { .. } | Self::Failed { .. }
        )
    }
}

#[derive(Clone, Debug)]
pub struct TagEditorModal {
    pub note_guid: NoteGuid,
    pub note_title: String,
    pub filter: TextInput,
    pub selected_tags: Vec<String>,
    pub cursor: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TagModalRow {
    CreateNew(String),
    Existing { name: String, checked: bool },
}

impl TagEditorModal {
    #[must_use]
    pub fn visible_rows(&self, all_tags: &[Tag]) -> Vec<TagModalRow> {
        let query = self.filter.text.trim();
        let query_lower = query.to_lowercase();
        let mut rows = Vec::new();

        if !query.is_empty() {
            let exists = all_tags.iter().any(|t| t.name.eq_ignore_ascii_case(query))
                || self
                    .selected_tags
                    .iter()
                    .any(|t| t.eq_ignore_ascii_case(query));
            if !exists {
                rows.push(TagModalRow::CreateNew(query.to_owned()));
            }
        }

        // Collect union of account tags + any newly staged tags on this note.
        let mut names: Vec<String> = all_tags.iter().map(|t| t.name.clone()).collect();
        for sel in &self.selected_tags {
            if !names.iter().any(|n| n.eq_ignore_ascii_case(sel)) {
                names.push(sel.clone());
            }
        }

        // Checked tags first (alphabetical), then unchecked tags (alphabetical).
        names.sort_by(|a, b| {
            let a_checked = self.is_checked(a);
            let b_checked = self.is_checked(b);
            b_checked
                .cmp(&a_checked)
                .then_with(|| a.to_lowercase().cmp(&b.to_lowercase()))
        });

        for name in names {
            if query_lower.is_empty() || name.to_lowercase().contains(&query_lower) {
                let checked = self.is_checked(&name);
                rows.push(TagModalRow::Existing { name, checked });
            }
        }

        rows
    }

    #[must_use]
    pub fn is_checked(&self, name: &str) -> bool {
        self.selected_tags
            .iter()
            .any(|t| t.eq_ignore_ascii_case(name))
    }

    pub fn toggle_name(&mut self, name: &str) {
        if let Some(pos) = self
            .selected_tags
            .iter()
            .position(|t| t.eq_ignore_ascii_case(name))
        {
            self.selected_tags.remove(pos);
        } else {
            self.selected_tags.push(name.to_owned());
        }
    }

    pub fn clamp_cursor(&mut self, len: usize) {
        if len == 0 {
            self.cursor = 0;
        } else if self.cursor >= len {
            self.cursor = len - 1;
        }
    }
}

#[derive(Clone, Debug)]
pub struct NotebookPickerModal {
    pub note_guid: NoteGuid,
    pub note_title: String,
    pub current_notebook: Option<NotebookGuid>,
    pub filter: TextInput,
    pub cursor: usize,
}

impl NotebookPickerModal {
    #[must_use]
    pub fn filtered<'a>(&self, notebooks: &'a [Notebook]) -> Vec<&'a Notebook> {
        let q = self.filter.text.trim().to_lowercase();
        notebooks
            .iter()
            .filter(|nb| {
                q.is_empty()
                    || nb.name.to_lowercase().contains(&q)
                    || nb
                        .stack
                        .as_deref()
                        .is_some_and(|s| s.to_lowercase().contains(&q))
            })
            .collect()
    }

    pub fn clamp_cursor(&mut self, len: usize) {
        if len == 0 {
            self.cursor = 0;
        } else if self.cursor >= len {
            self.cursor = len - 1;
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptKind {
    RenameNote,
    CreateNote,
}

#[derive(Clone, Debug)]
pub struct TextPromptModal {
    pub kind: PromptKind,
    pub note_guid: Option<NoteGuid>,
    pub input: TextInput,
}

pub const PREFERENCES_ITEMS_COUNT: usize = 1;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PreferencesModal {
    pub cursor: usize,
}

#[derive(Clone, Debug, Default)]
pub enum ModalState {
    #[default]
    None,
    TagEditor(TagEditorModal),
    NotebookPicker(NotebookPickerModal),
    TextPrompt(TextPromptModal),
    Preferences(PreferencesModal),
    Help,
}

/// Simple Unicode-aware single-line text input with cursor index (in chars).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TextInput {
    pub text: String,
    pub cursor_chars: usize,
}

impl TextInput {
    #[must_use]
    pub fn new(initial: impl Into<String>) -> Self {
        let text = initial.into();
        let cursor_chars = text.chars().count();
        Self { text, cursor_chars }
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor_chars = 0;
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('u') if ctrl => {
                self.clear();
                true
            }
            KeyCode::Char('a') if ctrl => {
                self.cursor_chars = 0;
                true
            }
            KeyCode::Char('e') if ctrl => {
                self.cursor_chars = self.text.chars().count();
                true
            }
            KeyCode::Char(ch) if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) => {
                let byte_idx = self.byte_index_at(self.cursor_chars);
                self.text.insert(byte_idx, ch);
                self.cursor_chars += 1;
                true
            }
            KeyCode::Backspace => {
                if self.cursor_chars > 0 {
                    let prev_char = self.cursor_chars - 1;
                    let start = self.byte_index_at(prev_char);
                    let end = self.byte_index_at(self.cursor_chars);
                    self.text.replace_range(start..end, "");
                    self.cursor_chars = prev_char;
                }
                true
            }
            KeyCode::Delete => {
                let len = self.text.chars().count();
                if self.cursor_chars < len {
                    let start = self.byte_index_at(self.cursor_chars);
                    let end = self.byte_index_at(self.cursor_chars + 1);
                    self.text.replace_range(start..end, "");
                }
                true
            }
            KeyCode::Left => {
                self.cursor_chars = self.cursor_chars.saturating_sub(1);
                true
            }
            KeyCode::Right => {
                let len = self.text.chars().count();
                if self.cursor_chars < len {
                    self.cursor_chars += 1;
                }
                true
            }
            KeyCode::Home => {
                self.cursor_chars = 0;
                true
            }
            KeyCode::End => {
                self.cursor_chars = self.text.chars().count();
                true
            }
            _ => false,
        }
    }

    #[must_use]
    pub fn byte_index_at(&self, char_pos: usize) -> usize {
        self.text
            .char_indices()
            .nth(char_pos)
            .map_or(self.text.len(), |(idx, _)| idx)
    }
}

/// Interactive hit-boxes recorded from the current terminal layout for mouse handling.
#[derive(Clone, Copy, Debug, Default)]
pub struct LayoutGeometry {
    pub terminal_width: u16,
    pub toggle_left_btn: Rect,
    pub search_bar: Rect,
    pub left_titles_inner: Option<Rect>,
    pub separator_x: Option<u16>,
    pub split_y_range: (u16, u16),
    pub title_hit: Rect,
    pub notebook_hit: Rect,
    pub tags_hit: Rect,
    pub vim_area: Rect,
    pub modal_area: Option<Rect>,
    pub modal_inner: Option<Rect>,
}

pub struct App {
    pub should_quit: bool,
    pub focus: FocusTarget,
    pub modal: ModalState,
    pub left_panel_visible: bool,
    pub left_width: LeftPanelWidthCols,
    pub dragging_separator: bool,

    pub search_input: TextInput,
    pub active_query: String,
    pub total_matching: usize,
    pub notes: Vec<NoteSummary>,
    pub selected_index: usize,

    pub notebooks: Vec<Notebook>,
    pub tags: Vec<Tag>,

    pub note_cache: HashMap<NoteGuid, NoteDetail>,
    pub prepared_docs: HashMap<NoteGuid, PreparedNoteDocument>,

    pub loading_list: bool,
    pub loading_note_guid: Option<NoteGuid>,
    /// Visible notes still waiting for a content fetch. The selected note is first.
    prefetch_queue: VecDeque<NoteGuid>,
    /// The single in-flight `LoadNote`. The worker never has more than one.
    note_load_inflight: Option<NoteGuid>,
    pub next_request_id: u64,
    pub latest_search_req: u64,
    pub latest_note_req: u64,

    pub save_indicator: SaveIndicator,
    pub anim_tick: usize,
    pub status_error: Option<String>,

    pub preferences: Preferences,
    pub preferences_path: Option<PathBuf>,
    pub preview_scroll: u16,
    pub pending_vim_guid: Option<NoteGuid>,

    pub vim_session: Option<VimSession>,
    pub geometry: LayoutGeometry,

    cmd_tx: Sender<WorkerCommand>,
}

impl App {
    pub fn new(cmd_tx: Sender<WorkerCommand>) -> Self {
        Self::with_preferences(cmd_tx, Preferences::default(), None)
    }

    pub fn with_preferences(
        cmd_tx: Sender<WorkerCommand>,
        preferences: Preferences,
        preferences_path: Option<PathBuf>,
    ) -> Self {
        let _ = cmd_tx.send(WorkerCommand::Bootstrap);
        Self {
            should_quit: false,
            focus: FocusTarget::LeftTitles,
            modal: ModalState::None,
            left_panel_visible: true,
            left_width: LeftPanelWidthCols::default(),
            dragging_separator: false,

            search_input: TextInput::default(),
            active_query: String::new(),
            total_matching: 0,
            notes: Vec::new(),
            selected_index: 0,

            notebooks: Vec::new(),
            tags: Vec::new(),

            note_cache: HashMap::new(),
            prepared_docs: HashMap::new(),

            loading_list: true,
            loading_note_guid: None,
            prefetch_queue: VecDeque::new(),
            note_load_inflight: None,
            next_request_id: 1,
            latest_search_req: 0,
            latest_note_req: 0,

            save_indicator: SaveIndicator::Idle,
            anim_tick: 0,
            status_error: None,

            preferences,
            preferences_path,
            preview_scroll: 0,
            pending_vim_guid: None,

            vim_session: None,
            geometry: LayoutGeometry::default(),

            cmd_tx,
        }
    }

    #[must_use]
    pub fn selected_preview_text(&self) -> Option<&str> {
        let guid = &self.selected_summary()?.guid;
        self.prepared_docs.get(guid).map(|d| d.markdown.as_str())
    }

    #[must_use]
    pub fn selected_summary(&self) -> Option<&NoteSummary> {
        self.notes.get(self.selected_index)
    }

    #[must_use]
    pub fn selected_detail(&self) -> Option<&NoteDetail> {
        let guid = &self.selected_summary()?.guid;
        self.note_cache.get(guid)
    }

    #[must_use]
    pub fn notebook_name(&self, guid: Option<&NotebookGuid>) -> String {
        let Some(g) = guid else {
            return "(Default)".to_owned();
        };
        self.notebooks
            .iter()
            .find(|nb| &nb.guid == g)
            .map_or_else(|| g.0.clone(), |nb| nb.name.clone())
    }

    pub fn tick(&mut self) {
        self.anim_tick = self.anim_tick.wrapping_add(1);
        let now = Instant::now();

        match &self.save_indicator {
            SaveIndicator::Saving {
                started_at,
                pending_completion: Some(done_label),
                ..
            } => {
                if now.duration_since(*started_at) >= MIN_SAVE_ANIMATION_DURATION {
                    self.save_indicator = SaveIndicator::Saved {
                        label: done_label.clone(),
                        until: now + SAVED_BADGE_DURATION,
                    };
                }
            }
            SaveIndicator::Saved { until, .. } | SaveIndicator::Failed { until, .. }
                if now >= *until =>
            {
                self.save_indicator = SaveIndicator::Idle;
            }
            _ => {}
        }

        self.poll_vim_activity();
    }

    pub fn poll_vim_activity(&mut self) {
        let mut saved_payload: Option<(NoteGuid, String)> = None;
        let mut exited_guid: Option<NoteGuid> = None;

        if let Some(vim) = &mut self.vim_session {
            vim.drain_output();
            if let Some(md) = vim.poll_saved_markdown() {
                saved_payload = Some((vim.note_guid.clone(), md));
            }
            if vim.has_exited() {
                exited_guid = Some(vim.note_guid.clone());
            }
        }

        if let Some((guid, edited_md)) = saved_payload {
            self.save_markdown_from_vim(&guid, &edited_md);
        }

        if let Some(guid) = exited_guid {
            self.vim_session = None;
            if !self.left_panel_visible {
                self.left_panel_visible = true;
            }
            self.focus = FocusTarget::LeftTitles;
            if self.preferences.vim_preview
                && self.selected_summary().is_some_and(|s| s.guid == guid)
            {
                self.ensure_vim_for_note(&guid);
            }
        }
    }

    fn save_markdown_from_vim(&mut self, guid: &NoteGuid, edited_md: &str) {
        let Some(detail) = self.note_cache.get(guid).cloned() else {
            return;
        };
        let prepared =
            self.prepared_docs.get(guid).cloned().unwrap_or_else(|| {
                PreparedNoteDocument::from_enml(&detail.enml, &detail.resources)
            });
        let new_enml = prepared.to_enml(edited_md);

        // Update local prepared markdown snapshot immediately so subsequent re-opens reflect edits.
        let refreshed = PreparedNoteDocument::from_enml(&new_enml, &detail.resources);
        self.prepared_docs.insert(guid.clone(), refreshed);
        if let Some(cached) = self.note_cache.get_mut(guid) {
            cached.enml.clone_from(&new_enml);
        }

        self.begin_saving("Saving note");
        let _ = self.cmd_tx.send(WorkerCommand::SaveContent {
            guid: guid.clone(),
            title: detail.title,
            enml: new_enml,
        });
    }

    fn begin_saving(&mut self, label: impl Into<String>) {
        self.status_error = None;
        self.save_indicator = SaveIndicator::Saving {
            label: label.into(),
            started_at: Instant::now(),
            pending_completion: None,
        };
    }

    fn finish_saving(&mut self, done_label: impl Into<String>) {
        let done_label = done_label.into();
        let now = Instant::now();
        match &mut self.save_indicator {
            SaveIndicator::Saving {
                started_at,
                pending_completion,
                ..
            } => {
                if now.duration_since(*started_at) >= MIN_SAVE_ANIMATION_DURATION {
                    self.save_indicator = SaveIndicator::Saved {
                        label: done_label,
                        until: now + SAVED_BADGE_DURATION,
                    };
                } else {
                    *pending_completion = Some(done_label);
                }
            }
            _ => {
                self.save_indicator = SaveIndicator::Saved {
                    label: done_label,
                    until: now + SAVED_BADGE_DURATION,
                };
            }
        }
    }

    pub fn handle_worker_event(&mut self, evt: WorkerEvent) {
        match evt {
            WorkerEvent::Bootstrapped {
                notebooks,
                tags,
                page,
            } => {
                self.notebooks = notebooks;
                self.tags = tags;
                self.loading_list = false;
                self.apply_search_page(page);
            }
            WorkerEvent::SearchCompleted { request_id, page } => {
                if request_id == self.latest_search_req {
                    self.loading_list = false;
                    self.apply_search_page(page);
                }
            }
            WorkerEvent::NoteLoaded { note, .. } => {
                let guid = note.guid.clone();
                let prepared = PreparedNoteDocument::from_enml(&note.enml, &note.resources);
                self.prepared_docs.insert(guid.clone(), prepared);
                self.note_cache.insert(guid.clone(), note);
                if self.note_load_inflight.as_ref() == Some(&guid) {
                    self.note_load_inflight = None;
                }
                if self.loading_note_guid.as_ref() == Some(&guid) {
                    self.loading_note_guid = None;
                }
                if self.selected_summary().is_some_and(|s| s.guid == guid)
                    && (self.preferences.vim_preview
                        || self.pending_vim_guid.as_ref() == Some(&guid))
                {
                    self.pending_vim_guid = None;
                    self.ensure_vim_for_note(&guid);
                }
                self.pump_prefetch();
            }
            WorkerEvent::NoteLoadFailed { guid, message, .. } => {
                if self.note_load_inflight.as_ref() == Some(&guid) {
                    self.note_load_inflight = None;
                }
                if self.loading_note_guid.as_ref() == Some(&guid) {
                    self.loading_note_guid = None;
                }
                self.status_error = Some(format!("Load note: {message}"));
                if is_rate_limited(&message) {
                    self.prefetch_queue.clear();
                } else {
                    self.pump_prefetch();
                }
            }
            WorkerEvent::ContentSaved {
                guid,
                enml,
                updated,
            } => {
                if let Some(detail) = self.note_cache.get_mut(&guid) {
                    detail.enml = enml;
                    if updated.is_some() {
                        detail.updated = updated;
                    }
                }
                if let Some(sum) = self.notes.iter_mut().find(|n| n.guid == guid)
                    && updated.is_some()
                {
                    sum.updated = updated;
                }
                self.finish_saving("Saved");
            }
            WorkerEvent::TagsSaved {
                guid,
                tag_guids,
                tag_names,
                updated,
                all_tags,
            } => {
                if let Some(all) = all_tags {
                    self.tags = all;
                }
                if let Some(detail) = self.note_cache.get_mut(&guid) {
                    detail.tag_guids.clone_from(&tag_guids);
                    detail.tag_names = tag_names;
                    if updated.is_some() {
                        detail.updated = updated;
                    }
                }
                if let Some(sum) = self.notes.iter_mut().find(|n| n.guid == guid) {
                    sum.tag_guids = tag_guids;
                    if updated.is_some() {
                        sum.updated = updated;
                    }
                }
                self.finish_saving("Tags saved");
            }
            WorkerEvent::NotebookSaved {
                guid,
                notebook_guid,
                updated,
            } => {
                if let Some(detail) = self.note_cache.get_mut(&guid) {
                    detail.notebook_guid = Some(notebook_guid.clone());
                    if updated.is_some() {
                        detail.updated = updated;
                    }
                }
                if let Some(sum) = self.notes.iter_mut().find(|n| n.guid == guid) {
                    sum.notebook_guid = Some(notebook_guid);
                    if updated.is_some() {
                        sum.updated = updated;
                    }
                }
                self.finish_saving("Notebook updated");
            }
            WorkerEvent::NoteRenamed {
                guid,
                new_title,
                updated,
            } => {
                if let Some(detail) = self.note_cache.get_mut(&guid) {
                    detail.title.clone_from(&new_title);
                    if updated.is_some() {
                        detail.updated = updated;
                    }
                }
                if let Some(sum) = self.notes.iter_mut().find(|n| n.guid == guid) {
                    sum.title = new_title;
                    if updated.is_some() {
                        sum.updated = updated;
                    }
                }
                self.finish_saving("Renamed");
            }
            WorkerEvent::NoteCreated { note } => {
                let guid = note.guid.clone();
                let summary = NoteSummary {
                    guid: guid.clone(),
                    title: note.title.clone(),
                    created: note.created,
                    updated: note.updated,
                    notebook_guid: note.notebook_guid.clone(),
                    tag_guids: note.tag_guids.clone(),
                    content_length: note.content_length,
                };
                let prepared = PreparedNoteDocument::from_enml(&note.enml, &note.resources);
                self.prepared_docs.insert(guid.clone(), prepared);
                self.note_cache.insert(guid.clone(), note);

                self.notes.insert(0, summary);
                if self.notes.len() > MAX_LEFT_TITLES {
                    self.notes.truncate(MAX_LEFT_TITLES);
                }
                self.total_matching = self.total_matching.saturating_add(1);
                self.selected_index = 0;
                self.ensure_vim_for_note(&guid);
                self.focus = FocusTarget::RightVim;
                self.finish_saving("Created");
            }
            WorkerEvent::Error {
                context,
                message,
                was_save,
            } => {
                self.loading_list = false;
                self.loading_note_guid = None;
                let full = format!("{context}: {message}");
                if was_save {
                    self.save_indicator = SaveIndicator::Failed {
                        message: full,
                        until: Instant::now() + ERROR_BADGE_DURATION,
                    };
                } else {
                    self.status_error = Some(full);
                }
            }
        }
    }

    fn apply_search_page(&mut self, mut page: SearchPage) {
        if page.notes.len() > MAX_LEFT_TITLES {
            page.notes.truncate(MAX_LEFT_TITLES);
        }
        self.total_matching = page.total_matching;
        self.notes = page.notes;
        self.selected_index = 0;

        if self.notes.is_empty() {
            self.vim_session = None;
            self.loading_note_guid = None;
            self.prefetch_queue.clear();
        } else {
            self.on_selection_changed();
        }
    }

    /// Rebuilds the one-at-a-time fetch list for the notes currently on screen.
    /// The selected note is first so its preview is not stuck behind the rest.
    fn rebuild_prefetch_queue(&mut self) {
        let selected = self.selected_summary().map(|s| s.guid.clone());
        let visible: Vec<NoteGuid> = self.notes.iter().map(|n| n.guid.clone()).collect();
        let inflight = self.note_load_inflight.clone();
        let mut queue = VecDeque::new();
        let mut consider = |guid: NoteGuid| {
            if self.note_cache.contains_key(&guid)
                || inflight.as_ref() == Some(&guid)
                || queue.contains(&guid)
            {
                return;
            }
            queue.push_back(guid);
        };
        if let Some(guid) = selected {
            consider(guid);
        }
        for guid in visible {
            consider(guid);
        }
        self.prefetch_queue = queue;
    }

    fn prioritize_prefetch(&mut self, guid: NoteGuid) {
        if self.note_cache.contains_key(&guid) || self.note_load_inflight.as_ref() == Some(&guid) {
            return;
        }
        self.prefetch_queue.retain(|queued| queued != &guid);
        self.prefetch_queue.push_front(guid);
        self.pump_prefetch();
    }

    /// Sends the next content fetch only when the worker is idle.
    fn pump_prefetch(&mut self) {
        if self.loading_list || self.note_load_inflight.is_some() {
            return;
        }
        while let Some(guid) = self.prefetch_queue.pop_front() {
            if self.note_cache.contains_key(&guid) || !self.notes.iter().any(|n| n.guid == guid) {
                continue;
            }
            let request_id = self.alloc_req_id();
            self.latest_note_req = request_id;
            self.note_load_inflight = Some(guid.clone());
            if self.selected_summary().is_some_and(|s| s.guid == guid) {
                self.loading_note_guid = Some(guid.clone());
            }
            let _ = self
                .cmd_tx
                .send(WorkerCommand::LoadNote { request_id, guid });
            return;
        }
    }

    fn on_selection_changed(&mut self) {
        self.preview_scroll = 0;
        self.pending_vim_guid = None;

        let Some(guid) = self.selected_summary().map(|s| s.guid.clone()) else {
            self.vim_session = None;
            return;
        };

        if self
            .vim_session
            .as_ref()
            .is_some_and(|v| v.note_guid != guid)
        {
            self.vim_session = None;
        }

        if let Some(detail) = self.note_cache.get(&guid) {
            self.loading_note_guid = None;
            if !self.prepared_docs.contains_key(&guid) {
                let prepared = PreparedNoteDocument::from_enml(&detail.enml, &detail.resources);
                self.prepared_docs.insert(guid.clone(), prepared);
            }
            if self.preferences.vim_preview {
                self.ensure_vim_for_note(&guid);
            } else {
                self.vim_session = None;
            }
        } else {
            self.vim_session = None;
            self.loading_note_guid = Some(guid);
        }
        self.rebuild_prefetch_queue();
        self.pump_prefetch();
    }

    /// Replaces the text preview with real embedded Vim for editing the selected note.
    pub fn open_selected_in_vim(&mut self) {
        let Some(guid) = self.selected_summary().map(|s| s.guid.clone()) else {
            return;
        };
        self.focus = FocusTarget::RightVim;
        if self.note_cache.contains_key(&guid) {
            self.pending_vim_guid = None;
            self.ensure_vim_for_note(&guid);
        } else {
            self.pending_vim_guid = Some(guid.clone());
            self.loading_note_guid = Some(guid.clone());
            self.prioritize_prefetch(guid);
        }
    }

    pub fn scroll_preview(&mut self, delta: i16) {
        let total_lines = self
            .selected_preview_text()
            .map_or(0usize, |txt| txt.lines().count().max(1));
        let max_scroll = u16::try_from(total_lines.saturating_sub(1)).unwrap_or(u16::MAX);
        if delta >= 0 {
            let add = delta.unsigned_abs();
            self.preview_scroll = self.preview_scroll.saturating_add(add).min(max_scroll);
        } else {
            let sub = delta.unsigned_abs();
            self.preview_scroll = self.preview_scroll.saturating_sub(sub);
        }
    }

    pub fn ensure_vim_for_note(&mut self, guid: &NoteGuid) {
        if self
            .vim_session
            .as_ref()
            .is_some_and(|s| &s.note_guid == guid)
        {
            return;
        }
        let Some(detail) = self.note_cache.get(guid) else {
            return;
        };
        let md = self
            .prepared_docs
            .entry(guid.clone())
            .or_insert_with(|| PreparedNoteDocument::from_enml(&detail.enml, &detail.resources))
            .markdown
            .clone();

        let rows = self.geometry.vim_area.height.max(12);
        let cols = self.geometry.vim_area.width.max(40);
        match VimSession::spawn(guid.clone(), &md, rows, cols) {
            Ok(session) => {
                self.vim_session = Some(session);
            }
            Err(e) => {
                self.status_error = Some(format!("Failed to start vim: {e}"));
            }
        }
    }

    pub fn sync_vim_size(&mut self) {
        let area = self.geometry.vim_area;
        if area.width > 0
            && area.height > 0
            && let Some(vim) = &mut self.vim_session
        {
            vim.resize(area.height, area.width);
        }
    }

    fn alloc_req_id(&mut self) -> u64 {
        let id = self.next_request_id;
        self.next_request_id = self.next_request_id.wrapping_add(1);
        id
    }

    pub fn execute_search(&mut self) {
        self.active_query = self.search_input.text.trim().to_owned();
        let parsed = ParsedSearch::parse(&self.active_query);
        let filter = parsed.compile(&self.notebooks);
        let req_id = self.alloc_req_id();
        self.latest_search_req = req_id;
        self.loading_list = true;
        self.prefetch_queue.clear();
        self.status_error = None;
        let _ = self.cmd_tx.send(WorkerCommand::Search {
            request_id: req_id,
            filter,
        });
    }

    pub fn toggle_left_panel(&mut self) {
        self.left_panel_visible = !self.left_panel_visible;
        if !self.left_panel_visible && self.focus == FocusTarget::LeftTitles {
            self.focus = FocusTarget::RightVim;
        }
    }

    pub fn open_tag_modal(&mut self) {
        let Some(detail) = self.selected_detail().cloned() else {
            return;
        };
        self.modal = ModalState::TagEditor(TagEditorModal {
            note_guid: detail.guid,
            note_title: detail.title,
            filter: TextInput::default(),
            selected_tags: detail.tag_names,
            cursor: 0,
        });
    }

    pub fn open_notebook_modal(&mut self) {
        let Some(detail) = self.selected_detail().cloned() else {
            return;
        };
        let cursor = detail
            .notebook_guid
            .as_ref()
            .and_then(|cur| self.notebooks.iter().position(|nb| &nb.guid == cur))
            .unwrap_or(0);
        self.modal = ModalState::NotebookPicker(NotebookPickerModal {
            note_guid: detail.guid,
            note_title: detail.title,
            current_notebook: detail.notebook_guid,
            filter: TextInput::default(),
            cursor,
        });
    }

    pub fn open_rename_modal(&mut self) {
        let Some(summary) = self.selected_summary().cloned() else {
            return;
        };
        self.modal = ModalState::TextPrompt(TextPromptModal {
            kind: PromptKind::RenameNote,
            note_guid: Some(summary.guid),
            input: TextInput::new(summary.title),
        });
    }

    pub fn open_create_note_modal(&mut self) {
        self.modal = ModalState::TextPrompt(TextPromptModal {
            kind: PromptKind::CreateNote,
            note_guid: None,
            input: TextInput::default(),
        });
    }

    pub fn open_preferences_modal(&mut self) {
        self.modal = ModalState::Preferences(PreferencesModal::default());
    }

    pub fn toggle_preference_at(&mut self, index: usize) {
        if index == 0 {
            self.preferences.vim_preview = !self.preferences.vim_preview;
            if let Some(path) = &self.preferences_path
                && let Err(e) = self.preferences.save(path)
            {
                self.status_error = Some(format!("Failed to save preferences: {e}"));
            }

            if self.preferences.vim_preview {
                if let Some(guid) = self.selected_summary().map(|s| s.guid.clone())
                    && self.note_cache.contains_key(&guid)
                {
                    self.ensure_vim_for_note(&guid);
                }
            } else if self.focus != FocusTarget::RightVim {
                self.vim_session = None;
            }
        }
    }

    pub fn handle_terminal_event(&mut self, event: Event) {
        match event {
            Event::Key(key) if key.kind == KeyEventKind::Press => self.handle_key(key),
            Event::Mouse(mouse) => self.handle_mouse(mouse),
            Event::Resize(_, _) => {
                // LayoutGeometry is recomputed on next render, which calls sync_vim_size().
            }
            _ => {}
        }
    }

    fn handle_key(&mut self, key: KeyEvent) {
        if !matches!(self.modal, ModalState::None) {
            self.handle_modal_key(key);
            return;
        }

        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);

        // Global shortcuts available even while editing inside real Vim.
        match key.code {
            KeyCode::Char('q') if ctrl => {
                self.should_quit = true;
                return;
            }
            KeyCode::F(1) => {
                self.modal = ModalState::Help;
                return;
            }
            KeyCode::F(2) => {
                self.toggle_left_panel();
                return;
            }
            KeyCode::Char('b') if alt => {
                self.toggle_left_panel();
                return;
            }
            KeyCode::F(3) => {
                self.focus = FocusTarget::SearchInput;
                return;
            }
            KeyCode::Char('s') if alt => {
                self.focus = FocusTarget::SearchInput;
                return;
            }
            KeyCode::F(4) => {
                self.open_tag_modal();
                return;
            }
            KeyCode::Char('t') if alt => {
                self.open_tag_modal();
                return;
            }
            KeyCode::F(5) => {
                self.open_notebook_modal();
                return;
            }
            KeyCode::Char('n') if alt => {
                self.open_notebook_modal();
                return;
            }
            KeyCode::F(6) => {
                self.open_rename_modal();
                return;
            }
            KeyCode::Char('r') if alt => {
                self.open_rename_modal();
                return;
            }
            KeyCode::F(7) => {
                self.open_preferences_modal();
                return;
            }
            KeyCode::Char('p') if alt => {
                self.open_preferences_modal();
                return;
            }
            KeyCode::Char('\\') if ctrl => {
                if !self.left_panel_visible {
                    self.left_panel_visible = true;
                }
                self.focus = FocusTarget::LeftTitles;
                return;
            }
            KeyCode::Char('h') if alt => {
                if !self.left_panel_visible {
                    self.left_panel_visible = true;
                }
                self.focus = FocusTarget::LeftTitles;
                return;
            }
            KeyCode::Char('l') if alt => {
                self.focus = FocusTarget::RightVim;
                return;
            }
            _ => {}
        }

        match self.focus {
            FocusTarget::LeftTitles => self.handle_left_panel_key(key),
            FocusTarget::SearchInput => self.handle_search_key(key),
            FocusTarget::RightVim => self.handle_right_panel_key(key),
        }
    }

    fn handle_right_panel_key(&mut self, key: KeyEvent) {
        if let Some(vim) = &mut self.vim_session {
            vim.send_key(key);
            return;
        }

        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Enter | KeyCode::Char('e' | 'i') if self.selected_summary().is_some() => {
                self.open_selected_in_vim();
            }
            KeyCode::Esc | KeyCode::Tab | KeyCode::Char('h') | KeyCode::Left => {
                if !self.left_panel_visible {
                    self.left_panel_visible = true;
                }
                self.focus = FocusTarget::LeftTitles;
            }
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char('?') => self.modal = ModalState::Help,
            KeyCode::Char('p') if !ctrl => self.open_preferences_modal(),
            KeyCode::Char('b') => self.toggle_left_panel(),
            KeyCode::Char('/') => self.focus = FocusTarget::SearchInput,
            KeyCode::Char('t') => self.open_tag_modal(),
            KeyCode::Char('n') => self.open_notebook_modal(),
            KeyCode::Char('r') if !ctrl => self.open_rename_modal(),
            KeyCode::Char('c') if !ctrl => self.open_create_note_modal(),
            KeyCode::Char(digit @ '1'..='9')
                if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                let idx = usize::from(digit as u8 - b'1');
                self.set_selection(idx);
            }
            KeyCode::Char('j') | KeyCode::Down => self.scroll_preview(1),
            KeyCode::Char('k') | KeyCode::Up => self.scroll_preview(-1),
            KeyCode::PageDown | KeyCode::Char(' ') => self.scroll_preview(12),
            KeyCode::PageUp => self.scroll_preview(-12),
            KeyCode::Char('d') if ctrl => self.scroll_preview(10),
            KeyCode::Char('u') if ctrl => self.scroll_preview(-10),
            KeyCode::Char('g') | KeyCode::Home => self.preview_scroll = 0,
            KeyCode::Char('G') | KeyCode::End => self.scroll_preview(i16::MAX),
            _ => {}
        }
    }

    fn handle_left_panel_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char('?') => self.modal = ModalState::Help,
            KeyCode::Char('p') if !ctrl => self.open_preferences_modal(),
            KeyCode::Char('b') => self.toggle_left_panel(),
            KeyCode::Char('/') => self.focus = FocusTarget::SearchInput,
            KeyCode::Char('t') => self.open_tag_modal(),
            KeyCode::Char('n') => self.open_notebook_modal(),
            KeyCode::Char('r') if !ctrl => self.open_rename_modal(),
            KeyCode::Char('R') => self.execute_search(),
            KeyCode::Char('r') if ctrl => self.execute_search(),
            KeyCode::Char('c') if !ctrl => self.open_create_note_modal(),
            KeyCode::Char(digit @ '1'..='9')
                if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                let idx = usize::from(digit as u8 - b'1');
                self.set_selection(idx);
            }
            KeyCode::Char('j') | KeyCode::Down => self.move_selection(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_selection(-1),
            KeyCode::PageDown => self.scroll_preview(12),
            KeyCode::PageUp => self.scroll_preview(-12),
            KeyCode::Char('d') if ctrl => self.scroll_preview(10),
            KeyCode::Char('u') if ctrl => self.scroll_preview(-10),
            KeyCode::Char('g') | KeyCode::Home => self.set_selection(0),
            KeyCode::Char('G') | KeyCode::End => {
                if !self.notes.is_empty() {
                    self.set_selection(self.notes.len() - 1);
                }
            }
            KeyCode::Enter | KeyCode::Char('e' | 'i') if self.selected_summary().is_some() => {
                self.open_selected_in_vim();
            }
            KeyCode::Tab | KeyCode::Char('l') | KeyCode::Right
                if self.selected_summary().is_some() =>
            {
                self.focus = FocusTarget::RightVim;
            }
            _ => {}
        }
    }

    fn handle_search_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.search_input = TextInput::new(self.active_query.clone());
                self.focus = if self.left_panel_visible {
                    FocusTarget::LeftTitles
                } else {
                    FocusTarget::RightVim
                };
            }
            KeyCode::Enter => {
                self.execute_search();
                self.focus = if self.left_panel_visible {
                    FocusTarget::LeftTitles
                } else {
                    FocusTarget::RightVim
                };
            }
            KeyCode::Down | KeyCode::Tab => {
                self.focus = if self.left_panel_visible {
                    FocusTarget::LeftTitles
                } else {
                    FocusTarget::RightVim
                };
            }
            _ => {
                self.search_input.handle_key(key);
            }
        }
    }

    fn handle_modal_key(&mut self, key: KeyEvent) {
        let current_modal = std::mem::take(&mut self.modal);
        match current_modal {
            ModalState::None => {}
            ModalState::Help => {
                if !matches!(
                    key.code,
                    KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q' | '?') | KeyCode::F(1)
                ) {
                    self.modal = ModalState::Help;
                }
            }
            ModalState::Preferences(mut pm) => match key.code {
                KeyCode::Esc | KeyCode::Char('q' | 'p') | KeyCode::F(7) => {}
                KeyCode::Up | KeyCode::Char('k') => {
                    pm.cursor = pm.cursor.saturating_sub(1);
                    self.modal = ModalState::Preferences(pm);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    if pm.cursor + 1 < PREFERENCES_ITEMS_COUNT {
                        pm.cursor += 1;
                    }
                    self.modal = ModalState::Preferences(pm);
                }
                KeyCode::Char(' ') | KeyCode::Enter => {
                    self.toggle_preference_at(pm.cursor);
                    self.modal = ModalState::Preferences(pm);
                }
                _ => {
                    self.modal = ModalState::Preferences(pm);
                }
            },
            ModalState::TagEditor(mut tm) => {
                let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                let rows = tm.visible_rows(&self.tags);
                tm.clamp_cursor(rows.len());

                match key.code {
                    KeyCode::Esc => {}
                    KeyCode::Up => {
                        tm.cursor = tm.cursor.saturating_sub(1);
                        self.modal = ModalState::TagEditor(tm);
                    }
                    KeyCode::Char('p') if ctrl => {
                        tm.cursor = tm.cursor.saturating_sub(1);
                        self.modal = ModalState::TagEditor(tm);
                    }
                    KeyCode::Down => {
                        if tm.cursor + 1 < rows.len() {
                            tm.cursor += 1;
                        }
                        self.modal = ModalState::TagEditor(tm);
                    }
                    KeyCode::Char('n') if ctrl => {
                        if tm.cursor + 1 < rows.len() {
                            tm.cursor += 1;
                        }
                        self.modal = ModalState::TagEditor(tm);
                    }
                    KeyCode::Char(' ') | KeyCode::Tab => {
                        if let Some(row) = rows.get(tm.cursor) {
                            match row {
                                TagModalRow::CreateNew(new_tag) => {
                                    tm.toggle_name(new_tag);
                                    tm.filter.clear();
                                    tm.cursor = 0;
                                }
                                TagModalRow::Existing { name, .. } => {
                                    tm.toggle_name(name);
                                }
                            }
                        }
                        self.modal = ModalState::TagEditor(tm);
                    }
                    KeyCode::Enter => {
                        if let Some(TagModalRow::CreateNew(new_tag)) = rows.get(tm.cursor) {
                            tm.toggle_name(new_tag);
                        }
                        self.begin_saving("Saving tags");
                        let _ = self.cmd_tx.send(WorkerCommand::SetTags {
                            guid: tm.note_guid,
                            title: tm.note_title,
                            tag_names: tm.selected_tags,
                        });
                    }
                    _ => {
                        if tm.filter.handle_key(key) {
                            let new_len = tm.visible_rows(&self.tags).len();
                            tm.clamp_cursor(new_len);
                        }
                        self.modal = ModalState::TagEditor(tm);
                    }
                }
            }
            ModalState::NotebookPicker(mut nm) => {
                let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                let filtered = nm.filtered(&self.notebooks);
                let len = filtered.len();
                nm.clamp_cursor(len);

                match key.code {
                    KeyCode::Esc => {}
                    KeyCode::Up => {
                        nm.cursor = nm.cursor.saturating_sub(1);
                        self.modal = ModalState::NotebookPicker(nm);
                    }
                    KeyCode::Char('p') if ctrl => {
                        nm.cursor = nm.cursor.saturating_sub(1);
                        self.modal = ModalState::NotebookPicker(nm);
                    }
                    KeyCode::Down => {
                        if nm.cursor + 1 < len {
                            nm.cursor += 1;
                        }
                        self.modal = ModalState::NotebookPicker(nm);
                    }
                    KeyCode::Char('n') if ctrl => {
                        if nm.cursor + 1 < len {
                            nm.cursor += 1;
                        }
                        self.modal = ModalState::NotebookPicker(nm);
                    }
                    KeyCode::Enter => {
                        if let Some(chosen) = filtered.get(nm.cursor) {
                            let chosen_guid = chosen.guid.clone();
                            self.begin_saving("Moving notebook");
                            let _ = self.cmd_tx.send(WorkerCommand::SetNotebook {
                                guid: nm.note_guid,
                                title: nm.note_title,
                                notebook_guid: chosen_guid,
                            });
                        }
                    }
                    _ => {
                        if nm.filter.handle_key(key) {
                            let new_len = nm.filtered(&self.notebooks).len();
                            nm.clamp_cursor(new_len);
                        }
                        self.modal = ModalState::NotebookPicker(nm);
                    }
                }
            }
            ModalState::TextPrompt(mut pm) => match key.code {
                KeyCode::Esc => {}
                KeyCode::Enter => {
                    let val = pm.input.text.trim().to_owned();
                    if !val.is_empty() {
                        match pm.kind {
                            PromptKind::RenameNote => {
                                if let Some(guid) = pm.note_guid {
                                    self.begin_saving("Renaming note");
                                    let _ = self.cmd_tx.send(WorkerCommand::RenameNote {
                                        guid,
                                        new_title: val,
                                    });
                                }
                            }
                            PromptKind::CreateNote => {
                                let default_nb = self
                                    .selected_detail()
                                    .and_then(|d| d.notebook_guid.clone())
                                    .or_else(|| {
                                        self.notebooks
                                            .iter()
                                            .find(|nb| nb.is_default)
                                            .map(|nb| nb.guid.clone())
                                    });
                                self.begin_saving("Creating note");
                                let _ = self.cmd_tx.send(WorkerCommand::CreateNote {
                                    title: val,
                                    notebook_guid: default_nb,
                                });
                            }
                        }
                    }
                }
                _ => {
                    pm.input.handle_key(key);
                    self.modal = ModalState::TextPrompt(pm);
                }
            },
        }
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) {
        let col = mouse.column;
        let row = mouse.row;

        // Handle active modal mouse clicks / scrolling first.
        if !matches!(self.modal, ModalState::None) {
            self.handle_modal_mouse(mouse);
            return;
        }

        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                // Check vertical separator grab zone first.
                if let Some(sep_x) = self.geometry.separator_x {
                    let (y0, y1) = self.geometry.split_y_range;
                    if row >= y0 && row < y1 && col.abs_diff(sep_x) <= 1 {
                        self.dragging_separator = true;
                        return;
                    }
                }

                if rect_contains(self.geometry.toggle_left_btn, col, row) {
                    self.toggle_left_panel();
                    return;
                }

                if rect_contains(self.geometry.search_bar, col, row) {
                    self.focus = FocusTarget::SearchInput;
                    return;
                }

                if let Some(list_area) = self.geometry.left_titles_inner
                    && rect_contains(list_area, col, row)
                {
                    self.focus = FocusTarget::LeftTitles;
                    let clicked_idx = usize::from(row - list_area.y);
                    if clicked_idx < self.notes.len() {
                        self.set_selection(clicked_idx);
                    }
                    return;
                }

                if rect_contains(self.geometry.notebook_hit, col, row) {
                    self.open_notebook_modal();
                    return;
                }

                if rect_contains(self.geometry.tags_hit, col, row) {
                    self.open_tag_modal();
                    return;
                }

                if rect_contains(self.geometry.title_hit, col, row) {
                    self.open_rename_modal();
                    return;
                }

                if rect_contains(self.geometry.vim_area, col, row) {
                    self.focus = FocusTarget::RightVim;
                    let vim_area = self.geometry.vim_area;
                    if let Some(vim) = &mut self.vim_session {
                        vim.send_mouse(mouse, vim_area);
                    }
                }
            }
            MouseEventKind::Drag(MouseButton::Left) if self.dragging_separator => {
                self.left_width = LeftPanelWidthCols::new(col, self.geometry.terminal_width);
                self.sync_vim_size();
            }
            MouseEventKind::Drag(_) => {
                if self.focus == FocusTarget::RightVim
                    && rect_contains(self.geometry.vim_area, col, row)
                {
                    let vim_area = self.geometry.vim_area;
                    if let Some(vim) = &mut self.vim_session {
                        vim.send_mouse(mouse, vim_area);
                    }
                }
            }
            MouseEventKind::Up(_) => {
                if self.dragging_separator {
                    self.dragging_separator = false;
                } else if self.focus == FocusTarget::RightVim
                    && rect_contains(self.geometry.vim_area, col, row)
                {
                    let vim_area = self.geometry.vim_area;
                    if let Some(vim) = &mut self.vim_session {
                        vim.send_mouse(mouse, vim_area);
                    }
                }
            }
            MouseEventKind::ScrollUp => {
                if self
                    .geometry
                    .left_titles_inner
                    .is_some_and(|r| rect_contains(r, col, row))
                {
                    self.move_selection(-1);
                } else if rect_contains(self.geometry.vim_area, col, row) {
                    let vim_area = self.geometry.vim_area;
                    if let Some(vim) = &mut self.vim_session {
                        vim.send_mouse(mouse, vim_area);
                    } else {
                        self.scroll_preview(-3);
                    }
                }
            }
            MouseEventKind::ScrollDown => {
                if self
                    .geometry
                    .left_titles_inner
                    .is_some_and(|r| rect_contains(r, col, row))
                {
                    self.move_selection(1);
                } else if rect_contains(self.geometry.vim_area, col, row) {
                    let vim_area = self.geometry.vim_area;
                    if let Some(vim) = &mut self.vim_session {
                        vim.send_mouse(mouse, vim_area);
                    } else {
                        self.scroll_preview(3);
                    }
                }
            }
            _ => {}
        }
    }

    fn handle_modal_mouse(&mut self, mouse: MouseEvent) {
        let Some(inner) = self.geometry.modal_inner else {
            return;
        };
        let modal_bounds = self.geometry.modal_area.unwrap_or(inner);
        let col = mouse.column;
        let row = mouse.row;

        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if !rect_contains(modal_bounds, col, row) {
                    self.modal = ModalState::None;
                    return;
                }
                if let ModalState::Preferences(mut pm) = self.modal {
                    if row > inner.y {
                        let clicked_idx = usize::from(row - (inner.y + 1));
                        if clicked_idx < PREFERENCES_ITEMS_COUNT {
                            pm.cursor = clicked_idx;
                            self.toggle_preference_at(clicked_idx);
                            self.modal = ModalState::Preferences(pm);
                        }
                    }
                    return;
                }
                // List rows inside borderless TagEditor / NotebookPicker begin at inner.y + 2
                if row >= inner.y + 2 {
                    let clicked_row = usize::from(row - (inner.y + 2));
                    let modal = std::mem::take(&mut self.modal);
                    match modal {
                        ModalState::TagEditor(mut tm) => {
                            let rows = tm.visible_rows(&self.tags);
                            if let Some(item) = rows.get(clicked_row) {
                                tm.cursor = clicked_row;
                                match item {
                                    TagModalRow::CreateNew(n) => {
                                        tm.toggle_name(n);
                                        tm.filter.clear();
                                    }
                                    TagModalRow::Existing { name, .. } => tm.toggle_name(name),
                                }
                            }
                            self.modal = ModalState::TagEditor(tm);
                        }
                        ModalState::NotebookPicker(nm) => {
                            let filtered = nm.filtered(&self.notebooks);
                            if let Some(chosen) = filtered.get(clicked_row) {
                                let chosen_guid = chosen.guid.clone();
                                self.begin_saving("Moving notebook");
                                let _ = self.cmd_tx.send(WorkerCommand::SetNotebook {
                                    guid: nm.note_guid,
                                    title: nm.note_title,
                                    notebook_guid: chosen_guid,
                                });
                            } else {
                                self.modal = ModalState::NotebookPicker(nm);
                            }
                        }
                        other => self.modal = other,
                    }
                }
            }
            MouseEventKind::ScrollUp => match &mut self.modal {
                ModalState::TagEditor(tm) => tm.cursor = tm.cursor.saturating_sub(1),
                ModalState::NotebookPicker(nm) => nm.cursor = nm.cursor.saturating_sub(1),
                _ => {}
            },
            MouseEventKind::ScrollDown => match &mut self.modal {
                ModalState::TagEditor(tm) => {
                    let len = tm.visible_rows(&self.tags).len();
                    if tm.cursor + 1 < len {
                        tm.cursor += 1;
                    }
                }
                ModalState::NotebookPicker(nm) => {
                    let len = nm.filtered(&self.notebooks).len();
                    if nm.cursor + 1 < len {
                        nm.cursor += 1;
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }

    pub fn move_selection(&mut self, delta: isize) {
        if self.notes.is_empty() {
            return;
        }
        let cur = isize::try_from(self.selected_index).unwrap_or(0);
        let max = isize::try_from(self.notes.len() - 1).unwrap_or(0);
        let next = usize::try_from((cur + delta).clamp(0, max)).unwrap_or(0);
        self.set_selection(next);
    }

    pub fn set_selection(&mut self, index: usize) {
        if index >= self.notes.len() || index == self.selected_index {
            return;
        }
        self.selected_index = index;
        self.on_selection_changed();
    }
}

fn is_rate_limited(message: &str) -> bool {
    message.contains("RATE_LIMIT_REACHED") || message.contains("429")
}

fn rect_contains(r: Rect, col: u16, row: u16) -> bool {
    col >= r.x
        && col < r.x.saturating_add(r.width)
        && row >= r.y
        && row < r.y.saturating_add(r.height)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn enforces_20_titles_limit_and_separator_drag_resize() {
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(tx);

        let many_notes: Vec<NoteSummary> = (1..=30)
            .map(|i| NoteSummary {
                guid: NoteGuid(format!("g-{i}")),
                title: format!("Note {i}"),
                created: None,
                updated: None,
                notebook_guid: None,
                tag_guids: Vec::new(),
                content_length: None,
            })
            .collect();

        app.handle_worker_event(WorkerEvent::Bootstrapped {
            notebooks: Vec::new(),
            tags: Vec::new(),
            page: SearchPage {
                total_matching: 30,
                notes: many_notes,
            },
        });

        assert_eq!(app.notes.len(), MAX_LEFT_TITLES);
        assert_eq!(app.notes.len(), 20);

        // Simulate dragging vertical separator by mouse.
        app.geometry.terminal_width = 120;
        app.geometry.separator_x = Some(34);
        app.geometry.split_y_range = (1, 35);

        app.handle_terminal_event(Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 34,
            row: 10,
            modifiers: KeyModifiers::empty(),
        }));
        assert!(app.dragging_separator);

        app.handle_terminal_event(Event::Mouse(MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: 48,
            row: 10,
            modifiers: KeyModifiers::empty(),
        }));
        assert_eq!(app.left_width.get(), 48);

        app.handle_terminal_event(Event::Mouse(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: 48,
            row: 10,
            modifiers: KeyModifiers::empty(),
        }));
        assert!(!app.dragging_separator);
    }

    #[test]
    fn toggles_left_panel_visibility() {
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(tx);
        assert!(app.left_panel_visible);

        app.toggle_left_panel();
        assert!(!app.left_panel_visible);
        assert_eq!(app.focus, FocusTarget::RightVim);

        app.toggle_left_panel();
        assert!(app.left_panel_visible);
    }

    #[test]
    fn preferences_modal_toggles_and_persists_vim_preview() {
        let dir = tempfile::tempdir().unwrap();
        let prefs_file = dir.path().join("preferences.conf");
        let (tx, _rx) = mpsc::channel();
        let mut app = App::with_preferences(tx, Preferences::default(), Some(prefs_file.clone()));
        assert!(!app.preferences.vim_preview);

        // Press `p` -> open Preferences, `Space` -> check "Vim preview", `p` -> close.
        app.handle_terminal_event(Event::Key(KeyEvent::new(
            KeyCode::Char('p'),
            KeyModifiers::empty(),
        )));
        assert!(matches!(app.modal, ModalState::Preferences(_)));

        app.handle_terminal_event(Event::Key(KeyEvent::new(
            KeyCode::Char(' '),
            KeyModifiers::empty(),
        )));
        assert!(app.preferences.vim_preview);
        assert!(Preferences::load_or_default(&prefs_file).vim_preview);

        app.handle_terminal_event(Event::Key(KeyEvent::new(
            KeyCode::Char('p'),
            KeyModifiers::empty(),
        )));
        assert!(matches!(app.modal, ModalState::None));
    }

    #[test]
    fn digit_keys_1_to_9_select_corresponding_note_index() {
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(tx);
        let notes: Vec<NoteSummary> = (1..=5)
            .map(|i| NoteSummary {
                guid: NoteGuid(format!("g-{i}")),
                title: format!("Note {i}"),
                created: None,
                updated: None,
                notebook_guid: None,
                tag_guids: Vec::new(),
                content_length: None,
            })
            .collect();

        app.handle_worker_event(WorkerEvent::Bootstrapped {
            notebooks: Vec::new(),
            tags: Vec::new(),
            page: SearchPage {
                total_matching: 5,
                notes,
            },
        });
        assert_eq!(app.selected_index, 0);

        // Press '4' in LeftTitles -> selects index 3 (4th note).
        app.handle_terminal_event(Event::Key(KeyEvent::new(
            KeyCode::Char('4'),
            KeyModifiers::empty(),
        )));
        assert_eq!(app.selected_index, 3);

        // Out-of-bounds digit ('9' when only 5 notes exist) and '0' are ignored.
        app.handle_terminal_event(Event::Key(KeyEvent::new(
            KeyCode::Char('9'),
            KeyModifiers::empty(),
        )));
        assert_eq!(app.selected_index, 3);
        app.handle_terminal_event(Event::Key(KeyEvent::new(
            KeyCode::Char('0'),
            KeyModifiers::empty(),
        )));
        assert_eq!(app.selected_index, 3);

        // Press '2' in native text preview (RightVim without active Vim session) -> selects index 1.
        app.focus = FocusTarget::RightVim;
        app.handle_terminal_event(Event::Key(KeyEvent::new(
            KeyCode::Char('2'),
            KeyModifiers::empty(),
        )));
        assert_eq!(app.selected_index, 1);

        // Typing digits in SearchInput appends to search text without switching selection.
        app.focus = FocusTarget::SearchInput;
        app.handle_terminal_event(Event::Key(KeyEvent::new(
            KeyCode::Char('5'),
            KeyModifiers::empty(),
        )));
        assert_eq!(app.selected_index, 1);
        assert_eq!(app.search_input.text, "5");
    }

    #[test]
    fn prefetches_visible_notes_one_at_a_time_and_stops_on_rate_limit() {
        let (tx, rx) = mpsc::channel();
        let mut app = App::new(tx);
        assert!(matches!(rx.try_recv(), Ok(WorkerCommand::Bootstrap)));

        let notes: Vec<NoteSummary> = (1..=3)
            .map(|i| NoteSummary {
                guid: NoteGuid(format!("g-{i}")),
                title: format!("Note {i}"),
                created: None,
                updated: None,
                notebook_guid: None,
                tag_guids: Vec::new(),
                content_length: None,
            })
            .collect();

        app.handle_worker_event(WorkerEvent::Bootstrapped {
            notebooks: Vec::new(),
            tags: Vec::new(),
            page: SearchPage {
                total_matching: 3,
                notes,
            },
        });

        assert_eq!(take_load(&rx), "g-1");
        assert!(rx.try_recv().is_err());

        app.set_selection(2);
        assert!(rx.try_recv().is_err());

        app.handle_worker_event(WorkerEvent::NoteLoaded {
            request_id: 1,
            note: detail("g-1"),
        });
        assert_eq!(take_load(&rx), "g-3");

        app.handle_worker_event(WorkerEvent::NoteLoadFailed {
            request_id: 2,
            guid: NoteGuid::from("g-3"),
            message: "Evernote NOT_FOUND".to_owned(),
        });
        assert_eq!(take_load(&rx), "g-2");

        app.handle_worker_event(WorkerEvent::NoteLoadFailed {
            request_id: 3,
            guid: NoteGuid::from("g-2"),
            message: "Evernote RATE_LIMIT_REACHED (19)".to_owned(),
        });
        assert!(rx.try_recv().is_err());
        assert!(app.note_cache.contains_key(&NoteGuid::from("g-1")));
        assert!(!app.note_cache.contains_key(&NoteGuid::from("g-2")));

        app.execute_search();
        assert!(matches!(rx.try_recv(), Ok(WorkerCommand::Search { .. })));
        assert!(rx.try_recv().is_err());
    }

    fn take_load(rx: &mpsc::Receiver<WorkerCommand>) -> String {
        match rx.try_recv() {
            Ok(WorkerCommand::LoadNote { guid, .. }) => guid.0,
            other => panic!("expected one LoadNote, got {other:?}"),
        }
    }

    fn detail(guid: &str) -> NoteDetail {
        NoteDetail {
            guid: NoteGuid::from(guid),
            title: guid.to_owned(),
            enml: "<en-note><div>body</div></en-note>".to_owned(),
            created: None,
            updated: None,
            notebook_guid: None,
            tag_guids: Vec::new(),
            tag_names: Vec::new(),
            source_url: None,
            content_length: Some(4),
            resources: Vec::new(),
        }
    }
}
