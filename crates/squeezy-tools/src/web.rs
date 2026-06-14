use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    net::{IpAddr, SocketAddr},
    pin::Pin,
    sync::LazyLock,
    time::{Duration, Instant, SystemTime},
};

use futures_util::StreamExt;
use regex::Regex;
use reqwest::{
    Url,
    header::{ACCEPT, HeaderMap, HeaderValue},
    redirect::Policy,
};
use serde::Deserialize;
use serde_json::{Value, json};
use squeezy_core::{Result, SqueezyError};
use tokio::time;
use tokio_util::sync::CancellationToken;

use crate::{
    ToolCall, ToolCostHint, ToolRegistry, ToolResult, ToolStatus, WebCallStats,
    collapse_whitespace, make_result, sha256_hex, tool_arg_error, tool_error,
    truncate::truncate_middle_bytes, unix_timestamp_millis,
};

pub(crate) const DEFAULT_WEB_SEARCH_RESULTS: usize = 8;
pub(crate) const MAX_WEB_SEARCH_RESULTS: usize = 20;
pub(crate) const DEFAULT_WEB_SEARCH_CONTEXT_CHARS: usize = 10_000;
pub(crate) const MAX_WEB_SEARCH_CONTEXT_CHARS: usize = 50_000;
/// Parallel Search MCP endpoint. Both Exa and Parallel speak the same
/// JSON-RPC `tools/call` protocol, so the dispatcher only varies URL,
/// auth, tool name, and argument shape.
pub const DEFAULT_PARALLEL_MCP_URL: &str = "https://search.parallel.ai/mcp";

/// Selects which remote MCP-style websearch backend handles a query. The
/// finding calls for pluggable providers so users can pick
/// based on price/quality; defaulting to Exa preserves prior behavior.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum WebSearchProvider {
    #[default]
    Exa,
    Parallel,
}

impl WebSearchProvider {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Exa => "exa",
            Self::Parallel => "parallel",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        let raw = raw.trim();
        if raw.eq_ignore_ascii_case("exa") {
            Some(Self::Exa)
        } else if raw.eq_ignore_ascii_case("parallel") {
            Some(Self::Parallel)
        } else {
            None
        }
    }
}
pub(crate) const DEFAULT_WEB_SEARCH_TIMEOUT_MS: u64 = 25_000;
pub(crate) const DEFAULT_WEB_SEARCH_MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
pub(crate) const DEFAULT_WEB_SEARCH_OUTPUT_BYTE_CAP: usize = 32_000;
pub(crate) const DEFAULT_WEB_FETCH_TIMEOUT_MS: u64 = 30_000;
pub(crate) const MAX_WEB_TIMEOUT_MS: u64 = 120_000;
pub(crate) const DEFAULT_WEB_FETCH_MAX_RESPONSE_BYTES: usize = 5 * 1024 * 1024;
pub(crate) const MAX_WEB_FETCH_MAX_RESPONSE_BYTES: usize = 10 * 1024 * 1024;
pub(crate) const DEFAULT_WEB_FETCH_OUTPUT_BYTE_CAP: usize = 32_000;
pub(crate) const MAX_WEB_REDIRECTS: usize = 5;
pub(crate) const WEB_CACHE_RECEIPT_TTL: Duration = Duration::from_secs(24 * 60 * 60);

pub(crate) type WebHttpFuture<'a> =
    Pin<Box<dyn Future<Output = std::result::Result<WebHttpResponse, String>> + Send + 'a>>;

pub(crate) trait WebHttpClient: Send + Sync + std::fmt::Debug {
    fn post_json<'a>(
        &'a self,
        url: &'a str,
        headers: Vec<(String, String)>,
        body: Value,
        max_response_bytes: usize,
    ) -> WebHttpFuture<'a>;

    fn get<'a>(&'a self, url: Url, max_response_bytes: usize) -> WebHttpFuture<'a>;
}

#[derive(Debug, Clone)]
pub(crate) struct WebHttpResponse {
    pub(crate) status: u16,
    pub(crate) headers: BTreeMap<String, String>,
    pub(crate) body: Vec<u8>,
}

impl WebHttpResponse {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(name)
            .or_else(|| self.headers.get(&name.to_ascii_lowercase()))
            .map(String::as_str)
    }

    fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    fn is_redirection(&self) -> bool {
        (300..400).contains(&self.status)
    }
}

#[derive(Debug)]
pub(crate) struct ReqwestWebHttpClient {
    client: reqwest::Client,
}

impl ReqwestWebHttpClient {
    pub(crate) fn new() -> Result<Self> {
        let client = reqwest::Client::builder()
            .redirect(Policy::none())
            .build()
            .map_err(|err| SqueezyError::Tool(format!("failed to create HTTP client: {err}")))?;
        Ok(Self { client })
    }
}

