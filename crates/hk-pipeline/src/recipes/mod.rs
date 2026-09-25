//! Decoder-workbench recipe runtime (ADR-0011 §2, §7). Module list pre-added by T-085 so each
//! owning task fills in only its own file.

pub mod audio; // T-866
pub mod capture; // T-092
pub mod graph; // T-088
pub mod hops;
pub mod messages; // T-111
pub mod openers; // T-088
pub mod refine; // T-870
pub mod runtime; // T-088
pub mod session; // T-869: ephemeral, session-owned audio pipelines
pub mod store; // T-088
pub mod swap; // T-088
pub mod tap_eye; // T-161
pub mod tap_spectrum; // T-160
pub mod tap_sync_search; // T-162
pub mod taps; // T-088 // T-093
