#!/usr/bin/env python3
"""Install the local Git pre-commit hook that updates README code badges."""

import os
from pathlib import Path
import subprocess
import sys


MARKER = '# evertui badge hook; managed by tools/install_git_hooks.py'
LAUNCHER = f'''#!/bin/sh
{MARKER}
root=$(git rev-parse --show-toplevel) || exit 1
exec python3 "$root/tools/pre_commit_badges.py"
'''


def repository_path(root: Path, name: str) -> Path:
    output = subprocess.check_output(
        ['git', 'rev-parse', '--path-format=absolute', '--git-path', name],
        cwd=root,
    )
    return Path(os.fsdecode(output).strip())


def install(root: Path | str) -> Path:
    """Install idempotently without overwriting foreign hooks or external hook dirs."""
    root_path = Path(root).resolve()
    hook = repository_path(root_path, 'hooks/pre-commit')
    common_output = subprocess.check_output(
        ['git', 'rev-parse', '--path-format=absolute', '--git-common-dir'],
        cwd=root_path,
    )
    common = Path(os.fsdecode(common_output).strip()).resolve()

    if root_path not in hook.resolve().parents and hook.resolve() != common / 'hooks/pre-commit':
        raise RuntimeError(
            f'hooks directory is outside this repository: {hook.parent}; '
            'add `python3 tools/pre_commit_badges.py` to your shared pre-commit hook explicitly'
        )

    if hook.is_symlink() or (
        hook.exists()
        and (not hook.is_file() or MARKER not in hook.read_text(encoding='utf-8'))
    ):
        raise RuntimeError(
            f'refusing to replace existing hook {hook}; '
            'add `python3 tools/pre_commit_badges.py` to your existing hook instead'
        )

    hook.parent.mkdir(parents=True, exist_ok=True)
    hook.write_text(LAUNCHER, encoding='utf-8', newline='\n')
    hook.chmod(hook.stat().st_mode | 0o111)
    print(f'Installed automatic staged-code badge hook: {hook}')
    return hook


def main() -> int:
    try:
        top = subprocess.check_output(['git', 'rev-parse', '--show-toplevel'])
        root = Path(os.fsdecode(top).strip())
        install(root)
    except (OSError, RuntimeError, subprocess.CalledProcessError) as err:
        print(f'evertui hook setup: {err}', file=sys.stderr)
        return 1
    return 0


if __name__ == '__main__':
    sys.exit(main())
