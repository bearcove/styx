//! Pull-based streaming parser for Styx.
//!
//! State machine: consume token, transition state, emit event.
//! No peeking. No backtracking. No unread.

use std::borrow::Cow;
use std::collections::VecDeque;

use crate::event::{Event, ParseErrorKind, ScalarKind, Separator};
use crate::lexer::Lexer;
use crate::span::Span;
use crate::token::{Token, TokenKind};

#[allow(unused_imports)]
use crate::trace;

/// Parser state. Stores spans, not string references.
#[derive(Debug, Clone, PartialEq)]
enum State {
    Start,
    BeforeRoot,
    ExpectEntry,
    /// After bare key - next token decides: `>` = attribute, else = value.
    AfterBareKey {
        key_span: Span,
    },
    /// After `@` in key position - next token is tag name or whitespace (unit key).
    AfterAtKey {
        at_span: Span,
    },
    /// After key (any kind) - expecting value.
    AfterKey {
        key_span: Span,
    },
    /// After `key>` - expecting attribute value.
    AfterGt {
        key_span: Span,
        in_chain: bool,
    },
    /// After attribute value - check for more attributes.
    MaybeMoreAttr {
        obj_span: Span,
    },
    /// In attr chain, just saw bare key - next token decides: `>` = more attr, else = TooManyAtoms.
    AfterBareKeyInAttr {
        key_span: Span,
        obj_span: Span,
    },
    /// Saw bare scalar in value position - check if followed by `>` (attribute chain start).
    AfterBareValue {
        value_span: Span,
    },
    /// After value - check for TooManyAtoms or boundary.
    AfterValue,
    ExpectSeqElem,
    Done,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Context {
    Object { implicit: bool },
    Sequence,
    AttrObject,
}

pub struct Parser2<'src> {
    input: &'src str,
    lexer: Lexer<'src>,
    state: State,
    context_stack: Vec<Context>,
    event_queue: VecDeque<Event<'src>>,
    pending_doc: Vec<(Span, &'src str)>,
    expr_mode: bool,
}

impl<'src> Parser2<'src> {
    pub fn new(source: &'src str) -> Self {
        Self {
            input: source,
            lexer: Lexer::new(source),
            state: State::Start,
            context_stack: Vec::new(),
            event_queue: VecDeque::new(),
            pending_doc: Vec::new(),
            expr_mode: false,
        }
    }

    pub fn new_expr(source: &'src str) -> Self {
        Self {
            input: source,
            lexer: Lexer::new(source),
            state: State::Start,
            context_stack: Vec::new(),
            event_queue: VecDeque::new(),
            pending_doc: Vec::new(),
            expr_mode: true,
        }
    }

    pub fn next_event(&mut self) -> Option<Event<'src>> {
        if let Some(ev) = self.event_queue.pop_front() {
            return Some(ev);
        }

        loop {
            if self.state == State::Done {
                return None;
            }

            if let Some(ev) = self.step() {
                return Some(ev);
            }

            if let Some(ev) = self.event_queue.pop_front() {
                return Some(ev);
            }
        }
    }

