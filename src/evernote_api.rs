use crate::auth::{AuthConfig, DEVELOPER_TOKEN_URL, default_config_dir};
use crate::domain::{
    MAX_LEFT_TITLES, NoteDetail, NoteGuid, NoteSummary, Notebook, NotebookGuid, ResourceMeta,
    SearchPage, Tag, TagGuid, TimestampMs,
};
use crate::enml;
use crate::search::CompiledEvernoteFilter;
use evernote::errors::{
    EDAMErrorCode, EDAMNotFoundException, EDAMSystemException, EDAMUserException,
};
use evernote::limits::EDAM_ATTRIBUTE_LEN_MAX;
use evernote::note_store::{
    NoteFilter, NoteStoreSyncClient, NotesMetadataResultSpec, TNoteStoreSyncClient,
};
use evernote::types as edam_types;
use evernote::user_store::{TUserStoreSyncClient, UserStoreSyncClient};
use std::collections::{HashMap, VecDeque};
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use thiserror::Error;
use thrift::protocol::{TBinaryInputProtocol, TBinaryOutputProtocol};
use thrift::transport::{TBufferedReadTransport, TBufferedWriteTransport};

#[derive(Debug, Error)]
pub enum ApiError {
    /// Evernote refused the request. Sending it again unchanged will fail the same way.
    #[error("{0}")]
    Remote(String),
    /// Network, timeout or protocol failure: the same request may succeed later.
    #[error("{0}")]
    Network(String),
    /// `RATE_LIMIT_REACHED`: Evernote says how long to wait before the next call.
    #[error("Evernote RATE_LIMIT_REACHED: retry in {}s", retry_after.as_secs())]
    RateLimited { retry_after: Duration },
}

/// Used when Evernote reports a rate limit without saying how long it lasts.
const DEFAULT_RATE_LIMIT_WAIT: Duration = Duration::from_secs(60);

impl ApiError {
    /// Whether sending the same request again later can succeed.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        !matches!(self, Self::Remote(_))
    }

    /// The wait Evernote asked for, when this is a rate limit.
    #[must_use]
    pub fn rate_limit_wait(&self) -> Option<Duration> {
        match self {
            Self::RateLimited { retry_after } => Some(*retry_after),
            _ => None,
        }
    }
}

pub struct UpdatedTagsOutcome {
    pub tag_guids: Vec<TagGuid>,
    pub tag_names: Vec<String>,
    pub updated: Option<TimestampMs>,
    pub all_tags: Option<Vec<Tag>>,
}

pub trait EvernoteService: Send {
    /// Latest notes only. Notebooks and tags are a separate call so the title list
    /// is not stuck behind `listNotebooks` and `listTags`.
    fn initial_notes(&mut self) -> Result<SearchPage, ApiError>;
    fn load_catalog(&mut self) -> Result<(Vec<Notebook>, Vec<Tag>), ApiError>;
    fn search_notes(&mut self, filter: &CompiledEvernoteFilter) -> Result<SearchPage, ApiError>;
    fn load_note(&mut self, guid: &NoteGuid) -> Result<NoteDetail, ApiError>;
    fn save_note_content(
        &mut self,
        guid: &NoteGuid,
        title: &str,
        enml_content: &str,
    ) -> Result<Option<TimestampMs>, ApiError>;
    fn set_note_tags(
        &mut self,
        guid: &NoteGuid,
        title: &str,
        tag_names: &[String],
    ) -> Result<UpdatedTagsOutcome, ApiError>;
    fn set_note_notebook(
        &mut self,
        guid: &NoteGuid,
        title: &str,
        notebook_guid: &NotebookGuid,
    ) -> Result<Option<TimestampMs>, ApiError>;
    fn rename_note(
        &mut self,
        guid: &NoteGuid,
        new_title: &str,
    ) -> Result<Option<TimestampMs>, ApiError>;
    fn set_note_source_url(
        &mut self,
        guid: &NoteGuid,
        title: &str,
        source_url: Option<&str>,
    ) -> Result<(Option<String>, Option<TimestampMs>), ApiError>;
    fn create_note(
        &mut self,
        title: &str,
        notebook_guid: Option<&NotebookGuid>,
    ) -> Result<NoteDetail, ApiError>;
}

