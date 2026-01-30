#![doc = include_str!("../README.md")]

mod ast;
mod parser;
mod syntax_kind;
mod validation;

// Re-export rowan types for convenience
pub use rowan::{TextRange, TextSize, TokenAtOffset};