    fn step(&mut self) -> Option<Event<'src>> {
        match self.state.clone() {
            State::Start => {
                self.state = State::BeforeRoot;
                Some(Event::DocumentStart)
            }
            State::BeforeRoot => self.step_before_root(),
            State::ExpectEntry => self.step_expect_entry(),
            State::AfterBareKey { key_span } => self.step_after_bare_key(key_span),
            State::AfterAtKey { at_span } => self.step_after_at_key(at_span),
            State::AfterKey { key_span } => self.step_after_key(key_span),
            State::AfterGt { key_span, in_chain } => self.step_after_gt(key_span, in_chain),
            State::MaybeMoreAttr { obj_span } => self.step_maybe_more_attr(obj_span),
            State::AfterBareKeyInAttr { key_span, obj_span } => {
                self.step_after_bare_key_in_attr(key_span, obj_span)
            }
            State::AfterBareValue { value_span } => self.step_after_bare_value(value_span),
            State::AfterValue => self.step_after_value(),
            State::ExpectSeqElem => self.step_expect_seq_elem(),
            State::Done => None,
        }
    }

    // === Token consumption ===

    fn next_token(&mut self) -> Token<'src> {
        loop {
            let t = self.lexer.next_token();
            if t.kind != TokenKind::Whitespace {
                return t;
            }
        }
    }

    fn next_token_skip_newlines(&mut self) -> Token<'src> {
        loop {
            let t = self.next_token();
            if t.kind != TokenKind::Newline {
                return t;
            }
        }
    }

    fn span_text(&self, span: Span) -> &'src str {
        &self.input[span.start as usize..span.end as usize]
    }

    // === State handlers ===

    fn step_before_root(&mut self) -> Option<Event<'src>> {
        let t = self.next_token();

        match t.kind {
            TokenKind::Eof => {
                if !self.expr_mode {
                    self.context_stack.push(Context::Object { implicit: true });
                    self.event_queue.push_back(Event::ObjectEnd {
                        span: Span::new(0, 0),
                    });
                }
                self.event_queue.push_back(Event::DocumentEnd);
                self.state = State::Done;
                Some(Event::ObjectStart {
                    span: Span::new(0, 0),
                    separator: Separator::Newline,
                })
            }

            TokenKind::Newline => None,

            TokenKind::LineComment => Some(Event::Comment {
                span: t.span,
                text: t.text,
            }),

            TokenKind::DocComment => {
                self.pending_doc.push((t.span, t.text));
                None
            }

            TokenKind::LBrace => {
                self.context_stack.push(Context::Object { implicit: false });
                self.state = State::ExpectEntry;
                Some(Event::ObjectStart {
                    span: t.span,
                    separator: Separator::Comma,
                })
            }

            // Implicit root
            TokenKind::BareScalar => {
                self.context_stack.push(Context::Object { implicit: true });
                self.emit_pending_docs();
                self.event_queue.push_back(Event::Key {
                    span: t.span,
                    tag: None,
                    payload: Some(Cow::Borrowed(t.text)),
                    kind: ScalarKind::Bare,
                });
                self.state = State::AfterBareKey { key_span: t.span };
                self.event_queue.push_back(Event::EntryStart);
                Some(Event::ObjectStart {
                    span: Span::new(0, 0),
                    separator: Separator::Newline,
                })
            }

            TokenKind::QuotedScalar => {
                self.context_stack.push(Context::Object { implicit: true });
                self.emit_pending_docs();
                self.event_queue.push_back(Event::Key {
                    span: t.span,
                    tag: None,
                    payload: Some(self.unescape_quoted(t.text)),
                    kind: ScalarKind::Quoted,
                });
                self.state = State::AfterKey { key_span: t.span };
                self.event_queue.push_back(Event::EntryStart);
                Some(Event::ObjectStart {
                    span: Span::new(0, 0),
                    separator: Separator::Newline,
                })
            }

            TokenKind::At => {
                self.context_stack.push(Context::Object { implicit: true });
                self.state = State::AfterAtKey { at_span: t.span };
                self.event_queue.push_back(Event::EntryStart);
                Some(Event::ObjectStart {
                    span: Span::new(0, 0),
                    separator: Separator::Newline,
                })
            }

            _ => {
                self.context_stack.push(Context::Object { implicit: true });
                self.state = State::ExpectEntry;
                Some(Event::ObjectStart {
                    span: Span::new(0, 0),
                    separator: Separator::Newline,
                })
            }
        }
    }

    fn step_expect_entry(&mut self) -> Option<Event<'src>> {
        let t = self.next_token_skip_newlines();

        match t.kind {
            TokenKind::Eof => self.close_at_eof(),

            TokenKind::RBrace => self.handle_rbrace(t.span),

            TokenKind::RParen => Some(Event::Error {
                span: t.span,
                kind: ParseErrorKind::UnexpectedToken,
            }),

            TokenKind::Comma => None,

            TokenKind::LineComment => Some(Event::Comment {
                span: t.span,
                text: t.text,
            }),

            TokenKind::DocComment => {
                self.pending_doc.push((t.span, t.text));
                None
            }

            TokenKind::BareScalar => {
                self.emit_pending_docs();
                self.event_queue.push_back(Event::Key {
                    span: t.span,
                    tag: None,
                    payload: Some(Cow::Borrowed(t.text)),
                    kind: ScalarKind::Bare,
                });
                self.state = State::AfterBareKey { key_span: t.span };
                Some(Event::EntryStart)
            }

            TokenKind::QuotedScalar => {
                self.emit_pending_docs();
                self.event_queue.push_back(Event::Key {
                    span: t.span,
                    tag: None,
                    payload: Some(self.unescape_quoted(t.text)),
                    kind: ScalarKind::Quoted,
                });
                self.state = State::AfterKey { key_span: t.span };
                Some(Event::EntryStart)
            }

            TokenKind::RawScalar => {
                self.emit_pending_docs();
                self.event_queue.push_back(Event::Key {
                    span: t.span,
                    tag: None,
                    payload: Some(Cow::Borrowed(Self::strip_raw_delimiters(t.text))),
                    kind: ScalarKind::Raw,
                });
                self.state = State::AfterKey { key_span: t.span };
                Some(Event::EntryStart)
            }

            TokenKind::At => {
                self.emit_pending_docs();
                self.state = State::AfterAtKey { at_span: t.span };
                Some(Event::EntryStart)
            }

            TokenKind::HeredocStart => {
                self.skip_heredoc();
                Some(Event::Error {
                    span: t.span,
                    kind: ParseErrorKind::InvalidKey,
                })
            }

            _ => Some(Event::Error {
                span: t.span,
                kind: ParseErrorKind::ExpectedKey,
            }),
        }
    }

    fn step_after_bare_key(&mut self, key_span: Span) -> Option<Event<'src>> {
        let t = self.next_token();

        match t.kind {
            TokenKind::Gt if t.span.start == key_span.end => {
                // Attribute syntax: key>value
                self.state = State::AfterGt {
                    key_span,
                    in_chain: false,
                };
                None
            }

            // Not attribute - handle as normal value position
            TokenKind::Newline
            | TokenKind::Eof
            | TokenKind::RBrace
            | TokenKind::RParen
            | TokenKind::Comma => {
                self.event_queue.push_back(Event::Unit { span: key_span });
                self.event_queue.push_back(Event::EntryEnd);
                self.state = State::ExpectEntry;
                self.handle_boundary_token(t)
            }

            TokenKind::LBrace => {
                if t.span.start == key_span.end {
                    self.event_queue.push_back(Event::Error {
                        span: t.span,
                        kind: ParseErrorKind::MissingWhitespaceBeforeBlock,
                    });
                }
                self.context_stack.push(Context::Object { implicit: false });
                self.state = State::ExpectEntry;
                Some(Event::ObjectStart {
                    span: t.span,
                    separator: Separator::Comma,
                })
            }

            TokenKind::LParen => {
                if t.span.start == key_span.end {
                    self.event_queue.push_back(Event::Error {
                        span: t.span,
                        kind: ParseErrorKind::MissingWhitespaceBeforeBlock,
                    });
                }
                self.context_stack.push(Context::Sequence);
                self.state = State::ExpectSeqElem;
                Some(Event::SequenceStart { span: t.span })
            }

            TokenKind::At => {
                let ev = self.parse_tag_value(t);
                self.state = State::AfterValue;
                Some(ev)
            }

            TokenKind::BareScalar => {
                // Could be simple value or start of attribute chain
                self.state = State::AfterBareValue { value_span: t.span };
                Some(Event::Scalar {
                    span: t.span,
                    value: Cow::Borrowed(t.text),
                    kind: ScalarKind::Bare,
                })
            }

            TokenKind::QuotedScalar => {
                self.state = State::AfterValue;
                Some(Event::Scalar {
                    span: t.span,
                    value: self.unescape_quoted(t.text),
                    kind: ScalarKind::Quoted,
                })
            }

            TokenKind::RawScalar => {
                self.state = State::AfterValue;
                Some(Event::Scalar {
                    span: t.span,
                    value: Cow::Borrowed(Self::strip_raw_delimiters(t.text)),
                    kind: ScalarKind::Raw,
                })
            }

            TokenKind::HeredocStart => {
                let ev = self.parse_heredoc(t.span);
                self.state = State::AfterValue;
                Some(ev)
            }

            TokenKind::LineComment => {
                self.event_queue.push_back(Event::Unit { span: key_span });
                self.event_queue.push_back(Event::EntryEnd);
                self.state = State::ExpectEntry;
                Some(Event::Comment {
                    span: t.span,
                    text: t.text,
                })
            }

            _ => {
                self.state = State::ExpectEntry;
                Some(Event::Error {
                    span: t.span,
                    kind: ParseErrorKind::UnexpectedToken,
                })
            }
        }
    }

    fn step_after_at_key(&mut self, at_span: Span) -> Option<Event<'src>> {
        let t = self.next_token();

        if t.kind == TokenKind::BareScalar && t.span.start == at_span.end {
            // @tagname
            let (tag_name, name_end) = self.extract_tag_name(t.text, t.span.start);

            // Check for invalid key forms
            let has_trailing_at = name_end < t.span.end;
            if has_trailing_at {
                self.state = State::ExpectEntry;
                return Some(Event::Error {
                    span: Span::new(at_span.start, name_end + 1),
                    kind: ParseErrorKind::InvalidKey,
                });
            }

            // Validate tag name
            if tag_name.is_empty() || !Self::is_valid_tag_name(tag_name) {
                self.event_queue.push_back(Event::Error {
                    span: Span::new(t.span.start, name_end),
                    kind: ParseErrorKind::InvalidTagName,
                });
            }

            // Skip @schema at implicit root
            if tag_name == "schema"
                && self.context_stack.last() == Some(&Context::Object { implicit: true })
            {
                self.skip_value();
                self.pending_doc.clear();
                self.state = State::ExpectEntry;
                return None;
            }

            self.event_queue.push_back(Event::Key {
                span: Span::new(at_span.start, name_end),
                tag: Some(tag_name),
                payload: None,
                kind: ScalarKind::Bare,
            });
            self.state = State::AfterKey {
                key_span: Span::new(at_span.start, name_end),
            };
            return None;
        }

        // @ alone = unit key
        self.event_queue.push_back(Event::Key {
            span: at_span,
            tag: None,
            payload: None,
            kind: ScalarKind::Bare,
        });

        // Now handle the token we got as value position
        match t.kind {
            TokenKind::Newline
            | TokenKind::Eof
            | TokenKind::RBrace
            | TokenKind::RParen
            | TokenKind::Comma => {
                self.event_queue.push_back(Event::Unit { span: at_span });
                self.event_queue.push_back(Event::EntryEnd);
                self.state = State::ExpectEntry;
                self.handle_boundary_token(t)
            }

            TokenKind::BareScalar => {
                self.state = State::AfterValue;
                Some(Event::Scalar {
                    span: t.span,
                    value: Cow::Borrowed(t.text),
                    kind: ScalarKind::Bare,
                })
            }

            TokenKind::QuotedScalar => {
                self.state = State::AfterValue;
                Some(Event::Scalar {
                    span: t.span,
                    value: self.unescape_quoted(t.text),
                    kind: ScalarKind::Quoted,
                })
            }

            _ => {
                self.event_queue.push_back(Event::Unit { span: at_span });
                self.event_queue.push_back(Event::EntryEnd);
                self.state = State::ExpectEntry;
                Some(Event::Error {
                    span: t.span,
                    kind: ParseErrorKind::UnexpectedToken,
                })
            }
        }
    }

    fn step_after_key(&mut self, key_span: Span) -> Option<Event<'src>> {
        let t = self.next_token();

        match t.kind {
            TokenKind::Newline
            | TokenKind::Eof
            | TokenKind::RBrace
            | TokenKind::RParen
            | TokenKind::Comma => {
                self.event_queue.push_back(Event::Unit { span: key_span });
                self.event_queue.push_back(Event::EntryEnd);
                self.state = State::ExpectEntry;
                self.handle_boundary_token(t)
            }

            TokenKind::LBrace => {
                self.context_stack.push(Context::Object { implicit: false });
                self.state = State::ExpectEntry;
                Some(Event::ObjectStart {
                    span: t.span,
                    separator: Separator::Comma,
                })
            }

            TokenKind::LParen => {
                self.context_stack.push(Context::Sequence);
                self.state = State::ExpectSeqElem;
                Some(Event::SequenceStart { span: t.span })
            }

            TokenKind::At => {
                let ev = self.parse_tag_value(t);
                self.state = State::AfterValue;
                Some(ev)
            }

            TokenKind::BareScalar => {
                self.state = State::AfterValue;
                Some(Event::Scalar {
                    span: t.span,
                    value: Cow::Borrowed(t.text),
                    kind: ScalarKind::Bare,
                })
            }

            TokenKind::QuotedScalar => {
                self.state = State::AfterValue;
                Some(Event::Scalar {
                    span: t.span,
                    value: self.unescape_quoted(t.text),
                    kind: ScalarKind::Quoted,
                })
            }

            TokenKind::RawScalar => {
                self.state = State::AfterValue;
                Some(Event::Scalar {
                    span: t.span,
                    value: Cow::Borrowed(Self::strip_raw_delimiters(t.text)),
                    kind: ScalarKind::Raw,
                })
            }

            TokenKind::HeredocStart => {
                let ev = self.parse_heredoc(t.span);
                self.state = State::AfterValue;
                Some(ev)
            }

            TokenKind::LineComment => {
                self.event_queue.push_back(Event::Unit { span: key_span });
                self.event_queue.push_back(Event::EntryEnd);
                self.state = State::ExpectEntry;
                Some(Event::Comment {
                    span: t.span,
                    text: t.text,
                })
            }

            _ => {
                self.event_queue.push_back(Event::Unit { span: key_span });
                self.event_queue.push_back(Event::EntryEnd);
                self.state = State::ExpectEntry;
                Some(Event::Error {
                    span: t.span,
                    kind: ParseErrorKind::UnexpectedToken,
                })
            }
        }
    }

    fn step_after_gt(&mut self, key_span: Span, in_chain: bool) -> Option<Event<'src>> {
        // We just saw `key>`, now we expect the value
        let t = self.next_token();

        match t.kind {
            TokenKind::BareScalar => {
                // Emit inner entry for this attribute
                self.event_queue.push_back(Event::EntryStart);
                self.event_queue.push_back(Event::Key {
                    span: key_span,
                    tag: None,
                    payload: Some(Cow::Borrowed(self.span_text(key_span))),
                    kind: ScalarKind::Bare,
                });
                self.event_queue.push_back(Event::Scalar {
                    span: t.span,
                    value: Cow::Borrowed(t.text),
                    kind: ScalarKind::Bare,
                });
                self.event_queue.push_back(Event::EntryEnd);

                if !in_chain {
                    // First attribute - we need to emit ObjectStart
                    self.context_stack.push(Context::AttrObject);
                    self.state = State::MaybeMoreAttr { obj_span: key_span };
                    return Some(Event::ObjectStart {
                        span: key_span,
                        separator: Separator::Comma,
                    });
                }

                self.state = State::MaybeMoreAttr { obj_span: key_span };
                None
            }

            TokenKind::QuotedScalar => {
                self.event_queue.push_back(Event::EntryStart);
                self.event_queue.push_back(Event::Key {
                    span: key_span,
                    tag: None,
                    payload: Some(Cow::Borrowed(self.span_text(key_span))),
                    kind: ScalarKind::Bare,
                });
                self.event_queue.push_back(Event::Scalar {
                    span: t.span,
                    value: self.unescape_quoted(t.text),
                    kind: ScalarKind::Quoted,
                });
                self.event_queue.push_back(Event::EntryEnd);

                if !in_chain {
                    self.context_stack.push(Context::AttrObject);
                    self.state = State::MaybeMoreAttr { obj_span: key_span };
                    return Some(Event::ObjectStart {
                        span: key_span,
                        separator: Separator::Comma,
                    });
                }

                self.state = State::MaybeMoreAttr { obj_span: key_span };
                None
            }

            TokenKind::RawScalar => {
                self.event_queue.push_back(Event::EntryStart);
                self.event_queue.push_back(Event::Key {
                    span: key_span,
                    tag: None,
                    payload: Some(Cow::Borrowed(self.span_text(key_span))),
                    kind: ScalarKind::Bare,
                });
                self.event_queue.push_back(Event::Scalar {
                    span: t.span,
                    value: Cow::Borrowed(Self::strip_raw_delimiters(t.text)),
                    kind: ScalarKind::Raw,
                });
                self.event_queue.push_back(Event::EntryEnd);

                if !in_chain {
                    self.context_stack.push(Context::AttrObject);
                    self.state = State::MaybeMoreAttr { obj_span: key_span };
                    return Some(Event::ObjectStart {
                        span: key_span,
                        separator: Separator::Comma,
                    });
                }

                self.state = State::MaybeMoreAttr { obj_span: key_span };
                None
            }

            TokenKind::LBrace => {
                // key>{...}
                self.event_queue.push_back(Event::EntryStart);
                self.event_queue.push_back(Event::Key {
                    span: key_span,
                    tag: None,
                    payload: Some(Cow::Borrowed(self.span_text(key_span))),
                    kind: ScalarKind::Bare,
                });

                if !in_chain {
                    self.context_stack.push(Context::AttrObject);
                    self.event_queue.push_back(Event::ObjectStart {
                        span: key_span,
                        separator: Separator::Comma,
                    });
                }

                self.context_stack.push(Context::Object { implicit: false });
                self.state = State::ExpectEntry;
                Some(Event::ObjectStart {
                    span: t.span,
                    separator: Separator::Comma,
                })
            }

            TokenKind::LParen => {
                // key>(...)
                self.event_queue.push_back(Event::EntryStart);
                self.event_queue.push_back(Event::Key {
                    span: key_span,
                    tag: None,
                    payload: Some(Cow::Borrowed(self.span_text(key_span))),
                    kind: ScalarKind::Bare,
                });

                if !in_chain {
                    self.context_stack.push(Context::AttrObject);
                    self.event_queue.push_back(Event::ObjectStart {
                        span: key_span,
                        separator: Separator::Comma,
                    });
                }

                self.context_stack.push(Context::Sequence);
                self.state = State::ExpectSeqElem;
                Some(Event::SequenceStart { span: t.span })
            }

            TokenKind::At => {
                // key>@tag
                self.event_queue.push_back(Event::EntryStart);
                self.event_queue.push_back(Event::Key {
                    span: key_span,
                    tag: None,
                    payload: Some(Cow::Borrowed(self.span_text(key_span))),
                    kind: ScalarKind::Bare,
                });

                let tag_ev = self.parse_tag_value(t);
                self.event_queue.push_back(tag_ev);
                self.event_queue.push_back(Event::EntryEnd);

                if !in_chain {
                    self.context_stack.push(Context::AttrObject);
                    self.state = State::MaybeMoreAttr { obj_span: key_span };
                    return Some(Event::ObjectStart {
                        span: key_span,
                        separator: Separator::Comma,
                    });
                }

                self.state = State::MaybeMoreAttr { obj_span: key_span };
                None
            }

            _ => {
                self.event_queue.push_back(Event::Error {
                    span: t.span,
                    kind: ParseErrorKind::ExpectedValue,
                });
                self.event_queue.push_back(Event::EntryEnd);
                self.state = State::ExpectEntry;
                None
            }
        }
    }

    fn step_maybe_more_attr(&mut self, obj_span: Span) -> Option<Event<'src>> {
        let t = self.next_token();

        match t.kind {
            TokenKind::BareScalar => {
                // Could be another attribute or TooManyAtoms
                // Need next token to decide
                self.state = State::AfterBareKeyInAttr {
                    key_span: t.span,
                    obj_span,
                };
                None
            }

            TokenKind::Newline
            | TokenKind::Eof
            | TokenKind::RBrace
            | TokenKind::RParen
            | TokenKind::Comma => {
                // End of attribute chain
                self.close_attr_obj(obj_span);
                self.handle_boundary_token(t)
            }

            TokenKind::LineComment => {
                self.close_attr_obj(obj_span);
                Some(Event::Comment {
                    span: t.span,
                    text: t.text,
                })
            }

            _ => {
                // TooManyAtoms
                self.event_queue.push_back(Event::Error {
                    span: t.span,
                    kind: ParseErrorKind::TooManyAtoms,
                });
                let boundary = self.skip_to_boundary();
                self.close_attr_obj(obj_span);
                self.handle_boundary_token(boundary)
            }
        }
    }

    fn step_after_bare_key_in_attr(
        &mut self,
        key_span: Span,
        obj_span: Span,
    ) -> Option<Event<'src>> {
        let t = self.next_token();

        match t.kind {
            TokenKind::Gt if t.span.start == key_span.end => {
                // Another attribute!
                self.state = State::AfterGt {
                    key_span,
                    in_chain: true,
                };
                None
            }

            // Not `>` immediately after - this is TooManyAtoms
            _ => {
                self.event_queue.push_back(Event::Error {
                    span: key_span,
                    kind: ParseErrorKind::TooManyAtoms,
                });
                let boundary = self.skip_to_boundary();
                self.close_attr_obj(obj_span);
                self.handle_boundary_token(boundary)
            }
        }
    }

    fn step_after_bare_value(&mut self, value_span: Span) -> Option<Event<'src>> {
        let t = self.next_token();

        match t.kind {
            TokenKind::Gt if t.span.start == value_span.end => {
                // The bare scalar we emitted was actually an attribute key!
                // We already emitted Scalar - that's now being reinterpreted.
                // Emit ObjectStart, then the inner entry structure.
                self.context_stack.push(Context::AttrObject);
                self.event_queue.push_back(Event::EntryStart);
                self.event_queue.push_back(Event::Key {
                    span: value_span,
                    tag: None,
                    payload: Some(Cow::Borrowed(self.span_text(value_span))),
                    kind: ScalarKind::Bare,
                });
                self.state = State::AfterGt {
                    key_span: value_span,
                    in_chain: true,
                };
                Some(Event::ObjectStart {
                    span: value_span,
                    separator: Separator::Comma,
                })
            }

            // Normal boundary - end entry
            TokenKind::Newline
            | TokenKind::Eof
            | TokenKind::RBrace
            | TokenKind::RParen
            | TokenKind::Comma => {
                self.event_queue.push_back(Event::EntryEnd);
                self.state = State::ExpectEntry;
                self.handle_boundary_token(t)
            }

            TokenKind::LineComment => {
                self.event_queue.push_back(Event::EntryEnd);
                self.state = State::ExpectEntry;
                Some(Event::Comment {
                    span: t.span,
                    text: t.text,
                })
            }

            // Extra atom - TooManyAtoms
            _ => {
                self.event_queue.push_back(Event::Error {
                    span: t.span,
                    kind: ParseErrorKind::TooManyAtoms,
                });
                let boundary = self.skip_to_boundary();
                self.event_queue.push_back(Event::EntryEnd);
                self.state = State::ExpectEntry;
                self.handle_boundary_token(boundary)
            }
        }
    }

    fn step_after_value(&mut self) -> Option<Event<'src>> {
        let t = self.next_token();

        match t.kind {
            TokenKind::Newline
            | TokenKind::Eof
            | TokenKind::RBrace
            | TokenKind::RParen
            | TokenKind::Comma => {
                self.event_queue.push_back(Event::EntryEnd);
                self.state = State::ExpectEntry;
                self.handle_boundary_token(t)
            }

            TokenKind::LineComment => {
                self.event_queue.push_back(Event::EntryEnd);
                self.state = State::ExpectEntry;
                Some(Event::Comment {
                    span: t.span,
                    text: t.text,
                })
            }

            // TooManyAtoms
            _ => {
                self.event_queue.push_back(Event::Error {
                    span: t.span,
                    kind: ParseErrorKind::TooManyAtoms,
                });
                let boundary = self.skip_to_boundary();
                self.event_queue.push_back(Event::EntryEnd);
                self.state = State::ExpectEntry;
                self.handle_boundary_token(boundary)
            }
        }
    }

    fn step_expect_seq_elem(&mut self) -> Option<Event<'src>> {
        let t = self.next_token_skip_newlines();

        match t.kind {
            TokenKind::RParen => {
                self.context_stack.pop();
                self.state = self.state_after_close();
                Some(Event::SequenceEnd { span: t.span })
            }

            TokenKind::Eof => {
                self.event_queue.push_back(Event::Error {
                    span: t.span,
                    kind: ParseErrorKind::UnclosedSequence,
                });
                self.close_at_eof()
            }

            TokenKind::RBrace => Some(Event::Error {
                span: t.span,
                kind: ParseErrorKind::UnexpectedToken,
            }),

            TokenKind::BareScalar => Some(Event::Scalar {
                span: t.span,
                value: Cow::Borrowed(t.text),
                kind: ScalarKind::Bare,
            }),

            TokenKind::QuotedScalar => Some(Event::Scalar {
                span: t.span,
                value: self.unescape_quoted(t.text),
                kind: ScalarKind::Quoted,
            }),

            TokenKind::RawScalar => Some(Event::Scalar {
                span: t.span,
                value: Cow::Borrowed(Self::strip_raw_delimiters(t.text)),
                kind: ScalarKind::Raw,
            }),

            TokenKind::HeredocStart => Some(self.parse_heredoc(t.span)),

            TokenKind::LBrace => {
                self.context_stack.push(Context::Object { implicit: false });
                self.state = State::ExpectEntry;
                Some(Event::ObjectStart {
                    span: t.span,
                    separator: Separator::Comma,
                })
            }

            TokenKind::LParen => {
                self.context_stack.push(Context::Sequence);
                Some(Event::SequenceStart { span: t.span })
            }

            TokenKind::At => Some(self.parse_tag_value(t)),

            TokenKind::LineComment => Some(Event::Comment {
                span: t.span,
                text: t.text,
            }),

            _ => None,
        }
    }

    // === Helpers ===

    fn handle_boundary_token(&mut self, t: Token<'src>) -> Option<Event<'src>> {
        match t.kind {
            TokenKind::RBrace => self.handle_rbrace(t.span),
            TokenKind::RParen => Some(Event::Error {
                span: t.span,
                kind: ParseErrorKind::UnexpectedToken,
            }),
            TokenKind::Eof => self.close_at_eof(),
            _ => None,
        }
    }

    fn close_at_eof(&mut self) -> Option<Event<'src>> {
        if let Some((span, _)) = self.pending_doc.first() {
            let span = *span;
            self.pending_doc.clear();
            self.event_queue.push_back(Event::Error {
                span,
                kind: ParseErrorKind::DanglingDocComment,
            });
        }

        if let Some(ctx) = self.context_stack.pop() {
            match ctx {
                Context::Object { implicit } => {
                    if !implicit {
                        self.event_queue.push_back(Event::Error {
                            span: Span::new(0, 0),
                            kind: ParseErrorKind::UnclosedObject,
                        });
                    }
                    if self.context_stack.is_empty() {
                        self.event_queue.push_back(Event::DocumentEnd);
                        self.state = State::Done;
                    }
                    return Some(Event::ObjectEnd {
                        span: Span::new(self.input.len() as u32, self.input.len() as u32),
                    });
                }
                Context::Sequence => {
                    self.event_queue.push_back(Event::Error {
                        span: Span::new(0, 0),
                        kind: ParseErrorKind::UnclosedSequence,
                    });
                    if self.context_stack.is_empty() {
                        self.event_queue.push_back(Event::DocumentEnd);
                        self.state = State::Done;
                    }
                    return Some(Event::SequenceEnd {
                        span: Span::new(self.input.len() as u32, self.input.len() as u32),
                    });
                }
                Context::AttrObject => {
                    self.event_queue.push_back(Event::ObjectEnd {
                        span: Span::new(self.input.len() as u32, self.input.len() as u32),
                    });
                    self.event_queue.push_back(Event::EntryEnd);
                    return self.close_at_eof();
                }
            }
        }

        self.state = State::Done;
        Some(Event::DocumentEnd)
    }

    fn handle_rbrace(&mut self, span: Span) -> Option<Event<'src>> {
        if let Some((doc_span, _)) = self.pending_doc.first() {
            let doc_span = *doc_span;
            self.pending_doc.clear();
            self.event_queue.push_back(Event::Error {
                span: doc_span,
                kind: ParseErrorKind::DanglingDocComment,
            });
        }

        match self.context_stack.pop() {
            Some(Context::Object { implicit: false }) => {
                self.state = self.state_after_close();
                Some(Event::ObjectEnd { span })
            }
            Some(ctx) => {
                self.context_stack.push(ctx);
                Some(Event::Error {
                    span,
                    kind: ParseErrorKind::UnexpectedToken,
                })
            }
            None => Some(Event::Error {
                span,
                kind: ParseErrorKind::UnexpectedToken,
            }),
        }
    }

    fn close_attr_obj(&mut self, obj_span: Span) {
        self.context_stack.pop(); // AttrObject
        self.event_queue
            .push_back(Event::ObjectEnd { span: obj_span });
        self.event_queue.push_back(Event::EntryEnd);
        self.state = State::ExpectEntry;
    }

    fn state_after_close(&self) -> State {
        match self.context_stack.last() {
            Some(Context::Object { .. }) => State::AfterValue,
            Some(Context::Sequence) => State::ExpectSeqElem,
            Some(Context::AttrObject) => State::MaybeMoreAttr {
                obj_span: Span::new(0, 0),
            },
            None => State::Done,
        }
    }

    fn parse_tag_value(&mut self, at_token: Token<'src>) -> Event<'src> {
        let t = self.next_token();

        if t.kind == TokenKind::BareScalar && t.span.start == at_token.span.end {
            let (tag_name, name_end) = self.extract_tag_name(t.text, t.span.start);
            let has_trailing_at = name_end < t.span.end;

            if tag_name.is_empty() || !Self::is_valid_tag_name(tag_name) {
                self.event_queue.push_back(Event::Error {
                    span: Span::new(t.span.start, name_end),
                    kind: ParseErrorKind::InvalidTagName,
                });
            }

            if has_trailing_at {
                self.event_queue.push_back(Event::Unit {
                    span: Span::new(name_end, name_end + 1),
                });
                self.event_queue.push_back(Event::TagEnd);
                return Event::TagStart {
                    span: Span::new(at_token.span.start, name_end + 1),
                    name: tag_name,
                };
            }

            // Check for payload - need to look at next token
            let next = self.next_token();
            if next.span.start == name_end {
                match next.kind {
                    TokenKind::LBrace => {
                        self.context_stack.push(Context::Object { implicit: false });
                        self.event_queue.push_back(Event::ObjectStart {
                            span: next.span,
                            separator: Separator::Comma,
                        });
                        self.state = State::ExpectEntry;
                        return Event::TagStart {
                            span: Span::new(at_token.span.start, name_end),
                            name: tag_name,
                        };
                    }
                    TokenKind::LParen => {
                        self.context_stack.push(Context::Sequence);
                        self.event_queue
                            .push_back(Event::SequenceStart { span: next.span });
                        self.state = State::ExpectSeqElem;
                        return Event::TagStart {
                            span: Span::new(at_token.span.start, name_end),
                            name: tag_name,
                        };
                    }
                    TokenKind::QuotedScalar => {
                        self.event_queue.push_back(Event::Scalar {
                            span: next.span,
                            value: self.unescape_quoted(next.text),
                            kind: ScalarKind::Quoted,
                        });
                        self.event_queue.push_back(Event::TagEnd);
                        return Event::TagStart {
                            span: Span::new(at_token.span.start, name_end),
                            name: tag_name,
                        };
                    }
                    TokenKind::RawScalar => {
                        self.event_queue.push_back(Event::Scalar {
                            span: next.span,
                            value: Cow::Borrowed(Self::strip_raw_delimiters(next.text)),
                            kind: ScalarKind::Raw,
                        });
                        self.event_queue.push_back(Event::TagEnd);
                        return Event::TagStart {
                            span: Span::new(at_token.span.start, name_end),
                            name: tag_name,
                        };
                    }
                    _ => {
                        // Not adjacent payload - implicit unit
                        // But we consumed a token we shouldn't have!
                        // This is the ONE place we need to handle this
                        self.event_queue.push_back(Event::Unit {
                            span: Span::new(name_end, name_end),
                        });
                        self.event_queue.push_back(Event::TagEnd);
                        // Handle the consumed token based on current context
                        self.handle_consumed_after_tag(next);
                        return Event::TagStart {
                            span: Span::new(at_token.span.start, name_end),
                            name: tag_name,
                        };
                    }
                }
            }

            // Not adjacent - implicit unit, handle consumed token
            self.event_queue.push_back(Event::Unit {
                span: Span::new(name_end, name_end),
            });
            self.event_queue.push_back(Event::TagEnd);
            self.handle_consumed_after_tag(next);
            return Event::TagStart {
                span: Span::new(at_token.span.start, name_end),
                name: tag_name,
            };
        }

        // @ alone - unit, but we consumed a token
        self.handle_consumed_after_tag(t);
        Event::Unit {
            span: at_token.span,
        }
    }

    fn handle_consumed_after_tag(&mut self, t: Token<'src>) {
        // We consumed a token after tag that we didn't use for the tag
        // Queue it as an event or error based on context
        match t.kind {
            TokenKind::Newline | TokenKind::Comma => {
                // Fine, just ends the entry
            }
            TokenKind::Eof => {
                self.event_queue.push_back(Event::EntryEnd);
                // Will trigger close_at_eof on next step
            }
            TokenKind::RBrace => {
                self.event_queue.push_back(Event::EntryEnd);
                if let Some(ev) = self.handle_rbrace(t.span) {
                    self.event_queue.push_back(ev);
                }
            }
            TokenKind::RParen => {
                self.event_queue.push_back(Event::EntryEnd);
                self.event_queue.push_back(Event::Error {
                    span: t.span,
                    kind: ParseErrorKind::UnexpectedToken,
                });
            }
            _ => {
                // TooManyAtoms
                self.event_queue.push_back(Event::Error {
                    span: t.span,
                    kind: ParseErrorKind::TooManyAtoms,
                });
                let boundary = self.skip_to_boundary();
                if let Some(ev) = self.handle_boundary_token(boundary) {
                    self.event_queue.push_back(ev);
                }
            }
        }
    }

    fn emit_pending_docs(&mut self) {
        for (span, text) in std::mem::take(&mut self.pending_doc) {
            self.event_queue.push_back(Event::DocComment { span, text });
        }
    }

    fn extract_tag_name<'a>(&self, text: &'a str, start: u32) -> (&'a str, u32) {
        let len = text.find('@').unwrap_or(text.len());
        (&text[..len], start + len as u32)
    }

    fn parse_heredoc(&mut self, start_span: Span) -> Event<'src> {
        let mut content = String::new();
        let mut end_span = start_span;
        loop {
            let t = self.lexer.next_token();
            match t.kind {
                TokenKind::HeredocContent => content.push_str(t.text),
                TokenKind::HeredocEnd => {
                    end_span = t.span;
                    break;
                }
                _ => break,
            }
        }
        Event::Scalar {
            span: Span::new(start_span.start, end_span.end),
            value: Cow::Owned(content),
            kind: ScalarKind::Heredoc,
        }
    }

    fn skip_heredoc(&mut self) {
        loop {
            let t = self.lexer.next_token();
            match t.kind {
                TokenKind::HeredocContent => {}
                TokenKind::HeredocEnd | TokenKind::Eof => break,
                _ => break,
            }
        }
    }

    fn skip_to_boundary(&mut self) -> Token<'src> {
        loop {
            let t = self.next_token();
            match t.kind {
                TokenKind::Newline
                | TokenKind::Eof
                | TokenKind::RBrace
                | TokenKind::RParen
                | TokenKind::Comma => return t,
                TokenKind::LBrace => self.skip_nested(TokenKind::RBrace),
                TokenKind::LParen => self.skip_nested(TokenKind::RParen),
                _ => {}
            }
        }
    }

    fn skip_nested(&mut self, closing: TokenKind) {
        let mut depth = 1;
        while depth > 0 {
            let t = self.lexer.next_token();
            match t.kind {
                TokenKind::LBrace | TokenKind::LParen => depth += 1,
                k if k == closing => depth -= 1,
                TokenKind::Eof => break,
                _ => {}
            }
        }
    }

    fn skip_value(&mut self) {
        let mut depth = 0i32;
        loop {
            let t = self.next_token();
            match t.kind {
                TokenKind::LBrace | TokenKind::LParen => depth += 1,
                TokenKind::RBrace | TokenKind::RParen if depth > 0 => depth -= 1,
                TokenKind::RBrace | TokenKind::RParen => break,
                TokenKind::Newline | TokenKind::Comma if depth == 0 => break,
                TokenKind::Eof => break,
                _ if depth == 0 => break,
                _ => {}
            }
        }
    }

    // === String processing ===

    fn unescape_quoted(&self, text: &'src str) -> Cow<'src, str> {
        let inner = if text.starts_with('"') && text.ends_with('"') && text.len() >= 2 {
            &text[1..text.len() - 1]
        } else {
            text
        };

        if !inner.contains('\\') {
            return Cow::Borrowed(inner);
        }

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
                        if chars.peek() == Some(&'{') {
                            chars.next();
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
                            let mut hex = String::with_capacity(4);
                            for _ in 0..4 {
                                if let Some(&c) = chars.peek() {
                                    if c.is_ascii_hexdigit() {
                                        hex.push(chars.next().unwrap());
                                    } else {
                                        break;
                                    }
                                }
                            }
                            if hex.len() == 4 {
                                if let Ok(code) = u32::from_str_radix(&hex, 16) {
                                    if let Some(ch) = char::from_u32(code) {
                                        result.push(ch);
                                    }
                                }
                            } else {
                                result.push_str("\\u");
                                result.push_str(&hex);
                            }
                        }
                    }
                    Some(c) => {
                        result.push('\\');
                        result.push(c);
                    }
                    None => result.push('\\'),
                }
            } else {
                result.push(c);
            }
        }

        Cow::Owned(result)
    }

    fn is_valid_tag_name(name: &str) -> bool {
        let mut chars = name.chars();
        match chars.next() {
            Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
            _ => return false,
        }
        chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    }

    fn strip_raw_delimiters(text: &str) -> &str {
        let after_r = text.strip_prefix('r').unwrap_or(text);
        let hash_count = after_r.chars().take_while(|&c| c == '#').count();
        let after_hashes = &after_r[hash_count..];
        let after_quote = after_hashes.strip_prefix('"').unwrap_or(after_hashes);
        let closing_len = 1 + hash_count;
        if after_quote.len() >= closing_len {
            &after_quote[..after_quote.len() - closing_len]
        } else {
            after_quote
        }
    }

    pub fn input(&self) -> &'src str {
        self.input
    }

    pub fn parse_to_vec(mut self) -> Vec<Event<'src>> {
        let mut events = Vec::new();
        while let Some(ev) = self.next_event() {
            events.push(ev);
        }
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use facet_testhelpers::test;
    use styx_testhelpers::{ActualError, assert_annotated_errors, source_without_annotations};

    fn parse(source: &str) -> Vec<Event<'_>> {
        Parser2::new(source).parse_to_vec()
    }

    fn error_kind_name(kind: &ParseErrorKind) -> &'static str {
        match kind {
            ParseErrorKind::UnexpectedToken => "UnexpectedToken",
            ParseErrorKind::UnclosedObject => "UnclosedObject",
            ParseErrorKind::UnclosedSequence => "UnclosedSequence",
            ParseErrorKind::MixedSeparators => "MixedSeparators",
            ParseErrorKind::InvalidEscape(_) => "InvalidEscape",
            ParseErrorKind::ExpectedKey => "ExpectedKey",
            ParseErrorKind::ExpectedValue => "ExpectedValue",
            ParseErrorKind::UnexpectedEof => "UnexpectedEof",
            ParseErrorKind::DuplicateKey { .. } => "DuplicateKey",
            ParseErrorKind::InvalidTagName => "InvalidTagName",
            ParseErrorKind::InvalidKey => "InvalidKey",
            ParseErrorKind::DanglingDocComment => "DanglingDocComment",
            ParseErrorKind::TooManyAtoms => "TooManyAtoms",
            ParseErrorKind::ReopenedPath { .. } => "ReopenedPath",
            ParseErrorKind::NestIntoTerminal { .. } => "NestIntoTerminal",
            ParseErrorKind::CommaInSequence => "CommaInSequence",
            ParseErrorKind::MissingWhitespaceBeforeBlock => "MissingWhitespaceBeforeBlock",
        }
    }

    fn assert_parse_errors(annotated_source: &str) {
        let source = source_without_annotations(annotated_source);
        let events = parse(&source);
        let actual_errors: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                Event::Error { span, kind } => Some(ActualError {
                    span: (*span).into(),
                    kind: error_kind_name(kind).to_string(),
                }),
                _ => None,
            })
            .collect();
        assert_annotated_errors(annotated_source, actual_errors);
    }

    #[test]
    fn test_empty_document() {
        let events = parse("");
        assert!(events.contains(&Event::DocumentStart));
        assert!(events.contains(&Event::DocumentEnd));
    }

    #[test]
    fn test_simple_entry() {
        let events = parse("foo bar");
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::Key { payload: Some(v), .. } if v == "foo"))
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::Scalar { value, .. } if value == "bar"))
        );
    }

    #[test]
    fn test_key_only() {
        let events = parse("foo");
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::Key { payload: Some(v), .. } if v == "foo"))
        );
        assert!(events.iter().any(|e| matches!(e, Event::Unit { .. })));
    }

    #[test]
    fn test_multiple_entries() {
        let events = parse("foo bar\nbaz qux");
        let keys: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                Event::Key {
                    payload: Some(v), ..
                } => Some(v.as_ref()),
                _ => None,
            })
            .collect();
        assert_eq!(keys, vec!["foo", "baz"]);
    }

    #[test]
    fn test_quoted_string() {
        let events = parse(r#"name "hello world""#);
        assert!(events
            .iter()
            .any(|e| matches!(e, Event::Scalar { value, kind: ScalarKind::Quoted, .. } if value == "hello world")));
    }

    #[test]
    fn test_quoted_escape() {
        let events = parse(r#"msg "hello\nworld""#);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::Scalar { value, .. } if value == "hello\nworld"))
        );
    }

    #[test]
    fn test_too_many_atoms() {
        assert_parse_errors(
            r#"
a b c
    ^ TooManyAtoms
"#,
        );
    }

    #[test]
    fn test_too_many_atoms_in_object() {
        assert_parse_errors(
            r#"
{label ": BIGINT" line 4}
                  ^^^^ TooManyAtoms
"#,
        );
    }

    #[test]
    fn test_unit_value() {
        let events = parse("flag @");
        assert!(events.iter().any(|e| matches!(e, Event::Unit { .. })));
    }

    #[test]
    fn test_unit_key() {
        let events = parse("@ value");
        assert!(events.iter().any(|e| matches!(
            e,
            Event::Key {
                payload: None,
                tag: None,
                ..
            }
        )));
    }

    #[test]
    fn test_tag() {
        let events = parse("type @user");
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::TagStart { name, .. } if *name == "user"))
        );
    }

    #[test]
    fn test_comments() {
        let events = parse("// comment\nfoo bar");
        assert!(events.iter().any(|e| matches!(e, Event::Comment { .. })));
    }

    #[test]
    fn test_doc_comments() {
        let events = parse("/// doc\nfoo bar");
        assert!(events.iter().any(|e| matches!(e, Event::DocComment { .. })));
    }

    #[test]
    fn test_doc_comment_at_eof_error() {
        assert_parse_errors(
            r#"
foo bar
/// dangling
^^^^^^^^^^^^ DanglingDocComment
"#,
        );
    }

    #[test]
    fn test_nested_object() {
        let events = parse("outer {inner {x 1}}");
        let obj_starts = events
            .iter()
            .filter(|e| matches!(e, Event::ObjectStart { .. }))
            .count();
        assert!(obj_starts >= 2);
    }

    #[test]
    fn test_sequence_elements() {
        let events = parse("items (a b c)");
        let scalars: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                Event::Scalar { value, .. } => Some(value.as_ref()),
                _ => None,
            })
            .collect();
        assert!(scalars.contains(&"a"));
        assert!(scalars.contains(&"b"));
        assert!(scalars.contains(&"c"));
    }

    #[test]
    fn test_tagged_object() {
        let events = parse("result @err{message oops}");
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::TagStart { name, .. } if *name == "err"))
        );
    }

    #[test]
    fn test_tagged_explicit_unit() {
        let events = parse("nothing @empty@");
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::TagStart { name, .. } if *name == "empty"))
        );
    }

    #[test]
    fn test_simple_attribute() {
        let events = parse("server host>localhost");
        let keys: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                Event::Key {
                    payload: Some(v), ..
                } => Some(v.as_ref()),
                _ => None,
            })
            .collect();
        assert!(keys.contains(&"server"));
        assert!(keys.contains(&"host"));
    }

    #[test]
    fn test_multiple_attributes() {
        let events = parse("server host>localhost port>8080");
        let keys: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                Event::Key {
                    payload: Some(v), ..
                } => Some(v.as_ref()),
                _ => None,
            })
            .collect();
        assert!(keys.contains(&"server"));
        assert!(keys.contains(&"host"));
        assert!(keys.contains(&"port"));
    }

    #[test]
    fn test_attribute_with_object_value() {
        let events = parse("config opts>{x 1}");
        let keys: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                Event::Key {
                    payload: Some(v), ..
                } => Some(v.as_ref()),
                _ => None,
            })
            .collect();
        assert!(keys.contains(&"config"));
        assert!(keys.contains(&"opts"));
        assert!(keys.contains(&"x"));
    }

    #[test]
    fn test_attribute_with_sequence_value() {
        let events = parse("config tags>(a b c)");
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::SequenceStart { .. }))
        );
    }

    #[test]
    fn test_attribute_with_tag_value() {
        let events = parse("config status>@ok");
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::TagStart { name, .. } if *name == "ok"))
        );
    }

    #[test]
    fn test_tag_with_dot_invalid() {
        assert_parse_errors(
            r#"
@Some.Type
 ^^^^^^^^^ InvalidTagName
"#,
        );
    }

    #[test]
    fn test_invalid_tag_name_starts_with_digit() {
        assert_parse_errors(
            r#"
x @123
   ^^^ InvalidTagName
"#,
        );
    }

    #[test]
    fn test_unicode_escape_braces() {
        let events = parse(r#"x "\u{1F600}""#);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::Scalar { value, .. } if value == "😀"))
        );
    }

    #[test]
    fn test_unicode_escape_4digit() {
        let events = parse(r#"x "\u0041""#);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::Scalar { value, .. } if value == "A"))
        );
    }

    #[test]
    fn test_heredoc_key_rejected() {
        assert_parse_errors(
            r#"
<<EOF
^^^^^^ InvalidKey
key
EOF value
"#,
        );
    }

    #[test]
    fn test_missing_comma_rejected() {
        assert_parse_errors(
            r#"
{server {host localhost port 8080}}
                        ^^^^ TooManyAtoms
"#,
        );
    }

    #[test]
    fn test_bare_scalar_is_string() {
        let events = parse("port 8080");
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::Scalar { value, .. } if value == "8080"))
        );
    }

    #[test]
    fn test_bool_like_is_string() {
        let events = parse("enabled true");
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::Scalar { value, .. } if value == "true"))
        );
    }
}
