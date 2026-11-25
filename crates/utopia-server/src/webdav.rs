//! WebDAV source: Nextcloud, ownCloud, Jianguoyun, Synology, Apache mod_dav,
//! rclone serve webdav, and any cloud drive that speaks this protocol.
//!
//! Judged by the four criteria of [0013](../../docs/decisions/0013-a-source-should-hand-over-its-history.md),
//! it is in the same class as object storage: it has `getlastmodified`, identity is the path,
//! and plenty of enterprise documents really do live on network drives. But "whether it overturns itself" can only be seen from the difference between two syncs;
//! WebDAV's version-control extension (RFC 3253) is almost never implemented on servers.
//!
//! **No WebDAV client library.** The protocol needs very little: one `PROPFIND` for the listing,
//! one `GET` for content, and the responses are XML. `reqwest` and `quick-xml` are already in the tree, so
//! the total is zero new dependencies; existing dav client crates either wrap their own HTTP stack,
//! or pull in all of RFC 4918 (locks, property writes, versions), none of which we use.

use anyhow::Context as _;
use chrono::{DateTime, Utc};
use quick_xml::events::Event;
use quick_xml::Reader;

/// Maximum number of files ingested in one sync. The reasoning is the same as for object storage: a drive can hold
/// a hundred thousand files, and ingestion is irreversible.
const MAX_FILES_PER_SYNC: usize = 2_000;

/// Size cap for a single file.
const MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;

/// Cap on directory recursion depth. **Not about depth, about cycles**: some servers expose symbolic links
/// or shared directories as paths that can point back to themselves, and `Depth: infinity` on such a directory
/// never returns. Walking level by level with a cap is safer than trusting the server.
const MAX_DEPTH: usize = 8;

/// One remote file.
pub struct RemoteFile {
    /// `webdav://host/path`: the `ingest_item` external_key convention is a URI shape
    pub external_key: String,
    pub filename: String,
    pub bytes: Vec<u8>,
    pub last_modified: Option<DateTime<Utc>>,
}

/// One entry returned by `PROPFIND`.
#[derive(Debug, PartialEq)]
struct Entry {
    /// XML references are decoded, but URL percent escapes remain intact.
    href: String,
    is_dir: bool,
    len: u64,
    modified: Option<DateTime<Utc>>,
}