pub struct EdamEvernoteService {
    auth: AuthConfig,
    agent: ureq::Agent,
    /// `~/.config/evertui/note_store_url`, or a test path.
    cache_path: PathBuf,
    note_store_url: Option<String>,
    cached_tags_by_guid: HashMap<TagGuid, String>,
}

type UserStore = UserStoreSyncClient<
    TBinaryInputProtocol<TBufferedReadTransport<HttpReadHalf>>,
    TBinaryOutputProtocol<TBufferedWriteTransport<HttpWriteHalf>>,
>;

type NoteStore = NoteStoreSyncClient<
    TBinaryInputProtocol<TBufferedReadTransport<HttpReadHalf>>,
    TBinaryOutputProtocol<TBufferedWriteTransport<HttpWriteHalf>>,
>;

impl EdamEvernoteService {
    #[must_use]
    pub fn new(auth: AuthConfig) -> Self {
        let agent = ureq::Agent::config_builder()
            .user_agent(concat!("evertui/", env!("CARGO_PKG_VERSION")))
            .timeout_global(Some(Duration::from_secs(30)))
            .build()
            .into();
        Self {
            auth,
            agent,
            cache_path: default_config_dir().join("note_store_url"),
            note_store_url: None,
            cached_tags_by_guid: HashMap::new(),
        }
    }

    fn user_store(&self) -> UserStore {
        let (r, w) = http_halves(&self.auth.user_store_url, self.agent.clone());
        UserStoreSyncClient::new(
            TBinaryInputProtocol::new(TBufferedReadTransport::new(r), true),
            TBinaryOutputProtocol::new(TBufferedWriteTransport::new(w), true),
        )
    }

    /// One NoteStore call. A transport failure drops the cached shard URL, asks
    /// `getUserUrls` for the current one, and retries the call once.
    fn call_note_store<T>(
        &mut self,
        op: impl Fn(&mut NoteStore) -> thrift::Result<T>,
    ) -> Result<T, ApiError> {
        let mut client = self.note_store(true)?;
        match op(&mut client) {
            Ok(value) => Ok(value),
            Err(err) if should_refresh_note_store(&err) => {
                let mut client = self.note_store(false)?;
                op(&mut client).map_err(map_thrift_error)
            }
            Err(err) => Err(map_thrift_error(err)),
        }
    }

    fn note_store(&mut self, allow_disk: bool) -> Result<NoteStore, ApiError> {
        if !allow_disk {
            self.note_store_url = None;
        }
        let url = if let Some(existing) = self.note_store_url.clone() {
            existing
        } else if allow_disk
            && let Some(cached) = read_note_store_cache(&self.cache_path, &self.auth)
        {
            self.note_store_url = Some(cached.clone());
            cached
        } else {
            self.resolve_note_store_url()?
        };

        let (r, w) = http_halves(&url, self.agent.clone());
        Ok(NoteStoreSyncClient::new(
            TBinaryInputProtocol::new(TBufferedReadTransport::new(r), true),
            TBinaryOutputProtocol::new(TBufferedWriteTransport::new(w), true),
        ))
    }

    fn resolve_note_store_url(&mut self) -> Result<String, ApiError> {
        let mut us = self.user_store();
        let urls = us
            .get_user_urls(self.auth.token().to_owned())
            .map_err(map_thrift_error)?;
        let resolved = urls.note_store_url.ok_or_else(|| {
            ApiError::Remote("Evernote did not return a NoteStore URL".to_owned())
        })?;
        self.note_store_url = Some(resolved.clone());
        write_note_store_cache(&self.cache_path, &self.auth, &resolved);
        Ok(resolved)
    }

    fn fetch_tags_internal(&mut self) -> Result<Vec<Tag>, ApiError> {
        let token = self.auth.token().to_owned();
        let raw_tags = self.call_note_store(|ns| ns.list_tags(token.clone()))?;
        let mut tags: Vec<Tag> = raw_tags
            .into_iter()
            .map(|t| Tag {
                guid: TagGuid(t.guid.unwrap_or_default()),
                name: t.name.unwrap_or_default(),
            })
            .collect();
        tags.sort_by_key(|a| a.name.to_lowercase());
        self.cached_tags_by_guid = tags
            .iter()
            .map(|t| (t.guid.clone(), t.name.clone()))
            .collect();
        Ok(tags)
    }

