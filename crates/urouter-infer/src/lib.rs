use std::{collections::BTreeMap, path::Path};

use onnx_pb::{ModelProto, TensorProto, open_model, tensor_proto::DataType};
use thiserror::Error;
use urouter_artifact::LEARNED_FEATURE_DIMENSIONS;

const EXPECTED_OPERATORS: [&str; 5] = ["MatMul", "Add", "Relu", "MatMul", "Add"];

#[derive(Debug, Clone, PartialEq)]
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

#[cfg(test)]
mod tests {
    use onnx_pb::{GraphProto, NodeProto};

    use super::*;

    const HIDDEN: usize = 3;

    /// Test fixtures declare dimensions as `usize` but ONNX stores `i64`.
    fn dim(value: usize) -> i64 {
        i64::try_from(value).expect("test dimension fits in i64")
    }

    fn node(operator: &str, output: &str) -> NodeProto {
        NodeProto {
            output: vec![output.to_owned()],
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

    /// A minimal graph that `from_model` accepts, as the base every negative
    /// case mutates exactly one field of.
    fn valid_model() -> ModelProto {
        model_with_initializers(vec![
            double_tensor(
                "hidden_weights",
                vec![dim(LEARNED_FEATURE_DIMENSIONS), dim(HIDDEN)],
                vec![1.0; LEARNED_FEATURE_DIMENSIONS * HIDDEN],
            ),
            double_tensor("hidden_bias", vec![dim(HIDDEN)], vec![0.0; HIDDEN]),
            double_tensor("output_weights", vec![dim(HIDDEN), 1], vec![1.0; HIDDEN]),
            double_tensor("output_bias", vec![1], vec![0.0]),
        ])
    }

    fn model_with_initializers(initializer: Vec<TensorProto>) -> ModelProto {
        ModelProto {
            graph: Some(GraphProto {
                node: EXPECTED_OPERATORS
                    .iter()
                    .enumerate()
                    .map(|(index, operator)| node(operator, &format!("out{index}")))
                    .collect(),
                initializer,
                ..GraphProto::default()
            }),
            ..ModelProto::default()
        }
    }

    fn replace(name: &str, tensor: TensorProto) -> ModelProto {
        let mut model = valid_model();
        let graph = model.graph.as_mut().unwrap();
        let index = graph
            .initializer
            .iter()
            .position(|candidate| candidate.name == name)
            .unwrap();
        graph.initializer[index] = tensor;
        model
    }

    #[test]
    fn the_baseline_model_is_accepted_and_scores() {
        let router = OnnxRouter::from_model(&valid_model()).unwrap();
        assert_eq!(router.score([0; LEARNED_FEATURE_DIMENSIONS]).unwrap(), 0);
        // Every hidden unit sums the features, ReLU keeps the positive sum, and
        // each output weight is 1, so the score is HIDDEN * sum(features).
        assert_eq!(router.score([1; LEARNED_FEATURE_DIMENSIONS]).unwrap(), 18);
    }

    #[test]
    fn rejects_a_model_without_a_graph() {
        let model = ModelProto::default();
        assert_eq!(
            OnnxRouter::from_model(&model).unwrap_err(),
            InferError::MissingGraph
        );
    }

    #[test]
    fn rejects_an_operator_sequence_outside_the_whitelist() {
        let mut model = valid_model();
        model.graph.as_mut().unwrap().node[2] = node("Conv", "out2");
        assert_eq!(
            OnnxRouter::from_model(&model).unwrap_err(),
            InferError::UnsupportedGraph
        );

        // A truncated graph is rejected for the same reason: the whitelist is a
        // sequence, not a set.
        let mut truncated = valid_model();
        truncated.graph.as_mut().unwrap().node.pop();
        assert_eq!(
            OnnxRouter::from_model(&truncated).unwrap_err(),
            InferError::UnsupportedGraph
        );
    }

    #[test]
    fn rejects_each_missing_initializer() {
        for name in [
            "hidden_weights",
            "hidden_bias",
            "output_weights",
            "output_bias",
        ] {
            let mut model = valid_model();
            model
                .graph
                .as_mut()
                .unwrap()
                .initializer
                .retain(|tensor| tensor.name != name);
            assert_eq!(
                OnnxRouter::from_model(&model).unwrap_err(),
                InferError::MissingTensor(name),
                "{name} was not reported as missing"
            );
        }
    }

    #[test]
    fn rejects_a_non_double_initializer() {
        let mut tensor = double_tensor("hidden_bias", vec![dim(HIDDEN)], vec![0.0; HIDDEN]);
        tensor.data_type = DataType::Float as i32;
        assert_eq!(
            OnnxRouter::from_model(&replace("hidden_bias", tensor)).unwrap_err(),
            InferError::InvalidTensor("hidden_bias")
        );
    }

    #[test]
    fn rejects_an_initializer_with_the_wrong_rank() {
        let tensor = double_tensor("hidden_bias", vec![dim(HIDDEN), 1], vec![0.0; HIDDEN]);
        assert_eq!(
            OnnxRouter::from_model(&replace("hidden_bias", tensor)).unwrap_err(),
            InferError::InvalidTensor("hidden_bias")
        );
    }

    /// The value count must agree with the declared dimensions, or `score` would
    /// index outside the weight matrix.
    #[test]
    fn rejects_an_initializer_whose_dims_disagree_with_its_value_count() {
        let tensor = double_tensor(
            "hidden_weights",
            vec![dim(LEARNED_FEATURE_DIMENSIONS), dim(HIDDEN)],
            vec![1.0; LEARNED_FEATURE_DIMENSIONS * HIDDEN - 1],
        );
        assert_eq!(
            OnnxRouter::from_model(&replace("hidden_weights", tensor)).unwrap_err(),
            InferError::InvalidTensor("hidden_weights")
        );
    }

    #[test]
    fn rejects_an_empty_output_bias() {
        let tensor = double_tensor("output_bias", vec![0], Vec::new());
        assert_eq!(
            OnnxRouter::from_model(&replace("output_bias", tensor)).unwrap_err(),
            InferError::InvalidTensor("output_bias")
        );
    }

    #[test]
    fn rejects_incompatible_mlp_shapes() {
        // Feature dimension disagrees with the compiled-in artifact schema.
        let wrong_features = double_tensor(
            "hidden_weights",
            vec![dim(LEARNED_FEATURE_DIMENSIONS + 1), dim(HIDDEN)],
            vec![1.0; (LEARNED_FEATURE_DIMENSIONS + 1) * HIDDEN],
        );
        assert_eq!(
            OnnxRouter::from_model(&replace("hidden_weights", wrong_features)).unwrap_err(),
            InferError::InvalidShape
        );

        // Output layer disagrees with the hidden width.
        let wrong_output = double_tensor(
            "output_weights",
            vec![dim(HIDDEN + 1), 1],
            vec![1.0; HIDDEN + 1],
        );
        assert_eq!(
            OnnxRouter::from_model(&replace("output_weights", wrong_output)).unwrap_err(),
            InferError::InvalidShape
        );
    }

    #[test]
    fn rejects_a_score_that_leaves_the_numeric_domain() {
        let saturating = double_tensor(
            "hidden_weights",
            vec![dim(LEARNED_FEATURE_DIMENSIONS), dim(HIDDEN)],
            vec![f64::MAX; LEARNED_FEATURE_DIMENSIONS * HIDDEN],
        );
        let router = OnnxRouter::from_model(&replace("hidden_weights", saturating)).unwrap();
        assert_eq!(
            router
                .score([i64::MAX; LEARNED_FEATURE_DIMENSIONS])
                .unwrap_err(),
            InferError::NumericOverflow
        );
    }

    /// A zero-width hidden layer satisfies every shape rule, so the contract is
    /// pinned rather than left to chance: the score degrades to the output bias.
    #[test]
    fn zero_hidden_units_degrade_to_the_output_bias_without_panicking() {
        let model = model_with_initializers(vec![
            double_tensor(
                "hidden_weights",
                vec![dim(LEARNED_FEATURE_DIMENSIONS), 0],
                Vec::new(),
            ),
            double_tensor("hidden_bias", vec![0], Vec::new()),
            double_tensor("output_weights", vec![0, 1], Vec::new()),
            double_tensor("output_bias", vec![1], vec![7.0]),
        ]);
        let router = OnnxRouter::from_model(&model).unwrap();
        assert_eq!(router.score([9; LEARNED_FEATURE_DIMENSIONS]).unwrap(), 7);
    }

    #[test]
    fn load_reports_a_decode_error_for_a_file_that_is_not_onnx() {
        let path = std::env::temp_dir().join(format!("urouter-infer-{}.onnx", std::process::id()));
        std::fs::write(&path, b"this is not a protobuf model").unwrap();
        let error = OnnxRouter::load(&path).unwrap_err();
        assert!(
            matches!(error, InferError::Decode(_)),
            "expected a decode error, got {error:?}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_reports_a_missing_file_as_a_decode_error() {
        let error = OnnxRouter::load("/nonexistent/urouter-infer/model.onnx").unwrap_err();
        assert!(
            matches!(error, InferError::Decode(_)),
            "expected a decode error, got {error:?}"
        );
    }
}