/// Parses a `multistatus` response.
///
/// **Matches the local name only, not the prefix.** A server may use `D:`, `d:`, `ns0:`, or no prefix at all:
/// the RFC allows any prefix to be bound to the `DAV:` namespace. A parser that hard-matches `d:response`
/// reads nothing at all from a different server, and the symptom is "sync succeeded,
/// zero files".
///
/// **The directory test is whether `<collection/>` is present, not whether the href ends in `/`.** The latter is a convention,
/// not the spec, and rclone and Nextcloud disagree on exactly this point.
fn parse_multistatus(xml: &str) -> anyhow::Result<Vec<Entry>> {
    let mut r = Reader::from_str(xml);

    let mut out = Vec::new();
    let mut cur: Option<Entry> = None;
    let mut field = String::new();
    let mut text = String::new();
    let mut buf = Vec::new();

    loop {
        match r.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let name = local_name(e.name().into_inner());
                match name.as_str() {
                    "response" => {
                        cur = Some(Entry {
                            href: String::new(),
                            is_dir: false,
                            len: 0,
                            modified: None,
                        });
                    }
                    "collection" => {
                        if let Some(c) = cur.as_mut() {
                            c.is_dir = true;
                        }
                    }
                    other => {
                        field = other.to_string();
                        text.clear();
                    }
                }
            }
            // `<collection/>` is usually a self-closing tag, so it arrives as Empty rather than Start:
            // without this case, every directory would be GET as a file
            Ok(Event::Empty(e)) => {
                if local_name(e.name().into_inner()) == "collection" {
                    if let Some(c) = cur.as_mut() {
                        c.is_dir = true;
                    }
                }
            }
            // XML references and CDATA arrive as separate events. Replacing the
            // field at each Text event truncates an href such as A&amp;B.txt to B.txt.
            Ok(Event::Text(t)) => {
                text.push_str(&t.xml_content(quick_xml::XmlVersion::Implicit1_0));
            }
            Ok(Event::CData(t)) => {
                text.push_str(&t.xml_content(quick_xml::XmlVersion::Implicit1_0));
            }
            Ok(Event::GeneralRef(e))
                if matches!(
                    field.as_str(),
                    "href" | "getcontentlength" | "getlastmodified"
                ) =>
            {
                let reference = format!("&{};", e.into_inner());
                // An unrecognised entity only affects its own entry: keep it as is. If the whole PROPFIND errored
                // on one `&nbsp;`, no file in a directory could sync
                match quick_xml::escape::unescape(&reference) {
                    Ok(v) => text.push_str(&v),
                    Err(_) => text.push_str(&reference),
                }
            }
            Ok(Event::End(e)) => {
                let name = local_name(e.name().into_inner());
                // **Empty values are not written back**: a Nextcloud / SabreDAV response often carries two propstat blocks:
                // the 200 block gives the value, and the 404 block lists the unfound properties again. The paired form
                // (`<getcontentlength></getcontentlength>`) opens a field and closes it again,
                // so writing it as is would overwrite a good value with empty. `fetch` skips zero-length entries, so the file still drops out of sync,
                // the very symptom this cut is meant to fix, in a different guise
                if name == field && !text.trim().is_empty() {
                    if let Some(c) = cur.as_mut() {
                        match field.as_str() {
                            "href" => c.href = text.trim().to_string(),
                            "getcontentlength" => c.len = text.trim().parse().unwrap_or(0),
                            "getlastmodified" => {
                                // RFC 1123, `Wed, 02 Sep 2026 15:04:05 GMT`
                                c.modified = DateTime::parse_from_rfc2822(text.trim())
                                    .ok()
                                    .map(|d| d.with_timezone(&Utc));
                            }
                            _ => {}
                        }
                    }
                }
                if name == "response" {
                    if let Some(c) = cur.take() {
                        out.push(c);
                    }
                }
                field.clear();
                text.clear();
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(anyhow::anyhow!("PROPFIND response parse failed: {e}")),
            _ => {}
        }
        buf.clear();
    }
    Ok(out)
}

/// Gets the local name of a tag, dropping the namespace prefix and lowercasing it.
///
/// The spec says element names are case-sensitive, but in practice some servers write `getLastModified`.
/// The prefix is split off too: `D:`, `d:`, and `ns0:` are all valid, since the RFC allows any prefix bound to `DAV:`.
fn local_name(raw: &str) -> String {
    raw.rsplit(':').next().unwrap_or(raw).to_ascii_lowercase()
}

/// Percent-escapes in hrefs.
///
/// Decode it ourselves rather than pulling in `percent-encoding`: only decoding is needed here, and decoding is ten-odd lines.
/// Invalid sequences are left as is, without guessing; decoding is used only for existing document identity and file names.
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let hex = std::str::from_utf8(&b[i + 1..i + 3]).ok();
            if let Some(v) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Walks directories level by level, fetching all files.
