use std::env;
use std::fmt;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use thiserror::Error;

pub const DEFAULT_EVERNOTE_BASE: &str = "www.evernote.com";
pub const YINXIANG_BASE: &str = "app.yinxiang.com";
pub const DEVELOPER_TOKEN_URL: &str = "https://www.evernote.com/api/DeveloperToken.action";

#[derive(Debug, Error)]
pub enum AuthError {
    #[error(
        "No Evernote developer token found.\n\
         Set EVERNOTE_DEV_TOKEN (or EVERTUI_DEV_TOKEN), place your token in ~/.config/evertui/token,\n\
         or obtain one from {DEVELOPER_TOKEN_URL}"
    )]
    MissingToken,
    #[error("I/O error while reading credentials: {0}")]
    Io(#[from] io::Error),
}

/// Redacted authentication configuration so the secret token is never printed in Debug output.
#[derive(Clone, PartialEq, Eq)]
pub struct AuthConfig {
    token: String,
    pub user_store_url: String,
}

impl fmt::Debug for AuthConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthConfig")
            .field("token", &"<redacted>")
            .field("user_store_url", &self.user_store_url)
            .finish()
    }
}

impl AuthConfig {
    #[must_use]
    pub fn new(token: String, user_store_url: String) -> Self {
        Self {
            token,
            user_store_url,
        }
    }

    #[must_use]
    pub fn token(&self) -> &str {
        &self.token
    }

    pub fn resolve(cli_token: Option<String>) -> Result<Self, AuthError> {
        let home = home_dir();
        let user_store_url = resolve_user_store_url(&home);

        if let Some(token) = cli_token
            .map(|t| t.trim().to_owned())
            .filter(|t| !t.is_empty())
        {
            return Ok(Self::new(token, user_store_url));
        }

        for env_key in ["EVERTUI_DEV_TOKEN", "EVERNOTE_DEV_TOKEN"] {
            if let Ok(val) = env::var(env_key) {
                let trimmed = val.trim();
                if !trimmed.is_empty() {
                    return Ok(Self::new(trimmed.to_owned(), user_store_url));
                }
            }
        }

        let config_token_path = config_dir(&home).join("token");
        if let Some(token) = read_trimmed_file(&config_token_path)? {
            return Ok(Self::new(token, user_store_url));
        }

        let reeknote_db = reeknote_dir(&home).join("reeknote.db");
        if let Some(token) = read_reeknote_token(&reeknote_db) {
            return Ok(Self::new(token, user_store_url));
        }

        if io::stdin().is_terminal()
            && io::stdout().is_terminal()
            && let Some(token) = prompt_and_save_token(&config_token_path)?
        {
            return Ok(Self::new(token, user_store_url));
        }

        Err(AuthError::MissingToken)
    }
}

fn home_dir() -> PathBuf {
    env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn config_dir(home: &Path) -> PathBuf {
    env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"))
        .join("evertui")
}

fn reeknote_dir(home: &Path) -> PathBuf {
    env::var_os("REEKNOTE_APP_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".reeknote"))
}

fn resolve_user_store_url(home: &Path) -> String {
    if let Ok(explicit) = env::var("EVERTUI_USER_STORE_URL") {
        let trimmed = explicit.trim();
        if !trimmed.is_empty() {
            return trimmed.to_owned();
        }
    }
    let is_yinxiang = env::var("REEKNOTE_BASE").ok().as_deref() == Some("yinxiang")
        || reeknote_dir(home).join("isyinxiang").is_file();
    let host = if is_yinxiang {
        YINXIANG_BASE
    } else {
        DEFAULT_EVERNOTE_BASE
    };
    format!("https://{host}/edam/user")
}

fn read_trimmed_file(path: &Path) -> io::Result<Option<String>> {
    if !path.is_file() {
        return Ok(None);
    }
    let content = fs::read_to_string(path)?;
    let trimmed = content.trim();
    if trimmed.is_empty() {
        Ok(None)
    } else {
        Ok(Some(trimmed.to_owned()))
    }
}

/// Reads a saved developer/OAuth token from `~/.reeknote/reeknote.db` if present.
fn read_reeknote_token(db_path: &Path) -> Option<String> {
    let content = fs::read_to_string(db_path).ok()?;
    parse_reeknote_token(&content)
}

fn parse_reeknote_token(content: &str) -> Option<String> {
    for line in content.lines() {
        let mut parts = line.split('\t');
        let kind = parts.next()?;
        let key = parts.next()?;
        let val = parts.next()?;
        if kind == "user.string" && percent_decode(key).as_deref() == Some("oAuthToken") {
            let decoded = percent_decode(val)?;
            if !decoded.trim().is_empty() {
                return Some(decoded);
            }
        }
    }
    None
}

fn percent_decode(input: &str) -> Option<String> {
    let mut bytes = Vec::with_capacity(input.len());
    let mut iter = input.as_bytes().iter().copied();
    while let Some(b) = iter.next() {
        if b == b'%' {
            let hi = iter.next()?;
            let lo = iter.next()?;
            let pair = [hi, lo];
            let hex_str = std::str::from_utf8(&pair).ok()?;
            let val = u8::from_str_radix(hex_str, 16).ok()?;
            bytes.push(val);
        } else {
            bytes.push(b);
        }
    }
    String::from_utf8(bytes).ok()
}

fn prompt_and_save_token(token_path: &Path) -> io::Result<Option<String>> {
    let mut stdout = io::stdout();
    writeln!(
        stdout,
        "Enter your Evernote developer token ({DEVELOPER_TOKEN_URL}):"
    )?;
    write!(stdout, "> ")?;
    stdout.flush()?;

    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    let token = line.trim().to_owned();
    if token.is_empty() {
        return Ok(None);
    }

    if let Some(parent) = token_path.parent() {
        fs::create_dir_all(parent)?;
    }
    write_private_token_file(token_path, &token)?;
    Ok(Some(token))
}

#[cfg(unix)]
fn write_private_token_file(path: &Path, token: &str) -> io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(token.as_bytes())?;
    file.write_all(b"\n")?;
    file.flush()
}

#[cfg(not(unix))]
fn write_private_token_file(path: &Path, token: &str) -> io::Result<()> {
    fs::write(path, format!("{token}\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_percent_encoded_reeknote_token() {
        let sample =
            "setting\teditor\tvim\nuser.string\toAuthToken\tS%3Ds8%3AU%3D42%3AE%3Ddevtoken_123\n";
        assert_eq!(
            parse_reeknote_token(sample).as_deref(),
            Some("S=s8:U=42:E=devtoken_123")
        );
    }

    #[test]
    fn debug_redacts_secret_token() {
        let cfg = AuthConfig::new(
            "S=s8:SECRET_VALUE".to_owned(),
            "https://www.evernote.com/edam/user".to_owned(),
        );
        let formatted = format!("{cfg:?}");
        assert!(!formatted.contains("SECRET_VALUE"));
        assert!(formatted.contains("<redacted>"));
    }
}
