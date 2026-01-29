//! Pull-based streaming parser for Styx.
//!
//! State machine design: consume token, transition state, emit event.
//! No lookahead gymnastics, no peeked token hell.

use std::borrow::Cow;
use std::collections::VecDeque;

use crate::event::{Event, ParseErrorKind, ScalarKind, Separator};
use crate::lexer::Lexer;
use crate::span::Span;
use crate::token::{Token, TokenKind};

#[allow(unused_imports)]
use crate::trace;

/// Parser state - stores spans/positions, NOT string references.
#[derive(Debug, Clone, PartialEq)]
enum State {
    Start,
    BeforeRoot,
    ExpectingEntry,
    /// After bare key - check for `>` (attribute syntax).
    AfterBareKey {
        key_span: Span,
    },
    /// After any key - expecting value.
    AfterKey {
        key_span: Span,
    },
    /// After value - check for TooManyAtoms.
    AfterValue,
    /// In attribute chain.
    InAttributeChain {
        obj_start_span: Span,
    },
    ExpectingSequenceElement,
    Done,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Context {
    Object { implicit: bool },
    Sequence,
    AttributeObject,
}

/// Pull-based streaming parser for Styx documents.
pub struct Parser2<'src> {
    input: &'src str,
    lexer: Lexer<'src>,
    state: State,
    context_stack: Vec<Context>,
    event_queue: VecDeque<Event<'src>>,
    pending_doc: Vec<(Span, &'src str)>,
    expr_mode: bool,
    /// Buffered tokens for "unread" functionality.
    buffered_tokens: VecDeque<Token<'src>>,
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
            buffered_tokens: VecDeque::new(),
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
            buffered_tokens: VecDeque::new(),
        }
    }

    pub fn next_event(&mut self) -> Option<Event<'src>> {
        // Drain queue first
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
            State::ExpectingEntry => self.step_expecting_entry(),
            State::AfterBareKey { key_span } => self.step_after_bare_key(key_span),
            State::AfterKey { key_span } => self.step_after_key(key_span),
            State::AfterValue => self.step_after_value(),
            State::InAttributeChain { obj_start_span } => {
                self.step_in_attribute_chain(obj_start_span)
            }
            State::ExpectingSequenceElement => self.step_expecting_sequence_element(),
            State::Done => None,
        }
    }

    // === Token access ===

    fn next_token(&mut self) -> Token<'src> {
        if let Some(t) = self.buffered_tokens.pop_front() {
            return t;
        }
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

    fn unread(&mut self, t: Token<'src>) {
        self.buffered_tokens.push_front(t);
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
                if self.expr_mode {
                    return Some(Event::DocumentEnd);
                }
                Some(Event::ObjectStart {
                    span: Span::new(0, 0),
                    separator: Separator::Newline,
                })
            }

            TokenKind::LineComment => Some(Event::Comment {
                span: t.span,
                text: t.text,
            }),

            TokenKind::DocComment => {
                self.pending_doc.push((t.span, t.text));
                None
            }

            TokenKind::Newline => None,

            TokenKind::LBrace => {
                self.context_stack.push(Context::Object { implicit: false });
                self.state = State::ExpectingEntry;
                Some(Event::ObjectStart {
                    span: t.span,
                    separator: Separator::Comma,
                })
            }

            _ => {
                // Implicit root
                self.context_stack.push(Context::Object { implicit: true });
                self.unread(t);
                self.state = State::ExpectingEntry;
                Some(Event::ObjectStart {
                    span: Span::new(0, 0),
                    separator: Separator::Newline,
                })
            }
        }
    }

    fn step_expecting_entry(&mut self) -> Option<Event<'src>> {
        let t = self.next_token_skip_newlines();

        match t.kind {
            TokenKind::Eof => self.close_contexts_at_eof(),

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
                let val = self.unescape_quoted(t.text);
                self.event_queue.push_back(Event::Key {
                    span: t.span,
                    tag: None,
                    payload: Some(val),
                    kind: ScalarKind::Quoted,
                });
                self.state = State::AfterKey { key_span: t.span };
                Some(Event::EntryStart)
            }

            TokenKind::RawScalar => {
                self.emit_pending_docs();
                let val = Self::strip_raw_delimiters(t.text);
                self.event_queue.push_back(Event::Key {
                    span: t.span,
                    tag: None,
                    payload: Some(Cow::Borrowed(val)),
                    kind: ScalarKind::Raw,
                });
                self.state = State::AfterKey { key_span: t.span };
                Some(Event::EntryStart)
            }

            TokenKind::At => self.handle_at_as_key(t.span),

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

        // Check for attribute syntax: immediate `>`
        if t.kind == TokenKind::Gt && t.span.start == key_span.end {
            return self.parse_attribute_value(key_span);
        }

        // Not attribute - continue as normal key
        self.unread(t);
        self.state = State::AfterKey { key_span };
        None
    }

    fn step_after_key(&mut self, key_span: Span) -> Option<Event<'src>> {
        let t = self.next_token();

        match t.kind {
            TokenKind::Newline
            | TokenKind::Eof
            | TokenKind::RBrace
            | TokenKind::RParen
            | TokenKind::Comma => {
                // Unit value
                self.event_queue.push_back(Event::Unit { span: key_span });
                self.event_queue.push_back(Event::EntryEnd);
                self.unread(t);
                self.state = State::ExpectingEntry;
                None
            }

            TokenKind::LBrace => {
                // Check MissingWhitespaceBeforeBlock
                if t.span.start == key_span.end {
                    self.event_queue.push_back(Event::Error {
                        span: t.span,
                        kind: ParseErrorKind::MissingWhitespaceBeforeBlock,
                    });
                }
                self.context_stack.push(Context::Object { implicit: false });
                self.state = State::ExpectingEntry;
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
                self.state = State::ExpectingSequenceElement;
                Some(Event::SequenceStart { span: t.span })
            }

            TokenKind::At => {
                let ev = self.parse_tag_value(t.span);
                self.state = State::AfterValue;
                Some(ev)
            }

            TokenKind::BareScalar => {
                // Check for attribute syntax in value position
                let next = self.next_token();
                if next.kind == TokenKind::Gt && next.span.start == t.span.end {
                    // key value>... is attribute syntax as value
                    return self.parse_attribute_chain_as_value(t.span);
                }
                self.unread(next);
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
                // Comment after key = unit value
                self.event_queue.push_back(Event::Unit { span: key_span });
                self.event_queue.push_back(Event::EntryEnd);
                self.state = State::ExpectingEntry;
                Some(Event::Comment {
                    span: t.span,
                    text: t.text,
                })
            }

            _ => {
                self.event_queue.push_back(Event::Unit { span: key_span });
                self.event_queue.push_back(Event::EntryEnd);
                self.state = State::ExpectingEntry;
                Some(Event::Error {
                    span: t.span,
                    kind: ParseErrorKind::UnexpectedToken,
                })
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
                self.unread(t);
                self.state = State::ExpectingEntry;
                None
            }

            TokenKind::LineComment => {
                self.event_queue.push_back(Event::EntryEnd);
                self.state = State::ExpectingEntry;
                Some(Event::Comment {
                    span: t.span,
                    text: t.text,
                })
            }

            // Extra atom = TooManyAtoms
            TokenKind::BareScalar
            | TokenKind::QuotedScalar
            | TokenKind::RawScalar
            | TokenKind::LBrace
            | TokenKind::LParen
            | TokenKind::At
            | TokenKind::HeredocStart => {
                self.event_queue.push_back(Event::Error {
                    span: t.span,
                    kind: ParseErrorKind::TooManyAtoms,
                });
                self.skip_to_entry_boundary();
                self.event_queue.push_back(Event::EntryEnd);
                self.state = State::ExpectingEntry;
                None
            }

            _ => {
                self.event_queue.push_back(Event::EntryEnd);
                self.state = State::ExpectingEntry;
                None
            }
        }
    }

    fn step_in_attribute_chain(&mut self, obj_start_span: Span) -> Option<Event<'src>> {
        let t = self.next_token();

        match t.kind {
            TokenKind::BareScalar => {
                // Check if followed by >
                let next = self.next_token();
                if next.kind == TokenKind::Gt && next.span.start == t.span.end {
                    // Another attribute
                    self.event_queue.push_back(Event::EntryStart);
                    self.event_queue.push_back(Event::Key {
                        span: t.span,
                        tag: None,
                        payload: Some(Cow::Borrowed(t.text)),
                        kind: ScalarKind::Bare,
                    });

                    let val = self.next_token();
                    let val_ev = self.token_to_scalar(val);
                    self.event_queue.push_back(val_ev);
                    self.event_queue.push_back(Event::EntryEnd);

                    self.state = State::InAttributeChain { obj_start_span };
                    return None;
                }

                // Not attribute - this is TooManyAtoms
                self.unread(next);
                self.event_queue.push_back(Event::Error {
                    span: t.span,
                    kind: ParseErrorKind::TooManyAtoms,
                });
                self.skip_to_entry_boundary();
                self.close_attribute_chain();
                None
            }

            TokenKind::Newline
            | TokenKind::Eof
            | TokenKind::RBrace
            | TokenKind::RParen
            | TokenKind::Comma => {
                self.unread(t);
                self.close_attribute_chain();
                None
            }

            TokenKind::LineComment => {
                self.close_attribute_chain();
                Some(Event::Comment {
                    span: t.span,
                    text: t.text,
                })
            }

            _ => {
                self.unread(t);
                self.close_attribute_chain();
                None
            }
        }
    }

    fn step_expecting_sequence_element(&mut self) -> Option<Event<'src>> {
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
                self.close_contexts_at_eof()
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
                self.state = State::ExpectingEntry;
                Some(Event::ObjectStart {
                    span: t.span,
                    separator: Separator::Comma,
                })
            }

            TokenKind::LParen => {
                self.context_stack.push(Context::Sequence);
                Some(Event::SequenceStart { span: t.span })
            }

            TokenKind::At => Some(self.parse_tag_value(t.span)),

            TokenKind::LineComment => Some(Event::Comment {
                span: t.span,
                text: t.text,
            }),

            _ => None,
        }
    }

    // === Helpers ===

    fn close_contexts_at_eof(&mut self) -> Option<Event<'src>> {
        // Check dangling docs
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
                Context::AttributeObject => {
                    self.event_queue.push_back(Event::ObjectEnd {
                        span: Span::new(self.input.len() as u32, self.input.len() as u32),
                    });
                    self.event_queue.push_back(Event::EntryEnd);
                    return self.close_contexts_at_eof();
                }
            }
        }

        self.state = State::Done;
        Some(Event::DocumentEnd)
    }

    fn handle_rbrace(&mut self, span: Span) -> Option<Event<'src>> {
        // Check dangling docs
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

    fn handle_at_as_key(&mut self, at_span: Span) -> Option<Event<'src>> {
        let t = self.next_token();

        if t.kind == TokenKind::BareScalar && t.span.start == at_span.end {
            // @tagname as key
            let (tag_name, name_end) = self.extract_tag_name(t.text, t.span.start);
            let has_trailing_at = name_end < t.span.end;

            // Invalid key forms: @tag{}, @tag(), @tag@
            if has_trailing_at {
                return Some(Event::Error {
                    span: Span::new(at_span.start, name_end + 1),
                    kind: ParseErrorKind::InvalidKey,
                });
            }

            let next = self.next_token();
            if next.span.start == name_end
                && matches!(
                    next.kind,
                    TokenKind::LBrace | TokenKind::LParen | TokenKind::At
                )
            {
                self.unread(next);
                return Some(Event::Error {
                    span: Span::new(at_span.start, name_end),
                    kind: ParseErrorKind::InvalidKey,
                });
            }
            self.unread(next);

            // Skip @schema at implicit root
            if tag_name == "schema"
                && self.context_stack.last() == Some(&Context::Object { implicit: true })
            {
                self.skip_value();
                self.pending_doc.clear();
                return None;
            }

            // Validate tag name
            if tag_name.is_empty() || !Self::is_valid_tag_name(tag_name) {
                self.event_queue.push_back(Event::Error {
                    span: Span::new(t.span.start, name_end),
                    kind: ParseErrorKind::InvalidTagName,
                });
            }

            self.emit_pending_docs();
            self.event_queue.push_back(Event::Key {
                span: Span::new(at_span.start, name_end),
                tag: Some(tag_name),
                payload: None,
                kind: ScalarKind::Bare,
            });
            self.state = State::AfterKey {
                key_span: Span::new(at_span.start, name_end),
            };
            return Some(Event::EntryStart);
        }

        // @ alone = unit key
        self.unread(t);
        self.emit_pending_docs();
        self.event_queue.push_back(Event::Key {
            span: at_span,
            tag: None,
            payload: None,
            kind: ScalarKind::Bare,
        });
        self.state = State::AfterKey { key_span: at_span };
        Some(Event::EntryStart)
    }

    fn parse_attribute_value(&mut self, key_span: Span) -> Option<Event<'src>> {
        // We just consumed `key>`, now parse the value
        let val_token = self.next_token();

        match val_token.kind {
            TokenKind::BareScalar | TokenKind::QuotedScalar | TokenKind::RawScalar => {
                let val_ev = self.token_to_scalar(val_token);

                // Check for more attributes
                let next = self.next_token();
                if next.kind == TokenKind::BareScalar {
                    let after = self.next_token();
                    if after.kind == TokenKind::Gt && after.span.start == next.span.end {
                        // Multiple attributes - start implicit object
                        self.context_stack.push(Context::AttributeObject);

                        // First entry
                        self.event_queue.push_back(Event::EntryStart);
                        self.event_queue.push_back(Event::Key {
                            span: key_span,
                            tag: None,
                            payload: Some(Cow::Borrowed(self.span_text(key_span))),
                            kind: ScalarKind::Bare,
                        });
                        self.event_queue.push_back(val_ev);
                        self.event_queue.push_back(Event::EntryEnd);

                        // Second entry
                        self.event_queue.push_back(Event::EntryStart);
                        self.event_queue.push_back(Event::Key {
                            span: next.span,
                            tag: None,
                            payload: Some(Cow::Borrowed(next.text)),
                            kind: ScalarKind::Bare,
                        });

                        let val2 = self.next_token();
                        let val2_ev = self.token_to_scalar(val2);
                        self.event_queue.push_back(val2_ev);
                        self.event_queue.push_back(Event::EntryEnd);

                        self.state = State::InAttributeChain {
                            obj_start_span: key_span,
                        };

                        return Some(Event::ObjectStart {
                            span: key_span,
                            separator: Separator::Comma,
                        });
                    }
                    self.unread(after);
                    self.unread(next);
                } else {
                    self.unread(next);
                }

                // Single attribute - wrap in object
                self.event_queue.push_back(Event::EntryStart);
                self.event_queue.push_back(Event::Key {
                    span: key_span,
                    tag: None,
                    payload: Some(Cow::Borrowed(self.span_text(key_span))),
                    kind: ScalarKind::Bare,
                });
                self.event_queue.push_back(val_ev);
                self.event_queue.push_back(Event::EntryEnd);
                self.event_queue
                    .push_back(Event::ObjectEnd { span: key_span });
                self.event_queue.push_back(Event::EntryEnd);
                self.state = State::ExpectingEntry;

                Some(Event::ObjectStart {
                    span: key_span,
                    separator: Separator::Comma,
                })
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

                self.context_stack.push(Context::Object { implicit: false });
                self.event_queue.push_back(Event::ObjectStart {
                    span: val_token.span,
                    separator: Separator::Comma,
                });
                self.state = State::ExpectingEntry;

                Some(Event::ObjectStart {
                    span: key_span,
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

                self.context_stack.push(Context::Sequence);
                self.event_queue.push_back(Event::SequenceStart {
                    span: val_token.span,
                });
                self.state = State::ExpectingSequenceElement;

                Some(Event::ObjectStart {
                    span: key_span,
                    separator: Separator::Comma,
                })
            }

            TokenKind::At => {
                // key>@tag
                let tag_ev = self.parse_tag_value(val_token.span);

                self.event_queue.push_back(Event::EntryStart);
                self.event_queue.push_back(Event::Key {
                    span: key_span,
                    tag: None,
                    payload: Some(Cow::Borrowed(self.span_text(key_span))),
                    kind: ScalarKind::Bare,
                });
                self.event_queue.push_back(tag_ev);
                self.event_queue.push_back(Event::EntryEnd);
                self.event_queue
                    .push_back(Event::ObjectEnd { span: key_span });
                self.event_queue.push_back(Event::EntryEnd);
                self.state = State::ExpectingEntry;

                Some(Event::ObjectStart {
                    span: key_span,
                    separator: Separator::Comma,
                })
            }

            _ => {
                self.event_queue.push_back(Event::Error {
                    span: val_token.span,
                    kind: ParseErrorKind::ExpectedValue,
                });
                self.event_queue.push_back(Event::EntryEnd);
                self.state = State::ExpectingEntry;
                None
            }
        }
    }

    fn parse_attribute_chain_as_value(&mut self, first_key_span: Span) -> Option<Event<'src>> {
        // We're at: `outer_key first_attr_key>` and just consumed the `>`
        let val = self.next_token();
        let val_ev = self.token_to_scalar(val);

        // Check for more attrs
        let next = self.next_token();
        if next.kind == TokenKind::BareScalar {
            let after = self.next_token();
            if after.kind == TokenKind::Gt && after.span.start == next.span.end {
                // Multiple - implicit object
                self.context_stack.push(Context::AttributeObject);

                self.event_queue.push_back(Event::EntryStart);
                self.event_queue.push_back(Event::Key {
                    span: first_key_span,
                    tag: None,
                    payload: Some(Cow::Borrowed(self.span_text(first_key_span))),
                    kind: ScalarKind::Bare,
                });
                self.event_queue.push_back(val_ev);
                self.event_queue.push_back(Event::EntryEnd);

                self.event_queue.push_back(Event::EntryStart);
                self.event_queue.push_back(Event::Key {
                    span: next.span,
                    tag: None,
                    payload: Some(Cow::Borrowed(next.text)),
                    kind: ScalarKind::Bare,
                });
                let val2 = self.next_token();
                let val2_ev = self.token_to_scalar(val2);
                self.event_queue.push_back(val2_ev);
                self.event_queue.push_back(Event::EntryEnd);

                self.state = State::InAttributeChain {
                    obj_start_span: first_key_span,
                };

                return Some(Event::ObjectStart {
                    span: first_key_span,
                    separator: Separator::Comma,
                });
            }
            self.unread(after);
            self.unread(next);
        } else {
            self.unread(next);
        }

        // Single attr
        self.event_queue.push_back(Event::EntryStart);
        self.event_queue.push_back(Event::Key {
            span: first_key_span,
            tag: None,
            payload: Some(Cow::Borrowed(self.span_text(first_key_span))),
            kind: ScalarKind::Bare,
        });
        self.event_queue.push_back(val_ev);
        self.event_queue.push_back(Event::EntryEnd);
        self.event_queue.push_back(Event::ObjectEnd {
            span: first_key_span,
        });
        self.state = State::AfterValue;

        Some(Event::ObjectStart {
            span: first_key_span,
            separator: Separator::Comma,
        })
    }

    fn close_attribute_chain(&mut self) {
        self.context_stack.pop(); // AttributeObject
        self.event_queue.push_back(Event::ObjectEnd {
            span: Span::new(0, 0),
        });
        self.event_queue.push_back(Event::EntryEnd);
        self.state = State::ExpectingEntry;
    }

    fn parse_tag_value(&mut self, at_span: Span) -> Event<'src> {
        let t = self.next_token();

        if t.kind == TokenKind::BareScalar && t.span.start == at_span.end {
            let (tag_name, name_end) = self.extract_tag_name(t.text, t.span.start);
            let has_trailing_at = name_end < t.span.end;

            if tag_name.is_empty() || !Self::is_valid_tag_name(tag_name) {
                self.event_queue.push_back(Event::Error {
                    span: Span::new(t.span.start, name_end),
                    kind: ParseErrorKind::InvalidTagName,
                });
            }

            if has_trailing_at {
                // @tag@
                self.event_queue.push_back(Event::Unit {
                    span: Span::new(name_end, name_end + 1),
                });
                self.event_queue.push_back(Event::TagEnd);
                return Event::TagStart {
                    span: Span::new(at_span.start, name_end + 1),
                    name: tag_name,
                };
            }

            // Check for payload
            let next = self.next_token();
            if next.span.start == name_end {
                match next.kind {
                    TokenKind::LBrace => {
                        self.context_stack.push(Context::Object { implicit: false });
                        self.event_queue.push_back(Event::ObjectStart {
                            span: next.span,
                            separator: Separator::Comma,
                        });
                        self.state = State::ExpectingEntry;
                        return Event::TagStart {
                            span: Span::new(at_span.start, name_end),
                            name: tag_name,
                        };
                    }
                    TokenKind::LParen => {
                        self.context_stack.push(Context::Sequence);
                        self.event_queue
                            .push_back(Event::SequenceStart { span: next.span });
                        self.state = State::ExpectingSequenceElement;
                        return Event::TagStart {
                            span: Span::new(at_span.start, name_end),
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
                            span: Span::new(at_span.start, name_end),
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
                            span: Span::new(at_span.start, name_end),
                            name: tag_name,
                        };
                    }
                    _ => {
                        self.unread(next);
                    }
                }
            } else {
                self.unread(next);
            }

            // Implicit unit
            self.event_queue.push_back(Event::Unit {
                span: Span::new(name_end, name_end),
            });
            self.event_queue.push_back(Event::TagEnd);
            return Event::TagStart {
                span: Span::new(at_span.start, name_end),
                name: tag_name,
            };
        }

        // @ alone
        self.unread(t);
        Event::Unit { span: at_span }
    }

    fn token_to_scalar(&mut self, t: Token<'src>) -> Event<'src> {
        match t.kind {
            TokenKind::BareScalar => Event::Scalar {
                span: t.span,
                value: Cow::Borrowed(t.text),
                kind: ScalarKind::Bare,
            },
            TokenKind::QuotedScalar => Event::Scalar {
                span: t.span,
                value: self.unescape_quoted(t.text),
                kind: ScalarKind::Quoted,
            },
            TokenKind::RawScalar => Event::Scalar {
                span: t.span,
                value: Cow::Borrowed(Self::strip_raw_delimiters(t.text)),
                kind: ScalarKind::Raw,
            },
            TokenKind::HeredocStart => self.parse_heredoc(t.span),
            _ => Event::Error {
                span: t.span,
                kind: ParseErrorKind::UnexpectedToken,
            },
        }
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

    fn skip_to_entry_boundary(&mut self) {
        loop {
            let t = self.next_token();
            match t.kind {
                TokenKind::Newline
                | TokenKind::Eof
                | TokenKind::RBrace
                | TokenKind::RParen
                | TokenKind::Comma => {
                    self.unread(t);
                    break;
                }
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
                TokenKind::RBrace | TokenKind::RParen => {
                    if depth == 0 {
                        self.unread(t);
                        break;
                    }
                    depth -= 1;
                }
                TokenKind::Newline | TokenKind::Comma if depth == 0 => break,
                TokenKind::Eof => break,
                _ if depth == 0 => break,
                _ => {}
            }
        }
    }

    fn state_after_close(&self) -> State {
        match self.context_stack.last() {
            Some(Context::Object { .. }) => State::AfterValue,
            Some(Context::Sequence) => State::ExpectingSequenceElement,
            Some(Context::AttributeObject) => State::InAttributeChain {
                obj_start_span: Span::new(0, 0),
            },
            None => State::Done,
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