impl WebHttpClient for ReqwestWebHttpClient {
    fn post_json<'a>(
        &'a self,
        url: &'a str,
        headers: Vec<(String, String)>,
        body: Value,
        max_response_bytes: usize,
    ) -> WebHttpFuture<'a> {
        Box::pin(async move {
            let mut request_headers = HeaderMap::new();
            for (name, value) in headers {
                let name = name
                    .parse::<reqwest::header::HeaderName>()
                    .map_err(|err| format!("invalid request header name: {err}"))?;
                let value = HeaderValue::from_str(&value)
                    .map_err(|err| format!("invalid request header value: {err}"))?;
                request_headers.insert(name, value);
            }
            let response = self
                .client
                .post(url)
                .headers(request_headers)
                .json(&body)
                .send()
                .await
                .map_err(|err| format!("websearch request failed: {err}"))?;
            let status = response.status().as_u16();
            let headers = response_headers(response.headers());
            let body = read_response_bytes(response, max_response_bytes).await?;
            Ok(WebHttpResponse {
                status,
                headers,
                body,
            })
        })
    }

    fn get<'a>(&'a self, url: Url, max_response_bytes: usize) -> WebHttpFuture<'a> {
        Box::pin(async move {
            // Resolve and SSRF-validate the host ourselves, then pin the dialed
            // IP to the validated address so reqwest does not perform a second,
            // independent DNS lookup (which an attacker could rebind to an
            // internal IP between validation and connect — DNS-rebinding TOCTOU).
            let pinned_ip = ensure_url_allowed(&url).await?;
            let host = url
                .host_str()
                .ok_or_else(|| "URL has no host".to_string())?;
            // Port 0 tells reqwest to use the URL's port (or the scheme default),
            // while the IP is fixed to the address we already validated. This
            // preserves the Host header and TLS SNI from the original hostname.
            let client = reqwest::Client::builder()
                .redirect(Policy::none())
                .resolve(host, SocketAddr::new(pinned_ip, 0))
                .build()
                .map_err(|err| format!("failed to create HTTP client: {err}"))?;
            let response = client
                .get(url)
                .header(
                    ACCEPT,
                    "text/plain;q=1.0, text/html;q=0.9, application/json;q=0.8, application/xml;q=0.7, */*;q=0.1",
                )
                .header("user-agent", "squeezy/0.1")
                .send()
                .await
                .map_err(|err| format!("webfetch request failed: {err}"))?;
            let status = response.status().as_u16();
            let headers = response_headers(response.headers());
            let body = read_response_bytes(response, max_response_bytes).await?;
            Ok(WebHttpResponse {
                status,
                headers,
                body,
            })
        })
    }
}

fn response_headers(headers: &HeaderMap) -> BTreeMap<String, String> {
    headers
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.as_str().to_ascii_lowercase(), value.to_string()))
        })
        .collect()
}

