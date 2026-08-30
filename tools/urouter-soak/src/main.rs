use std::{
    process::ExitCode,
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use clap::Parser;
use futures_util::{StreamExt, stream};
use serde::Serialize;
use serde_json::json;

#[derive(Debug, Clone, Parser)]
#[command(about = "Run a bounded uRouter core soak/SLO gate")]
struct Args {
    #[arg(long, default_value = "http://127.0.0.1:8787")]
    gateway: String,
    #[arg(long, default_value_t = 1_000)]
    requests: usize,
    #[arg(long, default_value_t = 0)]
    duration_seconds: u64,
    #[arg(long, default_value_t = 16)]
    concurrency: usize,
    #[arg(long, default_value_t = 250)]
    max_p95_ms: u64,
    #[arg(long, default_value_t = 0.001)]
    max_error_rate: f64,
    #[arg(long, default_value_t = 64)]
    max_rss_growth_mib: u64,
    #[arg(long, default_value = "soak")]
    tenant: String,
    #[arg(long, env = "UROUTER_MANAGEMENT_TOKEN")]
    management_token: Option<String>,
    #[arg(long, value_delimiter = ',')]
    canary_stages_basis_points: Vec<u16>,
    #[arg(long, default_value_t = 0)]
    stage_duration_seconds: u64,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Clone, Serialize)]
struct Report {
    requests: usize,
    succeeded: usize,
    failed: usize,
    error_rate: f64,
    elapsed_ms: u128,
    requests_per_second: f64,
    latency_p50_ms: u64,
    latency_p95_ms: u64,
    latency_p99_ms: u64,
    rss_start_mib: Option<u64>,
    rss_end_mib: Option<u64>,
    rss_growth_mib: Option<u64>,
    passed: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    stages: Vec<StageReport>,
}

