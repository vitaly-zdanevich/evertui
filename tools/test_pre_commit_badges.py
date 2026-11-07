"""Unit and integration tests for tools/pre_commit_badges.py and tools/install_git_hooks.py."""

import os
from pathlib import Path
import shlex
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

import install_git_hooks as installer
import pre_commit_badges as badges


TOOLS_DIR = Path(__file__).resolve().parent


class PreCommitBadgesTest(unittest.TestCase):
    def setUp(self):
        clean_env = {k: v for k, v in os.environ.items() if not k.startswith('GIT_')}
        self.env_patcher = mock.patch.dict(os.environ, clean_env, clear=True)
        self.env_patcher.start()
        self.addCleanup(self.env_patcher.stop)

        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)

        self.git('init', '-q')
        self.git('config', 'user.name', 'Badge Test')
        self.git('config', 'user.email', 'badge-test@example.invalid')
        self.git('config', 'core.hooksPath', '.git/hooks')

        for name in ('code_badges.py', 'pre_commit_badges.py', 'install_git_hooks.py'):
            self.write(f'tools/{name}', (TOOLS_DIR / name).read_text(encoding='utf-8'))
        self.write('src/lib.rs', 'pub fn first() {}\n')
        self.write('tests/e2e.rs', '#[test]\nfn check_it() {\n    assert!(true);\n}\n')
        self.git('add', '.')

    def git(self, *args: str, env=None) -> bytes:
        return subprocess.check_output(['git', *args], cwd=self.root, env=env, stderr=subprocess.PIPE)

    def write(self, rel: str, content: str) -> None:
        path = self.root / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content, encoding='utf-8')

    def commit(self, *args: str) -> bytes:
        return self.git('commit', '-q', *args, '-m', 'Test commit')

    def assert_badge_counts(self, prod: int, test: int, revision: str = ':') -> None:
        prod_svg = self.git('show', f'{revision}docs/badges/production-code.svg').decode('utf-8')
        test_svg = self.git('show', f'{revision}docs/badges/test-code.svg').decode('utf-8')
        self.assertIn(f'production code: {prod:,} lines', prod_svg)
        self.assertIn(f'test code: {test:,} lines', test_svg)

    def test_partial_staging_counts_only_staged_lines_and_keeps_unstaged_edits(self):
        unstaged = 'pub fn first() {}\npub fn unstaged_second() {}\n'
        self.write('src/lib.rs', unstaged)
        badges.refresh(self.root)
        self.assert_badge_counts(prod=1, test=4)
        self.assertEqual((self.root / 'src/lib.rs').read_text(encoding='utf-8'), unstaged)
        self.assertEqual(self.git('show', ':src/lib.rs'), b'pub fn first() {}\n')
        for badge_rel in badges.BADGES:
            self.assertEqual(
                self.git('show', f':{badge_rel.as_posix()}'),
                (self.root / badge_rel).read_bytes(),
            )

    def test_installed_hook_updates_badges_automatically_on_every_commit(self):
        installer.install(self.root)
        # First commit runs the installed pre-commit hook automatically
        self.commit()
        self.assert_badge_counts(prod=1, test=4, revision='HEAD:')
        self.assertEqual(self.git('status', '--porcelain'), b'')

        # Modify both src and tests and run `git commit -a`
        self.write(
            'src/lib.rs',
            'pub fn first() {}\npub fn second() {}\n#[cfg(test)]\nfn unit_helper() {}\n',
        )
        self.write(
            'tests/e2e.rs',
            '#[test]\nfn check_it() {\n    let a = 1;\n    assert_eq!(a, 1);\n}\n',
        )
        self.commit('-a')
        # prod: 2 lines, test: 2 inline lines + 5 e2e lines = 7 lines
        self.assert_badge_counts(prod=2, test=7, revision='HEAD:')
        self.assertEqual(self.git('status', '--porcelain'), b'')

    def test_unstaged_badge_edits_abort_without_changing_index(self):
        badges.refresh(self.root)
        self.write(str(badges.BADGES[0]), 'manual edit\n')
        before_tree = self.git('write-tree')
        with self.assertRaisesRegex(RuntimeError, 'unstaged badge'):
            badges.refresh(self.root)
        self.assertEqual(self.git('write-tree'), before_tree)
        self.assertEqual((self.root / badges.BADGES[0]).read_text(encoding='utf-8'), 'manual edit\n')

    def test_alternate_index_does_not_mutate_default_index(self):
        badges.refresh(self.root)
        self.commit()
        original = (self.root / '.git/index').read_bytes()
        alt = self.root / '.git/alt-index'
        shutil.copyfile(self.root / '.git/index', alt)
        with mock.patch.dict(os.environ, {'GIT_INDEX_FILE': str(alt)}):
            self.write('src/lib.rs', 'pub fn first() {}\npub fn second() {}\n')
            self.git('add', 'src/lib.rs')
            badges.refresh(self.root)
            self.assert_badge_counts(prod=2, test=4)
        self.assertEqual((self.root / '.git/index').read_bytes(), original)

    def test_staged_badge_deletion_is_regenerated(self):
        badges.refresh(self.root)
        self.commit()
        self.git('rm', str(badges.BADGES[0]))
        badges.refresh(self.root)
        self.assert_badge_counts(prod=1, test=4)
        self.assertEqual(self.git('status', '--porcelain'), b'')

    def test_export_ignores_smudge_filters(self):
        self.write('.gitattributes', '*.rs filter=addline\n')
        self.git('config', 'filter.addline.clean', 'cat')
        cmd = 'import sys; sys.stdout.write(sys.stdin.read() + "pub fn smudged() {}\\n")'
        self.git('config', 'filter.addline.smudge', shlex.quote(sys.executable) + ' -c ' + shlex.quote(cmd))
        self.git('add', '.gitattributes')
        badges.refresh(self.root)
        self.assert_badge_counts(prod=1, test=4)

    def test_installer_idempotency_and_safety_guards(self):
        # Idempotent on custom repo-local hooks dir
        self.git('config', 'core.hooksPath', '.custom-hooks')
        installer.install(self.root)
        installer.install(self.root)
        hook = self.root / '.custom-hooks/pre-commit'
        self.assertIn(installer.MARKER, hook.read_text(encoding='utf-8'))
        self.assertTrue(os.access(hook, os.X_OK))

        # Refuses overwriting foreign hook
        self.git('config', 'core.hooksPath', '.git/hooks')
        self.write('.git/hooks/pre-commit', '#!/bin/sh\necho custom\n')
        with self.assertRaisesRegex(RuntimeError, 'refusing to replace existing hook'):
            installer.install(self.root)

        # Refuses external shared hooks directory
        with tempfile.TemporaryDirectory() as ext_dir:
            self.git('config', 'core.hooksPath', ext_dir)
            with self.assertRaisesRegex(RuntimeError, 'outside this repository'):
                installer.install(self.root)
            self.assertFalse((Path(ext_dir) / 'pre-commit').exists())


if __name__ == '__main__':
    unittest.main()
