use super::*;
use std::fs;

const ID: &str = "99999999-0000-4000-8000-000000000001";

fn fixture() -> (tempfile::TempDir, PlainTextWriter, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let writer = PlainTextWriter::with_base_dir(temp.path().join("out"))
        .with_nosave_dir(temp.path().join("registry"));
    let session = temp.path().join("native").join(ID);
    fs::create_dir_all(&session).unwrap();
    let path = session.join("chat_history.jsonl");
    fs::write(&path, concat!(
        "{\"type\":\"user\",\"content\":[{\"text\":\"synthetic user canary\"}]}\n",
        "{\"type\":\"assistant\",\"content\":\"synthetic reply\",\"reasoning\":\"private reasoning canary\"}\n"
    )).unwrap();
    fs::write(
        session.join("summary.json"),
        "{\"created_at\":\"2026-10-18T12:00:00Z\"}",
    )
    .unwrap();
    (temp, writer, path)
}

#[test]
fn ordinary_capture_is_idempotent_and_excludes_reasoning() {
    let (_temp, writer, path) = fixture();
    assert!(matches!(
        import_grok_session(&writer, &path).unwrap(),
        Imported::Stored(2)
    ));
    let messages = writer
        .base_dir()
        .join(SOURCE)
        .join("2026-10-18")
        .join(ID)
        .join("messages.jsonl");
    let first = fs::read_to_string(&messages).unwrap();
    assert!(first.contains("synthetic user canary"));
    assert!(first.contains("synthetic reply"));
    assert!(!first.contains("private reasoning canary"));
    let before = fs::metadata(&messages).unwrap().modified().unwrap();
    assert!(matches!(
        import_grok_session(&writer, &path).unwrap(),
        Imported::Stored(2)
    ));
    assert_eq!(fs::read_to_string(&messages).unwrap(), first);
    assert_eq!(fs::metadata(messages).unwrap().modified().unwrap(), before);
}

#[test]
fn a_registered_session_is_skipped_before_its_transcript_is_read() {
    let (_temp, writer, path) = fixture();
    import::register_nosave(&writer, ID).unwrap();
    fs::remove_file(&path).unwrap();
    assert!(matches!(
        import_grok_session(&writer, &path).unwrap(),
        Imported::NoSave
    ));
    assert!(!writer.base_dir().exists());
}

#[test]
fn a_later_plain_import_still_obeys_the_marker() {
    let (_temp, writer, path) = fixture();
    import::register_nosave(&writer, ID).unwrap();
    for _ in 0..2 {
        assert!(matches!(
            import_grok_session(&writer, &path).unwrap(),
            Imported::NoSave
        ));
    }
    assert!(!writer.base_dir().exists());
}

#[test]
fn a_registry_that_cannot_be_inspected_fails_closed() {
    let (temp, writer, path) = fixture();
    fs::write(temp.path().join("registry"), "not a directory").unwrap();
    assert!(import_grok_session(&writer, &path).is_err());
    assert!(!writer.base_dir().exists());
}

#[test]
fn marking_a_previous_capture_prevents_new_writes_without_deleting_history() {
    let (_temp, writer, path) = fixture();
    import_grok_session(&writer, &path).unwrap();
    let messages = writer
        .base_dir()
        .join(SOURCE)
        .join("2026-10-18")
        .join(ID)
        .join("messages.jsonl");
    let before = fs::read(&messages).unwrap();
    import::register_nosave(&writer, ID).unwrap();
    fs::write(
        &path,
        "{\"type\":\"user\",\"content\":[{\"text\":\"later private turn\"}]}\n",
    )
    .unwrap();
    assert!(matches!(
        import_grok_session(&writer, &path).unwrap(),
        Imported::NoSave
    ));
    assert_eq!(fs::read(messages).unwrap(), before);
}

#[test]
fn no_save_never_marks_an_inferred_latest_or_all_sessions() {
    assert!(validate_target(&Target::Latest, true).is_err());
    assert!(validate_target(&Target::All, true).is_err());
    assert!(validate_target(&Target::Id("../another".into()), true).is_err());
    assert!(validate_target(&Target::Id(ID.into()), true).is_ok());
    assert!(validate_target(&Target::All, false).is_ok());
}

