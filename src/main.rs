use crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use evertui::app::App;
use evertui::auth::AuthConfig;
use evertui::evernote_api::EdamEvernoteService;
use evertui::preferences::Preferences;
use evertui::ui;
use evertui::wake::Waker;
use evertui::worker::BackendHandle;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use std::io::{self, Stdout};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::Instant;

/// Everything that can wake the UI loop. It sleeps until one arrives.
enum LoopEvent {
    Terminal(Event),
    TerminalFailed(io::Error),
    /// The Evernote worker or Vim has news; details are read from their own channels.
    Wake,
}

struct CliOptions {
    token: Option<String>,
    initial_query: Option<String>,
}

fn main() -> ExitCode {
    match try_main() {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("evertui: {msg}");
            ExitCode::FAILURE
        }
    }
}

fn try_main() -> Result<(), String> {
    let opts = match parse_args(std::env::args().skip(1).collect())? {
        ArgAction::Help => {
            print_help();
            return Ok(());
        }
        ArgAction::Version => {
            println!("evertui {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        ArgAction::Run(opts) => opts,
    };

    let auth = AuthConfig::resolve(opts.token).map_err(|e| e.to_string())?;
    let service = Box::new(EdamEvernoteService::new(auth));
    let (loop_tx, loop_rx) = mpsc::channel::<LoopEvent>();
    let (waker, wake_pending) = coalescing_waker(loop_tx.clone());
    let backend = BackendHandle::spawn_with_waker(service, waker.clone());

    install_panic_hook();
    let mut terminal = setup_terminal().map_err(|e| format!("Terminal init failed: {e}"))?;

    let prefs_path = Preferences::default_path();
    let prefs = Preferences::load_or_default(&prefs_path);
    let mut app = App::with_preferences(
        backend.cmd_tx.clone(),
        prefs,
        Some(prefs_path),
        opts.initial_query,
    );
    app.set_waker(waker);
    spawn_terminal_reader(loop_tx).map_err(|e| format!("Input thread failed: {e}"))?;

    let run_res = run_event_loop(&mut terminal, &mut app, &backend, &loop_rx, &wake_pending);
    let _ = restore_terminal(&mut terminal);

    run_res.map_err(|e| format!("Runtime I/O error: {e}"))
}

/// Sends at most one `Wake` until the loop picks it up, so a burst of Vim output or worker
/// events costs one loop pass, not one per chunk.
/// The flag says a `Wake` is already queued; the loop clears it before reading the sources.
fn coalescing_waker(tx: Sender<LoopEvent>) -> (Waker, Arc<AtomicBool>) {
    let pending = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&pending);
    let waker = Waker::new(move || {
        if !flag.swap(true, Ordering::AcqRel) {
            let _ = tx.send(LoopEvent::Wake);
        }
    });
    (waker, pending)
}

fn spawn_terminal_reader(tx: Sender<LoopEvent>) -> io::Result<()> {
    thread::Builder::new()
        .name("evertui-terminal-input".to_owned())
        .spawn(move || {
            loop {
                let msg = match event::read() {
                    Ok(ev) => LoopEvent::Terminal(ev),
                    Err(e) => LoopEvent::TerminalFailed(e),
                };
                let failed = matches!(msg, LoopEvent::TerminalFailed(_));
                if tx.send(msg).is_err() || failed {
                    break;
                }
            }
        })
        .map(drop)
}

/// Sleeps until input, a worker event, Vim output or exit, or a timer the app asked for
/// (`App::next_deadline`), then applies everything pending and draws once.
fn run_event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
    backend: &BackendHandle,
    loop_rx: &Receiver<LoopEvent>,
    wake_pending: &AtomicBool,
) -> io::Result<()> {
    loop {
        // Clear before reading the sources, so news that lands meanwhile sends a fresh `Wake`.
        wake_pending.store(false, Ordering::Release);
        while let Ok(evt) = backend.evt_rx.try_recv() {
            app.handle_worker_event(evt);
        }
        app.poll_vim_activity();
        if app
            .next_deadline()
            .is_some_and(|deadline| deadline <= Instant::now())
        {
            app.tick();
        }
        if app.should_quit {
            return Ok(());
        }

        terminal.draw(|frame| ui::render(frame, app))?;

        let first = match app.next_deadline() {
            Some(deadline) => {
                match loop_rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                    Ok(msg) => Some(msg),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => return Ok(()),
                }
            }
            None => match loop_rx.recv() {
                Ok(msg) => Some(msg),
                Err(_) => return Ok(()),
            },
        };
        for msg in first
            .into_iter()
            .chain(std::iter::from_fn(|| loop_rx.try_recv().ok()))
        {
            match msg {
                LoopEvent::Terminal(ev) => app.handle_terminal_event(ev),
                LoopEvent::TerminalFailed(e) => return Err(e),
                LoopEvent::Wake => {}
            }
        }
    }
}

fn setup_terminal() -> io::Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    Terminal::new(backend)
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> io::Result<()> {
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()
}

fn install_panic_hook() {
    let orig = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture);
        orig(info);
    }));
}

enum ArgAction {
    Help,
    Version,
    Run(CliOptions),
}

fn parse_args(args: Vec<String>) -> Result<ArgAction, String> {
    let mut token = None;
    let mut query_parts = Vec::new();
    let mut it = args.into_iter();

    while let Some(arg) = it.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(ArgAction::Help),
            "-V" | "--version" => return Ok(ArgAction::Version),
            "--token" => {
                let val = it
                    .next()
                    .ok_or_else(|| "--token requires a developer token argument".to_owned())?;
                token = Some(val);
            }
            _ if arg.starts_with("--token=") => {
                token = Some(arg["--token=".len()..].to_owned());
            }
            _ => query_parts.push(arg),
        }
    }

    let initial_query = if query_parts.is_empty() {
        None
    } else {
        Some(query_parts.join(" "))
    };

    Ok(ArgAction::Run(CliOptions {
        token,
        initial_query,
    }))
}

fn print_help() {
    println!(
        "evertui {}\n\
         Two-panel Evernote TUI with embedded real Vim.\n\n\
         USAGE:\n\
         \x20 evertui [--token <DEV_TOKEN>] [INITIAL_QUERY...]\n\n\
         AUTHENTICATION (checked in order):\n\
         \x20 1. --token <DEV_TOKEN>\n\
         \x20 2. EVERTUI_DEV_TOKEN or EVERNOTE_DEV_TOKEN environment variable\n\
         \x20 3. ~/.config/evertui/token\n\
         \x20 4. ~/.reeknote/reeknote.db (existing reeknote session)\n\n\
         SEARCH SYNTAX:\n\
         \x20 tag:foo  notebook:bar  t:foo  n:bar  \"t foo\"  \"n bar\"\n",
        env!("CARGO_PKG_VERSION")
    );
}
