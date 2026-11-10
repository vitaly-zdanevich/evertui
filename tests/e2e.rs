use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use evertui::app::{App, FocusTarget, SaveIndicator};
use evertui::domain::{
    NoteDetail, NoteGuid, NoteSummary, Notebook, NotebookGuid, ResourceMeta, SearchPage, Tag,
    TagGuid, TimestampMs,
};
use evertui::evernote_api::{ApiError, BootstrapData, EvernoteService, UpdatedTagsOutcome};
use evertui::search::CompiledEvernoteFilter;
use evertui::ui;
use evertui::worker::BackendHandle;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Default)]
struct RecordedCalls {
    searches: Vec<CompiledEvernoteFilter>,
    saved_contents: Vec<(NoteGuid, String)>,
    saved_tags: Vec<(NoteGuid, Vec<String>)>,
    saved_notebooks: Vec<(NoteGuid, NotebookGuid)>,
}

struct MockEvernoteService {
    calls: Arc<Mutex<RecordedCalls>>,
}

impl EvernoteService for MockEvernoteService {
    fn bootstrap(&mut self) -> Result<BootstrapData, ApiError> {
        let notebooks = vec![
            Notebook {
                guid: NotebookGuid::from("nb-inbox"),
                name: "INBOX".to_owned(),
                stack: None,
                is_default: true,
            },
            Notebook {
                guid: NotebookGuid::from("nb-archive"),
                name: "Archive".to_owned(),
                stack: Some("Personal".to_owned()),
                is_default: false,
            },
        ];
        let tags = vec![
            Tag {
                guid: TagGuid::from("tg-1"),
                name: "Evernote".to_owned(),
            },
            Tag {
                guid: TagGuid::from("tg-2"),
                name: "tui".to_owned(),
            },
        ];
        let notes = (1..=25)
            .map(|i| NoteSummary {
                guid: NoteGuid(format!("note-{i}")),
                title: format!("Title #{i}"),
                created: Some(TimestampMs(1_762_000_000_000)),
                updated: Some(TimestampMs(1_762_050_000_000)),
                notebook_guid: Some(NotebookGuid::from("nb-inbox")),
                tag_guids: vec![TagGuid::from("tg-1"), TagGuid::from("tg-2")],
                content_length: Some(256),
            })
            .collect();
        Ok(BootstrapData {
            notebooks,
            tags,
            initial_page: SearchPage {
                total_matching: 25,
                notes,
            },
        })
    }

    fn search_notes(&mut self, filter: &CompiledEvernoteFilter) -> Result<SearchPage, ApiError> {
        self.calls.lock().unwrap().searches.push(filter.clone());
        Ok(SearchPage {
            total_matching: 1,
            notes: vec![NoteSummary {
                guid: NoteGuid::from("note-1"),
                title: "Build Evernote TUI".to_owned(),
                created: Some(TimestampMs(1_762_000_000_000)),
                updated: Some(TimestampMs(1_762_050_000_000)),
                notebook_guid: Some(NotebookGuid::from("nb-inbox")),
                tag_guids: vec![TagGuid::from("tg-1"), TagGuid::from("tg-2")],
                content_length: Some(512),
            }],
        })
    }

    fn load_note(&mut self, guid: &NoteGuid) -> Result<NoteDetail, ApiError> {
        Ok(NoteDetail {
            guid: guid.clone(),
            title: "Build Evernote TUI".to_owned(),
            enml: "<en-note><div>First paragraph</div>\
                   <div><en-media type=\"image/png\" hash=\"11112222333344445555666677778888\"/></div>\
                   <div><en-media type=\"application/pdf\" hash=\"aaaabbbbccccddddeeeeffff00001111\"/></div>\
                   </en-note>"
                .to_owned(),
            created: Some(TimestampMs(1_762_000_000_000)),
            updated: Some(TimestampMs(1_762_050_000_000)),
            notebook_guid: Some(NotebookGuid::from("nb-inbox")),
            tag_guids: vec![TagGuid::from("tg-1"), TagGuid::from("tg-2")],
            tag_names: vec!["Evernote".to_owned(), "tui".to_owned()],
            source_url: None,
            content_length: Some(512),
            resources: vec![
                ResourceMeta {
                    guid: "res-img".to_owned(),
                    mime: Some("image/png".to_owned()),
                    filename: "wireframe.png".to_owned(),
                    body_hash_hex: "11112222333344445555666677778888".to_owned(),
                    size: 4096,
                },
                ResourceMeta {
                    guid: "res-pdf".to_owned(),
                    mime: Some("application/pdf".to_owned()),
                    filename: "notes.pdf".to_owned(),
                    body_hash_hex: "aaaabbbbccccddddeeeeffff00001111".to_owned(),
                    size: 8192,
                },
            ],
        })
    }