    /// All names in `tag_guids` order, or an empty list if any of them is unknown: a partial
    /// list would let the tag editor drop the missing tags when it saves.
    fn resolve_tag_names(&mut self, note_guid: &NoteGuid, tag_guids: &[TagGuid]) -> Vec<String> {
        if tag_guids.is_empty() {
            return Vec::new();
        }
        let mut names = Vec::with_capacity(tag_guids.len());
        let mut missing = false;
        for tg in tag_guids {
            if let Some(name) = self.cached_tags_by_guid.get(tg) {
                names.push(name.clone());
            } else {
                missing = true;
                break;
            }
        }
        if !missing {
            return names;
        }
        if self.fetch_tags_internal().is_ok() {
            let resolved: Vec<String> = tag_guids
                .iter()
                .filter_map(|tg| self.cached_tags_by_guid.get(tg).cloned())
                .collect();
            if resolved.len() == tag_guids.len() {
                return resolved;
            }
        }
        let token = self.auth.token().to_owned();
        let guid = note_guid.as_str().to_owned();
        match self.call_note_store(|ns| ns.get_note_tag_names(token.clone(), guid.clone())) {
            Ok(fetched) if fetched.len() == tag_guids.len() => fetched,
            _ => Vec::new(),
        }
    }
}

impl EdamEvernoteService {
    fn list_notebooks_internal(&mut self) -> Result<Vec<Notebook>, ApiError> {
        let token = self.auth.token().to_owned();
        let raw_notebooks = self.call_note_store(|ns| ns.list_notebooks(token.clone()))?;
        let mut notebooks: Vec<Notebook> = raw_notebooks
            .into_iter()
            .map(|nb| Notebook {
                guid: NotebookGuid(nb.guid.unwrap_or_default()),
                name: nb.name.unwrap_or_default(),
                stack: nb.stack,
                is_default: nb.default_notebook.unwrap_or(false),
            })
            .collect();
        notebooks.sort_by_key(|a| a.name.to_lowercase());
        Ok(notebooks)
    }
}

impl EvernoteService for EdamEvernoteService {
    fn initial_notes(&mut self) -> Result<SearchPage, ApiError> {
        self.search_notes(&CompiledEvernoteFilter {
            notebook_guid: None,
            words: None,
        })
    }

    fn load_catalog(&mut self) -> Result<(Vec<Notebook>, Vec<Tag>), ApiError> {
        let notebooks = self.list_notebooks_internal()?;
        let tags = self.fetch_tags_internal()?;
        Ok((notebooks, tags))
    }

    fn search_notes(&mut self, filter: &CompiledEvernoteFilter) -> Result<SearchPage, ApiError> {
        let token = self.auth.token().to_owned();
        let edam_filter = NoteFilter::new(
            Some(edam_types::NoteSortOrder::UPDATED.0),
            None,
            filter.words.clone(),
            filter.notebook_guid.as_ref().map(|g| g.0.clone()),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        );
        let spec = NotesMetadataResultSpec::new(
            Some(true), // include_title
            Some(true), // include_content_length
            Some(true), // include_created
            Some(true), // include_updated
            None,
            None,
            Some(true), // include_notebook_guid
            Some(true), // include_tag_guids
            None,
            None,
            None,
        );
        let max_notes = i32::try_from(MAX_LEFT_TITLES).unwrap_or(20);
        let result = self.call_note_store(|ns| {
            ns.find_notes_metadata(
                token.clone(),
                edam_filter.clone(),
                0,
                max_notes,
                spec.clone(),
            )
        })?;

        let notes = result
            .notes
            .into_iter()
            .take(MAX_LEFT_TITLES)
            .map(|m| NoteSummary {
                guid: NoteGuid(m.guid),
                title: m.title.unwrap_or_else(|| "(Untitled)".to_owned()),
                created: m.created.map(TimestampMs),
                updated: m.updated.map(TimestampMs),
                notebook_guid: m.notebook_guid.map(NotebookGuid),
                tag_guids: m
                    .tag_guids
                    .unwrap_or_default()
                    .into_iter()
                    .map(TagGuid)
                    .collect(),
                content_length: m.content_length.and_then(|n| usize::try_from(n).ok()),
            })
            .collect();

        Ok(SearchPage {
            total_matching: usize::try_from(result.total_notes).unwrap_or(0),
            notes,
        })
    }