#[derive(Debug, Clone, Serialize)]
struct StageReport {
    canary_basis_points: u16,
    requests: usize,
    failed: usize,
    error_rate: f64,
    latency_p95_ms: u64,
    passed: bool,
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();
    match run(&args).await {
        Ok(report) => {
            if args.json {
                println!(
                    "{}",
                    serde_json::to_string(&report).expect("report serializes")
                );
            } else {
                println!(
                    "requests={} ok={} failed={} error_rate={:.4}% rps={:.1} p50={}ms p95={}ms p99={}ms rss_growth={} passed={}",
                    report.requests,
                    report.succeeded,
                    report.failed,
                    report.error_rate * 100.0,
                    report.requests_per_second,
                    report.latency_p50_ms,
                    report.latency_p95_ms,
                    report.latency_p99_ms,
                    report
                        .rss_growth_mib
                        .map_or_else(|| "n/a".to_owned(), |value| format!("{value}MiB")),
                    report.passed
                );
            }
            if report.passed {
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
async fn run(args: &Args) -> Result<Report, Box<dyn std::error::Error>> {
    if args.canary_stages_basis_points.is_empty() {
        return run_workload(args).await;
    }
    if args
        .canary_stages_basis_points
        .iter()
        .any(|stage| *stage == 0 || *stage > 10_000)
    {
        return Err("canary stages must be in 1..=10000 basis points".into());
    }
    let token = args
        .management_token
        .as_deref()
        .ok_or("canary stages require a management token")?;
    let client = reqwest::Client::builder().no_proxy().build()?;
    let mut reports = Vec::new();
    for stage in &args.canary_stages_basis_points {
        update_rollout(args, &client, token, *stage).await?;
        let mut stage_args = args.clone();
        stage_args.canary_stages_basis_points.clear();
        if args.stage_duration_seconds > 0 {
            stage_args.duration_seconds = args.stage_duration_seconds;
        } else {
            stage_args.requests = args
                .requests
                .div_ceil(args.canary_stages_basis_points.len());
        }
        let report = run_workload(&stage_args).await?;
        reports.push((*stage, report.clone()));
        if !report.passed {
            rollback(args, &client, token, *stage).await?;
            break;
        }
    }
    let first = reports.first().ok_or("no canary stage executed")?;
    let last = reports.last().expect("stage report exists");
    let requests = reports
        .iter()
        .map(|(_, report)| report.requests)
        .sum::<usize>();
    let failed = reports
        .iter()
        .map(|(_, report)| report.failed)
        .sum::<usize>();
    let elapsed_ms = reports
        .iter()
        .map(|(_, report)| report.elapsed_ms)
        .sum::<u128>();
    let request_count = u32::try_from(requests).map_err(|_| "request count overflowed")?;
    let failed_count = u32::try_from(failed).map_err(|_| "failed request count overflowed")?;
    let error_rate = if requests == 0 {
        1.0
    } else {
        f64::from(failed_count) / f64::from(request_count)
    };
    let elapsed_seconds =
        Duration::from_millis(u64::try_from(elapsed_ms).unwrap_or(u64::MAX)).as_secs_f64();
    let stages = reports
        .iter()
        .map(|(stage, report)| StageReport {
            canary_basis_points: *stage,
            requests: report.requests,
            failed: report.failed,
            error_rate: report.error_rate,
            latency_p95_ms: report.latency_p95_ms,
            passed: report.passed,
        })
        .collect::<Vec<_>>();
    Ok(Report {
        requests,
        succeeded: requests.saturating_sub(failed),
        failed,
        error_rate,
        elapsed_ms,
        requests_per_second: f64::from(request_count) / elapsed_seconds,
        latency_p50_ms: reports
            .iter()
            .map(|(_, report)| report.latency_p50_ms)
            .max()
            .unwrap_or(0),
        latency_p95_ms: reports
            .iter()
            .map(|(_, report)| report.latency_p95_ms)
            .max()
            .unwrap_or(0),
        latency_p99_ms: reports
            .iter()
            .map(|(_, report)| report.latency_p99_ms)
            .max()
            .unwrap_or(0),
        rss_start_mib: first.1.rss_start_mib,
        rss_end_mib: last.1.rss_end_mib,
        rss_growth_mib: first
            .1
            .rss_start_mib
            .zip(last.1.rss_end_mib)
            .map(|(start, end)| end.saturating_sub(start)),
        passed: reports.iter().all(|(_, report)| report.passed),
        stages,
    })
}

async fn run_workload(args: &Args) -> Result<Report, Box<dyn std::error::Error>> {
    if (args.requests == 0 && args.duration_seconds == 0) || args.concurrency == 0 {
        return Err("requests or duration, and concurrency, must be greater than zero".into());
    }
    let client = reqwest::Client::builder()
        .no_proxy()
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(10))
        .build()?;
    let endpoint = format!("{}/v1/explain", args.gateway.trim_end_matches('/'));
    let latencies = Arc::new(tokio::sync::Mutex::new(Vec::with_capacity(args.requests)));
    let failures = Arc::new(AtomicUsize::new(0));
    let sequence = Arc::new(AtomicU64::new(0));
    let rss_start = gateway_rss_mib(args, &client).await;
    let started = Instant::now();

    let target_requests = if args.duration_seconds > 0 {
        usize::MAX
    } else {
        args.requests
    };
    let stop_at = (args.duration_seconds > 0)
        .then(|| Instant::now() + Duration::from_secs(args.duration_seconds));
    stream::iter(0..target_requests)
        .take_while(|_| {
            std::future::ready(stop_at.is_none_or(|deadline| Instant::now() < deadline))
        })
        .for_each_concurrent(args.concurrency, |_| {
            let client = client.clone();
            let endpoint = endpoint.clone();
            let tenant = args.tenant.clone();
            let latencies = Arc::clone(&latencies);
            let failures = Arc::clone(&failures);
            let sequence = Arc::clone(&sequence);
            async move {
                let id = sequence.fetch_add(1, Ordering::Relaxed);
                let request_started = Instant::now();
                let result = client
                    .post(endpoint)
                    .header("x-urouter-tenant-id", tenant)
                    .header("x-urouter-task-id", format!("soak-{id}"))
                    .json(&json!({
                        "model": "urouter/auto",
                        "messages": [{"role": "user", "content": "Classify this short request."}]
                    }))
                    .send()
                    .await;
                let ok = match result {
                    Ok(response) => response.status().is_success(),
                    Err(_) => false,
                };
                if !ok {
                    failures.fetch_add(1, Ordering::Relaxed);
                }
                let elapsed = request_started.elapsed().as_millis();
                latencies
                    .lock()
                    .await
                    .push(u64::try_from(elapsed).unwrap_or(u64::MAX));
            }
        })
        .await;

    let elapsed = started.elapsed();
    let rss_end = gateway_rss_mib(args, &client).await;
    let mut latencies = Arc::try_unwrap(latencies)
        .map_err(|_| "latency samples still shared")?
        .into_inner();
    latencies.sort_unstable();
    let requests = latencies.len();
    let request_count = u32::try_from(requests).map_err(|_| "request count overflowed")?;
    let failed = failures.load(Ordering::Relaxed);
    let failed_count = u32::try_from(failed).map_err(|_| "failed request count overflowed")?;
    let error_rate = f64::from(failed_count) / f64::from(request_count);
    let rss_growth = rss_start
        .zip(rss_end)
        .map(|(start, end)| end.saturating_sub(start));
    let p95 = percentile(&latencies, 95);
    let passed = error_rate <= args.max_error_rate
        && p95 <= args.max_p95_ms
        && rss_growth.is_none_or(|growth| growth <= args.max_rss_growth_mib);
    Ok(Report {
        requests,
        succeeded: requests - failed,
        failed,
        error_rate,
        elapsed_ms: elapsed.as_millis(),
        requests_per_second: f64::from(request_count) / elapsed.as_secs_f64(),
        latency_p50_ms: percentile(&latencies, 50),
        latency_p95_ms: p95,
        latency_p99_ms: percentile(&latencies, 99),
        rss_start_mib: rss_start,
        rss_end_mib: rss_end,
        rss_growth_mib: rss_growth,
        passed,
        stages: Vec::new(),
    })
}

async fn update_rollout(
    args: &Args,
    client: &reqwest::Client,
    token: &str,
    basis_points: u16,
) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!(
        "{}/v1/artifacts/rollout",
        args.gateway.trim_end_matches('/')
    );
    client
        .post(url)
        .bearer_auth(token)
        .header("x-urouter-tenant-id", &args.tenant)
        .json(&json!({
            "rollout": {"shadow": false, "canary_basis_points": basis_points, "minimum_samples": 0, "operation_limit": 10000},
            "reason": format!("urouter-soak stage {basis_points}bp")
        }))
        .send().await?.error_for_status()?;
    Ok(())
}

async fn rollback(
    args: &Args,
    client: &reqwest::Client,
    token: &str,
    basis_points: u16,
) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!(
        "{}/v1/artifacts/rollback",
        args.gateway.trim_end_matches('/')
    );
    client
        .post(url)
        .bearer_auth(token)
        .header("x-urouter-tenant-id", &args.tenant)
        .json(&json!({"reason": format!("urouter-soak SLO failure at {basis_points}bp")}))
        .send()
        .await?
        .error_for_status()?;
    Ok(())
}

fn percentile(values: &[u64], percentile: usize) -> u64 {
    let index = values
        .len()
        .saturating_mul(percentile)
        .div_ceil(100)
        .saturating_sub(1);
    values[index.min(values.len().saturating_sub(1))]
}

async fn gateway_rss_mib(args: &Args, client: &reqwest::Client) -> Option<u64> {
    let url = format!("{}/metrics", args.gateway.trim_end_matches('/'));
    let mut request = client.get(url).header("x-urouter-tenant-id", &args.tenant);
    if let Some(token) = &args.management_token {
        request = request.bearer_auth(token);
    }
    let body = request
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?
        .text()
        .await
        .ok()?;
    body.lines().find_map(|line| {
        line.strip_prefix("process_resident_memory_bytes ")
            .and_then(|value| value.parse::<u64>().ok())
            .map(|bytes| bytes / 1_048_576)
    })
}

#[cfg(test)]
mod tests {
    use super::percentile;

    #[test]
    fn percentile_uses_nearest_rank() {
        let values = (1..=100).collect::<Vec<_>>();
        assert_eq!(percentile(&values, 50), 50);
        assert_eq!(percentile(&values, 95), 95);
        assert_eq!(percentile(&values, 99), 99);
    }
}
