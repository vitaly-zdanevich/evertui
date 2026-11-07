use crate::domain::NoteGuid;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use rustix::fd::OwnedFd;
use rustix::pty::{OpenptFlags, grantpt, ioctl_tiocgptpeer, openpt, ptsname, unlockpt};
use rustix::termios::{Winsize, tcsetwinsize};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::num::NonZeroU16;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;
use tempfile::TempDir;

struct PtyCallbacks {
    writer: Arc<Mutex<File>>,
}

impl vt100::Callbacks for PtyCallbacks {
    fn unhandled_csi(
        &mut self,
        screen: &mut vt100::Screen,
        i1: Option<u8>,
        i2: Option<u8>,
        params: &[&[u16]],
        c: char,
    ) {
        match (i1, i2, c) {
            // Cursor Position Report (DSR 6)
            (None, None, 'n') if params.first().copied() == Some(&[6u16][..]) => {
                let (row, col) = screen.cursor_position();
                let reply = format!("\x1b[{};{}R", row.saturating_add(1), col.saturating_add(1));
                self.send_bytes(reply.as_bytes());
            }
            // Primary Device Attributes (DA1)
            (None, None, 'c') => {
                self.send_bytes(b"\x1b[?1;2c");
            }
            // Secondary Device Attributes (DA2)
            (Some(b'>'), None, 'c') => {
                self.send_bytes(b"\x1b[>0;115;0c");
            }
            _ => {}
        }
    }

    fn unhandled_osc(&mut self, _screen: &mut vt100::Screen, params: &[&[u8]]) {
        // OSC 11 background query used by Vim to detect dark/light theme without stalling.
        if params.first().copied() == Some(&b"11"[..]) && params.get(1).copied() == Some(&b"?"[..])
        {
            self.send_bytes(b"\x1b]11;rgb:1818/1818/2020\x07");
        }
    }
}

impl PtyCallbacks {
    fn send_bytes(&self, bytes: &[u8]) {
        if let Ok(mut w) = self.writer.lock() {
            let _ = w.write_all(bytes);
            let _ = w.flush();
        }
    }
}

/// Manages an embedded real `vim` process running inside a Linux PTY and rendered via `vt100`.
pub struct VimSession {
    pub note_guid: NoteGuid,
    _temp_dir: TempDir,
    note_file_path: PathBuf,
    saves_log_path: PathBuf,
    last_saves_len: u64,
    master_fd: OwnedFd,
    writer: Arc<Mutex<File>>,
    output_rx: Receiver<Vec<u8>>,
    parser: vt100::Parser<PtyCallbacks>,
    child: Child,
    rows: u16,
    cols: u16,
    basic_tty: bool,
}

impl VimSession {
    pub fn spawn(
        note_guid: NoteGuid,
        initial_markdown: &str,
        rows: u16,
        cols: u16,
    ) -> io::Result<Self> {
        let rows = rows.max(2);
        let cols = cols.max(10);
        let nz_rows = NonZeroU16::new(rows).expect("clamped >= 2");
        let nz_cols = NonZeroU16::new(cols).expect("clamped >= 10");

        let temp_dir = tempfile::Builder::new().prefix("evertui-vim-").tempdir()?;
        let safe_id: String = note_guid
            .as_str()
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
            .take(48)
            .collect();
        let file_stem = if safe_id.is_empty() { "note" } else { &safe_id };
        let note_file_path = temp_dir.path().join(format!("{file_stem}.md"));
        let saves_log_path = temp_dir.path().join("saves.log");

        fs::write(&note_file_path, initial_markdown)?;
        fs::write(&saves_log_path, b"")?;

        let (master_fd, slave_fd) = open_pty_pair(rows, cols)?;
        let reader_fd = master_fd.try_clone()?;
        let writer_fd = master_fd.try_clone()?;

        let slave_in = slave_fd.try_clone()?;
        let slave_out = slave_fd.try_clone()?;
        let slave_err = slave_fd;

        let vim_bin = std::env::var("EVERTUI_VIM")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "vim".to_owned());

        let outer_term = std::env::var("TERM").ok();
        let basic_tty = is_basic_tty_term(outer_term.as_deref());

        let autocmd = format!(
            "autocmd BufWritePost <buffer> silent! call writefile(['saved'], '{}', 'a')",
            vim_single_quote(&saves_log_path)
        );

        let mut cmd = if has_setsid() {
            let mut c = Command::new("setsid");
            c.arg("-w").arg("-c").arg(&vim_bin);
            c
        } else {
            Command::new(&vim_bin)
        };

