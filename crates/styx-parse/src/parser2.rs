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
            // Handle comments and doc comments before the root object
            if let Some(token) = self.peek_token() {
                trace!(?token.kind, "checking for pre-root comment");
                match token.kind {
                    TokenKind::LineComment => {
                        let token = self.next_token();
                        trace!("emitting pre-root Comment");
                        return Some(Event::Comment {
                            span: token.span,
                            text: token.text,
                        });
                    }
                    TokenKind::DocComment => {
                        let token = self.next_token();
                        self.pending_doc.push((token.span, token.text));
                        return self.next_event();
                    }
                    _ => {}
                }
            }

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
                TokenKind::LineComment => {
                    // Emit line comments as Comment events
                    let token = self.next_token();
                    return Some(Event::Comment {
                        span: token.span,
                        text: token.text,
                    });
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

                // Emit any buffered doc comments first
                let doc_comments = std::mem::take(&mut self.pending_doc);
                for (span, text) in &doc_comments {
                    self.peeked_events
                        .push(Event::DocComment { span: *span, text });
                }

                // Queue Key event after EntryStart (we return EntryStart, Key is queued)
                self.peeked_events.push(Event::Key {
                    span: key_token.span,
                    tag: None,
                    payload: Some(key),
                    kind: if key_token.kind == TokenKind::QuotedScalar {
                        ScalarKind::Quoted
                    } else {
                        ScalarKind::Bare
                    },
                });

                trace!("parse_object_entry: EntryStart");
                return Some(Event::EntryStart);
            }
            TokenKind::At => {
                let at_token = self.next_token();

                // Check if followed immediately by identifier
                if let Some(next) = self.peek_token()
                    && next.kind == TokenKind::BareScalar
                    && next.span.start == at_token.span.end
                {
                    let name_token = self.next_token();
                    let full_text = name_token.text;

                    // The bare scalar may contain @ which is not valid in tag names.
                    // We need to split at the first @ if present.
                    let tag_name_len = full_text.find('@').unwrap_or(full_text.len());
                    let tag_name = &full_text[..tag_name_len];
                    let name_span = Span::new(
                        name_token.span.start,
                        name_token.span.start + tag_name_len as u32,
                    );
                    let name_end = name_span.end;

                    // Check if there was a trailing @ in the token
                    let has_trailing_at = tag_name_len < full_text.len();

                    // Validate tag name
                    let invalid_tag_name =
                        tag_name.is_empty() || !Self::is_valid_tag_name(tag_name);

                    // Check what follows the tag name
                    if has_trailing_at {
                        // @foo@ as a key - error (tag with payload not valid as key)
                        return Some(Event::Error {
                            span: Span::new(at_token.span.start, name_end + 1),
                            kind: ParseErrorKind::InvalidKey,
                        });
                    }

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

                    // Emit doc comments first
                    let doc_comments = std::mem::take(&mut self.pending_doc);
                    for (span, text) in &doc_comments {
                        self.peeked_events
                            .push(Event::DocComment { span: *span, text });
                    }

                    // Emit InvalidTagName error if needed
                    if invalid_tag_name {
                        self.peeked_events.push(Event::Error {
                            span: name_span,
                            kind: ParseErrorKind::InvalidTagName,
                        });
                    }

                    // Queue Key after EntryStart
                    self.peeked_events.push(Event::Key {
                        span: Span::new(at_token.span.start, name_end),
                        tag: Some(tag_name),
                        payload: None,
                        kind: ScalarKind::Bare,
                    });

                    trace!(tag = tag_name, "parse_object_entry: tagged EntryStart");
                    return Some(Event::EntryStart);
                }

                // @ alone = unit key
                self.expecting_value = true;
                self.entry_atom_count = 1;

                let doc_comments = std::mem::take(&mut self.pending_doc);
                for (span, text) in &doc_comments {
                    self.peeked_events
                        .push(Event::DocComment { span: *span, text });
                }

                // Queue Key after EntryStart
                self.peeked_events.push(Event::Key {
                    span: at_token.span,
                    tag: None,
                    payload: None,
                    kind: ScalarKind::Bare,
                });

                trace!("parse_object_entry: unit EntryStart");
                return Some(Event::EntryStart);
            }
            TokenKind::HeredocStart => {
                // Heredocs are not valid as keys - emit error with just the start span
                let start_token = self.next_token();
                let start_span = start_token.span;

                // Consume the heredoc content and end
                loop {
                    let next = self.next_token();
                    match next.kind {
                        TokenKind::HeredocContent => continue,
                        TokenKind::HeredocEnd | TokenKind::Eof => break,
                        _ => break,
                    }
                }

                return Some(Event::Error {
                    span: start_span,
                    kind: ParseErrorKind::InvalidKey,
                });
            }
            TokenKind::RawScalar => {
                // Raw scalars are valid as keys - treat like quoted
                let key_token = self.next_token();
                let key = Self::strip_raw_delimiters(key_token.text);

                self.expecting_value = true;
                self.entry_atom_count = 1;

                let doc_comments = std::mem::take(&mut self.pending_doc);
                for (span, text) in &doc_comments {
                    self.peeked_events
                        .push(Event::DocComment { span: *span, text });
                }

                self.peeked_events.push(Event::Key {
                    span: key_token.span,
                    tag: None,
                    payload: Some(Cow::Borrowed(key)),
                    kind: ScalarKind::Raw,
                });

                trace!("parse_object_entry: raw EntryStart");
                return Some(Event::EntryStart);
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
            let full_text = name_token.text;

            // The bare scalar may contain @ which is not valid in tag names.
            // We need to split at the first @ if present.
            let tag_name_len = full_text.find('@').unwrap_or(full_text.len());
            let tag_name = &full_text[..tag_name_len];
            let name_span = Span::new(
                name_token.span.start,
                name_token.span.start + tag_name_len as u32,
            );
            let name_end = name_span.end;

            // Check if the tag name itself contained @
            let has_trailing_at = tag_name_len < full_text.len();

            // Validate tag name: must match [A-Za-z_][A-Za-z0-9_-]*
            // Note: dots are NOT allowed in tag names
            let invalid_tag_name = tag_name.is_empty() || !Self::is_valid_tag_name(tag_name);
            if invalid_tag_name {
                self.peeked_events.push(Event::Error {
                    span: name_span,
                    kind: ParseErrorKind::InvalidTagName,
                });
            }

            if has_trailing_at {
                // @foo@ - tag with explicit unit payload
                // The @ is already consumed as part of the bare scalar
                let at_span = Span::new(name_end, name_end + 1);
                self.peeked_events.push(Event::Unit { span: at_span });
                self.peeked_events.push(Event::TagEnd);
                return Event::TagStart {
                    span: Span::new(at_span_end - 1, name_end + 1),
                    name: tag_name,
                };
            }

            // Check for payload (next token immediately after tag name)
            if let Some(next) = self.peek_token() {
                if next.kind == TokenKind::LBrace && next.span.start == name_end {
                    // @foo{...} - tag with object payload
                    let brace_token = self.next_token();
                    self.stack.push(ContextState::Object { implicit: false });
                    self.peeked_events.push(Event::ObjectStart {
                        span: brace_token.span,
                        separator: Separator::Comma,
                    });
                    return Event::TagStart {
                        span: Span::new(at_span_end - 1, name_end),
                        name: tag_name,
                    };
                } else if next.kind == TokenKind::LParen && next.span.start == name_end {
                    // @foo(...) - tag with sequence payload
                    let paren_token = self.next_token();
                    self.stack.push(ContextState::Sequence);
                    self.peeked_events.push(Event::SequenceStart {
                        span: paren_token.span,
                    });
                    return Event::TagStart {
                        span: Span::new(at_span_end - 1, name_end),
                        name: tag_name,
                    };
                } else if next.kind == TokenKind::QuotedScalar && next.span.start == name_end {
                    // @foo"bar" - tag with quoted string payload
                    let scalar_token = self.next_token();
                    let value = self.unescape_quoted(scalar_token.text);
                    self.peeked_events.push(Event::Scalar {
                        span: scalar_token.span,
                        value,
                        kind: ScalarKind::Quoted,
                    });
                    self.peeked_events.push(Event::TagEnd);
                    return Event::TagStart {
                        span: Span::new(at_span_end - 1, name_end),
                        name: tag_name,
                    };
                } else if next.kind == TokenKind::RawScalar && next.span.start == name_end {
                    // @foo r#"bar"# - tag with raw string payload
                    let scalar_token = self.next_token();
                    let value = Self::strip_raw_delimiters(scalar_token.text);
                    self.peeked_events.push(Event::Scalar {
                        span: scalar_token.span,
                        value: Cow::Borrowed(value),
                        kind: ScalarKind::Raw,
                    });
                    self.peeked_events.push(Event::TagEnd);
                    return Event::TagStart {
                        span: Span::new(at_span_end - 1, name_end),
                        name: tag_name,
                    };
                }
            }

            // @foo - named tag with implicit unit payload
            self.peeked_events.push(Event::Unit { span: name_span });
            self.peeked_events.push(Event::TagEnd);
            return Event::TagStart {
                span: Span::new(at_span_end - 1, name_end),
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
                // Skip whitespace only (not comments!)
                match token.kind {
                    TokenKind::Whitespace => continue,
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
                TokenKind::Whitespace => continue,
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

    /// Check if a tag name is valid.
    /// Must match pattern: [A-Za-z_][A-Za-z0-9_-]*
    /// Note: dots are NOT allowed in tag names (they are path separators in keys).
    fn is_valid_tag_name(name: &str) -> bool {
        let mut chars = name.chars();

        // First char: letter or underscore
        match chars.next() {
            Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
            _ => return false,
        }

        // Rest: alphanumeric, underscore, or hyphen (no dots!)
        chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
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

    #[allow(unused_imports)]
    use crate::trace;

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

    /// Parse and log events for debugging
    #[allow(dead_code)]
    fn parse_debug(source: &str) -> Vec<Event<'_>> {
        tracing::info!(source, "parsing (debug mode)");
        let events = Parser2::new(source).parse_to_vec();
        tracing::info!(?events, "parsed events");
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

    /// Parse source with error annotations and assert errors match.
    ///
    /// Source can contain error annotations on lines following the source:
    /// ```
    /// r#"
    /// {server {host localhost port 8080}}
    ///                         ^^^^ TooManyAtoms
    /// "#
    /// ```
    ///
    /// The carets (`^`) indicate the span where an error is expected,
    /// and the error kind name follows the carets.
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
        // parser2[verify entry.toomany]
        // 3+ atoms should produce an error on `c`
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
        // @ followed by whitespace then value should emit Key with payload: None (unit key)
        let events = parse("@ server.schema.styx");
        trace!(?events, "parsed events for unit key test");
        // Should have: DocumentStart, EntryStart, Key (unit), Scalar (value), EntryEnd, DocumentEnd
        assert!(
            events.iter().any(|e| matches!(
                e,
                Event::Key {
                    payload: None,
                    tag: None,
                    ..
                }
            )),
            "should have Key event with payload: None (unit key), got: {:?}",
            events
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
    fn test_comments() {
        let events = parse("// comment\nfoo bar");
        assert!(events.iter().any(|e| matches!(e, Event::Comment { .. })));
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::Key { payload: Some(value), .. } if value == "foo"))
        );
    }

    #[test]
    fn test_doc_comments() {
        let events = parse("/// doc\nfoo bar");
        assert!(events.iter().any(|e| matches!(e, Event::DocComment { .. })));
    }

    // parser2[verify comment.doc]
    #[test]
    fn test_doc_comment_followed_by_entry_ok() {
        // Doc comment followed by entry is valid - no errors
        assert_parse_errors("/// documentation\nkey value");
    }

    // parser2[verify comment.doc]
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

    // parser2[verify comment.doc]
    #[test]
    fn test_doc_comment_before_closing_brace_error() {
        assert_parse_errors(
            r#"
{foo bar
/// dangling
^^^^^^^^^^^^ DanglingDocComment
}
"#,
        );
    }

    // parser2[verify comment.doc]
    #[test]
    fn test_multiple_doc_comments_before_entry_ok() {
        // Multiple consecutive doc comments before entry is fine - no errors
        assert_parse_errors("/// line 1\n/// line 2\nkey value");
    }

    // parser2[verify object.syntax]
    #[test]
    fn test_nested_object() {
        let events = parse("outer {inner {x 1}}");
        // Should have nested ObjectStart/ObjectEnd events
        let obj_starts = events
            .iter()
            .filter(|e| matches!(e, Event::ObjectStart { .. }))
            .count();
        assert!(
            obj_starts >= 2,
            "Expected at least 2 ObjectStart events for nested objects"
        );
    }

    // parser2[verify object.syntax]
    #[test]
    fn test_object_with_entries() {
        let events = parse("config {host localhost, port 8080}");
        // Check we have keys for host and port
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
        assert!(keys.contains(&"config"), "Missing key 'config'");
        assert!(keys.contains(&"host"), "Missing key 'host'");
        assert!(keys.contains(&"port"), "Missing key 'port'");
    }

    // parser2[verify sequence.syntax] parser2[verify sequence.elements]
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
        assert!(scalars.contains(&"a"), "Missing element 'a'");
        assert!(scalars.contains(&"b"), "Missing element 'b'");
        assert!(scalars.contains(&"c"), "Missing element 'c'");
    }

    // parser2[verify sequence.syntax]
    #[test]
    fn test_nested_sequences() {
        let events = parse("matrix ((1 2) (3 4))");
        let seq_starts = events
            .iter()
            .filter(|e| matches!(e, Event::SequenceStart { .. }))
            .count();
        assert_eq!(
            seq_starts, 3,
            "Expected 3 SequenceStart events (outer + 2 inner)"
        );
    }

    // parser2[verify tag.payload]
    #[test]
    fn test_tagged_object() {
        let events = parse("result @err{message oops}");
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::TagStart { name, .. } if *name == "err")),
            "Missing TagStart for @err"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::ObjectStart { .. })),
            "Missing ObjectStart for tagged object"
        );
    }

    // parser2[verify tag.payload]
    #[test]
    fn test_tagged_sequence() {
        let events = parse("color @rgb(255 128 0)");
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::TagStart { name, .. } if *name == "rgb")),
            "Missing TagStart for @rgb"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::SequenceStart { .. })),
            "Missing SequenceStart for tagged sequence"
        );
    }

    // parser2[verify tag.payload]
    #[test]
    fn test_tagged_scalar() {
        let events = parse(r#"name @nickname"Bob""#);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::TagStart { name, .. } if *name == "nickname")),
            "Missing TagStart for @nickname"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::Scalar { value, .. } if value == "Bob")),
            "Missing Scalar for tagged string"
        );
    }

    // parser2[verify tag.payload]
    #[test]
    fn test_tagged_explicit_unit() {
        let events = parse("nothing @empty@");
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::TagStart { name, .. } if *name == "empty")),
            "Missing TagStart for @empty"
        );
        // The explicit @ after tag creates a Unit payload
        let unit_count = events
            .iter()
            .filter(|e| matches!(e, Event::Unit { .. }))
            .count();
        assert!(
            unit_count >= 1,
            "Expected at least one Unit event for @empty@"
        );
    }

    // parser2[verify tag.payload]
    #[test]
    fn test_tag_whitespace_gap() {
        // Whitespace between tag and potential payload = no payload (implicit unit)
        // Use a simpler case: key with tag value that has whitespace before object
        let events = parse("x @tag\ny {a b}");
        // @tag should be its own value (implicit unit), y {a b} is a separate entry
        let tag_events: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, Event::TagStart { .. } | Event::TagEnd))
            .collect();
        // There should be TagStart and TagEnd
        assert_eq!(tag_events.len(), 2, "Expected TagStart and TagEnd");
        // And the tag should NOT have the object as payload (object should be in a different entry)
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
        assert!(keys.contains(&"x"), "Missing key 'x'");
        assert!(keys.contains(&"y"), "Missing key 'y'");
    }

    // parser2[verify object.syntax]
    #[test]
    fn test_object_in_sequence() {
        let events = parse("servers ({host a} {host b})");
        // Sequence containing objects
        let obj_starts = events
            .iter()
            .filter(|e| matches!(e, Event::ObjectStart { .. }))
            .count();
        assert_eq!(
            obj_starts, 2,
            "Expected 2 ObjectStart events for objects in sequence"
        );
    }

    // parser2[verify attr.syntax]
    #[test]
    fn test_simple_attribute() {
        let events = parse("server host>localhost");
        // key=server, value is object with {host: localhost}
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
        assert!(keys.contains(&"server"), "Missing key 'server'");
        assert!(keys.contains(&"host"), "Missing key 'host' from attribute");
    }

    // parser2[verify attr.values]
    #[test]
    fn test_attribute_values() {
        let events = parse("config name>app tags>(a b) opts>{x 1}");
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
        assert!(keys.contains(&"config"), "Missing key 'config'");
        assert!(keys.contains(&"name"), "Missing key 'name'");
        assert!(keys.contains(&"tags"), "Missing key 'tags'");
        assert!(keys.contains(&"opts"), "Missing key 'opts'");
        // Check sequence is present
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::SequenceStart { .. })),
            "Missing SequenceStart for tags>(a b)"
        );
    }

    // parser2[verify attr.atom]
    #[test]
    fn test_multiple_attributes() {
        // When attributes are at root level without a preceding key,
        // the first attribute key becomes the entry key, and the rest form the value
        let events = parse("server host>localhost port>8080");
        // key=server, value is object with {host: localhost, port: 8080}
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
        assert!(keys.contains(&"server"), "Missing key 'server'");
        assert!(keys.contains(&"host"), "Missing key 'host'");
        assert!(keys.contains(&"port"), "Missing key 'port'");
    }

    // parser2[verify entry.path.attributes]
    #[test]
    fn test_too_many_atoms_with_attributes() {
        // parser2[verify entry.toomany]
        // Old key-path syntax is now an error
        assert_parse_errors(
            r#"
spec selector matchLabels app>web tier>frontend
              ^^^^^^^^^^^ TooManyAtoms
"#,
        );
    }

    // parser2[verify attr.syntax]
    #[test]
    fn test_attribute_no_spaces() {
        // Spaces around > means it's NOT attribute syntax
        let events = parse("x > y");
        // This should be: key=x, then ">" and "y" as values (nested)
        // Since > is its own token when preceded by whitespace
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
        // "x" should be the first key, and ">" should NOT be treated as attribute syntax
        assert!(keys.contains(&"x"), "Missing key 'x'");
        // There should not be ">" as a key (it would be a value)
    }

    // parser2[verify document.root]
    #[test]
    fn test_explicit_root_after_comment() {
        // Regular comment before explicit root object
        let events = parse("// comment\n{a 1}");
        // Should have ObjectStart (explicit root), not be treated as implicit root
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::ObjectStart { .. })),
            "Should have ObjectStart for explicit root after comment"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::Key { payload: Some(value), .. } if value == "a")),
            "Should have key 'a'"
        );
    }

    // parser2[verify document.root]
    #[test]
    fn test_explicit_root_after_doc_comment() {
        // Doc comment before explicit root object
        let events = parse("/// doc comment\n{a 1}");
        // Should have ObjectStart (explicit root) AND the doc comment
        assert!(
            events.iter().any(|e| matches!(e, Event::DocComment { .. })),
            "Should preserve doc comment"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::ObjectStart { .. })),
            "Should have ObjectStart for explicit root after doc comment"
        );
    }

    // parser2[verify entry.key-equality]
    #[test]
    fn test_duplicate_bare_key() {
        assert_parse_errors(
            r#"
{a 1, a 2}
      ^ DuplicateKey
"#,
        );
    }

    // parser2[verify entry.key-equality]
    #[test]
    fn test_duplicate_quoted_key() {
        assert_parse_errors(
            r#"
{"key" 1, "key" 2}
          ^^^^^ DuplicateKey
"#,
        );
    }

    // parser2[verify entry.key-equality]
    #[test]
    fn test_duplicate_key_escape_normalized() {
        // "ab" and "a\u{62}" should be considered duplicates after escape processing
        assert_parse_errors(
            r#"
{"ab" 1, "a\u{62}" 2}
         ^^^^^^^^^ DuplicateKey
"#,
        );
    }

    // parser2[verify entry.key-equality]
    #[test]
    fn test_duplicate_unit_key() {
        assert_parse_errors(
            r#"
{@ 1, @ 2}
      ^ DuplicateKey
"#,
        );
    }

    // parser2[verify entry.key-equality]
    #[test]
    fn test_duplicate_tagged_key() {
        assert_parse_errors(
            r#"
{@foo 1, @foo 2}
         ^^^^ DuplicateKey
"#,
        );
    }

    // parser2[verify entry.key-equality]
    #[test]
    fn test_different_keys_ok() {
        assert_parse_errors(r#"{a 1, b 2, c 3}"#);
    }

    // parser2[verify entry.key-equality]
    #[test]
    fn test_duplicate_key_at_root() {
        // Test duplicate keys at the document root level (implicit root object)
        assert_parse_errors(
            r#"
a 1
a 2
^ DuplicateKey
"#,
        );
    }

    // parser2[verify object.separators]
    #[test]
    fn test_mixed_separators_comma_then_newline() {
        // Start with comma, then use newline - should error at the newline
        assert_parse_errors(
            r#"
{a 1, b 2
         ^ MixedSeparators
c 3}
"#,
        );
    }

    // parser2[verify object.separators]
    #[test]
    fn test_mixed_separators_newline_then_comma() {
        // Start with newline, then use comma - should error
        assert_parse_errors(
            r#"
{a 1
b 2, c 3}
   ^ MixedSeparators
"#,
        );
    }

    // parser2[verify object.separators]
    #[test]
    fn test_consistent_comma_separators() {
        // All commas - should be fine
        assert_parse_errors(r#"{a 1, b 2, c 3}"#);
    }

    // parser2[verify object.separators]
    #[test]
    fn test_consistent_newline_separators() {
        // All newlines - should be fine
        assert_parse_errors(
            r#"{a 1
b 2
c 3}"#,
        );
    }

    // parser2[verify tag.syntax]
    #[test]
    fn test_valid_tag_names() {
        // Valid tag names should not produce errors
        assert_parse_errors("@foo");
        assert_parse_errors("@_private");
        assert_parse_errors("@my-tag");
        assert_parse_errors("@Type123");
    }

    // parser2[verify tag.syntax]
    #[test]
    fn test_tag_with_dot_invalid() {
        // @Some.Type is invalid since dots are not allowed in tag names
        assert_parse_errors(
            r#"
@Some.Type
 ^^^^^^^^^ InvalidTagName
"#,
        );
    }

    // parser2[verify tag.syntax]
    #[test]
    fn test_invalid_tag_name_starts_with_digit() {
        assert_parse_errors(
            r#"
x @123
   ^^^ InvalidTagName
"#,
        );
    }

    // parser2[verify tag.syntax]
    #[test]
    fn test_invalid_tag_name_starts_with_hyphen() {
        assert_parse_errors(
            r#"
x @-foo
   ^^^^ InvalidTagName
"#,
        );
    }

    // parser2[verify tag.syntax]
    #[test]
    fn test_invalid_tag_name_starts_with_dot() {
        assert_parse_errors(
            r#"
x @.foo
   ^^^^ InvalidTagName
"#,
        );
    }

    // parser2[verify scalar.quoted.escapes]
    #[test]
    fn test_unicode_escape_braces() {
        let events = parse(r#"x "\u{1F600}""#);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::Scalar { value, .. } if value == "😀")),
            "\\u{{1F600}} should produce 😀"
        );
    }

    // parser2[verify scalar.quoted.escapes]
    #[test]
    fn test_unicode_escape_4digit() {
        let events = parse(r#"x "\u0041""#);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::Scalar { value, .. } if value == "A")),
            "\\u0041 should produce A"
        );
    }

    // parser2[verify scalar.quoted.escapes]
    #[test]
    fn test_unicode_escape_4digit_accented() {
        let events = parse(r#"x "\u00E9""#);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::Scalar { value, .. } if value == "é")),
            "\\u00E9 should produce é"
        );
    }

    // parser2[verify scalar.quoted.escapes]
    #[test]
    fn test_unicode_escape_mixed() {
        // Mix of \uXXXX and \u{X} forms
        let events = parse(r#"x "\u0048\u{65}\u006C\u{6C}\u006F""#);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::Scalar { value, .. } if value == "Hello")),
            "Mixed unicode escapes should produce Hello"
        );
    }

    // parser2[verify entry.keys]
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

    // parser2[verify scalar.quoted.escapes]
    #[test]
    fn test_invalid_escape_null() {
        // \0 is no longer a valid escape - must use \u{0} instead
        assert_parse_errors(
            r#"
x "\0"
   ^^ InvalidEscape
"#,
        );
    }

    // parser2[verify scalar.quoted.escapes]
    #[test]
    fn test_invalid_escape_unknown() {
        // \q, \?, \a etc. are not valid escapes
        assert_parse_errors(
            r#"
x "\q"
   ^^ InvalidEscape
"#,
        );
    }

    // parser2[verify scalar.quoted.escapes]
    #[test]
    fn test_invalid_escape_multiple() {
        // Multiple invalid escapes should all be reported
        assert_parse_errors(
            r#"
x "\0\q\?"
   ^^ InvalidEscape
     ^^ InvalidEscape
       ^^ InvalidEscape
"#,
        );
    }

    // parser2[verify scalar.quoted.escapes]
    #[test]
    fn test_valid_escapes_still_work() {
        // Make sure valid escapes still work
        let events = parse(r#"x "a\nb\tc\\d\"e""#);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::Scalar { value, .. } if value == "a\nb\tc\\d\"e")),
            "Valid escapes should still work"
        );
        // No errors should be reported
        assert_parse_errors(r#"x "a\nb\tc\\d\"e""#);
    }

    // parser2[verify scalar.quoted.escapes]
    #[test]
    fn test_invalid_escape_in_key() {
        // Invalid escapes in keys should also be reported
        assert_parse_errors(
            r#"
"\0" value
 ^^ InvalidEscape
"#,
        );
    }

    // parser2[verify entry.structure]
    #[test]
    fn test_simple_key_value_with_attributes() {
        // Simple key-value where value is an attributes object
        let events = parse("server host>localhost port>8080");
        // Should have keys: server, host, port
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
        assert!(keys.contains(&"server"), "Missing key 'server'");
        assert!(keys.contains(&"host"), "Missing key 'host'");
        assert!(keys.contains(&"port"), "Missing key 'port'");
        // No errors should be reported
        assert_parse_errors(r#"server host>localhost port>8080"#);
    }

    // parser2[verify entry.path]
    #[test]
    fn test_dotted_path_simple() {
        // a.b value should expand to a { b value }
        let events = parse("a.b value");
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
        assert_eq!(keys, vec!["a", "b"], "Should have keys 'a' and 'b'");
        // Should have ObjectStart for the nested object
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::ObjectStart { .. })),
            "Should have ObjectStart for nested structure"
        );
        // Should have the value
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::Scalar { value, .. } if value == "value")),
            "Should have scalar value 'value'"
        );
        // No errors
        assert_parse_errors(r#"a.b value"#);
    }

    // parser2[verify entry.path]
    #[test]
    fn test_dotted_path_three_segments() {
        // a.b.c deep should expand to a { b { c deep } }
        let events = parse("a.b.c deep");
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
        assert_eq!(keys, vec!["a", "b", "c"], "Should have keys 'a', 'b', 'c'");
        // Should have two ObjectStart events for nested objects
        let obj_starts: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, Event::ObjectStart { .. }))
            .collect();
        assert_eq!(
            obj_starts.len(),
            2,
            "Should have 2 ObjectStart for nested structure"
        );
        // No errors
        assert_parse_errors(r#"a.b.c deep"#);
    }

    // parser2[verify entry.path]
    #[test]
    fn test_dotted_path_with_implicit_unit() {
        // a.b without value should have implicit unit
        let events = parse("a.b");
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
        assert_eq!(keys, vec!["a", "b"], "Should have keys 'a' and 'b'");
        // Should have Unit for implicit value
        assert!(
            events.iter().any(|e| matches!(e, Event::Unit { .. })),
            "Should have implicit unit value"
        );
    }

    // parser2[verify entry.path]
    #[test]
    fn test_dotted_path_empty_segment() {
        // a..b value - empty segment is invalid
        assert_parse_errors(
            r#"
a..b value
^^^^ InvalidKey
"#,
        );
    }

    // parser2[verify entry.path]
    #[test]
    fn test_dotted_path_trailing_dot() {
        // a.b. value - trailing dot is invalid
        assert_parse_errors(
            r#"
a.b. value
^^^^ InvalidKey
"#,
        );
    }

    // parser2[verify entry.path]
    #[test]
    fn test_dotted_path_leading_dot() {
        // .a.b value - leading dot is invalid
        assert_parse_errors(
            r#"
.a.b value
^^^^ InvalidKey
"#,
        );
    }

    // parser2[verify entry.path]
    #[test]
    fn test_dotted_path_with_object_value() {
        // a.b { c d } should expand to a { b { c d } }
        let events = parse("a.b { c d }");
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
        assert!(keys.contains(&"a"), "Should have 'a'");
        assert!(keys.contains(&"b"), "Should have 'b'");
        assert!(keys.contains(&"c"), "Should have 'c'");
        // No errors
        assert_parse_errors(r#"a.b { c d }"#);
    }

    // parser2[verify entry.path]
    #[test]
    fn test_dotted_path_with_attributes_value() {
        // selector.matchLabels app>web - dotted path with attributes as value
        let events = parse("selector.matchLabels app>web");
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
        assert!(keys.contains(&"selector"), "Should have 'selector'");
        assert!(keys.contains(&"matchLabels"), "Should have 'matchLabels'");
        assert!(keys.contains(&"app"), "Should have 'app' from attribute");
        // No errors
        assert_parse_errors(r#"selector.matchLabels app>web"#);
    }

    // parser2[verify entry.path]
    #[test]
    fn test_dot_in_value_is_literal() {
        // key example.com - dot in value position is literal, not path separator
        let events = parse("key example.com");
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
        assert_eq!(keys, vec!["key"], "Should have only one key 'key'");
        // Value should be the full domain
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::Scalar { value, .. } if value == "example.com")),
            "Value should be 'example.com' as a single scalar"
        );
        // No errors
        assert_parse_errors(r#"key example.com"#);
    }

    // parser2[verify entry.path.sibling]
    #[test]
    fn test_sibling_dotted_paths() {
        // Sibling paths under common prefix should be allowed
        let events = parse("foo.bar.x value1\nfoo.bar.y value2\nfoo.baz value3");
        // Should have no errors
        assert_parse_errors(
            r#"foo.bar.x value1
foo.bar.y value2
foo.baz value3"#,
        );
        // Should have all keys
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
        assert!(keys.contains(&"foo"), "Should have 'foo'");
        assert!(keys.contains(&"bar"), "Should have 'bar'");
        assert!(keys.contains(&"baz"), "Should have 'baz'");
        assert!(keys.contains(&"x"), "Should have 'x'");
        assert!(keys.contains(&"y"), "Should have 'y'");
    }

    // parser2[verify entry.path.reopen]
    #[test]
    fn test_reopen_closed_path_error() {
        // Can't reopen a path after moving to a sibling
        assert_parse_errors(
            r#"
foo.bar {}
foo.baz {}
foo.bar.x value
^^^^^^^^^ ReopenedPath
"#,
        );
    }

    // parser2[verify entry.path.reopen]
    #[test]
    fn test_reopen_nested_closed_path_error() {
        // Can't reopen a nested path after moving to a higher-level sibling
        assert_parse_errors(
            r#"
a.b.c {}
a.b.d {}
a.x {}
a.b.e {}
^^^^^ ReopenedPath
"#,
        );
    }

    // parser2[verify entry.path.reopen]
    #[test]
    fn test_nest_into_scalar_error() {
        // Can't nest into a path that has a scalar value
        assert_parse_errors(
            r#"
a.b value
a.b.c deep
^^^^^ NestIntoTerminal
"#,
        );
    }

    // parser2[verify entry.path.sibling]
    #[test]
    fn test_different_top_level_paths_ok() {
        // Different top-level paths don't conflict
        assert_parse_errors(
            r#"server.host localhost
database.port 5432"#,
        );
    }

    // parser2[verify entry.whitespace]
    #[test]
    fn test_bare_key_requires_whitespace_before_brace() {
        // `key{}` without whitespace should be an error
        assert_parse_errors(
            r#"
config{}
      ^ MissingWhitespaceBeforeBlock
"#,
        );
    }

    // parser2[verify entry.whitespace]
    #[test]
    fn test_bare_key_requires_whitespace_before_paren() {
        // `key()` without whitespace should be an error
        assert_parse_errors(
            r#"
items(1 2 3)
     ^ MissingWhitespaceBeforeBlock
"#,
        );
    }

    // parser2[verify entry.whitespace]
    #[test]
    fn test_bare_key_with_whitespace_before_brace_ok() {
        // `key {}` with whitespace should be fine - no errors
        assert_parse_errors("config {}");
    }

    // parser2[verify entry.whitespace]
    #[test]
    fn test_bare_key_with_whitespace_before_paren_ok() {
        // `key ()` with whitespace should be fine - no errors
        assert_parse_errors("items (1 2 3)");
    }

    // parser2[verify entry.whitespace]
    #[test]
    fn test_tag_with_brace_no_whitespace_ok() {
        // `@tag{}` (tag with object payload) should NOT require whitespace - no errors
        assert_parse_errors("config @object{}");
    }

    // parser2[verify entry.whitespace]
    #[test]
    fn test_quoted_key_no_whitespace_ok() {
        // `"key"{}` - quoted keys don't have this restriction - no errors
        assert_parse_errors(r#""config"{}"#);
    }

    // parser2[verify entry.whitespace]
    #[test]
    fn test_minified_styx_with_whitespace() {
        // Minified Styx should work with required whitespace - no errors
        assert_parse_errors("{server {host localhost,port 8080}}");
    }

    #[test]
    fn test_missing_comma_rejected() {
        // Too many atoms, need a comma between localhost and port
        assert_parse_errors(
            r#"
{server {host localhost port 8080}}
                        ^^^^ TooManyAtoms
"#,
        );
    }

    // Example: annotation-style error testing for various error types

    #[test]
    fn test_invalid_escape_annotated() {
        assert_parse_errors(
            r#"
x "\0"
   ^^ InvalidEscape
"#,
        );
    }

    #[test]
    fn test_mixed_separators_annotated() {
        // Error is at the newline where we switch from comma to newline mode
        assert_parse_errors(
            r#"
{a 1, b 2
         ^ MixedSeparators
c 3}
"#,
        );
    }

    #[test]
    fn test_invalid_tag_name_annotated() {
        assert_parse_errors(
            r#"
x @123
   ^^^ InvalidTagName
"#,
        );
    }

    #[test]
    fn test_dangling_doc_comment_annotated() {
        assert_parse_errors(
            r#"
foo bar
/// dangling
^^^^^^^^^^^^ DanglingDocComment
"#,
        );
    }

    // Parser2-specific tests

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
