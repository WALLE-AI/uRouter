# urouter-lab

`urouter-lab` is the offline, deterministic evaluation and artifact pipeline. It
exports governed datasets, calculates IPS/SNIPS/DR reports, summarizes the five
benchmark categories, builds signed linear-policy artifacts, and verifies them.
It can also execute a five-category suite against an already running Gateway with
explicit request and cost caps. Gateway runs retain checks, routing headers, usage,
cost and latency but never retain provider response bodies or provider credentials.

Run `cargo run -p urouter-lab -- --help` for command and input details. Signing
keys are accepted only through the environment variable named by
`--signing-key-env`; they are never accepted as command-line values or written
to an artifact.
