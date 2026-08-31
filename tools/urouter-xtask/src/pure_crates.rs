//! Dependency-boundary gate for the crates that must stay free of I/O.
//!
//! `urouter-contracts` is consumed by the Gateway, the offline evaluation
//! pipeline and the artifact kernel. If it ever gains a runtime, HTTP or Redis
//! dependency, those consumers stop being able to share it.

use std::process::Command;

use crate::BoxError;

const PURE_CRATES: [&str; 1] = ["urouter-contracts"];
const BANNED_PACKAGES: [&str; 6] = ["axum", "hyper", "redis", "reqwest", "tokio", "tower"];

pub fn run() -> Result<bool, BoxError> {
    let mut failed = false;
    for crate_name in PURE_CRATES {
        let packages = dependency_packages(crate_name)?;
        let violations: Vec<&str> = BANNED_PACKAGES
            .into_iter()
            .filter(|banned| packages.iter().any(|package| package == banned))
            .collect();
        if !violations.is_empty() {
            eprintln!(
                "{crate_name} depends on forbidden I/O packages: {}",
                violations.join(", ")
            );
            failed = true;
        }
    }
    if failed {
        return Ok(false);
    }
    println!(
        "Pure-crate dependency boundary passed for: {}.",
        PURE_CRATES.join(", ")
    );
    Ok(true)
}

fn dependency_packages(crate_name: &str) -> Result<Vec<String>, BoxError> {
    let output = Command::new(env!("CARGO"))
        .args(["tree", "-p", crate_name, "--prefix", "none"])
        .output()?;
    if !output.status.success() {
        return Err(format!("cargo tree failed for {crate_name}").into());
    }
    Ok(parse_package_names(&String::from_utf8(output.stdout)?))
}

fn parse_package_names(tree: &str) -> Vec<String> {
    let mut packages: Vec<String> = tree
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .map(str::to_owned)
        .collect();
    packages.sort_unstable();
    packages.dedup();
    packages
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_deduplicates_cargo_tree_output() {
        let tree = "urouter-contracts v0.1.0 (/repo/crates/urouter-contracts)\n\
                    serde v1.0.0\n\
                    serde_json v1.0.0\n\
                    serde v1.0.0 (*)\n\
                    \n";
        assert_eq!(
            parse_package_names(tree),
            vec![
                "serde".to_owned(),
                "serde_json".to_owned(),
                "urouter-contracts".to_owned(),
            ]
        );
    }

    #[test]
    fn banned_packages_are_recognised_in_a_tree() {
        let packages = parse_package_names("urouter-contracts v0.1.0\ntokio v1.52.0\n");
        assert!(
            BANNED_PACKAGES
                .into_iter()
                .any(|banned| packages.iter().any(|package| package == banned))
        );
    }

    #[test]
    fn the_real_pure_crates_hold_the_boundary() {
        for crate_name in PURE_CRATES {
            let packages = dependency_packages(crate_name).unwrap();
            for banned in BANNED_PACKAGES {
                assert!(
                    !packages.iter().any(|package| package == banned),
                    "{crate_name} must not depend on {banned}"
                );
            }
        }
    }
}
