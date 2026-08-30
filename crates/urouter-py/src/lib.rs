use pyo3::{
    Bound, PyResult,
    exceptions::PyValueError,
    pyfunction, pymodule,
    types::{PyModule, PyModuleMethods},
    wrap_pyfunction,
};
use serde_json::Value;
use thiserror::Error;
use urouter_ai::pricing::{ModelCost, calculate_actual_cost};
use urouter_contracts::FeatureFrame;
use urouter_types::Usage;

pub fn feature_frame_json(request_json: &str) -> Result<String, BindingError> {
    let request = serde_json::from_str::<Value>(request_json)?;
    Ok(serde_json::to_string(&FeatureFrame::from_openai_chat(
        &request,
    ))?)
}

pub fn calculate_cost_json(
    model_cost_json: &str,
    usage_json: &str,
) -> Result<String, BindingError> {
    let cost = serde_json::from_str::<ModelCost>(model_cost_json)?;
    let usage = serde_json::from_str::<Usage>(usage_json)?;
    let breakdown = calculate_actual_cost(&cost, usage)?;
    Ok(serde_json::to_string(&breakdown)?)
}

#[pyfunction(name = "feature_frame")]
fn py_feature_frame(request_json: &str) -> PyResult<String> {
    feature_frame_json(request_json).map_err(|error| binding_value_error(&error))
}

#[pyfunction(name = "calculate_cost")]
fn py_calculate_cost(model_cost_json: &str, usage_json: &str) -> PyResult<String> {
    calculate_cost_json(model_cost_json, usage_json).map_err(|error| binding_value_error(&error))
}

#[pymodule]
fn urouter_py(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_function(wrap_pyfunction!(py_feature_frame, module)?)?;
    module.add_function(wrap_pyfunction!(py_calculate_cost, module)?)?;
    Ok(())
}

fn binding_value_error(error: &BindingError) -> pyo3::PyErr {
    PyValueError::new_err(error.to_string())
}

#[derive(Debug, Error)]
pub enum BindingError {
    #[error("invalid JSON binding input: {0}")]
    Json(#[from] serde_json::Error),
    #[error("cost calculation failed: {0}")]
    Pricing(#[from] urouter_ai::pricing::PricingError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use urouter_ai::pricing::{CostBreakdown, CostRates, ModelCost};
    use urouter_types::RateNanoUsdPerMillion;

    #[test]
    fn feature_binding_is_byte_equivalent_to_the_shared_rust_contract() {
        let request = serde_json::json!({
            "model": "urouter/auto",
            "messages": [{"role": "user", "content": "hello"}],
            "tools": [{"type": "function"}],
            "max_tokens": 128
        });
        let expected = FeatureFrame::from_openai_chat(&request);
        let actual: FeatureFrame =
            serde_json::from_str(&feature_frame_json(&request.to_string()).unwrap()).unwrap();
        assert_eq!(actual, expected);
    }

    #[test]
    fn pricing_binding_is_byte_equivalent_to_urouter_ai() {
        let cost = ModelCost {
            base: CostRates {
                input: RateNanoUsdPerMillion::new(1_000_000).unwrap(),
                output: RateNanoUsdPerMillion::new(2_000_000).unwrap(),
                cache_read: RateNanoUsdPerMillion::new(100_000).unwrap(),
                cache_write: RateNanoUsdPerMillion::new(500_000).unwrap(),
            },
            tiers: Vec::new(),
            long_cache_write: None,
        };
        let usage = Usage {
            input: 10,
            output: 5,
            cache_read: 2,
            cache_write: 1,
            cache_write_long: 0,
            reasoning: 0,
        };
        let expected: CostBreakdown = calculate_actual_cost(&cost, usage).unwrap();
        let actual: CostBreakdown = serde_json::from_str(
            &calculate_cost_json(
                &serde_json::to_string(&cost).unwrap(),
                &serde_json::to_string(&usage).unwrap(),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(actual, expected);
    }
}
