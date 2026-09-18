//! Microsoft Graph HTTP client: bearer auth, throttling (429/`Retry-After`),
//! 5xx backoff, one-shot 401 retry, `@odata.nextLink` pagination, and a delta
//! helper.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use bytes::Bytes;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::auth::Authenticator;

const MAX_RETRIES: u32 = 5;

#[derive(Clone)]
pub struct GraphClient {
    http: reqwest::Client,
    auth: Arc<Authenticator>,
    base: String,
}

/// A page of a delta query: the changed items plus the token used to fetch the
/// next batch of changes on the following poll.
#[derive(Debug, Clone)]
pub struct DeltaPage<T> {
    pub items: Vec<T>,
    /// `@odata.deltaLink` — pass back to [`GraphClient::delta`] next time.
    pub delta_link: Option<String>,
}

impl GraphClient {
    pub fn new(auth: Arc<Authenticator>) -> Self {
        Self {
            http: reqwest::Client::builder()
                .user_agent(concat!("ask-ark/", env!("CARGO_PKG_VERSION")))
                .build()
                .expect("building reqwest client"),
            auth,
            base: crate::config::graph_base(),
        }
    }

    fn url(&self, path: &str) -> String {
        if path.starts_with("http") {
            path.to_string()
        } else {
            format!("{}/{}", self.base, path.trim_start_matches('/'))
        }
    }

    /// Core request with auth, throttling, and retry. Returns the raw response
    /// body bytes on success (empty for 204).
    async fn send(
        &self,
        method: reqwest::Method,
        url: &str,
        body: Option<&Value>,
    ) -> Result<Vec<u8>> {
        let mut attempt = 0u32;
        let mut refreshed_once = false;

        loop {
            let token = self.auth.access_token().await?;
            let mut req = self
                .http
                .request(method.clone(), url)
                .bearer_auth(&token)
                .header(reqwest::header::ACCEPT, "application/json");
            if let Some(b) = body {
                req = req.json(b);
            }

            let resp = req.send().await.context("sending Graph request")?;
            let status = resp.status();

            // 401 once -> force a refresh by discarding the token and retrying.
            if status == reqwest::StatusCode::UNAUTHORIZED && !refreshed_once {
                refreshed_once = true;
                // access_token() will refresh on its own next loop iteration if
                // the cached token is (now) considered expired; nudge by retry.
                continue;
            }

            if status == reqwest::StatusCode::TOO_MANY_REQUESTS
                || status.is_server_error()
            {
                if attempt >= MAX_RETRIES {
                    let text = resp.text().await.unwrap_or_default();
                    anyhow::bail!("Graph request failed after {MAX_RETRIES} retries ({status}): {text}");
                }
                let wait = retry_after(&resp).unwrap_or_else(|| backoff(attempt));
                attempt += 1;
                tracing::debug!("Graph {status}; retrying in {:?} (attempt {attempt})", wait);
                tokio::time::sleep(wait).await;
                continue;
            }

            if !status.is_success() {
                let text = resp.text().await.unwrap_or_default();
                anyhow::bail!("Graph request failed ({status}): {text}");
            }

            return Ok(resp.bytes().await.context("reading Graph body")?.to_vec());
        }
    }

