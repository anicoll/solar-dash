//! A small S3-compatible client for offsite backups (Cloudflare R2 and other
//! stores that speak the S3 API): upload, list and delete objects.
//!
//! It signs requests with AWS Signature Version 4, written by hand on top of
//! `hmac` and `sha2`, so the program does not need a whole AWS SDK for three
//! calls. Requests use path-style URLs, `https://<host>/<bucket>/<key>`, and
//! the region `auto` that R2 expects. Uploads are signed with the real SHA-256
//! of the body (not `UNSIGNED-PAYLOAD`), so a file damaged on the way is
//! rejected by the store.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::Utc;
use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};

use crate::backup;
use crate::config;

/// R2 ignores the region but requires `auto` in the signature.
pub const REGION: &str = "auto";
const SERVICE: &str = "s3";

/// The access key pair used to sign requests.
#[derive(Clone)]
pub struct Credentials {
    pub access_key_id: String,
    pub secret_access_key: String,
}

// Written by hand to keep the secret out of logs.
impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("access_key_id", &self.access_key_id)
            .field("secret_access_key", &"<redacted>")
            .finish()
    }
}

fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts keys of any length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

/// Percent-encodes `text` the way SigV4 wants: everything except the unreserved
/// characters `A-Z a-z 0-9 - _ . ~` (and `/` when `keep_slash`).
fn uri_encode(text: &str, keep_slash: bool) -> String {
    let mut out = String::with_capacity(text.len());
    for &byte in text.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            b'/' if keep_slash => out.push('/'),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// What is needed to sign one request.
struct SignInput<'a> {
    method: &'a str,
    /// The absolute path, not yet encoded, such as `/bucket/solar/a b.db`.
    path: &'a str,
    /// Query parameters, not yet encoded, in any order.
    query: &'a [(&'a str, &'a str)],
    /// Headers to sign, in any order; names in any case. Must include `host`
    /// and `x-amz-date`.
    headers: &'a [(&'a str, &'a str)],
    /// Lowercase hex SHA-256 of the body.
    payload_hash: &'a str,
    /// `YYYYMMDDTHHMMSSZ`.
    amz_date: &'a str,
}

/// Returns the value of the `Authorization` header for a request.
fn authorization(creds: &Credentials, region: &str, input: &SignInput) -> String {
    let mut query: Vec<(String, String)> = input
        .query
        .iter()
        .map(|(k, v)| (uri_encode(k, false), uri_encode(v, false)))
        .collect();
    query.sort();
    let canonical_query = query
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&");

    let mut headers: Vec<(String, String)> = input
        .headers
        .iter()
        .map(|(k, v)| (k.to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    headers.sort();
    let canonical_headers: String = headers.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
    let signed_headers = headers
        .iter()
        .map(|(k, _)| k.as_str())
        .collect::<Vec<_>>()
        .join(";");

    let canonical_request = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        input.method,
        uri_encode(input.path, true),
        canonical_query,
        canonical_headers,
        signed_headers,
        input.payload_hash
    );

    let day = &input.amz_date[..8];
    let scope = format!("{day}/{region}/{SERVICE}/aws4_request");
    let to_sign = format!(
        "AWS4-HMAC-SHA256\n{}\n{scope}\n{}",
        input.amz_date,
        sha256_hex(canonical_request.as_bytes())
    );

    let k_date = hmac_sha256(
        format!("AWS4{}", creds.secret_access_key).as_bytes(),
        day.as_bytes(),
    );
    let k_region = hmac_sha256(&k_date, region.as_bytes());
    let k_service = hmac_sha256(&k_region, SERVICE.as_bytes());
    let k_signing = hmac_sha256(&k_service, b"aws4_request");
    let signature = hex::encode(hmac_sha256(&k_signing, to_sign.as_bytes()));

    format!(
        "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
        creds.access_key_id
    )
}

/// One page of a `ListObjectsV2` answer.
#[derive(Debug, PartialEq)]
struct ListPage {
    keys: Vec<String>,
    /// Set when more pages follow.
    next_token: Option<String>,
}

/// The text between `<tag>` and `</tag>` in `xml`, if present.
fn tag_text<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
    let start = xml.find(&format!("<{tag}>"))? + tag.len() + 2;
    let end = xml[start..].find(&format!("</{tag}>"))? + start;
    Some(&xml[start..end])
}

