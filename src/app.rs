use crate::domain::{
    LeftPanelWidthCols, MAX_LEFT_TITLES, NoteDetail, NoteGuid, NoteSummary, Notebook, NotebookGuid,
    SearchPage, Tag,
};
use crate::enml::PreparedNoteDocument;
use crate::evernote_api::normalize_source_url;
use crate::preferences::Preferences;
use crate::search::ParsedSearch;
use crate::vim_pane::VimSession;
use crate::wake::Waker;
use crate::worker::{CATALOG_CONTEXT, INITIAL_SYNC_CONTEXT, SaveRetry, WorkerCommand, WorkerEvent};
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::Rect;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

/// Spinner and progress-bar frame time while something is loading or saving.
pub const ANIMATION_FRAME: Duration = Duration::from_millis(50);
const MIN_SAVE_ANIMATION_DURATION: Duration = Duration::from_millis(400);
const SAVED_BADGE_DURATION: Duration = Duration::from_secs(2);
const ERROR_BADGE_DURATION: Duration = Duration::from_secs(6);
/// Waits between automatic retries of a failed save; the last one repeats.
const SAVE_RETRY_DELAYS: [Duration; 5] = [
    Duration::from_secs(2),
    Duration::from_secs(5),
    Duration::from_secs(15),
    Duration::from_secs(30),
    Duration::from_secs(60),
];
/// Shown after the first `q` while saves are still on their way to Evernote.
pub const QUIT_AFTER_SYNC_MESSAGE: &str = "Saving first; q again: quit, lose it";

/// Shown while a reply that would close the note open in Vim waits.
pub const DEFERRED_REPLY_MESSAGE: &str = "Results ready: leave Vim to see them";

/// The tag editor replaces a note's whole tag set, so it must not open with names missing.
pub const TAGS_NOT_LOADED_MESSAGE: &str = "Tag names not loaded yet: try again";

/// A spare Vim that dies this many times is not restarted again (Vim missing or broken).
const SPARE_VIM_MAX_FAILURES: u8 = 3;

/// Fits the status bar's 38 columns.
pub const UNSAVED_VIM_MESSAGE: &str = "Unsaved in Vim: :w save, :q! discard";

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
    SetSourceUrl,
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
    /// First title row shown. Kept across frames so the list scrolls only when the
    /// selection leaves the view, and so clicks map to the right note.
    pub left_titles_offset: usize,
    pub separator_x: Option<u16>,
    pub split_y_range: (u16, u16),
    pub title_hit: Rect,
    pub notebook_hit: Rect,
    pub tags_hit: Rect,
    pub url_hit: Rect,
    pub vim_area: Rect,
    pub modal_area: Option<Rect>,
    pub modal_inner: Option<Rect>,
}

/// One search result screen, kept so back and forward do not hit the network.
#[derive(Clone, Debug)]
struct SearchScreen {
    query: String,
    total_matching: usize,
    notes: Vec<NoteSummary>,
    selected_index: usize,
    preview_scroll: u16,
}

/// Note content saved in Vim that Evernote has not confirmed yet.
#[derive(Debug, Clone)]
struct UnsyncedNote {
    title: String,
    /// The latest content; older saves of the same note no longer matter.
    enml: String,
    failed_attempts: usize,
    /// When to send it again. `None` while a save is on its way, or after an error that
    /// retrying cannot fix (then the next `:w` sends it again).
    retry_at: Option<Instant>,
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
    search_history: Vec<SearchScreen>,
    search_history_pos: usize,
    /// Query for the search that has been sent and not yet applied.
    pending_search_query: Option<String>,
    pub total_matching: usize,
    pub notes: Vec<NoteSummary>,
    pub selected_index: usize,

    pub notebooks: Vec<Notebook>,
    pub tags: Vec<Tag>,
    /// Set once `listNotebooks` / `listTags` finish or fail. Until then notebook names stay hidden.
    catalog_ready: bool,
    catalog_requested: bool,

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

    pub save_indicator: SaveIndicator,
    pub anim_tick: usize,
    pub status_error: Option<String>,
    unsynced: HashMap<NoteGuid, UnsyncedNote>,
    /// `q` was pressed while saves were pending: quit as soon as they land.
    quit_after_sync: bool,
    /// Replies that would change the selected note, and so close Vim, held back while
    /// Vim is in use. Applied in order once it is safe (`replay_deferred_replies`).
    deferred_replies: Vec<WorkerEvent>,
    /// Title each note had before a rename Evernote has not confirmed yet, to put back if
    /// the rename fails. The new title is shown and sent with every save meanwhile.
    pending_renames: HashMap<NoteGuid, String>,
    /// An idle Vim started ahead of time, so opening a note skips Vim's startup.
    spare_vim: Option<VimSession>,
    spare_vim_enabled: bool,
    spare_vim_failures: u8,

    pub preferences: Preferences,
    pub preferences_path: Option<PathBuf>,
    pub preview_scroll: u16,
    pub pending_vim_guid: Option<NoteGuid>,

    pub vim_session: Option<VimSession>,
    pub geometry: LayoutGeometry,

    cmd_tx: Sender<WorkerCommand>,
    /// Given to each Vim session so its output and exit wake the UI loop.
    waker: Waker,
    last_tick: Instant,
}

impl App {
    pub fn new(cmd_tx: Sender<WorkerCommand>) -> Self {
        Self::with_preferences(cmd_tx, Preferences::default(), None, None)
    }

    /// With `initial_query`, startup sends only that search and skips the recent-notes
    /// bootstrap, so the list never flashes notes the user did not ask for.
    pub fn with_preferences(
        cmd_tx: Sender<WorkerCommand>,
        preferences: Preferences,
        preferences_path: Option<PathBuf>,
        initial_query: Option<String>,
    ) -> Self {
        let mut app = Self {
            should_quit: false,
            focus: FocusTarget::LeftTitles,
            modal: ModalState::None,
            left_panel_visible: true,
            left_width: LeftPanelWidthCols::default(),
            dragging_separator: false,

            search_input: TextInput::default(),
            active_query: String::new(),
            search_history: Vec::new(),
            search_history_pos: 0,
            pending_search_query: None,
            total_matching: 0,
            notes: Vec::new(),
            selected_index: 0,

            notebooks: Vec::new(),
            tags: Vec::new(),
            catalog_ready: false,
            catalog_requested: false,

            note_cache: HashMap::new(),
            prepared_docs: HashMap::new(),

            loading_list: true,
            loading_note_guid: None,
            prefetch_queue: VecDeque::new(),
            note_load_inflight: None,
            next_request_id: 1,
            latest_search_req: 0,

            save_indicator: SaveIndicator::Idle,
            anim_tick: 0,
            status_error: None,
            unsynced: HashMap::new(),
            quit_after_sync: false,
            deferred_replies: Vec::new(),
            pending_renames: HashMap::new(),
            spare_vim: None,
            spare_vim_enabled: false,
            spare_vim_failures: 0,

            preferences,
            preferences_path,
            preview_scroll: 0,
            pending_vim_guid: None,

            vim_session: None,
            geometry: LayoutGeometry::default(),

            cmd_tx,
            waker: Waker::noop(),
            last_tick: Instant::now(),
        };
        match initial_query {
            Some(query) => {
                app.search_input = TextInput::new(query);
                app.execute_search();
            }
            None => {
                let _ = app.cmd_tx.send(WorkerCommand::Bootstrap);
            }
        }
        app
    }

