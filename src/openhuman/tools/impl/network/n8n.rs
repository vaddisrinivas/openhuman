//! Native n8n connector backed by the n8n public REST API.

use crate::openhuman::config::N8nConfig;
use crate::openhuman::tools::traits::{PermissionLevel, Tool, ToolCategory, ToolResult};
use async_trait::async_trait;
use reqwest::{header, Url};
use serde_json::{json, Value};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

const API_KEY_HEADER: &str = "X-N8N-API-KEY";
const TOOL_NAME: &str = "n8n";
const MAX_LIMIT: u64 = 250;

pub struct N8nTool {
    base_url: Option<Url>,
    api_key: Option<String>,
    client: reqwest::Client,
    max_response_size: usize,
    config_error: Option<String>,
}

impl N8nTool {
    pub fn new(config: &N8nConfig) -> Self {
        Self::with_client(
            config,
            crate::openhuman::config::build_runtime_proxy_client_with_timeouts(
                "tool.n8n",
                config.timeout_secs,
                10,
            ),
        )
    }

    fn with_client(config: &N8nConfig, client: reqwest::Client) -> Self {
        let mut config_error = None;
        let base_url = match config.base_url.as_deref().map(normalize_base_url) {
            Some(Ok(url)) => Some(url),
            Some(Err(error)) => {
                config_error = Some(error.to_string());
                None
            }
            None => None,
        };

        let api_key = config
            .api_key
            .as_deref()
            .map(str::trim)
            .filter(|key| !key.is_empty())
            .map(str::to_string);

        Self {
            base_url,
            api_key,
            client,
            max_response_size: config.max_response_size.max(1),
            config_error,
        }
    }

    fn configured(&self) -> bool {
        self.config_error.is_none() && self.base_url.is_some() && self.api_key.is_some()
    }

    fn require_configured(&self) -> Result<(&Url, &str), String> {
        if let Some(error) = &self.config_error {
            return Err(format!("n8n connector configuration is invalid: {error}"));
        }
        let base_url = self.base_url.as_ref().ok_or_else(|| {
            "n8n connector is not configured: set [n8n].base_url or OPENHUMAN_N8N_BASE_URL"
                .to_string()
        })?;
        let api_key = self.api_key.as_deref().ok_or_else(|| {
            "n8n connector is not configured: set [n8n].api_key or OPENHUMAN_N8N_API_KEY"
                .to_string()
        })?;
        Ok((base_url, api_key))
    }

    fn api_url(&self, resource_path: &str) -> Result<String, String> {
        let (base_url, _) = self.require_configured()?;
        let mut url = base_url.clone();
        let base_path = url.path().trim_end_matches('/');
        let api_prefix = if base_path.ends_with("/api/v1") {
            base_path.to_string()
        } else {
            format!("{base_path}/api/v1")
        };
        let resource_path = resource_path.trim_start_matches('/');
        url.set_path(&format!("{api_prefix}/{resource_path}"));
        url.set_query(None);
        url.set_fragment(None);
        Ok(url.to_string())
    }

    fn status_payload(&self) -> Value {
        json!({
            "configured": self.configured(),
            "base_url": self.base_url.as_ref().map(|url| url.as_str()),
            "api_key_configured": self.api_key.is_some(),
            "config_error": self.config_error.as_deref(),
            "operations": [
                "status",
                "list_workflows",
                "get_workflow",
                "list_executions",
                "get_execution"
            ],
            "env": {
                "base_url": "OPENHUMAN_N8N_BASE_URL or N8N_BASE_URL",
                "api_key": "OPENHUMAN_N8N_API_KEY or N8N_API_KEY"
            }
        })
    }

