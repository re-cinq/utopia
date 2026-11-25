//! Contract for the built-asset part (#614 / #616). No database: `with_static_files` takes no state.
//!
//! This code has broken twice, both times with the same shape: **one path's failure was hidden by another path's success semantics**.
//! The first time, a missing file fell back to the home page (the browser parsed a web page as a module script, leaving a blank screen);
//! the second time, a missing file's 404 was also tagged `immutable`, so "this file does not exist" was
//! cached by the browser for a year, while asset names carry a hash and the next deploy requests exactly the name just cached.
//!
//! So what is pinned here, one assertion at a time, is **the status code, Content-Type and cache directive together**: leave out any one of them,
//! and both of the above slip through.
use super::with_static_files;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use tower::ServiceExt;

/// A minimal dist: one home page and one artifact with a hashed name.
fn dist() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("temp dir");
    std::fs::write(
        dir.path().join("index.html"),
        "<!doctype html><title>u</title>",
    )
    .expect("index");
    std::fs::create_dir(dir.path().join("assets")).expect("assets dir");
    std::fs::write(
        dir.path().join("assets/app-d34db33f.js"),
        "export default 1;",
    )
    .expect("asset");
    dir
}

async fn get(path: &str) -> (StatusCode, Option<String>, Option<String>) {
    let dir = dist();
    let app = with_static_files(Router::new(), dir.path().to_str().expect("utf-8 path"));
    let res = app
        .oneshot(
            Request::builder()
                .uri(path)
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    let head = |name: &str| {
        res.headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    };
    (res.status(), head("content-type"), head("cache-control"))
}

#[tokio::test]
async fn a_hashed_asset_is_served_and_may_be_kept_for_a_year() {
    let (status, ctype, cache) = get("/assets/app-d34db33f.js").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ctype.as_deref(), Some("text/javascript"));
    assert_eq!(
        cache.as_deref(),
        Some("public, max-age=31536000, immutable")
    );
}

/// **A missing artifact is a 404, and that 404 must never be stored.**
/// Returning the home page gives a blank screen (#616); caching the 404 for a year means the next deploy's new hash cannot be fetched on that machine
/// for a year. Both must be guarded, which is why both assertions live here.
#[tokio::test]
async fn a_missing_asset_is_a_404_that_nobody_keeps() {
    let (status, ctype, cache) = get("/assets/app-00000000.js").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_ne!(
        ctype.as_deref(),
        Some("text/html"),
        "a 404 must not be an HTML page"
    );
    assert_eq!(cache.as_deref(), Some("no-cache"));
}

/// A page-route refresh gets the home page: the history fallback is still there; this is the control for the one above.
#[tokio::test]
async fn a_page_route_still_gets_the_index() {
    for path in ["/", "/graph", "/kb/anything/deep"] {
        let (status, ctype, cache) = get(path).await;
        assert_eq!(status, StatusCode::OK, "{path}");
        assert_eq!(ctype.as_deref(), Some("text/html"), "{path}");
        assert_eq!(
            cache.as_deref(),
            Some("no-cache"),
            "{path} 的首页必须回源确认"
        );
    }
}