    /// Sends `LoadCatalog` once, behind the first list's `LoadNote` so the note body comes first.
    fn request_catalog_once(&mut self) {
        if !self.catalog_requested {
            self.catalog_requested = true;
            let _ = self.cmd_tx.send(WorkerCommand::LoadCatalog);
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
        if let Some(nb) = self.notebooks.iter().find(|nb| &nb.guid == g) {
            return nb.name.clone();
        }
        if self.catalog_ready {
            g.0.clone()
        } else {
            "...".to_owned()
        }
    }

    pub fn set_waker(&mut self, waker: Waker) {
        self.waker = waker;
    }

    /// When the UI loop must wake up even if no event arrives: the next animation frame,
    /// or when a "Saved" / error badge expires. `None` means nothing is time-based right now.
    #[must_use]
    pub fn next_deadline(&self) -> Option<Instant> {
        let animating = self.loading_list
            || self.loading_note_guid.is_some()
            || matches!(self.save_indicator, SaveIndicator::Saving { .. });
        let frame = animating.then(|| self.last_tick + ANIMATION_FRAME);
        let badge = match &self.save_indicator {
            SaveIndicator::Saved { until, .. } | SaveIndicator::Failed { until, .. } => {
                Some(*until)
            }
            _ => None,
        };
        let retry = self
            .unsynced
            .values()
            .filter_map(|note| note.retry_at)
            .min();
        frame.into_iter().chain(badge).chain(retry).min()
    }

    pub fn tick(&mut self) {
        self.anim_tick = self.anim_tick.wrapping_add(1);
        let now = Instant::now();
        self.last_tick = now;
        self.retry_due_saves(now);

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

    /// Keep one idle Vim ready from now on. Off by default so tests do not start Vim.
    pub fn enable_spare_vim(&mut self) {
        self.spare_vim_enabled = true;
        self.maintain_spare_vim();
    }

    /// Feeds the spare's output into its screen (so it is current once shown), and starts a
    /// new spare when there is none or the old one died.
    fn maintain_spare_vim(&mut self) {
        if !self.spare_vim_enabled {
            return;
        }
        if let Some(spare) = &mut self.spare_vim {
            spare.drain_output();
            if spare.has_exited() {
                self.spare_vim = None;
                self.spare_vim_failures = self.spare_vim_failures.saturating_add(1);
            }
        }
        if self.spare_vim.is_none() && self.spare_vim_failures < SPARE_VIM_MAX_FAILURES {
            let rows = self.geometry.vim_area.height.max(12);
            let cols = self.geometry.vim_area.width.max(40);
            match VimSession::spawn_spare(rows, cols, self.waker.clone()) {
                Ok(spare) => self.spare_vim = Some(spare),
                // Opening a note still starts Vim directly and reports its error there.
                Err(_) => self.spare_vim_failures = SPARE_VIM_MAX_FAILURES,
            }
        }
    }

    /// Hands out the spare if it is ready and takes the note; the next spare starts on the
    /// next loop pass.
    fn take_spare_vim(&mut self, guid: &NoteGuid, markdown: &str) -> Option<VimSession> {
        let mut spare = self.spare_vim.take()?;
        if !spare.is_ready() {
            self.spare_vim = Some(spare);
            return None;
        }
        // On failure the spare is dropped (killed) and replaced.
        spare.activate(guid.clone(), markdown).ok().map(|()| spare)
    }

    pub fn poll_vim_activity(&mut self) {
        self.maintain_spare_vim();
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

        // Drop the warning once `:w`, `u` or `:q!` resolved it.
        if self.status_error.as_deref() == Some(UNSAVED_VIM_MESSAGE)
            && !self
                .vim_session
                .as_ref()
                .is_some_and(VimSession::has_unsaved_changes)
        {
            self.status_error = None;
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

        self.replay_deferred_replies();
    }

    fn save_markdown_from_vim(&mut self, guid: &NoteGuid, edited_md: &str) {
        let Some(detail) = self.note_cache.get(guid).cloned() else {
            return;
        };
        let prepared =
            self.prepared_docs.get(guid).cloned().unwrap_or_else(|| {
                PreparedNoteDocument::from_enml(&detail.enml, &detail.resources)
            });
        // `:w` without edits would still turn the Markdown back into ENML and upload it,
        // which can change formatting the round trip does not keep. Vim adds a final newline.
        let unchanged = edited_md.strip_suffix('\n').unwrap_or(edited_md)
            == prepared
                .markdown
                .strip_suffix('\n')
                .unwrap_or(&prepared.markdown);
        if unchanged && !self.unsynced.contains_key(guid) {
            self.save_indicator = SaveIndicator::Saved {
                label: "No changes".to_owned(),
                until: Instant::now() + SAVED_BADGE_DURATION,
            };
            return;
        }
        let new_enml = prepared.to_enml(edited_md);

        // Update local prepared markdown snapshot immediately so subsequent re-opens reflect edits.
        let refreshed = PreparedNoteDocument::from_enml(&new_enml, &detail.resources);
        self.prepared_docs.insert(guid.clone(), refreshed);
        if let Some(cached) = self.note_cache.get_mut(guid) {
            cached.enml.clone_from(&new_enml);
        }

        self.unsynced.insert(
            guid.clone(),
            UnsyncedNote {
                title: detail.title.clone(),
                enml: new_enml.clone(),
                failed_attempts: 0,
                retry_at: None,
            },
        );
        self.begin_saving("Saving note");
        let _ = self.cmd_tx.send(WorkerCommand::SaveContent {
            guid: guid.clone(),
            title: detail.title,
            enml: new_enml,
        });
    }

    /// Sends again every failed save whose wait is over.
    fn retry_due_saves(&mut self, now: Instant) {
        let due: Vec<(NoteGuid, String, String)> = self
            .unsynced
            .iter_mut()
            .filter(|(_, note)| note.retry_at.is_some_and(|at| at <= now))
            .map(|(guid, note)| {
                note.retry_at = None;
                (guid.clone(), note.title.clone(), note.enml.clone())
            })
            .collect();
        for (guid, stored_title, enml) in due {
            // A rename since the failed save must not be undone by the retry.
            let title = self
                .note_cache
                .get(&guid)
                .map_or(stored_title, |detail| detail.title.clone());
            self.begin_saving("Retrying save");
            let _ = self
                .cmd_tx
                .send(WorkerCommand::SaveContent { guid, title, enml });
        }
    }

    /// Number of notes whose last `:w` has not reached Evernote yet.
    #[must_use]
    pub fn unsynced_count(&self) -> usize {
        self.unsynced.len()
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

    /// Vim is being worked in: it has focus, or edits not saved with `:w`.
    fn vim_in_use(&self) -> bool {
        self.vim_session
            .as_ref()
            .is_some_and(|vim| self.focus == FocusTarget::RightVim || vim.has_unsaved_changes())
    }

    /// Replies that select another note, which would close Vim and drop its edits.
    fn reply_replaces_selection(&self, evt: &WorkerEvent) -> bool {
        match evt {
            WorkerEvent::InitialNotes { .. } | WorkerEvent::NoteCreated { .. } => true,
            WorkerEvent::SearchCompleted { request_id, .. } => {
                *request_id == self.latest_search_req
            }
            _ => false,
        }
    }

    /// Applies held-back replies once Vim is closed, or left with everything saved.
    fn replay_deferred_replies(&mut self) {
        if self.deferred_replies.is_empty() {
            return;
        }
        if self.vim_in_use() {
            let unsaved = self
                .vim_session
                .as_ref()
                .is_some_and(VimSession::has_unsaved_changes);
            if !unsaved && self.status_error.is_none() {
                self.status_error = Some(DEFERRED_REPLY_MESSAGE.to_owned());
            }
            return;
        }
        if self.status_error.as_deref() == Some(DEFERRED_REPLY_MESSAGE) {
            self.status_error = None;
        }
        for evt in std::mem::take(&mut self.deferred_replies) {
            self.handle_worker_event(evt);
        }
    }

    fn local_title(&self, guid: &NoteGuid) -> Option<String> {
        self.note_cache
            .get(guid)
            .map(|detail| detail.title.clone())
            .or_else(|| {
                self.notes
                    .iter()
                    .find(|note| &note.guid == guid)
                    .map(|note| note.title.clone())
            })
    }

    /// Shows `title` everywhere the note appears, and makes later saves send it.
    fn set_local_title(&mut self, guid: &NoteGuid, title: &str) {
        if let Some(detail) = self.note_cache.get_mut(guid) {
            detail.title = title.to_owned();
        }
        if let Some(note) = self.notes.iter_mut().find(|note| &note.guid == guid) {
            note.title = title.to_owned();
        }
        self.sync_history_note(guid);
        self.remember_current_screen();
    }

    pub fn handle_worker_event(&mut self, evt: WorkerEvent) {
        if self.vim_in_use() && self.reply_replaces_selection(&evt) {
            self.loading_list = false;
            self.status_error = Some(
                if self
                    .vim_session
                    .as_ref()
                    .is_some_and(VimSession::has_unsaved_changes)
                {
                    UNSAVED_VIM_MESSAGE
                } else {
                    DEFERRED_REPLY_MESSAGE
                }
                .to_owned(),
            );
            self.deferred_replies.push(evt);
            return;
        }
        match evt {
            WorkerEvent::InitialNotes { page } => {
                self.loading_list = false;
                self.apply_search_page(page);
                // Queue the catalog behind the selected note's `LoadNote`, which
                // `apply_search_page` just sent.
                self.request_catalog_once();
            }
            WorkerEvent::CatalogLoaded { notebooks, tags } => {
                self.notebooks = notebooks;
                self.install_tags(tags);
                self.catalog_ready = true;
            }
            WorkerEvent::SearchCompleted { request_id, page } => {
                if request_id == self.latest_search_req {
                    self.loading_list = false;
                    self.apply_search_page(page);
                    // A startup query skips `Bootstrap`, so the catalog is fetched here.
                    self.request_catalog_once();
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
                let saved_enml = enml.clone();
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
                self.sync_history_note(&guid);
                self.remember_current_screen();
                self.finish_saving("Saved");
                // A newer `:w` of the same note may still be on its way.
                if self
                    .unsynced
                    .get(&guid)
                    .is_some_and(|note| note.enml == saved_enml)
                {
                    self.unsynced.remove(&guid);
                }
                if self.quit_after_sync && self.unsynced.is_empty() {
                    self.should_quit = true;
                }
            }
            WorkerEvent::SaveFailed {
                guid,
                enml,
                message,
                retry,
            } => {
                let now = Instant::now();
                let Some(note) = self
                    .unsynced
                    .get_mut(&guid)
                    .filter(|note| note.enml == enml)
                else {
                    // An older version failed; the newer one is already queued.
                    return;
                };
                note.failed_attempts += 1;
                let wait = match retry {
                    SaveRetry::No => None,
                    SaveRetry::Backoff => Some(
                        SAVE_RETRY_DELAYS
                            [(note.failed_attempts - 1).min(SAVE_RETRY_DELAYS.len() - 1)],
                    ),
                    SaveRetry::After(wait) => Some(wait),
                };
                note.retry_at = wait.map(|wait| now + wait);
                let message = match wait {
                    Some(wait) => format!("Not saved, retry in {}s: {message}", wait.as_secs()),
                    None => format!("Not saved (:w retries): {message}"),
                };
                self.save_indicator = SaveIndicator::Failed {
                    message,
                    until: now + ERROR_BADGE_DURATION,
                };
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
                self.sync_history_note(&guid);
                self.remember_current_screen();
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
                self.sync_history_note(&guid);
                self.remember_current_screen();
                self.finish_saving("Notebook updated");
            }
            WorkerEvent::RenameFailed {
                guid,
                new_title,
                message,
            } => {
                // Put the old title back, unless a newer rename has replaced this one.
                if self.local_title(&guid).as_deref() == Some(new_title.as_str())
                    && let Some(old) = self.pending_renames.remove(&guid)
                {
                    self.set_local_title(&guid, &old);
                }
                self.save_indicator = SaveIndicator::Failed {
                    message: format!("Rename failed: {message}"),
                    until: Instant::now() + ERROR_BADGE_DURATION,
                };
            }
            WorkerEvent::NoteRenamed {
                guid,
                new_title,
                updated,
            } => {
                if self.local_title(&guid).as_deref() == Some(new_title.as_str()) {
                    self.pending_renames.remove(&guid);
                } else if self.pending_renames.contains_key(&guid) {
                    // An older rename landed while a newer one is on its way: the newer title
                    // stays, and this one is now what to fall back to.
                    self.pending_renames.insert(guid.clone(), new_title.clone());
                } else {
                    self.set_local_title(&guid, &new_title);
                }
                if let Some(detail) = self.note_cache.get_mut(&guid)
                    && updated.is_some()
                {
                    detail.updated = updated;
                }
                if let Some(sum) = self.notes.iter_mut().find(|n| n.guid == guid)
                    && updated.is_some()
                {
                    sum.updated = updated;
                }
                self.sync_history_note(&guid);
                self.remember_current_screen();
                self.finish_saving("Renamed");
            }
            WorkerEvent::SourceUrlSaved {
                guid,
                source_url,
                updated,
            } => {
                if let Some(detail) = self.note_cache.get_mut(&guid) {
                    detail.source_url = source_url;
                    if updated.is_some() {
                        detail.updated = updated;
                    }
                }
                if let Some(sum) = self.notes.iter_mut().find(|n| n.guid == guid)
                    && updated.is_some()
                {
                    sum.updated = updated;
                }
                self.sync_history_note(&guid);
                self.remember_current_screen();
                self.finish_saving("URL saved");
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
                self.remember_current_screen();
                self.finish_saving("Created");
            }
            WorkerEvent::SearchFailed {
                request_id,
                message,
            } => {
                // An older search failing says nothing about the one on screen or on its way.
                if request_id == self.latest_search_req {
                    self.pending_search_query = None;
                    self.loading_list = false;
                    self.status_error = Some(format!("Search: {message}"));
                    // A startup query skips `Bootstrap`, so the catalog is fetched here too.
                    self.request_catalog_once();
                }
            }
            WorkerEvent::Error {
                context,
                message,
                was_save,
            } => {
                // Only reset state owned by the failed request: a failed tag save must not end
                // the loading state of a search or a note load running meanwhile.
                if context == CATALOG_CONTEXT {
                    self.catalog_ready = true;
                } else if context == INITIAL_SYNC_CONTEXT {
                    self.loading_list = false;
                }
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

    fn install_tags(&mut self, tags: Vec<Tag>) {
        for detail in self.note_cache.values_mut() {
            if detail.tag_guids.is_empty() || detail.tag_names.len() == detail.tag_guids.len() {
                continue;
            }
            let names: Vec<String> = detail
                .tag_guids
                .iter()
                .filter_map(|guid| {
                    tags.iter()
                        .find(|tag| &tag.guid == guid)
                        .map(|tag| tag.name.clone())
                })
                .collect();
            if names.len() == detail.tag_guids.len() {
                detail.tag_names = names;
            }
        }
        self.tags = tags;
    }

    fn apply_search_page(&mut self, mut page: SearchPage) {
        if let Some(query) = self.pending_search_query.take() {
            self.active_query = query.clone();
            self.search_input = TextInput::new(query);
        }
        if page.notes.len() > MAX_LEFT_TITLES {
            page.notes.truncate(MAX_LEFT_TITLES);
        }
        self.total_matching = page.total_matching;
        self.notes = page.notes;
        self.selected_index = 0;
        self.preview_scroll = 0;
        self.commit_search_screen();

        if self.notes.is_empty() {
            self.vim_session = None;
            self.loading_note_guid = None;
            self.prefetch_queue.clear();
        } else {
            self.on_selection_changed();
        }
    }

    fn capture_screen(&self) -> SearchScreen {
        SearchScreen {
            query: self.active_query.clone(),
            total_matching: self.total_matching,
            notes: self.notes.clone(),
            selected_index: self.selected_index,
            preview_scroll: self.preview_scroll,
        }
    }

    /// Writes the visible screen into the current history slot.
    fn remember_current_screen(&mut self) {
        if self
            .search_history
            .get(self.search_history_pos)
            .is_none_or(|slot| slot.query != self.active_query)
        {
            return;
        }
        let screen = self.capture_screen();
        self.search_history[self.search_history_pos] = screen;
    }

    /// Records a newly arrived search. Repeating the current query replaces that slot.
    fn commit_search_screen(&mut self) {
        let screen = self.capture_screen();
        if self
            .search_history
            .get(self.search_history_pos)
            .is_some_and(|existing| existing.query == screen.query)
        {
            self.search_history[self.search_history_pos] = screen;
            return;
        }
        self.search_history
            .truncate(self.search_history_pos.saturating_add(1));
        self.search_history.push(screen);
        self.search_history_pos = self.search_history.len() - 1;
    }

    fn sync_history_note(&mut self, guid: &NoteGuid) {
        let Some(updated) = self.notes.iter().find(|note| &note.guid == guid).cloned() else {
            return;
        };
        for screen in &mut self.search_history {
            if let Some(note) = screen.notes.iter_mut().find(|note| &note.guid == guid) {
                *note = updated.clone();
            }
        }
    }

    fn navigate_search_history(&mut self, delta: isize) {
        if self.search_history.is_empty() || self.blocked_by_unsaved_vim() {
            return;
        }
        self.remember_current_screen();
        let current = isize::try_from(self.search_history_pos).unwrap_or(0);
        let max = isize::try_from(self.search_history.len() - 1).unwrap_or(0);
        let next = current + delta;
        if !(0..=max).contains(&next) {
            return;
        }
        let Some(next) = usize::try_from(next).ok() else {
            return;
        };
        self.restore_search_screen(next);
    }

    fn restore_search_screen(&mut self, pos: usize) {
        let Some(screen) = self.search_history.get(pos).cloned() else {
            return;
        };
        self.search_history_pos = pos;
        self.pending_search_query = None;
        self.latest_search_req = self.alloc_req_id();
        self.loading_list = false;
        self.active_query.clone_from(&screen.query);
        self.search_input = TextInput::new(screen.query);
        self.total_matching = screen.total_matching;
        self.notes = screen.notes;
        self.selected_index = if self.notes.is_empty() {
            0
        } else {
            screen.selected_index.min(self.notes.len() - 1)
        };
        let scroll = screen.preview_scroll;
        self.prefetch_queue.clear();
        if self.notes.is_empty() {
            self.vim_session = None;
            self.loading_note_guid = None;
            self.preview_scroll = scroll;
        } else {
            self.on_selection_changed();
            self.preview_scroll = scroll;
        }
        self.remember_current_screen();
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
        self.remember_current_screen();
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
        if let Some(mut session) = self.take_spare_vim(guid, &md) {
            session.resize(rows, cols);
            self.vim_session = Some(session);
            return;
        }
        match VimSession::spawn(guid.clone(), &md, rows, cols, self.waker.clone()) {
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
        if area.width > 0 && area.height > 0 {
            // The spare too, so it needs no resize (and redraw) when it is shown.
            for vim in self.vim_session.iter_mut().chain(self.spare_vim.iter_mut()) {
                vim.resize(area.height, area.width);
            }
        }
    }

    fn alloc_req_id(&mut self) -> u64 {
        let id = self.next_request_id;
        self.next_request_id = self.next_request_id.wrapping_add(1);
        id
    }

    pub fn execute_search(&mut self) {
        if self.blocked_by_unsaved_vim() {
            return;
        }
        self.remember_current_screen();
        let query = self.search_input.text.trim().to_owned();
        self.pending_search_query = Some(query.clone());
        let parsed = ParsedSearch::parse(&query);
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
        // Saving replaces the note's whole tag set, so every current tag must be listed.
        // A note loaded before the tag list arrived may still lack its names.
        let names = if detail.tag_names.len() == detail.tag_guids.len() {
            Some(detail.tag_names)
        } else {
            detail
                .tag_guids
                .iter()
                .map(|guid| {
                    self.tags
                        .iter()
                        .find(|tag| &tag.guid == guid)
                        .map(|tag| tag.name.clone())
                })
                .collect::<Option<Vec<String>>>()
        };
        let Some(selected_tags) = names else {
            self.status_error = Some(TAGS_NOT_LOADED_MESSAGE.to_owned());
            // Reload unless a load is still on its way (a tag made elsewhere, or a failed load).
            if self.catalog_ready {
                self.catalog_ready = false;
                let _ = self.cmd_tx.send(WorkerCommand::LoadCatalog);
            }
            return;
        };
        if let Some(cached) = self.note_cache.get_mut(&detail.guid) {
            cached.tag_names.clone_from(&selected_tags);
        }
        self.modal = ModalState::TagEditor(TagEditorModal {
            note_guid: detail.guid,
            note_title: detail.title,
            filter: TextInput::default(),
            selected_tags,
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

    pub fn open_source_url_modal(&mut self) {
        let (note_guid, source_url) = {
            let Some(detail) = self.selected_detail() else {
                return;
            };
            (
                detail.guid.clone(),
                detail.source_url.clone().unwrap_or_default(),
            )
        };
        self.modal = ModalState::TextPrompt(TextPromptModal {
            kind: PromptKind::SetSourceUrl,
            note_guid: Some(note_guid),
            input: TextInput::new(source_url),
        });
    }

    fn submit_source_url(&mut self, pm: TextPromptModal) {
        let Some(guid) = pm.note_guid.clone() else {
            return;
        };
        let normalized = match normalize_source_url(&pm.input.text) {
            Ok(url) => url,
            Err(message) => {
                self.status_error = Some(message);
                self.modal = ModalState::TextPrompt(pm);
                return;
            }
        };
        let current = self
            .note_cache
            .get(&guid)
            .and_then(|detail| detail.source_url.clone());
        if current.as_deref() == normalized.as_deref() {
            return;
        }
        let Some(title) = self
            .note_cache
            .get(&guid)
            .map(|detail| detail.title.clone())
        else {
            return;
        };
        self.begin_saving("Saving URL");
        let _ = self.cmd_tx.send(WorkerCommand::SetSourceUrl {
            guid,
            title,
            source_url: normalized,
        });
    }

    /// Quits now, or once pending saves reach Evernote. A second `q` quits anyway.
    fn request_quit(&mut self) {
        if self.blocked_by_unsaved_vim() {
            return;
        }
        if self.unsynced.is_empty() || self.quit_after_sync {
            self.should_quit = true;
            return;
        }
        self.quit_after_sync = true;
        self.status_error = Some(QUIT_AFTER_SYNC_MESSAGE.to_owned());
    }

    pub fn open_create_note_modal(&mut self) {
        // The new note gets selected, which would close Vim.
        if self.blocked_by_unsaved_vim() {
            return;
        }
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
            } else if self.focus != FocusTarget::RightVim && !self.blocked_by_unsaved_vim() {
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
                self.request_quit();
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
            KeyCode::F(8) => {
                self.open_source_url_modal();
                return;
            }
            KeyCode::Char('u') if alt => {
                self.open_source_url_modal();
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
            KeyCode::Left if alt => {
                self.navigate_search_history(-1);
                return;
            }
            KeyCode::Right if alt => {
                self.navigate_search_history(1);
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
            KeyCode::Char('q') => self.request_quit(),
            KeyCode::Char('?') => self.modal = ModalState::Help,
            KeyCode::Char('p') if !ctrl => self.open_preferences_modal(),
            KeyCode::Char('b') => self.toggle_left_panel(),
            KeyCode::Char('/') => self.focus = FocusTarget::SearchInput,
            KeyCode::Char('t') => self.open_tag_modal(),
            KeyCode::Char('n') => self.open_notebook_modal(),
            KeyCode::Char('r') if !ctrl => self.open_rename_modal(),
            KeyCode::Char('u') if !ctrl => self.open_source_url_modal(),
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
            KeyCode::Char('q') => self.request_quit(),
            KeyCode::Char('?') => self.modal = ModalState::Help,
            KeyCode::Char('p') if !ctrl => self.open_preferences_modal(),
            KeyCode::Char('b') => self.toggle_left_panel(),
            KeyCode::Char('/') => self.focus = FocusTarget::SearchInput,
            KeyCode::Char('t') => self.open_tag_modal(),
            KeyCode::Char('n') => self.open_notebook_modal(),
            KeyCode::Char('r') if !ctrl => self.open_rename_modal(),
            KeyCode::Char('u') if !ctrl => self.open_source_url_modal(),
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
                KeyCode::Enter => match pm.kind {
                    PromptKind::SetSourceUrl => self.submit_source_url(pm),
                    PromptKind::RenameNote => {
                        let val = pm.input.text.trim().to_owned();
                        if !val.is_empty()
                            && let Some(guid) = pm.note_guid
                        {
                            if let Some(old) = self.local_title(&guid) {
                                self.pending_renames.entry(guid.clone()).or_insert(old);
                            }
                            self.set_local_title(&guid, &val);
                            self.begin_saving("Renaming note");
                            let _ = self.cmd_tx.send(WorkerCommand::RenameNote {
                                guid,
                                new_title: val,
                            });
                        }
                    }
                    PromptKind::CreateNote => {
                        let val = pm.input.text.trim().to_owned();
                        if !val.is_empty() {
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
                },
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
                    let clicked_idx =
                        self.geometry.left_titles_offset + usize::from(row - list_area.y);
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

                if rect_contains(self.geometry.url_hit, col, row) {
                    self.open_source_url_modal();
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
                // List rows inside TagEditor / NotebookPicker begin at inner.y + 2
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

    /// Leaving the note would kill Vim and lose edits not saved with `:w`. Refuses and says why.
    fn blocked_by_unsaved_vim(&mut self) -> bool {
        let unsaved = self
            .vim_session
            .as_ref()
            .is_some_and(VimSession::has_unsaved_changes);
        if unsaved {
            self.status_error = Some(UNSAVED_VIM_MESSAGE.to_owned());
        }
        unsaved
    }

    pub fn set_selection(&mut self, index: usize) {
        if index >= self.notes.len() || index == self.selected_index {
            return;
        }
        if self.blocked_by_unsaved_vim() {
            return;
        }
        self.selected_index = index;
        self.on_selection_changed();
        self.remember_current_screen();
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

        app.handle_worker_event(WorkerEvent::InitialNotes {
            page: SearchPage {
                total_matching: 30,
                notes: many_notes,
            },
        });
        app.handle_worker_event(WorkerEvent::CatalogLoaded {
            notebooks: Vec::new(),
            tags: Vec::new(),
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
        let mut app =
            App::with_preferences(tx, Preferences::default(), Some(prefs_file.clone()), None);
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

        app.handle_worker_event(WorkerEvent::InitialNotes {
            page: SearchPage {
                total_matching: 5,
                notes,
            },
        });
        app.handle_worker_event(WorkerEvent::CatalogLoaded {
            notebooks: Vec::new(),
            tags: Vec::new(),
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

        app.handle_worker_event(WorkerEvent::InitialNotes {
            page: SearchPage {
                total_matching: 3,
                notes,
            },
        });
        app.handle_worker_event(WorkerEvent::CatalogLoaded {
            notebooks: Vec::new(),
            tags: Vec::new(),
        });

        assert_eq!(take_load(&rx), "g-1");
        assert!(matches!(rx.try_recv(), Ok(WorkerCommand::LoadCatalog)));
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

    #[test]
    fn startup_query_skips_bootstrap_and_searches_first() {
        let (tx, rx) = mpsc::channel();
        let mut app =
            App::with_preferences(tx, Preferences::default(), None, Some("t git".to_owned()));
        let request_id = match rx.try_recv() {
            Ok(WorkerCommand::Search { request_id, filter }) => {
                assert_eq!(filter.words.as_deref(), Some("tag:git"));
                request_id
            }
            other => panic!("expected Search first, got {other:?}"),
        };
        assert!(rx.try_recv().is_err());
        assert!(app.loading_list);

        app.handle_worker_event(WorkerEvent::SearchCompleted {
            request_id,
            page: SearchPage {
                total_matching: 1,
                notes: vec![summary("g-1", "Git notes")],
            },
        });

        assert_eq!(app.active_query, "t git");
        assert_eq!(app.notes[0].title, "Git notes");
        assert_eq!(take_load(&rx), "g-1");
        assert!(matches!(rx.try_recv(), Ok(WorkerCommand::LoadCatalog)));

        // Later searches do not refetch the catalog.
        app.search_input = TextInput::new("other");
        app.execute_search();
        let request_id = match rx.try_recv() {
            Ok(WorkerCommand::Search { request_id, .. }) => request_id,
            other => panic!("expected Search, got {other:?}"),
        };
        app.handle_worker_event(WorkerEvent::SearchCompleted {
            request_id,
            page: SearchPage {
                total_matching: 0,
                notes: vec![],
            },
        });
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn shows_titles_and_loads_the_selected_note_before_the_catalog() {
        use crate::domain::{NotebookGuid, TagGuid};
        let (tx, rx) = mpsc::channel();
        let mut app = App::new(tx);
        assert!(matches!(rx.try_recv(), Ok(WorkerCommand::Bootstrap)));

        app.handle_worker_event(WorkerEvent::InitialNotes {
            page: SearchPage {
                total_matching: 1,
                notes: vec![NoteSummary {
                    guid: NoteGuid::from("g-1"),
                    title: "Fast".to_owned(),
                    created: None,
                    updated: None,
                    notebook_guid: Some(NotebookGuid::from("nb-1")),
                    tag_guids: vec![TagGuid::from("tg-1")],
                    content_length: None,
                }],
            },
        });

        assert!(!app.loading_list);
        assert_eq!(app.notes[0].title, "Fast");
        assert_eq!(app.notebook_name(Some(&NotebookGuid::from("nb-1"))), "...");
        assert_eq!(take_load(&rx), "g-1");
        assert!(matches!(rx.try_recv(), Ok(WorkerCommand::LoadCatalog)));

        app.handle_worker_event(WorkerEvent::NoteLoaded {
            request_id: 1,
            note: NoteDetail {
                guid: NoteGuid::from("g-1"),
                title: "Fast".to_owned(),
                enml: "<en-note><div>body</div></en-note>".to_owned(),
                created: None,
                updated: None,
                notebook_guid: Some(NotebookGuid::from("nb-1")),
                tag_guids: vec![TagGuid::from("tg-1")],
                tag_names: Vec::new(),
                source_url: None,
                content_length: Some(4),
                resources: Vec::new(),
            },
        });
        assert!(app.selected_detail().unwrap().tag_names.is_empty());

        app.handle_worker_event(WorkerEvent::CatalogLoaded {
            notebooks: vec![Notebook {
                guid: NotebookGuid::from("nb-1"),
                name: "INBOX".to_owned(),
                stack: None,
                is_default: true,
            }],
            tags: vec![Tag {
                guid: TagGuid::from("tg-1"),
                name: "work".to_owned(),
            }],
        });
        assert_eq!(
            app.notebook_name(Some(&NotebookGuid::from("nb-1"))),
            "INBOX"
        );
        assert_eq!(
            app.selected_detail().unwrap().tag_names,
            vec!["work".to_owned()]
        );
    }

    fn wait_for(app: &mut App, what: &str, done: impl Fn(&App) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && !done(app) {
            app.poll_vim_activity();
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(done(app), "timed out waiting for {what}");
    }

    fn key(code: KeyCode, modifiers: KeyModifiers) -> Event {
        Event::Key(KeyEvent::new(code, modifiers))
    }

    #[test]
    fn unsaved_vim_edits_block_leaving_the_note_until_saved() {
        let (tx, rx) = mpsc::channel();
        let mut app = App::new(tx);
        app.handle_worker_event(WorkerEvent::InitialNotes {
            page: SearchPage {
                total_matching: 2,
                notes: vec![summary("g-1", "One"), summary("g-2", "Two")],
            },
        });
        app.handle_worker_event(WorkerEvent::CatalogLoaded {
            notebooks: Vec::new(),
            tags: Vec::new(),
        });
        app.handle_worker_event(WorkerEvent::NoteLoaded {
            request_id: 1,
            note: detail("g-1"),
        });
        app.open_selected_in_vim();
        assert!(app.vim_session.is_some());

        // Edit in Vim, then leave it without `:w` (Ctrl+\ moves focus to the titles).
        app.vim_session
            .as_ref()
            .unwrap()
            .write_raw(b"GoNot saved yet\x1b");
        wait_for(&mut app, "the unsaved flag", |app| {
            app.vim_session
                .as_ref()
                .is_some_and(VimSession::has_unsaved_changes)
        });
        app.handle_terminal_event(key(KeyCode::Char('\\'), KeyModifiers::CONTROL));
        assert_eq!(app.focus, FocusTarget::LeftTitles);
        while rx.try_recv().is_ok() {}

        app.handle_terminal_event(key(KeyCode::Char('j'), KeyModifiers::NONE));
        assert_eq!(
            app.selected_index, 0,
            "j must not switch away from unsaved edits"
        );
        assert_eq!(app.status_error.as_deref(), Some(UNSAVED_VIM_MESSAGE));

        app.search_input = TextInput::new("other");
        app.execute_search();
        app.handle_terminal_event(alt_arrow(KeyCode::Left));
        app.handle_terminal_event(key(KeyCode::Char('q'), KeyModifiers::NONE));
        app.handle_terminal_event(key(KeyCode::Char('q'), KeyModifiers::CONTROL));
        app.open_create_note_modal();
        assert!(!app.should_quit);
        assert!(matches!(app.modal, ModalState::None));
        assert!(rx.try_recv().is_err(), "no search may be sent");
        assert!(
            app.vim_session.is_some(),
            "Vim with unsaved edits stays alive"
        );

        // `:w` saves and clears the warning; then switching works again.
        app.handle_terminal_event(key(KeyCode::Char('l'), KeyModifiers::ALT));
        app.vim_session.as_ref().unwrap().write_raw(b":w\r");
        wait_for(&mut app, "the save", |app| {
            app.vim_session
                .as_ref()
                .is_some_and(|vim| !vim.has_unsaved_changes())
                && app.status_error.is_none()
        });
        app.handle_terminal_event(key(KeyCode::Char('\\'), KeyModifiers::CONTROL));
        app.handle_terminal_event(key(KeyCode::Char('j'), KeyModifiers::NONE));
        assert_eq!(app.selected_index, 1);
    }

    #[test]
    fn replies_that_would_close_vim_wait_until_vim_is_left() {
        let (tx, rx) = mpsc::channel();
        let mut app = App::new(tx);
        app.handle_worker_event(WorkerEvent::InitialNotes {
            page: SearchPage {
                total_matching: 2,
                notes: vec![summary("g-1", "One"), summary("g-2", "Two")],
            },
        });
        app.handle_worker_event(WorkerEvent::NoteLoaded {
            request_id: 1,
            note: detail("g-1"),
        });

        // A search goes out, then the user starts editing before it answers.
        app.search_input = TextInput::new("later");
        app.execute_search();
        let request_id = app.latest_search_req;
        app.open_selected_in_vim();
        app.vim_session
            .as_ref()
            .unwrap()
            .write_raw(b"GoTyped while searching\x1b");
        wait_for(&mut app, "the unsaved flag", |app| {
            app.vim_session
                .as_ref()
                .is_some_and(VimSession::has_unsaved_changes)
        });

        app.handle_worker_event(WorkerEvent::SearchCompleted {
            request_id,
            page: SearchPage {
                total_matching: 1,
                notes: vec![summary("g-9", "Found")],
            },
        });
        assert!(app.vim_session.is_some(), "Vim with edits must stay open");
        assert_eq!(app.notes[0].guid.as_str(), "g-1", "list waits");
        assert_eq!(app.status_error.as_deref(), Some(UNSAVED_VIM_MESSAGE));
        assert!(!app.loading_list);

        // Saved but still editing: the results keep waiting.
        app.vim_session.as_ref().unwrap().write_raw(b":w\r");
        wait_for(&mut app, "the save", |app| {
            app.status_error.as_deref() == Some(DEFERRED_REPLY_MESSAGE)
        });
        assert_eq!(app.notes[0].guid.as_str(), "g-1");
        while rx.try_recv().is_ok() {}

        // Leaving Vim shows the results.
        app.handle_terminal_event(key(KeyCode::Char('\\'), KeyModifiers::CONTROL));
        app.poll_vim_activity();
        assert_eq!(app.notes[0].guid.as_str(), "g-9");
        assert_eq!(app.active_query, "later");
        assert_eq!(app.status_error, None);
    }

    fn rename(app: &mut App, rx: &mpsc::Receiver<WorkerCommand>, title: &str) {
        app.open_rename_modal();
        let ModalState::TextPrompt(prompt) = &mut app.modal else {
            panic!("rename prompt should open");
        };
        prompt.input = TextInput::new(title);
        app.handle_terminal_event(key(KeyCode::Enter, KeyModifiers::NONE));
        match rx.try_recv() {
            Ok(WorkerCommand::RenameNote { new_title, .. }) => assert_eq!(new_title, title),
            other => panic!("expected RenameNote, got {other:?}"),
        }
    }

    #[test]
    fn saves_after_a_rename_send_the_new_title_and_a_failed_rename_is_undone() {
        let (mut app, rx, guid, md) = app_with_loaded_note();
        let original = app.note_cache[&guid].title.clone();

        rename(&mut app, &rx, "Renamed");
        assert_eq!(
            app.notes[0].title, "Renamed",
            "shown before Evernote confirms"
        );
        app.save_markdown_from_vim(&guid, &format!("{md}\nmore\n"));
        match rx.try_recv() {
            Ok(WorkerCommand::SaveContent { title, .. }) => assert_eq!(title, "Renamed"),
            other => panic!("expected SaveContent, got {other:?}"),
        }

        app.handle_worker_event(WorkerEvent::RenameFailed {
            guid: guid.clone(),
            new_title: "Renamed".to_owned(),
            message: "boom".to_owned(),
        });
        assert_eq!(app.notes[0].title, original);
        assert_eq!(app.note_cache[&guid].title, original);
    }

    #[test]
    fn an_older_rename_landing_late_keeps_the_newer_title() {
        let (mut app, rx, guid, _md) = app_with_loaded_note();
        rename(&mut app, &rx, "Second");
        rename(&mut app, &rx, "Third");

        app.handle_worker_event(WorkerEvent::NoteRenamed {
            guid: guid.clone(),
            new_title: "Second".to_owned(),
            updated: None,
        });
        assert_eq!(app.notes[0].title, "Third");

        // If the newer one fails, the title falls back to the one Evernote has.
        app.handle_worker_event(WorkerEvent::RenameFailed {
            guid: guid.clone(),
            new_title: "Third".to_owned(),
            message: "boom".to_owned(),
        });
        assert_eq!(app.notes[0].title, "Second");
    }

    #[test]
    fn tag_editor_opens_only_when_every_tag_name_is_known() {
        use crate::domain::TagGuid;
        let (tx, rx) = mpsc::channel();
        let mut app = App::new(tx);
        app.handle_worker_event(WorkerEvent::InitialNotes {
            page: SearchPage {
                total_matching: 1,
                notes: vec![summary("g-1", "One")],
            },
        });
        // Loaded before the tag list: Evernote sent the tag GUIDs, but no names.
        let mut note = detail("g-1");
        note.tag_guids = vec![TagGuid::from("t-1"), TagGuid::from("t-2")];
        app.handle_worker_event(WorkerEvent::NoteLoaded {
            request_id: 1,
            note,
        });
        while rx.try_recv().is_ok() {}

        app.open_tag_modal();
        assert!(
            matches!(app.modal, ModalState::None),
            "must not open with no names"
        );
        assert_eq!(app.status_error.as_deref(), Some(TAGS_NOT_LOADED_MESSAGE));
        assert!(
            rx.try_recv().is_err(),
            "the first tag list load is still on its way"
        );

        // The tag list arrives but misses t-2 (made on another device): still refused,
        // and the list is loaded again.
        let tag = |guid: &str, name: &str| Tag {
            guid: TagGuid::from(guid),
            name: name.to_owned(),
        };
        app.handle_worker_event(WorkerEvent::CatalogLoaded {
            notebooks: Vec::new(),
            tags: vec![tag("t-1", "work")],
        });
        app.open_tag_modal();
        assert!(matches!(app.modal, ModalState::None));
        assert!(matches!(rx.try_recv(), Ok(WorkerCommand::LoadCatalog)));

        app.handle_worker_event(WorkerEvent::CatalogLoaded {
            notebooks: Vec::new(),
            tags: vec![tag("t-1", "work"), tag("t-2", "home")],
        });
        app.open_tag_modal();
        let ModalState::TagEditor(editor) = &app.modal else {
            panic!("tag editor should open once every name is known");
        };
        assert_eq!(editor.selected_tags, ["work", "home"]);
    }

    #[test]
    fn only_the_latest_search_failure_counts() {
        let (mut app, rx, _guid, _md) = app_with_loaded_note();
        app.search_input = TextInput::new("old");
        app.execute_search();
        let old_req = app.latest_search_req;
        app.search_input = TextInput::new("new");
        app.execute_search();
        let new_req = app.latest_search_req;
        while rx.try_recv().is_ok() {}

        // The old search fails after the new one was sent.
        app.handle_worker_event(WorkerEvent::SearchFailed {
            request_id: old_req,
            message: "boom".to_owned(),
        });
        assert!(app.loading_list, "the new search is still loading");
        assert_eq!(app.status_error, None);
        // A failed tag save meanwhile does not end it either.
        app.handle_worker_event(WorkerEvent::Error {
            context: "Save tags".to_owned(),
            message: "boom".to_owned(),
            was_save: true,
        });
        assert!(app.loading_list);

        app.handle_worker_event(WorkerEvent::SearchCompleted {
            request_id: new_req,
            page: SearchPage {
                total_matching: 1,
                notes: vec![summary("g-9", "Found")],
            },
        });
        assert_eq!(app.active_query, "new", "the new query is applied");
        assert_eq!(app.search_input.text, "new");

        // The latest search failing is reported and ends the loading state.
        app.search_input = TextInput::new("again");
        app.execute_search();
        app.handle_worker_event(WorkerEvent::SearchFailed {
            request_id: app.latest_search_req,
            message: "boom".to_owned(),
        });
        assert!(!app.loading_list);
        assert_eq!(app.status_error.as_deref(), Some("Search: boom"));
    }

    #[test]
    fn opening_a_note_uses_the_ready_spare_vim_and_starts_the_next() {
        let (mut app, _rx, guid, _md) = app_with_loaded_note();
        app.enable_spare_vim();
        wait_for(&mut app, "the spare Vim to be ready", |app| {
            app.spare_vim.as_ref().is_some_and(VimSession::is_ready)
        });
        let spare_pid = app.spare_vim.as_ref().unwrap().pid();

        app.open_selected_in_vim();
        let vim = app.vim_session.as_ref().expect("note open in Vim");
        assert_eq!(
            vim.pid(),
            spare_pid,
            "the spare shows the note: no new Vim started"
        );
        assert_eq!(vim.note_guid, guid);

        app.poll_vim_activity();
        let next = app.spare_vim.as_ref().expect("a new spare is started");
        assert_ne!(next.pid(), spare_pid);
    }

    fn app_with_loaded_note() -> (App, mpsc::Receiver<WorkerCommand>, NoteGuid, String) {
        let (tx, rx) = mpsc::channel();
        let mut app = App::new(tx);
        app.handle_worker_event(WorkerEvent::InitialNotes {
            page: SearchPage {
                total_matching: 1,
                notes: vec![summary("g-1", "One")],
            },
        });
        app.handle_worker_event(WorkerEvent::NoteLoaded {
            request_id: 1,
            note: detail("g-1"),
        });
        while rx.try_recv().is_ok() {}
        let guid = NoteGuid::from("g-1");
        let md = app.prepared_docs[&guid].markdown.clone();
        (app, rx, guid, md)
    }

    fn take_save(rx: &mpsc::Receiver<WorkerCommand>) -> String {
        match rx.try_recv() {
            Ok(WorkerCommand::SaveContent { enml, .. }) => enml,
            other => panic!("expected SaveContent, got {other:?}"),
        }
    }

    fn fail_save(app: &mut App, guid: &NoteGuid, enml: &str, retry: SaveRetry) {
        app.handle_worker_event(WorkerEvent::SaveFailed {
            guid: guid.clone(),
            enml: enml.to_owned(),
            message: "boom".to_owned(),
            retry,
        });
    }

    /// Pretends the retry wait is over, then lets the timer fire.
    fn fire_retry(app: &mut App, guid: &NoteGuid) {
        app.unsynced.get_mut(guid).unwrap().retry_at = Some(Instant::now());
        app.tick();
    }

    #[test]
    fn write_without_edits_is_not_uploaded() {
        let (mut app, rx, guid, md) = app_with_loaded_note();
        // Vim writes the buffer back, adding a final newline only when it is missing.
        assert!(md.ends_with('\n'));
        app.save_markdown_from_vim(&guid, &md);
        assert!(rx.try_recv().is_err(), "nothing may be sent to Evernote");
        app.save_markdown_from_vim(&guid, md.trim_end_matches('\n'));
        assert!(rx.try_recv().is_err(), "nothing may be sent to Evernote");
        assert!(matches!(
            &app.save_indicator,
            SaveIndicator::Saved { label, .. } if label == "No changes"
        ));
        assert_eq!(app.unsynced_count(), 0);
    }

    #[test]
    fn failed_save_retries_with_growing_delays_until_saved() {
        let (mut app, rx, guid, md) = app_with_loaded_note();
        app.save_markdown_from_vim(&guid, &format!("{md}\nmore\n"));
        let enml = take_save(&rx);
        assert_eq!(app.unsynced_count(), 1);

        let before = Instant::now();
        fail_save(&mut app, &guid, &enml, SaveRetry::Backoff);
        let retry_at = app.unsynced[&guid].retry_at.expect("a retry is scheduled");
        assert!(retry_at >= before + Duration::from_secs(2));
        assert!(app.next_deadline().is_some_and(|d| d <= retry_at));
        assert!(matches!(
            &app.save_indicator,
            SaveIndicator::Failed { message, .. } if message.starts_with("Not saved, retry in 2s")
        ));

        fire_retry(&mut app, &guid);
        assert_eq!(take_save(&rx), enml, "the same content is sent again");

        fail_save(&mut app, &guid, &enml, SaveRetry::Backoff);
        assert!(app.unsynced[&guid].retry_at.unwrap() >= Instant::now() + Duration::from_secs(4));
        fire_retry(&mut app, &guid);
        assert_eq!(take_save(&rx), enml);

        app.handle_worker_event(WorkerEvent::ContentSaved {
            guid: guid.clone(),
            enml,
            updated: None,
        });
        assert_eq!(app.unsynced_count(), 0);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn rate_limit_waits_as_told_and_refused_saves_wait_for_the_next_write() {
        let (mut app, rx, guid, md) = app_with_loaded_note();
        let edited = format!("{md}\nmore\n");
        app.save_markdown_from_vim(&guid, &edited);
        let enml = take_save(&rx);

        fail_save(
            &mut app,
            &guid,
            &enml,
            SaveRetry::After(Duration::from_secs(90)),
        );
        assert!(app.unsynced[&guid].retry_at.unwrap() >= Instant::now() + Duration::from_secs(89));

        // Evernote refused it: no automatic retry, but the next `:w` sends it again
        // even though the text did not change since.
        fail_save(&mut app, &guid, &enml, SaveRetry::No);
        assert_eq!(app.unsynced[&guid].retry_at, None);
        app.tick();
        assert!(rx.try_recv().is_err());
        app.save_markdown_from_vim(&guid, &edited);
        take_save(&rx);
    }

    #[test]
    fn failure_of_an_older_save_does_not_touch_the_newer_one() {
        let (mut app, rx, guid, md) = app_with_loaded_note();
        app.save_markdown_from_vim(&guid, &format!("{md}\nfirst\n"));
        let first = take_save(&rx);
        app.save_markdown_from_vim(&guid, &format!("{md}\nsecond\n"));
        let second = take_save(&rx);

        fail_save(&mut app, &guid, &first, SaveRetry::Backoff);
        assert_eq!(
            app.unsynced[&guid].retry_at, None,
            "the newer save is in flight"
        );
        app.handle_worker_event(WorkerEvent::ContentSaved {
            guid: guid.clone(),
            enml: second,
            updated: None,
        });
        assert_eq!(app.unsynced_count(), 0);
    }

    #[test]
    fn quit_waits_for_pending_saves_and_second_q_quits_anyway() {
        let (mut app, rx, guid, md) = app_with_loaded_note();
        app.save_markdown_from_vim(&guid, &format!("{md}\nmore\n"));
        let enml = take_save(&rx);

        app.handle_terminal_event(key(KeyCode::Char('q'), KeyModifiers::NONE));
        assert!(!app.should_quit);
        assert_eq!(app.status_error.as_deref(), Some(QUIT_AFTER_SYNC_MESSAGE));
        app.handle_worker_event(WorkerEvent::ContentSaved {
            guid: guid.clone(),
            enml: enml.clone(),
            updated: None,
        });
        assert!(app.should_quit, "quits once the save landed");

        let (mut app, rx, guid, md) = app_with_loaded_note();
        app.save_markdown_from_vim(&guid, &format!("{md}\nmore\n"));
        let enml = take_save(&rx);
        fail_save(&mut app, &guid, &enml, SaveRetry::Backoff);
        app.handle_terminal_event(key(KeyCode::Char('q'), KeyModifiers::NONE));
        assert!(!app.should_quit);
        app.handle_terminal_event(key(KeyCode::Char('q'), KeyModifiers::NONE));
        assert!(
            app.should_quit,
            "second q quits even though the save failed"
        );
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

    fn summary(guid: &str, title: &str) -> NoteSummary {
        NoteSummary {
            guid: NoteGuid::from(guid),
            title: title.to_owned(),
            created: None,
            updated: None,
            notebook_guid: None,
            tag_guids: Vec::new(),
            content_length: None,
        }
    }

    fn alt_arrow(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::ALT))
    }

    fn drain_loads(app: &mut App, rx: &mpsc::Receiver<WorkerCommand>) {
        while let Ok(cmd) = rx.try_recv() {
            match cmd {
                WorkerCommand::LoadNote { request_id, guid } => {
                    app.handle_worker_event(WorkerEvent::NoteLoaded {
                        request_id,
                        note: detail(guid.as_str()),
                    });
                }
                // Sent once, behind the first list's `LoadNote`.
                WorkerCommand::LoadCatalog => {}
                other => panic!("unexpected worker command: {other:?}"),
            }
        }
    }

    fn run_search(
        app: &mut App,
        rx: &mpsc::Receiver<WorkerCommand>,
        query: &str,
        notes: Vec<NoteSummary>,
    ) {
        app.search_input = TextInput::new(query);
        app.execute_search();
        let request_id = match rx.try_recv() {
            Ok(WorkerCommand::Search { request_id, .. }) => request_id,
            other => panic!("expected Search, got {other:?}"),
        };
        app.handle_worker_event(WorkerEvent::SearchCompleted {
            request_id,
            page: SearchPage {
                total_matching: notes.len(),
                notes,
            },
        });
        drain_loads(app, rx);
    }

    #[test]
    fn search_history_moves_back_and_forward_without_network() {
        let (tx, rx) = mpsc::channel();
        let mut app = App::new(tx);
        assert!(matches!(rx.try_recv(), Ok(WorkerCommand::Bootstrap)));
        app.handle_worker_event(WorkerEvent::InitialNotes {
            page: SearchPage {
                total_matching: 1,
                notes: vec![summary("home", "Home")],
            },
        });
        app.handle_worker_event(WorkerEvent::CatalogLoaded {
            notebooks: Vec::new(),
            tags: Vec::new(),
        });
        drain_loads(&mut app, &rx);

        run_search(
            &mut app,
            &rx,
            "aaa",
            vec![summary("a1", "Alpha"), summary("a2", "Alpine")],
        );
        app.set_selection(1);
        drain_loads(&mut app, &rx);
        run_search(&mut app, &rx, "bbb", vec![summary("b1", "Beta")]);
        run_search(&mut app, &rx, "ccc", vec![summary("c1", "Gamma")]);
        assert!(rx.try_recv().is_err());

        app.handle_terminal_event(alt_arrow(KeyCode::Left));
        app.handle_terminal_event(alt_arrow(KeyCode::Left));
        assert_eq!(app.active_query, "aaa");
        assert_eq!(app.notes.len(), 2);
        assert_eq!(app.notes[1].title, "Alpine");
        assert_eq!(app.selected_index, 1);
        assert!(app.note_cache.contains_key(&NoteGuid::from("a2")));
        assert!(rx.try_recv().is_err());

        app.handle_terminal_event(alt_arrow(KeyCode::Right));
        app.handle_terminal_event(alt_arrow(KeyCode::Right));
        assert_eq!(app.active_query, "ccc");
        assert_eq!(app.notes[0].title, "Gamma");
        assert!(rx.try_recv().is_err());

        app.search_input = TextInput::new("ddd");
        app.execute_search();
        let stale = match rx.try_recv() {
            Ok(WorkerCommand::Search { request_id, .. }) => request_id,
            other => panic!("expected Search, got {other:?}"),
        };
        app.handle_terminal_event(alt_arrow(KeyCode::Left));
        assert_eq!(app.active_query, "bbb");
        app.handle_worker_event(WorkerEvent::SearchCompleted {
            request_id: stale,
            page: SearchPage {
                total_matching: 1,
                notes: vec![summary("d1", "Delta")],
            },
        });
        assert_eq!(app.active_query, "bbb");
        assert_eq!(app.notes[0].title, "Beta");
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn sets_and_clears_source_url_from_prompt() {
        use crate::domain::TimestampMs;

        let (tx, rx) = mpsc::channel();
        let mut app = App::new(tx);
        assert!(matches!(rx.try_recv(), Ok(WorkerCommand::Bootstrap)));

        let mut note = detail("g1");
        note.title = "Track".to_owned();
        note.source_url = Some("https://old.example/a".to_owned());
        app.notes = vec![summary("g1", "Track")];
        app.note_cache.insert(note.guid.clone(), note);
        app.loading_list = false;

        let press = |app: &mut App, code: KeyCode| {
            app.handle_terminal_event(Event::Key(KeyEvent::new(code, KeyModifiers::empty())));
        };

        press(&mut app, KeyCode::Char('u'));
        let ModalState::TextPrompt(pm) = &app.modal else {
            panic!("expected source URL prompt, got {:?}", app.modal);
        };
        assert_eq!(pm.kind, PromptKind::SetSourceUrl);
        assert_eq!(pm.input.text, "https://old.example/a");

        press(&mut app, KeyCode::Enter);
        assert!(matches!(app.modal, ModalState::None));
        assert!(rx.try_recv().is_err());

        press(&mut app, KeyCode::Char('u'));
        let ModalState::TextPrompt(pm) = &mut app.modal else {
            panic!("expected source URL prompt");
        };
        pm.input = TextInput::new("  https://music.yandex.com/album/1  ");
        press(&mut app, KeyCode::Enter);
        match rx.try_recv() {
            Ok(WorkerCommand::SetSourceUrl {
                guid,
                title,
                source_url,
            }) => {
                assert_eq!(guid.0, "g1");
                assert_eq!(title, "Track");
                assert_eq!(
                    source_url.as_deref(),
                    Some("https://music.yandex.com/album/1")
                );
            }
            other => panic!("expected SetSourceUrl, got {other:?}"),
        }

        app.handle_worker_event(WorkerEvent::SourceUrlSaved {
            guid: NoteGuid::from("g1"),
            source_url: Some("https://music.yandex.com/album/1".to_owned()),
            updated: Some(TimestampMs(1_762_100_000_000)),
        });
        assert_eq!(
            app.selected_detail()
                .and_then(|d| d.source_url.clone())
                .as_deref(),
            Some("https://music.yandex.com/album/1")
        );
        assert_eq!(
            app.selected_summary().and_then(|summary| summary.updated),
            Some(TimestampMs(1_762_100_000_000))
        );

        app.geometry.url_hit = Rect {
            x: 2,
            y: 4,
            width: 20,
            height: 2,
        };
        app.handle_terminal_event(Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 3,
            row: 5,
            modifiers: KeyModifiers::empty(),
        }));
        let ModalState::TextPrompt(pm) = &mut app.modal else {
            panic!("click should open source URL prompt");
        };
        pm.input.clear();
        press(&mut app, KeyCode::Enter);
        match rx.try_recv() {
            Ok(WorkerCommand::SetSourceUrl { source_url, .. }) => assert_eq!(source_url, None),
            other => panic!("expected clear, got {other:?}"),
        };

        press(&mut app, KeyCode::F(8));
        let ModalState::TextPrompt(pm) = &mut app.modal else {
            panic!("F8 should open source URL prompt");
        };
        pm.input = TextInput::new("http://bad\nurl");
        press(&mut app, KeyCode::Enter);
        assert!(matches!(app.modal, ModalState::TextPrompt(_)));
        assert!(
            app.status_error
                .as_deref()
                .is_some_and(|err| err.contains("control"))
        );
        assert!(rx.try_recv().is_err());
    }
}
