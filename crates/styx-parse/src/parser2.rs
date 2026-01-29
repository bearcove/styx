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
    /// After tag with implicit unit where boundary was already consumed.
    AfterTagBoundaryConsumed {
        kind: TokenKind,
        span: Span,
    },
    /// Emitting remaining segments of a dotted path.
    EmitDottedPath {
        /// The segments remaining to emit (not including the first which was already emitted).
        segments: Vec<(Span, String)>,
        /// Index of current segment being processed.
        current_idx: usize,
        /// Number of ObjectStarts emitted (need to close this many).
        depth: usize,
    },
    /// After the last segment of a dotted path - expecting value.
    AfterDottedPathKey {
        /// Number of nested objects to close after value.
        depth: usize,
        /// Full path for validation when we see the value.
        path: Vec<String>,
        /// Span of the full dotted path for error reporting.
        path_span: Span,
    },
    /// After dotted path value - need to close nested objects.
    CloseDottedPath {
        /// Number of remaining objects to close.
        remaining: usize,
    },
    ExpectSeqElem,
    Done,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Context {
    Object { implicit: bool },
    Sequence,
    AttrObject,
}

/// Whether a path leads to an object (can have children) or a terminal value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathValueKind {
    Object,
    Terminal,
}

/// Error returned when path validation fails.
#[derive(Debug)]
enum PathError {
    Duplicate { original: Span },
    Reopened { closed_path: Vec<String> },
    NestIntoTerminal { terminal_path: Vec<String> },
}

/// Tracks dotted path state for sibling detection and reopen errors.
#[derive(Default)]
struct PathState {
    current_path: Vec<String>,
    closed_paths: std::collections::HashSet<Vec<String>>,
    assigned_paths: std::collections::HashMap<Vec<String>, (Span, PathValueKind)>,
}

impl PathState {
    fn check_and_update(
        &mut self,
        path: &[String],
        span: Span,
        value_kind: PathValueKind,
    ) -> Result<(), PathError> {
        // Check for duplicate (exact same path)
        if let Some(&(original, _)) = self.assigned_paths.get(path) {
            return Err(PathError::Duplicate { original });
        }

        // Check if any proper prefix is closed or has a terminal value
        for i in 1..path.len() {
            let prefix = &path[..i];
            if self.closed_paths.contains(prefix) {
                return Err(PathError::Reopened {
                    closed_path: prefix.to_vec(),
                });
            }
            if let Some(&(_, PathValueKind::Terminal)) = self.assigned_paths.get(prefix) {
                return Err(PathError::NestIntoTerminal {
                    terminal_path: prefix.to_vec(),
                });
            }
        }

        // Find common prefix length with current path
        let common_len = self
            .current_path
            .iter()
            .zip(path.iter())
            .take_while(|(a, b)| a == b)
            .count();

        // Close paths beyond the common prefix
        for i in common_len..self.current_path.len() {
            let closed: Vec<String> = self.current_path[..=i].to_vec();
            self.closed_paths.insert(closed);
        }

        // Record intermediate path segments as objects (if not already assigned)
        for i in 1..path.len() {
            let prefix = path[..i].to_vec();
            self.assigned_paths
                .entry(prefix)
                .or_insert((span, PathValueKind::Object));
        }

        // Update assigned paths and current path
        self.assigned_paths
            .insert(path.to_vec(), (span, value_kind));
        self.current_path = path.to_vec();

        Ok(())
    }
}