    pub async fn get_json<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        let url = self.url(path);
        let bytes = self.send(reqwest::Method::GET, &url, None).await?;
        serde_json::from_slice(&bytes).context("deserializing Graph response")
    }

    /// GET returning the raw body — used for attachment `$value` downloads.
    pub async fn get_bytes(&self, path: &str) -> Result<Vec<u8>> {
        let url = self.url(path);
        self.send(reqwest::Method::GET, &url, None).await
    }

    /// PUT one chunk to an upload session URL. Those URLs carry their own
    /// authorization, so the bearer token is deliberately not sent.
    /// Returns true once the upload is complete.
    ///
    /// Takes [`Bytes`] rather than a slice: chunks are refcounted views into the
    /// one buffer holding the file, so no chunk is ever copied. `Content-Length`
    /// is left to reqwest, which derives it from the body — setting it here as
    /// well would append a second, conflicting header.
    pub async fn put_upload_chunk(
        &self,
        upload_url: &str,
        start: u64,
        total: u64,
        chunk: Bytes,
    ) -> Result<bool> {
        let end = start + chunk.len() as u64 - 1;
        let resp = self
            .http
            .put(upload_url)
            .header(
                reqwest::header::CONTENT_RANGE,
                format!("bytes {start}-{end}/{total}"),
            )
            .body(chunk)
            .send()
            .await
            .context("uploading attachment chunk")?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("attachment upload failed ({status}): {text}");
        }
        // 201/200 = finished, 202 = more chunks expected.
        Ok(status != reqwest::StatusCode::ACCEPTED)
    }

    pub async fn post_json<T: DeserializeOwned>(&self, path: &str, body: &Value) -> Result<T> {
        let url = self.url(path);
        let bytes = self
            .send(reqwest::Method::POST, &url, Some(body))
            .await?;
        if bytes.is_empty() {
            // Some POSTs (e.g. sendMail) return 202 with no body.
            return serde_json::from_value(Value::Null).context("empty response");
        }
        serde_json::from_slice(&bytes).context("deserializing Graph response")
    }

    /// POST that ignores the response body (202/204 actions like `sendMail`).
    pub async fn post_action(&self, path: &str, body: &Value) -> Result<()> {
        let url = self.url(path);
        self.send(reqwest::Method::POST, &url, Some(body)).await?;
        Ok(())
    }

    pub async fn patch(&self, path: &str, body: &Value) -> Result<()> {
        let url = self.url(path);
        self.send(reqwest::Method::PATCH, &url, Some(body)).await?;
        Ok(())
    }

    pub async fn delete(&self, path: &str) -> Result<()> {
        let url = self.url(path);
        self.send(reqwest::Method::DELETE, &url, None).await?;
        Ok(())
    }

    /// Fetch a single page of a collection — does **not** follow
    /// `@odata.nextLink`. Use for bounded lists where `$top` already caps the
    /// result (e.g. an inbox view); avoids walking the entire mailbox.
    pub async fn get_page<T: DeserializeOwned>(&self, path: &str) -> Result<Vec<T>> {
        Ok(self.get_page_with_next(path).await?.0)
    }

    /// Like [`GraphClient::get_page`], but also returns the `@odata.nextLink`
    /// (if any) so callers can implement incremental "load more" scrolling.
    pub async fn get_page_with_next<T: DeserializeOwned>(
        &self,
        path: &str,
    ) -> Result<(Vec<T>, Option<String>)> {
        let url = self.url(path);
        let bytes = self.send(reqwest::Method::GET, &url, None).await?;
        let page: ODataPage<T> =
            serde_json::from_slice(&bytes).context("deserializing Graph page")?;
        Ok((page.value, page.next_link))
    }

    /// Fetch a collection, transparently following `@odata.nextLink` until the
    /// whole result set is retrieved.
    pub async fn get_collection<T: DeserializeOwned>(&self, path: &str) -> Result<Vec<T>> {
        let mut out = Vec::new();
        let mut next = Some(self.url(path));
        while let Some(url) = next {
            let bytes = self.send(reqwest::Method::GET, &url, None).await?;
            let page: ODataPage<T> = serde_json::from_slice(&bytes)
                .context("deserializing Graph collection page")?;
            out.extend(page.value);
            next = page.next_link;
        }
        Ok(out)
    }

    /// Run a delta query. Pass the resource delta path on first call (e.g.
    /// `me/mailFolders/inbox/messages/delta`) or a stored `@odata.deltaLink` on
    /// subsequent polls. Follows `@odata.nextLink` and returns the final
    /// `@odata.deltaLink`.
    pub async fn delta<T: DeserializeOwned>(&self, path: &str) -> Result<DeltaPage<T>> {
        let mut items = Vec::new();
        let mut next = Some(self.url(path));
        let mut delta_link = None;
        while let Some(url) = next {
            let bytes = self.send(reqwest::Method::GET, &url, None).await?;
            let page: ODataDeltaPage<T> = serde_json::from_slice(&bytes)
                .context("deserializing Graph delta page")?;
            items.extend(page.value);
            delta_link = page.delta_link.or(delta_link);
            next = page.next_link;
        }
        Ok(DeltaPage { items, delta_link })
    }
}

fn retry_after(resp: &reqwest::Response) -> Option<Duration> {
    resp.headers()
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
}

fn backoff(attempt: u32) -> Duration {
    // 0.5s, 1s, 2s, 4s, 8s ...
    Duration::from_millis(500u64.saturating_mul(1 << attempt.min(6)))
}

#[derive(serde::Deserialize)]
struct ODataPage<T> {
    #[serde(default = "Vec::new")]
    value: Vec<T>,
    #[serde(rename = "@odata.nextLink")]
    next_link: Option<String>,
}

#[derive(serde::Deserialize)]
struct ODataDeltaPage<T> {
    #[serde(default = "Vec::new")]
    value: Vec<T>,
    #[serde(rename = "@odata.nextLink")]
    next_link: Option<String>,
    #[serde(rename = "@odata.deltaLink")]
    delta_link: Option<String>,
}
