"""Unit tests for tools/code_badges.py."""

from pathlib import Path
import tempfile
import unittest

from code_badges import badge_svg, compute_counts


class CodeBadgesTest(unittest.TestCase):
    def test_separates_production_and_cfg_test_lines_ignoring_comments(self):
        sample_src = """\
// Module header comment
/* Nested /* block comment */ ignored */
pub fn add(a: i32, b: i32) -> i32 {
    let s = "// string literal is code, not a comment";
    let _brace_char = '{'; // inline comment on code line
    let _s_len = s.len() as i32;
    a + b + _s_len
}

#[cfg(test)]
fn helper_for_tests() -> i32 {
    42
}

#[cfg(test)]
mod tests {
    use super::*;

    // Comment inside test module
    #[test]
    fn checks_add() {
        assert_eq!(add(1, 2), 46);
        assert_eq!(helper_for_tests(), 42);
    }
}
"""
        sample_e2e = """\
// Integration test comment
#[test]
fn runs_e2e() {
    let x = 1 + 1;
    assert_eq!(x, 2);
}
"""
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / 'src').mkdir()
            (root / 'tests').mkdir()
            (root / 'src' / 'lib.rs').write_text(sample_src, encoding='utf-8')
            (root / 'tests' / 'e2e.rs').write_text(sample_e2e, encoding='utf-8')

            prod, tests = compute_counts(root, {'src/lib.rs', 'tests/e2e.rs'})
            self.assertEqual(prod, {'src/lib.rs': 6})
            self.assertEqual(tests, {'src/lib.rs': 13, 'tests/e2e.rs': 5})

    def test_handles_lifetimes_raw_strings_and_cfg_not_test(self):
        sample_src = """\
pub const LABEL: &'static str = r#"
#[cfg(test)]
// inside raw string, still code lines!
"#;

#[cfg(not(test))]
pub fn prod_only() -> usize {
    LABEL.len()
}

#[test]
fn standalone_test() {
    assert!(!LABEL.is_empty());
}
"""
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / 'src').mkdir()
            (root / 'src' / 'lib.rs').write_text(sample_src, encoding='utf-8')

            prod, tests = compute_counts(root, {'src/lib.rs'})
            # LABEL raw string spans 4 lines + #[cfg(not(test))] + 3 fn lines = 8 prod lines
            self.assertEqual(prod, {'src/lib.rs': 8})
            # #[test] + 3 fn lines = 4 test lines
            self.assertEqual(tests, {'src/lib.rs': 4})

    def test_handles_generic_where_clauses_and_cfg_test_struct_fields(self):
        sample_src = """\
pub struct State {
    pub active: bool,
    #[cfg(test)]
    pub debug_hits: usize,
}

#[cfg(test)]
pub(crate) fn generic_helper<A, B>(a: A, _b: B) -> A
where
    A: Clone,
    B: Copy,
{
    a.clone()
}
"""
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / 'src').mkdir()
            (root / 'src' / 'lib.rs').write_text(sample_src, encoding='utf-8')

            prod, tests = compute_counts(root, {'src/lib.rs'})
            self.assertEqual(prod, {'src/lib.rs': 3})
            self.assertEqual(tests, {'src/lib.rs': 10})

    def test_counts_python_test_files_excluding_docstrings_and_comments(self):
        sample_py = '''\
"""Module docstring excluded."""

import unittest


class SampleTest(unittest.TestCase):
    """Class docstring excluded."""

    def test_ok(self):
        """Method docstring excluded."""
        # Comment excluded
        value = 1 + 1
        self.assertEqual(value, 2)
'''
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / 'tools').mkdir()
            (root / 'tools' / 'test_sample.py').write_text(sample_py, encoding='utf-8')
            (root / 'tools' / 'helper.py').write_text('x = 1\n', encoding='utf-8')

            prod, tests = compute_counts(
                root, {'tools/test_sample.py', 'tools/helper.py'}
            )
            self.assertEqual(prod, {})
            # import unittest, class ..., def ..., value = ..., self.assertEqual... = 5 lines
            self.assertEqual(tests, {'tools/test_sample.py': 5})

    def test_badge_svg_is_pure_ascii_and_contains_formatted_count(self):
        svg = badge_svg(4812, scope='production')
        self.assertTrue(svg.isascii())
        self.assertIn('production code', svg)
        self.assertIn('4,812', svg)

        test_svg = badge_svg(1234, scope='test')
        self.assertTrue(test_svg.isascii())
        self.assertIn('test code', test_svg)
        self.assertIn('1,234', test_svg)


if __name__ == '__main__':
    unittest.main()
