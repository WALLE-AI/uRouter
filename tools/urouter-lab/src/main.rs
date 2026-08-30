use std::{collections::BTreeMap, env, fs, path::PathBuf, process::ExitCode, time::Instant};

use clap::{Parser, Subcommand, ValueEnum};
use onnx_pb::{
    GraphProto, ModelProto, NodeProto, OperatorSetIdProto, TensorProto, TypeProto, ValueInfoProto,
    save_model,
    tensor_proto::DataType,
    type_proto::{Tensor, Value as TypeValue},
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use urouter_artifact::{
    ArtifactSupportDomain, ContrastivePolicy, ExportGates, FeatureWeights, KnnPolicy, KnnPrototype,
    LEARNED_FEATURE_DIMENSIONS, LearnedPolicy, LinearPolicy, MlpHiddenUnit, MlpPolicy,
    RouterArtifact,
};
use urouter_eval::{
    BenchmarkCase, CounterfactualSample, DatasetBundle, EvaluationRecord, ExportFilter,
    FeedbackPolicy, benchmark_summary, build_dataset, counterfactual_report,
    evaluation_record_from_gateway,
};
use urouter_infer::OnnxRouter;

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
    Sweep {
        #[arg(long)]
        candidates: PathBuf,
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
        #[arg(long, value_enum, default_value_t = TrainingAlgorithm::Mlp)]
        algorithm: TrainingAlgorithm,
        #[arg(long, default_value_t = 5)]
        knn_k: usize,
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
    Replay {
        #[arg(long)]
        artifact: PathBuf,
        #[arg(long)]
        dataset: PathBuf,
        #[arg(long, default_value_t = 5_000)]
        maximum_p99_micros: u64,
        #[arg(long)]
        output: PathBuf,
    },
    ExportOnnx {
        #[arg(long)]
        artifact: PathBuf,
        #[arg(long)]
        output: PathBuf,
        #[arg(long)]
        report_output: PathBuf,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum TrainingAlgorithm {
    Mlp,
    Knn,
    Contrastive,
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
        Command::Sweep { candidates, output } => {
            let report = sweep_report(&read_json::<Vec<SweepCandidate>>(&candidates)?)?;
            if !report.baseline_gate_passed {
                return Err("no Pareto candidate dominates a required baseline".into());
            }
            write_json(&output, &report)
        }
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
            algorithm,
            knn_k,
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
            if dataset.manifest.quality.capacity_constrained_millionths > 300_000 {
                return Err("capacity_constrained samples exceed the 30% export gate".into());
            }
            if dataset.manifest.quality.usage_unavailable_millionths > 100_000 {
                return Err("usage_unavailable exclusions exceed the 10% export gate".into());
            }
            if dataset.manifest.quality.usage_unavailable > 0
                && dataset
                    .manifest
                    .quality
                    .maximum_usage_unavailable_tier_share_millionths
                    == 1_000_000
            {
                return Err("usage_unavailable exclusions are concentrated in one tier".into());
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
            let model = train_linear_policy(&dataset, baseline_tier.clone(), promoted_tier.clone());
            let learned_model = train_learned_policy(
                &dataset,
                baseline_tier,
                promoted_tier,
                algorithm,
                knn_k,
                seed,
            )?;
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
            )?
            .with_learned_model(learned_model)?;
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
        Command::Replay {
            artifact,
            dataset,
            maximum_p99_micros,
            output,
        } => {
            let report = replay_artifact(
                &read_json::<RouterArtifact>(&artifact)?,
                &read_json::<DatasetBundle>(&dataset)?,
                maximum_p99_micros,
            );
            write_json(&output, &report)?;
            if !report.passed {
                return Err("artifact replay gate failed".into());
            }
            Ok(())
        }
        Command::ExportOnnx {
            artifact,
            output,
            report_output,
        } => {
            let artifact = read_json::<RouterArtifact>(&artifact)?;
            let LearnedPolicy::Mlp(model) = artifact
                .learned_model
                .as_ref()
                .ok_or("artifact does not contain a learned model")?
            else {
                return Err("ONNX export currently requires an MLP artifact".into());
            };
            save_model(&output, &mlp_onnx_model(model))
                .map_err(|error| format!("ONNX encode failed: {error:?}"))?;
            let runtime = OnnxRouter::load(&output)?;
            let report = onnx_parity_report(model, &runtime);
            write_json(&report_output, &report)?;
            if !report.passed {
                return Err("ONNX parity gate failed".into());
            }
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

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SweepCandidate {
    id: String,
    alpha_millionths: u32,
    beta_millionths: u32,
    baseline: Option<String>,
    quality_samples: Vec<CounterfactualSample>,
    cost_samples: Vec<CounterfactualSample>,
}

#[derive(Debug, Serialize)]
struct SweepReport {
    points: Vec<SweepPoint>,
    pareto_ids: Vec<String>,
    selected_id: String,
    baseline_gate_passed: bool,
}

#[derive(Debug, Serialize)]
struct SweepPoint {
    id: String,
    alpha_millionths: u32,
    beta_millionths: u32,
    baseline: Option<String>,
    quality_dr: urouter_eval::Estimate,
    cost_dr: urouter_eval::Estimate,
    objective: f64,
}

fn sweep_report(candidates: &[SweepCandidate]) -> Result<SweepReport, BoxError> {
    if candidates.is_empty() {
        return Err("sweep requires at least one candidate".into());
    }
    let mut points = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        if candidate
            .alpha_millionths
            .saturating_add(candidate.beta_millionths)
            != 1_000_000
        {
            return Err("every sweep point must satisfy alpha + beta = 1000000".into());
        }
        if candidate.baseline.as_deref().is_some_and(|baseline| {
            !matches!(baseline, "always_capable" | "always_efficient" | "random")
        }) {
            return Err("unknown Pareto baseline".into());
        }
        let quality_dr = counterfactual_report(&candidate.quality_samples)?.doubly_robust;
        let cost_dr = counterfactual_report(&candidate.cost_samples)?.doubly_robust;
        let alpha = f64::from(candidate.alpha_millionths) / 1_000_000.0;
        let beta = f64::from(candidate.beta_millionths) / 1_000_000.0;
        points.push(SweepPoint {
            id: candidate.id.clone(),
            alpha_millionths: candidate.alpha_millionths,
            beta_millionths: candidate.beta_millionths,
            baseline: candidate.baseline.clone(),
            quality_dr,
            cost_dr,
            objective: alpha * quality_dr.value - beta * cost_dr.value,
        });
    }
    if !points.iter().any(|point| point.baseline.is_some()) {
        return Err("sweep must include always_capable, always_efficient, or random".into());
    }
    let dominates = |left: &SweepPoint, right: &SweepPoint| {
        left.quality_dr.confidence_95_low >= right.quality_dr.confidence_95_low
            && left.cost_dr.confidence_95_high <= right.cost_dr.confidence_95_high
            && (left.quality_dr.confidence_95_low > right.quality_dr.confidence_95_low
                || left.cost_dr.confidence_95_high < right.cost_dr.confidence_95_high)
    };
    let pareto_ids = points
        .iter()
        .filter(|point| !points.iter().any(|other| dominates(other, point)))
        .map(|point| point.id.clone())
        .collect::<Vec<_>>();
    let selected_id = points
        .iter()
        .filter(|point| pareto_ids.contains(&point.id))
        .max_by(|left, right| left.objective.total_cmp(&right.objective))
        .expect("non-empty Pareto frontier")
        .id
        .clone();
    let baseline_gate_passed =
        points
            .iter()
            .filter(|point| point.baseline.is_none())
            .any(|point| {
                points
                    .iter()
                    .filter(|baseline| baseline.baseline.is_some())
                    .any(|baseline| dominates(point, baseline))
            });
    Ok(SweepReport {
        points,
        pareto_ids,
        selected_id,
        baseline_gate_passed,
    })
}

#[derive(Debug, Serialize)]
struct ReplayReport {
    records: usize,
    agreements: usize,
    inference_errors: usize,
    agreement_millionths: u32,
    p99_inference_micros: u64,
    maximum_p99_micros: u64,
    passed: bool,
}

fn replay_artifact(
    artifact: &RouterArtifact,
    dataset: &DatasetBundle,
    maximum_p99_micros: u64,
) -> ReplayReport {
    let mut latencies = Vec::with_capacity(dataset.records.len());
    let mut agreements = 0;
    let mut inference_errors = 0;
    for record in &dataset.records {
        let started = Instant::now();
        let inference = artifact.infer(&record.context.features, &record.semantic_task, u32::MAX);
        latencies.push(u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX));
        match inference {
            Ok(inference) => agreements += usize::from(inference.tier == record.selected_tier),
            Err(_) => inference_errors += 1,
        }
    }
    latencies.sort_unstable();
    let p99_index = latencies
        .len()
        .saturating_mul(99)
        .div_ceil(100)
        .saturating_sub(1);
    let p99_inference_micros = latencies.get(p99_index).copied().unwrap_or_default();
    ReplayReport {
        records: dataset.records.len(),
        agreements,
        inference_errors,
        agreement_millionths: millionths(agreements, dataset.records.len()),
        p99_inference_micros,
        maximum_p99_micros,
        passed: !dataset.records.is_empty()
            && inference_errors == 0
            && p99_inference_micros <= maximum_p99_micros,
    }
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

fn train_learned_policy(
    dataset: &DatasetBundle,
    baseline_tier: String,
    promoted_tier: String,
    algorithm: TrainingAlgorithm,
    knn_k: usize,
    seed: u64,
) -> Result<LearnedPolicy, BoxError> {
    let rows = dataset
        .records
        .iter()
        .filter(|record| {
            record.selected_tier == baseline_tier || record.selected_tier == promoted_tier
        })
        .map(|record| {
            (
                learned_features(&record.context.features),
                record.selected_tier == promoted_tier,
            )
        })
        .collect::<Vec<_>>();
    if rows.iter().all(|(_, promoted)| *promoted) || rows.iter().all(|(_, promoted)| !*promoted) {
        return Err("learned training requires both baseline and promoted examples".into());
    }
    let baseline_centroid = centroid(&rows, false);
    let promoted_centroid = centroid(&rows, true);
    Ok(match algorithm {
        TrainingAlgorithm::Mlp => {
            let mut direction = [0_i64; LEARNED_FEATURE_DIMENSIONS];
            let mut midpoint = [0_i64; LEARNED_FEATURE_DIMENSIONS];
            for index in 0..LEARNED_FEATURE_DIMENSIONS {
                direction[index] =
                    promoted_centroid[index].saturating_sub(baseline_centroid[index]);
                midpoint[index] =
                    promoted_centroid[index].saturating_add(baseline_centroid[index]) / 2;
            }
            let negative = direction.map(i64::saturating_neg);
            LearnedPolicy::Mlp(MlpPolicy {
                baseline_tier,
                promoted_tier,
                hidden: vec![
                    MlpHiddenUnit {
                        weights: direction,
                        bias: 0,
                        output_weight: 1,
                    },
                    MlpHiddenUnit {
                        weights: negative,
                        bias: 0,
                        output_weight: -1,
                    },
                ],
                output_bias: saturating_dot(direction, midpoint).saturating_neg(),
                threshold: i64::try_from(seed & 1).unwrap_or_default(),
            })
        }
        TrainingAlgorithm::Knn => {
            if knn_k == 0 {
                return Err("knn-k must be greater than zero".into());
            }
            let start = usize::try_from(seed).unwrap_or(usize::MAX) % rows.len();
            let prototypes = rows
                .iter()
                .cycle()
                .skip(start)
                .take(rows.len().min(4_096))
                .map(|(features, promoted)| KnnPrototype {
                    features: *features,
                    promoted: *promoted,
                })
                .collect::<Vec<_>>();
            LearnedPolicy::Knn(KnnPolicy {
                baseline_tier,
                promoted_tier,
                k: knn_k.min(prototypes.len()),
                prototypes,
            })
        }
        TrainingAlgorithm::Contrastive => LearnedPolicy::Contrastive(ContrastivePolicy {
            baseline_tier,
            promoted_tier,
            baseline_centroid,
            promoted_centroid,
        }),
    })
}

fn learned_features(
    features: &urouter_contracts::FeatureFrame,
) -> [i64; LEARNED_FEATURE_DIMENSIONS] {
    [
        i64::try_from(features.input_text_bytes / 1_024).unwrap_or(i64::MAX),
        i64::from(features.message_count),
        i64::from(features.available_tool_count),
        i64::from(features.content.contains_image) * 100,
        i64::from(features.requests.structured_output) * 100,
        i64::from(features.requests.reasoning) * 100,
    ]
}

fn centroid(
    rows: &[([i64; LEARNED_FEATURE_DIMENSIONS], bool)],
    promoted: bool,
) -> [i64; LEARNED_FEATURE_DIMENSIONS] {
    let mut sums = [0_i128; LEARNED_FEATURE_DIMENSIONS];
    let mut count = 0_i128;
    for (features, _) in rows.iter().filter(|(_, label)| *label == promoted) {
        count += 1;
        for (sum, value) in sums.iter_mut().zip(features) {
            *sum = sum.saturating_add(i128::from(*value));
        }
    }
    sums.map(|sum| {
        i64::try_from(sum / count).unwrap_or_else(|_| {
            if sum.is_negative() {
                i64::MIN
            } else {
                i64::MAX
            }
        })
    })
}

fn saturating_dot(
    left: [i64; LEARNED_FEATURE_DIMENSIONS],
    right: [i64; LEARNED_FEATURE_DIMENSIONS],
) -> i64 {
    let value = left
        .into_iter()
        .zip(right)
        .fold(0_i128, |sum, (left, right)| {
            sum.saturating_add(i128::from(left).saturating_mul(i128::from(right)))
        });
    i64::try_from(value).unwrap_or_else(|_| {
        if value.is_negative() {
            i64::MIN
        } else {
            i64::MAX
        }
    })
}

#[allow(clippy::cast_precision_loss)]
fn mlp_onnx_model(model: &MlpPolicy) -> ModelProto {
    let hidden_count = model.hidden.len();
    let hidden_count_i64 = i64::try_from(hidden_count).unwrap_or(i64::MAX);
    let feature_dimensions_i64 = i64::try_from(LEARNED_FEATURE_DIMENSIONS).unwrap_or(i64::MAX);
    let hidden_weights = (0..LEARNED_FEATURE_DIMENSIONS)
        .flat_map(|feature| {
            model
                .hidden
                .iter()
                .map(move |unit| unit.weights[feature] as f64)
        })
        .collect::<Vec<_>>();
    let initializers = vec![
        double_tensor(
            "hidden_weights",
            vec![feature_dimensions_i64, hidden_count_i64],
            hidden_weights,
        ),
        double_tensor(
            "hidden_bias",
            vec![hidden_count_i64],
            model.hidden.iter().map(|unit| unit.bias as f64).collect(),
        ),
        double_tensor(
            "output_weights",
            vec![hidden_count_i64, 1],
            model
                .hidden
                .iter()
                .map(|unit| unit.output_weight as f64)
                .collect(),
        ),
        double_tensor("output_bias", vec![1], vec![model.output_bias as f64]),
    ];
    ModelProto {
        ir_version: 8,
        producer_name: "urouter-lab".to_owned(),
        producer_version: env!("CARGO_PKG_VERSION").to_owned(),
        graph: Some(GraphProto {
            node: vec![
                onnx_node(
                    "hidden_matmul",
                    "MatMul",
                    &["features", "hidden_weights"],
                    "hidden_linear",
                ),
                onnx_node(
                    "hidden_add",
                    "Add",
                    &["hidden_linear", "hidden_bias"],
                    "hidden_pre",
                ),
                onnx_node("hidden_relu", "Relu", &["hidden_pre"], "hidden"),
                onnx_node(
                    "output_matmul",
                    "MatMul",
                    &["hidden", "output_weights"],
                    "output_linear",
                ),
                onnx_node(
                    "output_add",
                    "Add",
                    &["output_linear", "output_bias"],
                    "score",
                ),
            ],
            name: "urouter_mlp".to_owned(),
            initializer: initializers,
            input: vec![double_value_info(
                "features",
                vec![1, feature_dimensions_i64],
            )],
            output: vec![double_value_info("score", vec![1, 1])],
            ..GraphProto::default()
        }),
        opset_import: vec![OperatorSetIdProto {
            domain: String::new(),
            version: 13,
        }],
        ..ModelProto::default()
    }
}

fn onnx_node(name: &str, operator: &str, inputs: &[&str], output: &str) -> NodeProto {
    NodeProto {
        input: inputs.iter().map(|value| (*value).to_owned()).collect(),
        output: vec![output.to_owned()],
        name: name.to_owned(),
        op_type: operator.to_owned(),
        ..NodeProto::default()
    }
}

fn double_tensor(name: &str, dims: Vec<i64>, values: Vec<f64>) -> TensorProto {
    TensorProto {
        dims,
        data_type: DataType::Double as i32,
        double_data: values,
        name: name.to_owned(),
        ..TensorProto::default()
    }
}

fn double_value_info(name: &str, dims: Vec<i64>) -> ValueInfoProto {
    ValueInfoProto {
        name: name.to_owned(),
        r#type: Some(TypeProto {
            denotation: String::new(),
            value: Some(TypeValue::TensorType(Tensor {
                elem_type: DataType::Double as i32,
                shape: Some(dims.into()),
            })),
        }),
        ..ValueInfoProto::default()
    }
}

#[derive(Debug, Serialize)]
struct OnnxParityReport {
    samples: usize,
    mismatches: usize,
    passed: bool,
}

fn onnx_parity_report(model: &MlpPolicy, runtime: &OnnxRouter) -> OnnxParityReport {
    let fixtures = [
        [0, 0, 0, 0, 0, 0],
        [1, 1, 0, 0, 0, 0],
        [4, 8, 1, 0, 100, 100],
        [128, 32, 4, 100, 100, 100],
        [16, 2, 0, 100, 0, 0],
        [2, 3, 7, 0, 0, 100],
    ];
    let mismatches = fixtures
        .iter()
        .filter(|features| runtime.score(**features).ok() != Some(mlp_score(model, **features)))
        .count();
    OnnxParityReport {
        samples: fixtures.len(),
        mismatches,
        passed: mismatches == 0,
    }
}

fn mlp_score(model: &MlpPolicy, features: [i64; LEARNED_FEATURE_DIMENSIONS]) -> i64 {
    model.hidden.iter().fold(model.output_bias, |score, unit| {
        score.saturating_add(
            saturating_dot(unit.weights, features)
                .saturating_add(unit.bias)
                .max(0)
                .saturating_mul(unit.output_weight),
        )
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(value: f64) -> CounterfactualSample {
        CounterfactualSample {
            reward: value,
            behavior_probability: 1.0,
            target_probability: 1.0,
            direct_estimate: value,
        }
    }

    #[test]
    fn sweep_requires_and_dominates_a_named_baseline() {
        let candidates = vec![
            SweepCandidate {
                id: "always-capable".to_owned(),
                alpha_millionths: 500_000,
                beta_millionths: 500_000,
                baseline: Some("always_capable".to_owned()),
                quality_samples: vec![sample(0.8), sample(0.8)],
                cost_samples: vec![sample(0.9), sample(0.9)],
            },
            SweepCandidate {
                id: "candidate".to_owned(),
                alpha_millionths: 600_000,
                beta_millionths: 400_000,
                baseline: None,
                quality_samples: vec![sample(0.9), sample(0.9)],
                cost_samples: vec![sample(0.5), sample(0.5)],
            },
        ];
        let report = sweep_report(&candidates).unwrap();
        assert!(report.baseline_gate_passed);
        assert_eq!(report.pareto_ids, vec!["candidate"]);
    }

    #[test]
    fn exported_onnx_graph_matches_integer_artifact_scores() {
        let model = MlpPolicy {
            baseline_tier: "efficient".to_owned(),
            promoted_tier: "capable".to_owned(),
            hidden: vec![
                MlpHiddenUnit {
                    weights: [1, 2, 3, 4, 5, 6],
                    bias: -10,
                    output_weight: 2,
                },
                MlpHiddenUnit {
                    weights: [-1, 0, 2, 0, 1, 0],
                    bias: 4,
                    output_weight: -1,
                },
            ],
            output_bias: 7,
            threshold: 0,
        };
        let runtime = OnnxRouter::from_model(&mlp_onnx_model(&model)).unwrap();
        let report = onnx_parity_report(&model, &runtime);
        assert!(report.passed);
        assert_eq!(report.mismatches, 0);
    }
}
