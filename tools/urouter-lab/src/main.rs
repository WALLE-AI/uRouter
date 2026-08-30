use std::{collections::BTreeMap, env, fs, path::PathBuf, process::ExitCode};

use clap::{Parser, Subcommand};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use urouter_artifact::{
    ArtifactSupportDomain, ExportGates, FeatureWeights, LinearPolicy, RouterArtifact,
};
use urouter_eval::{
    BenchmarkCase, CounterfactualSample, DatasetBundle, EvaluationRecord, ExportFilter,
    FeedbackPolicy, benchmark_summary, build_dataset, counterfactual_report,
    evaluation_record_from_gateway,
};

mod gateway_benchmark;

type BoxError = Box<dyn std::error::Error>;

#[derive(Debug, Parser)]
#[command(
    name = "urouter-lab",
    about = "Reproducible uRouter evaluation and artifact pipeline"
)]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Normalize {
        #[arg(long)]
        gateway_records: PathBuf,
        #[arg(long)]
        output: PathBuf,
    },
    Dataset {
        #[arg(long)]
        records: PathBuf,
        #[arg(long)]
        filter: PathBuf,
        #[arg(long)]
        deletion_generations: PathBuf,
        #[arg(long)]
        feedback_policy: PathBuf,
        #[arg(long)]
        output: PathBuf,
    },
    Counterfactual {
        #[arg(long)]
        samples: PathBuf,
        #[arg(long)]
        output: PathBuf,
    },
    Benchmark {
        #[arg(long)]
        cases: PathBuf,
        #[arg(long)]
        output: PathBuf,
    },
    GatewayBenchmark {
        #[arg(long)]
        suite: PathBuf,
        #[arg(long, default_value = "http://127.0.0.1:8787/v1")]
        base_url: String,
        #[arg(long)]
        cases_output: PathBuf,
        #[arg(long)]
        report_output: PathBuf,
        #[arg(long)]
        max_cost_nano_usd: u64,
        #[arg(long, default_value_t = 25)]
        max_requests: usize,
        #[arg(long, default_value_t = 120)]
        timeout_seconds: u64,
        #[arg(long)]
        gateway_api_key_env: Option<String>,
        #[arg(long)]
        only_case: Vec<String>,
    },
    Train {
        #[arg(long)]
        dataset: PathBuf,
        #[arg(long)]
        counterfactual_samples: PathBuf,
        #[arg(long)]
        benchmark_cases: PathBuf,
        #[arg(long)]
        baseline_tier: String,
        #[arg(long)]
        promoted_tier: String,
        #[arg(long)]
        signing_key_env: String,
        #[arg(long, default_value_t = 42)]
        seed: u64,
        #[arg(long, default_value_t = 0.0)]
        minimum_dr_lower_bound: f64,
        #[arg(long, default_value_t = 1.0)]
        minimum_effective_samples: f64,
        #[arg(long)]
        output: PathBuf,
    },
    Verify {
        #[arg(long)]
        artifact: PathBuf,
        #[arg(long)]
        catalog_revision: String,
        #[arg(long)]
        route_revision: String,
        #[arg(long)]
        signing_key_env: String,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    match run(Args::parse()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("urouter-lab: {error}");
            ExitCode::FAILURE
        }
    }
}

