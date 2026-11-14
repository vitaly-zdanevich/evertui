use crate::domain::{
    NoteDetail, NoteGuid, Notebook, NotebookGuid, SearchPage, Tag, TagGuid, TimestampMs,
};
use crate::evernote_api::EvernoteService;
use crate::search::CompiledEvernoteFilter;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};

pub const CATALOG_CONTEXT: &str = "Notebooks and tags";

#[derive(Debug)]
pub enum WorkerCommand {
    Bootstrap,
    /// `listNotebooks` and `listTags`. Runs after the first note body so startup can paint.
    LoadCatalog,
    Search {
        request_id: u64,
        filter: CompiledEvernoteFilter,
    },
    LoadNote {
        request_id: u64,
        guid: NoteGuid,
    },
    SaveContent {
        guid: NoteGuid,
        title: String,
        enml: String,
    },
    SetTags {
        guid: NoteGuid,
        title: String,
        tag_names: Vec<String>,
    },
    SetNotebook {
        guid: NoteGuid,
        title: String,
        notebook_guid: NotebookGuid,
    },
    RenameNote {
        guid: NoteGuid,
        new_title: String,
    },
    SetSourceUrl {
        guid: NoteGuid,
        title: String,
        source_url: Option<String>,
    },
    CreateNote {
        title: String,
        notebook_guid: Option<NotebookGuid>,
    },
    Shutdown,
}

#[derive(Debug)]
pub enum WorkerEvent {
    /// Titles are ready. Notebooks and tags may still be in flight.
    InitialNotes {
        page: SearchPage,
    },
    CatalogLoaded {
        notebooks: Vec<Notebook>,
        tags: Vec<Tag>,
    },
    SearchCompleted {
        request_id: u64,
        page: SearchPage,
    },
    NoteLoaded {
        request_id: u64,
        note: NoteDetail,
    },
    NoteLoadFailed {
        request_id: u64,
        guid: NoteGuid,
        message: String,
    },
    ContentSaved {
        guid: NoteGuid,
        enml: String,
        updated: Option<TimestampMs>,
    },
    TagsSaved {
        guid: NoteGuid,
        tag_guids: Vec<TagGuid>,
        tag_names: Vec<String>,
        updated: Option<TimestampMs>,
        all_tags: Option<Vec<Tag>>,
    },
    NotebookSaved {
        guid: NoteGuid,
        notebook_guid: NotebookGuid,
        updated: Option<TimestampMs>,
    },
    NoteRenamed {
        guid: NoteGuid,
        new_title: String,
        updated: Option<TimestampMs>,
    },
    SourceUrlSaved {
        guid: NoteGuid,
        source_url: Option<String>,
        updated: Option<TimestampMs>,
    },
    NoteCreated {
        note: NoteDetail,
    },
    Error {
        context: String,
        message: String,
        was_save: bool,
    },
}

pub struct BackendHandle {
    pub cmd_tx: Sender<WorkerCommand>,
    pub evt_rx: Receiver<WorkerEvent>,
    join_handle: Option<JoinHandle<()>>,
}

impl BackendHandle {
    pub fn spawn(mut service: Box<dyn EvernoteService>) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel::<WorkerCommand>();
        let (evt_tx, evt_rx) = mpsc::channel::<WorkerEvent>();

