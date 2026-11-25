//! Google Drive source: a person authorizes it with their own Google account, and it syncs the folders they pick (0074).
//!
//! Per the four criteria in [0013](../../docs/decisions/0013-a-source-should-hand-over-its-history.md):
//!
//! | Criterion | Google Drive |
//! |---|---|
//! | Real timestamp | `modifiedTime`, the last time the file content changed |
//! | Does it supersede itself | New versions of plans, minutes and contracts overwrite the old ones |
//! | Stable identity | File id; unchanged by renames or moves |
//! | Does enterprise knowledge live there | Most teams' documents live there |
//!
//! Like Notion, it **hands over only the current state**: revision history exists in the API, but old revisions of Google Docs can only
//! be exported as the full text at that time; which ones to sample is a separate decision, not done in this version.
//!
//! ## Authorization belongs to a person, not the deployment
//!
//! For Notion and object storage an admin pastes a key; here the **editor of this source** clicks "Connect Google
//! Drive", agrees to read-only access at Google, and we store the refresh token (sealed, `SOURCE_SECRET_KEYS`).
//! The OAuth client is per deployment (`UTOPIA_GOOGLE_DRIVE_CLIENT_ID` etc.); without it there is no such button.
//! Authorization also asks for `openid email`, only so the UI can make clear "this is being read as whom".
//!
//! ## Pitfalls to watch for
//!
//! **No recursive query.** `q` only accepts `'{id}' in parents`, so it walks level by level; when two selected folders
//! are nested inside each other, the same file appears twice, so deduplicate by id.
//!
//! **Things in shared drives are invisible by default.** Every request must carry `supportsAllDrives=true`,
//! and listings must also carry `includeItemsFromAllDrives=true`; without them there is no error, the results are just empty.
//!
//! **Google's own formats must be exported.** Docs/Sheets/Slides have no bytes, and `alt=media` returns 403;
//! use `export` to convert them to DOCX/XLSX/PPTX, which the existing parser handles (heading levels, tables and multiple sheets all survive).
//! The export limit is 10 MB; over it returns 403 `exportSizeLimitExceeded`, handled as a file that cannot be read.
//!
//! **Exported bytes differ on every call** (the zip carries timestamps), so "did it change" cannot rely on a content hash;
//! instead it uses the markers Drive provides: `md5Checksum` for binaries, `modifiedTime` for Google formats.
//!
//! Error text is in English: it is shown to users as-is in the source's sync status.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Duration;

use anyhow::Context as _;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use chrono::{DateTime, Utc};
use reqwest::StatusCode;
use serde_json::Value;
use uuid::Uuid;

pub const AUTHORIZE_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const API_ROOT: &str = "https://www.googleapis.com/drive/v3";

/// Read-only access to the whole Drive. `drive.file` is not enough: it only grants files the app created, or those selected one by one via the Picker;
/// selecting a folder does not bring along what is inside it
pub const DRIVE_SCOPE: &str = "https://www.googleapis.com/auth/drive.readonly";
pub const SCOPES: &str = "openid email https://www.googleapis.com/auth/drive.readonly";

const FOLDER_MIME: &str = "application/vnd.google-apps.folder";
const SHORTCUT_MIME: &str = "application/vnd.google-apps.shortcut";
/// The metadata a file needs; the listing and the shortcut targets both take these
const FILE_FIELDS: &str = "id,name,mimeType,modifiedTime,md5Checksum,size";

/// Maximum files listed per traversal. Listing is metadata only, and files whose markers have not changed are not downloaded, so the cap is generous;
/// it only guards against a tree so huge it would exhaust memory. When the cap is reached, the run says so instead of reporting success
const MAX_FILES_LISTED: usize = 50_000;
/// Maximum files downloaded (or exported) per sync, on the same scale as WebDAV and object storage: ingestion is irreversible.
/// The cap applies to downloads rather than listing: the listing is complete every round and unchanged files cost nothing; a round that hits the cap
/// says so, and the next round resumes from what has not been downloaded yet, so it never stops at the same spot every time (the kind of gap seen in #1212)
pub const MAX_DOWNLOADS_PER_SYNC: usize = 2_000;
/// Per-file cap, same as object storage
const MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;
/// How many levels to descend from a selected folder. Deeper levels are left unwalked, which is better than a sync getting stuck on one tree
const MAX_DEPTH: usize = 16;
/// Maximum subfolders a folder browser response returns
const MAX_FOLDERS_LISTED: usize = 1_000;
/// Maximum retries on 429 / 5xx
const MAX_RETRIES: u32 = 3;
const JSON_LIMIT: usize = 4 * 1024 * 1024;

