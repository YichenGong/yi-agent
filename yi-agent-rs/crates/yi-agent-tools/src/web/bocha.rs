use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::ToolsError;
use crate::web::config::{DEFAULT_BOCHA_BASE_URL, Freshness};
use crate::web::provider::{SearchResult, WebSearchProvider};

pub struct BochaSearchProvider {
    client: reqwest::Client,
    api_key: String,
    base_url: String,
    freshness: Freshness,
}

impl BochaSearchProvider {
    pub fn new(api_key: String) -> Self {
        Self::with_options(
            api_key,
            DEFAULT_BOCHA_BASE_URL.to_string(),
            Freshness::NoLimit,
        )
    }

    /// 带完整参数的构造：设置页可改端点与时效，这两个曾经是硬编码。
    pub fn with_options(api_key: String, base_url: String, freshness: Freshness) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("failed to build reqwest client");
        Self {
            client,
            api_key,
            base_url,
            freshness,
        }
    }

    pub fn with_base_url(api_key: String, base_url: String) -> Self {
        Self::with_options(api_key, base_url, Freshness::NoLimit)
    }

    #[cfg(test)]
    fn with_base_url_for_test(api_key: String, base_url: String) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .no_proxy()
            .build()
            .expect("failed to build reqwest client");
        Self {
            client,
            api_key,
            base_url,
            freshness: Freshness::NoLimit,
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
            freshness: self.freshness.as_bocha_str().to_string(),
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
            return Err(ToolsError::SearchEngine(format!(
                "unexpected status: {}",
                status
            )));
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
    use wiremock::matchers::{body_json, header, method, path};
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

    /// `freshness` 曾是硬编码 "noLimit"，用户改不了。现在必须随构造参数走，
    /// 否则设置页里的时效下拉框就是个摆设。
    #[tokio::test]
    async fn freshness_reaches_the_request_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/web-search"))
            .and(body_json(serde_json::json!({
                "query": "q",
                "summary": true,
                "count": 3,
                "freshness": "oneWeek",
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "code": 200,
                "data": { "webPages": { "value": [] } }
            })))
            .mount(&server)
            .await;

        let provider = BochaSearchProvider::with_options(
            "test-key".to_string(),
            server.uri(),
            Freshness::OneWeek,
        );
        // body_json 不匹配时 wiremock 回 404，这次 unwrap 就会炸。
        provider.search("q", 3).await.unwrap();
    }

    /// 默认构造仍可用（`new` 的内部实现改成了转调 `with_options`）。
    #[tokio::test]
    async fn new_still_works() {
        let provider = BochaSearchProvider::new("k".to_string());
        assert_eq!(provider.name(), "bocha");
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

        let provider =
            BochaSearchProvider::with_base_url_for_test("test-key".to_string(), server.uri());
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

        let provider =
            BochaSearchProvider::with_base_url_for_test("test-key".to_string(), server.uri());
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

        let provider =
            BochaSearchProvider::with_base_url_for_test("bad-key".to_string(), server.uri());
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

        let provider =
            BochaSearchProvider::with_base_url_for_test("test-key".to_string(), server.uri());
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

        let provider =
            BochaSearchProvider::with_base_url_for_test("test-key".to_string(), server.uri());
        let results = provider.search("test", 1).await.unwrap();
        assert_eq!(results[0].snippet, "LONG SUMMARY");
    }
}
