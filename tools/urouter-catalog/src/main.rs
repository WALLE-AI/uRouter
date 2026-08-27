use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::PathBuf,
    process::ExitCode,
};

use clap::{Parser, Subcommand};
use serde::Serialize;
use urouter_ai::{
    admission::eligible_models,
    capabilities::{CapabilityRequirement, Modality},
    catalog::{CatalogLoadError, CatalogManifest, CatalogSnapshot},
    pricing::calculate_actual_cost,
};
use urouter_types::{CatalogHash, ModelId, Usage};

#[derive(Debug, Parser)]
#[command(
    name = "urouter-catalog",
    about = "Validate and inspect uRouter catalogs"
)]
struct Args {
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Serialize)]
struct CheckOutput {
    valid: bool,
    providers: usize,
    models: usize,
    content_hash: CatalogHash,
}

#[derive(Debug, Serialize)]
struct HashChange {
    old: CatalogHash,
    new: CatalogHash,
}

#[derive(Debug, Serialize)]
struct DiffHashes {
    content: HashChange,
    pricing: HashChange,
    capabilities: HashChange,
    compat: HashChange,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
enum ChangeCategory {
    Pricing,
    Capabilities,
    Compat,
    Lifecycle,
    Facts,
}

impl ChangeCategory {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Pricing => "pricing",
            Self::Capabilities => "capabilities",
            Self::Compat => "compat",
            Self::Lifecycle => "lifecycle",
            Self::Facts => "facts",
        }
    }
}

#[derive(Debug, Serialize)]
struct ModelChange {
    id: ModelId,
    categories: BTreeSet<ChangeCategory>,
}

#[derive(Debug, Serialize)]
struct DiffOutput {
    hashes: DiffHashes,
    providers_changed: bool,
    added: Vec<ModelId>,
    removed: Vec<ModelId>,
    changed: Vec<ModelChange>,
}

#[derive(Debug, Subcommand)]
enum Command {
    Check {
        path: PathBuf,
    },
    Show {
        path: PathBuf,
        model: String,
    },
    List {
        path: PathBuf,
        #[arg(long)]
        provider: Option<String>,
        #[arg(long)]
        capability: Option<String>,
    },
    Cost {
        path: PathBuf,
        model: String,
        #[arg(long)]
        usage: PathBuf,
    },
    Diff {
        old: PathBuf,
        new: PathBuf,
    },
    Manifest {
        path: PathBuf,
    },
}

fn main() -> ExitCode {
    match run(Args::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: Args) -> Result<(), Box<dyn std::error::Error>> {
    let json = args.json;
    match args.command {
        Command::Check { path } => {
            let input = fs::read(&path)?;
            let catalog = load_bytes(&input, json)?;
            let manifest_path = path.with_file_name("manifest.json");
            if manifest_path.exists() {
                let manifest: CatalogManifest = serde_json::from_slice(&fs::read(&manifest_path)?)?;
                if !manifest.matches(&input, &catalog) {
                    return Err(format!("manifest does not match {}", path.display()).into());
                }
            }
            if json {
                println!(
                    "{}",
                    serde_json::to_string(&CheckOutput {
                        valid: true,
                        providers: catalog.providers().count(),
                        models: catalog.models().count(),
                        content_hash: catalog.hashes().content.clone(),
                    })?
                );
            } else {
                println!(
                    "valid: {} provider(s), {} model(s), {}",
                    catalog.providers().count(),
                    catalog.models().count(),
                    catalog.hashes().content
                );
            }
        }
        Command::Show { path, model } => {
            let catalog = load(&path, json)?;
            let id = ModelId::new(model)?;
            let model = catalog.model(&id).ok_or("model not found")?;
            println!("{}", serde_json::to_string_pretty(model)?);
        }
        Command::List {
            path,
            provider,
            capability,
        } => {
            let catalog = load(&path, json)?;
            let requirement = capability
                .as_deref()
                .map(parse_capability)
                .transpose()?
                .unwrap_or_default();
            let admitted = eligible_models(&catalog, &requirement);
            let models = admitted
                .eligible
                .into_iter()
                .filter(|id| {
                    let model = catalog.model(id).expect("admission returned a known model");
                    provider
                        .as_deref()
                        .is_none_or(|expected| model.provider.as_str() == expected)
                })
                .collect::<Vec<_>>();
            if json {
                println!("{}", serde_json::to_string(&models)?);
            } else {
                for id in models {
                    println!("{id}");
                }
            }
        }
        Command::Cost { path, model, usage } => {
            let catalog = load(&path, json)?;
            let id = ModelId::new(model)?;
            let model = catalog.model(&id).ok_or("model not found")?;
            let usage: Usage = serde_json::from_str(&fs::read_to_string(usage)?)?;
            let cost = calculate_actual_cost(&model.cost, usage)?;
            println!("{}", serde_json::to_string_pretty(&cost)?);
        }
        Command::Diff { old, new } => {
            let old = load(&old, json)?;
            let new = load(&new, json)?;
            let report = diff_catalogs(&old, &new);
            if json {
                println!("{}", serde_json::to_string(&report)?);
            } else {
                print_diff(&report);
            }
        }
        Command::Manifest { path } => {
            let input = fs::read(&path)?;
            let catalog = load_bytes(&input, json)?;
            let manifest = CatalogManifest::from_source(&input, &catalog);
            println!("{}", serde_json::to_string_pretty(&manifest)?);
        }
    }
    Ok(())
}

fn load(path: &PathBuf, json: bool) -> Result<CatalogSnapshot, Box<dyn std::error::Error>> {
    load_bytes(&fs::read(path)?, json)
}

fn load_bytes(input: &[u8], json: bool) -> Result<CatalogSnapshot, Box<dyn std::error::Error>> {
    match CatalogSnapshot::from_json_str(std::str::from_utf8(input)?) {
        Ok(catalog) => Ok(catalog),
        Err(CatalogLoadError::Validation(report)) => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({ "valid": false, "issues": report.issues })
                );
            } else {
                for issue in &report.issues {
                    eprintln!("{} {}: {}", issue.code, issue.path, issue.message);
                }
            }
            Err(Box::new(report))
        }
        Err(error) => Err(Box::new(error)),
    }
}