/// Deployment-level OAuth client. All three must be present; if any one is missing it counts as not configured
#[derive(Clone)]
pub struct OAuthConfig {
    pub client_id: String,
    pub client_secret: String,
    pub redirect: String,
}

pub fn oauth_config() -> Option<OAuthConfig> {
    let get = |key: &str| {
        std::env::var(key)
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    Some(OAuthConfig {
        client_id: get("UTOPIA_GOOGLE_DRIVE_CLIENT_ID")?,
        client_secret: get("UTOPIA_GOOGLE_DRIVE_CLIENT_SECRET")?,
        redirect: get("UTOPIA_GOOGLE_DRIVE_REDIRECT_URI")?,
    })
}

/// File, folder and shared drive ids contain only these characters. They are spliced into single quotes in `q`,
/// so anything not on this table is refused outright: more reliable than escaping
pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Folders selected in the source config: `folders: [{id, name}]`. Names are for display only; sync uses only the id
pub fn selected_folders(config: &Value) -> Vec<String> {
    config["folders"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|f| f["id"].as_str().or_else(|| f.as_str()))
                .map(str::trim)
                .filter(|id| valid_id(id))
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

/// Who connected this source: the Utopia user recorded at the callback. None = not connected yet, or connected before this rule existed
pub fn connected_by(config: &Value) -> Option<Uuid> {
    config["connected_by"].as_str()?.parse().ok()
}

/// What the authorization code exchange yields
#[derive(Debug)]
pub struct Grant {
    pub refresh_token: String,
    /// The Google account that authorized it, with the email verified in the ID token; used only for display in the UI
    pub account_email: Option<String>,
}

fn http() -> anyhow::Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()?)
}

/// Exchange the authorization code + PKCE verifier for tokens. The refresh token is what matters: access tokens expire after an hour,
/// while sync runs when no one is watching
pub async fn exchange_code(cfg: &OAuthConfig, code: &str, verifier: &str) -> anyhow::Result<Grant> {
    exchange_code_at(&http()?, TOKEN_URL, cfg, code, verifier).await
}

async fn exchange_code_at(
    http: &reqwest::Client,
    token_url: &str,
    cfg: &OAuthConfig,
    code: &str,
    verifier: &str,
) -> anyhow::Result<Grant> {
    let resp = http
        .post(token_url)
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("code_verifier", verifier),
            ("client_id", &cfg.client_id),
            ("client_secret", &cfg.client_secret),
            ("redirect_uri", &cfg.redirect),
        ])
        .send()
        .await
        .context("google token")?;
    let v = token_response(resp).await?;
    // On the consent screen Google lets a person untick one permission; if the Drive one is unticked, the token is still issued,
    // but nothing can be read. Say so here, which is better than letting the first sync fail with a 403
    let scopes = v["scope"].as_str().unwrap_or("");
    anyhow::ensure!(
        scopes.split_whitespace().any(|s| s == DRIVE_SCOPE),
        "Google did not grant read access to Drive; connect again and allow it"
    );
    // A refresh token is issued only on first consent (or with `prompt=consent`); `begin` always passes consent
    let refresh_token = v["refresh_token"]
        .as_str()
        .filter(|t| !t.is_empty())
        .context("Google returned no refresh token; connect again")?
        .to_string();
    let account_email = match v["id_token"].as_str() {
        Some(t) => id_token_email(t, &cfg.client_id)?,
        None => None,
    };
    Ok(Grant {
        refresh_token,
        account_email,
    })
}

/// Exchange a refresh token for an access token
async fn access_token_at(
    http: &reqwest::Client,
    token_url: &str,
    cfg: &OAuthConfig,
    refresh_token: &str,
) -> anyhow::Result<String> {
    let resp = http
        .post(token_url)
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", &cfg.client_id),
            ("client_secret", &cfg.client_secret),
        ])
        .send()
        .await
        .context("google token")?;
    let v = token_response(resp).await?;
    Ok(v["access_token"]
        .as_str()
        .filter(|t| !t.is_empty())
        .context("google token: response has no access_token")?
        .to_string())
}

