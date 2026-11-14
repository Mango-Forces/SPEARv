//! HTTP calls to the index-service.

use std::time::Duration;

use async_trait::async_trait;

use crate::api::*;

pub type Result<T> = std::result::Result<T, ClientError>;

#[derive(Debug)]
pub enum ClientError {
    /// The service refused the query (HTTP 400). The text is its reason.
    BadQuery(String),
    /// The service could not be reached or failed.
    Unavailable(String),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadQuery(text) => write!(f, "index query refused: {text}"),
            Self::Unavailable(text) => write!(f, "index-service: {text}"),
        }
    }
}

impl std::error::Error for ClientError {}

impl From<ClientError> for spearv_core::Error {
    fn from(err: ClientError) -> Self {
        match err {
            ClientError::BadQuery(text) => Self::Message(text),
            ClientError::Unavailable(text) => Self::Upstream(format!("index-service: {text}")),
        }
    }
}

#[derive(Clone)]
pub struct IndexClient {
    http: reqwest::Client,
    base: String,
}

impl IndexClient {
    pub fn new(base: impl Into<String>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(3))
            .timeout(Duration::from_secs(60))
            .user_agent("spearv-index-client")
            .build()
            .map_err(|err| ClientError::Unavailable(err.to_string()))?;
        Ok(Self { http, base: base.into().trim_end_matches('/').to_string() })
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    pub async fn coverage(&self, query: &CoverageQuery) -> Result<CoverageResponse> {
        self.get("coverage", Some(query)).await
    }

    pub async fn passes(&self, query: &CoverageQuery) -> Result<PassesResponse> {
        self.get("passes", Some(query)).await
    }

    pub async fn releases(&self) -> Result<ReleasesResponse> {
        self.get::<(), _>("releases", None).await
    }

    pub async fn generation_id(&self) -> Result<i64> {
        Ok(self.get::<(), GenerationResponse>("generation", None).await?.generation)
    }

    pub async fn health_report(&self) -> Result<Health> {
        self.get::<(), _>("health", None).await
    }

    async fn get<Q: serde::Serialize, T: serde::de::DeserializeOwned>(&self, route: &str, query: Option<&Q>) -> Result<T> {
        let mut request = self.http.get(format!("{}/{route}", self.base));
        if let Some(query) = query {
            request = request.query(query);
        }
        let response = request.send().await.map_err(|err| ClientError::Unavailable(chain(&err)))?;
        let status = response.status();
        if status == reqwest::StatusCode::BAD_REQUEST {
            let body: ApiError = response
                .json()
                .await
                .unwrap_or_else(|err| ApiError { error: format!("unreadable 400: {err}") });
            return Err(ClientError::BadQuery(body.error));
        }
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(ClientError::Unavailable(format!("{route}: HTTP {status}: {body}")));
        }
        response.json().await.map_err(|err| ClientError::Unavailable(format!("{route}: {}", chain(&err))))
    }
}

#[async_trait]
impl spearv_core::IndexService for IndexClient {
    async fn health(&self) -> spearv_core::Result<serde_json::Value> {
        let report = self.health_report().await?;
        serde_json::to_value(report).map_err(|err| spearv_core::Error::Upstream(format!("index-service health: {err}")))
    }

    async fn generation(&self) -> spearv_core::Result<String> {
        Ok(self.generation_id().await?.to_string())
    }
}

/// reqwest's own text stops at "error sending request"; the cause is in its sources.
fn chain(err: &dyn std::error::Error) -> String {
    let mut text = err.to_string();
    let mut source = err.source();
    while let Some(cause) = source {
        text = format!("{text}: {cause}");
        source = cause.source();
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refused_query_is_a_bad_request_and_an_outage_is_upstream() {
        let refused = spearv_core::Error::from(ClientError::BadQuery("dec must be in [-90, 90]".into()));
        assert!(matches!(&refused, spearv_core::Error::Message(text) if text.contains("dec")), "{refused}");
        let down = spearv_core::Error::from(ClientError::Unavailable("connection refused".into()));
        assert!(matches!(&down, spearv_core::Error::Upstream(text) if text.contains("connection refused")), "{down}");
    }
}