    fn save_note_content(
        &mut self,
        guid: &NoteGuid,
        _title: &str,
        enml_content: &str,
    ) -> Result<Option<TimestampMs>, ApiError> {
        self.calls
            .lock()
            .unwrap()
            .saved_contents
            .push((guid.clone(), enml_content.to_owned()));
        Ok(Some(TimestampMs(1_762_100_000_000)))
    }

    fn set_note_tags(
        &mut self,
        guid: &NoteGuid,
        _title: &str,
        tag_names: &[String],
    ) -> Result<UpdatedTagsOutcome, ApiError> {
        self.calls
            .lock()
            .unwrap()
            .saved_tags
            .push((guid.clone(), tag_names.to_vec()));
        Ok(UpdatedTagsOutcome {
            tag_guids: tag_names
                .iter()
                .enumerate()
                .map(|(i, _)| TagGuid(format!("tg-new-{i}")))
                .collect(),
            tag_names: tag_names.to_vec(),
            updated: Some(TimestampMs(1_762_100_000_000)),
            all_tags: None,
        })
    }

    fn set_note_notebook(
        &mut self,
        guid: &NoteGuid,
        _title: &str,
        notebook_guid: &NotebookGuid,
    ) -> Result<Option<TimestampMs>, ApiError> {
        self.calls
            .lock()
            .unwrap()
            .saved_notebooks
            .push((guid.clone(), notebook_guid.clone()));
        Ok(Some(TimestampMs(1_762_100_000_000)))
    }

    fn rename_note(
        &mut self,
        _guid: &NoteGuid,
        _new_title: &str,
    ) -> Result<Option<TimestampMs>, ApiError> {
        Ok(Some(TimestampMs(1_762_100_000_000)))
    }

    fn set_note_source_url(
        &mut self,
        _guid: &NoteGuid,
        _title: &str,
        source_url: Option<&str>,
    ) -> Result<(Option<String>, Option<TimestampMs>), ApiError> {
        Ok((
            source_url.map(str::to_owned),
            Some(TimestampMs(1_762_100_000_000)),
        ))
    }

    fn create_note(
        &mut self,
        title: &str,
        notebook_guid: Option<&NotebookGuid>,
    ) -> Result<NoteDetail, ApiError> {
        Ok(NoteDetail {
            guid: NoteGuid::from("created-1"),
            title: title.to_owned(),
            enml: "<en-note><div><br/></div></en-note>".to_owned(),
            created: Some(TimestampMs(1_762_100_000_000)),
            updated: Some(TimestampMs(1_762_100_000_000)),
            notebook_guid: notebook_guid.cloned(),
            tag_guids: Vec::new(),
            tag_names: Vec::new(),
            source_url: None,
            content_length: Some(38),
            resources: Vec::new(),
        })
    }
}

fn key_press(code: KeyCode) -> Event {
    Event::Key(KeyEvent::new(code, KeyModifiers::empty()))
}