/// The token endpoint's response. `invalid_grant` is reported separately: it means the person revoked the grant, changed their password, or the refresh token
/// expired after going unused for six months; reconnecting fixes it, retrying does not
async fn token_response(resp: reqwest::Response) -> anyhow::Result<Value> {
    let status = resp.status();
    let v: Value = resp
        .json()
        .await
        .context("google token: reading the response")?;
    if status.is_success() {
        return Ok(v);
    }
    if v["error"] == "invalid_grant" {
        anyhow::bail!(
            "Google Drive access was revoked or has expired; reconnect the source in its settings"
        );
    }
    anyhow::bail!(
        "google token returned {status}: {}",
        v["error_description"]
            .as_str()
            .or(v["error"].as_str())
            .unwrap_or("no message")
    )
}

/// The email in the ID token.
///
/// **Signature not verified**: this token came from us exchanging the authorization code directly with Google's token endpoint over TLS,
/// and OIDC Core 3.1.3.7 allows TLS to stand in for signature checking in this case. The issuer and audience are still checked: what is checked is
/// "Google signed this for this deployment". The email is only for display; it is not used for identity and does not open an account;
/// an unverified email is not shown, so the UI never claims an address the person does not own
fn id_token_email(id_token: &str, client_id: &str) -> anyhow::Result<Option<String>> {
    let payload = id_token
        .split('.')
        .nth(1)
        .context("google id_token is not a JWT")?;
    let claims: Value = serde_json::from_slice(
        &URL_SAFE_NO_PAD
            .decode(payload.trim_end_matches('='))
            .context("google id_token payload is not base64url")?,
    )
    .context("google id_token payload is not JSON")?;
    anyhow::ensure!(
        matches!(
            claims["iss"].as_str(),
            Some("https://accounts.google.com" | "accounts.google.com")
        ),
        "google id_token has an unexpected issuer"
    );
    anyhow::ensure!(
        claims["aud"].as_str() == Some(client_id),
        "google id_token was issued to a different client"
    );
    anyhow::ensure!(
        claims["exp"]
            .as_i64()
            .is_some_and(|exp| exp > Utc::now().timestamp()),
        "google id_token has expired"
    );
    Ok(match claims["email_verified"].as_bool() {
        Some(true) => claims["email"].as_str().map(String::from),
        _ => None,
    })
}

/// An item in the folder browser
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct FolderEntry {
    pub id: String,
    pub name: String,
}

/// A file in a selected folder
#[derive(Debug, Clone)]
pub struct DriveFile {
    pub id: String,
    pub name: String,
    pub mime: String,
    pub modified: Option<DateTime<Utc>>,
    md5: Option<String>,
    modified_raw: Option<String>,
}

impl DriveFile {
    /// `gdrive://{file_id}`: the file id is its most stable identity
    pub fn external_key(&self) -> String {
        format!("gdrive://{}", self.id)
    }

    /// The "did it change" marker; see the module comment
    pub fn marker(&self) -> String {
        self.md5
            .clone()
            .or_else(|| self.modified_raw.clone())
            .unwrap_or_default()
    }
}

/// The result of one traversal round
#[derive(Debug, Default)]
pub struct Listing {
    pub files: Vec<DriveFile>,
    /// The file count cap was reached, or a folder is deeper than `MAX_DEPTH`
    pub truncated: bool,
    /// A selected folder that cannot be opened (deleted, or no longer shared with this person). When present, reconciliation is skipped:
    /// failing to open it does not mean everything inside is gone
    pub unreachable: Vec<String>,
    /// Files skipped, one reason each: forms, or too large
    pub skipped: usize,
    /// Shortcuts pointing at files this person cannot open (not shared with them, or deleted), named by the shortcut's name
    pub broken_shortcuts: Vec<String>,
}

/// Bytes read from a file
pub struct Content {
    pub bytes: Vec<u8>,
    pub filename: String,
    pub mime: &'static str,
}

/// What each Google-native format exports to. Google formats not on this table (forms, drawings,
/// shortcuts, sites, ...) have no readable body and are skipped
fn export_as(mime: &str) -> Option<(&'static str, &'static str)> {
    Some(match mime {
        "application/vnd.google-apps.document" => (
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            "docx",
        ),
        "application/vnd.google-apps.spreadsheet" => (
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            "xlsx",
        ),
        "application/vnd.google-apps.presentation" => (
            "application/vnd.openxmlformats-officedocument.presentationml.presentation",
            "pptx",
        ),
        _ => return None,
    })
}

