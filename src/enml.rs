use crate::domain::ResourceMeta;
use std::collections::HashMap;

const EXPLICIT_BLANK_MARKER: &str = "\0";
const MEDIA_SENTINEL_PREFIX: &str = "\x1eEVERTUI_MEDIA_";
const MEDIA_SENTINEL_SUFFIX: &str = "\x1f";

/// Maps human-readable placeholders (e.g. `[Image: photo.png]`, `[Attachment: doc.pdf]`)
/// back to their exact `<en-media .../>` elements when saving edits from Vim.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PreparedNoteDocument {
    pub markdown: String,
    placeholders: Vec<(String, String)>,
}

impl PreparedNoteDocument {
    #[must_use]
    pub fn from_enml(enml: &str, resources: &[ResourceMeta]) -> Self {
        let mut body = extract_en_note_body(enml).unwrap_or(enml).to_owned();
        let mut placeholders: Vec<(String, String)> = Vec::new();
        let mut used_labels: HashMap<String, String> = HashMap::new();

        body = replace_en_media(&body, resources, &mut placeholders, &mut used_labels);
        body = replace_code_blocks(&body);
        body = replace_inline_code(&body);
        body = replace_blockquotes(&body);
        body = replace_headings(&body);
        body = replace_bold(&body);
        body = replace_italic(&body);
        body = replace_links(&body);
        body = replace_todos(&body);
        body = replace_paragraphs(&body);
        body = replace_divs(&body);
        body = body
            .replace("<br/>", "\n")
            .replace("<br />", "\n")
            .replace("<br>", "\n");
        body = body
            .replace("<hr/>", "\n---\n\n")
            .replace("<hr />", "\n---\n\n")
            .replace("<hr>", "\n---\n\n");
        body = strip_html_tags(&body);
        let unescaped = xml_unescape(&body);
        let mut markdown = normalize_blank_lines(&unescaped);

        for (idx, (label, _)) in placeholders.iter().enumerate() {
            let sentinel = format!("{MEDIA_SENTINEL_PREFIX}{idx}{MEDIA_SENTINEL_SUFFIX}");
            markdown = markdown.replace(&sentinel, label);
        }

        Self {
            markdown,
            placeholders,
        }
    }

    #[must_use]
    pub fn to_enml(&self, edited_markdown: &str) -> String {
        let html = markdown_to_enml_inner(edited_markdown, &self.placeholders);
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <!DOCTYPE en-note SYSTEM \"http://xml.evernote.com/pub/enml2.dtd\">\n\
             <en-note>{html}</en-note>"
        )
    }
}

#[must_use]
pub fn empty_enml() -> String {
    "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
     <!DOCTYPE en-note SYSTEM \"http://xml.evernote.com/pub/enml2.dtd\">\n\
     <en-note><div><br/></div></en-note>"
        .to_owned()
}

fn extract_en_note_body(enml: &str) -> Option<&str> {
    let open_idx = enml.find("<en-note")?;
    let open_end = enml[open_idx..].find('>')? + open_idx + 1;
    let close_idx = enml.rfind("</en-note>")?;
    (open_end <= close_idx).then_some(&enml[open_end..close_idx])
}

