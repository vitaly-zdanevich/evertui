use chrono::{Local, TimeZone};
use std::fmt;

/// Maximum number of note titles displayed in the left panel.
pub const MAX_LEFT_TITLES: usize = 20;

/// Strongly-typed Evernote Note GUID.
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NoteGuid(pub String);

impl NoteGuid {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for NoteGuid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for NoteGuid {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for NoteGuid {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

/// Strongly-typed Evernote Notebook GUID.
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NotebookGuid(pub String);

impl NotebookGuid {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for NotebookGuid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for NotebookGuid {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for NotebookGuid {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

/// Strongly-typed Evernote Tag GUID.
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TagGuid(pub String);

impl TagGuid {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TagGuid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for TagGuid {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for TagGuid {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

/// Milliseconds since Unix epoch as returned by Evernote EDAM.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TimestampMs(pub i64);

impl TimestampMs {
    #[must_use]
    pub fn format_datetime(self) -> String {
        match Local.timestamp_millis_opt(self.0).single() {
            Some(dt) => dt.format("%Y-%m-%d %H:%M").to_string(),
            None => self.0.to_string(),
        }
    }

    #[must_use]
    pub fn format_date(self) -> String {
        match Local.timestamp_millis_opt(self.0).single() {
            Some(dt) => dt.format("%Y-%m-%d").to_string(),
            None => self.0.to_string(),
        }
    }
}

/// Clamped left-panel width in columns (or dynamically bounded to terminal width).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LeftPanelWidthCols(u16);

impl LeftPanelWidthCols {
    pub const MIN: u16 = 18;
    pub const DEFAULT: u16 = 34;

    #[must_use]
    pub fn new(cols: u16, total_width: u16) -> Self {
        let max_left = total_width.saturating_sub(24).max(Self::MIN);
        Self(cols.clamp(Self::MIN.min(max_left), max_left))
    }

    #[must_use]
    pub fn get(self) -> u16 {
        self.0
    }
}

impl Default for LeftPanelWidthCols {
    fn default() -> Self {
        Self(Self::DEFAULT)
    }
}

/// Lightweight note metadata for the left-panel list (up to 20 items).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NoteSummary {
    pub guid: NoteGuid,
    pub title: String,
    pub created: Option<TimestampMs>,
    pub updated: Option<TimestampMs>,
    pub notebook_guid: Option<NotebookGuid>,
    pub tag_guids: Vec<TagGuid>,
    pub content_length: Option<usize>,
}

/// Resource metadata (without binary payload) used to render file/image placeholders.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ResourceMeta {
    pub guid: String,
    pub mime: Option<String>,
    pub filename: String,
    pub body_hash_hex: String,
    pub size: usize,
}

impl ResourceMeta {
    #[must_use]
    pub fn is_image(&self) -> bool {
        self.mime
            .as_deref()
            .is_some_and(|m| m.to_ascii_lowercase().starts_with("image/"))
    }
}

/// Full note detail including ENML content, metadata, tags, and attachment summaries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NoteDetail {
    pub guid: NoteGuid,
    pub title: String,
    pub enml: String,
    pub created: Option<TimestampMs>,
    pub updated: Option<TimestampMs>,
    pub notebook_guid: Option<NotebookGuid>,
    pub tag_guids: Vec<TagGuid>,
    pub tag_names: Vec<String>,
    pub source_url: Option<String>,
    pub content_length: Option<usize>,
    pub resources: Vec<ResourceMeta>,
}

impl NoteDetail {
    #[must_use]
    pub fn image_count(&self) -> usize {
        self.resources.iter().filter(|r| r.is_image()).count()
    }

    #[must_use]
    pub fn attachment_count(&self) -> usize {
        self.resources.iter().filter(|r| !r.is_image()).count()
    }
}

/// Evernote Notebook metadata.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notebook {
    pub guid: NotebookGuid,
    pub name: String,
    pub stack: Option<String>,
    pub is_default: bool,
}

/// Evernote Tag metadata.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tag {
    pub guid: TagGuid,
    pub name: String,
}

/// Result of a bounded note search.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchPage {
    pub total_matching: usize,
    pub notes: Vec<NoteSummary>,
}
