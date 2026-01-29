//! Pull-based streaming parser for Styx.
//!
//! This parser yields events one at a time as they are encountered,
//! rather than collecting them all into a callback.

use std::borrow::Cow;

use crate::event::{Event, ParseErrorKind, ScalarKind, Separator};
use crate::lexer::Lexer;
use crate::span::Span;
use crate::token::{Token, TokenKind};

#[allow(unused_imports)]
use crate::trace;

/// Pull-based streaming parser for Styx documents.
///
/// Unlike [`Parser`](crate::Parser) which uses callbacks, this parser implements
/// an iterator-like interface where you call `next_event()` to get events one at a time.
#[derive(Clone)]
pub struct Parser2<'src> {
    input: &'src str,
    lexer: Lexer<'src>,
    /// Stack of parsing contexts.
    stack: Vec<ContextState>,
    /// Peeked token (if any).
    peeked_token: Option<Token<'src>>,
    /// Peeked events queue (if any).
    peeked_events: Vec<Event<'src>>,
    /// Whether we've emitted DocumentStart.
    doc_started: bool,
    /// Whether we've emitted the root object start.
    root_started: bool,
    /// Whether parsing is complete.
    complete: bool,
    /// Current span for error reporting.
    current_span: Option<Span>,
    /// Whether we're expecting a value after a key.
    expecting_value: bool,
    /// Expression mode: parse a single value, not an implicit root object.
    expr_mode: bool,
    /// Buffered doc comments for the next field key.
    pending_doc: Vec<(Span, &'src str)>,
    /// Number of atoms seen in current entry (for TooManyAtoms detection).
    entry_atom_count: usize,
    /// Span of the second atom in current entry (for TooManyAtoms error reporting).
    second_atom_span: Option<Span>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum ContextState {
    /// Inside an object (braces or implicit root).
    Object { implicit: bool },
    /// Inside a sequence (parens).
    Sequence,
}

impl<'src> Parser2<'src> {
    /// Create a new parser for the given source (document mode).
    pub fn new(source: &'src str) -> Self {
        Self {
            input: source,
            lexer: Lexer::new(source),
            stack: Vec::new(),
            peeked_token: None,
            peeked_events: Vec::new(),
            doc_started: false,
            root_started: false,
            complete: false,
            current_span: None,
            expecting_value: false,
            expr_mode: false,
            pending_doc: Vec::new(),
            entry_atom_count: 0,
            second_atom_span: None,
        }
    }

    /// Create a new parser in expression mode.
    ///
    /// Expression mode parses a single value rather than an implicit root object.
    /// Use this for parsing embedded values like default values in schemas.
    pub fn new_expr(source: &'src str) -> Self {
        Self {
            input: source,
            lexer: Lexer::new(source),
            stack: Vec::new(),
            peeked_token: None,
            peeked_events: Vec::new(),
            doc_started: false,
            root_started: false,
            complete: false,
            current_span: None,
            expecting_value: true, // Start expecting a value immediately
            expr_mode: true,
            pending_doc: Vec::new(),
            entry_atom_count: 0,
            second_atom_span: None,
        }
    }

    /// Get the next event from the parser.
    ///
    /// Returns `None` when parsing is complete.
    pub fn next_event(&mut self) -> Option<Event<'src>> {
        // Return queued event if any (FIFO - take from front)
        if !self.peeked_events.is_empty() {
            let event = self.peeked_events.remove(0);
            trace!(?event, "next_event: returning queued event");
            return Some(event);
        }

        if self.complete {
            trace!("next_event: parsing complete");
            return None;
        }

        // Emit DocumentStart first
        if !self.doc_started {
            self.doc_started = true;
            trace!("next_event: emitting DocumentStart");
            return Some(Event::DocumentStart);
        }

        // Skip newlines between entries, but NOT when expecting a value.
        // A newline after a key means the key has unit value.
        if !self.expecting_value {
            self.skip_newlines();
        }

        // Handle root struct start (skip in expression mode)
        if !self.root_started && !self.expr_mode {
            self.root_started = true;

            // Check if document starts with explicit `{` - if so, that's the root object
            if let Some(token) = self.peek_token() {
                if token.kind == TokenKind::LBrace {
                    // Explicit root object - consume the `{` and emit ObjectStart
                    let brace_token = self.next_token();
                    self.stack.push(ContextState::Object { implicit: false });
                    trace!("next_event: explicit root ObjectStart");
                    return Some(Event::ObjectStart {
                        span: brace_token.span,
                        separator: Separator::Comma, // Will be refined by content
                    });
                }
            }

            // Implicit root object
            self.stack.push(ContextState::Object { implicit: true });
            trace!("next_event: emitting root ObjectStart");
            return Some(Event::ObjectStart {
                span: Span::new(0, 0),
                separator: Separator::Newline,
            });
        }
        self.root_started = true;

        // If we're expecting a value after a key
        if self.expecting_value {
            return self.parse_value();
        }

        // Check for end of current context
        let token = self.peek_token().cloned();
        if let Some(token) = token {
            match token.kind {
                TokenKind::Eof => {
                    // Pop remaining contexts
                    if let Some(ctx) = self.stack.pop() {
                        match ctx {
                            ContextState::Object { .. } => {
                                if self.stack.is_empty() {
                                    // Queue DocumentEnd after this ObjectEnd
                                    self.peeked_events.push(Event::DocumentEnd);
                                    self.complete = true;
                                }
                                trace!("next_event: EOF ObjectEnd");
                                return Some(Event::ObjectEnd { span: token.span });
                            }
                            ContextState::Sequence => {
                                trace!("next_event: EOF SequenceEnd");
                                return Some(Event::SequenceEnd { span: token.span });
                            }
                        }
                    }
                    // In expression mode with empty stack, we're done
                    self.complete = true;
                    return Some(Event::DocumentEnd);
                }
                TokenKind::RBrace => {
                    self.next_token();
                    match self.stack.pop() {
                        Some(ContextState::Object { implicit: false }) => {
                            trace!("next_event: RBrace ObjectEnd");
                            return Some(Event::ObjectEnd { span: token.span });
                        }
                        _ => {
                            // Mismatched brace - error
                            return Some(Event::Error {
                                span: token.span,
                                kind: ParseErrorKind::UnexpectedToken,
                            });
                        }
                    }
                }
                TokenKind::RParen => {
                    self.next_token();
                    match self.stack.pop() {
                        Some(ContextState::Sequence) => {
                            trace!("next_event: RParen SequenceEnd");
                            return Some(Event::SequenceEnd { span: token.span });
                        }
                        _ => {
                            return Some(Event::Error {
                                span: token.span,
                                kind: ParseErrorKind::UnexpectedToken,
                            });
                        }
                    }
                }
                TokenKind::Comma => {
                    // Skip comma separators
                    self.next_token();
                    self.skip_newlines();
                    return self.next_event();
                }
                TokenKind::Newline => {
                    self.next_token();
                    return self.next_event();
                }
                TokenKind::DocComment => {
                    // Buffer doc comments to attach to the next field key
                    let token = self.next_token();
                    self.pending_doc.push((token.span, token.text));
                    return self.next_event();
                }
                _ => {}
            }
        }

        // In object context, parse key-value
        if matches!(self.stack.last(), Some(ContextState::Object { .. })) {
            return self.parse_object_entry();
        }

        // In sequence context, parse values
        if matches!(self.stack.last(), Some(ContextState::Sequence)) {
            return self.parse_sequence_element();
        }

        None
    }

    /// Parse a value (after seeing a key in object context, or as a sequence element).
    fn parse_value(&mut self) -> Option<Event<'src>> {
        self.expecting_value = false;
        self.entry_atom_count += 1;
        trace!(atom_count = self.entry_atom_count, "parse_value");

        let token = self.peek_token().cloned();
        if let Some(token) = token {
            match token.kind {
                TokenKind::Newline | TokenKind::Eof | TokenKind::RBrace | TokenKind::Comma => {
                    // No value - emit unit
                    trace!("parse_value: no value found, emitting Unit");

                    // Check for TooManyAtoms before emitting EntryEnd
                    if self.entry_atom_count > 2 {
                        // We already have key + value, this would be a third atom
                        // But we're at a boundary, so just emit the unit and end entry
                    }

                    self.peeked_events.push(Event::EntryEnd);
                    self.entry_atom_count = 0;
                    self.second_atom_span = None;
                    return Some(Event::Unit {
                        span: self.current_span.unwrap_or(token.span),
                    });
                }
                TokenKind::LBrace => {
                    self.next_token();
                    self.stack.push(ContextState::Object { implicit: false });
                    trace!("parse_value: nested object ObjectStart");
                    return Some(Event::ObjectStart {
                        span: token.span,
                        separator: Separator::Comma, // Will be determined by content
                    });
                }
                TokenKind::LParen => {
                    self.next_token();
                    self.stack.push(ContextState::Sequence);
                    trace!("parse_value: SequenceStart");
                    return Some(Event::SequenceStart { span: token.span });
                }
                TokenKind::At => {
                    // Tag - could be @, @foo, @foo@, @foo(...), @foo{...}
                    self.next_token();
                    return Some(self.parse_tag(token.span.end));
                }
                TokenKind::BareScalar
                | TokenKind::QuotedScalar
                | TokenKind::RawScalar
                | TokenKind::HeredocStart => {
                    let value_token = self.next_token();
                    let kind = self.token_to_scalar_kind(value_token.kind);

                    // Handle heredoc content
                    if value_token.kind == TokenKind::HeredocStart {
                        let mut content = String::new();
                        let mut end_span = value_token.span;
                        loop {
                            let next = self.next_token();
                            match next.kind {
                                TokenKind::HeredocContent => {
                                    content.push_str(next.text);
                                }
                                TokenKind::HeredocEnd => {
                                    end_span = next.span;
                                    break;
                                }
                                _ => break,
                            }
                        }
                        trace!(?content, "parse_value: heredoc scalar");

                        // Check if there's another atom after this (TooManyAtoms)
                        self.check_for_extra_atoms();

                        return Some(Event::Scalar {
                            span: Span::new(value_token.span.start, end_span.end),
                            value: Cow::Owned(content),
                            kind: ScalarKind::Heredoc,
                        });
                    }

                    let text = value_token.text;
                    let value = self.process_scalar(text, kind);
                    trace!(?value, "parse_value: scalar");

                    // Check if there's another atom after this (TooManyAtoms)
                    self.check_for_extra_atoms();

                    return Some(Event::Scalar {
                        span: value_token.span,
                        value,
                        kind,
                    });
                }
                _ => {}
            }
        }
        None
    }

    /// Check if there are extra atoms after the value (TooManyAtoms error).
    fn check_for_extra_atoms(&mut self) {
        // Skip whitespace and peek at next token
        self.skip_whitespace();

        if let Some(token) = self.peek_token() {
            // If next token is not a boundary, it's an extra atom
            match token.kind {
                TokenKind::Newline
                | TokenKind::Eof
                | TokenKind::RBrace
                | TokenKind::RParen
                | TokenKind::Comma => {
                    // Normal boundary - emit EntryEnd
                    self.peeked_events.push(Event::EntryEnd);
                    self.entry_atom_count = 0;
                    self.second_atom_span = None;
                }
                TokenKind::BareScalar
                | TokenKind::QuotedScalar
                | TokenKind::RawScalar
                | TokenKind::LBrace
                | TokenKind::LParen
                | TokenKind::At
                | TokenKind::HeredocStart => {
                    // Extra atom! This is a TooManyAtoms error.
                    let extra_span = token.span;

                    // Queue the error event
                    self.peeked_events.push(Event::Error {
                        span: extra_span,
                        kind: ParseErrorKind::TooManyAtoms,
                    });

                    // Skip the extra atom(s) until we hit a boundary
                    self.skip_until_entry_boundary();

                    self.peeked_events.push(Event::EntryEnd);
                    self.entry_atom_count = 0;
                    self.second_atom_span = None;
                }
                _ => {
                    self.peeked_events.push(Event::EntryEnd);
                    self.entry_atom_count = 0;
                    self.second_atom_span = None;
                }
            }
        } else {
            self.peeked_events.push(Event::EntryEnd);
            self.entry_atom_count = 0;
            self.second_atom_span = None;
        }
    }

    /// Skip tokens until we hit an entry boundary.
    fn skip_until_entry_boundary(&mut self) {
        loop {
            self.skip_whitespace();
            if let Some(token) = self.peek_token() {
                match token.kind {
                    TokenKind::Newline
                    | TokenKind::Eof
                    | TokenKind::RBrace
                    | TokenKind::RParen
                    | TokenKind::Comma => {
                        break;
                    }
                    TokenKind::LBrace => {
                        // Skip nested object
                        self.next_token();
                        self.skip_nested_structure(TokenKind::RBrace);
                    }
                    TokenKind::LParen => {
                        // Skip nested sequence
                        self.next_token();
                        self.skip_nested_structure(TokenKind::RParen);
                    }
                    _ => {
                        self.next_token();
                    }
                }
            } else {
                break;
            }
        }
    }

    /// Skip a nested structure (object or sequence).
    fn skip_nested_structure(&mut self, closing: TokenKind) {
        let mut depth = 1;
        while depth > 0 {
            if let Some(token) = self.peek_token().cloned() {
                self.next_token();
                if token.kind == TokenKind::LBrace || token.kind == TokenKind::LParen {
                    depth += 1;
                } else if token.kind == closing
                    || (closing == TokenKind::RBrace && token.kind == TokenKind::RBrace)
                    || (closing == TokenKind::RParen && token.kind == TokenKind::RParen)
                {
                    depth -= 1;
                } else if token.kind == TokenKind::Eof {
                    break;
                }
            } else {
                break;
            }
        }
    }

    /// Parse an entry in object context.
    fn parse_object_entry(&mut self) -> Option<Event<'src>> {
        let token = self.peek_token().cloned()?;

        match token.kind {
            TokenKind::BareScalar | TokenKind::QuotedScalar => {
                let key_token = self.next_token();
                let key = if key_token.kind == TokenKind::QuotedScalar {
                    self.unescape_quoted(key_token.text)
                } else {
                    Cow::Borrowed(key_token.text)
                };

                self.expecting_value = true;
                self.entry_atom_count = 1; // Key is the first atom

                // Emit any buffered doc comments
                let doc_comments = std::mem::take(&mut self.pending_doc);
                for (span, text) in &doc_comments {
                    self.peeked_events
                        .push(Event::DocComment { span: *span, text });
                }

                // Queue EntryStart before the Key
                let mut events = vec![Event::EntryStart];
                events.extend(self.peeked_events.drain(..));
                self.peeked_events = events;

                trace!(?key, "parse_object_entry: Key");
                return Some(Event::Key {
                    span: key_token.span,
                    tag: None,
                    payload: Some(key),
                    kind: if key_token.kind == TokenKind::QuotedScalar {
                        ScalarKind::Quoted
                    } else {
                        ScalarKind::Bare
                    },
                });
            }
            TokenKind::At => {
                let at_token = self.next_token();

                // Check if followed immediately by identifier
                if let Some(next) = self.peek_token()
                    && next.kind == TokenKind::BareScalar
                    && next.span.start == at_token.span.end
                {
                    let name_token = self.next_token();
                    let tag_name = name_token.text;
                    let name_end = name_token.span.end;

                    // Check what follows the tag name
                    if let Some(after) = self.peek_token()
                        && after.span.start == name_end
                    {
                        match after.kind {
                            TokenKind::LBrace | TokenKind::LParen | TokenKind::At => {
                                // @foo{...} or @foo(...) or @foo@ as a key - error
                                return Some(Event::Error {
                                    span: Span::new(at_token.span.start, name_end),
                                    kind: ParseErrorKind::InvalidKey,
                                });
                            }
                            _ => {}
                        }
                    }

                    // Skip @schema at the implicit root level
                    if tag_name == "schema"
                        && self.stack.last() == Some(&ContextState::Object { implicit: true })
                    {
                        self.expecting_value = true;
                        self.skip_value_internal();
                        self.pending_doc.clear();
                        return self.next_event();
                    }

                    self.expecting_value = true;
                    self.entry_atom_count = 1;

                    // Emit doc comments and EntryStart
                    let doc_comments = std::mem::take(&mut self.pending_doc);
                    for (span, text) in &doc_comments {
                        self.peeked_events
                            .push(Event::DocComment { span: *span, text });
                    }

                    let mut events = vec![Event::EntryStart];
                    events.extend(self.peeked_events.drain(..));
                    self.peeked_events = events;

                    trace!(tag = tag_name, "parse_object_entry: tagged Key");
                    return Some(Event::Key {
                        span: Span::new(at_token.span.start, name_end),
                        tag: Some(tag_name),
                        payload: None,
                        kind: ScalarKind::Bare,
                    });
                }

                // @ alone = unit key
                self.expecting_value = true;
                self.entry_atom_count = 1;

                let doc_comments = std::mem::take(&mut self.pending_doc);
                for (span, text) in &doc_comments {
                    self.peeked_events
                        .push(Event::DocComment { span: *span, text });
                }

                let mut events = vec![Event::EntryStart];
                events.extend(self.peeked_events.drain(..));
                self.peeked_events = events;

                trace!("parse_object_entry: unit Key");
                return Some(Event::Key {
                    span: at_token.span,
                    tag: None,
                    payload: None,
                    kind: ScalarKind::Bare,
                });
            }
            _ => {}
        }

        None
    }

    /// Parse an element in sequence context.
    fn parse_sequence_element(&mut self) -> Option<Event<'src>> {
        let token = self.peek_token().cloned()?;

        match token.kind {
            TokenKind::BareScalar
            | TokenKind::QuotedScalar
            | TokenKind::RawScalar
            | TokenKind::HeredocStart => {
                let value_token = self.next_token();
                let kind = self.token_to_scalar_kind(value_token.kind);

                if value_token.kind == TokenKind::HeredocStart {
                    let mut content = String::new();
                    let mut end_span = value_token.span;
                    loop {
                        let next = self.next_token();
                        match next.kind {
                            TokenKind::HeredocContent => {
                                content.push_str(next.text);
                            }
                            TokenKind::HeredocEnd => {
                                end_span = next.span;
                                break;
                            }
                            _ => break,
                        }
                    }
                    return Some(Event::Scalar {
                        span: Span::new(value_token.span.start, end_span.end),
                        value: Cow::Owned(content),
                        kind: ScalarKind::Heredoc,
                    });
                }

                let value = self.process_scalar(value_token.text, kind);
                return Some(Event::Scalar {
                    span: value_token.span,
                    value,
                    kind,
                });
            }
            TokenKind::LBrace => {
                self.next_token();
                self.stack.push(ContextState::Object { implicit: false });
                return Some(Event::ObjectStart {
                    span: token.span,
                    separator: Separator::Comma,
                });
            }
            TokenKind::LParen => {
                self.next_token();
                self.stack.push(ContextState::Sequence);
                return Some(Event::SequenceStart { span: token.span });
            }
            TokenKind::At => {
                self.next_token();
                return Some(self.parse_tag(token.span.end));
            }
            _ => {}
        }

        None
    }

    /// Parse a tag and emit appropriate events.
    fn parse_tag(&mut self, at_span_end: u32) -> Event<'src> {
        // Check if followed by identifier (tag name)
        if let Some(next) = self.peek_token()
            && next.kind == TokenKind::BareScalar
            && next.span.start == at_span_end
        {
            let name_token = self.next_token();
            let tag_name = name_token.text;

            // Check for payload
            if let Some(next) = self.peek_token() {
                if next.kind == TokenKind::At && next.span.start == name_token.span.end {
                    // @foo@ - tag with explicit unit payload
                    self.next_token();
                    self.peeked_events.push(Event::Unit {
                        span: name_token.span,
                    });
                    self.peeked_events.push(Event::TagEnd);
                    return Event::TagStart {
                        span: Span::new(at_span_end - 1, name_token.span.end + 1),
                        name: tag_name,
                    };
                } else if next.kind == TokenKind::LBrace && next.span.start == name_token.span.end {
                    // @foo{...} - tag with object payload
                    let brace_token = self.next_token();
                    self.stack.push(ContextState::Object { implicit: false });
                    self.peeked_events.push(Event::ObjectStart {
                        span: brace_token.span,
                        separator: Separator::Comma,
                    });
                    return Event::TagStart {
                        span: Span::new(at_span_end - 1, name_token.span.end),
                        name: tag_name,
                    };
                } else if next.kind == TokenKind::LParen && next.span.start == name_token.span.end {
                    // @foo(...) - tag with sequence payload
                    let paren_token = self.next_token();
                    self.stack.push(ContextState::Sequence);
                    self.peeked_events.push(Event::SequenceStart {
                        span: paren_token.span,
                    });
                    return Event::TagStart {
                        span: Span::new(at_span_end - 1, name_token.span.end),
                        name: tag_name,
                    };
                }
            }

            // @foo - named tag with implicit unit payload
            self.peeked_events.push(Event::Unit {
                span: name_token.span,
            });
            self.peeked_events.push(Event::TagEnd);
            return Event::TagStart {
                span: Span::new(at_span_end - 1, name_token.span.end),
                name: tag_name,
            };
        }

        // Just @ alone - unit
        Event::Unit {
            span: Span::new(at_span_end - 1, at_span_end),
        }
    }

    /// Skip a value (for @schema skipping).
    fn skip_value_internal(&mut self) {
        let mut depth = 0i32;
        loop {
            let token = self.peek_token().cloned();
            match token.as_ref().map(|t| t.kind) {
                Some(TokenKind::LBrace) | Some(TokenKind::LParen) => {
                    self.next_token();
                    depth += 1;
                }
                Some(TokenKind::RBrace) | Some(TokenKind::RParen) => {
                    if depth == 0 {
                        break;
                    }
                    self.next_token();
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                Some(TokenKind::Newline) | Some(TokenKind::Comma) => {
                    if depth == 0 {
                        break;
                    }
                    self.next_token();
                }
                Some(TokenKind::Eof) => break,
                Some(_) => {
                    self.next_token();
                    if depth == 0 {
                        break;
                    }
                }
                None => break,
            }
        }
        self.expecting_value = false;
    }

    /// Peek at the next event without consuming it.
    pub fn peek_event(&mut self) -> Option<&Event<'src>> {
        if self.peeked_events.is_empty() {
            if let Some(event) = self.next_event() {
                self.peeked_events.insert(0, event);
            }
        }
        self.peeked_events.first()
    }

    /// Get the input source.
    pub fn input(&self) -> &'src str {
        self.input
    }

    /// Get the current span.
    pub fn current_span(&self) -> Option<Span> {
        self.current_span
    }

    // === Internal helpers ===

    /// Peek at the next token without consuming it.
    fn peek_token(&mut self) -> Option<&Token<'src>> {
        if self.peeked_token.is_none() {
            loop {
                let token = self.lexer.next_token();
                // Skip whitespace and line comments (but not doc comments)
                match token.kind {
                    TokenKind::Whitespace | TokenKind::LineComment => continue,
                    TokenKind::Eof => {
                        self.peeked_token = Some(token);
                        break;
                    }
                    _ => {
                        self.peeked_token = Some(token);
                        break;
                    }
                }
            }
        }
        self.peeked_token.as_ref()
    }

    /// Skip whitespace only (not newlines).
    fn skip_whitespace(&mut self) {
        while let Some(token) = self.peeked_token.as_ref() {
            if token.kind == TokenKind::Whitespace {
                self.peeked_token = None;
                self.peek_token();
            } else {
                break;
            }
        }
    }

    /// Consume the next token.
    fn next_token(&mut self) -> Token<'src> {
        if let Some(token) = self.peeked_token.take() {
            self.current_span = Some(token.span);
            return token;
        }
        loop {
            let token = self.lexer.next_token();
            match token.kind {
                TokenKind::Whitespace | TokenKind::LineComment => continue,
                _ => {
                    self.current_span = Some(token.span);
                    return token;
                }
            }
        }
    }

    /// Skip newlines and return true if any were found.
    fn skip_newlines(&mut self) -> bool {
        let mut found = false;
        loop {
            if let Some(token) = self.peek_token()
                && token.kind == TokenKind::Newline
            {
                self.next_token();
                found = true;
                continue;
            }
            break;
        }
        found
    }

    /// Process a scalar value - Styx is schema-driven, so all scalars are strings.
    fn process_scalar(&self, text: &'src str, kind: ScalarKind) -> Cow<'src, str> {
        match kind {
            ScalarKind::Bare => {
                // In Styx, bare scalars are just strings - no type guessing!
                Cow::Borrowed(text)
            }
            ScalarKind::Quoted => self.unescape_quoted(text),
            ScalarKind::Raw => Cow::Borrowed(Self::strip_raw_delimiters(text)),
            ScalarKind::Heredoc => Cow::Borrowed(text),
        }
    }

    /// Unescape a quoted string.
    fn unescape_quoted(&self, text: &'src str) -> Cow<'src, str> {
        // Remove surrounding quotes
        let inner = if text.starts_with('"') && text.ends_with('"') && text.len() >= 2 {
            &text[1..text.len() - 1]
        } else {
            text
        };

        // Check if any escapes present
        if !inner.contains('\\') {
            return Cow::Borrowed(inner);
        }

        // Process escapes
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
                            chars.next(); // consume '{'
                            let mut hex = String::new();
                            while let Some(&c) = chars.peek() {
                                if c == '}' {
                                    chars.next();
                                    break;
                                }
                                hex.push(chars.next().unwrap());
                            }
                            if let Ok(code) = u32::from_str_radix(&hex, 16)
                                && let Some(ch) = char::from_u32(code)
                            {
                                result.push(ch);
                            }
                        } else {
                            // \uXXXX form
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
                                if let Ok(code) = u32::from_str_radix(&hex, 16)
                                    && let Some(ch) = char::from_u32(code)
                                {
                                    result.push(ch);
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
                    None => {
                        result.push('\\');
                    }
                }
            } else {
                result.push(c);
            }
        }

        Cow::Owned(result)
    }

    /// Strip raw string delimiters.
    fn strip_raw_delimiters(text: &str) -> &str {
        // Raw string format: r#*"content"#*
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

    /// Get the scalar kind for a token.
    fn token_to_scalar_kind(&self, kind: TokenKind) -> ScalarKind {
        match kind {
            TokenKind::BareScalar => ScalarKind::Bare,
            TokenKind::QuotedScalar => ScalarKind::Quoted,
            TokenKind::RawScalar => ScalarKind::Raw,
            TokenKind::HeredocStart | TokenKind::HeredocContent | TokenKind::HeredocEnd => {
                ScalarKind::Heredoc
            }
            _ => ScalarKind::Bare,
        }
    }
}