fn replace_en_media(
    content: &str,
    resources: &[ResourceMeta],
    placeholders: &mut Vec<(String, String)>,
    used_labels: &mut HashMap<String, String>,
) -> String {
    let mut out = String::new();
    let mut rest = content;

    while let Some(idx) = rest.find("<en-media") {
        out.push_str(&rest[..idx]);
        let after_start = &rest[idx + "<en-media".len()..];
        let Some(tag_end) = after_start.find('>') else {
            out.push_str(&rest[idx..]);
            return out;
        };
        let raw_attrs = &after_start[..tag_end];
        let mut after_gt = &after_start[tag_end + 1..];
        if let Some(stripped) = after_gt.strip_prefix("</en-media>") {
            after_gt = stripped;
        }
        rest = after_gt;

        let mime = attr_value(raw_attrs, "type");
        let hash = attr_value(raw_attrs, "hash");

        let canonical_tag = match (&mime, &hash) {
            (Some(m), Some(h)) => {
                format!(
                    "<en-media type=\"{}\" hash=\"{}\"/>",
                    xml_escape_attr(m),
                    xml_escape_attr(h)
                )
            }
            _ => {
                let trimmed = raw_attrs.trim().trim_end_matches('/').trim();
                format!("<en-media {trimmed}/>")
            }
        };

        let base_label = build_placeholder_label(mime.as_deref(), hash.as_deref(), resources);
        let label = disambiguate_label(&base_label, &canonical_tag, used_labels);

        let sentinel_index = if let Some(existing) = placeholders
            .iter()
            .position(|(existing_label, _)| existing_label == &label)
        {
            existing
        } else {
            let i = placeholders.len();
            placeholders.push((label, canonical_tag));
            i
        };

        out.push_str(&format!(
            "{MEDIA_SENTINEL_PREFIX}{sentinel_index}{MEDIA_SENTINEL_SUFFIX}\n\n"
        ));
    }

    out.push_str(rest);
    out
}

fn disambiguate_label(
    base_label: &str,
    canonical_tag: &str,
    used_labels: &mut HashMap<String, String>,
) -> String {
    match used_labels.get(base_label) {
        None => {
            used_labels.insert(base_label.to_owned(), canonical_tag.to_owned());
            base_label.to_owned()
        }
        Some(existing_tag) if existing_tag == canonical_tag => base_label.to_owned(),
        Some(_) => {
            let inner = base_label
                .strip_prefix('[')
                .and_then(|s| s.strip_suffix(']'))
                .unwrap_or(base_label);
            for n in 2usize.. {
                let candidate = format!("[{inner} #{n}]");
                match used_labels.get(&candidate) {
                    None => {
                        used_labels.insert(candidate.clone(), canonical_tag.to_owned());
                        return candidate;
                    }
                    Some(tag) if tag == canonical_tag => return candidate,
                    Some(_) => {}
                }
            }
            unreachable!()
        }
    }
}

fn build_placeholder_label(
    mime: Option<&str>,
    hash: Option<&str>,
    resources: &[ResourceMeta],
) -> String {
    let matched_resource = hash.and_then(|h| {
        resources
            .iter()
            .find(|r| r.body_hash_hex.eq_ignore_ascii_case(h))
    });
    let effective_mime = mime.or_else(|| matched_resource.and_then(|r| r.mime.as_deref()));
    let is_image = effective_mime.is_some_and(|m| m.to_ascii_lowercase().starts_with("image/"));
    let kind = if is_image { "Image" } else { "Attachment" };

    let filename = matched_resource
        .and_then(|r| {
            let clean = sanitize_filename(&r.filename);
            (!clean.is_empty()).then_some(clean)
        })
        .unwrap_or_else(|| fallback_media_filename(is_image, effective_mime, hash));

    format!("[{kind}: {filename}]")
}

fn sanitize_filename(name: &str) -> String {
    name.trim()
        .chars()
        .filter(|c| !c.is_control() && *c != '[' && *c != ']')
        .collect::<String>()
}

fn fallback_media_filename(is_image: bool, mime: Option<&str>, hash: Option<&str>) -> String {
    let stem = if is_image { "image" } else { "attachment" };
    let short_hash = hash.map(|h| &h[..h.len().min(8)]);
    let ext = mime.and_then(extension_from_mime);
    match (short_hash, ext) {
        (Some(h), Some(e)) => format!("{stem}-{h}.{e}"),
        (Some(h), None) => format!("{stem}-{h}"),
        (None, Some(e)) => format!("{stem}.{e}"),
        (None, None) => stem.to_owned(),
    }
}

