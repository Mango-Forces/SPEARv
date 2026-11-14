//! Anonymous access to the public SPHEREx bucket: listing, header reads and HEAD.

use std::time::Duration;

use async_trait::async_trait;
use reqwest::{Client, StatusCode};

const REQUEST_TIMEOUT_SECS: u64 = 30;
const ATTEMPTS: u64 = 3;
/// A listing longer than this is treated as broken rather than followed forever.
const MAX_PAGES: usize = 10_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Object {
    pub key: String,
    pub etag: Option<String>,
    pub size: Option<i64>,
}

/// One page of `ListObjectsV2`. `next` is the continuation token, `None` on the last page.
#[derive(Clone, Debug, Default)]
pub struct ListPage {
    pub objects: Vec<Object>,
    pub prefixes: Vec<String>,
    pub next: Option<String>,
}

/// What a HEAD says about one key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Head {
    Found,
    /// HTTP 404: the object is gone.
    Missing,
    /// Anything else, a timeout included. Proves nothing either way.
    Unknown(String),
}

#[async_trait]
pub trait Bucket: Send + Sync {
    async fn list(&self, prefix: &str, delimiter: Option<&str>, continuation: Option<&str>) -> Result<ListPage, String>;

    /// The first `bytes` bytes of one object.
    async fn read_prefix(&self, key: &str, bytes: u64) -> Result<Vec<u8>, String>;

    async fn head(&self, key: &str) -> Head;
}

/// The child folders of `prefix`, e.g. `qr3/` for `""`. All pages, or an error.
pub async fn list_children(bucket: &dyn Bucket, prefix: &str) -> Result<Vec<String>, String> {
    let mut continuation = None;
    let mut children = Vec::new();
    for _ in 0..MAX_PAGES {
        let page = bucket.list(prefix, Some("/"), continuation.as_deref()).await?;
        children.extend(page.prefixes.iter().filter_map(|p| child_name(p, prefix)));
        continuation = page.next;
        if continuation.is_none() {
            children.sort();
            children.dedup();
            return Ok(children);
        }
    }
    Err(format!("listing {prefix} did not finish after {MAX_PAGES} pages"))
}

/// Every object under `prefix`, at any depth. All pages, or an error.
pub async fn list_objects(bucket: &dyn Bucket, prefix: &str) -> Result<Vec<Object>, String> {
    let mut continuation = None;
    let mut objects = Vec::new();
    for _ in 0..MAX_PAGES {
        let page = bucket.list(prefix, None, continuation.as_deref()).await?;
        objects.extend(page.objects);
        continuation = page.next;
        if continuation.is_none() {
            return Ok(objects);
        }
    }
    Err(format!("listing {prefix} did not finish after {MAX_PAGES} pages"))
}

fn child_name(prefix: &str, parent: &str) -> Option<String> {
    let name = prefix.strip_prefix(parent)?.trim_end_matches('/');
    (!name.is_empty() && !name.contains('/')).then(|| name.to_string())
}

pub struct HttpBucket {
    client: Client,
    base: String,
}

impl HttpBucket {
    pub fn new(base: impl Into<String>, connections: usize) -> Result<Self, String> {
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(8))
            .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
            .pool_max_idle_per_host(connections.max(16))
            .user_agent("spearv-index-service (anonymous SPHEREx listing)")
            .build()
            .map_err(|err| err.to_string())?;
        Ok(Self { client, base: base.into().trim_end_matches('/').to_string() })
    }

    async fn fetch_list(&self, prefix: &str, delimiter: Option<&str>, continuation: Option<&str>) -> Result<ListPage, String> {
        let mut query = vec![("list-type", "2"), ("prefix", prefix)];
        if let Some(delimiter) = delimiter {
            query.push(("delimiter", delimiter));
        }
        if let Some(token) = continuation {
            query.push(("continuation-token", token));
        }
        let response = self
            .client
            .get(format!("{}/", self.base))
            .query(&query)
            .send()
            .await
            .map_err(|err| format!("list {prefix}: {}", chain(&err)))?;
        let status = response.status();
        if status != StatusCode::OK {
            return Err(format!("list {prefix}: HTTP {status}"));
        }
        let body = response.text().await.map_err(|err| format!("list {prefix}: {}", chain(&err)))?;
        parse_list(&body).map_err(|err| format!("list {prefix}: {err}"))
    }

    async fn fetch_prefix(&self, key: &str, bytes: u64) -> Result<Vec<u8>, String> {
        let response = self
            .client
            .get(format!("{}/{}", self.base, key))
            .header("Range", format!("bytes=0-{}", bytes.saturating_sub(1)))
            .send()
            .await
            .map_err(|err| format!("read {key}: {}", chain(&err)))?;
        let status = response.status();
        // A 200 would be the whole 65 MB object.
        if status != StatusCode::PARTIAL_CONTENT {
            return Err(format!("read {key}: HTTP {status}; refusing a full-object body"));
        }
        let body = response.bytes().await.map_err(|err| format!("read {key}: {}", chain(&err)))?;
        Ok(body.to_vec())
    }
}

#[async_trait]
impl Bucket for HttpBucket {
    async fn list(&self, prefix: &str, delimiter: Option<&str>, continuation: Option<&str>) -> Result<ListPage, String> {
        retry(|| self.fetch_list(prefix, delimiter, continuation)).await
    }