impl ToolRegistry {
    pub(crate) async fn execute_websearch(
        &self,
        call: &ToolCall,
        cancel: CancellationToken,
    ) -> ToolResult {
        let web_started = Instant::now();
        let args = match serde_json::from_value::<WebSearchArgs>(call.arguments.clone()) {
            Ok(args) => args,
            Err(err) => return tool_arg_error(call, err),
        };
        if args.query.trim().is_empty() {
            return tool_error(call, "query must not be empty");
        }

        let num_results = args
            .num_results
            .unwrap_or(DEFAULT_WEB_SEARCH_RESULTS)
            .clamp(1, MAX_WEB_SEARCH_RESULTS);
        let context_max_characters = args
            .context_max_characters
            .unwrap_or(DEFAULT_WEB_SEARCH_CONTEXT_CHARS)
            .clamp(1, MAX_WEB_SEARCH_CONTEXT_CHARS);
        let timeout_ms = args
            .timeout_ms
            .unwrap_or(DEFAULT_WEB_SEARCH_TIMEOUT_MS)
            .min(MAX_WEB_TIMEOUT_MS);
        let output_byte_cap = args
            .output_byte_cap
            .unwrap_or(DEFAULT_WEB_SEARCH_OUTPUT_BYTE_CAP)
            .min(128_000);
        let search_type = args.search_type.unwrap_or_default();
        let livecrawl = args.livecrawl.unwrap_or_default();

        let provider = self.web_config.provider;
        let mut request_headers = vec![(
            "accept".to_string(),
            "application/json, text/event-stream".to_string(),
        )];
        let (endpoint_url, tool_name, arguments) = match provider {
            WebSearchProvider::Exa => {
                if let Some(api_key) = self.web_config.exa_api_key.as_deref() {
                    request_headers.push(("x-api-key".to_string(), api_key.to_string()));
                }
                (
                    self.web_config.exa_mcp_url.as_str(),
                    "web_search_exa",
                    json!({
                        "query": args.query,
                        "type": search_type.as_str(),
                        "numResults": num_results,
                        "livecrawl": livecrawl.as_str(),
                        "contextMaxCharacters": context_max_characters,
                    }),
                )
            }
            WebSearchProvider::Parallel => {
                if let Some(api_key) = self.web_config.parallel_api_key.as_deref() {
                    request_headers
                        .push(("authorization".to_string(), format!("Bearer {api_key}")));
                }
                (
                    self.web_config.parallel_mcp_url.as_str(),
                    "web_search",
                    json!({
                        "objective": args.query,
                        "search_queries": [args.query],
                    }),
                )
            }
        };

        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": tool_name,
                "arguments": arguments,
            },
        });
        let request_sha256 = sha256_hex(serde_json::to_vec(&body).unwrap_or_default());
        let fetch = async {
            let response = self
                .http
                .post_json(
                    endpoint_url,
                    request_headers,
                    body.clone(),
                    DEFAULT_WEB_SEARCH_MAX_RESPONSE_BYTES,
                )
                .await?;
            if !response.is_success() {
                return Err(format!(
                    "websearch provider returned HTTP {}",
                    response.status
                ));
            }
            let response_sha256 = sha256_hex(&response.body);
            let response_text = String::from_utf8_lossy(&response.body).to_string();
            let result = match parse_mcp_websearch_response(&response_text) {
                Some(result) => result,
                None => {
                    let message = parse_mcp_websearch_error(&response_text).map_or_else(
                        || "websearch provider returned no text content".to_string(),
                        |message| format!("websearch provider error: {message}"),
                    );
                    return Err(message);
                }
            };
            Ok::<_, String>((response_text.len(), response_sha256, result))
        };

        let (bytes_read, response_sha256, result) = match tokio::select! {
            _ = cancel.cancelled() => {
                let mut result = ToolResult::cancelled(call);
                result.web_call_stats = Some(WebCallStats {
                    provider_token: provider.as_str().to_string(),
                    status_token: "cancelled".to_string(),
                    ssrf_blocked: false,
                    redirect_blocked: false,
                    response_byte_bucket: WebCallStats::response_byte_bucket(0).to_string(),
                    duration_ms: web_started.elapsed().as_millis() as u64,
                });
                return result;
            },
            result = time::timeout(Duration::from_millis(timeout_ms), fetch) => result,
        } {
            Ok(Ok(result)) => result,
            Ok(Err(err)) => {
                let mut result = tool_error(call, err);
                result.web_call_stats = Some(WebCallStats {
                    provider_token: provider.as_str().to_string(),
                    status_token: "error".to_string(),
                    ssrf_blocked: false,
                    redirect_blocked: false,
                    response_byte_bucket: WebCallStats::response_byte_bucket(0).to_string(),
                    duration_ms: web_started.elapsed().as_millis() as u64,
                });
                return result;
            }
            Err(_) => {
                let mut result =
                    tool_error(call, format!("websearch timed out after {timeout_ms} ms"));
                result.web_call_stats = Some(WebCallStats {
                    provider_token: provider.as_str().to_string(),
                    status_token: "error".to_string(),
                    ssrf_blocked: false,
                    redirect_blocked: false,
                    response_byte_bucket: WebCallStats::response_byte_bucket(0).to_string(),
                    duration_ms: web_started.elapsed().as_millis() as u64,
                });
                return result;
            }
        };
        let retrieved_at_unix_ms = unix_timestamp_millis(SystemTime::now());
        let source_urls = extract_http_urls(&result);
        let redacted = self.redactor.redact(&result);
        let (quote, output_truncated) = truncate_middle_bytes(&redacted.text, output_byte_cap);
        let quote_sha256 = sha256_hex(quote.as_bytes());
        let stable_output_sha256 = web_stable_output_sha256(
            "websearch",
            &request_sha256,
            &response_sha256,
            &quote_sha256,
        );
        let quote_bytes = quote.len();
        let citations = web_citations_json(
            "websearch",
            &source_urls,
            retrieved_at_unix_ms,
            Some(&response_sha256),
            &quote_sha256,
            quote_bytes,
            output_truncated,
        );
        let cache_receipt = web_cache_receipt_json(
            "websearch",
            &request_sha256,
            Some(&response_sha256),
            &quote_sha256,
            &stable_output_sha256,
            retrieved_at_unix_ms,
        );
        let cost = ToolCostHint {
            bytes_read: bytes_read as u64,
            output_bytes: quote_bytes as u64,
            redactions: redacted.redactions,
            truncated: output_truncated,
            ..ToolCostHint::default()
        };

        let mut result = make_result(
            call,
            ToolStatus::Success,
            json!({
                "provider": provider.as_str(),
                "query": args.query,
                "result": quote,
                "source_urls": source_urls,
                "retrieved_at_unix_ms": retrieved_at_unix_ms,
                "evidence": {
                    "kind": "remote_search",
                    "source": "websearch",
                    "local": false,
                },
                "citations": citations,
                "cache_receipt": cache_receipt,
                "quote_limit_bytes": output_byte_cap,
                "quote_bytes": quote_bytes,
                "quote_truncated": output_truncated,
                "quote_sha256": quote_sha256,
                "truncated": output_truncated,
                "metadata": {
                    "num_results": num_results,
                    "search_type": search_type.as_str(),
                    "livecrawl": livecrawl.as_str(),
                    "context_max_characters": context_max_characters,
                    "output_byte_cap": output_byte_cap,
                },
            }),
            cost,
            None,
        );
        result.web_call_stats = Some(WebCallStats {
            provider_token: provider.as_str().to_string(),
            status_token: "success".to_string(),
            ssrf_blocked: false,
            redirect_blocked: false,
            response_byte_bucket: WebCallStats::response_byte_bucket(bytes_read).to_string(),
            duration_ms: web_started.elapsed().as_millis() as u64,
        });
        result
    }

    pub(crate) async fn execute_webfetch(
        &self,
        call: &ToolCall,
        cancel: CancellationToken,
    ) -> ToolResult {
        let web_started = Instant::now();
        let args = match serde_json::from_value::<WebFetchArgs>(call.arguments.clone()) {
            Ok(args) => args,
            Err(err) => return tool_arg_error(call, err),
        };
        let requested_url = args.url.clone();
        let mut url = match parse_http_url(&args.url) {
            Ok(url) => url,
            Err(err) => return tool_error(call, err),
        };
        let original_url = url.clone();
        let format = args.format.unwrap_or_default();
        let timeout_ms = args
            .timeout_ms
            .unwrap_or(DEFAULT_WEB_FETCH_TIMEOUT_MS)
            .min(MAX_WEB_TIMEOUT_MS);
        let max_response_bytes = args
            .max_response_bytes
            .unwrap_or(DEFAULT_WEB_FETCH_MAX_RESPONSE_BYTES)
            .clamp(1, MAX_WEB_FETCH_MAX_RESPONSE_BYTES);
        let output_byte_cap = args
            .output_byte_cap
            .unwrap_or(DEFAULT_WEB_FETCH_OUTPUT_BYTE_CAP)
            .min(128_000);

        let fetch = async {
            for redirect_count in 0..=MAX_WEB_REDIRECTS {
                ensure_url_allowed(&url).await?;
                let response = self.http.get(url.clone(), max_response_bytes).await?;
                if response.is_redirection() {
                    let next = redirect_url(&url, &response)?;
                    if next.origin() != original_url.origin() {
                        return Ok(WebFetchOutcome::Redirect {
                            status: response.status,
                            original_url: original_url.to_string(),
                            redirect_url: next.to_string(),
                        });
                    }
                    if redirect_count == MAX_WEB_REDIRECTS {
                        return Err("too many redirects".to_string());
                    }
                    url = next;
                    continue;
                }
                if !response.is_success() {
                    return Err(format!("webfetch returned HTTP status {}", response.status));
                }

                let content_type = response.header("content-type").unwrap_or("").to_string();
                if !is_textual_content_type(&content_type) {
                    return Err(format!(
                        "unsupported content type: {}",
                        if content_type.is_empty() {
                            "unknown"
                        } else {
                            content_type.as_str()
                        }
                    ));
                }

                return Ok(WebFetchOutcome::Fetched {
                    final_url: url.to_string(),
                    status: response.status,
                    content_type,
                    bytes: response.body,
                });
            }
            Err("too many redirects".to_string())
        };

        let outcome = match tokio::select! {
            _ = cancel.cancelled() => {
                let mut result = ToolResult::cancelled(call);
                result.web_call_stats = Some(WebCallStats {
                    provider_token: "webfetch".to_string(),
                    status_token: "cancelled".to_string(),
                    ssrf_blocked: false,
                    redirect_blocked: false,
                    response_byte_bucket: WebCallStats::response_byte_bucket(0).to_string(),
                    duration_ms: web_started.elapsed().as_millis() as u64,
                });
                return result;
            },
            result = time::timeout(Duration::from_millis(timeout_ms), fetch) => result,
        } {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(err)) => {
                let ssrf_blocked = err.contains("internal address");
                let redirect_blocked = err.contains("too many redirects");
                let duration_ms = web_started.elapsed().as_millis() as u64;
                let mut result = tool_error(call, err);
                result.web_call_stats = Some(WebCallStats {
                    provider_token: "webfetch".to_string(),
                    status_token: "error".to_string(),
                    ssrf_blocked,
                    redirect_blocked,
                    response_byte_bucket: WebCallStats::response_byte_bucket(0).to_string(),
                    duration_ms,
                });
                return result;
            }
            Err(_) => {
                let mut result =
                    tool_error(call, format!("webfetch timed out after {timeout_ms} ms"));
                result.web_call_stats = Some(WebCallStats {
                    provider_token: "webfetch".to_string(),
                    status_token: "error".to_string(),
                    ssrf_blocked: false,
                    redirect_blocked: false,
                    response_byte_bucket: WebCallStats::response_byte_bucket(0).to_string(),
                    duration_ms: web_started.elapsed().as_millis() as u64,
                });
                return result;
            }
        };

        match outcome {
            WebFetchOutcome::Redirect {
                status,
                original_url,
                redirect_url,
            } => {
                let mut result = make_result(
                    call,
                    ToolStatus::Error,
                    json!({
                        "error": "redirect to another host detected; call webfetch again with redirect_url if approved",
                        "status": status,
                        "original_url": original_url,
                        "redirect_url": redirect_url,
                    }),
                    ToolCostHint::default(),
                    None,
                );
                result.web_call_stats = Some(WebCallStats {
                    provider_token: "webfetch".to_string(),
                    status_token: "error".to_string(),
                    ssrf_blocked: false,
                    redirect_blocked: true,
                    response_byte_bucket: WebCallStats::response_byte_bucket(0).to_string(),
                    duration_ms: web_started.elapsed().as_millis() as u64,
                });
                result
            }
            WebFetchOutcome::Fetched {
                final_url,
                status,
                content_type,
                bytes,
            } => {
                let raw_len = bytes.len();
                let decoded = decode_body(&bytes, &content_type);
                let rendered = match format {
                    WebFetchFormat::Text if content_type_is_html(&content_type) => {
                        html_to_text(&decoded)
                    }
                    WebFetchFormat::Text => decoded.to_string(),
                    WebFetchFormat::Html => decoded.to_string(),
                };
                let retrieved_at_unix_ms = unix_timestamp_millis(SystemTime::now());
                let redacted = self.redactor.redact(&rendered);
                let (content, output_truncated) =
                    truncate_middle_bytes(&redacted.text, output_byte_cap);
                let content_sha256 = sha256_hex(&bytes);
                let quote_sha256 = sha256_hex(content.as_bytes());
                let request_sha256 =
                    web_fetch_request_sha256(&requested_url, format.as_str(), max_response_bytes);
                let stable_output_sha256 = web_stable_output_sha256(
                    "webfetch",
                    &request_sha256,
                    &content_sha256,
                    &quote_sha256,
                );
                let citation_urls = vec![final_url.clone()];
                let quote_bytes = content.len();
                let citations = web_citations_json(
                    "webfetch",
                    &citation_urls,
                    retrieved_at_unix_ms,
                    Some(&content_sha256),
                    &quote_sha256,
                    quote_bytes,
                    output_truncated,
                );
                let cache_receipt = web_cache_receipt_json(
                    "webfetch",
                    &request_sha256,
                    Some(&content_sha256),
                    &quote_sha256,
                    &stable_output_sha256,
                    retrieved_at_unix_ms,
                );
                let cost = ToolCostHint {
                    bytes_read: raw_len as u64,
                    output_bytes: quote_bytes as u64,
                    redactions: redacted.redactions,
                    truncated: output_truncated,
                    ..ToolCostHint::default()
                };
                let mut result = make_result(
                    call,
                    ToolStatus::Success,
                    json!({
                        "url": final_url.clone(),
                        "source_url": final_url,
                        "retrieved_at_unix_ms": retrieved_at_unix_ms,
                        "status": status,
                        "content_type": content_type,
                        "format": format.as_str(),
                        "bytes_read": raw_len,
                        "sha256": content_sha256.clone(),
                        "evidence": {
                            "kind": "remote_document",
                            "source": "webfetch",
                            "local": false,
                        },
                        "citations": citations,
                        "cache_receipt": cache_receipt,
                        "quote_limit_bytes": output_byte_cap,
                        "quote_bytes": quote_bytes,
                        "quote_truncated": output_truncated,
                        "quote_sha256": quote_sha256,
                        "truncated": output_truncated,
                        "content": content,
                    }),
                    cost,
                    Some(content_sha256),
                );
                result.web_call_stats = Some(WebCallStats {
                    provider_token: "webfetch".to_string(),
                    status_token: "success".to_string(),
                    ssrf_blocked: false,
                    redirect_blocked: false,
                    response_byte_bucket: WebCallStats::response_byte_bucket(raw_len).to_string(),
                    duration_ms: web_started.elapsed().as_millis() as u64,
                });
                result
            }
        }
    }
}