    fn load_note(&mut self, guid: &NoteGuid) -> Result<NoteDetail, ApiError> {
        let token = self.auth.token().to_owned();
        let guid_str = guid.as_str().to_owned();
        // Request note content and resource metadata WITHOUT downloading heavy binary blobs.
        let raw = self.call_note_store(|ns| {
            ns.get_note(token.clone(), guid_str.clone(), true, false, false, false)
        })?;

        let note_guid = NoteGuid(raw.guid.unwrap_or_else(|| guid.0.clone()));
        let tag_guids: Vec<TagGuid> = raw
            .tag_guids
            .unwrap_or_default()
            .into_iter()
            .map(TagGuid)
            .collect();
        let mut tag_names = raw.tag_names.unwrap_or_default();
        // A cold tag cache means startup has not fetched tags yet. Resolving them
        // here would download the whole tag list before the note body can render.
        if tag_names.is_empty() && !tag_guids.is_empty() && !self.cached_tags_by_guid.is_empty() {
            tag_names = self.resolve_tag_names(&note_guid, &tag_guids);
        }

        let resources = raw
            .resources
            .unwrap_or_default()
            .into_iter()
            .map(map_resource_meta)
            .collect();

        Ok(NoteDetail {
            guid: note_guid,
            title: raw.title.unwrap_or_else(|| "(Untitled)".to_owned()),
            enml: raw.content.unwrap_or_default(),
            created: raw.created.map(TimestampMs),
            updated: raw.updated.map(TimestampMs),
            notebook_guid: raw.notebook_guid.map(NotebookGuid),
            tag_guids,
            tag_names,
            source_url: raw.attributes.and_then(|a| a.source_u_r_l),
            content_length: raw.content_length.and_then(|n| usize::try_from(n).ok()),
            resources,
        })
    }

    fn save_note_content(
        &mut self,
        guid: &NoteGuid,
        title: &str,
        enml_content: &str,
    ) -> Result<Option<TimestampMs>, ApiError> {
        let token = self.auth.token().to_owned();
        // Leave `resources: None`, `tag_guids: None`, `notebook_guid: None` so Evernote
        // preserves existing binary attachments, tags, and notebook unmodified.
        let patch = edam_types::Note {
            guid: Some(guid.as_str().to_owned()),
            title: Some(sanitize_title(title)),
            content: Some(enml_content.to_owned()),
            ..Default::default()
        };

        let saved = self.call_note_store(|ns| ns.update_note(token.clone(), patch.clone()))?;
        Ok(saved.updated.map(TimestampMs))
    }

    fn set_note_tags(
        &mut self,
        guid: &NoteGuid,
        title: &str,
        tag_names: &[String],
    ) -> Result<UpdatedTagsOutcome, ApiError> {
        let cleaned: Vec<String> = tag_names
            .iter()
            .map(|t| t.trim().to_owned())
            .filter(|t| !t.is_empty())
            .collect();

        let had_unknown_tag = cleaned.iter().any(|name| {
            !self
                .cached_tags_by_guid
                .values()
                .any(|existing| existing.eq_ignore_ascii_case(name))
        });

        let token = self.auth.token().to_owned();
        let (tag_guids, patch_tag_names) = if cleaned.is_empty() {
            // Explicitly clear all tags on the note.
            (Some(Vec::new()), None)
        } else {
            (None, Some(cleaned.clone()))
        };
        let patch = edam_types::Note {
            guid: Some(guid.as_str().to_owned()),
            title: Some(sanitize_title(title)),
            tag_guids,
            tag_names: patch_tag_names,
            ..Default::default()
        };

        let saved = self.call_note_store(|ns| ns.update_note(token.clone(), patch.clone()))?;
        let updated_guids: Vec<TagGuid> = saved
            .tag_guids
            .unwrap_or_default()
            .into_iter()
            .map(TagGuid)
            .collect();

        let all_tags = if had_unknown_tag {
            self.fetch_tags_internal().ok()
        } else {
            None
        };
        let resolved_names = if cleaned.is_empty() {
            Vec::new()
        } else {
            let names = self.resolve_tag_names(guid, &updated_guids);
            if names.is_empty() { cleaned } else { names }
        };

        Ok(UpdatedTagsOutcome {
            tag_guids: updated_guids,
            tag_names: resolved_names,
            updated: saved.updated.map(TimestampMs),
            all_tags,
        })
    }