    async fn get_json(&self, resource_path: &str, query: Vec<(String, String)>) -> ToolResult {
        let (_, api_key) = match self.require_configured() {
            Ok(config) => config,
            Err(error) => return ToolResult::error(error),
        };
        let url = match self.api_url(resource_path) {
            Ok(url) => url,
            Err(error) => return ToolResult::error(error),
        };

        let request = self
            .client
            .get(&url)
            .header(API_KEY_HEADER, api_key)
            .header(header::ACCEPT, "application/json")
            .query(&query);

        let response = match request.send().await {
            Ok(response) => response,
            Err(error) => return ToolResult::error(format!("n8n API request failed: {error}")),
        };
        self.response_to_result(&url, response).await
    }

    async fn response_to_result(&self, url: &str, response: reqwest::Response) -> ToolResult {
        let status = response.status();
        let bytes = match response.bytes().await {
            Ok(bytes) => bytes,
            Err(error) => {
                return ToolResult::error(format!("Failed to read n8n API response: {error}"))
            }
        };

        if bytes.len() > self.max_response_size {
            return ToolResult::error(format!(
                "n8n API response exceeded max_response_size ({} bytes). Narrow the query or disable include_data.",
                self.max_response_size
            ));
        }

        let body = String::from_utf8_lossy(&bytes);
        if !status.is_success() {
            return ToolResult::error(format!(
                "n8n API error {} {}: {}",
                status.as_u16(),
                status.canonical_reason().unwrap_or("Unknown"),
                truncate_for_error(&body)
            ));
        }

        let data = match serde_json::from_str::<Value>(&body) {
            Ok(data) => data,
            Err(_) => json!({ "body": body.to_string() }),
        };

        ToolResult::json(json!({
            "status": status.as_u16(),
            "url": url,
            "data": data,
        }))
    }
}

#[async_trait]
impl Tool for N8nTool {
    fn name(&self) -> &str {
        TOOL_NAME
    }

    fn description(&self) -> &str {
        "Read n8n workflows and execution history from a configured n8n instance via the public /api/v1 REST API. \
         Configure with [n8n].base_url + [n8n].api_key, or OPENHUMAN_N8N_BASE_URL + OPENHUMAN_N8N_API_KEY."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "operation": {
                    "type": "string",
                    "enum": [
                        "status",
                        "list_workflows",
                        "get_workflow",
                        "list_executions",
                        "get_execution"
                    ],
                    "description": "n8n operation to run"
                },
                "id": {
                    "type": "string",
                    "description": "Workflow or execution id for get_workflow/get_execution"
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 250,
                    "description": "Maximum items to return for list operations"
                },
                "cursor": {
                    "type": "string",
                    "description": "Pagination cursor from the previous n8n response"
                },
                "active": {
                    "type": "boolean",
                    "description": "Workflow active-state filter for list_workflows"
                },
                "name": {
                    "type": "string",
                    "description": "Workflow name filter for list_workflows"
                },
                "tags": {
                    "type": "string",
                    "description": "Comma-separated workflow tag filter for list_workflows"
                },
                "project_id": {
                    "type": "string",
                    "description": "n8n project id filter"
                },
                "exclude_pinned_data": {
                    "type": "boolean",
                    "description": "Set n8n excludePinnedData for workflow reads"
                },
                "workflow_id": {
                    "type": "string",
                    "description": "Workflow id filter for list_executions"
                },
                "status": {
                    "type": "string",
                    "enum": [
                        "canceled",
                        "crashed",
                        "error",
                        "new",
                        "running",
                        "success",
                        "unknown",
                        "waiting"
                    ],
                    "description": "Execution status filter for list_executions"
                },
                "include_data": {
                    "type": "boolean",
                    "default": false,
                    "description": "Include detailed execution data. Defaults to false."
                },
                "redact_execution_data": {
                    "type": "boolean",
                    "default": true,
                    "description": "When include_data=true, keep execution data redacted by default."
                }
            },
            "required": ["operation"]
        })
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::ReadOnly
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::Skill
    }

    fn is_concurrency_safe(&self, _args: &Value) -> bool {
        true
    }

    fn max_result_size_chars(&self) -> Option<usize> {
        Some(50_000)
    }

    async fn execute(&self, args: Value) -> anyhow::Result<ToolResult> {
        let operation = args
            .get("operation")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("Missing 'operation' parameter"))?;

        let result = match operation {
            "status" => ToolResult::json(self.status_payload()),
            "list_workflows" => self.list_workflows(&args).await,
            "get_workflow" => self.get_workflow(&args).await,
            "list_executions" => self.list_executions(&args).await,
            "get_execution" => self.get_execution(&args).await,
            _ => ToolResult::error(format!(
                "Unknown n8n operation '{operation}'. Use status, list_workflows, get_workflow, list_executions, or get_execution."
            )),
        };
        Ok(result)
    }
}