/// Convenience: collect all events into a Vec.
impl<'src> Parser2<'src> {
    /// Parse and collect all events.
    pub fn parse_to_vec(mut self) -> Vec<Event<'src>> {
        let mut events = Vec::new();
        while let Some(event) = self.next_event() {
            events.push(event);
        }
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use facet_testhelpers::test;
    use styx_testhelpers::{ActualError, assert_annotated_errors, source_without_annotations};

    fn format_event(event: &Event<'_>) -> String {
        match event {
            Event::DocumentStart => "DocumentStart".to_string(),
            Event::DocumentEnd => "DocumentEnd".to_string(),
            Event::ObjectStart { span, separator } => {
                format!(
                    "ObjectStart span={}..{} separator={:?}",
                    span.start, span.end, separator
                )
            }
            Event::ObjectEnd { span } => format!("ObjectEnd span={}..{}", span.start, span.end),
            Event::SequenceStart { span } => {
                format!("SequenceStart span={}..{}", span.start, span.end)
            }
            Event::SequenceEnd { span } => format!("SequenceEnd span={}..{}", span.start, span.end),
            Event::EntryStart => "EntryStart".to_string(),
            Event::EntryEnd => "EntryEnd".to_string(),
            Event::Key {
                span,
                tag,
                payload,
                kind,
            } => format!(
                "Key span={}..{} tag={:?} payload={:?} kind={:?}",
                span.start, span.end, tag, payload, kind
            ),
            Event::Scalar { span, value, kind } => format!(
                "Scalar span={}..{} value={:?} kind={:?}",
                span.start, span.end, value, kind
            ),
            Event::Unit { span } => format!("Unit span={}..{}", span.start, span.end),
            Event::TagStart { span, name } => {
                format!("TagStart span={}..{} name={}", span.start, span.end, name)
            }
            Event::TagEnd => "TagEnd".to_string(),
            Event::Comment { span, text } => {
                format!("Comment span={}..{} text={:?}", span.start, span.end, text)
            }
            Event::DocComment { span, text } => format!(
                "DocComment span={}..{} text={:?}",
                span.start, span.end, text
            ),
            Event::Error { span, kind } => {
                format!("Error span={}..{} kind={:?}", span.start, span.end, kind)
            }
        }
    }

