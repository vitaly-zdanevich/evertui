use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Preferences {
    /// When false (default), selecting a note renders a fast native text preview
    /// and pressing Enter launches embedded Vim for editing.
    /// When true, selecting a note immediately previews it inside embedded Vim.
    pub vim_preview: bool,
}

impl Preferences {
    #[must_use]
    pub fn default_path() -> PathBuf {
        let home = env::var_os("HOME")
            .or_else(|| env::var_os("USERPROFILE"))
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"))
            .join("evertui")
            .join("preferences.conf")
    }

    #[must_use]
    pub fn load_or_default(path: &Path) -> Self {
        match fs::read_to_string(path) {
            Ok(text) => Self::parse(&text),
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, self.serialize())
    }

    #[must_use]
    pub fn parse(input: &str) -> Self {
        let mut prefs = Self::default();
        for raw_line in input.lines() {
            let line = raw_line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((key, value)) = line.split_once('=') {
                let k = key.trim();
                let v = value.trim();
                if k == "vim_preview" {
                    prefs.vim_preview =
                        matches!(v.to_ascii_lowercase().as_str(), "true" | "1" | "yes" | "on");
                }
            }
        }
        prefs
    }

    #[must_use]
    pub fn serialize(&self) -> String {
        format!(
            "# evertui preferences\nvim_preview = {}\n",
            self.vim_preview
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_fast_text_preview_not_vim_preview() {
        let prefs = Preferences::default();
        assert!(!prefs.vim_preview);
    }

    #[test]
    fn round_trips_preferences_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("evertui").join("preferences.conf");

        let missing = Preferences::load_or_default(&path);
        assert!(!missing.vim_preview);

        let updated = Preferences { vim_preview: true };
        updated.save(&path).unwrap();

        let reloaded = Preferences::load_or_default(&path);
        assert!(reloaded.vim_preview);
    }

    #[test]
    fn ignores_comments_and_unknown_keys() {
        let parsed = Preferences::parse("# comment\nunknown_flag = 123\nvim_preview = yes\n");
        assert!(parsed.vim_preview);
    }
}
