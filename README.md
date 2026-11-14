# evertui

[![Production code](docs/badges/production-code.svg)](src)
[![Test code](docs/badges/test-code.svg)](tests)

Two-panel Evernote TUI written in Rust with fast plain-text preview and embedded real `vim` in the right panel.
All note content stays strictly in RAM for performance and security (never written to `/tmp` or disk).
Uses pure ASCII chrome and 16-color ANSI styles so it works cleanly in Linux virtual consoles (`/dev/tty*`, `TERM=linux`) as well as modern terminal emulators.

## Features

- **Two-panel layout**:
  - **Left panel**: Up to 20 note titles (`j`/`k`, `1`-`9`, mouse click, scroll wheel).
  - **Right panel**: Note metadata header (Title, Notebook, Tags, Created & Updated dates, size, attachment counts, full source URL) + **fast native text preview** by default, replacing the preview with **real embedded `vim`** on `Enter` (`:w` saves directly to Evernote with an ASCII animated indicator).
- **Unsaved edits are never dropped**: while the embedded `vim` buffer has changes not yet saved with `:w`, switching notes, searching, creating a note, and quitting are refused with `Unsaved in Vim: :w save, :q! discard` in the status bar.
- **Preferences (`p` / `F7` / `Alt+p`)**:
  - `[ ] Vim preview` (unchecked by default for instant browsing; check to preview every selected note directly in embedded `vim`, saved in `~/.config/evertui/preferences.conf`).
- **Note content strictly in RAM (performance & security)**: Note previews, embedded `vim` buffers, and `:w` save notifications live exclusively in memory (using anonymous Linux kernel `memfd_create` descriptors with swap, backup, undo, and `viminfo` disabled, plus `TMPDIR` pointed at RAM `tmpfs`), so note text never touches `/tmp` or disk.
- **Placeholders for media & attachments**: Images render as `[Image: filename.ext]` and files as `[Attachment: filename.ext]`, round-tripping safely back to `<en-media>` on `:w`.
- **TTY-friendly borderless pure ASCII UI**: Zero borders for maximum screen space and minimal visual noise, with ASCII spinners (`| / - \`), progress bar (`[==  ]`), and standard 16 ANSI colors (no emojis or wide Unicode symbols).
- **Flexible search (`/`, `F3`, `Alt+s`)**:
  - `tag:something`, `notebook:something`
  - Aliases: `t:something`, `n:something`, `t something`, `n something`, `-t:something`, and quoted values (`t:"two words"`, `n "My Notebook"`).
  - Bash-style editing: `Alt+Left` / `Alt+Right` move by word, `Ctrl+w` deletes the word before the cursor, `Ctrl+a` / `Ctrl+e` to start / end, `Ctrl+u` clears.
- **Tag & Notebook management**:
  - `t` (`F4` / `Alt+t` / click `[Tags: ...]`) to toggle existing tags or create new tags on a note.
  - `n` (`F5` / `Alt+n` / click `[Notebook: ...]`) to move a note to another notebook.
  - `u` (`F8` / `Alt+u` / click the URL) to set the note source URL. It stays on one line and is not cut short with `...`. An empty prompt clears it.
  - `x` (`F9` / `Alt+x` / click `[ENML (x)]`) to see the note's raw ENML, the XHTML Evernote stores, as received or as last saved with `:w`.
- **Resizable & collapsible panels**:
  - Drag the seam between panels with the mouse to resize panels (resizing `vim`'s PTY live).
  - Press `b` / `F2` / `Alt+b` (or click `<< Hide` / `>> Titles`) to hide or show the left panel.
- **Developer token auth**: Reads `--token`, `EVERTUI_DEV_TOKEN`, `EVERNOTE_DEV_TOKEN`, `~/.config/evertui/token`, or falls back to `~/.reeknote/reeknote.db`.

## Gentoo install

```sh
eselect repository enable vitaly-zdanevich-overlay
eselect sync --repo vitaly-zdanevich-overlay
emerge -a evertui
```

## Build from source

Requirements: Linux, Rust 1.88 or newer (edition 2024), and `vim` on `PATH` (the right panel runs it).

```sh
git clone https://github.com/vitaly-zdanevich/evertui
cd evertui
cargo build --locked --release
```

The binary is `target/release/evertui`. To install it into `~/.cargo/bin`:

```sh
cargo install --locked --path .
```

## Run from source

Put your Evernote developer token in one of the places listed under **Developer token auth** above, for example:

```sh
export EVERTUI_DEV_TOKEN='S=s1:U=...'
```

Then run straight from the checkout. Arguments after `--` go to `evertui`; any words that are not options become the startup search:

```sh
cargo run --release                     # latest notes
cargo run --release -- t git            # start with a search for tag "git"
cargo run --release -- --token '...'    # pass the token directly
cargo run --release -- --help
```

Run the tests with `cargo test` (they need `vim` installed).

## Development

Enable the local Git `pre-commit` hook once per clone so `docs/badges/production-code.svg` and `docs/badges/test-code.svg` regenerate automatically from staged files on every commit:

```sh
python3 tools/install_git_hooks.py
```