pub(crate) fn enforce_web_quote_limit(mut result: ToolResult) -> ToolResult {
    let quote_field = match result.tool_name.as_str() {
        "webfetch" => "content",
        "websearch" => "result",
        _ => return result,
    };
    let Some(limit) = result
        .content
        .get("quote_limit_bytes")
        .and_then(Value::as_u64)
        .map(|value| value as usize)
    else {
        return result;
    };
    let Some(text) = result
        .content
        .get(quote_field)
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        return result;
    };

    let was_truncated = result
        .content
        .get("quote_truncated")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let (quote, limit_truncated) = truncate_middle_bytes(&text, limit);
    let quote_truncated = was_truncated || limit_truncated;
    let quote_bytes = quote.len();
    let quote_sha256 = sha256_hex(quote.as_bytes());
    let tool_name = result.tool_name.clone();

    let Some(object) = result.content.as_object_mut() else {
        return result;
    };
    object.insert(quote_field.to_string(), Value::String(quote));
    object.insert("quote_bytes".to_string(), json!(quote_bytes));
    object.insert("quote_truncated".to_string(), json!(quote_truncated));
    object.insert("quote_sha256".to_string(), json!(quote_sha256.clone()));
    object.insert("truncated".to_string(), json!(quote_truncated));

    if let Some(citations) = object.get_mut("citations").and_then(Value::as_array_mut) {
        for citation in citations {
            if let Some(citation) = citation.as_object_mut() {
                citation.insert("quote_bytes".to_string(), json!(quote_bytes));
                citation.insert("quote_truncated".to_string(), json!(quote_truncated));
                citation.insert("quote_sha256".to_string(), json!(quote_sha256.clone()));
            }
        }
    }

    if let Some(cache_receipt) = object
        .get_mut("cache_receipt")
        .and_then(Value::as_object_mut)
    {
        let kind = cache_receipt
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or(tool_name.as_str())
            .to_string();
        let request_sha256 = cache_receipt
            .get("request_sha256")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let content_sha256 = cache_receipt
            .get("content_sha256")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        cache_receipt.insert("quote_sha256".to_string(), json!(quote_sha256.clone()));
        cache_receipt.insert(
            "stable_output_sha256".to_string(),
            json!(web_stable_output_sha256(
                &kind,
                &request_sha256,
                &content_sha256,
                &quote_sha256,
            )),
        );
    }

    result.cost_hint.truncated = result.cost_hint.truncated || quote_truncated;
    let output = serde_json::to_vec(&result.content).unwrap_or_default();
    result.cost_hint.output_bytes = output.len() as u64;
    result.receipt.output_sha256 = sha256_hex(&output);
    result
}

