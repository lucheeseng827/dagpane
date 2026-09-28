//! `FileSource`, and the two questions it answers about a file on disk.

use std::sync::Arc;

use dagpane_connect::file::FileFormat;
use dagpane_connect::{FileSource, Source, SourceError};
use dagpane_core::frame::{Frame, FrameBuilder, TableBuilder};
use dagpane_core::ColumnType;

fn builder() -> Box<dyn FrameBuilder> {
    Box::new(TableBuilder::new())
}

fn write(dir: &std::path::Path, name: &str, text: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, text).unwrap();
    path
}

const SALES: &str = "region,amount,ok\nnorth,10.5,true\nsouth,,false\n";

#[test]
fn a_csv_loads_with_its_types_decided_by_its_values() {
    let dir = tempfile::tempdir().unwrap();
    let source = FileSource::new(write(dir.path(), "s.csv", SALES), FileFormat::Csv);

    assert_eq!(
        source.schema().unwrap(),
        vec![
            ("region".to_string(), ColumnType::Text),
            ("amount".to_string(), ColumnType::Float),
            ("ok".to_string(), ColumnType::Bool),
        ]
    );

    let frame: Arc<dyn Frame> = source.load(builder()).unwrap();
    assert_eq!(frame.rows(), 2);
    assert_eq!(frame.width(), 3);
    // An empty field is a null, never a zero — the distinction an aggregate depends on.
    assert!(frame.is_null(1, 1), "the blank amount became a value");
}

#[test]
fn an_extension_this_build_does_not_read_is_refused_rather_than_guessed() {
    let dir = tempfile::tempdir().unwrap();
    let parquet = write(dir.path(), "s.parquet", "PAR1");

    let err = FileSource::of_path(&parquet).unwrap_err();
    assert!(matches!(err, SourceError::Misconfigured { .. }), "{err:?}");
    assert!(
        !err.is_retryable(),
        "a format this build cannot read is not fixed by waiting"
    );

    // And the extension is what decides, not the content: a `.csv` is read as CSV.
    assert!(FileSource::of_path(write(dir.path(), "s.csv", SALES)).is_ok());
}

#[test]
fn a_missing_file_is_unreachable_and_a_broken_one_is_unreadable() {
    let dir = tempfile::tempdir().unwrap();

    // Missing: a retry may find it — a file being rewritten is briefly absent, and a
    // scheduler that gives up on that is worse than one that waits.
    let missing = FileSource::new(dir.path().join("nope.csv"), FileFormat::Csv);
    let err = missing.load(builder()).unwrap_err();
    assert!(matches!(err, SourceError::Unreachable { .. }), "{err:?}");
    assert!(err.is_retryable());
    assert!(missing.version().unwrap_err().is_retryable());

    // Present and wrong: waiting will not help.
    let ragged = FileSource::new(write(dir.path(), "r.csv", "a,b\n1,2\n3\n"), FileFormat::Csv);
    let err = ragged.load(builder()).unwrap_err();
    assert!(matches!(err, SourceError::Unreadable { .. }), "{err:?}");
    assert!(!err.is_retryable());
    // The message names the line, because a CSV nobody can find the bad line in is one
    // nobody fixes.
    assert!(err.to_string().contains("line 3"), "{err}");
}

#[test]
fn a_version_is_stable_while_the_file_is_and_moves_when_it_is_rewritten() {
    let dir = tempfile::tempdir().unwrap();
    let path = write(dir.path(), "s.csv", SALES);
    let source = FileSource::new(&path, FileFormat::Csv);

    let first = source.version().unwrap();
    assert_eq!(first, source.version().unwrap(), "asking twice changed it");

    // A different length: caught by either half of the pair.
    std::fs::write(&path, format!("{SALES}east,1.0,true\n")).unwrap();
    let grown = source.version().unwrap();
    assert_ne!(first, grown);

    // The same length, different content. This is the case a length-only check misses, and
    // it is why the modification time is in there — the two writes are far enough apart in
    // nanoseconds for the timestamp to have moved.
    std::fs::write(&path, format!("{SALES}west,1.0,true\n")).unwrap();
    assert_ne!(
        grown,
        source.version().unwrap(),
        "same length, different rows"
    );
}

#[test]
fn describe_names_the_format_and_the_path_and_appears_in_every_error() {
    let dir = tempfile::tempdir().unwrap();
    let source = FileSource::new(dir.path().join("gone.csv"), FileFormat::Csv);

    assert!(source.describe().starts_with("csv file "));
    assert!(source.describe().ends_with("gone.csv"));
    assert_eq!(
        source.load(builder()).unwrap_err().source_name(),
        source.describe(),
        "an error that does not say which source it is about is one somebody has to reproduce"
    );
}
