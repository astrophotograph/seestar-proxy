# Crashed recording sessions cannot be replayed (no manifest.json)

## Symptom

A recording directory left behind by `kill -9`, power loss or a crash contains
`control.jsonl` and `frames/` but no `manifest.json`. `ReplaySession::load`
fails with:

    Failed to read <dir>/manifest.json: No such file or directory (os error 2) — is this a valid recording directory?

## Reproduction

1. Run the proxy with recording enabled and let it capture some traffic.
2. `kill -9` the process. (Ctrl-C works: it calls `Recorder::finalize()`, the only
   place `manifest.json` is written — `src/recorder.rs`.)
3. Replay the directory. `ReplaySession::load` (`src/replay.rs`) reads the manifest
   first and returns `Err`.

Pinned by tests:
- `replay::tests::load_missing_manifest_with_valid_control_returns_helpful_error`
- `recorder::tests::record_control_without_finalize_is_readable_on_disk_and_has_no_manifest`
  (shows `control.jsonl` is flushed per message, so the data itself survives).

## Impact

The sessions most worth replaying (those that ended in a crash) are unloadable even
though all control data and frames are on disk. A crash is the realistic failure on a
headless Pi.

## Suggested direction (not implemented)

When `manifest.json` is missing, synthesize one on load from `control.jsonl`
(message count; last minus first timestamp as duration) and the files in `frames/`
(frame count). Optionally also write the manifest periodically while recording.
Frames are written with `fs::write` without fsync, so a truncated final frame is possible.
