# evertui

[![Production code](docs/badges/production-code.svg)](src)
[![Test code](docs/badges/test-code.svg)](tests)

Two-panel Evernote TUI written in Rust with embedded real `vim` in the right panel.
Uses pure ASCII chrome and 16-color ANSI styles so it works cleanly in Linux virtual consoles (`/dev/tty*`, `TERM=linux`) as well as modern terminal emulators.

## Features

- **Two-panel layout**:
  - **Left panel**: Up to 20 note titles (`j`/`k`, mouse click, scroll wheel).
  - **Right panel**: Note metadata header (Title, Notebook, Tags, Created & Updated dates, size, attachment counts) + **fast native text preview** by default, replacing the preview with **real embedded `vim`** on `Enter` (`:w` saves directly to Evernote with an ASCII animated indicator).
- **Preferences (`p` / `F7` / `Alt+p`)**:
  - `[ ] Vim preview` (unchecked by default for instant browsing; check to preview every selected note directly in embedded `vim`, saved in `~/.config/evertui/preferences.conf`).
- **RAM-only note editing (no `/tmp` files)**: Embedded `vim` buffers and `:w` save notifications use anonymous Linux kernel memory (`memfd_create`) with swap, backup, undo, and `viminfo` disabled, so note content never touches `/tmp` or disk.
- **Placeholders for media & attachments**: Images render as `[Image: filename.ext]` and files as `[Attachment: filename.ext]`, round-tripping safely back to `<en-media>` on `:w`.
- **TTY-friendly pure ASCII UI**: Simple ASCII borders, spinners (`| / - \`), progress bar (`[==  ]`), and standard 16 ANSI colors (no emojis or wide Unicode symbols).
- **Flexible search (`/`, `F3`, `Alt+s`)**:
  - `tag:something`, `notebook:something`
  - Aliases: `t:something`, `n:something`, `t something`, `n something`, `-t:something`, and quoted values (`t:"two words"`, `n "My Notebook"`).
- **Tag & Notebook management**:
  - `t` (`F4` / `Alt+t` / click `[Tags: ...]`) to toggle existing tags or create new tags on a note.
  - `n` (`F5` / `Alt+n` / click `[Notebook: ...]`) to move a note to another notebook.
- **Resizable & collapsible panels**:
  - Drag the vertical `|` separator with the mouse to resize panels (resizing `vim`'s PTY live).
  - Press `b` / `F2` / `Alt+b` (or click `<< Hide` / `>> Titles`) to hide or show the left panel.
- **Developer token auth**: Reads `--token`, `EVERTUI_DEV_TOKEN`, `EVERNOTE_DEV_TOKEN`, `~/.config/evertui/token`, or falls back to `~/.reeknote/reeknote.db`.

## Gentoo install

```sh
eselect repository enable vitaly-zdanevich-overlay
eselect sync --repo vitaly-zdanevich-overlay
emerge -a evertui
```

## Development

Enable the local Git `pre-commit` hook once per clone so `docs/badges/production-code.svg` and `docs/badges/test-code.svg` regenerate automatically from staged files on every commit:

```sh
python3 tools/install_git_hooks.py
```