impl N8nTool {
    async fn list_workflows(&self, args: &Value) -> ToolResult {
        let mut query = Vec::new();
        add_bool_query(args, &mut query, "active", "active");
        add_string_query(args, &mut query, "tags", "tags");
        add_string_query(args, &mut query, "name", "name");
        add_string_query(args, &mut query, "project_id", "projectId");
        add_bool_query(args, &mut query, "exclude_pinned_data", "excludePinnedData");
        if let Err(error) = add_limit_query(args, &mut query) {
            return ToolResult::error(error);
        }
        add_string_query(args, &mut query, "cursor", "cursor");
        self.get_json("workflows", query).await
    }

    async fn get_workflow(&self, args: &Value) -> ToolResult {
        let id = match required_id(args) {
            Ok(id) => id,
            Err(error) => return ToolResult::error(error),
        };
        let mut query = Vec::new();
        add_bool_query(args, &mut query, "exclude_pinned_data", "excludePinnedData");
        self.get_json(&format!("workflows/{id}"), query).await
    }

    async fn list_executions(&self, args: &Value) -> ToolResult {
        let mut query = Vec::new();
        add_execution_data_query(args, &mut query);
        if let Err(error) = add_execution_status_query(args, &mut query) {
            return ToolResult::error(error);
        }
        add_string_query(args, &mut query, "workflow_id", "workflowId");
        add_string_query(args, &mut query, "project_id", "projectId");
        if let Err(error) = add_limit_query(args, &mut query) {
            return ToolResult::error(error);
        }
        add_string_query(args, &mut query, "cursor", "cursor");
        self.get_json("executions", query).await
    }

    async fn get_execution(&self, args: &Value) -> ToolResult {
        let id = match required_id(args) {
            Ok(id) => id,
            Err(error) => return ToolResult::error(error),
        };
        let mut query = Vec::new();
        add_execution_data_query(args, &mut query);
        self.get_json(&format!("executions/{id}"), query).await
    }
}

fn normalize_base_url(raw: &str) -> anyhow::Result<Url> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        anyhow::bail!("base_url cannot be empty");
    }
    if trimmed.chars().any(char::is_whitespace) {
        anyhow::bail!("base_url cannot contain whitespace");
    }

    let mut url = Url::parse(trimmed)?;
    match url.scheme() {
        "https" => {}
        "http" if is_local_or_private_host(&url) => {}
        "http" => {
            anyhow::bail!(
                "refusing to send n8n API key over public HTTP; use HTTPS or a local/private HTTP host"
            );
        }
        scheme => anyhow::bail!("unsupported n8n URL scheme '{scheme}'; use http or https"),
    }

    if url.host_str().is_none() {
        anyhow::bail!("base_url must include a host");
    }
    if !url.username().is_empty() || url.password().is_some() {
        anyhow::bail!("base_url userinfo is not allowed");
    }
    if url.query().is_some() || url.fragment().is_some() {
        anyhow::bail!("base_url must not include query or fragment");
    }

    if url.path() != "/" {
        let trimmed_path = url.path().trim_end_matches('/').to_string();
        url.set_path(&trimmed_path);
    }
    Ok(url)
}

fn is_local_or_private_host(url: &Url) -> bool {
    let Some(host) = url
        .host_str()
        .map(|host| host.trim_matches('[').trim_matches(']'))
    else {
        return false;
    };
    let host = host.to_ascii_lowercase();
    let local_tld = host
        .rsplit('.')
        .next()
        .is_some_and(|label| label == "local");
    if host == "localhost" || host.ends_with(".localhost") || local_tld {
        return true;
    }
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(ip)) => is_non_global_v4(ip),
        Ok(IpAddr::V6(ip)) => is_non_global_v6(ip),
        Err(_) => false,
    }
}

