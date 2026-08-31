//! Validates that shipped deployment manifests pass the Gateway's own dry-run.
//!
//! A manifest that names a flag the binary rejects fails at rollout, not at
//! review. `--dry-run` already encodes the argument rules — for example that
//! `--redis-url` requires `--on-state-unavailable fail_closed` — so the manifests
//! are checked against it rather than against a second copy of those rules.
//!
//! This is a configuration check, not a connectivity check: `--dry-run` performs
//! no upstream, Redis or listener activity.

use std::path::Path;
use std::process::Command;

use crate::BoxError;

const MANIFESTS: [&str; 2] = ["docker-compose.yml", "deploy/kubernetes/gateway.yaml"];

pub fn run(root: &Path) -> Result<bool, BoxError> {
    let mut checked = 0_usize;
    let mut failed = false;
    for manifest in MANIFESTS {
        let path = root.join(manifest);
        let contents =
            std::fs::read_to_string(&path).map_err(|error| format!("{manifest}: {error}"))?;
        let blocks = gateway_argument_blocks(&contents);
        if blocks.is_empty() {
            return Err(format!("{manifest}: no Gateway argument block found").into());
        }
        for arguments in blocks {
            checked += 1;
            if !dry_run_accepts(root, &arguments)? {
                eprintln!("{manifest}: Gateway rejected {arguments:?}");
                failed = true;
            }
        }
    }
    if failed {
        return Ok(false);
    }
    println!("Gateway dry-run accepted {checked} deployment argument set(s).");
    Ok(true)
}

fn dry_run_accepts(root: &Path, arguments: &[String]) -> Result<bool, BoxError> {
    let status = Command::new(env!("CARGO"))
        .current_dir(root)
        .args(["run", "-q", "-p", "urouter-gateway", "--"])
        .args(arguments)
        .arg("--dry-run")
        .stdout(std::process::Stdio::null())
        .status()?;
    Ok(status.success())
}

/// Extracts block-style `args:`/`command:` sequences that configure the Gateway.
///
/// Deliberately narrow: it understands the one YAML shape these manifests use —
/// a mapping key followed by more-indented `- "value"` items — and nothing else.
/// Flow-style values such as the Redis `command: ["redis-server", ...]` yield no
/// items and are skipped, which is what excludes non-Gateway containers.
fn gateway_argument_blocks(contents: &str) -> Vec<Vec<String>> {
    let mut blocks = Vec::new();
    let lines: Vec<&str> = contents.lines().collect();
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index];
        let Some(key_indent) = block_key_indent(line) else {
            index += 1;
            continue;
        };
        let mut items = Vec::new();
        let mut cursor = index + 1;
        while cursor < lines.len() {
            let candidate = lines[cursor];
            let trimmed = candidate.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                cursor += 1;
                continue;
            }
            let indent = candidate.len() - candidate.trim_start().len();
            if indent <= key_indent || !trimmed.starts_with("- ") {
                break;
            }
            items.push(unquote(trimmed[2..].trim()));
            cursor += 1;
        }
        // Only sequences that actually start with a Gateway flag are candidates;
        // this keeps unrelated block sequences out of the check.
        if items.first().is_some_and(|first| first.starts_with("--")) {
            blocks.push(items);
        }
        index = cursor.max(index + 1);
    }
    blocks
}

fn block_key_indent(line: &str) -> Option<usize> {
    let trimmed = line.trim();
    if trimmed != "args:" && trimmed != "command:" {
        return None;
    }
    Some(line.len() - line.trim_start().len())
}

fn unquote(value: &str) -> String {
    let bytes = value.as_bytes();
    if bytes.len() >= 2
        && ((value.starts_with('"') && value.ends_with('"'))
            || (value.starts_with('\'') && value.ends_with('\'')))
    {
        return value[1..value.len() - 1].to_owned();
    }
    value.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_a_block_style_argument_sequence() {
        let manifest = concat!(
            "    command:\n",
            "      - \"--bind\"\n",
            "      - \"0.0.0.0:8787\"\n",
            "    ports:\n",
            "      - \"8787:8787\"\n",
        );
        assert_eq!(
            gateway_argument_blocks(manifest),
            vec![vec!["--bind".to_owned(), "0.0.0.0:8787".to_owned()]]
        );
    }

    #[test]
    fn skips_comments_and_blank_lines_inside_a_block() {
        let manifest = concat!(
            "  args:\n",
            "    - \"--bind\"\n",
            "    # explanation\n",
            "\n",
            "    - \"0.0.0.0:8787\"\n",
        );
        assert_eq!(
            gateway_argument_blocks(manifest),
            vec![vec!["--bind".to_owned(), "0.0.0.0:8787".to_owned()]]
        );
    }

    #[test]
    fn ignores_flow_style_and_non_flag_sequences() {
        let manifest = concat!(
            "    command: [\"redis-server\", \"--appendonly\", \"yes\"]\n",
            "    command:\n",
            "      - \"redis-server\"\n",
            "      - \"--appendonly\"\n",
        );
        assert!(gateway_argument_blocks(manifest).is_empty());
    }

    #[test]
    fn extracts_every_block_in_a_multi_service_manifest() {
        let manifest = concat!(
            "  a:\n",
            "    command:\n",
            "      - \"--bind\"\n",
            "  b:\n",
            "    command:\n",
            "      - \"--redis-url\"\n",
            "      - \"redis://redis:6379/\"\n",
        );
        assert_eq!(gateway_argument_blocks(manifest).len(), 2);
    }

    #[test]
    fn unquotes_both_quote_styles_and_leaves_bare_values() {
        assert_eq!(unquote("\"--bind\""), "--bind");
        assert_eq!(unquote("'--bind'"), "--bind");
        assert_eq!(unquote("--bind"), "--bind");
    }

    /// The shipped manifests must each expose at least one Gateway argument set,
    /// so that a refactor cannot silently turn this gate into a no-op.
    #[test]
    fn the_real_manifests_expose_argument_blocks() {
        let root = crate::repository_root().unwrap();
        for manifest in MANIFESTS {
            let contents = std::fs::read_to_string(root.join(manifest)).unwrap();
            let blocks = gateway_argument_blocks(&contents);
            assert!(!blocks.is_empty(), "{manifest} exposed no argument block");
            for block in blocks {
                assert!(
                    block.iter().any(|argument| argument == "--bind"),
                    "{manifest} block is missing --bind: {block:?}"
                );
            }
        }
    }
}
