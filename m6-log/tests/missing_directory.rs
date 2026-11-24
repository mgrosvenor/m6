//! A file sink refuses a missing directory instead of creating it, and the
//! error names the directory and the remedy.

use m6_log::Sink;

/// A missing directory is refused, the error names the path and the remedy, and
/// nothing is created.
///
/// The wrong outcome this catches: the sink creates the directory, so a typo in
/// an analytics path starts the service and writes where the operator did not
/// choose.
#[test]
fn a_missing_directory_is_refused_and_not_created() {
    let dir = tempfile::tempdir().expect("a directory");
    let parent = dir.path().join("missing").join("logs");
    let path = parent.join("analytics.ndjson");

    let error = match Sink::file(&path) {
        Ok(_) => panic!("a missing directory must be refused"),
        Err(error) => error,
    };
    assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    let message = error.to_string();
    assert!(
        message.contains(&parent.display().to_string()),
        "the error did not name the missing directory: {message}"
    );
    assert!(
        message.contains("create it, owned by"),
        "the error did not name who should create it: {message}"
    );
    assert!(
        message.contains("fix"),
        "the error did not name the remedy: {message}"
    );
    assert!(
        !parent.exists(),
        "the sink created the missing directory {}",
        parent.display()
    );
}

/// A directory that exists opens, and the file appears.
///
/// The wrong outcome this catches: refusing a directory that exists, so a
/// correctly configured service cannot log at all.
#[test]
fn an_existing_directory_is_opened() {
    let dir = tempfile::tempdir().expect("a directory");
    let path = dir.path().join("analytics.ndjson");

    let sink = Sink::file(&path).expect("an existing directory must open");
    assert!(
        path.is_file(),
        "the sink did not create its file {}",
        path.display()
    );
    drop(sink);
}