fn extension_from_mime(mime: &str) -> Option<&str> {
    match mime.to_ascii_lowercase().as_str() {
        "image/jpeg" => Some("jpg"),
        "image/png" => Some("png"),
        "image/gif" => Some("gif"),
        "image/webp" => Some("webp"),
        "image/svg+xml" => Some("svg"),
        "application/pdf" => Some("pdf"),
        "audio/mpeg" => Some("mp3"),
        "audio/ogg" => Some("ogg"),
        "text/plain" => Some("txt"),
        _ => mime.rsplit_once('/').map(|(_, sub)| sub),
    }
}

fn attr_value(attrs: &str, key: &str) -> Option<String> {
    for quote in ['"', '\''] {
        let needle = format!("{key}={quote}");
        if let Some(pos) = attrs.find(&needle) {
            let start = pos + needle.len();
            let end = attrs[start..].find(quote)? + start;
            return Some(attrs[start..end].to_owned());
        }
    }
    None
}

fn replace_code_blocks(content: &str) -> String {
    let after_en_code = replace_tag_blocks(content, "div", is_en_codeblock, |inner| {
        let code = extract_plain_code(inner);
        let trimmed = code.trim_matches('\n');
        if trimmed.trim().is_empty() {
            String::new()
        } else {
            format!("```\n{}\n```\n\n", xml_escape_tags_only(trimmed))
        }
    });
    replace_tag_blocks(
        &after_en_code,
        "pre",
        |_| true,
        |inner| {
            let code = extract_plain_code(inner);
            let trimmed = code.trim_matches('\n');
            if trimmed.trim().is_empty() {
                String::new()
            } else {
                format!("```\n{}\n```\n\n", xml_escape_tags_only(trimmed))
            }
        },
    )
}

fn is_en_codeblock(open_tag: &str) -> bool {
    let lower = open_tag.to_ascii_lowercase();
    lower.contains("-en-codeblock")
        || (lower.contains("font-family")
            && lower.contains("monospace")
            && lower.contains("background-color"))
}

fn replace_inline_code(content: &str) -> String {
    replace_tag_blocks(
        content,
        "code",
        |_| true,
        |inner| {
            let text = extract_plain_code(inner);
            let trimmed = text.trim();
            if trimmed.is_empty() {
                String::new()
            } else {
                format!("`{}`", xml_escape_tags_only(trimmed))
            }
        },
    )
}

fn replace_blockquotes(content: &str) -> String {
    replace_tag_blocks(
        content,
        "blockquote",
        |_| true,
        |inner| {
            let text = extract_plain_code(inner);
            let trimmed = text.trim_matches('\n');
            if trimmed.trim().is_empty() {
                return String::new();
            }
            let mut out = String::new();
            for line in trimmed.lines() {
                if line.trim().is_empty() {
                    out.push_str(">\n");
                } else {
                    out.push_str("> ");
                    out.push_str(&xml_escape_tags_only(line));
                    out.push('\n');
                }
            }
            out.push('\n');
            out
        },
    )
}

fn replace_headings(content: &str) -> String {
    let mut cur = content.to_owned();
    for (tag, hashes) in [
        ("h1", "#"),
        ("h2", "##"),
        ("h3", "###"),
        ("h4", "####"),
        ("h5", "#####"),
        ("h6", "######"),
    ] {
        cur = replace_tag_blocks(
            &cur,
            tag,
            |_| true,
            |inner| {
                let trimmed = inner.trim();
                if trimmed.is_empty() {
                    String::new()
                } else {
                    format!("{hashes} {trimmed}\n\n")
                }
            },
        );
    }
    cur
}

fn replace_bold(content: &str) -> String {
    let cur = replace_tag_blocks(content, "strong", |_| true, format_bold);
    replace_tag_blocks(&cur, "b", |_| true, format_bold)
}

fn format_bold(inner: &str) -> String {
    let text = extract_plain_code(inner);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        String::new()
    } else {
        format!("**{}**", xml_escape_tags_only(trimmed))
    }
}

fn replace_italic(content: &str) -> String {
    let cur = replace_tag_blocks(content, "em", |_| true, format_italic);
    replace_tag_blocks(&cur, "i", |_| true, format_italic)
}