    fn set_note_notebook(
        &mut self,
        guid: &NoteGuid,
        title: &str,
        notebook_guid: &NotebookGuid,
    ) -> Result<Option<TimestampMs>, ApiError> {
        let token = self.auth.token().to_owned();
        let patch = edam_types::Note {
            guid: Some(guid.as_str().to_owned()),
            title: Some(sanitize_title(title)),
            notebook_guid: Some(notebook_guid.as_str().to_owned()),
            ..Default::default()
        };

        let saved = self.call_note_store(|ns| ns.update_note(token.clone(), patch.clone()))?;
        Ok(saved.updated.map(TimestampMs))
    }

    fn rename_note(
        &mut self,
        guid: &NoteGuid,
        new_title: &str,
    ) -> Result<Option<TimestampMs>, ApiError> {
        let token = self.auth.token().to_owned();
        let patch = edam_types::Note {
            guid: Some(guid.as_str().to_owned()),
            title: Some(sanitize_title(new_title)),
            ..Default::default()
        };

        let saved = self.call_note_store(|ns| ns.update_note(token.clone(), patch.clone()))?;
        Ok(saved.updated.map(TimestampMs))
    }

    fn set_note_source_url(
        &mut self,
        guid: &NoteGuid,
        title: &str,
        source_url: Option<&str>,
    ) -> Result<(Option<String>, Option<TimestampMs>), ApiError> {
        let normalized = match source_url {
            Some(raw) => normalize_source_url(raw).map_err(ApiError::Remote)?,
            None => None,
        };
        // updateNote replaces Note.attributes as a whole. Send the current
        // attributes back with only sourceURL changed.
        let current = {
            let token = self.auth.token().to_owned();
            let guid_str = guid.as_str().to_owned();
            self.call_note_store(|ns| {
                ns.get_note(token.clone(), guid_str.clone(), false, false, false, false)
            })?
        };
        let mut attributes = current.attributes.unwrap_or_default();
        attributes.source_u_r_l.clone_from(&normalized);

        let token = self.auth.token().to_owned();
        let patch = edam_types::Note {
            guid: Some(guid.as_str().to_owned()),
            title: Some(sanitize_title(title)),
            attributes: Some(attributes),
            ..Default::default()
        };
        let saved = self.call_note_store(|ns| ns.update_note(token.clone(), patch.clone()))?;
        let saved_url = match saved.attributes {
            Some(attrs) => attrs.source_u_r_l,
            None => normalized,
        };
        Ok((saved_url, saved.updated.map(TimestampMs)))
    }

    fn create_note(
        &mut self,
        title: &str,
        notebook_guid: Option<&NotebookGuid>,
    ) -> Result<NoteDetail, ApiError> {
        let token = self.auth.token().to_owned();
        let draft = edam_types::Note {
            title: Some(sanitize_title(title)),
            content: Some(enml::empty_enml()),
            notebook_guid: notebook_guid.map(|g| g.0.clone()),
            ..Default::default()
        };

        let created = self.call_note_store(|ns| ns.create_note(token.clone(), draft.clone()))?;
        let guid = NoteGuid(created.guid.unwrap_or_default());
        Ok(NoteDetail {
            guid,
            title: created.title.unwrap_or_else(|| sanitize_title(title)),
            enml: created.content.unwrap_or_else(enml::empty_enml),
            created: created.created.map(TimestampMs),
            updated: created.updated.map(TimestampMs),
            notebook_guid: created.notebook_guid.map(NotebookGuid),
            tag_guids: Vec::new(),
            tag_names: Vec::new(),
            source_url: None,
            content_length: created.content_length.and_then(|n| usize::try_from(n).ok()),
            resources: Vec::new(),
        })
    }
}

/// Trim a source URL for Evernote's `sourceURL` attribute.
/// Empty input clears the URL. Evernote rejects control characters and strings
/// longer than 4096 UTF-16 code units.
pub fn normalize_source_url(raw: &str) -> Result<Option<String>, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    if trimmed.chars().any(char::is_control) {
        return Err("URL cannot contain control characters".to_owned());
    }
    let max = usize::try_from(EDAM_ATTRIBUTE_LEN_MAX).unwrap_or(4096);
    if trimmed.encode_utf16().count() > max {
        return Err(format!("URL is longer than {max} characters"));
    }
    Ok(Some(trimmed.to_owned()))
}