pub(crate) fn parse_mcp_websearch_response(body: &str) -> Option<String> {
    let trimmed = body.trim();
    if trimmed.starts_with('{')
        && let Some(result) = parse_mcp_payload(trimmed)
    {
        return Some(result);
    }

    let mut chunks = String::new();
    for line in body.lines() {
        let Some(payload) = line.strip_prefix("data: ") else {
            continue;
        };
        if let Some(result) = parse_mcp_payload(payload) {
            if !chunks.is_empty() {
                chunks.push_str("\n\n");
            }
            chunks.push_str(&result);
        }
    }
    (!chunks.is_empty()).then_some(chunks)
}

fn parse_mcp_payload(payload: &str) -> Option<String> {
    let trimmed = payload.trim();
    if !trimmed.starts_with('{') {
        return None;
    }
    let value = serde_json::from_str::<Value>(trimmed).ok()?;
    let mut texts = String::new();
    for text in value
        .get("result")?
        .get("content")?
        .as_array()?
        .iter()
        .filter_map(|item| item.get("text")?.as_str())
        .filter(|text| !text.trim().is_empty())
        .map(str::trim)
    {
        if !texts.is_empty() {
            texts.push_str("\n\n");
        }
        texts.push_str(text);
    }
    (!texts.is_empty()).then_some(texts)
}