fn format_italic(inner: &str) -> String {
    let text = extract_plain_code(inner);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        String::new()
    } else {
        format!("_{}_", xml_escape_tags_only(trimmed))
    }
}

fn replace_links(content: &str) -> String {
    replace_tag_blocks_with_open(
        content,
        "a",
        |_| true,
        |open_tag, inner| {
            let label = xml_escape_tags_only(extract_plain_code(inner).trim());
            let href =
                attr_value(open_tag, "href").map(|h| xml_escape_tags_only(&xml_unescape(&h)));
            match (label.is_empty(), href) {
                (true, Some(h)) => h,
                (false, Some(h)) if label == h => h,
                (false, Some(h)) => format!("[{label}]({h})"),
                (false, None) => label,
                (true, None) => String::new(),
            }
        },
    )
}

fn replace_todos(content: &str) -> String {
    content
        .replace("<en-todo checked=\"true\"></en-todo>", "- [x] ")
        .replace("<en-todo checked=\"true\"/>", "- [x] ")
        .replace("<en-todo checked=\"true\" />", "- [x] ")
        .replace("<en-todo checked=\"false\"></en-todo>", "- [ ] ")
        .replace("<en-todo checked=\"false\"/>", "- [ ] ")
        .replace("<en-todo></en-todo>", "- [ ] ")
        .replace("<en-todo/>", "- [ ] ")
        .replace("<en-todo />", "- [ ] ")
}

fn replace_paragraphs(content: &str) -> String {
    replace_tag_blocks(
        content,
        "p",
        |_| true,
        |inner| {
            let trimmed = inner.trim();
            if trimmed.is_empty() || matches!(trimmed, "<br/>" | "<br />" | "<br>") {
                format!("\n{EXPLICIT_BLANK_MARKER}\n")
            } else {
                format!("{trimmed}\n\n")
            }
        },
    )
}

fn replace_divs(content: &str) -> String {
    let mut out = String::new();
    let mut rest = content;

    while let Some((open_start, open_end, _)) = find_open_tag(rest, "div") {
        out.push_str(&rest[..open_start]);
        let Some((body_end, close_end)) = find_matching_close_tag(rest, "div", open_end) else {
            out.push_str(&rest[open_start..]);
            return out;
        };
        let inner_raw = &rest[open_end..body_end];
        let inner = replace_divs(inner_raw);
        let trimmed = inner.trim();
        let tail = &rest[close_end..];
        let next = tail.trim_start();
        let adjacent_div = find_open_tag(next, "div").is_some_and(|(s, _, _)| s == 0);

        if trimmed.is_empty() || matches!(trimmed, "<br/>" | "<br />" | "<br>") {
            out.push_str(&format!("\n{EXPLICIT_BLANK_MARKER}\n"));
        } else if adjacent_div {
            out.push_str(&format!("{trimmed}\n"));
        } else {
            out.push_str(&format!("{trimmed}\n\n"));
        }
        rest = if adjacent_div { next } else { tail };
    }

    out.push_str(rest);
    out
}

fn extract_plain_code(html: &str) -> String {
    let mut out = String::new();
    let mut tag = String::new();
    let mut in_tag = false;

    for ch in html.chars() {
        if in_tag {
            tag.push(ch);
            if ch == '>' {
                let lower = tag.to_ascii_lowercase();
                let t = lower.trim();
                if t.starts_with("<br") || t.starts_with("</div") || t.starts_with("</p") {
                    out.push('\n');
                }
                tag.clear();
                in_tag = false;
            }
        } else if ch == '<' {
            tag.push(ch);
            in_tag = true;
        } else {
            out.push(ch);
        }
    }
    if !tag.is_empty() {
        out.push_str(&tag);
    }
    xml_unescape(&out)
}

fn replace_tag_blocks<P, F>(content: &str, tag: &str, predicate: P, formatter: F) -> String
where
    P: Fn(&str) -> bool,
    F: Fn(&str) -> String,
{
    replace_tag_blocks_with_open(content, tag, predicate, |_, inner| formatter(inner))
}

