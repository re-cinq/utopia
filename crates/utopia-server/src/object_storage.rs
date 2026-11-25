//! Object storage source: S3 (and MinIO, Ceph RGW, R2 and others that speak the same protocol), Azure Blob,
//! Google Cloud Storage. All three share this one module: `object_store` puts the differences
//! in the builder, so the listing and fetching code does not change at all.
//!
//! By the four criteria of [0013](../../docs/decisions/0013-a-source-should-hand-over-its-history.md),
//! it is not the same class as the ticket systems:
//!
//! | Criterion | Object storage |
//! |---|---|
//! | Real timestamp | `LastModified`, but it is the **write time**, not the document's own time |
//! | Whether it overturns itself | Overwriting the same key is a change of mind, but it can only be seen as a difference between two syncs |
//! | Stable identity | `bucket/key`, the cleanest of the lot |
//! | Whether enterprise knowledge lives there | **The strongest one**: document dumps, archives and data-lake landing zones are all here |
//!
//! So its value lies in the fourth criterion: **reachability**. The second is only partial: the ticket systems can hand over
//! the whole change history at once, while object storage either has versioning turned on (not read by this version, see below), or it
//! accumulates only through repeated syncs. This is the same class as `url` / `rss`, and not lower than them.
//!
//! **Version history is not handled in this version.** `ListObjectVersions` can fetch the history 0013 wants in one go,
//! but most buckets do not have versioning on, and even those that do mostly hold write-once objects that never change. Ingesting every version
//! costs far more than it gains. Add it behind a switch once someone really needs it (say, a quarterly-updated policy file,
//! and they want to see what changed).
//!
//! **Why `object_store` rather than `aws-sdk-s3` / `rust-s3`**: measured transitive dependencies
//! `+7` against `+43` / `+42`. It reuses `reqwest`, `ring` and `quick-xml`, which are already in the tree.
//! The same API also covers Azure Blob and GCS, and the roadmap's "more data sources" and
//! "data lakehouse" both lie on this line: switching backends is just switching builders.

use anyhow::Context as _;
use chrono::{DateTime, Utc};
use futures_util::StreamExt as _;
use object_store::aws::AmazonS3Builder;
use object_store::azure::MicrosoftAzureBuilder;
use object_store::gcp::GoogleCloudStorageBuilder;
// `get` lives on `ObjectStoreExt`, not on `ObjectStore` itself, which has only
// `get_opts`. Without this line the error reads "&dyn ObjectStore has no get method",
// and the trait name does not show why.
use object_store::{path::Path as StorePath, ObjectStore, ObjectStoreExt as _};

/// The maximum number of objects ingested in one sync.
///
/// **Not a performance concern: it stops someone from sucking a whole bucket in.** A bucket with millions of objects is normal,
/// and ingestion is irreversible: every object must be extracted, vectorized and put into the graph. One wrong prefix
/// can burn a whole day's quota, and cleaning that up is much harder than configuring it.
///
/// When the cap is reached, it is reported in the sync stats, so people can see "there is more", rather than being silently truncated.
const MAX_OBJECTS_PER_SYNC: usize = 2_000;

/// Size cap for a single object. Same order of magnitude as the upload path: anything bigger is usually not
/// a document but a data file (backups, images, video), which the extractor cannot use.
const MAX_OBJECT_BYTES: u64 = 32 * 1024 * 1024;

/// One object waiting to be ingested.
pub struct RemoteObject {
    /// `s3://bucket/key`: the `ingest_item` external_key convention is a URI shape
    pub external_key: String,
    /// The file name the document gets: the last segment of the key
    pub filename: String,
    pub bytes: Vec<u8>,
    pub last_modified: Option<DateTime<Utc>>,
}