/// A Drive client holding an access token
pub struct Drive {
    http: reqwest::Client,
    token: std::sync::Mutex<String>,
    /// An access token expires after an hour, and a sync round can run longer: on a 401, exchange the refresh token for a new one and try again
    renewal: Option<Renewal>,
    // The private-address entry point lets HTTP tests run the real traversal logic, without adding any user configuration
    api_root: String,
}

struct Renewal {
    cfg: OAuthConfig,
    refresh_token: String,
    token_url: String,
}

impl Drive {
    /// Exchange the refresh token for an access token, getting a usable client
    pub async fn connect(cfg: &OAuthConfig, refresh_token: &str) -> anyhow::Result<Self> {
        let http = http()?;
        let token = access_token_at(&http, TOKEN_URL, cfg, refresh_token).await?;
        Ok(Self {
            http,
            token: std::sync::Mutex::new(token),
            renewal: Some(Renewal {
                cfg: cfg.clone(),
                refresh_token: refresh_token.into(),
                token_url: TOKEN_URL.into(),
            }),
            api_root: API_ROOT.into(),
        })
    }

    /// Send one GET. 429 and 5xx retry with backoff; a 401 exchanges for a new access token and tries once more (only once: if the new one is also rejected,
    /// the authorization itself is broken); other non-2xx errors carry Google's own message, and the status code and `reason` are kept in the error
    /// so the caller can tell them apart (404 = cannot be opened)
    async fn get(
        &self,
        what: &str,
        url: &str,
        query: &[(&str, &str)],
    ) -> anyhow::Result<reqwest::Response> {
        let url = reqwest::Url::parse_with_params(url, query)
            .with_context(|| format!("google drive {what}: building the URL"))?;
        let mut attempt = 0u32;
        let mut renewed = false;
        loop {
            let token = self.token.lock().expect("token lock").clone();
            let resp = self
                .http
                .get(url.clone())
                .bearer_auth(&token)
                .send()
                .await
                .with_context(|| format!("google drive {what}"))?;
            let status = resp.status();
            if let (StatusCode::UNAUTHORIZED, false, Some(r)) = (status, renewed, &self.renewal) {
                renewed = true;
                let fresh =
                    access_token_at(&self.http, &r.token_url, &r.cfg, &r.refresh_token).await?;
                *self.token.lock().expect("token lock") = fresh;
                continue;
            }
            if (status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error())
                && attempt < MAX_RETRIES
            {
                attempt += 1;
                let wait = Duration::from_millis(500 * 2u64.pow(attempt));
                tracing::info!(what, attempt, %status, "google drive busy, waiting before the next attempt");
                tokio::time::sleep(wait).await;
                continue;
            }
            if status.is_success() {
                return Ok(resp);
            }
            let v: Value = resp.json().await.unwrap_or(Value::Null);
            let reason = v["error"]["errors"][0]["reason"].as_str().unwrap_or("");
            return Err(DriveError {
                status,
                message: format!(
                    "google drive {what} returned {status}{}: {}",
                    if reason.is_empty() {
                        String::new()
                    } else {
                        format!(" ({reason})")
                    },
                    v["error"]["message"].as_str().unwrap_or("no message")
                ),
            }
            .into());
        }
    }

    async fn get_json(
        &self,
        what: &str,
        url: &str,
        query: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        let resp = self.get(what, url, query).await?;
        let bytes = read_capped(resp, JSON_LIMIT as u64)
            .await
            .with_context(|| format!("google drive {what}"))?;
        serde_json::from_slice(&bytes).with_context(|| format!("google drive {what}: not JSON"))
    }

