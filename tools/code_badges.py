#!/usr/bin/env python3
"""Generate and verify deterministic README badges for production and test LOC.

Counts nonblank, noncomment physical Rust code lines tracked by Git:
- `docs/badges/production-code.svg`: runtime code in `src/**/*.rs` with
  `#[cfg(test)]` / `#[test]` items excluded.
- `docs/badges/test-code.svg`: `#[cfg(test)]` / `#[test]` code in `src/**/*.rs`
  plus integration test code in `tests/**/*.rs`.
"""

import argparse
import ast
import io
import json
from pathlib import Path, PurePosixPath
import re
import subprocess
import tokenize


ROOT = Path(__file__).resolve().parents[1]
PRODUCTION_BADGE = Path('docs/badges/production-code.svg')
TEST_BADGE = Path('docs/badges/test-code.svg')
README_PATH = Path('README.md')


def tokenize_rust(source: bytes):
    """Yield (kind, text, start, end) tokens for Rust source bytes."""
    n = len(source)
    i = 0
    while i < n:
        b = source[i]

        # Line comment: //...
        if b == ord('/') and i + 1 < n and source[i + 1] == ord('/'):
            start = i
            i += 2
            while i < n and source[i] != ord('\n'):
                i += 1
            yield ('comment', source[start:i], start, i)
            continue

        # Nested block comment: /* ... */
        if b == ord('/') and i + 1 < n and source[i + 1] == ord('*'):
            start = i
            i += 2
            depth = 1
            while i < n and depth > 0:
                if source[i] == ord('/') and i + 1 < n and source[i + 1] == ord('*'):
                    depth += 1
                    i += 2
                elif source[i] == ord('*') and i + 1 < n and source[i + 1] == ord('/'):
                    depth -= 1
                    i += 2
                else:
                    i += 1
            yield ('comment', source[start:i], start, i)
            continue

        # Raw string literal: r#"..."#, br#"..."#, cr#"..."#
        raw_match = re.match(rb'(?:b?r|cr)(#*)(")', source[i:])
        if raw_match:
            start = i
            hashes = raw_match.group(1)
            i += raw_match.end()
            closing = b'"' + hashes
            end_pos = source.find(closing, i)
            i = n if end_pos == -1 else end_pos + len(closing)
            yield ('string', source[start:i], start, i)
            continue

        # Normal / byte / C string literal: "...", b"...", c"..."
        str_match = re.match(rb'(?:b|c)?"', source[i:])
        if str_match:
            start = i
            i += str_match.end()
            while i < n:
                if source[i] == ord('\\'):
                    i += 2
                elif source[i] == ord('"'):
                    i += 1
                    break
                else:
                    i += 1
            yield ('string', source[start:i], start, i)
            continue

        # Byte char literal b'x' or char literal 'x' vs lifetime 'a
        if b == ord('b') and i + 1 < n and source[i + 1] == ord("'"):
            start = i
            i += 2
            while i < n:
                if source[i] == ord('\\'):
                    i += 2
                elif source[i] == ord("'"):
                    i += 1
                    break
                else:
                    i += 1
            yield ('char', source[start:i], start, i)
            continue

        if b == ord("'"):
            char_match = re.match(
                rb"'(?:\\(?:x[0-9a-fA-F]{2}|u\{[0-9a-fA-F_]+\}|.)|[^'\\\n\r\t])'",
                source[i:],
            )
            if char_match:
                start = i
                i += char_match.end()
                yield ('char', source[start:i], start, i)
                continue
            life_match = re.match(rb"'[A-Za-z_][A-Za-z0-9_]*", source[i:])
            if life_match:
                start = i
                i += life_match.end()
                yield ('lifetime', source[start:i], start, i)
                continue

        # Whitespace
        if b in b' \t\r\n':
            start = i
            i += 1
            while i < n and source[i] in b' \t\r\n':
                i += 1
            yield ('ws', source[start:i], start, i)
            continue

        # Identifiers / keywords
        id_match = re.match(rb'[A-Za-z_][A-Za-z0-9_]*', source[i:])
        if id_match:
            start = i
            i += id_match.end()
            yield ('ident', source[start:i], start, i)
            continue

        # Single punctuation / delimiter byte
        yield ('punct', source[i : i + 1], i, i + 1)
        i += 1


def strip_comments_preserving_lines(source: bytes) -> str:
    """Replace comments with spaces (preserving newlines) and decode UTF-8."""
    out = bytearray(source)
    for kind, chunk, start, end in tokenize_rust(source):
        if kind == 'comment':
            out[start:end] = re.sub(rb'[^\n]', b' ', chunk)
    return out.decode('utf-8')


