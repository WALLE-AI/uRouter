# urouter-py

PyO3 bindings that call the same Rust implementations used online. The module
exports `feature_frame(request_json)` and `calculate_cost(model_cost_json,
usage_json)`; both return canonical JSON and raise `ValueError` for invalid input.

Build with `maturin develop` from this directory in Python 3.10 or newer. The
extension uses Python's stable ABI and does not contain a second feature or pricing
implementation.
