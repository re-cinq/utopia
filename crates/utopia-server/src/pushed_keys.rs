//! How documents from push sources (`api`, `statements`) store `external_key` from the identity the pusher supplies,
//! and how it is read back. The push handler, the read-back by `external_id`, and retrieval all use this one (#1271).

/// Sources with push keys: `api` pushes documents, `statements` pushes statements (0054). Both kinds share one key scheme for
/// generation, viewing and rotation
pub fn is_push_kind(kind: &str) -> bool {
    matches!(kind, "api" | "statements")
}

/// The `external_key` a push source files a pushed identity under (`api:…`, `statements:…`).
/// The push handlers write it and the read by `external_id` (#1276) looks it up, so the two
/// cannot drift apart.
pub fn pushed_key(source: &utopia_core::models::Source, identity: &str) -> String {
    format!("{}:{identity}", source.kind)
}

/// The reverse of `pushed_key`: documents from push sources hand back the identity the pusher supplied; other sources have none (#1271)
pub fn pushed_identity<'k>(kind: &str, key: &'k str) -> Option<&'k str> {
    if !is_push_kind(kind) {
        return None;
    }
    key.strip_prefix(kind)?.strip_prefix(':')
}
