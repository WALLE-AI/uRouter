//! Lightweight credential scanner over the files git knows about.
//!
//! This is deliberately a shape scanner, not an entropy scanner: it reproduces
//! the four patterns the original PowerShell gate enforced. Binary and non-UTF-8
//! files are skipped, exactly as before.

use std::path::Path;
use std::process::Command;

use crate::BoxError;

/// Split so that no single line of this file is itself a private-key header.
const PRIVATE_KEY_HEAD: &str = "-----BEGIN ";
const PRIVATE_KEY_TAIL: &str = "PRIVATE KEY-----";
const PRIVATE_KEY_KINDS: [&str; 4] = ["", "RSA ", "EC ", "OPENSSH "];

const GITHUB_TOKEN_KINDS: [&str; 5] = ["ghp_", "gho_", "ghu_", "ghs_", "ghr_"];

pub fn run(root: &Path) -> Result<bool, BoxError> {
    let files = tracked_files(root)?;
    let mut findings = Vec::new();
    for relative in &files {
        let full = root.join(relative);
        if !full.is_file() {
            continue;
        }
        // Binary and non-UTF-8 tracked files are outside this scanner.
        let Ok(contents) = std::fs::read_to_string(&full) else {
            continue;
        };
        for (index, line) in contents.lines().enumerate() {
            if line_has_secret(line) {
                findings.push(format!("{relative}:{}", index + 1));
                break;
            }
        }
    }

    if findings.is_empty() {
        println!("Secret scan passed for {} repository files.", files.len());
        return Ok(true);
    }
    eprintln!("Potential secrets found at:");
    for finding in &findings {
        eprintln!("{finding}");
    }
    Ok(false)
}

fn tracked_files(root: &Path) -> Result<Vec<String>, BoxError> {
    let output = Command::new("git")
        .current_dir(root)
        .args([
            "-c",
            "core.quotePath=false",
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
        ])
        .output()?;
    if !output.status.success() {
        return Err("git ls-files failed".into());
    }
    Ok(String::from_utf8(output.stdout)?
        .lines()
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect())
}

fn line_has_secret(line: &str) -> bool {
    has_openai_key(line)
        || has_github_token(line)
        || has_aws_access_key_id(line)
        || has_private_key_header(line)
}

/// `sk-` followed by at least 20 alphanumerics, not preceded by an alphanumeric.
fn has_openai_key(line: &str) -> bool {
    matches_prefixed_token(line, "sk-", 20, is_alphanumeric, true)
}

fn has_github_token(line: &str) -> bool {
    GITHUB_TOKEN_KINDS
        .iter()
        .any(|prefix| matches_prefixed_token(line, prefix, 30, is_alphanumeric, true))
}

fn has_aws_access_key_id(line: &str) -> bool {
    matches_prefixed_token(line, "AKIA", 16, is_upper_alphanumeric, false)
}

fn has_private_key_header(line: &str) -> bool {
    let mut search = line;
    while let Some(offset) = search.find(PRIVATE_KEY_HEAD) {
        let rest = &search[offset + PRIVATE_KEY_HEAD.len()..];
        if PRIVATE_KEY_KINDS.iter().any(|kind| {
            rest.strip_prefix(kind)
                .is_some_and(|tail| tail.starts_with(PRIVATE_KEY_TAIL))
        }) {
            return true;
        }
        search = &search[offset + PRIVATE_KEY_HEAD.len()..];
    }
    false
}

/// Returns true when `line` contains `prefix` followed by at least `minimum`
/// characters accepted by `accept`. When `boundary` is set, the character before
/// the prefix must not be alphanumeric, reproducing the original lookbehind.
fn matches_prefixed_token(
    line: &str,
    prefix: &str,
    minimum: usize,
    accept: fn(char) -> bool,
    boundary: bool,
) -> bool {
    let bytes = line.as_bytes();
    let mut from = 0;
    while let Some(offset) = line[from..].find(prefix) {
        let start = from + offset;
        let preceded_by_alphanumeric = start > 0
            && bytes[..start]
                .iter()
                .next_back()
                .is_some_and(u8::is_ascii_alphanumeric);
        if !(boundary && preceded_by_alphanumeric) {
            let run = line[start + prefix.len()..]
                .chars()
                .take_while(|character| accept(*character))
                .count();
            if run >= minimum {
                return true;
            }
        }
        from = start + prefix.len();
    }
    false
}

fn is_alphanumeric(character: char) -> bool {
    character.is_ascii_alphanumeric()
}

fn is_upper_alphanumeric(character: char) -> bool {
    character.is_ascii_digit() || character.is_ascii_uppercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test fixtures are assembled at runtime so that this file never contains a
    /// literal the scanner would flag when it scans its own source.
    fn joined(parts: &[&str]) -> String {
        parts.concat()
    }

    #[test]
    fn detects_an_openai_style_key() {
        let line = joined(&["api_key = \"sk-", &"a".repeat(24), "\""]);
        assert!(line_has_secret(&line));
    }

    #[test]
    fn ignores_a_short_or_embedded_sk_prefix() {
        assert!(!line_has_secret(&joined(&["sk-", &"a".repeat(19)])));
        assert!(!line_has_secret(&joined(&["risk-", &"a".repeat(24)])));
    }

    #[test]
    fn detects_every_github_token_kind() {
        for prefix in GITHUB_TOKEN_KINDS {
            assert!(
                line_has_secret(&joined(&[prefix, &"b".repeat(32)])),
                "{prefix} was not detected"
            );
        }
        assert!(!line_has_secret(&joined(&["ghp_", &"b".repeat(29)])));
    }

    #[test]
    fn detects_an_aws_access_key_id() {
        assert!(line_has_secret(&joined(&["AKIA", &"C".repeat(16)])));
        assert!(!line_has_secret(&joined(&["AKIA", &"C".repeat(15)])));
        assert!(!line_has_secret(&joined(&["AKIA", &"c".repeat(16)])));
    }

    #[test]
    fn detects_every_private_key_header_kind() {
        for kind in PRIVATE_KEY_KINDS {
            let line = joined(&[PRIVATE_KEY_HEAD, kind, PRIVATE_KEY_TAIL]);
            assert!(line_has_secret(&line), "{kind:?} was not detected");
        }
    }

    #[test]
    fn ignores_a_private_key_header_with_an_unknown_kind() {
        let line = joined(&[PRIVATE_KEY_HEAD, "CERTIFICATE ", PRIVATE_KEY_TAIL]);
        assert!(!line_has_secret(&line));
    }

    #[test]
    fn ignores_ordinary_source_lines() {
        assert!(!line_has_secret(
            "let client = reqwest::Client::builder().no_proxy();"
        ));
        assert!(!line_has_secret("# uRouter"));
        assert!(!line_has_secret(""));
    }
}