    /// List files by `q`, paging through all pages or until `limit`. Second item: whether it stopped because the cap was reached
    async fn list(
        &self,
        q: &str,
        fields: &str,
        limit: usize,
    ) -> anyhow::Result<(Vec<Value>, bool)> {
        let url = format!("{}/files", self.api_root);
        let fields = format!("nextPageToken,files({fields})");
        let mut out = Vec::new();
        let mut page_token: Option<String> = None;
        loop {
            let mut query = vec![
                ("q", q),
                ("fields", fields.as_str()),
                ("pageSize", "1000"),
                ("orderBy", "name"),
                ("supportsAllDrives", "true"),
                ("includeItemsFromAllDrives", "true"),
            ];
            if let Some(t) = &page_token {
                query.push(("pageToken", t));
            }
            let v = self.get_json("files.list", &url, &query).await?;
            for f in v["files"].as_array().into_iter().flatten() {
                if out.len() >= limit {
                    return Ok((out, true));
                }
                out.push(f.clone());
            }
            page_token = v["nextPageToken"].as_str().map(String::from);
            if page_token.is_none() {
                return Ok((out, false));
            }
        }
    }

    /// Top level of the folder browser: My Drive, Shared with me, and every visible shared drive.
    /// The names of the first two are swapped for their localized labels by the UI, keyed by id
    pub async fn roots(&self) -> anyhow::Result<Vec<FolderEntry>> {
        let mut out = vec![
            FolderEntry {
                id: "root".into(),
                name: "My Drive".into(),
            },
            FolderEntry {
                id: SHARED_WITH_ME.into(),
                name: "Shared with me".into(),
            },
        ];
        let url = format!("{}/drives", self.api_root);
        let mut page_token: Option<String> = None;
        loop {
            let mut query = vec![
                ("pageSize", "100"),
                ("fields", "nextPageToken,drives(id,name)"),
            ];
            if let Some(t) = &page_token {
                query.push(("pageToken", t));
            }
            let v = self.get_json("drives.list", &url, &query).await?;
            for d in v["drives"].as_array().into_iter().flatten() {
                if let (Some(id), Some(name)) = (d["id"].as_str(), d["name"].as_str()) {
                    out.push(FolderEntry {
                        id: id.into(),
                        name: name.into(),
                    });
                }
            }
            page_token = v["nextPageToken"].as_str().map(String::from);
            if page_token.is_none() || out.len() >= MAX_FOLDERS_LISTED {
                return Ok(out);
            }
        }
    }

    /// Subfolders under a folder (or a shared drive, or "Shared with me")
    pub async fn child_folders(&self, parent: &str) -> anyhow::Result<Vec<FolderEntry>> {
        let q = if parent == SHARED_WITH_ME {
            format!("sharedWithMe = true and mimeType = '{FOLDER_MIME}' and trashed = false")
        } else {
            anyhow::ensure!(valid_id(parent), "invalid folder id");
            format!("'{parent}' in parents and mimeType = '{FOLDER_MIME}' and trashed = false")
        };
        let (items, _) = self.list(&q, "id,name", MAX_FOLDERS_LISTED).await?;
        Ok(items
            .iter()
            .filter_map(|f| {
                Some(FolderEntry {
                    id: f["id"].as_str()?.into(),
                    name: f["name"].as_str()?.into(),
                })
            })
            .collect())
    }

