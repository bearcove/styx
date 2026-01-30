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

mod event;

mod lexer;

mod parser;

mod span;

mod token;

mod tokenizer;

use event::{Event, ParseErrorKind, ScalarKind, Separator};
use lexer::{Lexeme, Lexer};
use parser::Parser;
use span::Span;
use token::{Token, TokenKind};
use tokenizer::Tokenizer;