#[allow(clippy::too_many_lines)]
async fn run(args: Args) -> Result<(), BoxError> {
    match args.command {
        Command::Normalize {
            gateway_records,
            output,
        } => {
            let source = read_gateway_records(&gateway_records)?;
            let mut records = Vec::new();
            let mut rejected = Vec::new();
            for (index, value) in source.iter().enumerate() {
                match evaluation_record_from_gateway(value) {
                    Ok(record) => records.push(record),
                    Err(error) => rejected.push(NormalizationRejection {
                        index,
                        error: error.to_string(),
                    }),
                }
            }
            write_json(&output, &NormalizationOutput { records, rejected })
        }
        Command::Dataset {
            records,
            filter,
            deletion_generations,
            feedback_policy,
            output,
        } => write_json(
            &output,
            &build_dataset(
                read_json::<Vec<EvaluationRecord>>(&records)?,
                &read_json::<ExportFilter>(&filter)?,
                &read_json::<BTreeMap<String, u64>>(&deletion_generations)?,
                &read_json::<FeedbackPolicy>(&feedback_policy)?,
            )?,
        ),
        Command::Counterfactual { samples, output } => write_json(
            &output,
            &counterfactual_report(&read_json::<Vec<CounterfactualSample>>(&samples)?)?,
        ),
        Command::Benchmark { cases, output } => write_json(
            &output,
            &benchmark_summary(&read_json::<Vec<BenchmarkCase>>(&cases)?),
        ),
        Command::GatewayBenchmark {
            suite,
            base_url,
            cases_output,
            report_output,
            max_cost_nano_usd,
            max_requests,
            timeout_seconds,
            gateway_api_key_env,
            only_case,
        } => {
            let output = gateway_benchmark::run(
                &suite,
                &base_url,
                max_cost_nano_usd,
                max_requests,
                timeout_seconds,
                gateway_api_key_env.as_deref(),
                &only_case,
            )
            .await?;
            write_json(&cases_output, &output.cases)?;
            write_json(&report_output, &output.report)?;
            if output.report.budget_exceeded {
                return Err("gateway benchmark stopped after exceeding its cost budget".into());
            }
            if output.report.failed > 0 {
                return Err("one or more gateway benchmark cases failed".into());
            }
            Ok(())
        }
        Command::Train {
            dataset,
            counterfactual_samples,
            benchmark_cases,
            baseline_tier,
            promoted_tier,
            signing_key_env,
            seed,
            minimum_dr_lower_bound,
            minimum_effective_samples,
            output,
        } => {
            let dataset = read_json::<DatasetBundle>(&dataset)?;
            let counterfactual = counterfactual_report(&read_json::<Vec<CounterfactualSample>>(
                &counterfactual_samples,
            )?)?;
            let cases = read_json::<Vec<BenchmarkCase>>(&benchmark_cases)?;
            let benchmark = benchmark_summary(&cases);
            let required_categories = 5;
            if benchmark.by_category.len() != required_categories || benchmark.cases == 0 {
                return Err(
                    "benchmark must cover daily, tool, math, code, and long_context".into(),
                );
            }
            let counterfactual_passed = counterfactual.doubly_robust.confidence_95_low
                >= minimum_dr_lower_bound
                && counterfactual.effective_sample_size >= minimum_effective_samples;
            let quality_lower_bound = cases
                .iter()
                .map(|case| case.quality_millionths)
                .min()
                .unwrap_or(-1);
            let failed = dataset
                .records
                .iter()
                .filter(|record| {
                    record
                        .outcome
                        .as_ref()
                        .is_some_and(|outcome| outcome.failed)
                })
                .count();
            let error_rate = millionths(failed, dataset.records.len());
            let model = train_linear_policy(&dataset, baseline_tier, promoted_tier);
            let support = ArtifactSupportDomain {
                semantic_tasks: dataset
                    .records
                    .iter()
                    .map(|record| record.semantic_task.clone())
                    .collect(),
                maximum_input_text_bytes: dataset
                    .records
                    .iter()
                    .map(|record| record.context.features.input_text_bytes)
                    .max()
                    .unwrap_or(0),
                tools_supported: dataset
                    .records
                    .iter()
                    .any(|record| record.context.features.available_tool_count > 0),
            };
            let mut artifact = RouterArtifact::build(
                dataset.manifest.feature_schema,
                dataset.manifest.catalog_revision.clone(),
                dataset.manifest.route_revision.clone(),
                dataset.manifest.dataset_hash.clone(),
                seed,
                dataset.records.len(),
                model,
                support,
                ExportGates {
                    reproducible: true,
                    privacy_passed: !dataset.manifest.privacy_audit_samples.is_empty(),
                    support_domain_defined: !dataset.records.is_empty(),
                    counterfactual_passed,
                    quality_lower_bound_millionths: quality_lower_bound,
                    maximum_error_rate_millionths: error_rate,
                    maximum_cost_regression_millionths: 0,
                },
            )?;
            let key = env::var(&signing_key_env)
                .map_err(|_| "artifact signing key environment variable is not set")?;
            artifact.sign(key.as_bytes())?;
            write_json(&output, &artifact)
        }
        Command::Verify {
            artifact,
            catalog_revision,
            route_revision,
            signing_key_env,
        } => {
            let artifact = read_json::<RouterArtifact>(&artifact)?;
            let key = env::var(&signing_key_env)
                .map_err(|_| "artifact signing key environment variable is not set")?;
            artifact.verify(
                artifact.feature_schema,
                &catalog_revision,
                &route_revision,
                Some(key.as_bytes()),
            )?;
            println!("{}", artifact.artifact_revision);
            Ok(())
        }
    }
}

#[derive(Debug, Serialize)]
struct NormalizationOutput {
    records: Vec<EvaluationRecord>,
    rejected: Vec<NormalizationRejection>,
}

#[derive(Debug, Serialize)]
struct NormalizationRejection {
    index: usize,
    error: String,
}

fn train_linear_policy(
    dataset: &DatasetBundle,
    baseline_tier: String,
    promoted_tier: String,
) -> LinearPolicy {
    let promoted = dataset
        .records
        .iter()
        .filter(|record| record.selected_tier == promoted_tier)
        .count();
    let promoted_ratio = millionths(promoted, dataset.records.len());
    LinearPolicy {
        baseline_tier,
        promoted_tier,
        threshold_millis: 1_000,
        bias_millis: i64::from(promoted_ratio / 1_000).saturating_sub(500),
        weights: FeatureWeights {
            input_kib_millis: 20,
            message_millis: 30,
            tool_millis: 1_200,
            image_millis: 1_200,
            structured_millis: 800,
            reasoning_millis: 900,
        },
    }
}

fn millionths(numerator: usize, denominator: usize) -> u32 {
    if denominator == 0 {
        return 1_000_000;
    }
    let value = numerator.saturating_mul(1_000_000) / denominator;
    u32::try_from(value).unwrap_or(1_000_000)
}

fn read_json<T: DeserializeOwned>(path: &PathBuf) -> Result<T, BoxError> {
    Ok(serde_json::from_str(&fs::read_to_string(path)?)?)
}

fn read_gateway_records(path: &PathBuf) -> Result<Vec<Value>, BoxError> {
    let source = fs::read_to_string(path)?;
    if let Ok(records) = serde_json::from_str::<Vec<Value>>(&source) {
        return Ok(records);
    }
    source
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).map_err(Into::into))
        .collect()
}

fn write_json(path: &PathBuf, value: &impl Serialize) -> Result<(), BoxError> {
    fs::write(path, serde_json::to_vec_pretty(value)?)?;
    Ok(())
}