    /// Walk the selected folders and collect files.
    ///
    /// Breadth-first; folders already visited are not entered again (when two selected folders are nested in each other); files are deduplicated by id
    pub async fn walk(&self, folders: &[String]) -> anyhow::Result<Listing> {
        let mut listing = Listing::default();
        let mut visited: HashSet<String> = HashSet::new();
        let mut seen_files: HashSet<String> = HashSet::new();
        let mut queue: VecDeque<(String, usize)> = VecDeque::new();

        for id in folders {
            match self.folder_is_open(id).await {
                Ok(true) => queue.push_back((id.clone(), 0)),
                // Inside the trash: its contents no longer count for this person, so do not descend; let reconciliation flag them
                Ok(false) => {}
                Err(e) if is_not_found(&e) => listing.unreachable.push(id.clone()),
                Err(e) => return Err(e),
            }
        }

        while let Some((folder, depth)) = queue.pop_front() {
            if !visited.insert(folder.clone()) {
                continue;
            }
            let q = format!("'{folder}' in parents and trashed = false");
            let room = MAX_FILES_LISTED.saturating_sub(listing.files.len()) + MAX_FOLDERS_LISTED;
            let fields = format!("{FILE_FIELDS},shortcutDetails(targetId,targetMimeType)");
            let (items, cut) = self.list(&q, &fields, room).await?;
            if cut {
                listing.truncated = true;
            }
            for item in items {
                // A meeting organized by someone else keeps its notes and transcript in their Drive, and this side's folder holds only a shortcut:
                // swap it for the one it points to, after which it is handled like an ordinary file
                let f = if item["mimeType"] == SHORTCUT_MIME {
                    match self.shortcut_target(&item).await {
                        Ok(Some(target)) => target,
                        Ok(None) => {
                            listing.skipped += 1;
                            continue;
                        }
                        Err(e) if is_inaccessible(&e) => {
                            let name = item["name"].as_str().unwrap_or("a shortcut");
                            listing.broken_shortcuts.push(name.to_string());
                            continue;
                        }
                        Err(e) => return Err(e),
                    }
                } else {
                    item
                };
                let (Some(id), Some(name), Some(mime)) =
                    (f["id"].as_str(), f["name"].as_str(), f["mimeType"].as_str())
                else {
                    continue;
                };
                // Ids returned by Drive are later spliced into queries and URLs: same character set as the selected folders, and any that do not fit are refused
                if !valid_id(id) {
                    listing.skipped += 1;
                    continue;
                }
                if mime == FOLDER_MIME {
                    if depth + 1 > MAX_DEPTH {
                        listing.truncated = true;
                    } else {
                        queue.push_back((id.to_string(), depth + 1));
                    }
                    continue;
                }
                let google_format = mime.starts_with("application/vnd.google-apps.");
                if google_format && export_as(mime).is_none() {
                    listing.skipped += 1;
                    continue;
                }
                // Drive's size is an integer in string form; Google formats have no size
                let size = f["size"].as_str().and_then(|s| s.parse::<u64>().ok());
                if !google_format && size.is_none_or(|n| n == 0 || n > MAX_FILE_BYTES) {
                    listing.skipped += 1;
                    continue;
                }
                if !seen_files.insert(id.to_string()) {
                    continue;
                }
                if listing.files.len() >= MAX_FILES_LISTED {
                    listing.truncated = true;
                    return Ok(listing);
                }
                let modified_raw = f["modifiedTime"].as_str().map(String::from);
                listing.files.push(DriveFile {
                    id: id.into(),
                    name: name.into(),
                    mime: mime.into(),
                    modified: modified_raw
                        .as_deref()
                        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                        .map(|d| d.with_timezone(&Utc)),
                    md5: f["md5Checksum"].as_str().map(String::from),
                    modified_raw,
                });
            }
        }
        Ok(listing)
    }

    /// The one the shortcut points to. A folder target is treated as a folder; a file target takes the target's own metadata,
    /// with the change marker, size and export format all computed from the target, and the file also identified by the target's id (so selecting it directly and reaching it through a shortcut give the same result).
    /// If the target is in the trash -> `None`; if it cannot be opened -> the 403/404 error
    async fn shortcut_target(&self, shortcut: &Value) -> anyhow::Result<Option<Value>> {
        let details = &shortcut["shortcutDetails"];
        let Some(target) = details["targetId"].as_str().filter(|id| valid_id(id)) else {
            return Ok(None);
        };
        if details["targetMimeType"] == FOLDER_MIME {
            return Ok(Some(serde_json::json!({
                "id": target,
                "name": shortcut["name"],
                "mimeType": FOLDER_MIME,
            })));
        }
        let url = format!("{}/files/{target}", self.api_root);
        let fields = format!("{FILE_FIELDS},trashed");
        let v = self
            .get_json(
                "files.get",
                &url,
                &[("fields", fields.as_str()), ("supportsAllDrives", "true")],
            )
            .await?;
        Ok((v["trashed"].as_bool() != Some(true)).then_some(v))
    }

    /// Whether this selected item still exists: exists and not in the trash -> true; in the trash -> false;
    /// cannot be opened -> 404 error. `root` is "My Drive"; a shared drive's id can also be used as a folder
    async fn folder_is_open(&self, id: &str) -> anyhow::Result<bool> {
        let url = format!("{}/files/{id}", self.api_root);
        let v = self
            .get_json(
                "files.get",
                &url,
                &[
                    ("fields", "id,mimeType,trashed"),
                    ("supportsAllDrives", "true"),
                ],
            )
            .await?;
        anyhow::ensure!(
            v["mimeType"] == FOLDER_MIME,
            "google drive: selected item {id} is not a folder"
        );
        Ok(v["trashed"].as_bool() != Some(true))
    }