fn is_non_global_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || ip.is_multicast()
        || (a == 100 && (64..=127).contains(&b))
        || a >= 240
        || (a == 192 && b == 0 && (c == 0 || c == 2))
        || (a == 198 && b == 51)
        || (a == 203 && b == 0)
        || (a == 198 && (18..=19).contains(&b))
}

fn is_non_global_v6(ip: Ipv6Addr) -> bool {
    let segs = ip.segments();
    ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        || (segs[0] & 0xfe00) == 0xfc00
        || (segs[0] & 0xffc0) == 0xfe80
        || (segs[0] == 0x2001 && segs[1] == 0x0db8)
        || ip.to_ipv4_mapped().is_some_and(is_non_global_v4)
}

fn add_string_query(args: &Value, query: &mut Vec<(String, String)>, key: &str, param: &str) {
    if let Some(value) = args
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        query.push((param.to_string(), value.to_string()));
    }
}

fn add_bool_query(args: &Value, query: &mut Vec<(String, String)>, key: &str, param: &str) {
    if let Some(value) = args.get(key).and_then(Value::as_bool) {
        query.push((param.to_string(), value.to_string()));
    }
}

fn add_limit_query(args: &Value, query: &mut Vec<(String, String)>) -> Result<(), String> {
    let Some(limit) = args.get("limit").and_then(Value::as_u64) else {
        return Ok(());
    };
    if !(1..=MAX_LIMIT).contains(&limit) {
        return Err(format!("limit must be between 1 and {MAX_LIMIT}"));
    }
    query.push(("limit".to_string(), limit.to_string()));
    Ok(())
}

fn add_execution_status_query(
    args: &Value,
    query: &mut Vec<(String, String)>,
) -> Result<(), String> {
    let Some(status) = args
        .get("status")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return Ok(());
    };
    const ALLOWED: &[&str] = &[
        "canceled", "crashed", "error", "new", "running", "success", "unknown", "waiting",
    ];
    if !ALLOWED.contains(&status) {
        return Err(format!("status must be one of {}", ALLOWED.join(", ")));
    }
    query.push(("status".to_string(), status.to_string()));
    Ok(())
}

fn add_execution_data_query(args: &Value, query: &mut Vec<(String, String)>) {
    let include_data = args
        .get("include_data")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    query.push(("includeData".to_string(), include_data.to_string()));

    if include_data {
        let redact = args
            .get("redact_execution_data")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        query.push(("redactExecutionData".to_string(), redact.to_string()));
    }
}