pub struct Parser2<'src> {
    input: &'src str,
    lexer: Lexer<'src>,
    state: State,
    context_stack: Vec<Context>,
    event_queue: VecDeque<Event<'src>>,
    pending_doc: Vec<(Span, &'src str)>,
    expr_mode: bool,
    path_state: PathState,
    /// Keys seen in current object scope for duplicate detection.
    /// Maps normalized key string to its first occurrence span.
    keys_in_scope: Vec<std::collections::HashMap<String, Span>>,
    /// Separator for current object scope (for mixed separator detection).
    separators_in_scope: Vec<Option<Separator>>,
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
            path_state: PathState::default(),
            keys_in_scope: Vec::new(),
            separators_in_scope: Vec::new(),
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
            path_state: PathState::default(),
            keys_in_scope: Vec::new(),
            separators_in_scope: Vec::new(),
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
            State::AfterTagBoundaryConsumed { kind, span } => {
                self.step_after_tag_boundary_consumed(kind, span)
            }
            State::EmitDottedPath {
                segments,
                current_idx,
                depth,
            } => self.step_emit_dotted_path(segments, current_idx, depth),
            State::AfterDottedPathKey {
                depth,
                path,
                path_span,
            } => self.step_after_dotted_path_key(depth, path, path_span),
            State::CloseDottedPath { remaining } => self.step_close_dotted_path(remaining),
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
            if t.kind == TokenKind::Newline {
                // Track newline as separator for mixed separator detection
                self.track_separator(Separator::Newline, t.span);
                continue;
            }
            return t;
        }
    }

    /// Track a separator and emit error if mixing separator types.
    fn track_separator(&mut self, sep: Separator, span: Span) {
        if let Some(current) = self.separators_in_scope.last_mut() {
            match *current {
                None => *current = Some(sep),
                Some(existing) if existing != sep => {
                    // Mixed separators!
                    self.event_queue.push_back(Event::Error {
                        span,
                        kind: ParseErrorKind::MixedSeparators,
                    });
                }
                _ => {} // Same separator, fine
            }
        }
    }

    fn span_text(&self, span: Span) -> &'src str {
        &self.input[span.start as usize..span.end as usize]
    }

    fn emit_path_error(&self, err: PathError, span: Span) -> Event<'src> {
        let kind = match err {
            PathError::Duplicate { original } => ParseErrorKind::DuplicateKey { original },
            PathError::Reopened { closed_path } => ParseErrorKind::ReopenedPath { closed_path },
            PathError::NestIntoTerminal { terminal_path } => {
                ParseErrorKind::NestIntoTerminal { terminal_path }
            }
        };
        Event::Error { span, kind }
    }

    // === Key scope management for duplicate detection ===

    fn push_key_scope(&mut self) {
        self.keys_in_scope.push(std::collections::HashMap::new());
    }

    fn pop_key_scope(&mut self) {
        self.keys_in_scope.pop();
    }

    /// Normalize a key for duplicate detection.
    /// Returns a string that represents the key's identity.
    fn normalize_key(&self, tag: Option<&str>, payload: Option<&Cow<'src, str>>) -> String {
        match (tag, payload) {
            (None, None) => "@".to_string(),                     // unit key
            (Some(name), None) => format!("@{}", name),          // tagged without payload
            (Some(name), Some(p)) => format!("@{}:{}", name, p), // tagged with payload
            (None, Some(p)) => p.to_string(),                    // regular key
        }
    }

    /// Check for duplicate key and record it. Returns Some(error) if duplicate.
    fn check_duplicate_key(
        &mut self,
        span: Span,
        tag: Option<&str>,
        payload: Option<&Cow<'src, str>>,
    ) -> Option<Event<'src>> {
        let normalized = self.normalize_key(tag, payload);

        if let Some(scope) = self.keys_in_scope.last_mut() {
            if let Some(&original) = scope.get(&normalized) {
                return Some(Event::Error {
                    span,
                    kind: ParseErrorKind::DuplicateKey { original },
                });
            }
            scope.insert(normalized, span);
        }
        None
    }

    /// Emit a Key event and check for duplicates. Queues error if duplicate.
    fn emit_key(
        &mut self,
        span: Span,
        tag: Option<&'src str>,
        payload: Option<Cow<'src, str>>,
        kind: ScalarKind,
    ) -> Event<'src> {
        if let Some(err) = self.check_duplicate_key(span, tag, payload.as_ref()) {
            self.event_queue.push_back(err);
        }
        Event::Key {
            span,
            tag,
            payload,
            kind,
        }
    }

    /// Queue a Key event and check for duplicates.
    fn queue_key(
        &mut self,
        span: Span,
        tag: Option<&'src str>,
        payload: Option<Cow<'src, str>>,
        kind: ScalarKind,
    ) {
        if let Some(err) = self.check_duplicate_key(span, tag, payload.as_ref()) {
            self.event_queue.push_back(err);
        }
        self.event_queue.push_back(Event::Key {
            span,
            tag,
            payload,
            kind,
        });
    }

    /// Push an object context and associated key scope.
    fn push_object_context(&mut self, implicit: bool) {
        self.context_stack.push(Context::Object { implicit });
        self.push_key_scope();
        self.separators_in_scope.push(None);
    }

    /// Push an attribute object context and associated key scope.
    fn push_attr_object_context(&mut self) {
        self.context_stack.push(Context::AttrObject);
        self.push_key_scope();
        self.separators_in_scope.push(None);
    }

    /// Pop context and if it was an object, also pop key scope and separator scope.
    fn pop_context(&mut self) -> Option<Context> {
        let ctx = self.context_stack.pop();
        if matches!(
            ctx,
            Some(Context::Object { .. }) | Some(Context::AttrObject)
        ) {
            self.pop_key_scope();
            self.separators_in_scope.pop();
        }
        ctx
    }

    // === State handlers ===

    fn step_before_root(&mut self) -> Option<Event<'src>> {
        let t = self.next_token();

        match t.kind {
            TokenKind::Eof => {
                if !self.expr_mode {
                    self.push_object_context(true);
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
                self.push_object_context(false);
                self.state = State::ExpectEntry;
                Some(Event::ObjectStart {
                    span: t.span,
                    separator: Separator::Comma,
                })
            }

            // Implicit root
            TokenKind::BareScalar => {
                self.push_object_context(true);
                self.emit_pending_docs();
                self.event_queue.push_back(Event::EntryStart);
                self.event_queue.push_back(Event::ObjectStart {
                    span: Span::new(0, 0),
                    separator: Separator::Newline,
                });

                // Check for dotted path
                if t.text.contains('.') {
                    if let Some(segments) = self.parse_dotted_path(t.text, t.span) {
                        return self.start_dotted_path(segments, t.span);
                    } else {
                        // Invalid dotted path
                        self.state = State::ExpectEntry;
                        return Some(Event::Error {
                            span: t.span,
                            kind: ParseErrorKind::InvalidKey,
                        });
                    }
                }

                // Regular bare key
                self.state = State::AfterBareKey { key_span: t.span };
                Some(self.emit_key(t.span, None, Some(Cow::Borrowed(t.text)), ScalarKind::Bare))
            }

            TokenKind::QuotedScalar => {
                self.push_object_context(true);
                self.emit_pending_docs();
                let payload = self.unescape_quoted(t.text);
                self.queue_key(t.span, None, Some(payload), ScalarKind::Quoted);
                self.state = State::AfterKey { key_span: t.span };
                self.event_queue.push_back(Event::EntryStart);
                Some(Event::ObjectStart {
                    span: Span::new(0, 0),
                    separator: Separator::Newline,
                })
            }

            TokenKind::At => {
                self.push_object_context(true);
                self.state = State::AfterAtKey { at_span: t.span };
                self.event_queue.push_back(Event::EntryStart);
                Some(Event::ObjectStart {
                    span: Span::new(0, 0),
                    separator: Separator::Newline,
                })
            }

            TokenKind::HeredocStart => {
                // Heredoc as key is invalid
                self.push_object_context(true);
                self.skip_heredoc();
                self.state = State::ExpectEntry;
                self.event_queue.push_back(Event::Error {
                    span: t.span,
                    kind: ParseErrorKind::InvalidKey,
                });
                Some(Event::ObjectStart {
                    span: Span::new(0, 0),
                    separator: Separator::Newline,
                })
            }

            _ => {
                self.push_object_context(true);
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

            TokenKind::Comma => {
                self.track_separator(Separator::Comma, t.span);
                None
            }

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

                // Check for dotted path
                if t.text.contains('.') {
                    if let Some(segments) = self.parse_dotted_path(t.text, t.span) {
                        self.event_queue.push_back(Event::EntryStart);
                        return self.start_dotted_path(segments, t.span);
                    } else {
                        // Invalid dotted path
                        return Some(Event::Error {
                            span: t.span,
                            kind: ParseErrorKind::InvalidKey,
                        });
                    }
                }

                // Regular bare key
                self.queue_key(t.span, None, Some(Cow::Borrowed(t.text)), ScalarKind::Bare);
                self.state = State::AfterBareKey { key_span: t.span };
                Some(Event::EntryStart)
            }

            TokenKind::QuotedScalar => {
                self.emit_pending_docs();
                let payload = self.unescape_quoted(t.text);
                self.queue_key(t.span, None, Some(payload), ScalarKind::Quoted);
                self.state = State::AfterKey { key_span: t.span };
                Some(Event::EntryStart)
            }

            TokenKind::RawScalar => {
                self.emit_pending_docs();
                let payload = Cow::Borrowed(Self::strip_raw_delimiters(t.text));
                self.queue_key(t.span, None, Some(payload), ScalarKind::Raw);
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
                self.push_object_context(false);
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
                // parse_tag_value sets state to AfterTagBoundaryConsumed if it consumed a boundary
                if !matches!(self.state, State::AfterTagBoundaryConsumed { .. }) {
                    self.state = State::AfterValue;
                }
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

            let key_span = Span::new(at_span.start, name_end);
            self.queue_key(key_span, Some(tag_name), None, ScalarKind::Bare);
            self.state = State::AfterKey { key_span };
            return None;
        }

        // @ alone = unit key
        self.queue_key(at_span, None, None, ScalarKind::Bare);

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
                self.push_object_context(false);
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
                // parse_tag_value sets state to AfterTagBoundaryConsumed if it consumed a boundary
                if !matches!(self.state, State::AfterTagBoundaryConsumed { .. }) {
                    self.state = State::AfterValue;
                }
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
                let key_text = Cow::Borrowed(self.span_text(key_span));
                self.queue_key(key_span, None, Some(key_text), ScalarKind::Bare);
                self.event_queue.push_back(Event::Scalar {
                    span: t.span,
                    value: Cow::Borrowed(t.text),
                    kind: ScalarKind::Bare,
                });
                self.event_queue.push_back(Event::EntryEnd);

                if !in_chain {
                    // First attribute - we need to emit ObjectStart
                    self.push_attr_object_context();
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
                let key_text = Cow::Borrowed(self.span_text(key_span));
                self.queue_key(key_span, None, Some(key_text), ScalarKind::Bare);
                self.event_queue.push_back(Event::Scalar {
                    span: t.span,
                    value: self.unescape_quoted(t.text),
                    kind: ScalarKind::Quoted,
                });
                self.event_queue.push_back(Event::EntryEnd);

                if !in_chain {
                    self.push_attr_object_context();
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
                let key_text = Cow::Borrowed(self.span_text(key_span));
                self.queue_key(key_span, None, Some(key_text), ScalarKind::Bare);
                self.event_queue.push_back(Event::Scalar {
                    span: t.span,
                    value: Cow::Borrowed(Self::strip_raw_delimiters(t.text)),
                    kind: ScalarKind::Raw,
                });
                self.event_queue.push_back(Event::EntryEnd);

                if !in_chain {
                    self.push_attr_object_context();
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
                let key_text = Cow::Borrowed(self.span_text(key_span));
                self.queue_key(key_span, None, Some(key_text), ScalarKind::Bare);

                if !in_chain {
                    self.push_attr_object_context();
                    self.event_queue.push_back(Event::ObjectStart {
                        span: key_span,
                        separator: Separator::Comma,
                    });
                }

                self.push_object_context(false);
                self.state = State::ExpectEntry;
                Some(Event::ObjectStart {
                    span: t.span,
                    separator: Separator::Comma,
                })
            }

            TokenKind::LParen => {
                // key>(...)
                self.event_queue.push_back(Event::EntryStart);
                let key_text = Cow::Borrowed(self.span_text(key_span));
                self.queue_key(key_span, None, Some(key_text), ScalarKind::Bare);

                if !in_chain {
                    self.push_attr_object_context();
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
                let key_text = Cow::Borrowed(self.span_text(key_span));
                self.queue_key(key_span, None, Some(key_text), ScalarKind::Bare);

                let tag_ev = self.parse_tag_value(t);
                self.event_queue.push_back(tag_ev);
                self.event_queue.push_back(Event::EntryEnd);

                if !in_chain {
                    self.push_attr_object_context();
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
                self.push_attr_object_context();
                self.event_queue.push_back(Event::EntryStart);
                let key_text = Cow::Borrowed(self.span_text(value_span));
                self.queue_key(value_span, None, Some(key_text), ScalarKind::Bare);
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
            TokenKind::Newline => {
                self.track_separator(Separator::Newline, t.span);
                self.event_queue.push_back(Event::EntryEnd);
                self.state = State::ExpectEntry;
                self.handle_boundary_token(t)
            }

            TokenKind::Comma => {
                self.track_separator(Separator::Comma, t.span);
                self.event_queue.push_back(Event::EntryEnd);
                self.state = State::ExpectEntry;
                self.handle_boundary_token(t)
            }

            TokenKind::Eof | TokenKind::RBrace | TokenKind::RParen => {
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
            TokenKind::Newline => {
                self.track_separator(Separator::Newline, t.span);
                self.event_queue.push_back(Event::EntryEnd);
                self.state = State::ExpectEntry;
                self.handle_boundary_token(t)
            }

            TokenKind::Comma => {
                self.track_separator(Separator::Comma, t.span);
                self.event_queue.push_back(Event::EntryEnd);
                self.state = State::ExpectEntry;
                self.handle_boundary_token(t)
            }

            TokenKind::Eof | TokenKind::RBrace | TokenKind::RParen => {
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
                self.push_object_context(false);
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

        if let Some(ctx) = self.pop_context() {
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

        match self.pop_context() {
            Some(Context::Object { implicit: false }) => {
                self.state = self.state_after_close();
                Some(Event::ObjectEnd { span })
            }
            Some(ctx) => {
                // Push back - this wasn't the right context to close
                self.context_stack.push(ctx);
                if matches!(ctx, Context::Object { .. } | Context::AttrObject) {
                    self.push_key_scope(); // Restore the scope we popped
                }
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
        self.pop_context(); // AttrObject - pops key scope too
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
                        self.push_object_context(false);
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
        // We consumed a token after tag that we didn't use for the tag.
        // Transition to a state that knows which boundary was consumed.
        match t.kind {
            TokenKind::Newline
            | TokenKind::Comma
            | TokenKind::Eof
            | TokenKind::RBrace
            | TokenKind::RParen => {
                // Boundary token - let the state machine handle it
                self.state = State::AfterTagBoundaryConsumed {
                    kind: t.kind,
                    span: t.span,
                };
            }
            _ => {
                // TooManyAtoms - skip to boundary and handle that
                self.event_queue.push_back(Event::Error {
                    span: t.span,
                    kind: ParseErrorKind::TooManyAtoms,
                });
                let boundary = self.skip_to_boundary();
                self.state = State::AfterTagBoundaryConsumed {
                    kind: boundary.kind,
                    span: boundary.span,
                };
            }
        }
    }

    fn step_after_tag_boundary_consumed(
        &mut self,
        kind: TokenKind,
        span: Span,
    ) -> Option<Event<'src>> {
        // We already consumed a boundary token after a tag value.
        // Emit EntryEnd and handle the boundary.
        self.event_queue.push_back(Event::EntryEnd);
        self.state = State::ExpectEntry;

        match kind {
            TokenKind::Newline | TokenKind::Comma => {
                // Simple boundary, just continue
                None
            }
            TokenKind::Eof => self.close_at_eof(),
            TokenKind::RBrace => self.handle_rbrace(span),
            TokenKind::RParen => Some(Event::Error {
                span,
                kind: ParseErrorKind::UnexpectedToken,
            }),
            _ => None, // Shouldn't happen
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

    // === Dotted path handling ===

    /// Parse segments from a dotted path like "a.b.c".
    /// Returns None if the path is invalid (empty segments, leading/trailing dots).
    fn parse_dotted_path(&self, text: &str, span: Span) -> Option<Vec<(Span, String)>> {
        let segments: Vec<&str> = text.split('.').collect();

        // Validate: no empty segments
        if segments.is_empty() || segments.iter().any(|s| s.is_empty()) {
            return None;
        }

        // Calculate spans for each segment
        let mut result = Vec::with_capacity(segments.len());
        let mut offset = span.start;
        for segment in segments {
            let segment_len = segment.len() as u32;
            let segment_span = Span::new(offset, offset + segment_len);
            result.push((segment_span, segment.to_string()));
            offset += segment_len + 1; // +1 for the dot
        }

        Some(result)
    }

    /// Start emitting a dotted path. Emits first key and transitions to EmitDottedPath state.
    fn start_dotted_path(
        &mut self,
        segments: Vec<(Span, String)>,
        full_span: Span,
    ) -> Option<Event<'src>> {
        if segments.is_empty() {
            return Some(Event::Error {
                span: full_span,
                kind: ParseErrorKind::InvalidKey,
            });
        }

        let (first_span, first_segment) = segments[0].clone();

        if segments.len() == 1 {
            // Not actually dotted, just a regular key
            self.state = State::AfterBareKey {
                key_span: first_span,
            };
            return Some(self.emit_key(
                first_span,
                None,
                Some(Cow::Owned(first_segment)),
                ScalarKind::Bare,
            ));
        }

        // Multiple segments - emit first key and ObjectStart, then continue
        self.event_queue.push_back(Event::ObjectStart {
            span: first_span,
            separator: Separator::Newline,
        });

        self.state = State::EmitDottedPath {
            segments,
            current_idx: 1,
            depth: 1,
        };

        Some(self.emit_key(
            first_span,
            None,
            Some(Cow::Owned(first_segment)),
            ScalarKind::Bare,
        ))
    }

    fn step_emit_dotted_path(
        &mut self,
        segments: Vec<(Span, String)>,
        current_idx: usize,
        depth: usize,
    ) -> Option<Event<'src>> {
        let (span, segment) = segments[current_idx].clone();

        // Emit EntryStart for this nested entry
        self.event_queue.push_back(Event::EntryStart);

        if current_idx == segments.len() - 1 {
            // Last segment - emit key and transition to expecting value
            // Build full path for validation
            let path: Vec<String> = segments.iter().map(|(_, s)| s.clone()).collect();
            let path_span = Span::new(segments[0].0.start, segments.last().unwrap().0.end);
            self.state = State::AfterDottedPathKey {
                depth,
                path,
                path_span,
            };
            Some(self.emit_key(span, None, Some(Cow::Owned(segment)), ScalarKind::Bare))
        } else {
            // Not last - emit key, ObjectStart, and continue
            self.event_queue.push_back(Event::ObjectStart {
                span,
                separator: Separator::Newline,
            });
            self.state = State::EmitDottedPath {
                segments,
                current_idx: current_idx + 1,
                depth: depth + 1,
            };
            Some(self.emit_key(span, None, Some(Cow::Owned(segment)), ScalarKind::Bare))
        }
    }

    fn step_after_dotted_path_key(
        &mut self,
        depth: usize,
        path: Vec<String>,
        path_span: Span,
    ) -> Option<Event<'src>> {
        // Expecting value after the last segment of dotted path
        let t = self.next_token();

        // Determine value kind based on token
        let value_kind = match t.kind {
            TokenKind::LBrace | TokenKind::LParen => PathValueKind::Object,
            _ => PathValueKind::Terminal,
        };

        // Validate path before emitting value
        if let Err(err) = self
            .path_state
            .check_and_update(&path, path_span, value_kind)
        {
            // Emit error and skip the value we already peeked at
            let error_event = self.emit_path_error(err, path_span);

            // Skip the value token(s) we already consumed the opening for
            match t.kind {
                TokenKind::LBrace => self.skip_nested(TokenKind::RBrace),
                TokenKind::LParen => self.skip_nested(TokenKind::RParen),
                _ => {} // Scalar values are already consumed
            }

            // Need to emit EntryEnd and close nested objects
            self.event_queue.push_back(Event::EntryEnd);
            self.state = State::CloseDottedPath {
                remaining: depth - 1,
            };
            return Some(error_event);
        }

        match t.kind {
            TokenKind::Newline
            | TokenKind::Eof
            | TokenKind::RBrace
            | TokenKind::RParen
            | TokenKind::Comma => {
                // Implicit unit value
                self.event_queue.push_back(Event::Unit {
                    span: Span::new(t.span.start, t.span.start),
                });
                self.event_queue.push_back(Event::EntryEnd);
                self.state = State::CloseDottedPath {
                    remaining: depth - 1,
                };
                self.handle_boundary_token(t)
            }

            TokenKind::LBrace => {
                self.push_object_context(false);
                self.state = State::ExpectEntry;
                // After the object closes, we'll need to close the dotted path objects
                // This is handled by the context stack
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
                if !matches!(self.state, State::AfterTagBoundaryConsumed { .. }) {
                    self.state = State::AfterValue;
                }
                Some(ev)
            }

            TokenKind::BareScalar => {
                // Check for attribute syntax
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
                self.state = State::AfterValue;
                Some(self.parse_heredoc(t.span))
            }

            _ => Some(Event::Error {
                span: t.span,
                kind: ParseErrorKind::ExpectedValue,
            }),
        }
    }

    fn step_close_dotted_path(&mut self, remaining: usize) -> Option<Event<'src>> {
        if remaining == 0 {
            self.state = State::ExpectEntry;
            return None;
        }

        // Close one object and one entry
        self.event_queue.push_back(Event::EntryEnd);
        self.state = State::CloseDottedPath {
            remaining: remaining - 1,
        };
        Some(Event::ObjectEnd {
            span: Span::new(0, 0),
        })
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

    // Additional tests from old Parser

    #[test]
    fn test_doc_comment_followed_by_entry_ok() {
        assert_parse_errors("/// documentation\nkey value");
    }

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

    #[test]
    fn test_multiple_doc_comments_before_entry_ok() {
        assert_parse_errors("/// line 1\n/// line 2\nkey value");
    }

    #[test]
    fn test_object_with_entries() {
        let events = parse("config {host localhost, port 8080}");
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
        assert!(keys.contains(&"config"));
        assert!(keys.contains(&"host"));
        assert!(keys.contains(&"port"));
    }

    #[test]
    fn test_nested_sequences() {
        let events = parse("matrix ((1 2) (3 4))");
        let seq_starts = events
            .iter()
            .filter(|e| matches!(e, Event::SequenceStart { .. }))
            .count();
        assert_eq!(seq_starts, 3);
    }

    #[test]
    fn test_tagged_sequence() {
        let events = parse("color @rgb(255 128 0)");
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::TagStart { name, .. } if *name == "rgb"))
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::SequenceStart { .. }))
        );
    }

    #[test]
    fn test_tagged_scalar() {
        let events = parse(r#"name @nickname"Bob""#);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::TagStart { name, .. } if *name == "nickname"))
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::Scalar { value, .. } if value == "Bob"))
        );
    }

    #[test]
    fn test_tag_whitespace_gap() {
        let events = parse("x @tag\ny {a b}");
        let tag_events: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, Event::TagStart { .. } | Event::TagEnd))
            .collect();
        assert_eq!(tag_events.len(), 2);
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
        assert!(keys.contains(&"x"));
        assert!(keys.contains(&"y"));
    }

    #[test]
    fn test_object_in_sequence() {
        let events = parse("servers ({host a} {host b})");
        let obj_starts = events
            .iter()
            .filter(|e| matches!(e, Event::ObjectStart { .. }))
            .count();
        // 3 = implicit root + 2 objects in sequence
        assert_eq!(obj_starts, 3);
    }

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
        assert!(keys.contains(&"config"));
        assert!(keys.contains(&"name"));
        assert!(keys.contains(&"tags"));
        assert!(keys.contains(&"opts"));
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::SequenceStart { .. }))
        );
    }

    #[test]
    fn test_too_many_atoms_with_attributes() {
        assert_parse_errors(
            r#"
spec selector matchLabels app>web tier>frontend
              ^^^^^^^^^^^ TooManyAtoms
"#,
        );
    }

    #[test]
    fn test_attribute_no_spaces() {
        let events = parse("x > y");
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
        assert!(keys.contains(&"x"));
    }

    #[test]
    fn test_explicit_root_after_comment() {
        let events = parse("// comment\n{a 1}");
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::ObjectStart { .. }))
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::Key { payload: Some(value), .. } if value == "a"))
        );
    }

    #[test]
    fn test_explicit_root_after_doc_comment() {
        let events = parse("/// doc comment\n{a 1}");
        assert!(events.iter().any(|e| matches!(e, Event::DocComment { .. })));
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::ObjectStart { .. }))
        );
    }

    #[test]
    fn test_duplicate_bare_key() {
        assert_parse_errors(
            r#"
{a 1, a 2}
      ^ DuplicateKey
"#,
        );
    }

    #[test]
    fn test_duplicate_quoted_key() {
        assert_parse_errors(
            r#"
{"key" 1, "key" 2}
          ^^^^^ DuplicateKey
"#,
        );
    }

    #[test]
    fn test_duplicate_key_escape_normalized() {
        assert_parse_errors(
            r#"
{"ab" 1, "a\u{62}" 2}
         ^^^^^^^^^ DuplicateKey
"#,
        );
    }

    #[test]
    fn test_duplicate_unit_key() {
        assert_parse_errors(
            r#"
{@ 1, @ 2}
      ^ DuplicateKey
"#,
        );
    }

    #[test]
    fn test_duplicate_tagged_key() {
        assert_parse_errors(
            r#"
{@foo 1, @foo 2}
         ^^^^ DuplicateKey
"#,
        );
    }

    #[test]
    fn test_different_keys_ok() {
        assert_parse_errors(r#"{a 1, b 2, c 3}"#);
    }

    #[test]
    fn test_duplicate_key_at_root() {
        assert_parse_errors(
            r#"
a 1
a 2
^ DuplicateKey
"#,
        );
    }

    #[test]
    fn test_mixed_separators_comma_then_newline() {
        assert_parse_errors(
            r#"
{a 1, b 2
         ^ MixedSeparators
c 3}
"#,
        );
    }

    #[test]
    fn test_mixed_separators_newline_then_comma() {
        assert_parse_errors(
            r#"
{a 1
b 2, c 3}
   ^ MixedSeparators
"#,
        );
    }

    #[test]
    fn test_consistent_comma_separators() {
        assert_parse_errors(r#"{a 1, b 2, c 3}"#);
    }

    #[test]
    fn test_consistent_newline_separators() {
        assert_parse_errors(
            r#"{a 1
b 2
c 3}"#,
        );
    }

    #[test]
    fn test_valid_tag_names() {
        assert_parse_errors("@foo");
        assert_parse_errors("@_private");
        assert_parse_errors("@my-tag");
        assert_parse_errors("@Type123");
    }

    #[test]
    fn test_invalid_tag_name_starts_with_hyphen() {
        assert_parse_errors(
            r#"
x @-foo
   ^^^^ InvalidTagName
"#,
        );
    }

    #[test]
    fn test_invalid_tag_name_starts_with_dot() {
        assert_parse_errors(
            r#"
x @.foo
   ^^^^ InvalidTagName
"#,
        );
    }

    #[test]
    fn test_unicode_escape_4digit_accented() {
        let events = parse(r#"x "\u00E9""#);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::Scalar { value, .. } if value == "é"))
        );
    }

    #[test]
    fn test_unicode_escape_mixed() {
        let events = parse(r#"x "\u0048\u{65}\u006C\u{6C}\u006F""#);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::Scalar { value, .. } if value == "Hello"))
        );
    }

    #[test]
    fn test_invalid_escape_null() {
        assert_parse_errors(
            r#"
x "\0"
   ^^ InvalidEscape
"#,
        );
    }

    #[test]
    fn test_invalid_escape_unknown() {
        assert_parse_errors(
            r#"
x "\q"
   ^^ InvalidEscape
"#,
        );
    }

    #[test]
    fn test_invalid_escape_multiple() {
        assert_parse_errors(
            r#"
x "\0\q\?"
   ^^ InvalidEscape
     ^^ InvalidEscape
       ^^ InvalidEscape
"#,
        );
    }

    #[test]
    fn test_valid_escapes_still_work() {
        let events = parse(r#"x "a\nb\tc\\d\"e""#);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::Scalar { value, .. } if value == "a\nb\tc\\d\"e"))
        );
        assert_parse_errors(r#"x "a\nb\tc\\d\"e""#);
    }

    #[test]
    fn test_invalid_escape_in_key() {
        assert_parse_errors(
            r#"
"\0" value
 ^^ InvalidEscape
"#,
        );
    }

    #[test]
    fn test_simple_key_value_with_attributes() {
        let events = parse("server host>localhost port>8080");
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
        assert!(keys.contains(&"server"));
        assert!(keys.contains(&"host"));
        assert!(keys.contains(&"port"));
        assert_parse_errors(r#"server host>localhost port>8080"#);
    }

    #[test]
    fn test_dotted_path_simple() {
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
        assert_eq!(keys, vec!["a", "b"]);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::ObjectStart { .. }))
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::Scalar { value, .. } if value == "value"))
        );
        assert_parse_errors(r#"a.b value"#);
    }

    #[test]
    fn test_dotted_path_three_segments() {
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
        assert_eq!(keys, vec!["a", "b", "c"]);
        let obj_starts: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, Event::ObjectStart { .. }))
            .collect();
        // 3 = implicit root + 2 from dotted path (a { b { c deep } })
        assert_eq!(obj_starts.len(), 3);
        assert_parse_errors(r#"a.b.c deep"#);
    }

    #[test]
    fn test_dotted_path_with_implicit_unit() {
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
        assert_eq!(keys, vec!["a", "b"]);
        assert!(events.iter().any(|e| matches!(e, Event::Unit { .. })));
    }

    #[test]
    fn test_dotted_path_empty_segment() {
        assert_parse_errors(
            r#"
a..b value
^^^^ InvalidKey
"#,
        );
    }

    #[test]
    fn test_dotted_path_trailing_dot() {
        assert_parse_errors(
            r#"
a.b. value
^^^^ InvalidKey
"#,
        );
    }

    #[test]
    fn test_dotted_path_leading_dot() {
        assert_parse_errors(
            r#"
.a.b value
^^^^ InvalidKey
"#,
        );
    }

    #[test]
    fn test_dotted_path_with_object_value() {
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
        assert!(keys.contains(&"a"));
        assert!(keys.contains(&"b"));
        assert!(keys.contains(&"c"));
        assert_parse_errors(r#"a.b { c d }"#);
    }

    #[test]
    fn test_dotted_path_with_attributes_value() {
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
        assert!(keys.contains(&"selector"));
        assert!(keys.contains(&"matchLabels"));
        assert!(keys.contains(&"app"));
        assert_parse_errors(r#"selector.matchLabels app>web"#);
    }

    #[test]
    fn test_dot_in_value_is_literal() {
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
        assert_eq!(keys, vec!["key"]);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::Scalar { value, .. } if value == "example.com"))
        );
        assert_parse_errors(r#"key example.com"#);
    }

    #[test]
    fn test_sibling_dotted_paths() {
        let events = parse("foo.bar.x value1\nfoo.bar.y value2\nfoo.baz value3");
        assert_parse_errors(
            r#"foo.bar.x value1
foo.bar.y value2
foo.baz value3"#,
        );
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
        assert!(keys.contains(&"foo"));
        assert!(keys.contains(&"bar"));
        assert!(keys.contains(&"baz"));
        assert!(keys.contains(&"x"));
        assert!(keys.contains(&"y"));
    }

    #[test]
    fn test_reopen_closed_path_error() {
        assert_parse_errors(
            r#"
foo.bar {}
foo.baz {}
foo.bar.x value
^^^^^^^^^ ReopenedPath
"#,
        );
    }

    #[test]
    fn test_reopen_nested_closed_path_error() {
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

    #[test]
    fn test_nest_into_scalar_error() {
        assert_parse_errors(
            r#"
a.b value
a.b.c deep
^^^^^ NestIntoTerminal
"#,
        );
    }

    #[test]
    fn test_different_top_level_paths_ok() {
        assert_parse_errors(
            r#"server.host localhost
database.port 5432"#,
        );
    }

    #[test]
    fn test_bare_key_requires_whitespace_before_brace() {
        assert_parse_errors(
            r#"
config{}
      ^ MissingWhitespaceBeforeBlock
"#,
        );
    }

    #[test]
    fn test_bare_key_requires_whitespace_before_paren() {
        assert_parse_errors(
            r#"
items(1 2 3)
     ^ MissingWhitespaceBeforeBlock
"#,
        );
    }

    #[test]
    fn test_bare_key_with_whitespace_before_brace_ok() {
        assert_parse_errors("config {}");
    }

    #[test]
    fn test_bare_key_with_whitespace_before_paren_ok() {
        assert_parse_errors("items (1 2 3)");
    }

    #[test]
    fn test_tag_with_brace_no_whitespace_ok() {
        assert_parse_errors("config @object{}");
    }

    #[test]
    fn test_quoted_key_no_whitespace_ok() {
        assert_parse_errors(r#""config"{}"#);
    }

    #[test]
    fn test_minified_styx_with_whitespace() {
        assert_parse_errors("{server {host localhost,port 8080}}");
    }

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
}