fn replace_tag_blocks_with_open<P, F>(
    content: &str,
    tag: &str,
    predicate: P,
    formatter: F,
) -> String
where
    P: Fn(&str) -> bool,
    F: Fn(&str, &str) -> String,
{
    let mut out = String::new();
    let mut rest = content;

    while let Some((open_start, open_end, open_tag)) = find_open_tag(rest, tag) {
        out.push_str(&rest[..open_start]);
        if open_tag.ends_with("/>") || !predicate(open_tag) {
            out.push_str(&rest[open_start..open_end]);
            rest = &rest[open_end..];
            continue;
        }
        let Some((body_end, close_end)) = find_matching_close_tag(rest, tag, open_end) else {
            out.push_str(&rest[open_start..]);
            return out;
        };
        out.push_str(&formatter(open_tag, &rest[open_end..body_end]));
        rest = &rest[close_end..];
    }

    out.push_str(rest);
    out
}

fn find_open_tag<'a>(content: &'a str, tag: &str) -> Option<(usize, usize, &'a str)> {
    let needle = format!("<{tag}");
    let mut offset = 0;
    while let Some(rel) = content[offset..].find(&needle) {
        let start = offset + rel;
        let name_end = start + needle.len();
        let next = content[name_end..].chars().next();
        if next.is_some_and(|c| matches!(c, '>' | '/' | ' ' | '\t' | '\n' | '\r')) {
            let rel_end = content[name_end..].find('>')?;
            let end = name_end + rel_end + 1;
            return Some((start, end, &content[start..end]));
        }
        offset = name_end;
    }
    None
}

fn find_matching_close_tag(content: &str, tag: &str, body_start: usize) -> Option<(usize, usize)> {
    let close_needle = format!("</{tag}>");
    let mut depth = 1usize;
    let mut cursor = body_start;

    while cursor < content.len() {
        let next_open = find_open_tag(&content[cursor..], tag)
            .map(|(s, e, open)| (cursor + s, cursor + e, open.ends_with("/>")));
        let next_close = content[cursor..].find(&close_needle).map(|s| cursor + s);

        match (next_open, next_close) {
            (Some((os, oe, self_closing)), Some(cs)) if os < cs => {
                if !self_closing {
                    depth += 1;
                }
                cursor = oe;
            }
            (_, Some(cs)) => {
                depth -= 1;
                let ce = cs + close_needle.len();
                if depth == 0 {
                    return Some((cs, ce));
                }
                cursor = ce;
            }
            (Some((_, oe, self_closing)), None) => {
                if !self_closing {
                    depth += 1;
                }
                cursor = oe;
            }
            (None, None) => return None,
        }
    }
    None
}