fn sanitize_title(title: &str) -> String {
    let cleaned: String = title
        .trim()
        .chars()
        .filter(|c| !c.is_control())
        .take(255)
        .collect();
    if cleaned.is_empty() {
        "Untitled".to_owned()
    } else {
        cleaned
    }
}

fn map_resource_meta(res: edam_types::Resource) -> ResourceMeta {
    let (body_hash_hex, size) = match res.data {
        Some(data) => {
            let hash = data
                .body_hash
                .as_deref()
                .map(hex_encode)
                .unwrap_or_default();
            let sz = data.size.and_then(|n| usize::try_from(n).ok()).unwrap_or(0);
            (hash, sz)
        }
        None => (String::new(), 0),
    };
    let filename = res.attributes.and_then(|a| a.file_name).unwrap_or_default();
    ResourceMeta {
        guid: res.guid.unwrap_or_default(),
        mime: res.mime,
        filename,
        body_hash_hex,
        size,
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
    }
    s
}

struct HttpThriftState {
    url: String,
    agent: ureq::Agent,
    request: Vec<u8>,
    response: VecDeque<u8>,
}

struct HttpReadHalf {
    state: Arc<Mutex<HttpThriftState>>,
}

struct HttpWriteHalf {
    state: Arc<Mutex<HttpThriftState>>,
}

fn http_halves(url: &str, agent: ureq::Agent) -> (HttpReadHalf, HttpWriteHalf) {
    let state = Arc::new(Mutex::new(HttpThriftState {
        url: url.to_owned(),
        agent,
        request: Vec::new(),
        response: VecDeque::new(),
    }));
    (
        HttpReadHalf {
            state: Arc::clone(&state),
        },
        HttpWriteHalf { state },
    )
}

impl Read for HttpReadHalf {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut st = self
            .state
            .lock()
            .map_err(|_| io::Error::other("Thrift state mutex poisoned"))?;
        let mut n = 0;
        while n < buf.len() {
            let Some(b) = st.response.pop_front() else {
                break;
            };
            buf[n] = b;
            n += 1;
        }
        Ok(n)
    }
}

impl Write for HttpWriteHalf {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut st = self
            .state
            .lock()
            .map_err(|_| io::Error::other("Thrift state mutex poisoned"))?;
        st.request.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        let (url, agent, payload) = {
            let mut st = self
                .state
                .lock()
                .map_err(|_| io::Error::other("Thrift state mutex poisoned"))?;
            let body = std::mem::take(&mut st.request);
            (st.url.clone(), st.agent.clone(), body)
        };

        let mut resp = agent
            .post(&url)
            .header("Content-Type", "application/x-thrift")
            .header("Accept", "application/x-thrift")
            .send(payload.as_slice())
            .map_err(|e| io::Error::other(format!("HTTP error contacting Evernote: {e}")))?;

        let bytes = resp
            .body_mut()
            .read_to_vec()
            .map_err(|e| io::Error::other(format!("Failed reading Evernote response: {e}")))?;

        let mut st = self
            .state
            .lock()
            .map_err(|_| io::Error::other("Thrift state mutex poisoned"))?;
        st.response = bytes.into_iter().collect();
        Ok(())
    }
}

/// HTTP and decode failures mean the shard URL may have moved. Evernote
/// application errors (auth, rate limit, missing note, invalid content) do not.
fn should_refresh_note_store(err: &thrift::Error) -> bool {
    matches!(
        err,
        thrift::Error::Transport(_) | thrift::Error::Protocol(_)
    )
}

fn token_fingerprint(token: &str) -> String {
    format!("{:x}", md5::compute(token.as_bytes()))
}

fn read_note_store_cache(path: &Path, auth: &AuthConfig) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    let cached = parse_note_store_cache(&text)?;
    if cached.user_store != auth.user_store_url
        || cached.token_fp != token_fingerprint(auth.token())
    {
        return None;
    }
    Some(cached.note_store)
}

