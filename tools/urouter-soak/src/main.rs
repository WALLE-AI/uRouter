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

#[derive(Debug, Parser)]
#[command(about = "Run a bounded uRouter core soak/SLO gate")]
struct Args {
    #[arg(long, default_value = "http://127.0.0.1:8787")]
    gateway: String,
    #[arg(long, default_value_t = 1_000)]
    requests: usize,
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
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Serialize)]
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

async fn run(args: &Args) -> Result<Report, Box<dyn std::error::Error>> {
    if args.requests == 0 || args.concurrency == 0 {
        return Err("requests and concurrency must be greater than zero".into());
    }
    let request_count =
        u32::try_from(args.requests).map_err(|_| "requests must not exceed 4,294,967,295")?;
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

    stream::iter(0..args.requests)
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
        requests: args.requests,
        succeeded: args.requests - failed,
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
    })
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
