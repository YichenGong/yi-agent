use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::ToolsError;
use crate::web::provider::{SearchResult, WebSearchProvider};

pub struct BochaSearchProvider {
    client: reqwest::Client,
    api_key: String,
    base_url: String,
}

impl BochaSearchProvider {
    pub fn new(api_key: String) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("failed to build reqwest client");
        Self {
            client,
            api_key,
            base_url: "https://api.bocha.cn/v1".to_string(),
        }
    }

    pub fn with_base_url(api_key: String, base_url: String) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("failed to build reqwest client");
        Self {
            client,
            api_key,
            base_url,
        }
    }

    pub fn from_env() -> Option<Self> {
        let api_key = std::env::var("BOCHA_API_KEY").ok()?;
        Some(Self::new(api_key))
    }
}

#[derive(Serialize)]
struct BochaRequest {
    query: String,
    summary: bool,
    count: usize,
    freshness: String,
}

#[derive(Deserialize)]
struct BochaResponse {
    #[serde(default)]
    #[allow(dead_code)]
    code: Option<u16>,
    #[serde(default)]
    #[allow(dead_code)]
    msg: Option<String>,
    data: Option<BochaData>,
}

#[derive(Deserialize)]
struct BochaData {
    #[serde(default, rename = "webPages")]
    web_pages: Option<WebPages>,
}

#[derive(Deserialize)]
struct WebPages {
    #[serde(default)]
    value: Vec<WebPageValue>,
}

#[derive(Deserialize)]
struct WebPageValue {
    #[serde(default)]
    name: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    snippet: String,
    #[serde(default)]
    summary: String,
}

#[async_trait]
impl WebSearchProvider for BochaSearchProvider {
    fn name(&self) -> &str {
        "bocha"
    }

    async fn search(&self, query: &str, count: usize) -> Result<Vec<SearchResult>, ToolsError> {
        let req_body = BochaRequest {
            query: query.to_string(),
            summary: true,
            count,
            freshness: "noLimit".to_string(),
        };

        let resp = self
            .client
            .post(format!("{}/web-search", self.base_url))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&req_body)
            .send()
            .await
            .map_err(|e| ToolsError::Http(e.to_string()))?;

        let status = resp.status().as_u16();

        if status == 401 {
            return Err(ToolsError::SearchEngine("invalid API key".to_string()));
        }
        if status == 403 {
            return Err(ToolsError::SearchEngine("insufficient balance".to_string()));
        }
        if status == 429 {
            return Err(ToolsError::SearchEngine("rate limited".to_string()));
        }
        if status >= 500 {
            let body = resp.text().await.unwrap_or_default();
            return Err(ToolsError::SearchEngine(format!("server error: {}", body)));
        }
        if !resp.status().is_success() {
            return Err(ToolsError::SearchEngine(format!("unexpected status: {}", status)));
        }

        let bocha_resp: BochaResponse = resp
            .json()
            .await
            .map_err(|e| ToolsError::Http(format!("failed to parse response: {}", e)))?;

        let results: Vec<SearchResult> = bocha_resp
            .data
            .and_then(|d| d.web_pages)
            .map(|wp| wp.value)
            .unwrap_or_default()
            .into_iter()
            .map(|v| SearchResult {
                title: v.name,
                url: v.url,
                snippet: if v.summary.is_empty() {
                    v.snippet
                } else {
                    v.summary
                },
            })
            .collect();

        Ok(results)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn sample_response() -> String {
        r#"{
            "code": 200,
            "msg": null,
            "data": {
                "_type": "SearchResponse",
                "webPages": {
                    "webSearchUrl": "",
                    "totalEstimatedMatches": 3,
                    "value": [
                        {
                            "name": "Result One",
                            "url": "https://example.com/1",
                            "snippet": "Short description one",
                            "summary": "Full summary one"
                        },
                        {
                            "name": "Result Two",
                            "url": "https://example.com/2",
                            "snippet": "Short description two",
                            "summary": ""
                        }
                    ]
                }
            }
        }"#
        .to_string()
    }

    #[tokio::test]
    async fn search_returns_results() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/web-search"))
            .and(header("authorization", "Bearer test-key"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .set_body_string(sample_response()),
            )
            .mount(&server)
            .await;

        let provider = BochaSearchProvider::with_base_url("test-key".to_string(), server.uri());
        let results = provider.search("test query", 5).await.unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].title, "Result One");
        assert_eq!(results[0].url, "https://example.com/1");
        assert_eq!(results[0].snippet, "Full summary one");
        assert_eq!(results[1].snippet, "Short description two");
    }

    #[tokio::test]
    async fn search_no_results() {
        let server = MockServer::start().await;
        let empty_response = r#"{
            "code": 200,
            "data": {
                "webPages": {
                    "value": []
                }
            }
        }"#;
        Mock::given(method("POST"))
            .and(path("/web-search"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .set_body_string(empty_response),
            )
            .mount(&server)
            .await;

        let provider = BochaSearchProvider::with_base_url("test-key".to_string(), server.uri());
        let results = provider.search("nothing", 5).await.unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn search_invalid_key() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/web-search"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;

        let provider = BochaSearchProvider::with_base_url("bad-key".to_string(), server.uri());
        let result = provider.search("test", 5).await;
        assert!(matches!(result, Err(ToolsError::SearchEngine(_))));
    }

    #[tokio::test]
    async fn search_rate_limited() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/web-search"))
            .respond_with(ResponseTemplate::new(429))
            .mount(&server)
            .await;

        let provider = BochaSearchProvider::with_base_url("test-key".to_string(), server.uri());
        let result = provider.search("test", 5).await;
        assert!(matches!(result, Err(ToolsError::SearchEngine(_))));
    }

    #[tokio::test]
    async fn search_uses_summary_field() {
        let server = MockServer::start().await;
        let response = r#"{
            "code": 200,
            "data": {
                "webPages": {
                    "value": [
                        {
                            "name": "Test",
                            "url": "https://example.com",
                            "snippet": "SHORT",
                            "summary": "LONG SUMMARY"
                        }
                    ]
                }
            }
        }"#;
        Mock::given(method("POST"))
            .and(path("/web-search"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .set_body_string(response),
            )
            .mount(&server)
            .await;

        let provider = BochaSearchProvider::with_base_url("test-key".to_string(), server.uri());
        let results = provider.search("test", 1).await.unwrap();
        assert_eq!(results[0].snippet, "LONG SUMMARY");
    }
}