/// Undoes the five XML character escapes (`&amp;` last, so `&amp;lt;` stays
/// `&lt;`).
fn xml_unescape(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// Reads the keys, `IsTruncated` and `NextContinuationToken` from a
/// `ListObjectsV2` answer. Only what listing needs; not a general XML parser.
fn parse_list(xml: &str) -> Result<ListPage> {
    anyhow::ensure!(
        xml.contains("<ListBucketResult"),
        "the store's answer is not a bucket listing"
    );
    let mut keys = Vec::new();
    // Everything outside the `<Contents>` blocks; the paging fields are in
    // here (S3 puts them before the objects, others after).
    let mut outside = String::new();
    let mut rest = xml;
    while let Some(start) = rest.find("<Contents>") {
        outside.push_str(&rest[..start]);
        let after = &rest[start + "<Contents>".len()..];
        let end = after
            .find("</Contents>")
            .context("the bucket listing is cut off")?;
        let key = tag_text(&after[..end], "Key").context("a listed object has no key")?;
        keys.push(xml_unescape(key));
        rest = &after[end + "</Contents>".len()..];
    }
    outside.push_str(rest);
    let tail = outside.as_str();
    let truncated = tag_text(tail, "IsTruncated") == Some("true");
    let next_token = if truncated {
        let token = tag_text(tail, "NextContinuationToken")
            .context("the listing is truncated but has no continuation token")?;
        Some(xml_unescape(token))
    } else {
        None
    };
    Ok(ListPage { keys, next_token })
}

/// A client for one bucket.
pub struct Client {
    http: reqwest::Client,
    /// `scheme://host[:port]`, no trailing slash.
    endpoint: String,
    /// `host[:port]`, for the `Host` header.
    host: String,
    bucket: String,
    creds: Credentials,
}

impl Client {
    /// `endpoint` is `scheme://host[:port]`, such as
    /// `https://<account>.r2.cloudflarestorage.com`.
    pub fn new(endpoint: &str, bucket: &str, creds: Credentials) -> Result<Self> {
        let endpoint = endpoint.trim_end_matches('/');
        let host = endpoint
            .split_once("://")
            .map(|(_, rest)| rest)
            .filter(|host| !host.is_empty() && !host.contains('/'))
            .with_context(|| {
                format!("the endpoint {endpoint:?} must look like https://host (no path)")
            })?;
        anyhow::ensure!(!bucket.is_empty(), "the bucket name is empty");
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(20))
            // Room for a slow upload of a database of tens of MB.
            .timeout(Duration::from_secs(900))
            .build()
            .context("could not build the HTTP client")?;
        Ok(Self {
            http,
            host: host.to_string(),
            endpoint: endpoint.to_string(),
            bucket: bucket.to_string(),
            creds,
        })
    }

    /// Signs and sends one request, and returns the answer body on a 2xx.
    async fn send(
        &self,
        method: reqwest::Method,
        key: Option<&str>,
        query: &[(&str, &str)],
        body: Vec<u8>,
    ) -> Result<String> {
        let path = match key {
            Some(key) => format!("/{}/{key}", self.bucket),
            None => format!("/{}", self.bucket),
        };
        let amz_date = Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
        let payload_hash = sha256_hex(&body);
        let auth = authorization(
            &self.creds,
            REGION,
            &SignInput {
                method: method.as_str(),
                path: &path,
                query,
                headers: &[
                    ("host", &self.host),
                    ("x-amz-content-sha256", &payload_hash),
                    ("x-amz-date", &amz_date),
                ],
                payload_hash: &payload_hash,
                amz_date: &amz_date,
            },
        );
        let mut url = format!("{}{}", self.endpoint, uri_encode(&path, true));
        for (i, (k, v)) in query.iter().enumerate() {
            url.push(if i == 0 { '?' } else { '&' });
            url.push_str(&format!(
                "{}={}",
                uri_encode(k, false),
                uri_encode(v, false)
            ));
        }
        let response = self
            .http
            .request(method, &url)
            .header("x-amz-content-sha256", &payload_hash)
            .header("x-amz-date", &amz_date)
            .header("authorization", auth)
            .body(body)
            .send()
            .await
            .context("could not reach the store")?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            let snippet: String = text.chars().take(300).collect();
            bail!("the store answered {status}: {snippet}");
        }
        Ok(text)
    }

    /// Uploads `body` as `key` in a single request, replacing any object there.
    pub async fn put_object(&self, key: &str, body: Vec<u8>) -> Result<()> {
        self.send(reqwest::Method::PUT, Some(key), &[], body)
            .await
            .with_context(|| format!("could not upload {key}"))?;
        Ok(())
    }

    /// The keys of all objects whose key starts with `prefix`, following
    /// continuation tokens until the listing is complete.
    pub async fn list_objects(&self, prefix: &str) -> Result<Vec<String>> {
        let mut keys = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut query = vec![("list-type", "2"), ("prefix", prefix)];
            if let Some(token) = &token {
                query.push(("continuation-token", token));
            }
            let xml = self
                .send(reqwest::Method::GET, None, &query, Vec::new())
                .await
                .context("could not list the bucket")?;
            let page = parse_list(&xml)?;
            keys.extend(page.keys);
            match page.next_token {
                Some(next) => token = Some(next),
                None => return Ok(keys),
            }
        }
    }

    /// Deletes `key`. Deleting a key that does not exist succeeds.
    pub async fn delete_object(&self, key: &str) -> Result<()> {
        self.send(reqwest::Method::DELETE, Some(key), &[], Vec::new())
            .await
            .with_context(|| format!("could not delete {key}"))?;
        Ok(())
    }
}

