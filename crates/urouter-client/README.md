# urouter-client

Replay-aware client for a list of uRouter Gateway addresses. A request must be
declared replay-safe to fail over. Unsafe requests are attempted once, and a
stream is never replayed after successful response headers are returned.