fn required_id(args: &Value) -> Result<String, String> {
    let id = args
        .get("id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| "Missing required 'id' parameter".to_string())?;
    Ok(urlencoding::encode(id).into_owned())
}

fn truncate_for_error(body: &str) -> String {
    const MAX_ERROR_CHARS: usize = 1000;
    if body.chars().count() <= MAX_ERROR_CHARS {
        return body.to_string();
    }
    let prefix: String = body.chars().take(MAX_ERROR_CHARS).collect();
    format!("{prefix}... [truncated]")
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::{Query, State};
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::get;
    use axum::{Json, Router};
    use serde::Deserialize;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct MockState {
        seen_api_key: Arc<Mutex<Option<String>>>,
        seen_query: Arc<Mutex<Option<Value>>>,
    }

    #[derive(Debug, Deserialize)]
    struct WorkflowQuery {
        active: Option<bool>,
        limit: Option<u64>,
        cursor: Option<String>,
    }

    async fn start_mock_server(state: MockState) -> String {
        let app = Router::new()
            .route(
                "/api/v1/workflows",
                get(
                    |State(state): State<MockState>,
                     headers: HeaderMap,
                     Query(query): Query<WorkflowQuery>| async move {
                        let api_key = headers
                            .get(API_KEY_HEADER)
                            .and_then(|value| value.to_str().ok())
                            .map(str::to_string);
                        *state.seen_api_key.lock().unwrap() = api_key;
                        *state.seen_query.lock().unwrap() = Some(json!({
                            "active": query.active,
                            "limit": query.limit,
                            "cursor": query.cursor,
                        }));
                        Json(json!({
                            "data": [
                                { "id": "wf_1", "name": "Smoke", "active": true }
                            ],
                            "nextCursor": null
                        }))
                    },
                ),
            )
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{addr}")
    }

    fn test_config(base_url: Option<String>) -> N8nConfig {
        N8nConfig {
            enabled: true,
            base_url,
            api_key: Some("test-key".into()),
            max_response_size: 1_000_000,
            timeout_secs: 10,
        }
    }

    #[test]
    fn status_reports_missing_config_without_secret() {
        let tool = N8nTool::with_client(&N8nConfig::default(), reqwest::Client::new());
        let payload = tool.status_payload();
        assert_eq!(payload["configured"], false);
        assert_eq!(payload["api_key_configured"], false);
        assert!(payload.to_string().contains("OPENHUMAN_N8N_API_KEY"));
    }

    #[test]
    fn normalize_base_url_accepts_private_http_and_rejects_public_http() {
        assert!(normalize_base_url("http://127.0.0.1:5678").is_ok());
        assert!(normalize_base_url("http://100.116.244.64:5678").is_ok());
        let err = normalize_base_url("http://example.com").unwrap_err();
        assert!(err.to_string().contains("public HTTP"));
        assert!(normalize_base_url("https://example.com").is_ok());
    }

    #[test]
    fn api_url_appends_api_v1_once() {
        let cfg = test_config(Some("https://n8n.example.com/api/v1".into()));
        let tool = N8nTool::with_client(&cfg, reqwest::Client::new());
        assert_eq!(
            tool.api_url("workflows").unwrap(),
            "https://n8n.example.com/api/v1/workflows"
        );
    }

    #[test]
    fn schema_marks_read_only_skill_tool() {
        let tool = N8nTool::with_client(&test_config(None), reqwest::Client::new());
        assert_eq!(tool.name(), "n8n");
        assert_eq!(tool.permission_level(), PermissionLevel::ReadOnly);
        assert_eq!(tool.category(), ToolCategory::Skill);
        assert!(tool.parameters_schema()["properties"]["operation"].is_object());
    }

    #[tokio::test]
    async fn list_workflows_calls_n8n_api_with_api_key() {
        let state = MockState::default();
        let base_url = start_mock_server(state.clone()).await;
        let tool = N8nTool::with_client(&test_config(Some(base_url)), reqwest::Client::new());

        let result = tool
            .execute(json!({
                "operation": "list_workflows",
                "active": true,
                "limit": 25,
                "cursor": "next"
            }))
            .await
            .unwrap();

        assert!(!result.is_error, "{}", result.output());
        assert_eq!(
            state.seen_api_key.lock().unwrap().as_deref(),
            Some("test-key")
        );
        let query = state.seen_query.lock().unwrap().clone().unwrap();
        assert_eq!(query["active"], true);
        assert_eq!(query["limit"], 25);
        assert_eq!(query["cursor"], "next");
        assert!(result.output().contains("wf_1"));
    }

    #[tokio::test]
    async fn surfaces_n8n_http_errors_without_leaking_api_key() {
        let app = Router::new().route(
            "/api/v1/workflows",
            get(|| async { (StatusCode::UNAUTHORIZED, "bad key") }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let tool = N8nTool::with_client(
            &test_config(Some(format!("http://{addr}"))),
            reqwest::Client::new(),
        );
        let result = tool
            .execute(json!({ "operation": "list_workflows" }))
            .await
            .unwrap();

        assert!(result.is_error);
        assert!(result.output().contains("401"));
        assert!(!result.output().contains("test-key"));
    }
}
