use std::{
    collections::BTreeSet,
    env, fs,
    path::Path,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use reqwest::{Client, header::HeaderMap};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use urouter_eval::{BenchmarkCase, BenchmarkCategory};
use urouter_types::MoneyNanoUsd;

type BoxError = Box<dyn std::error::Error>;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GatewayBenchmarkSuite {
    schema_version: u32,
    #[serde(default = "one")]
    repeats: usize,
    cases: Vec<GatewayCaseSpec>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GatewayCaseSpec {
    id: String,
    category: BenchmarkCategory,
    semantic_task: String,
    request: Value,
    #[serde(default)]
    repeat_latest_user_text: Option<RepeatText>,
    expected: GatewayExpectation,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RepeatText {
    #[serde(default)]
    prefix: String,
    text: String,
    count: usize,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct GatewayExpectation {
    tier: Option<String>,
    model: Option<String>,
    #[serde(default)]
    content_contains: Vec<String>,
    tool_name: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct GatewayBenchmarkOutput {
    pub(crate) cases: Vec<BenchmarkCase>,
    pub(crate) report: GatewayBenchmarkReport,
}

#[derive(Debug, Serialize)]
pub(crate) struct GatewayBenchmarkReport {
    schema_version: u32,
    started_at_unix_s: u64,
    base_url: String,
    requested_budget_nano_usd: u64,
    observed_cost_nano_usd: u64,
    planned_requests: usize,
    completed_requests: usize,
    pub(crate) budget_exceeded: bool,
    passed: usize,
    pub(crate) failed: usize,
    runs: Vec<GatewayRunReport>,
}

#[derive(Debug, Serialize)]
struct GatewayRunReport {
    id: String,
    category: BenchmarkCategory,
    status: u16,
    passed: bool,
    latency_ms: u64,
    cost_nano_usd: u64,
    tier: Option<String>,
    model: Option<String>,
    reason: Option<String>,
    checks: Vec<GatewayCheck>,
}

#[derive(Debug, Serialize)]
struct GatewayCheck {
    name: String,
    passed: bool,
    expected: String,
    actual: String,
}

pub(crate) async fn run(
    suite_path: &Path,
    base_url: &str,
    maximum_cost_nano_usd: u64,
    maximum_requests: usize,
    timeout_seconds: u64,
    gateway_api_key_env: Option<&str>,
    only_cases: &[String],
) -> Result<GatewayBenchmarkOutput, BoxError> {
    let suite: GatewayBenchmarkSuite = serde_json::from_str(&fs::read_to_string(suite_path)?)?;
    validate_suite(&suite, maximum_cost_nano_usd)?;
    let selected = select_cases(&suite, only_cases)?;
    let request_count = suite.repeats.saturating_mul(selected.len());
    if request_count > maximum_requests || maximum_requests == 0 {
        return Err(
            format!("suite requests {request_count} exceed maximum {maximum_requests}").into(),
        );
    }
    // The benchmark addresses a Gateway directly, like the Gateway's own
    // upstream client. An ambient `http_proxy` must not silently re-route a
    // benchmark run or stall a loopback Gateway.
    let client = Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(timeout_seconds))
        .build()?;
    let gateway_token = gateway_api_key_env
        .map(|name| env::var(name).map_err(|_| format!("gateway API key env {name} is not set")))
        .transpose()?;
    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
    let started_at_unix_s = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let mut cases = Vec::new();
    let mut runs = Vec::new();
    let mut observed_cost = 0_u64;
    let planned_requests = request_count;
    let mut budget_exceeded = false;

    'runs: for repeat in 0..suite.repeats {
        for spec in &selected {
            let request = expanded_request(spec)?;
            let started = Instant::now();
            let mut builder = client.post(&url).json(&request);
            if let Some(token) = gateway_token.as_deref() {
                builder = builder.bearer_auth(token);
            }
            let response = builder.send().await?;
            let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
            let status = response.status().as_u16();
            let headers = response.headers().clone();
            let body: Value = response.json().await.unwrap_or(Value::Null);
            let id = format!("{}-run-{}", spec.id, repeat + 1);
            let report = evaluate(&id, spec, status, elapsed_ms, &headers, &body);
            observed_cost = observed_cost.saturating_add(report.cost_nano_usd);
            cases.push(BenchmarkCase {
                id,
                category: spec.category,
                semantic_task: spec.semantic_task.clone(),
                quality_millionths: if report.passed { 1_000_000 } else { 0 },
                cost_nano_usd: report.cost_nano_usd,
                latency_ms: report.latency_ms,
                supported: status == 200,
            });
            runs.push(report);
            if observed_cost > maximum_cost_nano_usd {
                budget_exceeded = true;
                break 'runs;
            }
        }
    }
    let passed = runs.iter().filter(|run| run.passed).count();
    Ok(GatewayBenchmarkOutput {
        cases,
        report: GatewayBenchmarkReport {
            schema_version: 1,
            started_at_unix_s,
            base_url: base_url.to_owned(),
            requested_budget_nano_usd: maximum_cost_nano_usd,
            observed_cost_nano_usd: observed_cost,
            planned_requests,
            completed_requests: runs.len(),
            budget_exceeded,
            passed,
            failed: runs.len().saturating_sub(passed),
            runs,
        },
    })
}

fn one() -> usize {
    1
}

fn validate_suite(
    suite: &GatewayBenchmarkSuite,
    maximum_cost_nano_usd: u64,
) -> Result<(), BoxError> {
    if suite.schema_version != 1 {
        return Err("gateway benchmark suite schema_version must be 1".into());
    }
    if suite.repeats == 0 || suite.cases.is_empty() {
        return Err("gateway benchmark suite must contain cases and positive repeats".into());
    }
    if maximum_cost_nano_usd == 0 {
        return Err("max-cost-nano-usd must be greater than zero".into());
    }
    let mut ids = BTreeSet::new();
    let mut categories = BTreeSet::new();
    for spec in &suite.cases {
        if spec.id.trim().is_empty() || !ids.insert(&spec.id) {
            return Err("gateway benchmark case IDs must be non-empty and unique".into());
        }
        categories.insert(spec.category);
        if spec.request.get("stream").and_then(Value::as_bool) == Some(true) {
            return Err(format!("case {} requests unsupported streaming", spec.id).into());
        }
        if spec.request.get("model").and_then(Value::as_str) != Some("urouter/auto") {
            return Err(format!("case {} must request urouter/auto", spec.id).into());
        }
    }
    if categories.len() != 5 {
        return Err("gateway benchmark suite must cover all five categories".into());
    }
    Ok(())
}

fn select_cases<'a>(
    suite: &'a GatewayBenchmarkSuite,
    only_cases: &[String],
) -> Result<Vec<&'a GatewayCaseSpec>, BoxError> {
    if only_cases.is_empty() {
        return Ok(suite.cases.iter().collect());
    }
    let requested = only_cases
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let selected = suite
        .cases
        .iter()
        .filter(|spec| requested.contains(spec.id.as_str()))
        .collect::<Vec<_>>();
    if selected.len() != requested.len() {
        return Err("one or more --only-case IDs do not exist in the suite".into());
    }
    Ok(selected)
}

fn expanded_request(spec: &GatewayCaseSpec) -> Result<Value, BoxError> {
    let mut request = spec.request.clone();
    if let Some(repeat) = &spec.repeat_latest_user_text {
        if repeat.count == 0 || repeat.count > 10_000 || repeat.text.len() > 1_024 {
            return Err(format!("case {} has invalid repeat_latest_user_text", spec.id).into());
        }
        let messages = request
            .get_mut("messages")
            .and_then(Value::as_array_mut)
            .ok_or_else(|| format!("case {} has no messages", spec.id))?;
        let content = messages
            .iter_mut()
            .rev()
            .find(|message| message.get("role").and_then(Value::as_str) == Some("user"))
            .and_then(|message| message.get_mut("content"))
            .ok_or_else(|| format!("case {} latest user content must be text", spec.id))?;
        let original = content
            .as_str()
            .ok_or_else(|| format!("case {} latest user content must be text", spec.id))?;
        let expanded = format!(
            "{}{} {}",
            repeat.prefix,
            repeat.text.repeat(repeat.count),
            original
        );
        if expanded.len() > 1_048_576 {
            return Err(format!("case {} expands beyond one MiB", spec.id).into());
        }
        *content = Value::String(expanded);
    }
    Ok(request)
}

fn evaluate(
    id: &str,
    spec: &GatewayCaseSpec,
    status: u16,
    latency_ms: u64,
    headers: &HeaderMap,
    body: &Value,
) -> GatewayRunReport {
    let tier = header(headers, "x-urouter-tier");
    let model = header(headers, "x-urouter-model");
    let reason = header(headers, "x-urouter-reason");
    let content = body
        .pointer("/choices/0/message/content")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let tool_name = body
        .pointer("/choices/0/message/tool_calls/0/function/name")
        .and_then(Value::as_str);
    let mut checks = vec![GatewayCheck {
        name: "http_status".to_owned(),
        passed: status == 200,
        expected: "200".to_owned(),
        actual: status.to_string(),
    }];
    if let Some(expected) = spec.expected.tier.as_deref() {
        checks.push(check("tier", expected, tier.as_deref()));
    }
    if let Some(expected) = spec.expected.model.as_deref() {
        checks.push(check("model", expected, model.as_deref()));
    }
    for expected in &spec.expected.content_contains {
        checks.push(GatewayCheck {
            name: "content_contains".to_owned(),
            passed: content.to_lowercase().contains(&expected.to_lowercase()),
            expected: expected.clone(),
            actual: format!("content_length={}", content.len()),
        });
    }
    if let Some(expected) = spec.expected.tool_name.as_deref() {
        checks.push(check("tool_name", expected, tool_name));
    }
    GatewayRunReport {
        id: id.to_owned(),
        category: spec.category,
        status,
        passed: checks.iter().all(|check| check.passed),
        latency_ms,
        cost_nano_usd: nano_usd(body.pointer("/urouter/cost/total")),
        tier,
        model,
        reason,
        checks,
    }
}

fn check(name: &str, expected: &str, actual: Option<&str>) -> GatewayCheck {
    GatewayCheck {
        name: name.to_owned(),
        passed: actual == Some(expected),
        expected: expected.to_owned(),
        actual: actual.unwrap_or("missing").to_owned(),
    }
}

fn header(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

fn nano_usd(value: Option<&Value>) -> u64 {
    let Some(value) = value else {
        return 0;
    };
    if let Some(value) = value
        .as_u64()
        .or_else(|| value.as_i64().and_then(|value| u64::try_from(value).ok()))
    {
        return value;
    }
    serde_json::from_value::<MoneyNanoUsd>(value.clone())
        .ok()
        .and_then(|money| u64::try_from(money.as_nano_usd()).ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    #[test]
    fn evaluates_route_content_tool_and_cost_without_retaining_body() {
        let spec = GatewayCaseSpec {
            id: "weather".to_owned(),
            category: BenchmarkCategory::Tool,
            semantic_task: "weather".to_owned(),
            request: json!({"model":"urouter/auto","messages":[],"stream":false}),
            repeat_latest_user_text: None,
            expected: GatewayExpectation {
                tier: Some("capable".to_owned()),
                model: Some("model-a".to_owned()),
                content_contains: Vec::new(),
                tool_name: Some("get_weather".to_owned()),
            },
        };
        let mut headers = HeaderMap::new();
        headers.insert("x-urouter-tier", "capable".parse().unwrap());
        headers.insert("x-urouter-model", "model-a".parse().unwrap());
        let report = evaluate(
            "weather-run-1",
            &spec,
            200,
            25,
            &headers,
            &json!({
                "choices":[{"message":{"tool_calls":[{"function":{"name":"get_weather"}}]}}],
                "urouter":{"cost":{"total":"0.000000123"}}
            }),
        );
        assert!(report.passed);
        assert_eq!(report.cost_nano_usd, 123);
        assert_eq!(report.checks.len(), 4);
    }

    #[tokio::test]
    async fn runner_executes_five_categories_accounts_cost_and_discards_body() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for _ in 0..5 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = vec![0_u8; 16_384];
                let _ = socket.read(&mut request).await.unwrap();
                let body = json!({
                    "choices": [{"message": {"content": "PROVIDER_PRIVATE_BODY"}}],
                    "urouter": {"cost": {"total": "0.000000123"}}
                })
                .to_string();
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nx-urouter-tier: efficient\r\nx-urouter-model: model-a\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });
        let categories = ["daily", "tool", "math", "code", "long_context"];
        let cases = categories
            .iter()
            .enumerate()
            .map(|(index, category)| {
                json!({
                    "id": format!("case-{index}"),
                    "category": category,
                    "semantic_task": category,
                    "request": {
                        "model": "urouter/auto",
                        "messages": [{"role": "user", "content": "test"}],
                        "stream": false
                    },
                    "expected": {"tier": "efficient", "model": "model-a"}
                })
            })
            .collect::<Vec<_>>();
        let path = env::temp_dir().join(format!(
            "urouter-gateway-benchmark-{}.json",
            std::process::id()
        ));
        fs::write(
            &path,
            serde_json::to_vec(&json!({
                "schema_version": 1,
                "repeats": 1,
                "cases": cases
            }))
            .unwrap(),
        )
        .unwrap();
        let output = run(
            &path,
            &format!("http://{address}/v1"),
            1_000,
            5,
            5,
            None,
            &[],
        )
        .await
        .unwrap();
        fs::remove_file(path).unwrap();
        server.await.unwrap();
        assert_eq!(output.cases.len(), 5);
        assert_eq!(output.report.observed_cost_nano_usd, 615);
        assert_eq!(output.report.passed, 5);
        assert!(!output.report.budget_exceeded);
        let serialized = serde_json::to_string(&output.report).unwrap();
        assert!(!serialized.contains("PROVIDER_PRIVATE_BODY"));
    }
}