def code_line_numbers(source: bytes) -> set[int]:
    """Return 0-based indices of lines containing at least one code token."""
    cleaned = strip_comments_preserving_lines(source)
    return {idx for idx, line in enumerate(cleaned.split('\n')) if line.strip()}


def _split_top_level_commas(tokens):
    groups = [[]]
    depth = 0
    for tok in tokens:
        txt = tok[1]
        if txt in (b'(', b'[', b'{'):
            depth += 1
            groups[-1].append(tok)
        elif txt in (b')', b']', b'}'):
            depth = max(0, depth - 1)
            groups[-1].append(tok)
        elif txt == b',' and depth == 0:
            groups.append([])
        else:
            groups[-1].append(tok)
    return [g for g in groups if g]


def _eval_cfg_expr(tokens, test_mode: bool):
    if not tokens:
        return None
    if len(tokens) == 1 and tokens[0][0] == 'ident':
        return {b'test': test_mode, b'true': True, b'false': False}.get(tokens[0][1])
    if (
        len(tokens) >= 3
        and tokens[0][0] == 'ident'
        and tokens[1][1] == b'('
        and tokens[-1][1] == b')'
    ):
        fn = tokens[0][1]
        args = _split_top_level_commas(tokens[2:-1])
        vals = [_eval_cfg_expr(arg, test_mode) for arg in args]
        if fn == b'all':
            return False if False in vals else (None if None in vals else True)
        if fn == b'any':
            return True if True in vals else (None if None in vals else False)
        if fn == b'not' and len(vals) == 1:
            return None if vals[0] is None else not vals[0]
    return None


def _attr_is_enabled(inner_tokens, test_mode: bool) -> bool:
    """Determine if #[inner_tokens] keeps the item enabled in the given mode."""
    if not inner_tokens:
        return True
    # #[test] or #[tokio::test] / #[foo::test]
    ident_parts = [t[1] for t in inner_tokens if t[0] in ('ident', 'punct')]
    if ident_parts == [b'test'] or (
        len(ident_parts) >= 3
        and ident_parts[-1] == b'test'
        and ident_parts[-2] == b':'
        and ident_parts[-3] == b':'
    ):
        return test_mode
    if (
        len(inner_tokens) >= 4
        and inner_tokens[0][1] == b'cfg'
        and inner_tokens[1][1] == b'('
        and inner_tokens[-1][1] == b')'
    ):
        args = _split_top_level_commas(inner_tokens[2:-1])
        if args:
            return _eval_cfg_expr(args[0], test_mode) is not False
    if (
        len(inner_tokens) >= 4
        and inner_tokens[0][1] == b'cfg_attr'
        and inner_tokens[1][1] == b'('
        and inner_tokens[-1][1] == b')'
    ):
        args = _split_top_level_commas(inner_tokens[2:-1])
        if args and _eval_cfg_expr(args[0], test_mode) is True:
            return all(_attr_is_enabled(arg, test_mode) for arg in args[1:])
    return True


def _find_matching_delim(sig_tokens, open_idx: int, open_byte: bytes, close_byte: bytes) -> int:
    depth = 0
    for j in range(open_idx, len(sig_tokens)):
        if sig_tokens[j][1] == open_byte:
            depth += 1
        elif sig_tokens[j][1] == close_byte:
            depth -= 1
            if depth == 0:
                return j
    return len(sig_tokens) - 1


_ITEM_KEYWORDS = {
    b'fn',
    b'async',
    b'unsafe',
    b'const',
    b'extern',
    b'struct',
    b'enum',
    b'union',
    b'trait',
    b'impl',
    b'mod',
    b'type',
    b'static',
    b'use',
    b'macro_rules',
}


def _starts_rust_item(sig_tokens, start_idx: int) -> bool:
    j = start_idx
    if j < len(sig_tokens) and sig_tokens[j][1] == b'pub':
        j += 1
        if j < len(sig_tokens) and sig_tokens[j][1] == b'(':
            j = _find_matching_delim(sig_tokens, j, b'(', b')') + 1
    return j < len(sig_tokens) and sig_tokens[j][1] in _ITEM_KEYWORDS