/// What one upload did.
#[derive(Debug, PartialEq)]
pub struct Synced {
    /// The object that was uploaded.
    pub key: String,
    /// Old backups removed from the bucket to stay within `keep`.
    pub deleted: Vec<String>,
    /// How many backups the bucket holds now.
    pub count: usize,
}

/// The backups in a bucket, kept to the newest `keep`.
///
/// Pruning counts backups instead of using an R2 lifecycle rule on purpose. An
/// age-based rule keeps deleting whether or not new backups arrive, so if the
/// server stopped uploading (down for weeks, a broken token) the bucket would
/// slowly empty, exactly when a copy is needed. Counting only deletes when a
/// new backup has just been stored, so the newest `keep` survive however old
/// they are.
pub struct Offsite {
    client: Client,
    /// Object names start with this; empty or ending in `/`.
    prefix: String,
    keep: usize,
}

impl Offsite {
    pub fn new(settings: &config::Offsite) -> Result<Self> {
        let client = Client::new(
            &settings.endpoint(),
            &settings.bucket,
            Credentials {
                access_key_id: settings.access_key_id.clone(),
                secret_access_key: settings.secret_access_key.clone(),
            },
        )?;
        Ok(Self {
            client,
            prefix: settings.prefix(),
            keep: settings.keep,
        })
    }

    fn key_for(&self, path: &Path) -> Result<String> {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .with_context(|| format!("{} has no usable file name", path.display()))?;
        Ok(format!("{}{name}", self.prefix))
    }