/// Extracts the message from a top-level JSON-RPC `error` object. JSON-RPC
/// servers conventionally return protocol-level failures (quota, invalid
/// argument, transient server error) with HTTP 200 and no `result`, so
/// `parse_mcp_websearch_response` yields `None`; this recovers the actual
/// `error.message` (prefixed with `error.code` when present) so the failure
/// reason is not swallowed.
pub(crate) fn parse_mcp_websearch_error(body: &str) -> Option<String> {
    let trimmed = body.trim();
    if trimmed.starts_with('{')
        && let Some(message) = parse_mcp_error_payload(trimmed)
    {
        return Some(message);
    }

    body.lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .find_map(parse_mcp_error_payload)
}

fn parse_mcp_error_payload(payload: &str) -> Option<String> {
    let trimmed = payload.trim();
    if !trimmed.starts_with('{') {
        return None;
    }
    let value = serde_json::from_str::<Value>(trimmed).ok()?;
    let error = value.get("error")?;
    let message = error.get("message")?.as_str()?.trim();
    if message.is_empty() {
        return None;
    }
    match error.get("code").and_then(Value::as_i64) {
        Some(code) => Some(format!("{message} (code {code})")),
        None => Some(message.to_string()),
    }
}

fn web_fetch_request_sha256(
    requested_url: &str,
    format: &str,
    max_response_bytes: usize,
) -> String {
    sha256_hex(
        json!({
            "tool": "webfetch",
            "url": requested_url,
            "format": format,
            "max_response_bytes": max_response_bytes,
        })
        .to_string(),
    )
}

pub(crate) fn web_stable_output_sha256(
    kind: &str,
    request_sha256: &str,
    content_sha256: &str,
    quote_sha256: &str,
) -> String {
    sha256_hex(format!(
        "{kind}\0{request_sha256}\0{content_sha256}\0{quote_sha256}"
    ))
}

fn web_cache_receipt_json(
    kind: &str,
    request_sha256: &str,
    content_sha256: Option<&str>,
    quote_sha256: &str,
    stable_output_sha256: &str,
    retrieved_at_unix_ms: u128,
) -> Value {
    let stale_after_unix_ms = web_cache_stale_after_unix_ms(retrieved_at_unix_ms);
    json!({
        "kind": kind,
        "request_sha256": request_sha256,
        "content_sha256": content_sha256,
        "quote_sha256": quote_sha256,
        "stable_output_sha256": stable_output_sha256,
        "retrieved_at_unix_ms": retrieved_at_unix_ms,
        "stale_after_unix_ms": stale_after_unix_ms,
        "status": web_cache_receipt_status(retrieved_at_unix_ms, retrieved_at_unix_ms),
    })
}

pub(crate) fn web_cache_stale_after_unix_ms(retrieved_at_unix_ms: u128) -> u128 {
    retrieved_at_unix_ms.saturating_add(WEB_CACHE_RECEIPT_TTL.as_millis())
}

pub(crate) fn web_cache_receipt_status(
    retrieved_at_unix_ms: u128,
    now_unix_ms: u128,
) -> &'static str {
    if now_unix_ms > web_cache_stale_after_unix_ms(retrieved_at_unix_ms) {
        "stale"
    } else {
        "fresh"
    }
}

fn web_citations_json(
    prefix: &str,
    source_urls: &[String],
    retrieved_at_unix_ms: u128,
    content_sha256: Option<&str>,
    quote_sha256: &str,
    quote_bytes: usize,
    quote_truncated: bool,
) -> Value {
    Value::Array(
        source_urls
            .iter()
            .enumerate()
            .map(|(index, url)| {
                json!({
                    "id": format!("{prefix}-{}", index + 1),
                    "url": url,
                    "retrieved_at_unix_ms": retrieved_at_unix_ms,
                    "content_sha256": content_sha256,
                    "quote_sha256": quote_sha256,
                    "quote_bytes": quote_bytes,
                    "quote_truncated": quote_truncated,
                })
            })
            .collect(),
    )
}

static URL_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"https?://[^\s<>"'`\)\]\}]+"#).expect("URL_REGEX is a valid pattern")
});

pub(crate) fn extract_http_urls(text: &str) -> Vec<String> {
    let mut urls = BTreeSet::new();
    for found in URL_REGEX.find_iter(text) {
        let url = found
            .as_str()
            .trim_end_matches(['.', ',', ';', ':', '!', '?']);
        if let Ok(parsed) = parse_http_url(url)
            && parsed.host_str().is_some()
        {
            urls.insert(parsed.to_string());
        }
    }
    urls.into_iter().collect()
}