#[test]
fn conflicting_explicit_and_inherited_ids_cannot_register_the_wrong_session() {
    assert!(select_target(&[ID.into()], Some("parent-session"), true).is_err());
    assert!(select_target(&[ID.into()], Some(ID), true).is_ok());
    assert!(select_target(&[], Some(ID), true).is_ok());
    assert!(select_target(&[], None, true).is_err());
    assert!(select_target(&["--all".into()], Some(ID), true).is_err());
    assert!(select_target(&["--latest".into()], Some(ID), true).is_err());
    // Explicit manual imports in an ordinary Grok session remain possible.
    assert!(select_target(&[ID.into()], Some("parent-session"), false).is_ok());
}

#[test]
fn mixed_batch_commits_allowed_records_but_reports_genuine_errors() {
    let (temp, writer, allowed) = fixture();
    let protected = temp.path().join("native/protected/chat_history.jsonl");
    let broken = temp.path().join("native/broken/chat_history.jsonl");
    import::register_nosave(&writer, "protected").unwrap();
    // A protected missing file is never read; an ordinary missing file errors.
    let error = import_targets(&writer, &[broken, protected, allowed]).unwrap_err();
    assert!(error.to_string().contains("1 Grok import(s) failed"));
    assert!(writer
        .base_dir()
        .join(SOURCE)
        .join("2026-10-18")
        .join(ID)
        .join("messages.jsonl")
        .is_file());
    assert!(!writer
        .base_dir()
        .join(SOURCE)
        .join("2026-10-18/protected")
        .exists());
}

#[test]
fn malformed_transcripts_preserve_existing_capture() {
    for damage in ["interior", "trailing"] {
        let (_temp, writer, path) = fixture();
        import_grok_session(&writer, &path).unwrap();
        let captured = writer.base_dir().join(SOURCE).join("2026-10-18").join(ID);
        let before_messages = fs::read(captured.join("messages.jsonl")).unwrap();
        let before_metadata = fs::read(captured.join("session.json")).unwrap();
        let good = fs::read_to_string(&path).unwrap();
        let bad = if damage == "interior" {
            format!("{good}malformed\n{good}")
        } else {
            format!("{good}{{truncated")
        };
        fs::write(&path, bad).unwrap();
        assert!(import_grok_session(&writer, &path).is_err());
        assert_eq!(
            fs::read(captured.join("messages.jsonl")).unwrap(),
            before_messages
        );
        assert_eq!(
            fs::read(captured.join("session.json")).unwrap(),
            before_metadata
        );
    }
}

#[test]
fn fresh_corruption_creates_no_capture_and_blank_lines_are_allowed() {
    let (_temp, writer, path) = fixture();
    let good = fs::read_to_string(&path).unwrap();
    fs::write(&path, format!("{good}{{truncated")).unwrap();
    assert!(import_grok_session(&writer, &path).is_err());
    // Taking the import lock may create the root, but no session is published.
    assert!(!writer.base_dir().join(SOURCE).exists());
    fs::write(&path, format!("\n \t\n{good}\n")).unwrap();
    assert!(matches!(
        import_grok_session(&writer, &path).unwrap(),
        Imported::Stored(2)
    ));
}

#[test]
fn unknown_event_kinds_do_not_block_ordinary_capture() {
    let (_temp, writer, path) = fixture();
    let good = fs::read_to_string(&path).unwrap();
    fs::write(
        &path,
        format!("{{\"type\":\"future_event\",\"payload\":{{\"synthetic\":true}}}}\n{good}"),
    )
    .unwrap();
    assert!(matches!(
        import_grok_session(&writer, &path).unwrap(),
        Imported::Stored(2)
    ));
}

#[test]
fn interrupted_json_tail_recovers_complete_prefix_but_terminated_damage_refuses() {
    let (_temp, writer, path) = fixture();
    let good = fs::read_to_string(&path).unwrap();
    let partial = "{\"type\":\"assistant\",\"content\":\"unfinished";
    fs::write(&path, format!("{good}{partial}")).unwrap();
    assert!(matches!(
        import_grok_session(&writer, &path).unwrap(),
        Imported::Stored(2)
    ));
    let captured = writer.base_dir().join(SOURCE).join("2026-10-18").join(ID);
    let before = fs::read(captured.join("messages.jsonl")).unwrap();
    fs::write(&path, format!("{good}{partial}\n")).unwrap();
    assert!(import_grok_session(&writer, &path).is_err());
    assert_eq!(fs::read(captured.join("messages.jsonl")).unwrap(), before);
}
