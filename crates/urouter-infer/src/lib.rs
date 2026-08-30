use std::{collections::BTreeMap, path::Path};

use onnx_pb::{ModelProto, TensorProto, open_model, tensor_proto::DataType};
use thiserror::Error;
use urouter_artifact::LEARNED_FEATURE_DIMENSIONS;

const EXPECTED_OPERATORS: [&str; 5] = ["MatMul", "Add", "Relu", "MatMul", "Add"];

pub struct OnnxRouter {
    hidden_weights: Vec<f64>,
    hidden_bias: Vec<f64>,
    output_weights: Vec<f64>,
    output_bias: f64,
}

impl OnnxRouter {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, InferError> {
        let model = open_model(path).map_err(|error| InferError::Decode(format!("{error:?}")))?;
        Self::from_model(&model)
    }

    pub fn from_model(model: &ModelProto) -> Result<Self, InferError> {
        let graph = model.graph.as_ref().ok_or(InferError::MissingGraph)?;
        let operators = graph
            .node
            .iter()
            .map(|node| node.op_type.as_str())
            .collect::<Vec<_>>();
        if operators != EXPECTED_OPERATORS {
            return Err(InferError::UnsupportedGraph);
        }
        let tensors = graph
            .initializer
            .iter()
            .map(|tensor| (tensor.name.as_str(), tensor))
            .collect::<BTreeMap<_, _>>();
        let hidden_weights = tensor(&tensors, "hidden_weights", 2)?;
        let hidden_bias = tensor(&tensors, "hidden_bias", 1)?;
        let output_weights = tensor(&tensors, "output_weights", 2)?;
        let output_bias = *tensor(&tensors, "output_bias", 1)?
            .values
            .first()
            .ok_or(InferError::InvalidTensor("output_bias"))?;
        let dimensions = i64::try_from(LEARNED_FEATURE_DIMENSIONS).unwrap_or(i64::MAX);
        let hidden_count = i64::try_from(hidden_bias.values.len()).unwrap_or(i64::MAX);
        if hidden_weights.dims[0] != dimensions
            || hidden_weights.dims[1] != hidden_count
            || output_weights.dims != [hidden_count, 1]
        {
            return Err(InferError::InvalidShape);
        }
        Ok(Self {
            hidden_weights: hidden_weights.values,
            hidden_bias: hidden_bias.values,
            output_weights: output_weights.values,
            output_bias,
        })
    }

    #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
    pub fn score(&self, features: [i64; LEARNED_FEATURE_DIMENSIONS]) -> Result<i64, InferError> {
        let hidden_count = self.hidden_bias.len();
        let mut hidden = self.hidden_bias.clone();
        for (feature_index, feature) in features.into_iter().enumerate() {
            for (hidden_index, value) in hidden.iter_mut().enumerate() {
                *value += feature as f64
                    * self.hidden_weights[feature_index * hidden_count + hidden_index];
            }
        }
        let score = hidden
            .iter()
            .zip(&self.output_weights)
            .fold(self.output_bias, |score, (hidden, weight)| {
                score + hidden.max(0.0) * weight
            });
        if !score.is_finite() || score < i64::MIN as f64 || score > i64::MAX as f64 {
            return Err(InferError::NumericOverflow);
        }
        Ok(score.round() as i64)
    }
}

struct TensorValues {
    dims: Vec<i64>,
    values: Vec<f64>,
}

fn tensor(
    tensors: &BTreeMap<&str, &TensorProto>,
    name: &'static str,
    rank: usize,
) -> Result<TensorValues, InferError> {
    let tensor = tensors.get(name).ok_or(InferError::MissingTensor(name))?;
    if tensor.data_type != DataType::Double as i32 || tensor.dims.len() != rank {
        return Err(InferError::InvalidTensor(name));
    }
    let elements = tensor.dims.iter().try_fold(1_usize, |size, dimension| {
        usize::try_from(*dimension)
            .ok()
            .and_then(|dimension| size.checked_mul(dimension))
    });
    if elements != Some(tensor.double_data.len()) {
        return Err(InferError::InvalidTensor(name));
    }
    Ok(TensorValues {
        dims: tensor.dims.clone(),
        values: tensor.double_data.clone(),
    })
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum InferError {
    #[error("ONNX protobuf decode failed: {0}")]
    Decode(String),
    #[error("ONNX model has no graph")]
    MissingGraph,
    #[error("ONNX graph is outside the bounded MLP operator whitelist")]
    UnsupportedGraph,
    #[error("ONNX graph is missing initializer {0}")]
    MissingTensor(&'static str),
    #[error("ONNX initializer {0} has an invalid type, rank, or value count")]
    InvalidTensor(&'static str),
    #[error("ONNX MLP tensor shapes are incompatible")]
    InvalidShape,
    #[error("ONNX inference exceeded the numeric domain")]
    NumericOverflow,
}
