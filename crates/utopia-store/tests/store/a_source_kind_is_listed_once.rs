//! #247: a source's kind is defined in one place only, and the front and back ends are checked against the table.
//!
//! The back-end `SourceKind` (utopia-core) enumerates one list in one enum, used in two places: the creation allowlist and sync dispatch
//! (the latter is matched exhaustively on the enum, so the compiler guarantees that adding a kind forces a decision about how it syncs). The front end's copy lives in
//! `web/src/sourceKinds.ts`, and this test reads it out and compares it with the enum — previously each side was handwritten, and five
//! connectors reached the UI and sync but not the creation allowlist, so they could be selected in the UI and creation failed with
//! "kind must be one of…". The kind of drift that neither unit tests nor tsc can see is visible here.
//!
//! No database needed.

use std::path::Path;
use utopia_core::models::SourceKind;

/// Reads out, in order, the literals inside the quotes of `CREATABLE_SOURCE_KINDS = [ "…", … ] as const`
fn frontend_kinds(src: &str) -> Vec<String> {
    let start = src
        .find("CREATABLE_SOURCE_KINDS = [")
        .expect("web/src/sourceKinds.ts declares CREATABLE_SOURCE_KINDS");
    let body = &src[start..];
    let end = body.find(']').expect("the array closes");
    body[..end]
        .split('"')
        .skip(1)
        .step_by(2)
        .map(str::to_string)
        .collect()
}

#[test]
fn the_frontend_list_matches_the_backend_enum() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../web/src/sourceKinds.ts");
    let src =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let frontend = frontend_kinds(&src);
    let backend: Vec<String> = SourceKind::creatable()
        .map(|k| k.as_str().to_string())
        .collect();
    assert_eq!(
        frontend, backend,
        "web/src/sourceKinds.ts and utopia_core::models::SourceKind list different kinds (order matters: it is the dialog's order)"
    );
}

#[test]
fn every_kind_round_trips_through_its_string() {
    for k in SourceKind::all() {
        assert_eq!(SourceKind::parse(k.as_str()), Some(k), "{k:?}");
    }
    assert_eq!(SourceKind::parse("watch_folder"), None);
    assert!(!SourceKind::Memory.creatable_by_hand());
    assert!(!SourceKind::Upload.creatable_by_hand());
    assert!(SourceKind::S3.creatable_by_hand());
}