        let handle = thread::Builder::new()
            .name("evertui-evernote-worker".to_owned())
            .spawn(move || {
                while let Ok(cmd) = cmd_rx.recv() {
                    match cmd {
                        WorkerCommand::Shutdown => break,
                        WorkerCommand::Bootstrap => match service.initial_notes() {
                            Ok(page) => {
                                let _ = evt_tx.send(WorkerEvent::InitialNotes { page });
                            }
                            Err(e) => {
                                let _ = evt_tx.send(WorkerEvent::Error {
                                    context: "Initial sync".to_owned(),
                                    message: e.to_string(),
                                    was_save: false,
                                });
                            }
                        },
                        WorkerCommand::LoadCatalog => match service.load_catalog() {
                            Ok((notebooks, tags)) => {
                                let _ = evt_tx.send(WorkerEvent::CatalogLoaded { notebooks, tags });
                            }
                            Err(e) => {
                                let _ = evt_tx.send(WorkerEvent::Error {
                                    context: CATALOG_CONTEXT.to_owned(),
                                    message: e.to_string(),
                                    was_save: false,
                                });
                            }
                        },
                        WorkerCommand::Search { request_id, filter } => {
                            match service.search_notes(&filter) {
                                Ok(page) => {
                                    let _ = evt_tx
                                        .send(WorkerEvent::SearchCompleted { request_id, page });
                                }
                                Err(e) => {
                                    let _ = evt_tx.send(WorkerEvent::Error {
                                        context: "Search".to_owned(),
                                        message: e.to_string(),
                                        was_save: false,
                                    });
                                }
                            }
                        }
                        WorkerCommand::LoadNote { request_id, guid } => {
                            match service.load_note(&guid) {
                                Ok(note) => {
                                    let _ =
                                        evt_tx.send(WorkerEvent::NoteLoaded { request_id, note });
                                }
                                Err(e) => {
                                    let _ = evt_tx.send(WorkerEvent::NoteLoadFailed {
                                        request_id,
                                        guid,
                                        message: e.to_string(),
                                    });
                                }
                            }
                        }
                        WorkerCommand::SaveContent { guid, title, enml } => {
                            match service.save_note_content(&guid, &title, &enml) {
                                Ok(updated) => {
                                    let _ = evt_tx.send(WorkerEvent::ContentSaved {
                                        guid,
                                        enml,
                                        updated,
                                    });
                                }
                                Err(e) => {
                                    let _ = evt_tx.send(WorkerEvent::Error {
                                        context: "Save note".to_owned(),
                                        message: e.to_string(),
                                        was_save: true,
                                    });
                                }
                            }
                        }
                        WorkerCommand::SetTags {
                            guid,
                            title,
                            tag_names,
                        } => match service.set_note_tags(&guid, &title, &tag_names) {
                            Ok(outcome) => {
                                let _ = evt_tx.send(WorkerEvent::TagsSaved {
                                    guid,
                                    tag_guids: outcome.tag_guids,
                                    tag_names: outcome.tag_names,
                                    updated: outcome.updated,
                                    all_tags: outcome.all_tags,
                                });
                            }
                            Err(e) => {
                                let _ = evt_tx.send(WorkerEvent::Error {
                                    context: "Save tags".to_owned(),
                                    message: e.to_string(),
                                    was_save: true,
                                });
                            }
                        },
                        WorkerCommand::SetNotebook {
                            guid,
                            title,
                            notebook_guid,
                        } => match service.set_note_notebook(&guid, &title, &notebook_guid) {
                            Ok(updated) => {
                                let _ = evt_tx.send(WorkerEvent::NotebookSaved {
                                    guid,
                                    notebook_guid,
                                    updated,
                                });
                            }
                            Err(e) => {
                                let _ = evt_tx.send(WorkerEvent::Error {
                                    context: "Set notebook".to_owned(),
                                    message: e.to_string(),
                                    was_save: true,
                                });
                            }
                        },
                        WorkerCommand::RenameNote { guid, new_title } => {
                            match service.rename_note(&guid, &new_title) {
                                Ok(updated) => {
                                    let _ = evt_tx.send(WorkerEvent::NoteRenamed {
                                        guid,
                                        new_title,
                                        updated,
                                    });
                                }
                                Err(e) => {
                                    let _ = evt_tx.send(WorkerEvent::Error {
                                        context: "Rename note".to_owned(),
                                        message: e.to_string(),
                                        was_save: true,
                                    });
                                }
                            }
                        }
                        WorkerCommand::SetSourceUrl {
                            guid,
                            title,
                            source_url,
                        } => {
                            match service.set_note_source_url(&guid, &title, source_url.as_deref())
                            {
                                Ok((source_url, updated)) => {
                                    let _ = evt_tx.send(WorkerEvent::SourceUrlSaved {
                                        guid,
                                        source_url,
                                        updated,
                                    });
                                }
                                Err(e) => {
                                    let _ = evt_tx.send(WorkerEvent::Error {
                                        context: "Save URL".to_owned(),
                                        message: e.to_string(),
                                        was_save: true,
                                    });
                                }
                            }
                        }
                        WorkerCommand::CreateNote {
                            title,
                            notebook_guid,
                        } => match service.create_note(&title, notebook_guid.as_ref()) {
                            Ok(note) => {
                                let _ = evt_tx.send(WorkerEvent::NoteCreated { note });
                            }
                            Err(e) => {
                                let _ = evt_tx.send(WorkerEvent::Error {
                                    context: "Create note".to_owned(),
                                    message: e.to_string(),
                                    was_save: true,
                                });
                            }
                        },
                    }
                }
            })
            .expect("failed to spawn Evernote worker thread");

        Self {
            cmd_tx,
            evt_rx,
            join_handle: Some(handle),
        }
    }
}

impl Drop for BackendHandle {
    fn drop(&mut self) {
        let _ = self.cmd_tx.send(WorkerCommand::Shutdown);
        if let Some(h) = self.join_handle.take() {
            let _ = h.join();
        }
    }
}