fn strip_html_tags(content: &str) -> String {
    let mut out = String::with_capacity(content.len());
    let mut in_tag = false;
    for ch in content.chars() {
        match ch {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    out
}

fn normalize_blank_lines(content: &str) -> String {
    let normalized = content.replace("\r\n", "\n");
    let mut out = String::new();
    let mut generated_blank = false;
    let mut explicit_blanks = 0usize;

    for line in normalized.lines() {
        if line.trim() == EXPLICIT_BLANK_MARKER {
            explicit_blanks += 1;
        } else if line.trim().is_empty() {
            generated_blank = true;
        } else {
            if !out.is_empty() {
                let blanks = explicit_blanks.max(usize::from(generated_blank));
                out.push_str(&"\n".repeat(blanks));
            }
            out.push_str(line.trim_end());
            out.push('\n');
            generated_blank = false;
            explicit_blanks = 0;
        }
    }

    if !out.ends_with('\n') && !out.is_empty() {
        out.push('\n');
    }
    out
}

fn markdown_to_enml_inner(markdown: &str, placeholders: &[(String, String)]) -> String {
    let mut html = String::new();
    let mut in_code_fence: Option<usize> = None;
    let mut code_lines: Vec<String> = Vec::new();

    for raw_line in markdown.replace("\r\n", "\n").lines() {
        if let Some(fence_len) = in_code_fence {
            let trimmed = raw_line.trim();
            if trimmed.starts_with("```")
                && trimmed.chars().take_while(|&c| c == '`').count() >= fence_len
                && trimmed.trim_start_matches('`').trim().is_empty()
            {
                in_code_fence = None;
                let code_body = code_lines
                    .iter()
                    .map(|l| format!("<div>{}</div>", xml_escape_text_or_br(l)))
                    .collect::<String>();
                html.push_str(&format!(
                    "<div style=\"-en-codeblock:true;box-sizing:border-box;padding:8px;\
                     font-family:Monaco,Menlo,Consolas,monospace;font-size:12px;\
                     background-color:rgb(251,250,248);\">{code_body}</div>"
                ));
                code_lines.clear();
            } else {
                code_lines.push(raw_line.to_owned());
            }
            continue;
        }

        let trimmed_start = raw_line.trim_start();
        if trimmed_start.starts_with("```") {
            let count = trimmed_start.chars().take_while(|&c| c == '`').count();
            in_code_fence = Some(count);
            continue;
        }

        let trimmed = raw_line.trim();
        if trimmed.is_empty() {
            html.push_str("<div><br/></div>");
            continue;
        }

        if matches!(trimmed, "---" | "***" | "------------------------") {
            html.push_str("<hr/>");
            continue;
        }

        if let Some((_, media_xml)) = placeholders.iter().find(|(label, _)| label == trimmed) {
            html.push_str(&format!("<div>{media_xml}</div>"));
            continue;
        }

        if let Some((level, heading_text)) = parse_markdown_heading(trimmed_start) {
            let rendered = render_inline_markdown(heading_text, placeholders);
            html.push_str(&format!("<h{level}>{rendered}</h{level}>"));
            continue;
        }

        if let Some(quote_body) = trimmed_start
            .strip_prefix("> ")
            .or_else(|| (trimmed_start == ">").then_some(""))
        {
            let rendered = if quote_body.is_empty() {
                "<br/>".to_owned()
            } else {
                render_inline_markdown(quote_body, placeholders)
            };
            html.push_str(&format!("<blockquote><div>{rendered}</div></blockquote>"));
            continue;
        }

        if let Some((checked, task_text)) = parse_markdown_todo(trimmed_start) {
            let todo_tag = if checked {
                "<en-todo checked=\"true\"/>"
            } else {
                "<en-todo/>"
            };
            let rendered = render_inline_markdown(task_text, placeholders);
            html.push_str(&format!("<div>{todo_tag}{rendered}</div>"));
            continue;
        }

        let rendered = render_inline_markdown(raw_line, placeholders);
        html.push_str(&format!("<div>{rendered}</div>"));
    }

    if in_code_fence.is_some() && !code_lines.is_empty() {
        let code_body = code_lines
            .iter()
            .map(|l| format!("<div>{}</div>", xml_escape_text_or_br(l)))
            .collect::<String>();
        html.push_str(&format!(
            "<div style=\"-en-codeblock:true;\">{code_body}</div>"
        ));
    }

    if html.is_empty() {
        html.push_str("<div><br/></div>");
    }

    html
}

fn parse_markdown_heading(line: &str) -> Option<(usize, &str)> {
    let hashes = line.chars().take_while(|&c| c == '#').count();
    if (1..=6).contains(&hashes) {
        let rest = &line[hashes..];
        if let Some(stripped) = rest.strip_prefix(' ') {
            return Some((hashes, stripped.trim()));
        }
    }
    None
}

fn parse_markdown_todo(line: &str) -> Option<(bool, &str)> {
    for (prefix, checked) in [
        ("- [x] ", true),
        ("- [X] ", true),
        ("* [x] ", true),
        ("* [X] ", true),
        ("- [ ] ", false),
        ("* [ ] ", false),
    ] {
        if let Some(rest) = line.strip_prefix(prefix) {
            return Some((checked, rest));
        }
    }
    None
}

fn render_inline_markdown(text: &str, placeholders: &[(String, String)]) -> String {
    let mut working = text.to_owned();
    let mut inline_tokens: Vec<String> = Vec::new();

    for (label, media_xml) in placeholders {
        if working.contains(label) {
            let idx = inline_tokens.len();
            inline_tokens.push(media_xml.clone());
            let marker = format!("{MEDIA_SENTINEL_PREFIX}INLINE_{idx}{MEDIA_SENTINEL_SUFFIX}");
            working = working.replace(label, &marker);
        }
    }

    let mut out = String::new();
    let chars: Vec<char> = working.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        // Inline code `...`
        if chars[i] == '`'
            && let Some(end) = chars[i + 1..].iter().position(|&c| c == '`')
        {
            let inner: String = chars[i + 1..i + 1 + end].iter().collect();
            out.push_str("<code>");
            out.push_str(&xml_escape_text(&inner));
            out.push_str("</code>");
            i += end + 2;
            continue;
        }

        // Bold **...**
        if i + 1 < chars.len()
            && chars[i] == '*'
            && chars[i + 1] == '*'
            && let Some(end) = find_double_delim(&chars[i + 2..], '*')
        {
            let inner: String = chars[i + 2..i + 2 + end].iter().collect();
            out.push_str("<b>");
            out.push_str(&xml_escape_text(&inner));
            out.push_str("</b>");
            i += end + 4;
            continue;
        }

        // Markdown link [label](https://...)
        if chars[i] == '['
            && let Some((label, url, consumed)) = try_parse_link(&chars[i..])
        {
            out.push_str("<a href=\"");
            out.push_str(&xml_escape_attr(&url));
            out.push_str("\">");
            out.push_str(&xml_escape_text(&label));
            out.push_str("</a>");
            i += consumed;
            continue;
        }

        // Italic _..._ (when bounded cleanly)
        if chars[i] == '_'
            && (i == 0 || chars[i - 1].is_whitespace())
            && i + 1 < chars.len()
            && !chars[i + 1].is_whitespace()
            && let Some(end) = chars[i + 1..].iter().position(|&c| c == '_')
            && end > 0
        {
            let inner: String = chars[i + 1..i + 1 + end].iter().collect();
            out.push_str("<i>");
            out.push_str(&xml_escape_text(&inner));
            out.push_str("</i>");
            i += end + 2;
            continue;
        }

        out.push_str(&xml_escape_char(chars[i]));
        i += 1;
    }

    for (idx, media_xml) in inline_tokens.iter().enumerate() {
        let marker = format!("{MEDIA_SENTINEL_PREFIX}INLINE_{idx}{MEDIA_SENTINEL_SUFFIX}");
        out = out.replace(&marker, media_xml);
    }

    if out.is_empty() {
        "<br/>".to_owned()
    } else {
        out
    }
}

fn find_double_delim(slice: &[char], delim: char) -> Option<usize> {
    if slice.len() < 2 {
        return None;
    }
    (0..slice.len() - 1).find(|&idx| idx > 0 && slice[idx] == delim && slice[idx + 1] == delim)
}

fn try_parse_link(slice: &[char]) -> Option<(String, String, usize)> {
    debug_assert_eq!(slice.first(), Some(&'['));
    let close_bracket = slice[1..].iter().position(|&c| c == ']')? + 1;
    if slice.get(close_bracket + 1) != Some(&'(') {
        return None;
    }
    let url_start = close_bracket + 2;
    let close_paren = slice[url_start..].iter().position(|&c| c == ')')? + url_start;
    let label: String = slice[1..close_bracket].iter().collect();
    let url: String = slice[url_start..close_paren].iter().collect();
    if !(url.starts_with("http://") || url.starts_with("https://") || url.starts_with("evernote:"))
    {
        return None;
    }
    Some((label, url, close_paren + 1))
}

fn xml_escape_text_or_br(line: &str) -> String {
    if line.is_empty() {
        "<br/>".to_owned()
    } else {
        xml_escape_text(line)
    }
}

fn xml_escape_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        out.push_str(&xml_escape_char(ch));
    }
    out
}

