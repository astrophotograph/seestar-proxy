#!/usr/bin/env bash
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel 2>/dev/null || true)"
if [[ -z "${repo_root}" ]]; then
  echo "Error: not inside a git repository."
  exit 1
fi

hook_path="${repo_root}/.git/hooks/pre-commit"

cat >"${hook_path}" <<'HOOK'
#!/usr/bin/env bash
set -euo pipefail

if ! command -v cargo >/dev/null 2>&1; then
  echo "pre-commit: cargo not found; skipping format."
  exit 0
fi

echo "pre-commit: running cargo fmt..."
cargo fmt

# Re-stage any .rs files that cargo fmt changed (staged or unstaged).
mapfile -t changed < <(git diff --name-only -- '*.rs' || true)
if [[ ${#changed[@]} -gt 0 ]]; then
  git add -- "${changed[@]}"
fi

echo "pre-commit: running cargo clippy..."
cargo clippy -- -D warnings || {
  echo "pre-commit: clippy found issues; please fix them before committing."
  exit 1
}
HOOK

chmod +x "${hook_path}"
echo "Installed pre-commit hook at ${hook_path}"
