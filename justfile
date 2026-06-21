# seestar-proxy — task runner
#
# Run `just` (or `just --list`) to see all recipes.
# See CLAUDE.md and doc/architecture.md for project details.

# Default telescope address; override per-invocation, e.g. `just dev 192.168.1.50`,
# or persistently via the SEESTAR_UPSTREAM environment variable.
upstream := env_var_or_default("SEESTAR_UPSTREAM", "seestar.local")

# Show available recipes.
default:
    @just --list

# ── Run ──────────────────────────────────────────────────────────────────────

# Run the proxy, forwarding any extra args (e.g. `just run --upstream 192.168.1.50 --discovery`).
run *args:
    cargo run -- {{args}}

# Typical dev loop: run against {{upstream}} with discovery bridging and debug logging.
dev addr=upstream:
    cargo run -- --upstream {{addr}} --discovery -v

# Raw pipe mode (transparent byte forwarding, single client, for diagnostics).
raw addr=upstream:
    cargo run -- --upstream {{addr}} --raw

# Record telescope traffic to a session directory.
record dir addr=upstream:
    cargo run -- --upstream {{addr}} --record {{dir}}

# Replay a previously recorded session directory (no telescope needed).
replay dir:
    cargo run -- --replay {{dir}}

# ── Build ────────────────────────────────────────────────────────────────────

# Debug build.
build:
    cargo build

# Size-optimised release build (good for Raspberry Pi).
build-release:
    cargo build --release

# Release build without WireGuard (smaller binary).
build-no-wg:
    cargo build --release --no-default-features

# Cross-compile a release build for 64-bit Raspberry Pi (requires `cargo install cross`).
build-pi:
    cross build --release --target aarch64-unknown-linux-musl

# Cross-compile a release build for 32-bit Raspberry Pi (requires `cargo install cross`).
build-pi32:
    cross build --release --target armv7-unknown-linux-musleabihf

# ── Test ─────────────────────────────────────────────────────────────────────

# Run all unit and integration tests.
test:
    cargo test

# Faster test runner (used in CI).
test-fast:
    cargo nextest run

# Run tests for a single module, e.g. `just test-mod metrics`.
test-mod module:
    cargo test --lib {{module}}

# Do NOT use --all-features: lua54+luajit breaks mlua-sys, tailscale needs Go.
# Enforce the CI line-coverage threshold (75%) with cargo-llvm-cov.
coverage:
    cargo llvm-cov --features wireguard,luajit,ntp --lcov --output-path lcov.info --fail-under-lines 75

# ── Lint & format ────────────────────────────────────────────────────────────

# Format all sources.
fmt:
    cargo fmt

# Check formatting without modifying files (as CI does).
fmt-check:
    cargo fmt -- --check

# Clippy with all warnings treated as errors (the hard gate).
clippy:
    cargo clippy -- -D warnings

# Run formatting + clippy together (the full pre-commit gate).
lint: fmt clippy

# Fast type-check without producing a binary.
check:
    cargo check

# ── Bluetooth LE tools ───────────────────────────────────────────────────────

# Scan for nearby Seestar telescopes over Bluetooth LE.
ble-scan *args:
    cargo run --features bluetooth --bin ble_scan -- {{args}}

# Pair / configure a Seestar's WiFi over Bluetooth LE.
ble-pair *args:
    cargo run --features bluetooth --bin ble_pair -- {{args}}

# ── Maintenance ──────────────────────────────────────────────────────────────

# Install the pre-commit hook (cargo fmt + clippy on staged files).
install-hooks:
    ./scripts/install_pre_commit_rustfmt.sh

# Remove build artifacts.
clean:
    cargo clean
