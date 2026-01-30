use crate::event::Event;
use crate::lexer::Lexer;

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