        cmd.arg("-n")
            .arg("--cmd")
            .arg("set t_RV= t_u7= | let g:loaded_confirm_quit=1")
            .arg("-c")
            .arg("set filetype=markdown")
            .arg("-c")
            .arg(&autocmd);

        if basic_tty {
            cmd.arg("-c")
                .arg("set notermguicolors")
                .env("TERM", "linux")
                .env_remove("COLORTERM");
        } else {
            cmd.env("TERM", "xterm-256color");
            match std::env::var("COLORTERM") {
                Ok(ct) if !ct.trim().is_empty() => {
                    cmd.env("COLORTERM", ct);
                }
                _ => {
                    cmd.env_remove("COLORTERM");
                }
            }
        }

        cmd.arg(&note_file_path)
            .stdin(Stdio::from(slave_in))
            .stdout(Stdio::from(slave_out))
            .stderr(Stdio::from(slave_err));

        let child = cmd.spawn()?;

        let writer = Arc::new(Mutex::new(File::from(writer_fd)));
        let callbacks = PtyCallbacks {
            writer: Arc::clone(&writer),
        };
        let parser = vt100::Parser::new_with_callbacks(nz_rows, nz_cols, 0, callbacks);

        let (output_tx, output_rx) = mpsc::channel::<Vec<u8>>();
        thread::Builder::new()
            .name("evertui-vim-pty-reader".to_owned())
            .spawn(move || {
                let mut reader = File::from(reader_fd);
                let mut buf = [0u8; 8192];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if output_tx.send(buf[..n].to_vec()).is_err() {
                                break;
                            }
                        }
                    }
                }
            })?;

        Ok(Self {
            note_guid,
            _temp_dir: temp_dir,
            note_file_path,
            saves_log_path,
            last_saves_len: 0,
            master_fd,
            writer,
            output_rx,
            parser,
            child,
            rows,
            cols,
            basic_tty,
        })
    }

    /// Drains any pending PTY output into the `vt100` screen parser.
    /// Returns `true` if screen bytes were processed.
    pub fn drain_output(&mut self) -> bool {
        let mut updated = false;
        while let Ok(chunk) = self.output_rx.try_recv() {
            self.parser.process(&chunk);
            updated = true;
        }
        updated
    }

    /// Checks whether Vim completed a `:w` (`BufWritePost`) since the last check.
    /// If so, reads and returns the freshly written Markdown content.
    pub fn poll_saved_markdown(&mut self) -> Option<String> {
        let meta = fs::metadata(&self.saves_log_path).ok()?;
        let len = meta.len();
        if len > self.last_saves_len {
            self.last_saves_len = len;
            fs::read_to_string(&self.note_file_path).ok()
        } else {
            None
        }
    }

    /// Returns `true` if the `vim` child process has exited (e.g. `:q` or `:wq`).
    pub fn has_exited(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(Some(_)))
    }

    /// Resizes both the kernel PTY (`TIOCSWINSZ` -> `SIGWINCH` to Vim) and the `vt100` grid.
    pub fn resize(&mut self, rows: u16, cols: u16) {
        let rows = rows.max(2);
        let cols = cols.max(10);
        if self.rows == rows && self.cols == cols {
            return;
        }
        self.rows = rows;
        self.cols = cols;
        if let (Some(nz_r), Some(nz_c)) = (NonZeroU16::new(rows), NonZeroU16::new(cols)) {
            self.parser.set_size(nz_r, nz_c);
        }
        let ws = Winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let _ = tcsetwinsize(&self.master_fd, ws);
    }

    pub fn send_key(&mut self, key: KeyEvent) {
        let app_cursor = self.parser.screen().application_cursor();
        if let Some(bytes) = encode_key_event(key, app_cursor) {
            self.write_raw(&bytes);
        }
    }

    pub fn scroll_lines(&mut self, up: bool) {
        // Ctrl+Y scrolls Vim window up one line; Ctrl+E scrolls down one line in Normal mode.
        if up {
            self.write_raw(&[0x19, 0x19, 0x19]);
        } else {
            self.write_raw(&[0x05, 0x05, 0x05]);
        }
    }

    /// Forwards mouse clicks, drags, releases, and wheel scrolling inside `area` to Vim
    /// when Vim has enabled terminal mouse tracking (`set mouse=a`).
    pub fn send_mouse(&mut self, mouse: MouseEvent, area: Rect) {
        if mouse.column < area.x
            || mouse.column >= area.x.saturating_add(area.width)
            || mouse.row < area.y
            || mouse.row >= area.y.saturating_add(area.height)
        {
            return;
        }

        let mode = self.parser.screen().mouse_protocol_mode();
        if mode == vt100::MouseProtocolMode::None {
            match mouse.kind {
                MouseEventKind::ScrollUp => self.scroll_lines(true),
                MouseEventKind::ScrollDown => self.scroll_lines(false),
                _ => {}
            }
            return;
        }

        let x = mouse.column.saturating_sub(area.x).saturating_add(1);
        let y = mouse.row.saturating_sub(area.y).saturating_add(1);

        let (cb, suffix) = match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => (0u8, 'M'),
            MouseEventKind::Down(MouseButton::Middle) => (1u8, 'M'),
            MouseEventKind::Down(MouseButton::Right) => (2u8, 'M'),
            MouseEventKind::Up(MouseButton::Left) => (0u8, 'm'),
            MouseEventKind::Up(MouseButton::Middle) => (1u8, 'm'),
            MouseEventKind::Up(MouseButton::Right) => (2u8, 'm'),
            MouseEventKind::Drag(MouseButton::Left) => (32u8, 'M'),
            MouseEventKind::Drag(MouseButton::Middle) => (33u8, 'M'),
            MouseEventKind::Drag(MouseButton::Right) => (34u8, 'M'),
            MouseEventKind::ScrollUp => (64u8, 'M'),
            MouseEventKind::ScrollDown => (65u8, 'M'),
            _ => return,
        };

        if self.parser.screen().mouse_protocol_encoding() == vt100::MouseProtocolEncoding::Sgr {
            let seq = format!("\x1b[<{cb};{x};{y}{suffix}");
            self.write_raw(seq.as_bytes());
        } else if suffix == 'M' && x <= 223 && y <= 223 {
            let b0 = cb.saturating_add(32);
            let bx = u8::try_from(x).unwrap_or(1).saturating_add(32);
            let by = u8::try_from(y).unwrap_or(1).saturating_add(32);
            self.write_raw(&[0x1b, b'[', b'M', b0, bx, by]);
        }
    }

    pub fn write_raw(&self, bytes: &[u8]) {
        if let Ok(mut w) = self.writer.lock() {
            let _ = w.write_all(bytes);
            let _ = w.flush();
        }
    }

    /// Renders the `vt100::Screen` into the given Ratatui `Rect`, returning the hardware
    /// cursor coordinate if Vim wants the cursor visible.
    pub fn render(&self, area: Rect, buf: &mut Buffer) -> Option<(u16, u16)> {
        if area.width == 0 || area.height == 0 {
            return None;
        }
        let screen = self.parser.screen();
        let (scr_rows, scr_cols) = screen.size();
        let max_r = area.height.min(scr_rows.get());
        let max_c = area.width.min(scr_cols.get());

        for r in 0..max_r {
            for c in 0..max_c {
                let Some(cell) = screen.cell(r, c) else {
                    continue;
                };
                if cell.is_wide_continuation() {
                    continue;
                }
                let dst = &mut buf[(area.x + c, area.y + r)];
                if cell.has_contents() {
                    dst.set_symbol(cell.contents());
                } else {
                    dst.set_symbol(" ");
                }
                dst.set_style(cell_to_style(cell, self.basic_tty));
            }
        }

        if screen.hide_cursor() {
            None
        } else {
            let (cr, cc) = screen.cursor_position();
            if cr < area.height && cc < area.width {
                Some((area.x + cc, area.y + cr))
            } else {
                None
            }
        }
    }
}

