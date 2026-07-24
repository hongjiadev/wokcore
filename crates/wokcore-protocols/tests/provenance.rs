use std::{fs, path::PathBuf};

#[test]
fn migrated_protocol_notice_retains_source_mit_attribution() {
    let repository = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let notice = fs::read_to_string(repository.join("NOTICE.md")).unwrap();
    let migration = fs::read_to_string(repository.join("MIGRATION.md")).unwrap();

    assert!(notice.contains("Copyright (c) 2026 WokRouter contributors"));
    assert!(notice.contains("source MIT notice"));
    assert!(notice.contains("domain types, protocol source, and fixture files"));
    assert!(notice.contains("`MIT OR Apache-2.0`"));
    assert!(migration.contains(
        "direct WokRouter source copyright and permission notice is retained in `NOTICE.md`"
    ));
    assert!(migration.contains("OpenCodex MIT attribution remains listed in `NOTICE.md`"));
    assert!(repository.join("LICENSE-MIT").is_file());
}