/// Builds a client from the source config. The three clouds share this one entry point, dispatching on `kind`.
///
/// **All three call the bucket `bucket` in config**, even though Azure calls it a container and GCS calls it a storage bucket.
/// If the config keys followed each product's terminology, the form and the docs would each say something different, while this layer only needs to know
/// what the thing that holds objects is called. Credential fields keep each vendor's original names (`access_key_id` /
/// `account_key` / `service_account_key`), because users copy those from the console,
/// and renaming them would only make people fail to match them up.
///
/// **If `endpoint` is set, it is self-hosted** (MinIO, Ceph, R2, Azurite). In that case S3 is forced to
/// path-style: virtual-host addressing requires that a domain such as `bucket.host` resolves, and a self-hosted
/// deployment usually has only a bare IP. Without forcing it, the symptom is "bucket does not exist", though the bucket is right there.
///
/// Allowing http is also for this case: a MinIO on an internal network, or a local emulator, often has no TLS.
/// **This is a downgrade the config chooses**, not the default: without an endpoint it still uses https only.
pub fn client(kind: &str, config: &serde_json::Value) -> anyhow::Result<Box<dyn ObjectStore>> {
    let s = |k: &str| {
        config[k]
            .as_str()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
    };
    let bucket =
        s("bucket").ok_or_else(|| anyhow::anyhow!("{kind} source is missing config.bucket"))?;
    let endpoint = s("endpoint");
    let insecure = endpoint
        .as_deref()
        .is_some_and(|e| e.starts_with("http://"));

    match kind {
        "azure_blob" => {
            let mut b = MicrosoftAzureBuilder::new().with_container_name(&bucket);
            if let Some(account) = s("account_name") {
                b = b.with_account(account);
            }
            if let Some(key) = s("account_key") {
                b = b.with_access_key(key);
            }
            if let Some(e) = endpoint {
                b = b.with_endpoint(e).with_allow_http(insecure);
            }
            Ok(Box::new(b.build().context("azure blob client")?))
        }
        "gcs" => {
            let mut b = GoogleCloudStorageBuilder::new().with_bucket_name(&bucket);
            // The whole service account JSON is pasted in, not a path: the source config is a one-line JSONB,
            // and the server may not have that file at all
            let key = s("service_account_key");
            if let Some(json) = key.clone() {
                b = b.with_service_account_key(json);
            }
            if let Some(e) = endpoint {
                // **`with_base_url` rather than `with_url`.** The latter is for parsing a storage location
                // such as `gs://bucket/path`. Feeding it an http endpoint
                // gives "Unknown url scheme cannot be parsed", and the name does not show the difference
                b = b.with_base_url(&e);
                // Self-hosted endpoints usually lack Google's credential system (emulators, S3 gateways).
                // If no service account is given, skip signing; otherwise the builder goes looking for default credentials and fails
                if key.is_none() {
                    b = b.with_skip_signature(true);
                }
            }
            Ok(Box::new(b.build().context("gcs client")?))
        }
        // s3, and everything else that speaks the same protocol
        _ => {
            let mut b = AmazonS3Builder::new().with_bucket_name(&bucket);
            if let Some(region) = s("region") {
                b = b.with_region(region);
            }
            if let (Some(key), Some(secret)) = (s("access_key_id"), s("secret_access_key")) {
                b = b.with_access_key_id(key).with_secret_access_key(secret);
            }
            if let Some(e) = endpoint {
                b = b
                    .with_endpoint(e)
                    .with_virtual_hosted_style_request(false)
                    .with_allow_http(insecure);
            }
            Ok(Box::new(b.build().context("object storage client")?))
        }
    }
}

/// Each cloud's URI scheme, used as the prefix of external_key.
///
/// Hard-coded rather than built from kind directly: GCS's common form is gs while the source type is gcs,
/// and Azure's container URI is not called azure_blob either. What is wanted here is the shape others recognise.
fn uri_scheme(kind: &str) -> &'static str {
    match kind {
        "azure_blob" => "azure",
        "gcs" => "gs",
        _ => "s3",
    }
}