///
/// **No `Depth: infinity`.** The spec lets a server refuse it (`403 Propfind-Finite-Depth`),
/// and Nextcloud and Apache mod_dav refuse it by default; a server that accepts it would
/// dump tens of MB of XML in one go on a large directory. Capping each level avoids both problems.
pub async fn fetch(
    http: &reqwest::Client,
    base: &str,
    root: &str,
    auth: Option<(&str, &str)>,
) -> anyhow::Result<(Vec<RemoteFile>, bool)> {
    let host = reqwest::Url::parse(base)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_else(|| "webdav".into());
    let base = base.trim_end_matches('/').to_string();

    let mut out = Vec::new();
    let mut truncated = false;
    let root = normalize(root);
    let mut queue = vec![(root.clone(), root, 0usize)];
    let mut unreadable = 0usize;

    while let Some((dir, request_dir, depth)) = queue.pop() {
        if depth > MAX_DEPTH {
            tracing::warn!(%dir, "directory too deep, going no further");
            continue;
        }
        let url = format!("{base}{request_dir}");
        let mut req = http
            .request(reqwest::Method::from_bytes(b"PROPFIND").unwrap(), &url)
            .header("Depth", "1")
            .header("Content-Type", "application/xml");
        if let Some((u, p)) = auth {
            req = req.basic_auth(u, Some(p));
        }
        let resp = req
            .send()
            .await
            .with_context(|| format!("PROPFIND {url}"))?;
        if !resp.status().is_success() {
            anyhow::bail!("PROPFIND {url} returned {}", resp.status());
        }
        let xml = resp.text().await.context("reading PROPFIND response")?;

        for e in parse_multistatus(&xml)? {
            // Send the original escaped path: decoding %23/%3F turns filename
            // bytes into URL syntax, and %2523 would fetch a file named with '#'.
            let path = document_path(&e.href, &base);
            let request_path = normalize(&strip_base(&e.href, &base));
            // The server lists the queried directory itself as well; skip it, or the walk would spin forever
            if path == dir || request_path == request_dir {
                continue;
            }
            if e.is_dir {
                queue.push((path, request_path, depth + 1));
                continue;
            }
            if e.len == 0 || e.len > MAX_FILE_BYTES {
                continue;
            }
            if out.len() >= MAX_FILES_PER_SYNC {
                truncated = true;
                break;
            }

            // A file that cannot be fetched should not take the whole sync down with it: the same judgment as on the object storage side
            let mut g = http.get(format!("{base}{request_path}"));
            if let Some((u, p)) = auth {
                g = g.basic_auth(u, Some(p));
            }
            let got = async {
                let r = g.send().await?;
                if !r.status().is_success() {
                    anyhow::bail!("{}", r.status());
                }
                Ok::<_, anyhow::Error>(r.bytes().await?)
            }
            .await;
            let bytes = match got {
                Ok(b) => b,
                Err(err) => {
                    unreadable += 1;
                    tracing::warn!(%path, error = %err, "file could not be fetched, skipping");
                    continue;
                }
            };

            out.push(RemoteFile {
                external_key: format!("webdav://{host}{path}"),
                filename: path.rsplit('/').next().unwrap_or(&path).to_string(),
                bytes: bytes.to_vec(),
                last_modified: e.modified,
            });
        }
        if truncated {
            break;
        }
    }
    if unreadable > 0 {
        tracing::warn!(unreadable, "some files could not be fetched");
    }
    Ok((out, truncated))
}

/// Normalises paths to the shape `/a/b`: a leading slash, no trailing slash (except the root).
fn normalize(p: &str) -> String {
    let t = p.trim();
    let t = t.strip_suffix('/').unwrap_or(t);
    if t.is_empty() {
        "/".into()
    } else if t.starts_with('/') {
        t.into()
    } else {
        format!("/{t}")
    }
}

fn document_path(href: &str, base: &str) -> String {
    let decoded = percent_decode(href);
    if !decoded.starts_with(base) && (href.starts_with("http://") || href.starts_with("https://")) {
        if let Ok(mut url) = reqwest::Url::parse(href) {
            // Proxy-rewritten absolute hrefs historically produce URL-serialized
            // keys. Keep that shape, but decode after parsing and re-encode each
            // segment so '#'/'?' cannot truncate keys and literal '%' stays distinct.
            if let Some(segments) = url
                .path_segments()
                .map(|parts| parts.map(percent_decode).collect::<Vec<_>>())
            {
                if let Ok(mut path) = url.path_segments_mut() {
                    path.clear().extend(segments);
                }
                return normalize(url.path());
            }
        }
    }
    normalize(&strip_base(&decoded, base))
}

