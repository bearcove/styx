use std::borrow::Cow;

use crate::event::{Event, ParseErrorKind, ScalarKind, Separator};
use crate::lexer::Lexer;
use crate::span::Span;
use crate::token::TokenKind;

#[allow(unused_imports)]
use crate::trace;

/// Parser state machine.
///
/// RULES:
/// 1. Each call to next_event() returns exactly ONE event (or None when done).
/// 2. State encodes everything needed to produce the next event.
/// 3. NO event queue. NO peeking. NO buffering.
/// 4. If we read a token and need to emit multiple events before processing it,
///    we encode the token info in state and emit events one at a time.
///
/// State naming:
/// - `Emit*` states emit an event without reading tokens
/// - `Expect*` / `After*` states read tokens to decide what to emit
#[derive(Debug, Clone, PartialEq)]
enum State {
    /// Initial state - emit DocumentStart.
    Start,

    /// Emit ObjectStart for implicit root object.
    EmitRootObjectStart,

    /// Inside an object, expecting an entry (or closing brace/EOF).
    ExpectEntry,

    /// Emit EntryStart, then go to EmitKey.
    EmitEntryStart {
        key_span: Span,
        key_kind: ScalarKind,
    },

    /// Emit Key event, then read value token.
    EmitKey {
        key_span: Span,
        key_kind: ScalarKind,
    },

    /// Emit Scalar value, then EntryEnd.
    EmitScalarValue { span: Span, kind: ScalarKind },

    /// Emit Unit value (for key without value), then EntryEnd.
    EmitUnitValue { span: Span },

    /// Emit EntryEnd, then go back to ExpectEntry.
    EmitEntryEnd,

    /// Emit ObjectStart as a value (nested object).
    EmitObjectStartValue { span: Span },

    /// Emit SequenceStart as a value.
    EmitSequenceStartValue { span: Span },

    /// Inside a sequence, expecting an element.
    ExpectSeqElem,

    /// Emit TagStart, then check for payload.
    EmitTagStart { tag_span: Span },

    /// After emitting TagStart, check for payload.
    AfterTagStart { tag_span: Span },

    /// Emit TagEnd after a tag with no payload or after payload.
    EmitTagEnd,

    /// Emit DocumentEnd, then Done.
    EmitDocumentEnd,

    /// Done - return None forever.
    Done,
}

/// Context for nested structures.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Context {
    /// Inside an object. `implicit` = true for the root object.
    Object { implicit: bool },
    /// Inside a sequence.
    Sequence,
}

#[derive(Clone)]
pub struct Parser3<'src> {
    input: &'src str,
    lexer: Lexer<'src>,
    state: State,
    /// Stack of nested contexts (objects/sequences).
    context_stack: Vec<Context>,
    // WE DO NOT PEEK
    // WE DO NOT UNPEEK
    // WE DO NOT BUFFER EVENTS
    // WE DO NOT COLLECT ALL TOKENS
    // WE DO NOT COLLECT ALL EVENTS
    // WE ARE A PULL PARSER, FULLY STREAMING, WITH A STATE MACHINE
    // AND THAT IS ALL.
}

impl<'src> Parser3<'src> {
    pub fn new(source: &'src str) -> Self {
        Self {
            input: source,
            lexer: Lexer::new(source),
            state: State::Start,
            context_stack: Vec::new(),
        }
    }

    fn eof_span(&self) -> Span {
        let pos = self.input.len() as u32;
        Span::new(pos, pos)
    }

