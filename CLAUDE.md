# seestar-proxy — Claude Code Guide

## Project overview

TCP proxy and multiplexer for [Seestar](https://www.zwoastro.com/seestar/) smart telescopes,
written in Rust (edition 2024). Allows multiple clients to share a single telescope connection.
See [doc/architecture.md](doc/architecture.md) for a full description of the design.

## Build commands

```bash
# Development
cargo build

# Release (size-optimised for Raspberry Pi)
cargo build --release

# Without WireGuard (smaller binary)
cargo build --release --no-default-features

# Cross-compile for Pi (requires `cargo install cross`)
cross build --release --target aarch64-unknown-linux-musl
cross build --release --target armv7-unknown-linux-musleabihf
```

## Testing

```bash
# Run all unit and integration tests
cargo test

# Faster test runner (used in CI)
cargo nextest run

# Run tests for a specific module
cargo test --lib metrics
cargo test --lib control
```

Unit tests live at the bottom of each source file in a `#[cfg(test)]` block.
Integration tests are in `tests/`.

### Code coverage

CI enforces a **75% line coverage threshold** using `cargo-llvm-cov`:

```bash
cargo llvm-cov --features wireguard,luajit,ntp --lcov --output-path lcov.info --fail-under-lines 75
```

Do not use `--all-features`: combining `lua54 + luajit` breaks `mlua-sys`, and `tailscale`
requires a Go toolchain.

## Linting and formatting

A pre-commit hook runs automatically on every commit. It:
1. Formats staged `.rs` files with `cargo fmt`
2. Runs `cargo clippy -- -D warnings` and blocks the commit on any warning

Run manually before committing:

```bash
cargo fmt
cargo clippy -- -D warnings
```

`rustfmt` is configured in `rustfmt.toml` (edition 2024, max line width 100).

CI (`ci.yml`) runs `cargo fmt -- --check` and `cargo clippy -- -D warnings` on every push and
pull request to `main`.

## Feature flags

| Feature | Default | Notes |
|---------|---------|-------|
| `wireguard` | on | Embedded WireGuard VPN endpoint |
| `luajit` | on | LuaJIT runtime for hook scripts |
| `lua54` | off | PUC-Lua 5.4 runtime (mutually exclusive with `luajit`) |
| `ntp` | off | NTP clock sync at startup |
| `tailscale` | off | Embedded Tailscale node (requires Go toolchain to build) |

## Code conventions

### Write unit tests for new logic

Every non-trivial function or behaviour change must be accompanied by unit tests. Add them in a
`#[cfg(test)]` block at the bottom of the same file. Follow the naming pattern already used in
the codebase:

```
<unit_under_test>_<condition>_<expected_result>
```

For example: `update_event_pi_status_battery_float_is_rounded`.

### Keep tests focused

- One assertion per test where practical — use separate tests for separate behaviours.
- Test edge cases explicitly: missing fields, type coercions, state that must be preserved vs.
  cleared.
- Avoid mocking internal Rust state; prefer constructing real `Metrics`, `Config`, etc. objects.

### Clippy is a hard gate

All clippy warnings are errors (`-D warnings`). Fix them rather than suppressing them. If
suppression is genuinely necessary, use a targeted `#[allow(...)]` with a comment explaining why.

### Async and channels

The proxy is fully async (Tokio). Prefer channels for communication between tasks; use `Mutex`
only where shared mutable state is unavoidable. New tasks should follow the existing
spawn-and-channel pattern in `control.rs` and `imaging.rs`.

### No speculative features

Only implement what is explicitly asked for. Do not add error handling for scenarios that cannot
occur, extra configuration options, or backwards-compatibility shims for code that has no
existing callers.

## Architecture reference

- [doc/architecture.md](doc/architecture.md) — module structure, concurrency model, design decisions
- [doc/hooks.md](doc/hooks.md) — Lua scripting API reference