def _find_item_end_index(sig_tokens, start_idx: int) -> int:
    """Find the last token index of the Rust item/field/arm starting at start_idx."""
    is_item = _starts_rust_item(sig_tokens, start_idx)
    paren_depth = 0
    bracket_depth = 0
    j = start_idx
    while j < len(sig_tokens):
        txt = sig_tokens[j][1]
        if txt == b'(':
            paren_depth += 1
        elif txt == b')':
            if paren_depth == 0:
                return max(start_idx, j - 1)
            paren_depth -= 1
        elif txt == b'[':
            bracket_depth += 1
        elif txt == b']':
            if bracket_depth == 0:
                return max(start_idx, j - 1)
            bracket_depth -= 1
        elif paren_depth == 0 and bracket_depth == 0:
            if txt == b';':
                return j
            if txt == b',' and not is_item:
                return j
            if txt == b'{':
                close_brace = _find_matching_delim(sig_tokens, j, b'{', b'}')
                # Trailing comma on match arm / struct field initializer
                if (
                    not is_item
                    and close_brace + 1 < len(sig_tokens)
                    and sig_tokens[close_brace + 1][1] == b','
                ):
                    return close_brace + 1
                return close_brace
            if txt == b'}':
                return max(start_idx, j - 1)
        j += 1
    return len(sig_tokens) - 1


def mask_disabled_rust_items(source: bytes, *, test_mode: bool) -> bytes:
    """Mask items disabled by #[cfg(test)] / #[test] (or #[cfg(not(test))])."""
    sig_tokens = [
        tok for tok in tokenize_rust(source) if tok[0] not in ('ws', 'comment')
    ]
    masked = bytearray(source)

    def mask_span(start_b: int, end_b: int):
        masked[start_b:end_b] = re.sub(rb'[^\n]', b' ', source[start_b:end_b])

    idx = 0
    total = len(sig_tokens)
    while idx < total:
        # Inner crate/module attribute: #![cfg(test)]
        if (
            idx + 2 < total
            and sig_tokens[idx][1] == b'#'
            and sig_tokens[idx + 1][1] == b'!'
            and sig_tokens[idx + 2][1] == b'['
        ):
            close_idx = _find_matching_delim(sig_tokens, idx + 2, b'[', b']')
            inner = sig_tokens[idx + 3 : close_idx]
            if not _attr_is_enabled(inner, test_mode):
                mask_span(0, len(source))
                return bytes(masked)
            idx = close_idx + 1
            continue

        # Outer attribute chain: #[...] #[...] <item>
        if idx + 1 < total and sig_tokens[idx][1] == b'#' and sig_tokens[idx + 1][1] == b'[':
            attr_start_byte = sig_tokens[idx][2]
            keep_item = True
            cur = idx
            while (
                cur + 1 < total
                and sig_tokens[cur][1] == b'#'
                and sig_tokens[cur + 1][1] == b'['
            ):
                close_idx = _find_matching_delim(sig_tokens, cur + 1, b'[', b']')
                inner = sig_tokens[cur + 2 : close_idx]
                if not _attr_is_enabled(inner, test_mode):
                    keep_item = False
                cur = close_idx + 1

            if not keep_item and cur < total:
                end_tok_idx = _find_item_end_index(sig_tokens, cur)
                mask_span(attr_start_byte, sig_tokens[end_tok_idx][3])
                idx = end_tok_idx + 1
                continue

            idx = cur
            continue

        idx += 1

    return bytes(masked)


def python_code_line_numbers(source: bytes) -> set[int]:
    """Return 1-based code line numbers in Python source, excluding comments & docstrings."""
    tree = ast.parse(source)
    docstring_spans: set[tuple[int, int]] = set()
    for node in ast.walk(tree):
        if isinstance(node, (ast.Module, ast.ClassDef, ast.FunctionDef, ast.AsyncFunctionDef)):
            if (
                node.body
                and isinstance(node.body[0], ast.Expr)
                and isinstance(node.body[0].value, ast.Constant)
                and isinstance(node.body[0].value.value, str)
            ):
                doc_node = node.body[0].value
                if doc_node.lineno is not None and doc_node.end_lineno is not None:
                    docstring_spans.add((doc_node.lineno, doc_node.end_lineno))

    raw_lines = source.decode('utf-8').splitlines()
    code_lines: set[int] = set()
    ignored_types = {
        tokenize.COMMENT,
        tokenize.NL,
        tokenize.NEWLINE,
        tokenize.INDENT,
        tokenize.DEDENT,
        tokenize.ENCODING,
        tokenize.ENDMARKER,
    }
    for tok in tokenize.tokenize(io.BytesIO(source).readline):
        if tok.type in ignored_types:
            continue
        if tok.type == tokenize.STRING and (tok.start[0], tok.end[0]) in docstring_spans:
            continue
        for line_no in range(tok.start[0], tok.end[0] + 1):
            if 1 <= line_no <= len(raw_lines) and raw_lines[line_no - 1].strip():
                code_lines.add(line_no)
    return code_lines


