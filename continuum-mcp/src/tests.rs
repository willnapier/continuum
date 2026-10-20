use super::*;
use std::{fs, os::unix::fs::symlink};

fn fixture() -> (tempfile::TempDir, Archive, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("archive");
    let session = root.join("codex/2026-10-20/synthetic");
    fs::create_dir_all(&session).unwrap();
    let message = Message {
        id: 1,
        role: "user".into(),
        content: format!("Synthetic {}", "é".repeat(160)),
        timestamp: "2026-10-20T12:00:00Z".into(),
    };
    fs::write(
        session.join("messages.jsonl"),
        serde_json::to_string(&message).unwrap(),
    )
    .unwrap();
    let archive = Archive::open(&root).unwrap();
    (temp, archive, session)
}

#[test]
fn positive_search_date_prefix_unicode_and_empty_results() {
    let (_temp, a, _) = fixture();
    let hits = a
        .search("SYNTHETIC.*é", Some("codex"), Some("2026-10"), 10)
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].preview.chars().count(), 153);
    assert!(a.search("absent", None, None, 10).unwrap().is_empty());
    assert_eq!(a.timeline(None, None, 10).unwrap().len(), 1);
    assert_eq!(
        a.conversation("codex", "2026-10-20", "synthetic")
            .unwrap()
            .0
            .len(),
        1
    );
}

#[test]
fn invalid_queries_and_filters_are_errors() {
    let (_temp, a, _) = fixture();
    assert!(a.search("[", None, None, 10).is_err());
    assert!(a.search("x", None, Some("2026-99"), 10).is_err());
    assert!(a.search("x", Some(".."), None, 10).is_err());
    for n in [0, 1001] {
        assert!(a.timeline(None, None, n).is_err());
    }
    for bad in ["", ".", "..", "/tmp", "a/b", "a\\b"] {
        assert!(a.conversation(bad, "2026-10-20", "synthetic").is_err());
        assert!(a.conversation("codex", "2026-10-20", bad).is_err());
    }
}

#[test]
fn corrupted_or_missing_messages_never_become_empty_success() {
    let (_temp, a, session) = fixture();
    let path = session.join("messages.jsonl");
    let good = fs::read_to_string(&path).unwrap();
    fs::write(&path, format!("{good}\nmalformed\n{good}\n")).unwrap();
    assert!(a.conversation("codex", "2026-10-20", "synthetic").is_err());
    assert!(a.search("absent", None, None, 1).is_err());
    assert!(a.timeline(None, None, 1).is_err());
    fs::remove_file(path).unwrap();
    assert!(a.timeline(None, None, 10).is_err());
}

#[test]
fn optional_metadata_can_be_absent_but_not_corrupt() {
    let (_temp, a, session) = fixture();
    assert!(a
        .conversation("codex", "2026-10-20", "synthetic")
        .unwrap()
        .1
        .is_none());
    fs::write(session.join("session.json"), "{broken").unwrap();
    assert!(a.conversation("codex", "2026-10-20", "synthetic").is_err());
}

#[test]
fn symlink_files_and_directory_components_are_refused() {
    let (temp, a, session) = fixture();
    let outside = temp.path().join("outside");
    fs::rename(&session, &outside).unwrap();
    symlink(&outside, &session).unwrap();
    assert!(a.conversation("codex", "2026-10-20", "synthetic").is_err());
    assert!(a.search("Synthetic", None, None, 10).is_err());
    fs::remove_file(&session).unwrap();
    fs::create_dir(&session).unwrap();
    symlink(
        outside.join("messages.jsonl"),
        session.join("messages.jsonl"),
    )
    .unwrap();
    assert!(a.conversation("codex", "2026-10-20", "synthetic").is_err());
    assert!(a
        .validate_source_path(&session.join("messages.jsonl"))
        .is_err());
}

#[test]
fn semantic_results_must_reference_valid_archive_files() {
    let (temp, a, session) = fixture();
    let response = |p: &Path| {
        serde_json::json!([{"path":p,"similarity":0.9,"snippet":"synthetic"}]).to_string()
    };
    assert_eq!(
        a.parse_semantic(&response(&session.join("messages.jsonl")))
            .unwrap()
            .len(),
        1
    );
    assert!(a
        .parse_semantic(&response(&temp.path().join("outside/messages.jsonl")))
        .is_err());
    assert!(a
        .parse_semantic(&response(&session.join("../messages.jsonl")))
        .is_err());
    assert!(a.parse_semantic("human formatted output").is_err());
}

#[test]
fn existing_indexes_may_reference_session_metadata() {
    let (_temp, a, session) = fixture();
    let path = session.join("session.json");
    fs::write(&path, r#"{"id":"synthetic","assistant":"codex"}"#).unwrap();
    a.validate_source_path(&path).unwrap();
    assert!(a.source_text(&path).unwrap().contains("synthetic"));
    let response = serde_json::json!([{"path":path,"similarity":0.9}]).to_string();
    assert_eq!(
        a.parse_semantic(&response).unwrap()[0].filename,
        "session.json"
    );
    fs::write(path, "{broken").unwrap();
    assert!(a.parse_semantic(&response).is_err());
}
