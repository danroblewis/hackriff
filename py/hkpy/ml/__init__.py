"""Model training for the hackriff classification cascade (orchestration only).

Nothing here runs on the real-time path: training produces a versioned weights file plus a
manifest, and Rust (``hk-ml``) loads and runs it. See ADR-0016 §4.6 and §6.
"""