/// An href may be an absolute URL or just a path; both are valid, and each server writes its own.
fn strip_base(href: &str, base: &str) -> String {
    if let Some(rest) = href
        .strip_prefix(base)
        .filter(|rest| rest.is_empty() || rest.starts_with('/'))
    {
        return rest.to_string();
    }
    let path = reqwest::Url::parse(href)
        .ok()
        .filter(|url| matches!(url.scheme(), "http" | "https"))
        .map(|url| url.path().to_string())
        .unwrap_or_else(|| href.to_string());
    // Nextcloud returns origin-relative hrefs that already include the base's
    // path. Strip whole path segments before fetch appends the configured base.
    if let Ok(url) = reqwest::Url::parse(base) {
        if let Some(rest) = path
            .strip_prefix(url.path().trim_end_matches('/'))
            .filter(|rest| rest.is_empty() || rest.starts_with('/'))
        {
            return rest.to_string();
        }
    }
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The same property declared twice: the 200 block gives the value, and the 404 block lists the unfound one again.
    /// The empty occurrence must not overwrite the good value: after overwriting, the length is 0, and `fetch` would skip the file
    #[test]
    fn an_empty_redeclaration_does_not_clobber_a_property() {
        let paired = r#"<multistatus xmlns="DAV:"><response>
            <href>/docs/a.txt</href>
            <propstat><prop><getcontentlength>5</getcontentlength>
              <getlastmodified>Wed, 02 Sep 2026 15:04:05 GMT</getlastmodified></prop>
              <status>HTTP/1.1 200 OK</status></propstat>
            <propstat><prop><getcontentlength></getcontentlength>
              <getlastmodified></getlastmodified></prop>
              <status>HTTP/1.1 404 Not Found</status></propstat>
            </response></multistatus>"#;
        for xml in [
            paired.to_string(),
            // The self-closing form goes through Event::Empty and does not open a field anyway; both forms must pass
            paired
                .replace(
                    "<getcontentlength></getcontentlength>",
                    "<getcontentlength/>",
                )
                .replace("<getlastmodified></getlastmodified>", "<getlastmodified/>"),
        ] {
            let e = parse_multistatus(&xml).unwrap();
            assert_eq!(e.len(), 1);
            assert_eq!(
                e[0].len, 5,
                "the empty run overwrote the length with 0: {xml}"
            );
            assert!(e[0].modified.is_some(), "the empty run wiped out the time");
        }
    }

    /// An unrecognised entity only affects its own entry, without bringing down the whole listing
    #[test]
    fn an_unknown_entity_does_not_abort_the_listing() {
        let xml = r#"<multistatus xmlns="DAV:">
            <response><href>/docs/a&nbsp;b.txt</href><propstat><prop>
              <getcontentlength>5</getcontentlength></prop></propstat></response>
            <response><href>/docs/c.txt</href><propstat><prop>
              <getcontentlength>7</getcontentlength></prop></propstat></response>
            </multistatus>"#;
        let e = parse_multistatus(xml).expect("one odd entity should not fail the whole listing");
        assert_eq!(e.len(), 2, "the other one was lost too");
        assert_eq!(e[1].href, "/docs/c.txt");
    }

    #[test]
    fn xml_text_fragments_keep_the_complete_href_and_properties() {
        for href in [
            "/docs/A&amp;B.txt",
            "/docs/A&#38;B.txt",
            "/docs/A&#x26;B.txt",
            "<![CDATA[/docs/A&B.txt]]>",
            "/docs/A<![CDATA[&]]>B.txt",
        ] {
            let xml = format!(
                r#"<multistatus xmlns="DAV:"><response>
                <href>{href}</href><propstat><prop>
                <getcontentlength>&#53;</getcontentlength>
                <getlastmodified>Wed, 02 Sep 2026 15:04:05 GMT</getlastmodified>
                </prop></propstat></response></multistatus>"#
            );
            let entries = parse_multistatus(&xml).unwrap();
            assert_eq!(entries[0].href, "/docs/A&B.txt", "{href}");
            assert_eq!(entries[0].len, 5);
            assert!(entries[0].modified.is_some());
        }
    }

    #[tokio::test]
    async fn an_xml_escaped_filename_is_fetched_from_its_full_path() {
        use axum::{
            routing::{any, get},
            Router,
        };
        const XML: &str = r#"<multistatus xmlns="DAV:"><response>
            <href>/docs/A&amp;B.txt</href><propstat><prop>
            <getcontentlength>5</getcontentlength>
            </prop></propstat></response></multistatus>"#;
        let app = Router::new()
            .route(
                "/docs",
                any(|| async { (axum::http::StatusCode::MULTI_STATUS, XML) }),
            )
            .route("/docs/A&B.txt", get(|| async { "hello" }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let result = fetch(&reqwest::Client::new(), &base, "/docs", None).await;
        server.abort();
        let (files, truncated) = result.unwrap();
        assert!(!truncated);
        assert_eq!(
            files.len(),
            1,
            "the escaped href must not be silently skipped"
        );
        assert_eq!(files[0].filename, "A&B.txt");
        assert_eq!(files[0].bytes, b"hello");
        assert!(files[0].external_key.ends_with("/docs/A&B.txt"));
    }

    #[tokio::test]
    async fn percent_encoded_filenames_use_the_original_request_path() {
        use axum::{
            http::{Method, StatusCode, Uri},
            Router,
        };

        const FILES: &[(&str, &str, &str)] = &[
            ("/docs/ordinary.txt", "ordinary.txt", "ordinary.txt"),
            ("/docs/%61lpha.txt", "alpha.txt", "alpha.txt"),
            ("/docs/plan%23a.txt", "plan#a.txt", "plan%23a.txt"),
            ("/docs/plan%23b.txt", "plan#b.txt", "plan%23b.txt"),
            ("/docs/what%3Fa.txt", "what?a.txt", "what%3Fa.txt"),
            ("/docs/what%3Fb.txt", "what?b.txt", "what%3Fb.txt"),
            ("/docs/literal%23.txt", "literal#.txt", "literal%23.txt"),
            (
                "/docs/literal%2523.txt",
                "literal%23.txt",
                "literal%2523.txt",
            ),
            (
                "/docs/%E4%B8%AD%E6%96%87%20notes.txt",
                "中文 notes.txt",
                "%E4%B8%AD%E6%96%87%20notes.txt",
            ),
        ];
        for href_kind in ["relative", "absolute", "proxy"] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let prefix = match href_kind {
                "absolute" => base.as_str(),
                "proxy" => "https://reverse-proxy.example",
                _ => "",
            };
            let entries = FILES.iter().map(|(path, _, _)| format!(
                "<response><href>{prefix}{path}</href><propstat><prop><getcontentlength>5</getcontentlength></prop></propstat></response>"
            )).collect::<String>();
            let xml = format!("<multistatus xmlns=\"DAV:\">{entries}</multistatus>");
            let app = Router::new().fallback(move |method: Method, uri: Uri| {
                let xml = xml.clone();
                async move {
                    let target = uri.path_and_query().unwrap().as_str();
                    if method.as_str() == "PROPFIND" && target == "/docs" {
                        (StatusCode::MULTI_STATUS, xml)
                    } else if method == Method::GET
                        && FILES.iter().any(|(path, _, _)| *path == target)
                    {
                        (StatusCode::OK, target.to_string())
                    } else {
                        (StatusCode::NOT_FOUND, String::new())
                    }
                }
            });
            let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let result = fetch(&reqwest::Client::new(), &base, "/docs", None).await;
            server.abort();
            let (files, truncated) = result.unwrap();
            assert!(!truncated);
            assert_eq!(files.len(), FILES.len(), "href kind: {href_kind}");
            let keys = files
                .iter()
                .map(|file| file.external_key.as_str())
                .collect::<std::collections::HashSet<_>>();
            assert_eq!(
                keys.len(),
                FILES.len(),
                "distinct files must keep distinct identities: {href_kind}"
            );
            for (file, (request_path, decoded_name, proxy_name)) in files.iter().zip(FILES) {
                let filename = if href_kind == "proxy" {
                    proxy_name
                } else {
                    decoded_name
                };
                assert_eq!(file.bytes, request_path.as_bytes());
                assert_eq!(&file.filename, filename);
                assert_eq!(
                    file.external_key,
                    format!("webdav://127.0.0.1/docs/{filename}")
                );
            }
        }
    }

    #[tokio::test]
    async fn percent_encoded_directories_keep_their_request_path_when_traversed() {
        use axum::{
            http::{Method, StatusCode, Uri},
            Router,
        };
        use std::sync::{Arc, Mutex};

        const PREFIX: &str = "/remote.php/dav/files/alice";
        const DIR: &str = "/docs/folder%23%3F%2520";
        const FILE: &str = "/docs/folder%23%3F%2520/note%23one.txt";
        for (href_kind, prefix) in [
            ("origin-root", ""),
            ("path", PREFIX),
            ("absolute", PREFIX),
            ("proxy", PREFIX),
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let origin = format!("http://{}", listener.local_addr().unwrap());
            let href_origin = match href_kind {
                "absolute" => origin.as_str(),
                "proxy" => "https://cloud.example.com",
                _ => "",
            };
            let collection = |dir: &str| {
                format!(
                "<response><href>{href_origin}{prefix}{dir}/</href><propstat><prop><resourcetype><collection/></resourcetype></prop></propstat></response>"
            )
            };
            let root = format!(
                "<multistatus xmlns=\"DAV:\">{}{}</multistatus>",
                collection("/docs"),
                collection(DIR)
            );
            let nested = format!(
                "<multistatus xmlns=\"DAV:\">{}<response><href>{href_origin}{prefix}{FILE}</href><propstat><prop><getcontentlength>5</getcontentlength></prop></propstat></response></multistatus>", collection(DIR)
            );
            let requests = Arc::new(Mutex::new(Vec::new()));
            let seen = requests.clone();
            let app = Router::new().fallback(move |method: Method, uri: Uri| {
                let (root, nested, seen) = (root.clone(), nested.clone(), seen.clone());
                async move {
                    seen.lock().unwrap().push(format!("{method} {uri}"));
                    let path = uri.path_and_query().unwrap().as_str();
                    match (method.as_str(), path.strip_prefix(prefix).unwrap_or("")) {
                        ("PROPFIND", "/docs") => (StatusCode::MULTI_STATUS, root),
                        ("PROPFIND", DIR) => (StatusCode::MULTI_STATUS, nested),
                        ("GET", FILE) => (StatusCode::OK, "hello".to_string()),
                        _ => (StatusCode::NOT_FOUND, String::new()),
                    }
                }
            });
            let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let base = format!("{origin}{prefix}");
            let result = fetch(&reqwest::Client::new(), &base, "/docs", None).await;
            server.abort();
            let (files, truncated) = result.unwrap_or_else(|err| panic!("{href_kind}: {err:#}"));
            assert!(!truncated);
            assert_eq!(files.len(), 1, "{href_kind}");
            assert_eq!(files[0].bytes, b"hello");
            let (filename, key) = if href_kind == "proxy" {
                (
                    "note%23one.txt",
                    format!("webdav://127.0.0.1{prefix}{FILE}"),
                )
            } else {
                (
                    "note#one.txt",
                    "webdav://127.0.0.1/docs/folder#?%20/note#one.txt".to_string(),
                )
            };
            assert_eq!(files[0].filename, filename);
            assert_eq!(files[0].external_key, key);
            assert_eq!(
                *requests.lock().unwrap(),
                [
                    format!("PROPFIND {prefix}/docs"),
                    format!("PROPFIND {prefix}{DIR}"),
                    format!("GET {prefix}{FILE}")
                ],
                "{href_kind}"
            );
        }
    }

    /// **Namespace prefixes are arbitrary.** The same response under a different prefix must yield the same result,
    /// or switching to another server gives "sync succeeded, zero files": the hardest kind of failure to diagnose.
    #[test]
    fn any_namespace_prefix_parses_the_same() {
        let with_d = r#"<?xml version="1.0"?>
<D:multistatus xmlns:D="DAV:"><D:response>
  <D:href>/docs/a.txt</D:href>
  <D:propstat><D:prop>
    <D:getcontentlength>5</D:getcontentlength>
    <D:getlastmodified>Wed, 02 Sep 2026 15:04:05 GMT</D:getlastmodified>
    <D:resourcetype/>
  </D:prop></D:propstat>
</D:response></D:multistatus>"#;
        // The prefix is swapped; the namespace declaration itself is unaffected (`xmlns:D="DAV:"` does not contain the substring `D:`)
        let with_ns0 = with_d.replace("D:", "ns0:");
        let a = parse_multistatus(with_d).unwrap();
        let b = parse_multistatus(&with_ns0).unwrap();
        assert_eq!(a, b, "cannot be parsed again under a different prefix");
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].href, "/docs/a.txt");
        assert_eq!(a[0].len, 5);
        assert!(!a[0].is_dir);
        assert!(
            a[0].modified.is_some(),
            "getlastmodified 是 doc_time 的来源"
        );
    }

    /// **The directory test is `<collection/>`, and it is usually a self-closing tag.**
    /// A parser that handles only `Event::Start` would GET every directory as a file.
    #[test]
    fn a_self_closing_collection_is_still_a_directory() {
        let xml = r#"<multistatus xmlns="DAV:"><response>
  <href>/docs/</href>
  <propstat><prop><resourcetype><collection/></resourcetype></prop></propstat>
</response></multistatus>"#;
        let e = parse_multistatus(xml).unwrap();
        assert!(e[0].is_dir, "self-closing collection was not recognized");
    }

    /// hrefs are percent-escaped: a URL built by concatenating a path with Chinese characters would 404.
    #[test]
    fn a_percent_encoded_href_is_decoded() {
        assert_eq!(
            percent_decode("/docs/%E4%B8%AD%E6%96%87.txt"),
            "/docs/中文.txt"
        );
        // Invalid sequences are left as is, without guessing
        assert_eq!(percent_decode("/a%ZZb"), "/a%ZZb");
    }

    /// Normalise paths: a leading slash, no trailing slash. Inconsistent ends would make the same directory walked twice.
    #[test]
    fn paths_normalise_to_one_shape() {
        for (raw, want) in [
            ("docs", "/docs"),
            ("/docs/", "/docs"),
            ("/", "/"),
            ("", "/"),
        ] {
            assert_eq!(normalize(raw), want, "{raw}");
        }
    }

    #[test]
    fn a_base_path_is_stripped_only_at_a_segment_boundary() {
        let base = "https://cloud.example.com/dav/alice";
        for (href, want) in [
            ("/dav/alice", ""),
            ("/dav/alice/docs/a.txt", "/docs/a.txt"),
            ("/dav/alice2/a.txt", "/dav/alice2/a.txt"),
            (
                "https://cloud.example.com/dav/alice2/a.txt",
                "/dav/alice2/a.txt",
            ),
        ] {
            assert_eq!(strip_base(href, base), want, "{href}");
        }
    }

    /// Connects to a real WebDAV server. Skipped without `UTOPIA_DAV_TEST_URL`.
    ///
    /// Using rclone to start one is the least effort, and its href shape differs from Nextcloud's,
    /// which is exactly what checks that parsing does not depend on the server:
    /// ```text
    /// mkdir -p /tmp/dav/docs && echo hello > /tmp/dav/docs/a.txt
    /// rclone serve webdav /tmp/dav --addr 127.0.0.1:18081 --user u --pass p
    /// UTOPIA_DAV_TEST_URL=http://127.0.0.1:18081 UTOPIA_DAV_TEST_USER=u     ///   UTOPIA_DAV_TEST_PASS=p cargo test -p utopia-server webdav
    /// ```
    #[tokio::test]
    async fn it_reads_from_a_real_webdav_server() -> anyhow::Result<()> {
        let Ok(base) = std::env::var("UTOPIA_DAV_TEST_URL") else {
            eprintln!("跳过：未设 UTOPIA_DAV_TEST_URL");
            return Ok(());
        };
        let user = std::env::var("UTOPIA_DAV_TEST_USER").unwrap_or_default();
        let pass = std::env::var("UTOPIA_DAV_TEST_PASS").unwrap_or_default();
        let auth = (!user.is_empty()).then_some((user.as_str(), pass.as_str()));

        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()?;
        let (files, truncated) = fetch(&http, &base, "/docs", auth).await?;
        assert!(!truncated);

        let a = files.iter().find(|f| f.filename == "a.txt").expect("a.txt");
        assert_eq!(a.bytes, b"hello from webdav");
        assert!(
            a.external_key.starts_with("webdav://"),
            "{}",
            a.external_key
        );
        assert!(a.last_modified.is_some());
        // The directory itself should not be mixed in
        assert!(files.iter().all(|f| !f.filename.is_empty()));
        Ok(())
    }
}