    /// S3 closes pooled connections, and a crawl makes a million of these reads.
    async fn read_prefix(&self, key: &str, bytes: u64) -> Result<Vec<u8>, String> {
        retry(|| self.fetch_prefix(key, bytes)).await
    }

    async fn head(&self, key: &str) -> Head {
        let mut last = String::new();
        for attempt in 1..=ATTEMPTS {
            match self.client.head(format!("{}/{}", self.base, key)).send().await {
                Ok(response) if response.status() == StatusCode::OK => return Head::Found,
                Ok(response) if response.status() == StatusCode::NOT_FOUND => return Head::Missing,
                // 403 is what S3 answers for a missing key when listing is denied.
                // Listing is allowed here, so it means something else: prove nothing.
                Ok(response) => last = format!("HTTP {}", response.status()),
                Err(err) => last = chain(&err),
            }
            tokio::time::sleep(Duration::from_millis(500 * attempt)).await;
        }
        Head::Unknown(last)
    }
}

async fn retry<T, F, Fut>(mut call: F) -> Result<T, String>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, String>>,
{
    let mut attempt = 1;
    loop {
        match call().await {
            Err(_) if attempt < ATTEMPTS => {
                tokio::time::sleep(Duration::from_millis(500 * attempt)).await;
                attempt += 1;
            }
            result => return result,
        }
    }
}

fn chain(err: &dyn std::error::Error) -> String {
    let mut text = err.to_string();
    let mut source = err.source();
    while let Some(cause) = source {
        text = format!("{text}: {cause}");
        source = cause.source();
    }
    text
}

fn parse_list(xml: &str) -> Result<ListPage, String> {
    let mut page = ListPage::default();
    for block in blocks(xml, "Contents") {
        let Some(key) = inner_text(block, "Key") else { continue };
        page.objects.push(Object {
            key,
            etag: inner_text(block, "ETag"),
            size: inner_text(block, "Size").and_then(|size| size.parse().ok()),
        });
    }
    for block in blocks(xml, "CommonPrefixes") {
        if let Some(prefix) = inner_text(block, "Prefix") {
            page.prefixes.push(prefix);
        }
    }
    if !xml.contains("<ListBucketResult") {
        return Err("not a ListBucketResult".into());
    }
    let truncated = inner_text(xml, "IsTruncated").is_some_and(|value| value.eq_ignore_ascii_case("true"));
    if truncated {
        page.next = inner_text(xml, "NextContinuationToken");
        if page.next.is_none() {
            return Err("listing says truncated but has no NextContinuationToken".into());
        }
    }
    Ok(page)
}

fn blocks<'a>(xml: &'a str, tag: &str) -> Vec<&'a str> {
    let (open, close) = (format!("<{tag}>"), format!("</{tag}>"));
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find(&open) {
        let after = &rest[start + open.len()..];
        let Some(end) = after.find(&close) else { break };
        out.push(&after[..end]);
        rest = &after[end + close.len()..];
    }
    out
}

fn inner_text(xml: &str, tag: &str) -> Option<String> {
    let (open, close) = (format!("<{tag}>"), format!("</{tag}>"));
    let start = xml.find(&open)?;
    let after = &xml[start + open.len()..];
    let end = after.find(&close)?;
    Some(decode_xml(&after[..end]))
}

fn decode_xml(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('&') {
        out.push_str(&rest[..start]);
        let after = &rest[start..];
        let Some(end) = after.find(';') else {
            out.push_str(after);
            return out;
        };
        let entity = &after[1..end];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => entity
                .strip_prefix("#x")
                .or_else(|| entity.strip_prefix("#X"))
                .and_then(|hex| u32::from_str_radix(hex, 16).ok())
                .or_else(|| entity.strip_prefix('#').and_then(|dec| dec.parse().ok()))
                .and_then(char::from_u32),
        };
        match decoded {
            Some(character) => out.push(character),
            None => out.push_str(&after[..=end]),
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_listing_page_carries_keys_etags_sizes_and_the_token() {
        let page = parse_list(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?><ListBucketResult><IsTruncated>true</IsTruncated>\
             <Contents><Key>qr3/level2/a.fits</Key><LastModified>2026-10-13T10:57:58.000Z</LastModified>\
             <ETag>&quot;5d41402abc4b2a76b9719d911017c592-8&quot;</ETag><Size>65131200</Size></Contents>\
             <CommonPrefixes><Prefix>qr4/</Prefix></CommonPrefixes>\
             <NextContinuationToken>tok&amp;en</NextContinuationToken></ListBucketResult>",
        )
        .unwrap();
        assert_eq!(
            page.objects,
            vec![Object {
                key: "qr3/level2/a.fits".into(),
                etag: Some("\"5d41402abc4b2a76b9719d911017c592-8\"".into()),
                size: Some(65_131_200),
            }]
        );
        assert_eq!(page.prefixes, vec!["qr4/"]);
        assert_eq!(page.next.as_deref(), Some("tok&en"));
    }

    #[test]
    fn a_truncated_page_without_a_token_or_an_error_body_is_refused() {
        assert!(parse_list("<ListBucketResult><IsTruncated>true</IsTruncated></ListBucketResult>").is_err());
        assert!(parse_list("<Error><Code>SlowDown</Code></Error>").is_err());
        let last = parse_list("<ListBucketResult><IsTruncated>false</IsTruncated></ListBucketResult>").unwrap();
        assert!(last.next.is_none() && last.objects.is_empty());
    }
}