def compute_counts(root: Path, tracked: set[str]) -> tuple[dict[str, int], dict[str, int]]:
    """Return (production_by_file, test_by_file) line counts."""
    prod_counts: dict[str, int] = {}
    test_counts: dict[str, int] = {}

    for rel in sorted(tracked):
        path = PurePosixPath(rel)
        if path.suffix == '.rs':
            raw = (root / path).read_bytes()
            if rel.startswith('src/'):
                prod_masked = mask_disabled_rust_items(raw, test_mode=False)
                test_masked = mask_disabled_rust_items(raw, test_mode=True)
                prod_lines = code_line_numbers(prod_masked)
                all_lines = code_line_numbers(test_masked)
                only_test_lines = all_lines - prod_lines
                if prod_lines:
                    prod_counts[rel] = len(prod_lines)
                if only_test_lines:
                    test_counts[rel] = len(only_test_lines)
            elif rel.startswith('tests/'):
                t_lines = code_line_numbers(mask_disabled_rust_items(raw, test_mode=True))
                if t_lines:
                    test_counts[rel] = len(t_lines)
        elif path.suffix == '.py' and (
            rel.startswith('tests/') or (rel.startswith('tools/') and path.name.startswith('test_'))
        ):
            py_lines = python_code_line_numbers((root / path).read_bytes())
            if py_lines:
                test_counts[rel] = len(py_lines)

    return prod_counts, test_counts


def badge_svg(count: int, *, scope: str) -> str:
    """Render a deterministic local SVG badge matching youta's style."""
    value = f'{count:,}'
    label, excluded, label_width = {
        'production': ('production code', 'tests, comments, blank lines and dependencies', 115),
        'test': ('test code', 'comments, blank lines and dependencies', 115),
    }[scope]
    width = label_width + 75
    return f'''<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="20" role="img" aria-label="{label}: {value} lines">
\t<title>{label.capitalize()}: {value} lines; excludes {excluded}</title>
\t<linearGradient id="shade" x2="0" y2="100%"><stop offset="0" stop-color="#fff" stop-opacity=".1"/><stop offset="1" stop-opacity=".1"/></linearGradient>
\t<clipPath id="round"><rect width="{width}" height="20" rx="3"/></clipPath>
\t<g clip-path="url(#round)"><path fill="#555" d="M0 0h{label_width}v20H0z"/><path fill="#007ec6" d="M{label_width} 0h75v20h-75z"/><path fill="url(#shade)" d="M0 0h{width}v20H0z"/></g>
\t<g fill="#fff" text-anchor="middle" font-family="Verdana,Geneva,DejaVu Sans,sans-serif" font-size="11">
\t\t<text x="{label_width / 2:g}" y="15" fill="#010101" fill-opacity=".3">{label}</text><text x="{label_width / 2:g}" y="14">{label}</text>
\t\t<text x="{label_width + 37.5:g}" y="15" fill="#010101" fill-opacity=".3">{value}</text><text x="{label_width + 37.5:g}" y="14">{value}</text>
\t</g>
</svg>
'''


def tracked_files(root: Path) -> set[str]:
    output = subprocess.check_output(['git', 'ls-files', '-z'], cwd=root).decode('utf-8')
    return set(output.split('\0')) - {''}


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument('--write', action='store_true', help='regenerate checked-in SVG badges')
    mode.add_argument('--check', action='store_true', help='fail if SVG badges or README links are missing/stale')
    args = parser.parse_args()

    tracked = tracked_files(ROOT)
    prod_files, test_files = compute_counts(ROOT, tracked)
    prod_total = sum(prod_files.values())
    test_total = sum(test_files.values())

    print(
        json.dumps(
            {
                'production': {'total': prod_total, 'files': prod_files},
                'tests': {'total': test_total, 'files': test_files},
            },
            indent=2,
        )
    )

    badges = [
        (PRODUCTION_BADGE, badge_svg(prod_total, scope='production')),
        (TEST_BADGE, badge_svg(test_total, scope='test')),
    ]

    for badge_rel, expected_svg in badges:
        target = ROOT / badge_rel
        if args.write:
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_text(expected_svg, encoding='utf-8')
        elif args.check:
            if not target.is_file() or target.read_text(encoding='utf-8') != expected_svg:
                parser.exit(
                    1,
                    f'{badge_rel} is missing or stale. Run: python3 tools/code_badges.py --write\n',
                )

    if args.check:
        readme_text = (ROOT / README_PATH).read_text(encoding='utf-8')
        for badge_rel, _ in badges:
            if badge_rel.as_posix() not in readme_text:
                parser.exit(1, f'{README_PATH} does not reference {badge_rel.as_posix()}\n')


if __name__ == '__main__':
    main()