fn xml_escape_char(ch: char) -> String {
    match ch {
        '&' => "&amp;".to_owned(),
        '<' => "&lt;".to_owned(),
        '>' => "&gt;".to_owned(),
        '"' => "&quot;".to_owned(),
        '\'' => "&apos;".to_owned(),
        c if c.is_control() && !matches!(c, '\t' | '\n') => String::new(),
        c => c.to_string(),
    }
}

fn xml_escape_attr(text: &str) -> String {
    xml_escape_text(text)
}

fn xml_escape_tags_only(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn xml_unescape(text: &str) -> String {
    text.replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_image_and_file_placeholders_with_filenames_and_roundtrips() {
        let resources = vec![
            ResourceMeta {
                guid: "r1".to_owned(),
                mime: Some("image/png".to_owned()),
                filename: "diagram.png".to_owned(),
                body_hash_hex: "0123456789abcdef0123456789abcdef".to_owned(),
                size: 1200,
            },
            ResourceMeta {
                guid: "r2".to_owned(),
                mime: Some("application/pdf".to_owned()),
                filename: "spec.pdf".to_owned(),
                body_hash_hex: "fedcba9876543210fedcba9876543210".to_owned(),
                size: 8400,
            },
        ];

        let enml = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
            <!DOCTYPE en-note SYSTEM \"http://xml.evernote.com/pub/enml2.dtd\">\
            <en-note>\
            <div>Hello Evernote</div>\
            <div><en-media type=\"image/png\" hash=\"0123456789abcdef0123456789abcdef\"/></div>\
            <div><en-media type=\"application/pdf\" hash=\"fedcba9876543210fedcba9876543210\"/></div>\
            </en-note>";

        let doc = PreparedNoteDocument::from_enml(enml, &resources);
        assert!(doc.markdown.contains("Hello Evernote"));
        assert!(doc.markdown.contains("[Image: diagram.png]"));
        assert!(doc.markdown.contains("[Attachment: spec.pdf]"));

        let edited = format!("{}\nAdded line in vim\n", doc.markdown.trim_end());
        let saved_enml = doc.to_enml(&edited);
        assert!(
            saved_enml.contains(
                "<en-media type=\"image/png\" hash=\"0123456789abcdef0123456789abcdef\"/>"
            )
        );
        assert!(saved_enml.contains(
            "<en-media type=\"application/pdf\" hash=\"fedcba9876543210fedcba9876543210\"/>"
        ));
        assert!(saved_enml.contains("Added line in vim"));
    }

    #[test]
    fn uses_fallback_filename_when_resource_has_no_filename() {
        let resources = vec![ResourceMeta {
            guid: "r1".to_owned(),
            mime: Some("image/jpeg".to_owned()),
            filename: String::new(),
            body_hash_hex: "abcdef1234567890abcdef1234567890".to_owned(),
            size: 512,
        }];
        let enml = "<en-note><en-media type=\"image/jpeg\" hash=\"abcdef1234567890abcdef1234567890\"/></en-note>";
        let doc = PreparedNoteDocument::from_enml(enml, &resources);
        assert_eq!(doc.markdown.trim(), "[Image: image-abcdef12.jpg]");
    }

    #[test]
    fn converts_todos_headings_and_links() {
        let enml = "<en-note><h1>Plan</h1><div><en-todo checked=\"true\"/>Ship MVP</div><div><a href=\"https://evernote.com\">Site</a></div></en-note>";
        let doc = PreparedNoteDocument::from_enml(enml, &[]);
        assert!(doc.markdown.contains("# Plan"));
        assert!(doc.markdown.contains("- [x] Ship MVP"));
        assert!(doc.markdown.contains("[Site](https://evernote.com)"));

        let rebuilt = doc.to_enml(&doc.markdown);
        assert!(rebuilt.contains("<h1>Plan</h1>"));
        assert!(rebuilt.contains("<en-todo checked=\"true\"/>Ship MVP"));
        assert!(rebuilt.contains("<a href=\"https://evernote.com\">Site</a>"));
    }
}