#[test]
fn full_tui_workflow_with_placeholders_search_tags_notebook_and_real_vim_save() {
    let calls = Arc::new(Mutex::new(RecordedCalls::default()));
    let service = Box::new(MockEvernoteService {
        calls: Arc::clone(&calls),
    });
    let backend = BackendHandle::spawn(service);
    let mut app = App::new(backend.cmd_tx.clone());
    let mut terminal = Terminal::new(TestBackend::new(120, 32)).unwrap();

    // Wait for bootstrap + note-1 load + real Vim rendering placeholders.
    let deadline = Instant::now() + Duration::from_secs(6);
    let mut screen_text = String::new();
    while Instant::now() < deadline {
        while let Ok(evt) = backend.evt_rx.try_recv() {
            app.handle_worker_event(evt);
        }
        app.poll_vim_activity();
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();
        screen_text = buffer_to_string(terminal.backend().buffer());
        if screen_text.contains("[Image: wireframe.png]")
            && screen_text.contains("[Attachment: notes.pdf]")
        {
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }

    // 1. Left panel enforces 20 titles max, and digits 1-9 jump directly to note indices.
    assert_eq!(app.notes.len(), 20);
    app.handle_terminal_event(key_press(KeyCode::Char('9')));
    assert_eq!(app.selected_index, 8);
    app.handle_terminal_event(key_press(KeyCode::Char('1')));
    assert_eq!(app.selected_index, 0);
    while let Ok(evt) = backend.evt_rx.try_recv() {
        app.handle_worker_event(evt);
    }
    // 2. Default mode (`vim_preview == false`) renders fast native Text Preview without spawning Vim,
    //    and places `Title:` directly below the top bar without a `"Note | Text Preview..."` row.
    assert!(!app.preferences.vim_preview);
    assert!(app.vim_session.is_none());
    assert!(!screen_text.contains("Note | Text Preview"));
    assert_eq!(app.geometry.title_hit.y, 1);
    assert!(screen_text.contains("INBOX"));
    assert!(screen_text.contains("#Evernote #tui"));
    assert!(screen_text.contains("[Image: wireframe.png]"));
    assert!(screen_text.contains("[Attachment: notes.pdf]"));

    // 2b. Open Preferences with `p`, verify `[ ] Vim preview`, toggle on & off, close with Esc.
    app.handle_terminal_event(key_press(KeyCode::Char('p')));
    terminal.draw(|f| ui::render(f, &mut app)).unwrap();
    let prefs_screen = buffer_to_string(terminal.backend().buffer());
    assert!(prefs_screen.contains("Preferences (p)"));
    assert!(prefs_screen.contains("[ ] Vim preview"));

    app.handle_terminal_event(key_press(KeyCode::Char(' ')));
    assert!(app.preferences.vim_preview);
    assert!(app.vim_session.is_some());
    terminal.draw(|f| ui::render(f, &mut app)).unwrap();
    assert!(buffer_to_string(terminal.backend().buffer()).contains("[x] Vim preview"));

    // Uncheck back to fast text preview default and close Preferences.
    app.handle_terminal_event(key_press(KeyCode::Char(' ')));
    assert!(!app.preferences.vim_preview);
    assert!(app.vim_session.is_none());
    app.handle_terminal_event(key_press(KeyCode::Esc));

    // 3. Search using space-separated aliases `t tui n INBOX`.
    app.handle_terminal_event(key_press(KeyCode::Char('/')));
    assert_eq!(app.focus, FocusTarget::SearchInput);
    for ch in "t tui n INBOX".chars() {
        app.handle_terminal_event(key_press(KeyCode::Char(ch)));
    }
    app.handle_terminal_event(key_press(KeyCode::Enter));
    assert!(app.loading_list);

    let search_deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < search_deadline && app.loading_list {
        while let Ok(evt) = backend.evt_rx.try_recv() {
            app.handle_worker_event(evt);
        }
        thread::sleep(Duration::from_millis(15));
    }
    {
        let rec = calls.lock().unwrap();
        assert_eq!(rec.searches.len(), 1);
        assert_eq!(
            rec.searches[0].notebook_guid,
            Some(NotebookGuid::from("nb-inbox"))
        );
        assert_eq!(rec.searches[0].words.as_deref(), Some("tag:tui"));
    }

    // 4. Add a new tag ("rust") via Tag Modal (`t`).
    app.handle_terminal_event(key_press(KeyCode::Char('t')));
    for ch in "rust".chars() {
        app.handle_terminal_event(key_press(KeyCode::Char(ch)));
    }
    app.handle_terminal_event(key_press(KeyCode::Enter));
    assert!(matches!(app.save_indicator, SaveIndicator::Saving { .. }));

    // 5. Switch notebook to Archive (`n` -> Down -> Enter).
    let tag_deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < tag_deadline && calls.lock().unwrap().saved_tags.is_empty() {
        while let Ok(evt) = backend.evt_rx.try_recv() {
            app.handle_worker_event(evt);
        }
        thread::sleep(Duration::from_millis(15));
    }
    assert!(
        calls.lock().unwrap().saved_tags[0]
            .1
            .contains(&"rust".to_owned())
    );

    app.handle_terminal_event(key_press(KeyCode::Char('n')));
    for ch in "Archive".chars() {
        app.handle_terminal_event(key_press(KeyCode::Char(ch)));
    }
    app.handle_terminal_event(key_press(KeyCode::Enter));
    let nb_deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < nb_deadline && calls.lock().unwrap().saved_notebooks.is_empty() {
        while let Ok(evt) = backend.evt_rx.try_recv() {
            app.handle_worker_event(evt);
        }
        thread::sleep(Duration::from_millis(15));
    }
    assert_eq!(
        calls.lock().unwrap().saved_notebooks[0].1,
        NotebookGuid::from("nb-archive")
    );

    // 6. Drag vertical separator by mouse and hide/show left panel.
    terminal.draw(|f| ui::render(f, &mut app)).unwrap();
    let sep_x = app.geometry.separator_x.expect("separator visible");
    app.handle_terminal_event(Event::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: sep_x,
        row: 10,
        modifiers: KeyModifiers::empty(),
    }));
    app.handle_terminal_event(Event::Mouse(MouseEvent {
        kind: MouseEventKind::Drag(MouseButton::Left),
        column: 44,
        row: 10,
        modifiers: KeyModifiers::empty(),
    }));
    app.handle_terminal_event(Event::Mouse(MouseEvent {
        kind: MouseEventKind::Up(MouseButton::Left),
        column: 44,
        row: 10,
        modifiers: KeyModifiers::empty(),
    }));
    assert_eq!(app.left_width.get(), 44);

    app.handle_terminal_event(key_press(KeyCode::F(2)));
    assert!(!app.left_panel_visible);
    terminal.draw(|f| ui::render(f, &mut app)).unwrap();
    assert!(app.geometry.separator_x.is_none());
    assert!(app.vim_session.is_none());

    // 7. Press Enter to replace text preview with real embedded Vim, edit & `:wq`,
    //    verify ENML preserves <en-media> tags and exiting Vim returns to fast text preview!
    app.handle_terminal_event(key_press(KeyCode::Enter));
    assert!(app.vim_session.is_some());
    if let Some(vim) = &mut app.vim_session {
        vim.write_raw(b"GoEdited inside embedded vim!\x1b:wq\r");
    }

    let vim_save_deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < vim_save_deadline
        && (calls.lock().unwrap().saved_contents.is_empty() || app.vim_session.is_some())
    {
        app.poll_vim_activity();
        while let Ok(evt) = backend.evt_rx.try_recv() {
            app.handle_worker_event(evt);
        }
        thread::sleep(Duration::from_millis(20));
    }

    assert!(app.vim_session.is_none());
    terminal.draw(|f| ui::render(f, &mut app)).unwrap();
    let updated_preview = buffer_to_string(terminal.backend().buffer());
    assert!(updated_preview.contains("Edited inside embedded vim!"));

    let saved = calls.lock().unwrap();
    assert_eq!(saved.saved_contents.len(), 1);
    let saved_enml = &saved.saved_contents[0].1;
    assert!(saved_enml.contains("Edited inside embedded vim!"));
    assert!(saved_enml.contains("11112222333344445555666677778888"));
    assert!(saved_enml.contains("aaaabbbbccccddddeeeeffff00001111"));
}

fn buffer_to_string(buf: &ratatui::buffer::Buffer) -> String {
    let area = buf.area;
    let mut out = String::new();
    for y in area.y..area.y + area.height {
        for x in area.x..area.x + area.width {
            out.push_str(buf[(x, y)].symbol());
        }
        out.push('\n');
    }
    out
}