async fn read_response_bytes(
    response: reqwest::Response,
    max_response_bytes: usize,
) -> std::result::Result<Vec<u8>, String> {
    let content_length = response.content_length();
    if content_length.is_some_and(|len| len > max_response_bytes as u64) {
        return Err(format!(
            "response too large; content-length exceeds {max_response_bytes} bytes"
        ));
    }

    let mut bytes = Vec::with_capacity(
        content_length
            .map(|len| len.min(max_response_bytes as u64) as usize)
            .unwrap_or_default(),
    );
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|err| format!("failed to read response body: {err}"))?;
        if bytes.len().saturating_add(chunk.len()) > max_response_bytes {
            return Err(format!(
                "response too large; exceeded {max_response_bytes} bytes"
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn parse_http_url(raw: &str) -> std::result::Result<Url, String> {
    let url = Url::parse(raw).map_err(|err| format!("invalid URL: {err}"))?;
    match url.scheme() {
        "http" | "https" => Ok(url),
        _ => Err("URL must start with http:// or https://".to_string()),
    }
}

/// True for addresses that must never be reached by `webfetch`: loopback,
/// unspecified, link-local (incl. cloud IMDS `169.254.169.254` and IPv6
/// `fe80::/10`), private / unique-local ranges (RFC1918, `fc00::/7`),
/// shared/CGNAT space (`100.64.0.0/10`, RFC 6598), the `0.0.0.0/8`
/// "this network" block, IETF protocol assignments (`192.0.0.0/24`), and the
/// limited broadcast address. This blocks SSRF to internal/shared-tenancy
/// hosts and instance-metadata endpoints.
fn ip_is_blocked(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            v4.is_loopback()
                || v4.is_unspecified()
                || v4.is_link_local()
                || v4.is_private()
                // 0.0.0.0/8 "this network" (is_unspecified only matches 0.0.0.0).
                || o[0] == 0
                // 100.64.0.0/10 shared address space / CGNAT (RFC 6598).
                || (o[0] == 100 && (64..=127).contains(&o[1]))
                // 192.0.0.0/24 IETF protocol assignments (RFC 6890).
                || (o[0] == 192 && o[1] == 0 && o[2] == 0)
                // Limited broadcast 255.255.255.255.
                || v4.is_broadcast()
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                // Unique-local fc00::/7 (`is_unique_local` is unstable).
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                // Link-local fe80::/10.
                || (v6.segments()[0] & 0xffc0) == 0xfe80
                // IPv4-mapped (::ffff:a.b.c.d) reuses the IPv4 ruling.
                || v6.to_ipv4_mapped().map(|m| ip_is_blocked(&IpAddr::V4(m))) == Some(true)
        }
    }
}

/// Rejects `webfetch` targets that resolve to internal addresses and returns
/// the single validated IP that the caller must dial. Literal-IP hosts are
/// checked directly; hostnames (including `localhost`) are resolved and
/// rejected if *any* resolved address is internal (defeating DNS round-robin
/// where every record is returned in one lookup).
///
/// The returned address must be pinned for the actual connection (see
/// `ReqwestWebHttpClient::get`). This is what prevents DNS rebinding: without
/// pinning, reqwest would perform its own second DNS lookup at connect time
/// and an attacker controlling the host's DNS could return a public IP here
/// and an internal IP for the connect — a TOCTOU bypass. Re-run on every
/// redirect hop, since the host can change between hops.
async fn ensure_url_allowed(url: &Url) -> std::result::Result<IpAddr, String> {
    let host = url
        .host_str()
        .ok_or_else(|| "URL has no host".to_string())?;
    let blocked = "refusing to fetch internal address (loopback/link-local/private)".to_string();

    if let Ok(ip) = host.parse::<IpAddr>() {
        return if ip_is_blocked(&ip) {
            Err(blocked)
        } else {
            Ok(ip)
        };
    }

    let port = url.port_or_known_default().unwrap_or(0);
    let resolved: Vec<SocketAddr> = tokio::net::lookup_host((host, port))
        .await
        .map_err(|err| format!("failed to resolve host: {err}"))?
        .collect();
    let first = resolved
        .first()
        .ok_or_else(|| "failed to resolve host".to_string())?
        .ip();
    for addr in &resolved {
        if ip_is_blocked(&addr.ip()) {
            return Err(blocked);
        }
    }
    Ok(first)
}

pub(crate) fn web_url_host(raw: &str) -> std::result::Result<String, String> {
    parse_http_url(raw).and_then(|url| {
        url.host_str()
            .map(str::to_string)
            .ok_or_else(|| "URL has no host".to_string())
    })
}

fn redirect_url(current: &Url, response: &WebHttpResponse) -> std::result::Result<Url, String> {
    let location = response
        .header("location")
        .ok_or_else(|| "redirect response did not include a location".to_string())?;
    current
        .join(location)
        .map_err(|err| format!("invalid redirect location: {err}"))
        .and_then(|url| parse_http_url(url.as_str()))
}

pub(crate) fn is_textual_content_type(content_type: &str) -> bool {
    let mime = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    mime.is_empty()
        || mime.starts_with("text/")
        || matches!(
            mime.as_str(),
            "application/json"
                | "application/xml"
                | "application/xhtml+xml"
                | "application/javascript"
                | "application/x-javascript"
                | "image/svg+xml"
        )
        || mime.ends_with("+json")
        || mime.ends_with("+xml")
}

fn content_type_is_html(content_type: &str) -> bool {
    let mime = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    matches!(mime.as_str(), "text/html" | "application/xhtml+xml")
}

/// Extract the lowercased `charset` parameter from a `Content-Type` header, if present.
fn charset_from_content_type(content_type: &str) -> Option<String> {
    content_type.split(';').skip(1).find_map(|param| {
        let (name, value) = param.split_once('=')?;
        if name.trim().eq_ignore_ascii_case("charset") {
            Some(value.trim().trim_matches('"').to_ascii_lowercase())
        } else {
            None
        }
    })
}

/// Decode a response body to text using the charset declared in its `Content-Type`.
///
/// Handles the common single-byte legacy encodings (`windows-1252` and
/// `ISO-8859-1`/Latin-1) whose high bytes are not valid UTF-8 and would
/// otherwise be replaced with U+FFFD. Falls back to a lossy UTF-8 decode when
/// the charset is absent, UTF-8, or unrecognized.
pub(crate) fn decode_body(bytes: &[u8], content_type: &str) -> String {
    match charset_from_content_type(content_type).as_deref() {
        Some("windows-1252" | "cp1252") => bytes.iter().map(|&b| windows_1252_char(b)).collect(),
        Some("iso-8859-1" | "latin1" | "latin-1" | "iso8859-1" | "us-ascii" | "ascii") => {
            // Latin-1 maps every byte directly to the matching Unicode code point.
            bytes.iter().map(|&b| b as char).collect()
        }
        _ => String::from_utf8_lossy(bytes).into_owned(),
    }
}

/// Map a single windows-1252 byte to its Unicode character.
///
/// Identical to Latin-1 except for the `0x80..=0x9F` range, which windows-1252
/// fills with printable punctuation (e.g. the curly apostrophe `0x92`).
fn windows_1252_char(byte: u8) -> char {
    match byte {
        0x80 => '\u{20AC}', // €
        0x82 => '\u{201A}', // ‚
        0x83 => '\u{0192}', // ƒ
        0x84 => '\u{201E}', // „
        0x85 => '\u{2026}', // …
        0x86 => '\u{2020}', // †
        0x87 => '\u{2021}', // ‡
        0x88 => '\u{02C6}', // ˆ
        0x89 => '\u{2030}', // ‰
        0x8A => '\u{0160}', // Š
        0x8B => '\u{2039}', // ‹
        0x8C => '\u{0152}', // Œ
        0x8E => '\u{017D}', // Ž
        0x91 => '\u{2018}', // ‘
        0x92 => '\u{2019}', // ’
        0x93 => '\u{201C}', // “
        0x94 => '\u{201D}', // ”
        0x95 => '\u{2022}', // •
        0x96 => '\u{2013}', // –
        0x97 => '\u{2014}', // —
        0x98 => '\u{02DC}', // ˜
        0x99 => '\u{2122}', // ™
        0x9A => '\u{0161}', // š
        0x9B => '\u{203A}', // ›
        0x9C => '\u{0153}', // œ
        0x9E => '\u{017E}', // ž
        0x9F => '\u{0178}', // Ÿ
        // 0x81, 0x8D, 0x8F, 0x90, 0x9D are unassigned; fall back to Latin-1.
        other => other as char,
    }
}

pub(crate) fn html_to_text(html: &str) -> String {
    let stripped = strip_html_blocks(html);
    let mut text = String::new();
    let mut in_tag = false;
    for char in stripped.chars() {
        match char {
            '<' => {
                in_tag = true;
                text.push(' ');
            }
            '>' => {
                in_tag = false;
                text.push(' ');
            }
            _ if !in_tag => text.push(char),
            _ => {}
        }
    }
    collapse_whitespace(&decode_html_entities(&text))
}

fn strip_html_blocks(html: &str) -> String {
    let mut output = html.to_string();
    for tag in ["script", "style", "noscript", "iframe", "object", "embed"] {
        output = strip_html_block_tag(&output, tag);
    }
    output
}

fn strip_html_block_tag(input: &str, tag: &str) -> String {
    let mut output = String::new();
    let mut rest = input;
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    loop {
        let Some(start) = find_ascii_case_insensitive(rest, &open) else {
            output.push_str(rest);
            break;
        };
        output.push_str(&rest[..start]);
        let after_start = &rest[start..];
        let Some(end) = find_ascii_case_insensitive(after_start, &close) else {
            break;
        };
        rest = &after_start[end + close.len()..];
    }
    output
}

fn find_ascii_case_insensitive(haystack: &str, needle: &str) -> Option<usize> {
    let needle = needle.as_bytes();
    haystack
        .as_bytes()
        .windows(needle.len())
        .position(|window| window.eq_ignore_ascii_case(needle))
}

fn decode_html_entities(input: &str) -> String {
    input
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WebSearchArgs {
    pub(crate) query: String,
    num_results: Option<usize>,
    search_type: Option<WebSearchType>,
    livecrawl: Option<WebSearchLivecrawl>,
    context_max_characters: Option<usize>,
    timeout_ms: Option<u64>,
    output_byte_cap: Option<usize>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum WebSearchType {
    #[default]
    Auto,
    Fast,
    Deep,
}

impl WebSearchType {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Fast => "fast",
            Self::Deep => "deep",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum WebSearchLivecrawl {
    #[default]
    Fallback,
    Preferred,
}

impl WebSearchLivecrawl {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Fallback => "fallback",
            Self::Preferred => "preferred",
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WebFetchArgs {
    pub(crate) url: String,
    format: Option<WebFetchFormat>,
    timeout_ms: Option<u64>,
    max_response_bytes: Option<usize>,
    output_byte_cap: Option<usize>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum WebFetchFormat {
    #[default]
    Text,
    Html,
}

impl WebFetchFormat {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Html => "html",
        }
    }
}

#[derive(Debug)]
enum WebFetchOutcome {
    Redirect {
        status: u16,
        original_url: String,
        redirect_url: String,
    },
    Fetched {
        final_url: String,
        status: u16,
        content_type: String,
        bytes: Vec<u8>,
    },
}

#[cfg(test)]
#[path = "web_tests.rs"]
mod tests;
