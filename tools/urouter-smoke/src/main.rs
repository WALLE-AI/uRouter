use std::{
    collections::BTreeMap,
    env, fs,
    path::PathBuf,
    process::{Command, ExitCode},
    time::Instant,
};

use clap::Parser;
use serde::Serialize;
use serde_json::{Value, json};
use urouter_ai::{
    auth::AuthPlan,
    capabilities::{Modality, ThinkingSupport},
    catalog::CatalogSnapshot,
    endpoint::EndpointPlan,
    evidence::CatalogEvidence,
    pricing::{CostBreakdown, PriceSource, calculate_actual_cost},
};
use urouter_types::{ModelId, Usage};

const STATUS_MARKER: &str = "__UROUTER_STATUS__";
const IMAGE_DATA_URL: &str = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";

#[derive(Debug, Parser)]
#[command(about = "Run explicit, catalog-driven endpoint smoke tests")]
struct Args {
    #[arg(long, default_value = "catalog/catalog.json")]
    catalog: PathBuf,
    #[arg(long)]
    model: String,
    #[arg(long, default_value_t = 90)]
    timeout_seconds: u64,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Serialize)]
struct CheckResult {
    name: &'static str,
    passed: bool,
    status: Option<u16>,
    elapsed_ms: u128,
    detail: String,
}

#[derive(Debug, Serialize)]
struct SmokeReport {
    model_id: ModelId,
    upstream_model_id: String,
    endpoint: String,
    evidence: CatalogEvidence,
    usage: Option<Usage>,
    cost: Option<CostBreakdown>,
    checks: Vec<CheckResult>,
}

impl SmokeReport {
    fn passed(&self) -> bool {
        self.checks.iter().all(|check| check.passed)
    }
}

struct CurlResponse {
    status: u16,
    body: String,
    elapsed_ms: u128,
}

