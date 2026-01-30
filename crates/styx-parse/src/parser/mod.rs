use crate::Event;
use crate::Lexer;

/// Parser frame for tracking nested structures.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq)]
struct Frame {
    kind: FrameKind,
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq)]
enum FrameKind {
    Object {},
    Seq {},
}

/// Frame-based pull parser for Styx.
#[allow(dead_code)]
#[derive(Clone)]
pub struct Parser<'src> {
    input: &'src str,
    lexer: Lexer<'src>,
    stack: Vec<Frame>,
}

impl<'src> Parser<'src> {
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

    pub fn parse_to_vec(mut self) -> Vec<Event<'src>> {
        let mut events = Vec::new();
        while let Some(event) = self.next_event() {
            events.push(event);
        }
        events
    }
}

#[cfg(test)]
mod tests;
