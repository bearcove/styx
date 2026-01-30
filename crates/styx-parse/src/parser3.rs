use std::borrow::Cow;
use std::collections::VecDeque;

use crate::event::{Event, ParseErrorKind, ScalarKind, Separator};
use crate::lexer::Lexer;
use crate::span::Span;
use crate::token::{Token, TokenKind};

#[allow(unused_imports)]
use crate::trace;

#[derive(Debug, Clone, PartialEq)]
enum State {
    // FILL ME
    Todo,
}

#[derive(Clone)]
pub struct Parser3<'src> {
    input: &'src str,
    lexer: Lexer<'src>,
    state: State,
    // WE DO NOT PEEK
    // WE DO NOT UNPEEK
    // WE DO NOT BUFFER EVENTS
    // WE DO NOT COLLECT ALL TOKENS
    // WE DO NOT COLLECT ALL EVENTS
    // WE ARE A PULL PARSER, FULLY STREAMING, WITH A STATE MACHINE
    // AND THAT IS ALL. WE DO NOT NEED LOOKAHEAD, LOOKBEHIND, OR LOOKY LOOK.
}

impl<'src> Parser3<'src> {
    pub fn new(source: &'src str) -> Self {
        Self {
            input: source,
            lexer: Lexer::new(source),
            state: State::Todo,
        }
    }

    pub fn next_event(&mut self) -> Option<Event<'src>> {
        todo!("let's go")
    }
}

#[cfg(test)]
mod tests;