    fn text_at(&self, span: Span) -> &'src str {
        &self.input[span.start as usize..span.end as usize]
    }

    /// Skip whitespace but not newlines.
    fn next_token_skip_ws(&mut self) -> crate::token::Token<'src> {
        loop {
            let t = self.lexer.next_token();
            if t.kind == TokenKind::Whitespace {
                continue;
            }
            return t;
        }
    }

    /// Skip whitespace and newlines.
    fn next_token_skip_ws_nl(&mut self) -> crate::token::Token<'src> {
        loop {
            let t = self.lexer.next_token();
            match t.kind {
                TokenKind::Whitespace | TokenKind::Newline | TokenKind::LineComment => continue,
                _ => return t,
            }
        }
    }

    /// Unescape a quoted string (strip quotes, process escapes).
    fn unescape_quoted(&self, text: &'src str) -> Cow<'src, str> {
        // Strip surrounding quotes
        let inner = &text[1..text.len() - 1];

        // Fast path: no escapes
        if !inner.contains('\\') {
            return Cow::Borrowed(inner);
        }

        // Slow path: process escapes
        let mut result = String::with_capacity(inner.len());
        let mut chars = inner.chars().peekable();

        while let Some(c) = chars.next() {
            if c == '\\' {
                match chars.next() {
                    Some('n') => result.push('\n'),
                    Some('r') => result.push('\r'),
                    Some('t') => result.push('\t'),
                    Some('\\') => result.push('\\'),
                    Some('"') => result.push('"'),
                    Some('u') => {
                        // Unicode escape: \uXXXX or \u{X...}
                        if chars.peek() == Some(&'{') {
                            chars.next(); // consume '{'
                            let mut hex = String::new();
                            while let Some(&c) = chars.peek() {
                                if c == '}' {
                                    chars.next();
                                    break;
                                }
                                hex.push(chars.next().unwrap());
                            }
                            if let Ok(code) = u32::from_str_radix(&hex, 16) {
                                if let Some(ch) = char::from_u32(code) {
                                    result.push(ch);
                                }
                            }
                        } else {
                            // \uXXXX
                            let hex: String = chars.by_ref().take(4).collect();
                            if let Ok(code) = u32::from_str_radix(&hex, 16) {
                                if let Some(ch) = char::from_u32(code) {
                                    result.push(ch);
                                }
                            }
                        }
                    }
                    Some(other) => {
                        // Invalid escape - keep as-is for now, validation will catch it
                        result.push('\\');
                        result.push(other);
                    }
                    None => result.push('\\'),
                }
            } else {
                result.push(c);
            }
        }

        Cow::Owned(result)
    }

    pub fn parse_to_vec(mut self) -> Vec<Event<'src>> {
        let mut events = Vec::new();
        while let Some(event) = self.next_event() {
            events.push(event);
        }
        events
    }

    pub fn next_event(&mut self) -> Option<Event<'src>> {
        loop {
            trace!(state = ?self.state, "next_event");
            match std::mem::replace(&mut self.state, State::Done) {
                State::Start => {
                    self.state = State::EmitRootObjectStart;
                    return Some(Event::DocumentStart);
                }

                State::EmitRootObjectStart => {
                    self.context_stack.push(Context::Object { implicit: true });
                    self.state = State::ExpectEntry;
                    return Some(Event::ObjectStart {
                        span: Span::new(0, 0),
                        separator: Separator::Newline,
                    });
                }

                State::ExpectEntry => {
                    let t = self.next_token_skip_ws_nl();

                    match t.kind {
                        TokenKind::Eof => {
                            // End of input - close root object
                            self.context_stack.pop();
                            self.state = State::EmitDocumentEnd;
                            return Some(Event::ObjectEnd {
                                span: self.eof_span(),
                            });
                        }

                        TokenKind::RBrace => {
                            // Close explicit object
                            match self.context_stack.pop() {
                                Some(Context::Object { implicit: false }) => {
                                    self.state = State::EmitEntryEnd;
                                    return Some(Event::ObjectEnd { span: t.span });
                                }
                                Some(Context::Object { implicit: true }) => {
                                    // Can't close implicit root with }
                                    self.context_stack.push(Context::Object { implicit: true });
                                    self.state = State::ExpectEntry;
                                    return Some(Event::Error {
                                        span: t.span,
                                        kind: ParseErrorKind::UnexpectedToken,
                                    });
                                }
                                _ => {
                                    self.state = State::ExpectEntry;
                                    return Some(Event::Error {
                                        span: t.span,
                                        kind: ParseErrorKind::UnexpectedToken,
                                    });
                                }
                            }
                        }

                        TokenKind::BareScalar => {
                            self.state = State::EmitEntryStart {
                                key_span: t.span,
                                key_kind: ScalarKind::Bare,
                            };
                            continue;
                        }

                        TokenKind::QuotedScalar => {
                            self.state = State::EmitEntryStart {
                                key_span: t.span,
                                key_kind: ScalarKind::Quoted,
                            };
                            continue;
                        }

                        TokenKind::At => {
                            // TODO: @ (unit key or tag)
                            todo!("@ in key position")
                        }

                        TokenKind::DocComment => {
                            return Some(Event::DocComment {
                                span: t.span,
                                text: t.text,
                            });
                        }

                        _ => {
                            return Some(Event::Error {
                                span: t.span,
                                kind: ParseErrorKind::UnexpectedToken,
                            });
                        }
                    }
                }

                State::EmitEntryStart { key_span, key_kind } => {
                    self.state = State::EmitKey { key_span, key_kind };
                    return Some(Event::EntryStart);
                }

                State::EmitKey { key_span, key_kind } => {
                    // Get the key text
                    let key_text = self.text_at(key_span);
                    let key_payload = match key_kind {
                        ScalarKind::Quoted => self.unescape_quoted(key_text),
                        _ => Cow::Borrowed(key_text),
                    };

                    // Read next token to see what the value is
                    let t = self.next_token_skip_ws();
                    trace!(token = ?t, "EmitKey got token");

                    match t.kind {
                        TokenKind::BareScalar => {
                            self.state = State::EmitScalarValue {
                                span: t.span,
                                kind: ScalarKind::Bare,
                            };
                            return Some(Event::Key {
                                span: key_span,
                                tag: None,
                                payload: Some(key_payload),
                                kind: key_kind,
                            });
                        }

                        TokenKind::QuotedScalar => {
                            self.state = State::EmitScalarValue {
                                span: t.span,
                                kind: ScalarKind::Quoted,
                            };
                            return Some(Event::Key {
                                span: key_span,
                                tag: None,
                                payload: Some(key_payload),
                                kind: key_kind,
                            });
                        }

                        TokenKind::Newline | TokenKind::Eof => {
                            self.state = State::EmitUnitValue { span: key_span };
                            return Some(Event::Key {
                                span: key_span,
                                tag: None,
                                payload: Some(key_payload),
                                kind: key_kind,
                            });
                        }

                        TokenKind::RBrace => {
                            // Key with unit value, then close brace
                            // We need to emit: Key, Unit, EntryEnd, ObjectEnd
                            // Current call: emit Key
                            // Next state needs to emit Unit, then handle the }
                            // But we consumed the }!
                            //
                            // Solution: add a state that remembers we need to close
                            todo!("unit value then close brace")
                        }

                        TokenKind::LBrace => {
                            // Nested object as value
                            self.state = State::EmitObjectStartValue { span: t.span };
                            return Some(Event::Key {
                                span: key_span,
                                tag: None,
                                payload: Some(key_payload),
                                kind: key_kind,
                            });
                        }

                        TokenKind::LParen => {
                            // Sequence as value
                            self.state = State::EmitSequenceStartValue { span: t.span };
                            return Some(Event::Key {
                                span: key_span,
                                tag: None,
                                payload: Some(key_payload),
                                kind: key_kind,
                            });
                        }

                        TokenKind::At => {
                            // Tagged value
                            todo!("tagged value")
                        }

                        _ => {
                            self.state = State::EmitEntryEnd;
                            return Some(Event::Error {
                                span: t.span,
                                kind: ParseErrorKind::UnexpectedToken,
                            });
                        }
                    }
                }

                State::EmitScalarValue { span, kind } => {
                    let text = self.text_at(span);
                    let value = match kind {
                        ScalarKind::Quoted => self.unescape_quoted(text),
                        _ => Cow::Borrowed(text),
                    };
                    self.state = State::EmitEntryEnd;
                    return Some(Event::Scalar { span, value, kind });
                }

                State::EmitUnitValue { span } => {
                    self.state = State::EmitEntryEnd;
                    return Some(Event::Unit { span });
                }

                State::EmitEntryEnd => {
                    // After EntryEnd, check what context we're in
                    match self.context_stack.last() {
                        Some(Context::Object { .. }) => {
                            self.state = State::ExpectEntry;
                        }
                        Some(Context::Sequence) => {
                            self.state = State::ExpectSeqElem;
                        }
                        None => {
                            self.state = State::EmitDocumentEnd;
                        }
                    }
                    return Some(Event::EntryEnd);
                }

                State::EmitObjectStartValue { span } => {
                    self.context_stack.push(Context::Object { implicit: false });
                    self.state = State::ExpectEntry;
                    return Some(Event::ObjectStart {
                        span,
                        separator: Separator::Comma, // Explicit objects use comma
                    });
                }

                State::EmitSequenceStartValue { span } => {
                    self.context_stack.push(Context::Sequence);
                    self.state = State::ExpectSeqElem;
                    return Some(Event::SequenceStart { span });
                }

                State::ExpectSeqElem => {
                    let t = self.next_token_skip_ws_nl();

                    match t.kind {
                        TokenKind::RParen => {
                            // End of sequence
                            self.context_stack.pop();
                            self.state = State::EmitEntryEnd;
                            return Some(Event::SequenceEnd { span: t.span });
                        }

                        TokenKind::Eof => {
                            // Unclosed sequence
                            return Some(Event::Error {
                                span: self.eof_span(),
                                kind: ParseErrorKind::UnclosedSequence,
                            });
                        }

                        TokenKind::BareScalar => {
                            // Element value
                            self.state = State::ExpectSeqElem;
                            return Some(Event::Scalar {
                                span: t.span,
                                value: Cow::Borrowed(self.text_at(t.span)),
                                kind: ScalarKind::Bare,
                            });
                        }

                        TokenKind::QuotedScalar => {
                            self.state = State::ExpectSeqElem;
                            let text = self.text_at(t.span);
                            return Some(Event::Scalar {
                                span: t.span,
                                value: self.unescape_quoted(text),
                                kind: ScalarKind::Quoted,
                            });
                        }

                        TokenKind::LBrace => {
                            // Nested object in sequence
                            self.context_stack.push(Context::Object { implicit: false });
                            self.state = State::ExpectEntry;
                            return Some(Event::ObjectStart {
                                span: t.span,
                                separator: Separator::Comma,
                            });
                        }

                        TokenKind::LParen => {
                            // Nested sequence
                            self.context_stack.push(Context::Sequence);
                            self.state = State::ExpectSeqElem;
                            return Some(Event::SequenceStart { span: t.span });
                        }

                        TokenKind::At => {
                            // Tag in sequence
                            todo!("tag in sequence")
                        }

                        _ => {
                            return Some(Event::Error {
                                span: t.span,
                                kind: ParseErrorKind::UnexpectedToken,
                            });
                        }
                    }
                }

                State::EmitDocumentEnd => {
                    self.state = State::Done;
                    return Some(Event::DocumentEnd);
                }

                State::Done => {
                    return None;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
