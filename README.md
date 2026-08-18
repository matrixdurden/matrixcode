# MatrixCode

MatrixCode is a clean-room Rust terminal coding agent focused on minimal resource use, low latency, and a small maintainable architecture.

The project intentionally starts as a single crate and earns every dependency and abstraction. JCode and OpenCode can be studied as references, but they are not source-of-truth codebases for MatrixCode.

## Principles

- Rust 2024, native single-binary distribution.
- First frame and input readiness are startup-critical; repo/session/provider work must be lazy.
- Event-driven UI; no idle polling or needless redraws.
- Persistent sessions load metadata first and content on demand.
- File writes are journaled as MatrixCode transactions so undo/redo does not touch Git history.
- Codex and Claude only for the initial product.
- Dependencies, background tasks, allocations, and copies must justify their cost.

## Development

```sh
cargo fmt --all -- --check
cargo check --all-targets
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
cargo build --release
```

## Roadmap

1. Bootstrap and benchmark baseline.
2. Minimal event-driven TUI.
3. Lazy persistent sessions.
4. Transaction journal with conflict-safe undo/redo.
5. Coding tools and permission layer.
6. Provider/auth core and explicit multi-account selection.
7. Codex integration.
8. Claude integration.
9. Full coding-agent loop.
10. Profile-guided cleanup and JCode comparison.
