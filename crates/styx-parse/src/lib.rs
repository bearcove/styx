#![doc = include_str!("../README.md")]
//! Event-based parser for the Styx configuration language.
//!
//! This crate provides a low-level lexer and event-based parser for Styx documents.
//! It's designed to be used by higher-level tools like `styx-tree` (document tree)
//! and `facet-styx` (serde-like deserialization).

// Conditional tracing macros
#[cfg(feature = "tracing")]
macro_rules! trace {
    ($($arg:tt)*) => { ::tracing::trace!($($arg)*) };
}

#[cfg(not(feature = "tracing"))]
macro_rules! trace {
    ($($arg:tt)*) => {};
}

#[allow(unused_imports)]
pub(crate) use trace;

pub mod callback;
pub mod event;
pub mod parser;
pub mod parser3;
pub mod parser4;
mod span;
mod token;
mod tokenizer;

pub use callback::ParseCallback;
pub use event::{Event, ParseErrorKind, ScalarKind, Separator};
pub use parser3::Parser3;
pub use parser4::Parser4;
pub use span::Span;
pub use token::{Token, TokenKind};
pub use tokenizer::Tokenizer;

/// Pull-based streaming parser for Styx documents.
///
/// This is an alias for [`Parser3`], the modern pull-based parser implementation.
/// It replaces the callback-based parser and provides a simple `next_event()` interface.
pub type Parser2<'src> = Parser3<'src>;
