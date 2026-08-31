//! Redis restart and upstream failure gate.
//!
//! Starts a repository-owned Redis container, runs the contracts that are
//! `#[ignore]`d without a real Redis, restarts the container, reruns them, and
//! writes a machine-readable report. Any failed scenario fails the gate.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use clap::Args as ClapArgs;
use serde_json::{Value, json};

use crate::BoxError;

#[derive(ClapArgs)]
pub struct ChaosArgs {
    #[arg(long, default_value = "target/p2-chaos-report.json")]
    output_path: PathBuf,
    #[arg(long, default_value_t = 16380)]
    redis_port: u16,
    /// Optional Gateway base URL; when set, the SLO soak runs as a final gate.
    #[arg(long)]
    gateway: Option<String>,
    #[arg(long, default_value_t = 1000)]
    soak_requests: u32,
}

struct Check {
    name: &'static str,
    passed: bool,
    exit_code: i32,
    elapsed_ms: u128,
}

pub fn run(args: &ChaosArgs, root: &Path) -> Result<bool, BoxError> {
    let container = format!("urouter-p2-chaos-{}", std::process::id());
    let started_at = SystemTime::now();
    let mut checks = Vec::new();

    let outcome = run_scenarios(args, root, &container, &mut checks);
    // The container is removed even when a scenario returns early with an error.
    let _ = docker(&["rm", "-f", &container]);

    let passed = outcome.is_ok() && checks.iter().all(|check| check.passed);
    write_report(args, root, started_at, &checks, passed)?;
    outcome?;
    Ok(passed)
}

fn run_scenarios(
    args: &ChaosArgs,
    root: &Path,
    container: &str,
    checks: &mut Vec<Check>,
) -> Result<(), BoxError> {
    let port_mapping = format!("{}:6379", args.redis_port);
    docker(&[
        "run",
        "-d",
        "--rm",
        "--name",
        container,
        "-p",
        &port_mapping,
        "redis:7-alpine",
    ])?;

    if !wait_until_ready(container) {
        return Err("Redis chaos container did not become ready".into());
    }

    let redis_url = format!("redis://127.0.0.1:{}/", args.redis_port);
    for (name, arguments) in BEFORE_RESTART {
        checks.push(cargo_gate(name, arguments, root, &redis_url));
    }

    docker(&["restart", container])?;
    if !wait_until_ready(container) {
        return Err("Redis chaos container did not become ready after restart".into());
    }

    for (name, arguments) in AFTER_RESTART {
        checks.push(cargo_gate(name, arguments, root, &redis_url));
    }

    if let Some(gateway) = &args.gateway {
        checks.push(soak_gate(args, gateway, root, &redis_url));
    }
    Ok(())
}

fn soak_gate(args: &ChaosArgs, gateway: &str, root: &Path, redis_url: &str) -> Check {
    let requests = args.soak_requests.to_string();
    cargo_gate(
        "gateway_slo",
        &[
            "run",
            "-q",
            "-p",
            "urouter-soak",
            "--",
            "--gateway",
            gateway,
            "--requests",
            &requests,
            "--json",
        ],
        root,
        redis_url,
    )
}

const LIBRARY_CONTRACTS: &[&str] = &["test", "-p", "urouter-gateway", "--lib", "--", "--ignored"];
const GATEWAY_CONTRACTS: &[&str] = &[
    "test",
    "-p",
    "urouter-gateway",
    "--bin",
    "urouter-gateway",
    "--",
    "--ignored",
];

const BEFORE_RESTART: [(&str, &[&str]); 4] = [
    (
        "typed_timeout_fallback",
        &[
            "test",
            "-p",
            "urouter-gateway",
            "--bin",
            "urouter-gateway",
            "timeout_uses_its_typed_fallback_chain",
        ],
    ),
    (
        "partial_stream_failure",
        &[
            "test",
            "-p",
            "urouter-gateway",
            "--bin",
            "urouter-gateway",
            "partial_stream_failure_never_runs_success_finalization",
        ],
    ),
    ("redis_library_contracts_before_restart", LIBRARY_CONTRACTS),
    ("redis_gateway_contracts_before_restart", GATEWAY_CONTRACTS),
];

