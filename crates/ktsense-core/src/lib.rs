//! Pure domain layer for ktsense.
//!
//! This crate holds the model, the compressors, the ranking and the budgeting logic. It must never
//! depend on the filesystem, a process, a runtime, or a parser: adapters convert the outside world
//! into these types, and the CLI and MCP front-ends render them. Keeping the rule makes every
//! algorithm here testable from hand-built values.

#![forbid(unsafe_code)]

pub mod render;
pub mod skeleton;

pub use render::{render_markdown, render_skeleton, RenderOptions};
pub use skeleton::{
    DeclKind, Declaration, FileSkeleton, Modifier, Parameter, ParameterProperty, Visibility,
};

/// Version reported by every front-end, sourced from the workspace manifest.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Estimates the token cost of rendered output so budgeted commands can stop in time.
///
/// The default implementation is a byte-ratio approximation rather than a real tokenizer, so a
/// front-end that wants exactness can supply its own without changing any caller.
pub trait TokenEstimator {
    fn estimate(&self, text: &str) -> usize;
}

/// Byte-ratio estimator tuned for Kotlin-like source text.
#[derive(Debug, Clone, Copy, Default)]
pub struct ByteRatioEstimator;

const BYTES_PER_TOKEN: f64 = 3.6;

impl TokenEstimator for ByteRatioEstimator {
    fn estimate(&self, text: &str) -> usize {
        (text.len() as f64 / BYTES_PER_TOKEN).ceil() as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_text_costs_nothing() {
        assert_eq!(ByteRatioEstimator.estimate(""), 0);
    }

    #[test]
    fn estimate_rounds_up_so_a_budget_is_never_understated() {
        assert_eq!(ByteRatioEstimator.estimate("fun main()"), 3);
    }
}