    /// A file's bytes: Google formats are exported, everything else downloaded as-is
    pub async fn content(&self, file: &DriveFile) -> anyhow::Result<Content> {
        anyhow::ensure!(
            valid_id(&file.id),
            "google drive: file id {:?} is not a Drive id",
            file.id
        );
        if let Some((mime, ext)) = export_as(&file.mime) {
            let url = format!("{}/files/{}/export", self.api_root, file.id);
            let resp = self.get("export", &url, &[("mimeType", mime)]).await?;
            let bytes = read_capped(resp, MAX_FILE_BYTES).await?;
            let filename = if file.name.to_lowercase().ends_with(&format!(".{ext}")) {
                file.name.clone()
            } else {
                format!("{}.{ext}", file.name)
            };
            return Ok(Content {
                bytes,
                filename,
                mime,
            });
        }
        let url = format!("{}/files/{}", self.api_root, file.id);
        let resp = self
            .get(
                "download",
                &url,
                &[("alt", "media"), ("supportsAllDrives", "true")],
            )
            .await?;
        // Does not guess the MIME type, for the same reason as object storage: parsing checks magic bytes first, then the extension
        Ok(Content {
            bytes: read_capped(resp, MAX_FILE_BYTES).await?,
            filename: file.name.clone(),
            mime: "application/octet-stream",
        })
    }
}

/// The placeholder id for "Shared with me" in the folder browser. Real ids contain no colon, so it cannot collide
pub const SHARED_WITH_ME: &str = "shared:with-me";

/// A non-2xx response from Drive
#[derive(Debug)]
struct DriveError {
    status: StatusCode,
    message: String,
}

impl std::fmt::Display for DriveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for DriveError {}

fn is_not_found(e: &anyhow::Error) -> bool {
    e.downcast_ref::<DriveError>()
        .is_some_and(|d| d.status == StatusCode::NOT_FOUND)
}

/// Cannot be opened: Drive sometimes returns 404 and sometimes 403 for files not shared with this person
fn is_inaccessible(e: &anyhow::Error) -> bool {
    e.downcast_ref::<DriveError>()
        .is_some_and(|d| d.status == StatusCode::NOT_FOUND || d.status == StatusCode::FORBIDDEN)
}

/// Read a size-capped response body. Check Content-Length first, then count while reading
async fn read_capped(mut resp: reqwest::Response, limit: u64) -> anyhow::Result<Vec<u8>> {
    if resp.content_length().is_some_and(|n| n > limit) {
        anyhow::bail!("response is larger than {limit} bytes");
    }
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await? {
        body.extend_from_slice(&chunk);
        if body.len() as u64 > limit {
            anyhow::bail!("response is larger than {limit} bytes");
        }
    }
    Ok(body)
}

/// Each file's marker as of the last ingest round
pub async fn markers(
    pool: &sqlx::PgPool,
    source_id: Uuid,
) -> anyhow::Result<HashMap<String, String>> {
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT file_id, marker FROM google_drive_files WHERE source_id = $1")
            .bind(source_id)
            .fetch_all(pool)
            .await?;
    Ok(rows.into_iter().collect())
}

pub async fn remember(
    pool: &sqlx::PgPool,
    source_id: Uuid,
    file_id: &str,
    marker: &str,
) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO google_drive_files (source_id, file_id, marker) VALUES ($1, $2, $3)
         ON CONFLICT (source_id, file_id) DO UPDATE SET marker = EXCLUDED.marker",
    )
    .bind(source_id)
    .bind(file_id)
    .bind(marker)
    .execute(pool)
    .await?;
    Ok(())
}

/// Files that did not appear in this round have their markers forgotten: when they reappear they must be read again, and cannot be skipped on the strength of an old marker.
/// Called only on fully completed traversal rounds, under the same condition as reconciliation
pub async fn forget_others(
    pool: &sqlx::PgPool,
    source_id: Uuid,
    seen: &[String],
) -> anyhow::Result<()> {
    sqlx::query("DELETE FROM google_drive_files WHERE source_id = $1 AND NOT (file_id = ANY($2))")
        .bind(source_id)
        .bind(seen)
        .execute(pool)
        .await?;
    Ok(())
}

#[cfg(test)]
#[path = "google_drive_tests.rs"]
mod tests;