impl Drop for VimSession {
    fn drop(&mut self) {
        if let Ok(None) = self.child.try_wait() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn open_pty_pair(rows: u16, cols: u16) -> io::Result<(OwnedFd, OwnedFd)> {
    let master = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY | OpenptFlags::CLOEXEC)
        .map_err(io::Error::from)?;
    grantpt(&master).map_err(io::Error::from)?;
    unlockpt(&master).map_err(io::Error::from)?;

    let ws = Winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    tcsetwinsize(&master, ws).map_err(io::Error::from)?;

    let slave = match ioctl_tiocgptpeer(&master, OpenptFlags::RDWR | OpenptFlags::NOCTTY) {
        Ok(fd) => fd,
        Err(_) => {
            let name = ptsname(&master, Vec::new()).map_err(io::Error::from)?;
            let path = PathBuf::from(name.to_string_lossy().into_owned());
            let file = File::options().read(true).write(true).open(path)?;
            OwnedFd::from(file)
        }
    };

    Ok((master, slave))
}

fn has_setsid() -> bool {
    Path::new("/usr/bin/setsid").is_file() || Path::new("/bin/setsid").is_file()
}

fn is_basic_tty_term(term: Option<&str>) -> bool {
    match term.map(str::trim) {
        None | Some("" | "linux" | "dumb") => true,
        Some(t) => t.starts_with("vt"),
    }
}

fn vim_single_quote(path: &Path) -> String {
    path.to_string_lossy().replace('\'', "''")
}

fn cell_to_style(cell: &vt100::Cell, basic_tty: bool) -> Style {
    let mut style = Style::default()
        .fg(map_vt_color(cell.fgcolor(), basic_tty))
        .bg(map_vt_color(cell.bgcolor(), basic_tty));

    let mut mods = Modifier::empty();
    if cell.bold() {
        mods |= Modifier::BOLD;
    }
    if cell.dim() {
        mods |= Modifier::DIM;
    }
    if cell.italic() {
        mods |= Modifier::ITALIC;
    }
    if cell.underline() {
        mods |= Modifier::UNDERLINED;
    }
    if cell.inverse() {
        mods |= Modifier::REVERSED;
    }
    style = style.add_modifier(mods);
    style
}

fn map_vt_color(c: vt100::Color, basic_tty: bool) -> Color {
    match c {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(i) if i < 16 => ansi16_from_idx(i),
        vt100::Color::Idx(i) if basic_tty => ansi16_from_idx(i % 16),
        vt100::Color::Idx(i) => Color::Indexed(i),
        vt100::Color::Rgb(r, g, b) if basic_tty => quantize_rgb_to_ansi16(r, g, b),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

fn ansi16_from_idx(idx: u8) -> Color {
    match idx {
        0 => Color::Black,
        1 => Color::Red,
        2 => Color::Green,
        3 => Color::Yellow,
        4 => Color::Blue,
        5 => Color::Magenta,
        6 => Color::Cyan,
        7 => Color::Gray,
        8 => Color::DarkGray,
        9 => Color::LightRed,
        10 => Color::LightGreen,
        11 => Color::LightYellow,
        12 => Color::LightBlue,
        13 => Color::LightMagenta,
        14 => Color::LightCyan,
        _ => Color::White,
    }
}

fn quantize_rgb_to_ansi16(r: u8, g: u8, b: u8) -> Color {
    const PALETTE: [(u8, u8, u8); 16] = [
        (0, 0, 0),
        (170, 0, 0),
        (0, 170, 0),
        (170, 85, 0),
        (0, 0, 170),
        (170, 0, 170),
        (0, 170, 170),
        (170, 170, 170),
        (85, 85, 85),
        (255, 85, 85),
        (85, 255, 85),
        (255, 255, 85),
        (85, 85, 255),
        (255, 85, 255),
        (85, 255, 255),
        (255, 255, 255),
    ];
    let mut best_idx = 0u8;
    let mut best_dist = u32::MAX;
    for (idx, &(pr, pg, pb)) in PALETTE.iter().enumerate() {
        let dr = u32::from(r.abs_diff(pr));
        let dg = u32::from(g.abs_diff(pg));
        let db = u32::from(b.abs_diff(pb));
        let dist = dr * dr + dg * dg + db * db;
        if dist < best_dist {
            best_dist = dist;
            best_idx = u8::try_from(idx).unwrap_or(0);
        }
    }
    ansi16_from_idx(best_idx)
}

fn encode_key_event(key: KeyEvent, app_cursor: bool) -> Option<Vec<u8>> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);

    let mut out = Vec::new();
    if alt {
        out.push(0x1b);
    }

    match key.code {
        KeyCode::Char(ch) => {
            if ctrl && let Some(ctrl_byte) = encode_ctrl_char(ch) {
                out.push(ctrl_byte);
                return Some(out);
            }
            let mut buf = [0u8; 4];
            let encoded = ch.encode_utf8(&mut buf);
            out.extend_from_slice(encoded.as_bytes());
            Some(out)
        }
        KeyCode::Enter => {
            out.push(b'\r');
            Some(out)
        }
        KeyCode::Backspace => {
            out.push(0x7f);
            Some(out)
        }
        KeyCode::Tab => {
            out.push(b'\t');
            Some(out)
        }
        KeyCode::BackTab => {
            out.extend_from_slice(b"\x1b[Z");
            Some(out)
        }
        KeyCode::Esc => {
            out.push(0x1b);
            Some(out)
        }
        KeyCode::Up => {
            out.extend_from_slice(if app_cursor { b"\x1bOA" } else { b"\x1b[A" });
            Some(out)
        }
        KeyCode::Down => {
            out.extend_from_slice(if app_cursor { b"\x1bOB" } else { b"\x1b[B" });
            Some(out)
        }
        KeyCode::Right => {
            out.extend_from_slice(if app_cursor { b"\x1bOC" } else { b"\x1b[C" });
            Some(out)
        }
        KeyCode::Left => {
            out.extend_from_slice(if app_cursor { b"\x1bOD" } else { b"\x1b[D" });
            Some(out)
        }
        KeyCode::Home => {
            out.extend_from_slice(b"\x1b[H");
            Some(out)
        }
        KeyCode::End => {
            out.extend_from_slice(b"\x1b[F");
            Some(out)
        }
        KeyCode::PageUp => {
            out.extend_from_slice(b"\x1b[5~");
            Some(out)
        }
        KeyCode::PageDown => {
            out.extend_from_slice(b"\x1b[6~");
            Some(out)
        }
        KeyCode::Insert => {
            out.extend_from_slice(b"\x1b[2~");
            Some(out)
        }
        KeyCode::Delete => {
            out.extend_from_slice(b"\x1b[3~");
            Some(out)
        }
        KeyCode::F(n) => {
            let seq = match n {
                1 => "\x1bOP".to_owned(),
                2 => "\x1bOQ".to_owned(),
                3 => "\x1bOR".to_owned(),
                4 => "\x1bOS".to_owned(),
                5 => "\x1b[15~".to_owned(),
                6 => "\x1b[17~".to_owned(),
                7 => "\x1b[18~".to_owned(),
                8 => "\x1b[19~".to_owned(),
                9 => "\x1b[20~".to_owned(),
                10 => "\x1b[21~".to_owned(),
                11 => "\x1b[23~".to_owned(),
                12 => "\x1b[24~".to_owned(),
                _ => return None,
            };
            out.extend_from_slice(seq.as_bytes());
            Some(out)
        }
        _ => None,
    }
}

fn encode_ctrl_char(ch: char) -> Option<u8> {
    let lower = ch.to_ascii_lowercase();
    match lower {
        'a'..='z' => Some((lower as u8) - b'a' + 1),
        '[' | '3' => Some(0x1b),
        '\\' | '4' => Some(0x1c),
        ']' | '5' => Some(0x1d),
        '^' | '6' => Some(0x1e),
        '_' | '/' | '7' => Some(0x1f),
        ' ' | '@' | '2' => Some(0x00),
        '?' | '8' => Some(0x7f),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn spawns_real_vim_edits_and_detects_colon_w_save() {
        let mut session =
            VimSession::spawn(NoteGuid::from("test-guid-123"), "Initial line\n", 24, 80)
                .expect("should spawn vim in PTY");

        // Wait until Vim renders initial content into the vt100 screen.
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            session.drain_output();
            if session.parser.screen().contents().contains("Initial line") {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        assert!(
            session.parser.screen().contents().contains("Initial line"),
            "Vim screen should display initial markdown"
        );

        // Append a new line in Vim and execute `:w`
        session.write_raw(b"GoSaved from real vim\x1b:w\r");

        let mut saved_text = None;
        let save_deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < save_deadline {
            session.drain_output();
            if let Some(md) = session.poll_saved_markdown() {
                saved_text = Some(md);
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }

        let md = saved_text.expect("`:w` in Vim should trigger BufWritePost save detection");
        assert!(md.contains("Initial line"));
        assert!(md.contains("Saved from real vim"));

        // Quit Vim with `:q`
        session.write_raw(b":q\r");
        let quit_deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < quit_deadline {
            session.drain_output();
            if session.has_exited() {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        assert!(
            session.has_exited(),
            "Vim should exit cleanly on :q; screen was:\n{}",
            session.parser.screen().contents()
        );
    }

    #[test]
    fn tty_mode_detects_linux_term_and_avoids_rgb_or_256_indexed_colors() {
        assert!(is_basic_tty_term(None));
        assert!(is_basic_tty_term(Some("")));
        assert!(is_basic_tty_term(Some("linux")));
        assert!(is_basic_tty_term(Some("vt100")));
        assert!(!is_basic_tty_term(Some("xterm-256color")));
        assert!(!is_basic_tty_term(Some("tmux-256color")));

        for idx in 0..=255u8 {
            let mapped = map_vt_color(vt100::Color::Idx(idx), true);
            assert!(
                !matches!(mapped, Color::Indexed(_) | Color::Rgb(_, _, _)),
                "basic_tty mapped Idx({idx}) to {mapped:?}"
            );
        }
        let rgb_mapped = map_vt_color(vt100::Color::Rgb(220, 40, 40), true);
        assert!(
            !matches!(rgb_mapped, Color::Indexed(_) | Color::Rgb(_, _, _)),
            "basic_tty mapped Rgb to {rgb_mapped:?}"
        );
    }
}