fn diff_catalogs(old: &CatalogSnapshot, new: &CatalogSnapshot) -> DiffOutput {
    let old_models = old
        .models()
        .map(|model| (&model.id, model))
        .collect::<BTreeMap<_, _>>();
    let new_models = new
        .models()
        .map(|model| (&model.id, model))
        .collect::<BTreeMap<_, _>>();
    let old_ids = old_models.keys().copied().collect::<BTreeSet<_>>();
    let new_ids = new_models.keys().copied().collect::<BTreeSet<_>>();
    let added = new_ids
        .difference(&old_ids)
        .map(|id| (*id).clone())
        .collect();
    let removed = old_ids
        .difference(&new_ids)
        .map(|id| (*id).clone())
        .collect();
    let changed = old_ids
        .intersection(&new_ids)
        .filter_map(|id| {
            let old = old_models[id];
            let new = new_models[id];
            let pricing = old.cost != new.cost;
            let capabilities = old.capabilities != new.capabilities;
            let compat = old.compat != new.compat;
            let lifecycle = old.lifecycle != new.lifecycle;
            let facts = old.upstream_id != new.upstream_id
                || old.name != new.name
                || old.provider != new.provider
                || old.api != new.api
                || old.base_url != new.base_url
                || old.headers != new.headers
                || old.aliases != new.aliases
                || old.source != new.source;
            let categories = [
                (ChangeCategory::Pricing, pricing),
                (ChangeCategory::Capabilities, capabilities),
                (ChangeCategory::Compat, compat),
                (ChangeCategory::Lifecycle, lifecycle),
                (ChangeCategory::Facts, facts),
            ]
            .into_iter()
            .filter_map(|(category, changed)| changed.then_some(category))
            .collect::<BTreeSet<_>>();
            (!categories.is_empty()).then(|| ModelChange {
                id: (*id).clone(),
                categories,
            })
        })
        .collect();

    DiffOutput {
        hashes: DiffHashes {
            content: HashChange {
                old: old.hashes().content.clone(),
                new: new.hashes().content.clone(),
            },
            pricing: HashChange {
                old: old.hashes().pricing.clone(),
                new: new.hashes().pricing.clone(),
            },
            capabilities: HashChange {
                old: old.hashes().capabilities.clone(),
                new: new.hashes().capabilities.clone(),
            },
            compat: HashChange {
                old: old.hashes().compat.clone(),
                new: new.hashes().compat.clone(),
            },
        },
        providers_changed: old.document().providers != new.document().providers,
        added,
        removed,
        changed,
    }
}

fn print_diff(report: &DiffOutput) {
    println!(
        "content: {} -> {}",
        report.hashes.content.old, report.hashes.content.new
    );
    println!(
        "pricing: {} -> {}",
        report.hashes.pricing.old, report.hashes.pricing.new
    );
    println!(
        "capabilities: {} -> {}",
        report.hashes.capabilities.old, report.hashes.capabilities.new
    );
    println!(
        "compat: {} -> {}",
        report.hashes.compat.old, report.hashes.compat.new
    );
    if report.providers_changed {
        println!("changed providers");
    }
    for id in &report.added {
        println!("added model: {id}");
    }
    for id in &report.removed {
        println!("removed model: {id}");
    }
    for change in &report.changed {
        let categories = change
            .categories
            .iter()
            .map(|category| category.as_str())
            .collect::<Vec<_>>()
            .join(",");
        println!("changed model: {} [{categories}]", change.id);
    }
}

fn parse_capability(value: &str) -> Result<CapabilityRequirement, String> {
    let mut requirement = CapabilityRequirement::default();
    match value {
        "text" => {
            requirement.input_modalities.insert(Modality::Text);
        }
        "image" | "vision" => {
            requirement.input_modalities.insert(Modality::Image);
        }
        "audio" => {
            requirement.input_modalities.insert(Modality::Audio);
        }
        "video" => {
            requirement.input_modalities.insert(Modality::Video);
        }
        "tools" | "tool_calling" => requirement.tool_calling = true,
        "structured_output" => requirement.structured_output = true,
        "reasoning" => requirement.reasoning = true,
        unknown => return Err(format!("unknown capability: {unknown}")),
    }
    Ok(requirement)
}

#[cfg(test)]
mod tests {
    use super::*;
    use urouter_ai::catalog::LifecycleStatus;

    #[test]
    fn diff_classifies_model_lifecycle_change() {
        let old =
            CatalogSnapshot::from_json_str(include_str!("../../../catalog/catalog.json")).unwrap();
        let mut document = old.document();
        document.models[0].lifecycle = LifecycleStatus::Deprecated;
        let new = CatalogSnapshot::from_document(document).unwrap();
        let report = diff_catalogs(&old, &new);
        assert_eq!(report.changed.len(), 1);
        assert_eq!(
            report.changed[0].categories,
            BTreeSet::from([ChangeCategory::Lifecycle])
        );
    }
}
