# Replay of a recorded session diverges from the live run

Two defects found while writing the record → replay round-trip test
(`tests/replay_integration.rs`, `round_trip::replay_of_recorded_session_pins_late_client_and_state_divergence`).
Both are pinned as CURRENT behaviour; flip the asserts when fixed.

## 1. Replay starts at spawn, so a client that connects later receives nothing

### Symptom

Start the proxy in replay mode and connect a client. For a recording whose
messages have (near-)zero inter-message deltas, the client receives no
telescope messages at all. The recording was fully played back before the
client's `TcpStream::connect` completed.

### Cause

`src/control.rs` (replay branch of `run`) spawns `replay_control` immediately.
`src/replay.rs::replay_control` sleeps only for the delta between consecutive
timestamps and broadcasts each line on `event_tx`. A `tokio::sync::broadcast`
channel does not buffer for receivers that have not subscribed yet, so every
message sent before the first client connects is dropped.

### Impact

A replay that is faster than the time it takes a client to connect, which
covers every short recording and the first messages of every long one, is
invisible to the client. The handshake-done flag is set, so the proxy looks
healthy while the client sees an idle telescope.

### Suggested fix

Gate playback on the first client connection (or on an explicit start signal),
or add a configurable pre-roll delay. The test's `late client unexpectedly
received` assert documents the expected post-fix behaviour.

## 2. Replay never updates the telescope status snapshot

### Symptom

After a full replay, `Metrics::telescope_status()` is still the default
(`battery == None`, `is_stacking == false`) even though the recording contains
`PiStatus` and `Stack` events and the live run that produced it populated the
snapshot (`battery == Some(87)`).

### Cause

The live upstream reader calls `m.update_event(event_name, &msg)` for every
event (`src/control.rs:679`). `replay_control` increments `control_rx` and
`control_events` and pushes the ctrl-rx log entry, but never calls
`Metrics::update_event`, so the derived telescope state is never applied.

### Impact

Dashboards, hooks and anything reading `telescope_status()` show a blank
telescope during replay. Event counters match the live run, which hides the
gap.

### Suggested fix

In `replay_control`, after `is_event(&v)`, call `m.update_event(name, &v)`
with the same event-name extraction the live path uses.
