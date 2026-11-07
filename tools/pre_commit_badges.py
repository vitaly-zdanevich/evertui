#!/usr/bin/env python3
"""Regenerate the two README code badges from the exact staged Git index.

Exports staged blobs into a temporary directory, runs the staged
`tools/code_badges.py --write` against the active Git index, and updates only
`docs/badges/production-code.svg` and `docs/badges/test-code.svg` in both the
commit index and working tree so partial staging and `git commit -a` work
cleanly in a single commit.
"""

import os
from pathlib import Path
import subprocess
import sys
import tempfile


BADGES = (
    Path('docs/badges/production-code.svg'),
    Path('docs/badges/test-code.svg'),
)
COUNTER_REL = Path('tools/code_badges.py')


def git(root: Path, *arguments: str, input_bytes: bytes | None = None) -> bytes:
    """Run Git while preserving its active index and disabling lazy fetches."""
    env = {**os.environ, 'GIT_NO_LAZY_FETCH': '1'}
    return subprocess.check_output(
        ['git', *arguments],
        cwd=root,
        input=input_bytes,
        env=env,
        stderr=subprocess.PIPE,
    )


def git_path(root: Path, name: str) -> Path:
    """Resolve repository-local paths, including an inherited temporary index."""
    raw = git(root, 'rev-parse', '--path-format=absolute', '--git-path', name)
    return Path(os.fsdecode(raw).strip())


def index_entries(root: Path) -> dict[Path, tuple[str, str]]:
    """Read stage-0 blob identities and reject unresolved merge entries."""
    entries: dict[Path, tuple[str, str]] = {}
    for record in git(root, 'ls-files', '--stage', '-z').split(b'\0'):
        if not record:
            continue
        description, name = record.split(b'\t', 1)
        mode, object_id, stage = description.decode('ascii').split()
        if stage != '0':
            raise RuntimeError('resolve staged merge conflicts before regenerating badges')
        entries[Path(os.fsdecode(name))] = (mode, object_id)
    return entries


def check_badge_worktree(root: Path, entries: dict[Path, tuple[str, str]]) -> None:
    """Reject unstaged manual badge edits before modifying the index or files."""
    for name in BADGES:
        path = root / name
        if any((root / part).is_symlink() for part in (name, *name.parents)):
            raise RuntimeError(f'badge path must not contain a symbolic link: {name}')
        entry = entries.get(name)
        if entry is not None and entry[0] not in {'100644', '100755'}:
            raise RuntimeError(f'badge must be a regular staged file: {name}')
        staged = git(root, 'cat-file', 'blob', entry[1]) if entry is not None else None
        working = path.read_bytes() if path.exists() else None
        if working != staged:
            raise RuntimeError(
                f'unstaged badge changes in {name}; stage or discard those edits before committing'
            )


def export_index(root: Path, entries: dict[Path, tuple[str, str]], snapshot: Path) -> None:
    """Materialize raw staged blobs without checkout filters or network access."""
    blobs: list[tuple[Path, str, str]] = []
    for name, (mode, object_id) in entries.items():
        if name.is_absolute() or '..' in name.parts:
            raise RuntimeError(f'unsafe staged path: {name}')
        if mode == '160000':
            (snapshot / name).mkdir(parents=True, exist_ok=True)
        elif mode in {'100644', '100755', '120000'}:
            blobs.append((name, mode, object_id))
        else:
            raise RuntimeError(f'unsupported staged file mode {mode}: {name}')

    if not blobs:
        return

    batch_in = ''.join(f'{object_id}\n' for _, _, object_id in blobs).encode('ascii')
    contents = git(root, 'cat-file', '--batch', input_bytes=batch_in)
    offset = 0
    symlinks: list[tuple[Path, bytes]] = []
    for name, mode, object_id in blobs:
        header_end = contents.index(b'\n', offset)
        returned_id, kind, size = contents[offset:header_end].decode('ascii').split()
        if returned_id != object_id or kind != 'blob':
            raise RuntimeError(f'expected a staged blob for {name}')
        start = header_end + 1
        end = start + int(size)
        if contents[end : end + 1] != b'\n':
            raise RuntimeError(f'incomplete staged blob for {name}')
        content = contents[start:end]
        offset = end + 1
        path = snapshot / name
        path.parent.mkdir(parents=True, exist_ok=True)
        if mode == '120000':
            symlinks.append((path, content))
        else:
            path.write_bytes(content)

    for path, content in symlinks:
        path.symlink_to(os.fsdecode(content))


def refresh(root: Path | str) -> None:
    """Count staged sources and stage only the regenerated badge SVGs."""
    root_path = Path(root).resolve()
    entries = index_entries(root_path)
    check_badge_worktree(root_path, entries)

    with tempfile.TemporaryDirectory(prefix='evertui-staged-badges-') as tmp_dir:
        snapshot = Path(tmp_dir)
        export_index(root_path, entries, snapshot)
        counter = snapshot / COUNTER_REL
        for name in (*BADGES, COUNTER_REL):
            if any((snapshot / part).is_symlink() for part in (name, *name.parents)):
                raise RuntimeError(f'counter and badge paths must not contain symlinks: {name}')
        if not counter.is_file():
            raise RuntimeError(f'stage {COUNTER_REL} before committing')

        env = os.environ.copy()
        env.update(
            {
                'GIT_DIR': os.fsdecode(git(root_path, 'rev-parse', '--absolute-git-dir')).strip(),
                'GIT_WORK_TREE': str(snapshot),
                'GIT_INDEX_FILE': str(git_path(root_path, 'index')),
            }
        )
        env.pop('GIT_PREFIX', None)

        subprocess.run(
            [sys.executable, str(counter), '--write'],
            cwd=snapshot,
            env=env,
            check=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )

        generated = {name: (snapshot / name).read_bytes() for name in BADGES}
        check_badge_worktree(root_path, entries)

        updates: list[bytes] = []
        for name, content in generated.items():
            object_id = (
                git(root_path, 'hash-object', '-w', '--stdin', input_bytes=content)
                .strip()
                .decode('ascii')
            )
            mode = entries.get(name, ('100644', ''))[0]
            updates.append(f'{mode} {object_id}\t{name.as_posix()}\0'.encode('ascii'))

        git(root_path, 'update-index', '-z', '--index-info', input_bytes=b''.join(updates))
        for name, content in generated.items():
            path = root_path / name
            path.parent.mkdir(parents=True, exist_ok=True)
            if not path.exists() or path.read_bytes() != content:
                path.write_bytes(content)


def main() -> int:
    try:
        root = Path(os.fsdecode(git(Path.cwd(), 'rev-parse', '--show-toplevel')).strip())
        print('Updating code-count badges from staged files...', file=sys.stderr)
        refresh(root)
    except (OSError, RuntimeError, subprocess.CalledProcessError) as err:
        detail = (
            err.stderr.decode(errors='replace').strip()
            if isinstance(err, subprocess.CalledProcessError) and err.stderr
            else str(err)
        )
        print(f'evertui badge hook: {detail}', file=sys.stderr)
        return 1
    return 0


if __name__ == '__main__':
    sys.exit(main())