const AFTER_RESTART: [(&str, &[&str]); 2] = [
    ("redis_library_contracts_after_restart", LIBRARY_CONTRACTS),
    ("redis_gateway_contracts_after_restart", GATEWAY_CONTRACTS),
];

fn cargo_gate(name: &'static str, arguments: &[&str], root: &Path, redis_url: &str) -> Check {
    let started = Instant::now();
    let status = Command::new(env!("CARGO"))
        .current_dir(root)
        .env("UROUTER_TEST_REDIS_URL", redis_url)
        .args(arguments)
        .status();
    let exit_code = match &status {
        Ok(status) => status.code().unwrap_or(-1),
        Err(_) => -1,
    };
    Check {
        name,
        passed: exit_code == 0,
        exit_code,
        elapsed_ms: started.elapsed().as_millis(),
    }
}

fn wait_until_ready(container: &str) -> bool {
    for _ in 0..30 {
        if docker(&["exec", container, "redis-cli", "ping"]).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    false
}

fn docker(arguments: &[&str]) -> Result<(), BoxError> {
    let output = Command::new("docker").args(arguments).output()?;
    if output.status.success() {
        return Ok(());
    }
    Err(format!(
        "docker {} failed: {}",
        arguments.join(" "),
        String::from_utf8_lossy(&output.stderr).trim()
    )
    .into())
}

fn write_report(
    args: &ChaosArgs,
    root: &Path,
    started_at: SystemTime,
    checks: &[Check],
    passed: bool,
) -> Result<(), BoxError> {
    let report = json!({
        "schema_version": 1,
        "started_at": rfc3339_utc(started_at)?,
        "completed_at": rfc3339_utc(SystemTime::now())?,
        "redis_restart_count": 1,
        "passed": passed,
        "checks": checks.iter().map(check_json).collect::<Vec<Value>>(),
    });
    let resolved = root.join(&args.output_path);
    if let Some(parent) = resolved.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&resolved, serde_json::to_vec_pretty(&report)?)?;
    println!("P2 chaos report: {}", resolved.display());
    Ok(())
}

fn check_json(check: &Check) -> Value {
    json!({
        "name": check.name,
        "passed": check.passed,
        "exit_code": check.exit_code,
        "elapsed_ms": check.elapsed_ms,
    })
}

/// Formats a `SystemTime` as RFC 3339 UTC without pulling in a date library.
fn rfc3339_utc(time: SystemTime) -> Result<String, BoxError> {
    let seconds = time.duration_since(UNIX_EPOCH)?.as_secs();
    let days = i64::try_from(seconds / 86_400)?;
    let second_of_day = seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    Ok(format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        second_of_day / 3600,
        (second_of_day % 3600) / 60,
        second_of_day % 60
    ))
}

/// Howard Hinnant's `civil_from_days`, restricted to the proleptic Gregorian
/// calendar and to days at or after the Unix epoch.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = u32::try_from(day_of_year - (153 * shifted_month + 2) / 5 + 1).unwrap_or(1);
    let month = u32::try_from(if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    })
    .unwrap_or(1);
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_the_unix_epoch() {
        assert_eq!(rfc3339_utc(UNIX_EPOCH).unwrap(), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn formats_known_instants() {
        let cases = [
            (1_709_209_496_u64, "2024-02-29T12:24:56Z"),
            (1_767_225_600_u64, "2026-01-01T00:00:00Z"),
            (951_782_400_u64, "2000-02-29T00:00:00Z"),
        ];
        for (seconds, expected) in cases {
            let time = UNIX_EPOCH + Duration::from_secs(seconds);
            assert_eq!(rfc3339_utc(time).unwrap(), expected, "seconds={seconds}");
        }
    }

    #[test]
    fn report_check_json_is_stable() {
        let check = Check {
            name: "typed_timeout_fallback",
            passed: false,
            exit_code: 101,
            elapsed_ms: 42,
        };
        assert_eq!(
            check_json(&check),
            json!({
                "name": "typed_timeout_fallback",
                "passed": false,
                "exit_code": 101,
                "elapsed_ms": 42,
            })
        );
    }
}