fn write_note_store_cache(path: &Path, auth: &AuthConfig, note_store: &str) {
    if note_store.contains(|ch: char| ch.is_whitespace()) {
        return;
    }
    let Some(parent) = path.parent() else {
        return;
    };
    if fs::create_dir_all(parent).is_err() {
        return;
    }
    let body = format!(
        "# Cached Evernote note store. Replaced when this URL stops answering.\n\
         user_store={}\n\
         token_fp={}\n\
         note_store={}\n",
        auth.user_store_url,
        token_fingerprint(auth.token()),
        note_store,
    );
    if fs::write(path, body).is_err() {
        return;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    }
}

struct ParsedNoteStoreCache {
    user_store: String,
    token_fp: String,
    note_store: String,
}

fn parse_note_store_cache(text: &str) -> Option<ParsedNoteStoreCache> {
    let mut user_store = None;
    let mut token_fp = None;
    let mut note_store = None;
    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = line.split_once('=')?;
        let value = value.trim();
        if value.is_empty() {
            return None;
        }
        match key.trim() {
            "user_store" => user_store = Some(value.to_owned()),
            "token_fp" => token_fp = Some(value.to_owned()),
            "note_store" if value.starts_with("https://") || value.starts_with("http://") => {
                note_store = Some(value.to_owned());
            }
            "note_store" => return None,
            _ => {}
        }
    }
    Some(ParsedNoteStoreCache {
        user_store: user_store?,
        token_fp: token_fp?,
        note_store: note_store?,
    })
}

fn map_thrift_error(err: thrift::Error) -> ApiError {
    if let thrift::Error::User(boxed) = &err {
        if let Some(uex) = boxed.downcast_ref::<EDAMUserException>() {
            if uex.error_code == EDAMErrorCode::RATE_LIMIT_REACHED {
                return ApiError::RateLimited {
                    retry_after: DEFAULT_RATE_LIMIT_WAIT,
                };
            }
            return ApiError::Remote(format_user_exception(uex));
        }
        if let Some(sex) = boxed.downcast_ref::<EDAMSystemException>() {
            if sex.error_code == EDAMErrorCode::RATE_LIMIT_REACHED {
                let retry_after = sex
                    .rate_limit_duration
                    .and_then(|secs| u64::try_from(secs).ok())
                    .map_or(DEFAULT_RATE_LIMIT_WAIT, Duration::from_secs);
                return ApiError::RateLimited { retry_after };
            }
            let msg = sex.message.as_deref().unwrap_or("system error");
            return ApiError::Remote(format!(
                "Evernote system error ({}): {msg}",
                sex.error_code.0
            ));
        }
        if let Some(nf) = boxed.downcast_ref::<EDAMNotFoundException>() {
            let id = nf.identifier.as_deref().unwrap_or("item");
            return ApiError::Remote(format!("Not found on Evernote: {id}"));
        }
    }
    if should_refresh_note_store(&err) {
        return ApiError::Network(format!("Evernote connection failed: {err}"));
    }
    ApiError::Remote(format!("Evernote API error: {err}"))
}

fn format_user_exception(uex: &EDAMUserException) -> String {
    let code_name = match uex.error_code {
        EDAMErrorCode::BAD_DATA_FORMAT => "BAD_DATA_FORMAT",
        EDAMErrorCode::PERMISSION_DENIED => "PERMISSION_DENIED",
        EDAMErrorCode::INTERNAL_ERROR => "INTERNAL_ERROR",
        EDAMErrorCode::DATA_REQUIRED => "DATA_REQUIRED",
        EDAMErrorCode::LIMIT_REACHED => "LIMIT_REACHED",
        EDAMErrorCode::QUOTA_REACHED => "QUOTA_REACHED",
        EDAMErrorCode::INVALID_AUTH => "INVALID_AUTH",
        EDAMErrorCode::AUTH_EXPIRED => "AUTH_EXPIRED",
        EDAMErrorCode::DATA_CONFLICT => "DATA_CONFLICT",
        EDAMErrorCode::ENML_VALIDATION => "ENML_VALIDATION",
        EDAMErrorCode::RATE_LIMIT_REACHED => "RATE_LIMIT_REACHED",
        _ => "EDAM_USER_ERROR",
    };
    let mut msg = format!("Evernote {code_name} ({})", uex.error_code.0);
    if let Some(param) = uex.parameter.as_deref().filter(|p| !p.is_empty()) {
        msg.push_str(&format!(" [{param}]"));
    }
    if matches!(
        uex.error_code,
        EDAMErrorCode::INVALID_AUTH | EDAMErrorCode::AUTH_EXPIRED
    ) {
        msg.push_str(&format!(
            " -- obtain a valid dev token at {DEVELOPER_TOKEN_URL}"
        ));
    }
    msg
}

