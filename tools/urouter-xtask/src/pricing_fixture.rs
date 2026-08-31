//! Guards the size of the golden pricing vector fixture.
//!
//! The exact-cost kernel is only as trustworthy as the vectors that pin it, so
//! the fixture is not allowed to shrink below the reviewed floor.

use std::path::Path;

use serde_json::Value;

use crate::BoxError;

const FIXTURE_PATH: &str = "catalog/fixtures/pricing-golden.json";
const MINIMUM_VECTORS: usize = 30;

pub fn run(root: &Path) -> Result<bool, BoxError> {
    let contents = std::fs::read_to_string(root.join(FIXTURE_PATH))?;
    let count = vector_count(&contents)?;
    if count < MINIMUM_VECTORS {
        eprintln!("pricing fixture must contain at least {MINIMUM_VECTORS} vectors, found {count}");
        return Ok(false);
    }
    println!("Pricing fixture holds {count} golden vector(s).");
    Ok(true)
}

fn vector_count(contents: &str) -> Result<usize, BoxError> {
    match serde_json::from_str::<Value>(contents)? {
        Value::Array(vectors) => Ok(vectors.len()),
        _ => Err(format!("{FIXTURE_PATH} must be a JSON array").into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_the_vectors_in_an_array() {
        assert_eq!(vector_count("[{\"a\":1},{\"b\":2}]").unwrap(), 2);
    }

    #[test]
    fn rejects_a_non_array_fixture() {
        assert!(vector_count("{\"vectors\": []}").is_err());
    }

    #[test]
    fn the_real_fixture_meets_the_floor() {
        let root = crate::repository_root().unwrap();
        let contents = std::fs::read_to_string(root.join(FIXTURE_PATH)).unwrap();
        assert!(vector_count(&contents).unwrap() >= MINIMUM_VECTORS);
    }
}
