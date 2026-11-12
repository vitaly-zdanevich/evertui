use crossterm::event::{self, DisableMouseCapture, EnableMouseCapture};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use evertui::app::App;
use evertui::auth::AuthConfig;
use evertui::evernote_api::EdamEvernoteService;
use evertui::preferences::Preferences;
use evertui::ui;
use evertui::worker::BackendHandle;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use std::io::{self, Stdout};
use std::process::ExitCode;
use std::time::{Duration, Instant};

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
    let backend = BackendHandle::spawn(service);

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

    let run_res = run_event_loop(&mut terminal, &mut app, &backend);
    let _ = restore_terminal(&mut terminal);

    run_res.map_err(|e| format!("Runtime I/O error: {e}"))
}

fn run_event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
    backend: &BackendHandle,
) -> io::Result<()> {
    let tick_rate = Duration::from_millis(50);
    let mut last_tick = Instant::now();

    while !app.should_quit {
        while let Ok(evt) = backend.evt_rx.try_recv() {
            app.handle_worker_event(evt);
        }
        app.poll_vim_activity();

        terminal.draw(|frame| ui::render(frame, app))?;

        let timeout = if app.save_indicator.is_animating() || app.loading_list {
            Duration::from_millis(50)
        } else {
            Duration::from_millis(25)
        };

        if event::poll(timeout)? {
            let ev = event::read()?;
            app.handle_terminal_event(ev);
            while event::poll(Duration::ZERO)? {
                let next_ev = event::read()?;
                app.handle_terminal_event(next_ev);
            }
        }

        if last_tick.elapsed() >= tick_rate {
            app.tick();
            last_tick = Instant::now();
        }
    }

    Ok(())
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
