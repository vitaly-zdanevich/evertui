use crate::domain::{Notebook, NotebookGuid};

/// A tag filter parsed from a search query.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TagFilter {
    pub name: String,
    pub negated: bool,
}

/// Structured representation of a user's search input.
///
/// Supports:
/// - `tag:something`, `-tag:something`
/// - `notebook:something`
/// - `t:something`, `-t:something`
/// - `n:something`
/// - `t something`
/// - `n something`
/// - Quoted values such as `t:"two words"`, `t "two words"`, `n:"My Notebook"`, `n "My Notebook"`
/// - Arbitrary additional search words / phrases
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ParsedSearch {
    pub notebook: Option<String>,
    pub tags: Vec<TagFilter>,
    pub terms: Vec<TokenTerm>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TokenTerm {
    pub text: String,
    pub quoted: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompiledEvernoteFilter {
    pub notebook_guid: Option<NotebookGuid>,
    pub words: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RawToken {
    value: String,
    quoted: bool,
}

impl ParsedSearch {
    #[must_use]
    pub fn parse(input: &str) -> Self {
        let tokens = tokenize(input);
        let mut parsed = Self::default();
        let mut idx = 0;

        while idx < tokens.len() {
            let tok = &tokens[idx];

            if !tok.quoted {
                // Handle space-separated aliases: "t something" and "n something"
                // (also "-t something" for negated tag).
                if matches_ci(&tok.value, "t") && idx + 1 < tokens.len() {
                    let next = &tokens[idx + 1];
                    if !next.value.is_empty() {
                        parsed.tags.push(TagFilter {
                            name: next.value.clone(),
                            negated: false,
                        });
                        idx += 2;
                        continue;
                    }
                }
                if matches_ci(&tok.value, "-t") && idx + 1 < tokens.len() {
                    let next = &tokens[idx + 1];
                    if !next.value.is_empty() {
                        parsed.tags.push(TagFilter {
                            name: next.value.clone(),
                            negated: true,
                        });
                        idx += 2;
                        continue;
                    }
                }
                if matches_ci(&tok.value, "n") && idx + 1 < tokens.len() {
                    let next = &tokens[idx + 1];
                    if !next.value.is_empty() {
                        parsed.notebook = Some(next.value.clone());
                        idx += 2;
                        continue;
                    }
                }

                // Handle colon-prefixed forms: tag:..., t:..., -tag:..., -t:..., notebook:..., n:...
                // Also handle accidental space right after colon like `tag: foo` or `n: foo`.
                if let Some((prefix, rest)) = tok.value.split_once(':') {
                    if let Some(negated) = classify_tag_prefix(prefix) {
                        if !rest.is_empty() {
                            parsed.tags.push(TagFilter {
                                name: rest.to_owned(),
                                negated,
                            });
                            idx += 1;
                            continue;
                        } else if idx + 1 < tokens.len() && !tokens[idx + 1].value.is_empty() {
                            parsed.tags.push(TagFilter {
                                name: tokens[idx + 1].value.clone(),
                                negated,
                            });
                            idx += 2;
                            continue;
                        }
                    } else if is_notebook_prefix(prefix) {
                        if !rest.is_empty() {
                            parsed.notebook = Some(rest.to_owned());
                            idx += 1;
                            continue;
                        } else if idx + 1 < tokens.len() && !tokens[idx + 1].value.is_empty() {
                            parsed.notebook = Some(tokens[idx + 1].value.clone());
                            idx += 2;
                            continue;
                        }
                    }
                }
            }

            parsed.terms.push(TokenTerm {
                text: tok.value.clone(),
                quoted: tok.quoted,
            });
            idx += 1;
        }

        parsed
    }

    /// Compiles into an Evernote `NoteFilter` specification (`notebook_guid` + `words`).
    ///
    /// Evernote's search grammar requires `notebook:...` to appear first in `words`
    /// if not passed via `NoteFilter.notebookGuid`. If the notebook matches a known
    /// notebook (exact or case-insensitive), we resolve its `NotebookGuid` directly.
    #[must_use]
    pub fn compile(&self, known_notebooks: &[Notebook]) -> CompiledEvernoteFilter {
        let mut parts: Vec<String> = Vec::new();
        let mut resolved_notebook_guid = None;

        if let Some(nb_query) = &self.notebook {
            if let Some(nb) = resolve_notebook(nb_query, known_notebooks) {
                resolved_notebook_guid = Some(nb.guid.clone());
            } else {
                parts.push(format_field_expr("notebook", nb_query, false));
            }
        }

        for tag in &self.tags {
            parts.push(format_field_expr("tag", &tag.name, tag.negated));
        }

        for term in &self.terms {
            if term.quoted || term.text.contains(char::is_whitespace) {
                parts.push(format!("\"{}\"", escape_quotes(&term.text)));
            } else {
                parts.push(term.text.clone());
            }
        }

        let words = if parts.is_empty() {
            None
        } else {
            Some(parts.join(" "))
        };

        CompiledEvernoteFilter {
            notebook_guid: resolved_notebook_guid,
            words,
        }
    }
}

fn resolve_notebook<'a>(query: &str, notebooks: &'a [Notebook]) -> Option<&'a Notebook> {
    if let Some(exact) = notebooks.iter().find(|nb| nb.name == query) {
        return Some(exact);
    }
    let lower = query.to_lowercase();
    if let Some(ci) = notebooks.iter().find(|nb| nb.name.to_lowercase() == lower) {
        return Some(ci);
    }
    let partial: Vec<&Notebook> = notebooks
        .iter()
        .filter(|nb| nb.name.to_lowercase().contains(&lower))
        .collect();
    if partial.len() == 1 {
        Some(partial[0])
    } else {
        None
    }
}

fn matches_ci(value: &str, expected_ascii_lower: &str) -> bool {
    value.eq_ignore_ascii_case(expected_ascii_lower)
}

fn classify_tag_prefix(prefix: &str) -> Option<bool> {
    if prefix.eq_ignore_ascii_case("tag") || prefix.eq_ignore_ascii_case("t") {
        Some(false)
    } else if prefix.eq_ignore_ascii_case("-tag") || prefix.eq_ignore_ascii_case("-t") {
        Some(true)
    } else {
        None
    }
}

fn is_notebook_prefix(prefix: &str) -> bool {
    prefix.eq_ignore_ascii_case("notebook") || prefix.eq_ignore_ascii_case("n")
}

fn format_field_expr(field: &str, value: &str, negated: bool) -> String {
    let sign = if negated { "-" } else { "" };
    if value.contains(char::is_whitespace) {
        format!("{sign}{field}:\"{}\"", escape_quotes(value))
    } else {
        format!("{sign}{field}:{value}")
    }
}

fn escape_quotes(text: &str) -> String {
    text.replace('"', "\\\"")
}

fn tokenize(input: &str) -> Vec<RawToken> {
    let mut tokens = Vec::new();
    let mut chars = input.chars().peekable();

    while let Some(&ch) = chars.peek() {
        if ch.is_whitespace() {
            chars.next();
            continue;
        }

        if ch == '"' {
            chars.next();
            let mut buf = String::new();
            while let Some(c) = chars.next() {
                if c == '\\' {
                    if let Some(escaped) = chars.next() {
                        buf.push(escaped);
                    }
                } else if c == '"' {
                    break;
                } else {
                    buf.push(c);
                }
            }
            tokens.push(RawToken {
                value: buf,
                quoted: true,
            });
            continue;
        }

        let mut buf = String::new();
        let mut saw_quote_after_colon = false;
        while let Some(&c) = chars.peek() {
            if c.is_whitespace() {
                break;
            }
            if c == '"' && buf.ends_with(':') {
                // Support `tag:"two words"`, `t:"two words"`, `notebook:"My Notes"`, `n:"My Notes"`
                saw_quote_after_colon = true;
                chars.next();
                while let Some(qc) = chars.next() {
                    if qc == '\\' {
                        if let Some(escaped) = chars.next() {
                            buf.push(escaped);
                        }
                    } else if qc == '"' {
                        break;
                    } else {
                        buf.push(qc);
                    }
                }
                break;
            }
            buf.push(c);
            chars.next();
        }
        let _ = saw_quote_after_colon;
        tokens.push(RawToken {
            value: buf,
            quoted: false,
        });
    }

    tokens
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_notebooks() -> Vec<Notebook> {
        vec![
            Notebook {
                guid: NotebookGuid::from("nb-inbox"),
                name: "INBOX".to_owned(),
                stack: None,
                is_default: true,
            },
            Notebook {
                guid: NotebookGuid::from("nb-work"),
                name: "Work Projects".to_owned(),
                stack: Some("Work".to_owned()),
                is_default: false,
            },
        ]
    }

    #[test]
    fn parses_full_tag_and_notebook_syntax() {
        let parsed = ParsedSearch::parse("tag:Evernote notebook:INBOX rust");
        assert_eq!(parsed.notebook.as_deref(), Some("INBOX"));
        assert_eq!(
            parsed.tags,
            vec![TagFilter {
                name: "Evernote".to_owned(),
                negated: false,
            }]
        );
        assert_eq!(parsed.terms.len(), 1);
        assert_eq!(parsed.terms[0].text, "rust");
    }

    #[test]
    fn parses_colon_aliases_t_and_n() {
        let parsed = ParsedSearch::parse("t:tui n:INBOX -t:old");
        assert_eq!(parsed.notebook.as_deref(), Some("INBOX"));
        assert_eq!(
            parsed.tags,
            vec![
                TagFilter {
                    name: "tui".to_owned(),
                    negated: false,
                },
                TagFilter {
                    name: "old".to_owned(),
                    negated: true,
                },
            ]
        );
    }

    #[test]
    fn parses_space_aliases_t_and_n() {
        let parsed = ParsedSearch::parse("t tui n INBOX build");
        assert_eq!(parsed.notebook.as_deref(), Some("INBOX"));
        assert_eq!(
            parsed.tags,
            vec![TagFilter {
                name: "tui".to_owned(),
                negated: false,
            }]
        );
        assert_eq!(parsed.terms.len(), 1);
        assert_eq!(parsed.terms[0].text, "build");
    }

    #[test]
    fn parses_quoted_multiword_tags_and_notebooks() {
        let p1 = ParsedSearch::parse("t:\"two words\" n:\"Work Projects\" \"exact phrase\"");
        assert_eq!(p1.notebook.as_deref(), Some("Work Projects"));
        assert_eq!(p1.tags[0].name, "two words");
        assert_eq!(p1.terms[0].text, "exact phrase");
        assert!(p1.terms[0].quoted);

        let p2 = ParsedSearch::parse("t \"two words\" n \"Work Projects\"");
        assert_eq!(p2.notebook.as_deref(), Some("Work Projects"));
        assert_eq!(p2.tags[0].name, "two words");
    }

    #[test]
    fn compiles_resolving_known_notebook_and_ordering_unknown_notebook_first() {
        let nbs = sample_notebooks();

        let known = ParsedSearch::parse("t tui n inbox hello").compile(&nbs);
        assert_eq!(known.notebook_guid, Some(NotebookGuid::from("nb-inbox")));
        assert_eq!(known.words.as_deref(), Some("tag:tui hello"));

        let unknown = ParsedSearch::parse("t:tui n:Archive hello").compile(&nbs);
        assert_eq!(unknown.notebook_guid, None);
        assert_eq!(
            unknown.words.as_deref(),
            Some("notebook:Archive tag:tui hello")
        );
    }
}