#[cfg(test)]
mod tests {
    use super::{
        ApiError, EDAMErrorCode, EDAMSystemException, EDAMUserException, map_thrift_error,
    };
    use super::{
        normalize_source_url, parse_note_store_cache, read_note_store_cache,
        should_refresh_note_store, token_fingerprint, write_note_store_cache,
    };
    use crate::auth::AuthConfig;
    use std::time::Duration;
    use thrift::{
        ApplicationError, ApplicationErrorKind, ProtocolError, ProtocolErrorKind, TransportError,
        TransportErrorKind,
    };

    #[test]
    fn errors_say_whether_a_retry_can_help() {
        let limited = map_thrift_error(thrift::Error::User(Box::new(EDAMSystemException::new(
            EDAMErrorCode::RATE_LIMIT_REACHED,
            None,
            42,
        ))));
        assert_eq!(limited.rate_limit_wait(), Some(Duration::from_secs(42)));
        assert!(limited.is_retryable());
        // The app's prefetch logic looks for this text.
        assert!(limited.to_string().contains("RATE_LIMIT_REACHED"));

        let network = map_thrift_error(thrift::Error::Transport(TransportError::new(
            TransportErrorKind::TimedOut,
            "timed out",
        )));
        assert!(matches!(network, ApiError::Network(_)));
        assert!(network.is_retryable());
        assert_eq!(network.rate_limit_wait(), None);

        let refused = map_thrift_error(thrift::Error::User(Box::new(EDAMUserException::new(
            EDAMErrorCode::ENML_VALIDATION,
            None,
        ))));
        assert!(!refused.is_retryable());
    }

    #[test]
    fn normalize_source_url_trims_clears_and_rejects_invalid_values() {
        assert_eq!(
            normalize_source_url("  https://music.yandex.com/album/1  ").unwrap(),
            Some("https://music.yandex.com/album/1".to_owned())
        );
        assert_eq!(normalize_source_url("   ").unwrap(), None);
        assert!(normalize_source_url("http://bad\nurl").is_err());
        let max = "a".repeat(4096);
        assert_eq!(
            normalize_source_url(&max).unwrap().as_deref(),
            Some(max.as_str())
        );
        assert!(normalize_source_url(&"a".repeat(4097)).is_err());
    }

    #[test]
    fn note_store_cache_round_trips_without_storing_the_token() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("note_store_url");
        let auth = AuthConfig::new(
            "S=s1:U=1:secret-token".to_owned(),
            "https://www.evernote.com/edam/user".to_owned(),
        );
        let url = "https://www.evernote.com/shard/s1/notestore";
        write_note_store_cache(&path, &auth, url);

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("secret-token"));
        assert!(text.contains(&token_fingerprint(auth.token())));
        assert_eq!(read_note_store_cache(&path, &auth).as_deref(), Some(url));

        let other_account = AuthConfig::new(
            "other-token".to_owned(),
            "https://www.evernote.com/edam/user".to_owned(),
        );
        assert_eq!(read_note_store_cache(&path, &other_account), None);

        let other_service = AuthConfig::new(
            "S=s1:U=1:secret-token".to_owned(),
            "https://app.yinxiang.com/edam/user".to_owned(),
        );
        assert_eq!(read_note_store_cache(&path, &other_service), None);
    }

    #[test]
    fn note_store_cache_rejects_a_non_url() {
        let text = "\
user_store=https://www.evernote.com/edam/user
token_fp=abc
note_store=not a url
";
        assert!(parse_note_store_cache(text).is_none());
    }

    #[test]
    fn refreshes_the_note_store_only_after_a_transport_failure() {
        assert!(should_refresh_note_store(&thrift::Error::Transport(
            TransportError::new(TransportErrorKind::Unknown, "HTTP error")
        )));
        assert!(should_refresh_note_store(&thrift::Error::Protocol(
            ProtocolError::new(ProtocolErrorKind::InvalidData, "not thrift")
        )));
        assert!(!should_refresh_note_store(&thrift::Error::Application(
            ApplicationError::new(ApplicationErrorKind::Unknown, "RATE_LIMIT_REACHED")
        )));
    }
}