fn main() -> ExitCode {
    let args = Args::parse();
    match run(&args) {
        Ok(report) => {
            if args.json {
                println!(
                    "{}",
                    serde_json::to_string(&report).expect("smoke report must serialize")
                );
            } else {
                print_report(&report);
            }
            if report.passed() {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

#[allow(clippy::too_many_lines)]
fn run(args: &Args) -> Result<SmokeReport, Box<dyn std::error::Error>> {
    let catalog = CatalogSnapshot::from_json_str(&fs::read_to_string(&args.catalog)?)?;
    let requested = ModelId::new(&args.model)?;
    let resolution = catalog.resolve_model(&requested).ok_or("model not found")?;
    let model = resolution.model;
    let provider = catalog
        .provider(&model.provider)
        .ok_or("model provider not found")?;
    let endpoint = EndpointPlan::for_model(provider, model)?;
    let headers = resolve_headers(&endpoint)?;
    let chat_url = format!(
        "{}/chat/completions",
        endpoint.url.as_str().trim_end_matches('/')
    );
    let mut checks = Vec::new();

    let basic = request_json(
        &chat_url,
        &headers,
        &json!({
            "model": model.upstream_id,
            "messages": [{"role": "user", "content": "Reply with exactly UROUTER_SMOKE_OK"}],
            "temperature": 0,
            "max_tokens": 128,
            "stream": false
        }),
        args.timeout_seconds,
    )?;
    let (usage, basic_check) = check_basic(&basic);
    checks.push(basic_check);

    if model.compat.supports_usage_in_streaming == Some(true) {
        let response = request_json(
            &chat_url,
            &headers,
            &json!({
                "model": model.upstream_id,
                "messages": [{"role": "user", "content": "Reply with the single word pong."}],
                "temperature": 0,
                "max_tokens": 64,
                "stream": true,
                "stream_options": {"include_usage": true}
            }),
            args.timeout_seconds,
        )?;
        checks.push(check_stream(&response));
    }

    if model.capabilities.structured_output {
        let response = request_json(
            &chat_url,
            &headers,
            &json!({
                "model": model.upstream_id,
                "messages": [{"role": "user", "content": "Return status=ok and count=3."}],
                "response_format": {
                    "type": "json_schema",
                    "json_schema": {
                        "name": "result",
                        "strict": true,
                        "schema": {
                            "type": "object",
                            "properties": {
                                "status": {"type": "string", "enum": ["ok"]},
                                "count": {"type": "integer"}
                            },
                            "required": ["status", "count"],
                            "additionalProperties": false
                        }
                    }
                },
                "temperature": 0,
                "max_tokens": 128
            }),
            args.timeout_seconds,
        )?;
        checks.push(check_structured(&response));
    }

    if model.capabilities.tool_calling {
        let response = request_json(
            &chat_url,
            &headers,
            &json!({
                "model": model.upstream_id,
                "messages": [{"role": "user", "content": "Get the weather in Beijing using the tool."}],
                "tools": [{
                    "type": "function",
                    "function": {
                        "name": "get_weather",
                        "description": "Get current weather",
                        "parameters": {
                            "type": "object",
                            "properties": {"city": {"type": "string"}},
                            "required": ["city"]
                        }
                    }
                }],
                "tool_choice": "required",
                "temperature": 0,
                "max_tokens": 256
            }),
            args.timeout_seconds,
        )?;
        checks.push(check_tool(&response));
    }

    if model
        .capabilities
        .input_modalities
        .contains(&Modality::Image)
    {
        let response = request_json(
            &chat_url,
            &headers,
            &json!({
                "model": model.upstream_id,
                "messages": [{
                    "role": "user",
                    "content": [
                        {"type": "text", "text": "Confirm image receipt with IMAGE_OK."},
                        {"type": "image_url", "image_url": {"url": IMAGE_DATA_URL}}
                    ]
                }],
                "temperature": 0,
                "max_tokens": 64
            }),
            args.timeout_seconds,
        )?;
        checks.push(check_image(&response));
    }

    if !matches!(model.capabilities.reasoning, ThinkingSupport::Unsupported) {
        let response = request_json(
            &chat_url,
            &headers,
            &json!({
                "model": model.upstream_id,
                "messages": [{"role": "user", "content": "Think briefly: what is 17 times 19?"}],
                "chat_template_kwargs": {"enable_thinking": true},
                "temperature": 0,
                "max_tokens": 256
            }),
            args.timeout_seconds,
        )?;
        checks.push(check_reasoning(&response));
    }

    let cost = usage
        .map(|actual| calculate_actual_cost(&model.cost, actual))
        .transpose()?;
    let evidence =
        CatalogEvidence::from_catalog(&catalog, resolution.canonical_id, PriceSource::Catalog)
            .ok_or("catalog evidence model disappeared")?;
    Ok(SmokeReport {
        model_id: resolution.canonical_id.clone(),
        upstream_model_id: model.upstream_id.clone(),
        endpoint: endpoint.url.to_string(),
        evidence,
        usage,
        cost,
        checks,
    })
}

fn resolve_headers(
    endpoint: &EndpointPlan,
) -> Result<BTreeMap<String, String>, Box<dyn std::error::Error>> {
    let mut headers = endpoint.public_headers.clone();
    if let AuthPlan::ApiKeyEnv {
        env: variable,
        header,
        prefix,
    } = &endpoint.auth
    {
        let secret = env::var(variable)
            .map_err(|_| format!("credential environment variable {variable} is not set"))?;
        headers.insert(header.clone(), format!("{prefix}{secret}"));
    }
    Ok(headers)
}

fn request_json(
    url: &str,
    headers: &BTreeMap<String, String>,
    payload: &Value,
    timeout_seconds: u64,
) -> Result<CurlResponse, Box<dyn std::error::Error>> {
    let started = Instant::now();
    let mut command = Command::new("curl");
    command
        .arg("--noproxy")
        .arg("*")
        .arg("-sS")
        .arg("--max-time")
        .arg(timeout_seconds.to_string())
        .arg("-H")
        .arg("content-type: application/json");
    for (name, value) in headers {
        command.arg("-H").arg(format!("{name}: {value}"));
    }
    let output = command
        .arg("-d")
        .arg(serde_json::to_string(payload)?)
        .arg("-w")
        .arg(format!("\n{STATUS_MARKER}%{{http_code}}"))
        .arg(url)
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "curl failed with status {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )
        .into());
    }
    let output = String::from_utf8(output.stdout)?;
    let (body, status) = output
        .rsplit_once(STATUS_MARKER)
        .ok_or("curl response did not contain HTTP status")?;
    Ok(CurlResponse {
        status: status.trim().parse()?,
        body: body.trim_end().to_owned(),
        elapsed_ms: started.elapsed().as_millis(),
    })
}

fn check_basic(response: &CurlResponse) -> (Option<Usage>, CheckResult) {
    let parsed_response = serde_json::from_str::<Value>(&response.body);
    let usage = parsed_response.as_ref().ok().and_then(parse_usage);
    let content = parsed_response
        .as_ref()
        .ok()
        .and_then(|value| value.pointer("/choices/0/message/content"))
        .and_then(Value::as_str);
    let valid =
        response.status == 200 && usage.is_some() && content.is_some_and(|text| !text.is_empty());
    let detail = match (&parsed_response, content) {
        (Ok(_), Some(text)) => format!(
            "content_present=true exact={}",
            text.trim() == "UROUTER_SMOKE_OK"
        ),
        (Ok(_), None) => "response did not contain assistant content".to_owned(),
        (Err(error), _) => format!("invalid JSON response: {error}"),
    };
    (
        usage,
        CheckResult {
            name: "chat",
            passed: valid,
            status: Some(response.status),
            elapsed_ms: response.elapsed_ms,
            detail,
        },
    )
}

fn parse_usage(value: &Value) -> Option<Usage> {
    let usage = value.get("usage")?;
    let prompt_tokens = usage.get("prompt_tokens")?.as_u64()?;
    let cache_read = usage
        .pointer("/prompt_tokens_details/cached_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    Some(Usage {
        input: prompt_tokens.checked_sub(cache_read)?,
        output: usage.get("completion_tokens")?.as_u64()?,
        cache_read,
        ..Usage::default()
    })
}

fn check_stream(response: &CurlResponse) -> CheckResult {
    let passed = response.status == 200
        && response.body.contains("data: [DONE]")
        && response.body.contains("\"usage\":{");
    CheckResult {
        name: "stream_usage",
        passed,
        status: Some(response.status),
        elapsed_ms: response.elapsed_ms,
        detail: format!(
            "done={} usage={}",
            response.body.contains("data: [DONE]"),
            response.body.contains("\"usage\":")
        ),
    }
}

fn check_structured(response: &CurlResponse) -> CheckResult {
    let parsed = serde_json::from_str::<Value>(&response.body);
    let content = parsed
        .as_ref()
        .ok()
        .and_then(|value| value.pointer("/choices/0/message/content"))
        .and_then(Value::as_str)
        .and_then(|text| serde_json::from_str::<Value>(text).ok());
    let conforms = content.as_ref().is_some_and(|value| {
        value.get("status").and_then(Value::as_str) == Some("ok")
            && value.get("count").and_then(Value::as_i64) == Some(3)
    });
    CheckResult {
        name: "structured_output",
        passed: response.status == 200 && conforms,
        status: Some(response.status),
        elapsed_ms: response.elapsed_ms,
        detail: format!("schema_conforms={conforms}"),
    }
}

fn check_tool(response: &CurlResponse) -> CheckResult {
    let parsed_response = serde_json::from_str::<Value>(&response.body);
    let name = parsed_response
        .as_ref()
        .ok()
        .and_then(|value| value.pointer("/choices/0/message/tool_calls/0/function/name"))
        .and_then(Value::as_str);
    let valid = response.status == 200 && name == Some("get_weather");
    CheckResult {
        name: "tool_calling",
        passed: valid,
        status: Some(response.status),
        elapsed_ms: response.elapsed_ms,
        detail: format!("tool={}", name.unwrap_or("missing")),
    }
}

fn check_image(response: &CurlResponse) -> CheckResult {
    let content = serde_json::from_str::<Value>(&response.body)
        .ok()
        .and_then(|value| value.pointer("/choices/0/message/content").cloned())
        .and_then(|value| value.as_str().map(str::to_owned));
    let passed = response.status == 200
        && content
            .as_deref()
            .is_some_and(|text| text.contains("IMAGE_OK"));
    CheckResult {
        name: "image_input",
        passed,
        status: Some(response.status),
        elapsed_ms: response.elapsed_ms,
        detail: format!("acknowledged={}", content.is_some()),
    }
}

fn check_reasoning(response: &CurlResponse) -> CheckResult {
    let parsed = serde_json::from_str::<Value>(&response.body);
    let reasoning = parsed
        .as_ref()
        .ok()
        .and_then(|value| value.pointer("/choices/0/message/reasoning"))
        .and_then(Value::as_str);
    let content = parsed
        .as_ref()
        .ok()
        .and_then(|value| value.pointer("/choices/0/message/content"))
        .and_then(Value::as_str);
    let separated = reasoning.is_some_and(|text| !text.is_empty())
        && content.is_some_and(|text| text.contains("323"));
    CheckResult {
        name: "reasoning",
        passed: response.status == 200 && separated,
        status: Some(response.status),
        elapsed_ms: response.elapsed_ms,
        detail: format!("separated={separated}"),
    }
}

fn print_report(report: &SmokeReport) {
    println!("model: {}", report.model_id);
    println!("endpoint: {}", report.endpoint);
    println!("catalog: {}", report.evidence.content_hash);
    for check in &report.checks {
        println!(
            "{}: {} ({} ms) {}",
            check.name,
            if check.passed { "pass" } else { "fail" },
            check.elapsed_ms,
            check.detail
        );
    }
    if let Some(cost) = &report.cost {
        println!("actual cost: {}", cost.total);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_chat_usage() {
        let value = json!({
            "usage": {
                "prompt_tokens": 12,
                "completion_tokens": 3,
                "prompt_tokens_details": {"cached_tokens": 4}
            }
        });
        let usage = parse_usage(&value).unwrap();
        assert_eq!(usage.input, 8);
        assert_eq!(usage.output, 3);
        assert_eq!(usage.cache_read, 4);
    }
}
