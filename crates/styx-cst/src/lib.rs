#![doc = include_str!("../README.md")]

mod ast;
mod parser;
mod syntax_kind;
mod validation;

use parser::{Parse, ParseError, parse};
use syntax_kind::{StyxLanguage, SyntaxElement, SyntaxKind, SyntaxNode, SyntaxToken};
use validation::{Diagnostic, Severity, validate};

// Re-export rowan types for convenience
pub use rowan::{TextRange, TextSize, TokenAtOffset};
