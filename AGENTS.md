Run `cargo fmt` before every commit. CI runs `cargo fmt --check` and fails on any diff.

Version bump: follow semver: if this is a fix - increment the path version, like 0.0.1 -> 0.0.2
If this is a new feature - increase minor version: 0.0.1 -> 0.1.0
