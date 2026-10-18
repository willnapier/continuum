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