    fn parse(source: &str) -> Vec<Event<'_>> {
        tracing::debug!("parsing with Parser2");
        let events = Parser2::new(source).parse_to_vec();
        if tracing::enabled!(tracing::Level::DEBUG) {
            let rendered = events
                .iter()
                .map(format_event)
                .collect::<Vec<_>>()
                .join("\n");
            tracing::debug!(events = %rendered, "parsed events");
        }
        events
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
        assert!(events.contains(&Event::DocumentStart));
        assert!(events.contains(&Event::DocumentEnd));
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::Key { payload: Some(value), .. } if value == "foo"))
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
                .any(|e| matches!(e, Event::Key { payload: Some(value), .. } if value == "foo"))
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
                    payload: Some(value),
                    ..
                } => Some(value.as_ref()),
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
        // parser[verify entry.toomany]
        // 3+ atoms should produce an error on the third atom
        assert_parse_errors(
            r#"
a b c
    ^ TooManyAtoms
"#,
        );
    }

    #[test]
    fn test_too_many_atoms_in_object() {
        // The original issue: {label ": BIGINT" line 4} should be an error
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
        assert!(
            events.iter().any(|e| matches!(
                e,
                Event::Key {
                    payload: None,
                    tag: None,
                    ..
                }
            )),
            "should have Key event with payload: None (unit key)"
        );
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
    fn test_nested_object() {
        let events = parse("outer {inner {x 1}}");
        let obj_starts = events
            .iter()
            .filter(|e| matches!(e, Event::ObjectStart { .. }))
            .count();
        // Root implicit object + outer explicit + inner explicit = 3
        assert!(
            obj_starts >= 2,
            "Expected at least 2 ObjectStart events for nested objects"
        );
    }

    #[test]
    fn test_sequence() {
        let events = parse("items (a b c)");
        let scalars: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                Event::Scalar { value, .. } => Some(value.as_ref()),
                _ => None,
            })
            .collect();
        assert!(scalars.contains(&"a"), "Missing element 'a'");
        assert!(scalars.contains(&"b"), "Missing element 'b'");
        assert!(scalars.contains(&"c"), "Missing element 'c'");
    }

    #[test]
    fn test_bare_scalar_is_string() {
        // Styx is schema-driven - bare scalars should NOT be type-guessed
        let events = parse("port 8080");
        // The value "8080" should be a string, not parsed as a number
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::Scalar { value, .. } if value == "8080")),
            "8080 should be preserved as string"
        );
    }

    #[test]
    fn test_bool_like_is_string() {
        // "true" and "false" should be strings in Styx, not booleans
        let events = parse("enabled true");
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::Scalar { value, .. } if value == "true")),
            "true should be preserved as string 'true'"
        );
    }
}
