/// Loader tests for `ReplaySession::load` against a vendored real capture
/// (`tests/data/session_s50_fw670/`), so CI always exercises real telescope bytes.
use seestar_proxy::replay::ReplaySession;
use std::path::PathBuf;

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data/session_s50_fw670")
}

/// Count jsonl lines by direction: (telescope, total non-empty).
fn count_lines() -> (usize, usize) {
    let text = std::fs::read_to_string(fixture_dir().join("control.jsonl")).unwrap();
    let mut telescope = 0;
    let mut total = 0;
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        total += 1;
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        if v["direction"] == "telescope" {
            telescope += 1;
        }
    }
    (telescope, total)
}

#[test]
fn replay_session_load_vendored_session_succeeds() {
    assert!(ReplaySession::load(&fixture_dir()).is_ok());
}

#[test]
fn replay_session_load_vendored_session_keeps_only_telescope_messages() {
    let session = ReplaySession::load(&fixture_dir()).unwrap();
    let (telescope, _) = count_lines();
    assert!(telescope > 0);
    assert_eq!(session.control_messages.len(), telescope);
}

#[test]
fn replay_session_load_vendored_session_has_no_frames() {
    let session = ReplaySession::load(&fixture_dir()).unwrap();
    assert!(session.frame_paths.is_empty());
}

#[test]
fn replay_session_load_vendored_manifest_count_matches_jsonl_lines() {
    let text = std::fs::read_to_string(fixture_dir().join("manifest.json")).unwrap();
    let manifest: serde_json::Value = serde_json::from_str(&text).unwrap();
    let (_, total) = count_lines();
    assert_eq!(
        manifest["control_message_count"].as_u64().unwrap() as usize,
        total
    );
}
