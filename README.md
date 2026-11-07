# evertui

Two-panel Evernote TUI written in Rust with embedded real `vim` in the right panel.
Uses pure ASCII chrome and 16-color ANSI styles so it works cleanly in Linux virtual consoles (`/dev/tty*`, `TERM=linux`) as well as modern terminal emulators.

## Features

- **Two-panel layout**:
  - **Left panel**: Up to 20 note titles (`j`/`k`, mouse click, scroll wheel).
  - **Right panel**: Note metadata header (Title, Notebook, Tags, Created & Updated dates, size, attachment counts) + **real embedded `vim`** running in a PTY (`:w` saves directly to Evernote with an ASCII animated indicator).
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