    /// The backups among `keys`: those named `<prefix>solar-YYYY-MM-DD.db`
    /// exactly, oldest first. Anything else in the bucket is not ours.
    fn backups<'a>(&self, keys: &'a [String]) -> Vec<(chrono::NaiveDate, &'a str)> {
        let mut found: Vec<_> = keys
            .iter()
            .filter_map(|key| {
                let name = key.strip_prefix(&self.prefix)?;
                Some((backup::date_of(name)?, key.as_str()))
            })
            .collect();
        found.sort();
        found
    }

    /// Uploads the backup at `path`, then deletes the oldest backups in the
    /// bucket beyond `keep`.
    pub async fn upload_and_prune(&self, path: &Path) -> Result<Synced> {
        let key = self.key_for(path)?;
        let body = tokio::fs::read(path)
            .await
            .with_context(|| format!("could not read {}", path.display()))?;
        self.client.put_object(&key, body).await?;
        self.prune(key).await
    }

    /// Uploads the backup at `path` only if the bucket has no object for it
    /// (say an earlier upload failed). Returns `None` when it was already there.
    pub async fn upload_if_missing(&self, path: &Path) -> Result<Option<Synced>> {
        let key = self.key_for(path)?;
        let keys = self.client.list_objects(&self.prefix).await?;
        if keys.contains(&key) {
            return Ok(None);
        }
        self.upload_and_prune(path).await.map(Some)
    }

    /// Deletes backups beyond `keep`, oldest first (by the date in the name).
    async fn prune(&self, just_uploaded: String) -> Result<Synced> {
        let keys = self
            .client
            .list_objects(&self.prefix)
            .await
            .context("uploaded, but could not list the bucket to prune it")?;
        let backups = self.backups(&keys);
        let surplus = backups.len().saturating_sub(self.keep);
        let mut deleted = Vec::new();
        for (_, key) in backups.iter().take(surplus) {
            self.client
                .delete_object(key)
                .await
                .context("uploaded, but could not delete an old backup")?;
            deleted.push(key.to_string());
        }
        Ok(Synced {
            key: just_uploaded,
            count: backups.len() - deleted.len(),
            deleted,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::Bytes;
    use axum::extract::{Path, Query, State};
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::get;
    use std::collections::{BTreeMap, HashMap};
    use std::sync::{Arc, Mutex};

    const EXAMPLE_KEY: &str = "AKIAIOSFODNN7EXAMPLE";
    const EXAMPLE_SECRET: &str = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
    const EMPTY_HASH: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    fn example_creds() -> Credentials {
        Credentials {
            access_key_id: EXAMPLE_KEY.into(),
            secret_access_key: EXAMPLE_SECRET.into(),
        }
    }

    // The "GET Object" example from AWS's SigV4 documentation for S3.
    #[test]
    fn signs_the_aws_get_object_example() {
        let auth = authorization(
            &example_creds(),
            "us-east-1",
            &SignInput {
                method: "GET",
                path: "/test.txt",
                query: &[],
                headers: &[
                    ("Host", "examplebucket.s3.amazonaws.com"),
                    ("Range", "bytes=0-9"),
                    ("x-amz-content-sha256", EMPTY_HASH),
                    ("x-amz-date", "20130524T000000Z"),
                ],
                payload_hash: EMPTY_HASH,
                amz_date: "20130524T000000Z",
            },
        );
        assert_eq!(
            auth,
            "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request, \
             SignedHeaders=host;range;x-amz-content-sha256;x-amz-date, \
             Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
        );
    }

    // The "GET Bucket (List Objects)" example from the same page; it has a
    // query string.
    #[test]
    fn signs_the_aws_list_objects_example() {
        let auth = authorization(
            &example_creds(),
            "us-east-1",
            &SignInput {
                method: "GET",
                path: "/",
                query: &[("prefix", "J"), ("max-keys", "2")],
                headers: &[
                    ("host", "examplebucket.s3.amazonaws.com"),
                    ("x-amz-content-sha256", EMPTY_HASH),
                    ("x-amz-date", "20130524T000000Z"),
                ],
                payload_hash: EMPTY_HASH,
                amz_date: "20130524T000000Z",
            },
        );
        assert!(auth.ends_with(
            "Signature=34b48302e7b5fa45bde8084f4b7868a86f0a534bc59db6670ed5711ef69dc6f7"
        ));
    }

    #[test]
    fn encodes_like_sigv4() {
        assert_eq!(uri_encode("a b/c+d~e", true), "a%20b/c%2Bd~e");
        assert_eq!(uri_encode("a/b", false), "a%2Fb");
    }

    #[test]
    fn parses_a_listing() {
        let xml = "<?xml version=\"1.0\"?><ListBucketResult><Name>b</Name><Prefix>p/</Prefix>\
                   <KeyCount>2</KeyCount><IsTruncated>true</IsTruncated>\
                   <Contents><Key>p/a&amp;b.db</Key><Size>1</Size></Contents>\
                   <Contents><Key>p/c.db</Key></Contents>\
                   <NextContinuationToken>tok&amp;1</NextContinuationToken></ListBucketResult>";
        assert_eq!(
            parse_list(xml).unwrap(),
            ListPage {
                keys: vec!["p/a&b.db".into(), "p/c.db".into()],
                next_token: Some("tok&1".into()),
            }
        );
        let last = "<ListBucketResult><IsTruncated>false</IsTruncated></ListBucketResult>";
        assert_eq!(
            parse_list(last).unwrap(),
            ListPage {
                keys: vec![],
                next_token: None
            }
        );
        assert!(parse_list("<html>nope</html>").is_err());
        assert!(
            parse_list("<ListBucketResult><IsTruncated>true</IsTruncated></ListBucketResult>")
                .is_err()
        );
    }

    /// A tiny in-memory bucket that checks the headers it is sent.
    #[derive(Clone, Default)]
    struct Store {
        objects: Arc<Mutex<BTreeMap<String, Vec<u8>>>>,
        /// Objects per listing page.
        page_size: usize,
    }

    fn check_signed(headers: &HeaderMap, body: &[u8]) {
        let auth = headers["authorization"].to_str().unwrap();
        assert!(
            auth.starts_with("AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/"),
            "{auth}"
        );
        assert!(auth.contains("/auto/s3/aws4_request"), "{auth}");
        assert!(auth.contains("SignedHeaders=host;x-amz-content-sha256;x-amz-date,"));
        assert_eq!(
            headers["x-amz-content-sha256"].to_str().unwrap(),
            sha256_hex(body)
        );
    }

    async fn put(
        State(store): State<Store>,
        Path(path): Path<String>,
        headers: HeaderMap,
        body: Bytes,
    ) -> StatusCode {
        check_signed(&headers, &body);
        store.objects.lock().unwrap().insert(path, body.to_vec());
        StatusCode::OK
    }

    async fn delete(
        State(store): State<Store>,
        Path(path): Path<String>,
        headers: HeaderMap,
    ) -> StatusCode {
        check_signed(&headers, b"");
        store.objects.lock().unwrap().remove(&path);
        StatusCode::NO_CONTENT
    }

    async fn list(
        State(store): State<Store>,
        Query(query): Query<HashMap<String, String>>,
        headers: HeaderMap,
    ) -> String {
        check_signed(&headers, b"");
        assert_eq!(query["list-type"], "2");
        let prefix = format!("bkt/{}", query["prefix"]);
        let after = query.get("continuation-token").cloned().unwrap_or_default();
        let all: Vec<String> = store
            .objects
            .lock()
            .unwrap()
            .keys()
            .filter(|k| k.starts_with(&prefix) && **k > after)
            .cloned()
            .collect();
        let page: Vec<&String> = all.iter().take(store.page_size).collect();
        let mut xml = String::from("<?xml version=\"1.0\"?><ListBucketResult>");
        for key in &page {
            let key = key.strip_prefix("bkt/").unwrap().replace('&', "&amp;");
            xml.push_str(&format!("<Contents><Key>{key}</Key></Contents>"));
        }
        if all.len() > page.len() {
            xml.push_str(&format!(
                "<IsTruncated>true</IsTruncated><NextContinuationToken>{}</NextContinuationToken>",
                page.last().unwrap()
            ));
        } else {
            xml.push_str("<IsTruncated>false</IsTruncated>");
        }
        xml.push_str("</ListBucketResult>");
        xml
    }

    /// Starts the mock store on a free port and returns a client for it.
    async fn mock(page_size: usize) -> (Client, Store) {
        let store = Store {
            page_size,
            ..Store::default()
        };
        let app = Router::new()
            .route("/bkt", get(list))
            .route("/bkt/", get(list))
            .route("/{*path}", axum::routing::put(put).delete(delete))
            .with_state(store.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = Client::new(&format!("http://{addr}"), "bkt", example_creds()).unwrap();
        (client, store)
    }

    #[tokio::test]
    async fn puts_lists_and_deletes() {
        let (client, store) = mock(2).await;
        for day in 1..=5 {
            client
                .put_object(&format!("p/solar-2026-10-0{day}.db"), vec![day; 10])
                .await
                .unwrap();
        }
        client
            .put_object("other/x&y.txt", b"hi".to_vec())
            .await
            .unwrap();
        assert_eq!(
            store.objects.lock().unwrap()["bkt/p/solar-2026-10-03.db"],
            vec![3; 10]
        );

        // Five objects at two per page: three round trips, one listing.
        let keys = client.list_objects("p/").await.unwrap();
        assert_eq!(keys.len(), 5);
        assert_eq!(keys[0], "p/solar-2026-10-01.db");
        assert_eq!(keys[4], "p/solar-2026-10-05.db");
        assert_eq!(
            client.list_objects("other/").await.unwrap(),
            ["other/x&y.txt"]
        );

        client.delete_object("p/solar-2026-10-01.db").await.unwrap();
        assert_eq!(client.list_objects("p/").await.unwrap().len(), 4);
    }

    #[tokio::test]
    async fn store_errors_are_reported() {
        let (client, _) = mock(10).await;
        // Nothing listens on this route, so the mock answers 405.
        let err = client
            .send(reqwest::Method::GET, Some("a/b"), &[], vec![])
            .await;
        assert!(format!("{:#}", err.unwrap_err()).contains("405"));
        assert!(Client::new("not a url", "b", example_creds()).is_err());
        assert!(Client::new("https://h.example/path", "b", example_creds()).is_err());
    }

    #[test]
    fn debug_hides_the_secret() {
        let text = format!("{:?}", example_creds());
        assert!(!text.contains("wJalr"));
        assert!(text.contains("AKIAIOSFODNN7EXAMPLE"));
    }

    fn offsite_for(client: Client, prefix: &str, keep: usize) -> Offsite {
        Offsite {
            client,
            prefix: prefix.to_string(),
            keep,
        }
    }

    #[tokio::test]
    async fn keeps_only_the_newest_backups_and_leaves_other_objects() {
        let (client, store) = mock(2).await;
        let offsite = offsite_for(client, "p/", 3);
        // Objects that are not backups, or are under another prefix.
        for key in [
            "p/notes.txt",
            "p/pre-update-20260101-000000.db",
            "q/solar-2026-01-01.db",
        ] {
            offsite.client.put_object(key, b"x".to_vec()).await.unwrap();
        }
        let dir = tempfile::tempdir().unwrap();
        // Uploaded out of order: the newest by date win, not the last uploaded.
        for day in [5, 1, 2, 4, 3] {
            let path = dir.path().join(format!("solar-2026-10-0{day}.db"));
            std::fs::write(&path, vec![day; 4]).unwrap();
            let synced = offsite.upload_and_prune(&path).await.unwrap();
            assert_eq!(synced.key, format!("p/solar-2026-10-0{day}.db"));
        }
        let keys: Vec<String> = store.objects.lock().unwrap().keys().cloned().collect();
        let left: Vec<&str> = keys
            .iter()
            .map(|k| k.strip_prefix("bkt/").unwrap())
            .collect();
        assert_eq!(
            left,
            [
                "p/notes.txt",
                "p/pre-update-20260101-000000.db",
                "p/solar-2026-10-03.db",
                "p/solar-2026-10-04.db",
                "p/solar-2026-10-05.db",
                "q/solar-2026-01-01.db",
            ]
        );
    }

    #[tokio::test]
    async fn reports_what_was_deleted() {
        let (client, _) = mock(10).await;
        let offsite = offsite_for(client, "p/", 2);
        let dir = tempfile::tempdir().unwrap();
        let put = |name: &str| {
            let path = dir.path().join(name);
            std::fs::write(&path, "x").unwrap();
            path
        };
        offsite
            .upload_and_prune(&put("solar-2026-10-02.db"))
            .await
            .unwrap();
        offsite
            .upload_and_prune(&put("solar-2026-10-03.db"))
            .await
            .unwrap();
        let synced = offsite
            .upload_and_prune(&put("solar-2026-10-04.db"))
            .await
            .unwrap();
        assert_eq!(synced.deleted, ["p/solar-2026-10-02.db"]);
        assert_eq!(synced.count, 2);
        // The rule is the newest `keep` by date, so a file older than all the
        // others is removed again at once.
        let synced = offsite
            .upload_and_prune(&put("solar-2026-09-01.db"))
            .await
            .unwrap();
        assert_eq!(synced.deleted, ["p/solar-2026-09-01.db"]);
        assert_eq!(synced.count, 2);
    }

    #[tokio::test]
    async fn uploads_a_missing_backup_only_once() {
        let (client, store) = mock(10).await;
        let offsite = offsite_for(client, "", 15);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("solar-2026-10-04.db");
        std::fs::write(&path, "data").unwrap();
        let first = offsite.upload_if_missing(&path).await.unwrap();
        assert_eq!(first.unwrap().key, "solar-2026-10-04.db");
        assert_eq!(
            store.objects.lock().unwrap()["bkt/solar-2026-10-04.db"],
            b"data"
        );
        assert_eq!(offsite.upload_if_missing(&path).await.unwrap(), None);
    }
}