/// Lists the objects under a prefix and fetches each one back.
///
/// **Zero-byte objects are always skipped; the criterion is size, not name.** The intuitive approach is to check whether the key ends with `/`,
/// since the console and `aws s3 sync` create "folders" that way. But `object_store`'s
/// `Path` **normalises the trailing slash away**, so `docs/` becomes `docs`, and that check
/// never holds; it then GETs `docs`, while the real key in the bucket is `docs/`,
/// so a 404 takes down the whole sync. This was measured to blow up exactly that way on MinIO.
///
/// Judging by size is broader still: a zero-byte object is not a document whatever its name is, and `ingest_item` given
/// empty bytes would only return `Unchanged`.
///
/// The format is not judged here. `utopia_ingest` dispatches extraction by extension plus mime, and anything unrecognised
/// is decoded as text in every case. Keeping another whitelist here would write the same thing twice,
/// and the two lists would eventually diverge.
pub async fn fetch(
    kind: &str,
    store: &dyn ObjectStore,
    bucket: &str,
    prefix: Option<&str>,
) -> anyhow::Result<(Vec<RemoteObject>, bool)> {
    // **Each of the three uses its own scheme.** Identity must be unique: a bucket with the same name on S3 and GCS is two
    // different things, and sharing one prefix would let the documents of two sources claim each other
    let scheme = uri_scheme(kind);
    // The prefix is the key in the bucket as written. `Path::from` escapes characters such as `#`, `[` and `%`,
    // so a `Case #12` key becomes `Case %2312`, and no object is found
    let p = prefix.map(|p| StorePath::parse(p).unwrap_or_else(|_| StorePath::from(p)));
    let mut listing = store.list(p.as_ref());
    let mut out = Vec::new();
    let mut truncated = false;
    let mut unreadable = 0usize;

    while let Some(meta) = listing.next().await {
        let meta = meta.context("listing objects")?;
        let key = meta.location.as_ref().to_string();

        if meta.size == 0 {
            continue;
        }
        if meta.size > MAX_OBJECT_BYTES {
            tracing::info!(%key, size = meta.size, "对象太大，跳过");
            continue;
        }
        if out.len() >= MAX_OBJECTS_PER_SYNC {
            truncated = true;
            break;
        }

        // **One object that cannot be fetched back must not take down the whole sync.** Time passes between listing and fetching,
        // and being deleted midway or having permissions changed are normal; the bigger the bucket, the more common. Record it and report it at the end,
        // rather than letting a single 404 on the 900th object throw away the results of the first 899.
        let got = async {
            let r = store.get(&meta.location).await?;
            r.bytes().await
        }
        .await;
        let bytes = match got {
            Ok(b) => b,
            Err(e) => {
                unreadable += 1;
                tracing::warn!(%key, error = %e, "对象取不回来，跳过");
                continue;
            }
        };

        out.push(RemoteObject {
            external_key: format!("{scheme}://{bucket}/{key}"),
            filename: key.rsplit('/').next().unwrap_or(&key).to_string(),
            bytes: bytes.to_vec(),
            last_modified: Some(meta.last_modified),
        });
    }
    if unreadable > 0 {
        tracing::warn!(unreadable, bucket, "有对象没能取回");
    }
    Ok((out, truncated))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A self-hosted endpoint must use path-style, or requests go to `bucket.host`,
    /// and a self-hosted deployment generally has no such DNS record. The symptom is "bucket does not exist" when the bucket is right there.
    #[test]
    fn a_self_hosted_endpoint_builds() {
        let cfg = serde_json::json!({
            "bucket": "docs",
            "endpoint": "http://127.0.0.1:9000",
            "region": "us-east-1",
            "access_key_id": "minioadmin",
            "secret_access_key": "minioadmin",
        });
        assert!(client("s3", &cfg).is_ok());
    }

    /// Missing bucket must report which item is missing: follow the wording of the jira / github sources.
    #[test]
    fn a_missing_bucket_says_so() {
        let e = client("s3", &serde_json::json!({ "region": "us-east-1" })).unwrap_err();
        assert!(e.to_string().contains("config.bucket"), "{e}");
    }

    /// The public-cloud path: with no endpoint, path-style must not be forced,
    /// and plain http must not be allowed either.
    #[test]
    fn a_cloud_bucket_needs_no_endpoint() {
        let cfg = serde_json::json!({
            "bucket": "docs",
            "region": "ap-southeast-1",
            "access_key_id": "AKIA...",
            "secret_access_key": "...",
        });
        assert!(client("s3", &cfg).is_ok());
    }

    /// Prefixes with `#`, `[]` or Chinese characters in the folder name are listed as is: that is how the keys are written in the bucket.
    /// `Path::from` escapes non-ASCII bytes too, so a folder such as `合同/2024` could not be listed even before, and no object was found
    #[tokio::test]
    async fn a_prefix_with_symbols_lists_its_objects() {
        use object_store::memory::InMemory;
        let store = InMemory::new();
        for key in [
            "Case #12/brief.txt",
            "Q1 [final]/plan.md",
            "合同/2024/租约.pdf",
            "other/x.txt",
        ] {
            store
                .put(&StorePath::parse(key).unwrap(), "hello".into())
                .await
                .unwrap();
        }
        for (prefix, want) in [
            ("Case #12", "s3://b/Case #12/brief.txt"),
            ("Q1 [final]/", "s3://b/Q1 [final]/plan.md"),
            ("合同/2024", "s3://b/合同/2024/租约.pdf"),
        ] {
            let (objs, _) = fetch("s3", &store, "b", Some(prefix)).await.unwrap();
            let keys: Vec<&str> = objs.iter().map(|o| o.external_key.as_str()).collect();
            assert_eq!(keys, [want], "{prefix}");
        }
    }

    /// Connects to a real S3-compatible endpoint and tests only the **read** side: list, fetch content, and check identity.
    ///
    /// **Skipped rather than failing without `UTOPIA_S3_TEST_ENDPOINT`**, the same convention as the database tests
    /// (see CONTRIBUTING). The three cases above only prove that the builder assembles, whereas this one guards
    /// that **the protocol on the wire really works**: whether path-style took effect,
    /// whether plain http was allowed, whether the `s3://` identity is assembled correctly, and whether `LastModified` comes back.
    /// None of these can be discovered when the client is constructed.
    ///
    /// **The data is laid out from outside; the test never writes, only reads.** The first version had the test `put` its own data, and it got
    /// bitten by `object_store`'s `Path`: it normalises `docs/` to `docs`,
    /// so a "directory placeholder" was written as an ordinary object named `docs`, and MinIO does not allow
    /// object `docs` and prefix `docs/` to coexist—the whole prefix was overwritten, and the listing came back empty.
    /// A real placeholder object can only be made by another client, so let another client make it.
    ///
    /// How to run it (start MinIO first, then seed data with curl; curl has SigV4 built in):
    /// ```text
    /// MINIO_ROOT_USER=u MINIO_ROOT_PASSWORD=p123456 \
    ///   minio server ./data --address 127.0.0.1:19000 &
    /// S3="curl -s --aws-sigv4 aws:amz:us-east-1:s3 -u u:p123456"
    /// B=http://127.0.0.1:19000/utopia-conn-test
    /// $S3 -X PUT $B                                  # create the bucket
    /// $S3 -X PUT --data-raw hello    "$B/docs/a.txt"
    /// $S3 -X PUT --data-raw '# world' "$B/docs/b.md"
    /// $S3 -X PUT --data-raw ''        "$B/docs/"     # directory placeholder
    /// UTOPIA_S3_TEST_ENDPOINT=http://127.0.0.1:19000 \
    ///   UTOPIA_S3_TEST_KEY=u UTOPIA_S3_TEST_SECRET=p123456 \
    ///   cargo test -p utopia-server object_storage
    /// ```
    #[tokio::test]
    async fn it_reads_from_a_real_s3_endpoint() -> anyhow::Result<()> {
        let Ok(endpoint) = std::env::var("UTOPIA_S3_TEST_ENDPOINT") else {
            eprintln!("跳过：未设 UTOPIA_S3_TEST_ENDPOINT");
            return Ok(());
        };
        let bucket = "utopia-conn-test";
        let cfg = serde_json::json!({
            "bucket": bucket,
            "endpoint": endpoint,
            "region": "us-east-1",
            "access_key_id": std::env::var("UTOPIA_S3_TEST_KEY").unwrap_or("minioadmin".into()),
            "secret_access_key": std::env::var("UTOPIA_S3_TEST_SECRET").unwrap_or("minioadmin".into()),
        });
        let store = client("s3", &cfg)?;

        let (objs, truncated) = fetch("s3", store.as_ref(), bucket, Some("docs")).await?;
        assert!(!truncated, "三个对象不该触发上限");

        let keys: Vec<&str> = objs.iter().map(|o| o.external_key.as_str()).collect();
        for want in [
            "s3://utopia-conn-test/docs/a.txt",
            "s3://utopia-conn-test/docs/b.md",
        ] {
            assert!(keys.contains(&want), "少了 {want}：{keys:?}");
        }

        // **This guards a bug that really blew up.** The bucket has a `docs/` directory placeholder object,
        // and `object_store` normalised the trailing slash away, so it entered
        // the loop as `docs`, GET returned 404, and the whole sync failed. None of the three unit tests
        // that construct a client can detect this; only a real connection to a bucket that holds a "folder" will show it.
        assert_eq!(objs.len(), 2, "目录占位对象没被滤掉：{keys:?}");

        let a = objs.iter().find(|o| o.filename == "a.txt").expect("a.txt");
        assert_eq!(a.bytes, b"hello", "取回的内容不对");
        assert!(
            a.last_modified.is_some(),
            "LastModified 是 doc_time 的唯一来源"
        );
        Ok(())
    }

    /// A real Azure Blob connection. **It is a different wire protocol from S3's**: signing algorithm, listing
    /// response shape, and container-versus-bucket semantics all differ, so sharing one builder abstraction does not mean
    /// sharing a path that works.
    ///
    /// Verified with Azurite (the official emulator); skipped without `UTOPIA_AZURE_TEST_ENDPOINT`:
    /// ```text
    /// docker run -d -p 10000:10000 mcr.microsoft.com/azure-storage/azurite \\
    ///   azurite-blob --blobHost 0.0.0.0 --skipApiVersionCheck
    /// # Seed data with rclone (rclone natively supports Azurite's development account)
    /// UTOPIA_AZURE_TEST_ENDPOINT=http://127.0.0.1:10000/devstoreaccount1 \\
    ///   cargo test -p utopia-server object_storage
    /// ```
    #[tokio::test]
    async fn it_reads_from_azure_blob() -> anyhow::Result<()> {
        let Ok(endpoint) = std::env::var("UTOPIA_AZURE_TEST_ENDPOINT") else {
            eprintln!("跳过：未设 UTOPIA_AZURE_TEST_ENDPOINT");
            return Ok(());
        };
        let cfg = serde_json::json!({
            "bucket": "utopia-conn-test",
            "endpoint": endpoint,
            "account_name": "devstoreaccount1",
            // Azurite's fixed development key. A public constant, not a credential
            "account_key": "Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw==",
        });
        let store = client("azure_blob", &cfg)?;
        let (objs, _) = fetch(
            "azure_blob",
            store.as_ref(),
            "utopia-conn-test",
            Some("docs"),
        )
        .await?;

        let a = objs.iter().find(|o| o.filename == "a.txt").expect("a.txt");
        assert_eq!(a.bytes, b"hello from azure");
        // **Identity must carry each vendor's own scheme**: a bucket with the same name on two clouds is two different things
        assert_eq!(a.external_key, "azure://utopia-conn-test/docs/a.txt");
        assert!(a.last_modified.is_some());
        Ok(())
    }

    /// GCS against a real bucket. Run on 2026-10-05 in us-central1: flat
    /// namespace, uniform access, a service account with only
    /// `Storage Object Viewer` on the bucket. It passed.
    ///
    /// **CI still skips it: there is no usable emulator.** `fsouza/fake-gcs-server`
    /// implements the JSON API (`/storage/v1/b/...`), while `object_store` lists
    /// over the XML API (`/bucket?list-type=2`), where it answers 404.
    ///
    /// The bucket needs two things under `docs/`:
    /// - `a.txt` containing `hello`
    /// - one folder made in the Cloud Console. The console writes a zero-byte
    ///   placeholder (`docs/x/`) and `object_store` hands it back as `docs/x`,
    ///   the same shape that took S3 sync down. Without one the test cannot
    ///   reach that path, so it fails instead of passing hollow.
    ///
    /// ```text
    /// UTOPIA_GCS_TEST_ENDPOINT=https://storage.googleapis.com \\
    ///   UTOPIA_GCS_TEST_BUCKET=your-bucket \\
    ///   UTOPIA_GCS_TEST_KEY="$(cat service-account.json)" \\
    ///   cargo test -p utopia-server object_storage
    /// ```
    #[tokio::test]
    async fn it_reads_from_gcs() -> anyhow::Result<()> {
        let (Ok(endpoint), Ok(bucket)) = (
            std::env::var("UTOPIA_GCS_TEST_ENDPOINT"),
            std::env::var("UTOPIA_GCS_TEST_BUCKET"),
        ) else {
            eprintln!("跳过：未设 UTOPIA_GCS_TEST_ENDPOINT / _BUCKET");
            return Ok(());
        };
        let mut cfg = serde_json::json!({ "bucket": bucket, "endpoint": endpoint });
        if let Ok(k) = std::env::var("UTOPIA_GCS_TEST_KEY") {
            cfg["service_account_key"] = serde_json::Value::String(k);
        }
        let store = client("gcs", &cfg)?;

        // Look at the raw listing first. `fetch` drops placeholders by size, so
        // its output alone cannot tell "filtered" from "never there".
        let prefix = StorePath::from("docs");
        let mut raw = store.list(Some(&prefix));
        let mut placeholders = 0usize;
        while let Some(meta) = raw.next().await {
            if meta?.size == 0 {
                placeholders += 1;
            }
        }
        assert!(
            placeholders > 0,
            "no zero-byte placeholder under docs/: make a folder in the Cloud Console"
        );

        let (objs, _) = fetch("gcs", store.as_ref(), &bucket, Some("docs")).await?;
        let keys: Vec<&str> = objs.iter().map(|o| o.external_key.as_str()).collect();
        assert!(
            objs.iter().all(|o| !o.bytes.is_empty()),
            "a placeholder reached fetch's output: {keys:?}"
        );

        let a = objs.iter().find(|o| o.filename == "a.txt").expect("a.txt");
        assert_eq!(a.bytes, b"hello", "wrong content read back");
        assert_eq!(a.external_key, format!("gs://{bucket}/docs/a.txt"));
        assert!(
            a.last_modified.is_some(),
            "LastModified is the only source of doc_time"
        );
        Ok(())
    }
}
