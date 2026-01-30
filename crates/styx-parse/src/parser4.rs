use std::borrow::Cow;

use crate::event::{Event, ParseErrorKind, ScalarKind, Separator};
use crate::lexer::Lexer;
use crate::span::Span;
use crate::token::TokenKind;

#[allow(unused_imports)]
use crate::trace;

/// Parser state machine.
#[derive(Debug, Clone, PartialEq)]
struct Frame {
    // can add some fields here
    kind: FrameKind,
}

#[derive(Debug, Clone, PartialEq)]
enum FrameKind {
    Object {},
    Seq {},
    // etc.
}

#[derive(Clone)]
pub struct Parser4<'src> {
    input: &'src str,
    lexer: Lexer<'src>,
    stack: Vec<Frame>,
}

impl<'src> Parser4<'src> {
    /// Create a new parser in document mode (implicit root object).
    pub fn new(source: &'src str) -> Self {
        Self {
            input: source,
            lexer: Lexer::new(source),
            stack: Default::default(),
        }
    }

    pub fn next_event(&mut self) -> Option<Event<'src>> {
        todo!("good luck")
    }
}

#[cfg(test)]
mod tests;
