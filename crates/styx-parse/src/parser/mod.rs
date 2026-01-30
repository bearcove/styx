//! Pull-based event parser for Styx.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet, VecDeque};

use styx_tokenizer::Span;

use crate::events::{ParseErrorKind, ScalarKind, Separator};
use crate::{Event, Lexeme, Lexer};

/// Pull-based event parser for Styx.
#[derive(Clone)]
pub struct Parser<'src> {
    input: &'src str,
    lexer: Lexer<'src>,
    stack: Vec<Frame<'src>>,
    event_queue: VecDeque<Event<'src>>,
}

/// Shared state for parsing object entries.
#[derive(Clone, Default)]
struct ObjectState {
    seen_keys: HashMap<KeyValue, Span>,
    pending_doc_comment: Option<Span>,
}

/// Parser frame for tracking nested structures.
#[derive(Clone)]
enum Frame<'src> {
    /// Haven't emitted DocumentStart yet.
    BeforeDocument,

    /// At implicit document root.
    DocumentRoot {
        state: ObjectState,
        path_state: PathState,
    },

    /// Inside explicit object { ... }.
    Object {
        start_span: Span,
        state: ObjectState,
    },

    /// Inside sequence ( ... ).
    Sequence { start_span: Span },

    /// Document ended.
    AfterDocument,
}

impl<'src> Parser<'src> {
    /// Create a new parser for the given source.
    pub fn new(source: &'src str) -> Self {
        Self {
            input: source,
            lexer: Lexer::new(source),
            stack: vec![Frame::BeforeDocument],
            event_queue: VecDeque::new(),
        }
    }

    /// Get the next event from the parser.
    pub fn next_event(&mut self) -> Option<Event<'src>> {
        // Drain queue first
        if let Some(event) = self.event_queue.pop_front() {
            return Some(event);
        }

        // Advance state machine
        self.advance()
    }

    /// Parse all events into a vector.
    pub fn parse_to_vec(mut self) -> Vec<Event<'src>> {
        let mut events = Vec::new();
        while let Some(event) = self.next_event() {
            events.push(event);
        }
        events
    }

    /// Advance the state machine, possibly queuing events.
    fn advance(&mut self) -> Option<Event<'src>> {
        loop {
            let frame = self.stack.last_mut()?;

            match frame {
                Frame::BeforeDocument => {
                    // Transition to document root
                    *frame = Frame::DocumentRoot {
                        state: ObjectState::default(),
                        path_state: PathState::default(),
                    };
                    return Some(Event::DocumentStart);
                }

                Frame::DocumentRoot { state, path_state } => {
                    // Skip whitespace and newlines, handle comments
                    loop {
                        let lexeme = self.lexer.next_lexeme();
                        match lexeme {
                            Lexeme::Eof => {
                                // Check for dangling doc comment
                                if let Some(span) = state.pending_doc_comment.take() {
                                    self.event_queue.push_back(Event::Error {
                                        span,
                                        kind: ParseErrorKind::DanglingDocComment,
                                    });
                                }
                                self.event_queue.push_back(Event::DocumentEnd);
                                *self.stack.last_mut().unwrap() = Frame::AfterDocument;
                                return self.event_queue.pop_front();
                            }
                            Lexeme::Newline { .. } | Lexeme::Comma { .. } => {
                                // Skip separators at root level
                                continue;
                            }
                            Lexeme::Comment { span, text } => {
                                return Some(Event::Comment { span, text });
                            }
                            Lexeme::DocComment { span, text } => {
                                state.pending_doc_comment = Some(span);
                                return Some(Event::DocComment { span, text });
                            }
                            Lexeme::ObjectStart { span } => {
                                // Explicit root object - check for dangling doc comment first
                                if let Some(doc_span) = state.pending_doc_comment.take() {
                                    // Doc comment before explicit root is fine, it attaches to the object
                                    self.event_queue.push_back(Event::ObjectStart {
                                        span,
                                        separator: Separator::Newline,
                                    });
                                    self.stack.push(Frame::Object {
                                        start_span: span,
                                        state: ObjectState::default(),
                                    });
                                    // Return the doc comment, object start is queued
                                    return Some(Event::DocComment {
                                        span: doc_span,
                                        text: &self.input
                                            [doc_span.start as usize..doc_span.end as usize],
                                    });
                                }
                                // Push object frame
                                self.stack.push(Frame::Object {
                                    start_span: span,
                                    state: ObjectState::default(),
                                });
                                return Some(Event::ObjectStart {
                                    span,
                                    separator: Separator::Newline,
                                });
                            }
                            _ => {
                                // Start of an entry - collect atoms
                                state.pending_doc_comment = None;
                                let atoms = self.collect_entry_atoms(lexeme);
                                if !atoms.is_empty() {
                                    self.emit_entry(&atoms, state, Some(path_state));
                                }
                                return self.event_queue.pop_front();
                            }
                        }
                    }
                }

                Frame::Object { start_span, state } => {
                    let start_span = *start_span;
                    loop {
                        let lexeme = self.lexer.next_lexeme();
                        match lexeme {
                            Lexeme::Eof => {
                                // Unclosed object
                                if let Some(span) = state.pending_doc_comment.take() {
                                    self.event_queue.push_back(Event::Error {
                                        span,
                                        kind: ParseErrorKind::DanglingDocComment,
                                    });
                                }
                                self.event_queue.push_back(Event::Error {
                                    span: start_span,
                                    kind: ParseErrorKind::UnclosedObject,
                                });
                                self.event_queue
                                    .push_back(Event::ObjectEnd { span: start_span });
                                self.stack.pop();
                                return self.event_queue.pop_front();
                            }
                            Lexeme::ObjectEnd { span } => {
                                // Check for dangling doc comment
                                if let Some(doc_span) = state.pending_doc_comment.take() {
                                    self.event_queue.push_back(Event::Error {
                                        span: doc_span,
                                        kind: ParseErrorKind::DanglingDocComment,
                                    });
                                }
                                self.stack.pop();
                                return Some(Event::ObjectEnd { span });
                            }
                            Lexeme::Newline { .. } | Lexeme::Comma { .. } => {
                                continue;
                            }
                            Lexeme::Comment { span, text } => {
                                return Some(Event::Comment { span, text });
                            }
                            Lexeme::DocComment { span, text } => {
                                state.pending_doc_comment = Some(span);
                                return Some(Event::DocComment { span, text });
                            }
                            _ => {
                                state.pending_doc_comment = None;
                                let atoms = self.collect_entry_atoms(lexeme);
                                if !atoms.is_empty() {
                                    self.emit_entry(&atoms, state, None);
                                }
                                return self.event_queue.pop_front();
                            }
                        }
                    }
                }

                Frame::Sequence { start_span } => {
                    let start_span = *start_span;
                    loop {
                        let lexeme = self.lexer.next_lexeme();
                        match lexeme {
                            Lexeme::Eof => {
                                // Unclosed sequence
                                self.event_queue.push_back(Event::Error {
                                    span: start_span,
                                    kind: ParseErrorKind::UnclosedSequence,
                                });
                                self.event_queue
                                    .push_back(Event::SequenceEnd { span: start_span });
                                self.stack.pop();
                                return self.event_queue.pop_front();
                            }
                            Lexeme::SeqEnd { span } => {
                                self.stack.pop();
                                return Some(Event::SequenceEnd { span });
                            }
                            Lexeme::Newline { .. } => {
                                continue;
                            }
                            Lexeme::Comma { span } => {
                                // Commas not allowed in sequences
                                return Some(Event::Error {
                                    span,
                                    kind: ParseErrorKind::CommaInSequence,
                                });
                            }
                            Lexeme::Comment { span, text } => {
                                return Some(Event::Comment { span, text });
                            }
                            Lexeme::DocComment { span, text } => {
                                // Doc comments in sequences are just emitted
                                return Some(Event::DocComment { span, text });
                            }
                            _ => {
                                // Parse single element
                                let atom = self.parse_atom(lexeme);
                                self.emit_atom_as_value(&atom);
                                return self.event_queue.pop_front();
                            }
                        }
                    }
                }

                Frame::AfterDocument => {
                    return None;
                }
            }
        }
    }

    /// Collect atoms for an entry until a boundary (newline, comma, closing delimiter, EOF).
    fn collect_entry_atoms(&mut self, first: Lexeme<'src>) -> Vec<Atom<'src>> {
        let mut atoms = Vec::new();

        // Parse first atom
        let first_atom = self.parse_atom(first);
        atoms.push(first_atom);

        // Collect more atoms until boundary
        loop {
            let lexeme = self.lexer.next_lexeme();
            match lexeme {
                Lexeme::Eof
                | Lexeme::Newline { .. }
                | Lexeme::Comma { .. }
                | Lexeme::ObjectEnd { .. }
                | Lexeme::SeqEnd { .. }
                | Lexeme::Comment { .. }
                | Lexeme::DocComment { .. } => {
                    // Put it back conceptually - we need to re-process this
                    // Actually, we can't put it back, so we need a different approach
                    // For now, handle this by pushing back to queue
                    match lexeme {
                        Lexeme::Comment { span, text } => {
                            self.event_queue.push_back(Event::Comment { span, text });
                        }
                        Lexeme::DocComment { span, text } => {
                            self.event_queue.push_back(Event::DocComment { span, text });
                        }
                        _ => {
                            // These will be handled on next iteration
                        }
                    }
                    break;
                }
                _ => {
                    let atom = self.parse_atom(lexeme);
                    atoms.push(atom);
                }
            }
        }

        atoms
    }

    /// Parse a single atom from a lexeme.
    fn parse_atom(&mut self, lexeme: Lexeme<'src>) -> Atom<'src> {
        match lexeme {
            Lexeme::Scalar { span, value, kind } => Atom {
                span,
                content: AtomContent::Scalar { value, kind },
            },

            Lexeme::Unit { span } => Atom {
                span,
                content: AtomContent::Unit,
            },

            Lexeme::Tag {
                span,
                name,
                has_payload,
            } => {
                // Validate tag name
                let invalid_name = !is_valid_tag_name(name);

                let payload = if has_payload {
                    // Parse payload
                    let next = self.lexer.next_lexeme();
                    Some(Box::new(self.parse_atom(next)))
                } else {
                    None
                };

                let end = payload.as_ref().map(|p| p.span.end).unwrap_or(span.end);

                Atom {
                    span: Span::new(span.start, end),
                    content: AtomContent::Tag {
                        name,
                        payload,
                        invalid_name,
                    },
                }
            }

            Lexeme::ObjectStart { span } => self.parse_object_atom(span),

            Lexeme::SeqStart { span } => self.parse_sequence_atom(span),

            Lexeme::AttrKey { span, key } => self.parse_attributes(span, key),

            Lexeme::Error { span, message } => Atom {
                span,
                content: AtomContent::Error { message },
            },

            // These shouldn't appear as atoms
            Lexeme::ObjectEnd { span }
            | Lexeme::SeqEnd { span }
            | Lexeme::Comma { span }
            | Lexeme::Newline { span } => Atom {
                span,
                content: AtomContent::Error {
                    message: "unexpected token",
                },
            },

            Lexeme::Comment { span, .. } | Lexeme::DocComment { span, .. } => Atom {
                span,
                content: AtomContent::Error {
                    message: "unexpected comment",
                },
            },

            Lexeme::Eof => Atom {
                span: Span::new(self.input.len() as u32, self.input.len() as u32),
                content: AtomContent::Error {
                    message: "unexpected EOF",
                },
            },
        }
    }

    /// Parse an object atom { ... }.
    fn parse_object_atom(&mut self, start_span: Span) -> Atom<'src> {
        let mut entries: Vec<ObjectEntry<'src>> = Vec::new();
        let mut seen_keys: HashMap<KeyValue, Span> = HashMap::new();
        let mut duplicate_key_spans: Vec<(Span, Span)> = Vec::new();
        let mut dangling_doc_comment_spans: Vec<Span> = Vec::new();
        let mut pending_doc_comment: Option<(Span, &'src str)> = None;
        let mut unclosed = false;
        let mut end_span = start_span;

        loop {
            let lexeme = self.lexer.next_lexeme();
            match lexeme {
                Lexeme::Eof => {
                    unclosed = true;
                    if let Some((span, _)) = pending_doc_comment {
                        dangling_doc_comment_spans.push(span);
                    }
                    break;
                }
                Lexeme::ObjectEnd { span } => {
                    if let Some((span, _)) = pending_doc_comment {
                        dangling_doc_comment_spans.push(span);
                    }
                    end_span = span;
                    break;
                }
                Lexeme::Newline { .. } | Lexeme::Comma { .. } => {
                    continue;
                }
                Lexeme::Comment { .. } => {
                    continue;
                }
                Lexeme::DocComment { span, text } => {
                    pending_doc_comment = Some((span, text));
                }
                _ => {
                    let doc_comment = pending_doc_comment.take();
                    let entry_atoms = self.collect_entry_atoms(lexeme);

                    if !entry_atoms.is_empty() {
                        let key = entry_atoms[0].clone();

                        // Check for duplicate key
                        let key_value = KeyValue::from_atom(&key, self.input);
                        if let Some(&original_span) = seen_keys.get(&key_value) {
                            duplicate_key_spans.push((original_span, key.span));
                        } else {
                            seen_keys.insert(key_value, key.span);
                        }

                        let (value, too_many_atoms_span) = if entry_atoms.len() == 1 {
                            (
                                Atom {
                                    span: key.span,
                                    content: AtomContent::Unit,
                                },
                                None,
                            )
                        } else if entry_atoms.len() == 2 {
                            (entry_atoms[1].clone(), None)
                        } else {
                            (entry_atoms[1].clone(), Some(entry_atoms[2].span))
                        };

                        entries.push(ObjectEntry {
                            key,
                            value,
                            doc_comment,
                            too_many_atoms_span,
                        });
                    }
                }
            }
        }

        Atom {
            span: Span::new(start_span.start, end_span.end),
            content: AtomContent::Object {
                entries,
                duplicate_key_spans,
                dangling_doc_comment_spans,
                unclosed,
            },
        }
    }

    /// Parse a sequence atom ( ... ).
    fn parse_sequence_atom(&mut self, start_span: Span) -> Atom<'src> {
        let mut elements: Vec<Atom<'src>> = Vec::new();
        let mut unclosed = false;
        let mut comma_spans: Vec<Span> = Vec::new();
        let mut end_span = start_span;

        loop {
            let lexeme = self.lexer.next_lexeme();
            match lexeme {
                Lexeme::Eof => {
                    unclosed = true;
                    break;
                }
                Lexeme::SeqEnd { span } => {
                    end_span = span;
                    break;
                }
                Lexeme::Newline { .. } => {
                    continue;
                }
                Lexeme::Comma { span } => {
                    comma_spans.push(span);
                    continue;
                }
                Lexeme::Comment { .. } | Lexeme::DocComment { .. } => {
                    continue;
                }
                _ => {
                    let elem = self.parse_atom(lexeme);
                    elements.push(elem);
                }
            }
        }

        Atom {
            span: Span::new(start_span.start, end_span.end),
            content: AtomContent::Sequence {
                elements,
                unclosed,
                comma_spans,
            },
        }
    }

    /// Parse attributes (key>value chains).
    fn parse_attributes(&mut self, first_span: Span, first_key: &'src str) -> Atom<'src> {
        let mut attrs = Vec::new();

        // Parse first value
        let first_value = self.parse_attribute_value();
        attrs.push(AttributeEntry {
            key: first_key,
            key_span: first_span,
            value: first_value,
        });

        // Continue parsing more attributes
        loop {
            let lexeme = self.lexer.next_lexeme();
            match lexeme {
                Lexeme::AttrKey { span, key } => {
                    let value = self.parse_attribute_value();
                    attrs.push(AttributeEntry {
                        key,
                        key_span: span,
                        value,
                    });
                }
                Lexeme::Eof
                | Lexeme::Newline { .. }
                | Lexeme::Comma { .. }
                | Lexeme::ObjectEnd { .. }
                | Lexeme::SeqEnd { .. } => {
                    break;
                }
                _ => {
                    // Not an attribute - this shouldn't happen in well-formed input
                    // but we need to handle it
                    break;
                }
            }
        }

        let end = attrs
            .last()
            .map(|a| a.value.span.end)
            .unwrap_or(first_span.end);

        Atom {
            span: Span::new(first_span.start, end),
            content: AtomContent::Attributes(attrs),
        }
    }

    /// Parse an attribute value.
    fn parse_attribute_value(&mut self) -> Atom<'src> {
        let lexeme = self.lexer.next_lexeme();
        self.parse_atom(lexeme)
    }

    /// Emit events for an entry.
    fn emit_entry(
        &mut self,
        atoms: &[Atom<'src>],
        state: &mut ObjectState,
        path_state: Option<&mut PathState>,
    ) {
        if atoms.is_empty() {
            return;
        }

        let key_atom = &atoms[0];

        // Check for invalid key types
        match &key_atom.content {
            AtomContent::Scalar {
                kind: ScalarKind::Heredoc,
                ..
            } => {
                self.event_queue.push_back(Event::Error {
                    span: key_atom.span,
                    kind: ParseErrorKind::InvalidKey,
                });
            }
            AtomContent::Object { .. } | AtomContent::Sequence { .. } => {
                self.event_queue.push_back(Event::Error {
                    span: key_atom.span,
                    kind: ParseErrorKind::InvalidKey,
                });
            }
            _ => {}
        }

        // Check for dotted path
        if let AtomContent::Scalar {
            value,
            kind: ScalarKind::Bare,
        } = &key_atom.content
        {
            if value.contains('.') {
                self.emit_dotted_path_entry(value, key_atom.span, atoms, state, path_state);
                return;
            }
        }

        // Simple key - check for duplicates
        let key_value = KeyValue::from_atom(key_atom, self.input);
        if let Some(&original_span) = state.seen_keys.get(&key_value) {
            self.event_queue.push_back(Event::Error {
                span: key_atom.span,
                kind: ParseErrorKind::DuplicateKey {
                    original: original_span,
                },
            });
        } else {
            state.seen_keys.insert(key_value.clone(), key_atom.span);
        }

        // Check path state if at root
        if let Some(ps) = path_state {
            let key_text = key_value.to_string();
            let path = vec![key_text];
            let value_kind = if atoms.len() >= 2 {
                match &atoms[1].content {
                    AtomContent::Object { .. } | AtomContent::Attributes(_) => {
                        PathValueKind::Object
                    }
                    _ => PathValueKind::Terminal,
                }
            } else {
                PathValueKind::Terminal
            };

            if let Err(err) = ps.check_and_update(&path, key_atom.span, value_kind) {
                self.emit_path_error(err, key_atom.span);
            }
        }

        // Emit entry events
        self.event_queue.push_back(Event::EntryStart);
        self.emit_atom_as_key(key_atom);

        if atoms.len() == 1 {
            self.event_queue.push_back(Event::Unit {
                span: key_atom.span,
            });
        } else if atoms.len() >= 2 {
            self.emit_atom_as_value(&atoms[1]);
        }

        if atoms.len() > 2 {
            self.event_queue.push_back(Event::Error {
                span: atoms[2].span,
                kind: ParseErrorKind::TooManyAtoms,
            });
        }

        self.event_queue.push_back(Event::EntryEnd);
    }

    /// Emit events for a dotted path entry.
    fn emit_dotted_path_entry(
        &mut self,
        path_text: &str,
        path_span: Span,
        atoms: &[Atom<'src>],
        state: &mut ObjectState,
        path_state: Option<&mut PathState>,
    ) {
        let segments: Vec<&str> = path_text.split('.').collect();

        // Validate path
        if segments.is_empty() || segments.iter().any(|s| s.is_empty()) {
            self.event_queue.push_back(Event::Error {
                span: path_span,
                kind: ParseErrorKind::InvalidKey,
            });
            self.event_queue.push_back(Event::EntryStart);
            self.event_queue.push_back(Event::EntryEnd);
            return;
        }

        // Check for duplicate at root level
        let first_segment = segments[0].to_string();
        let first_key_value = KeyValue::Scalar(first_segment.clone());
        if state.seen_keys.contains_key(&first_key_value) {
            // First segment already exists - that's okay for dotted paths,
            // the path state will catch actual duplicates
        } else {
            state.seen_keys.insert(first_key_value, path_span);
        }

        // Check path state
        if let Some(ps) = path_state {
            let path: Vec<String> = segments.iter().map(|s| s.to_string()).collect();
            let value_kind = if atoms.len() >= 2 {
                match &atoms[1].content {
                    AtomContent::Object { .. } | AtomContent::Attributes(_) => {
                        PathValueKind::Object
                    }
                    _ => PathValueKind::Terminal,
                }
            } else {
                PathValueKind::Terminal
            };

            if let Err(err) = ps.check_and_update(&path, path_span, value_kind) {
                self.emit_path_error(err, path_span);
            }
        }

        // Emit nested structure
        let depth = segments.len();
        let mut current_offset = path_span.start;

        for (i, segment) in segments.iter().enumerate() {
            let segment_len = segment.len() as u32;
            let segment_span = Span::new(current_offset, current_offset + segment_len);

            if i > 0 {
                self.event_queue.push_back(Event::EntryStart);
            } else {
                self.event_queue.push_back(Event::EntryStart);
            }

            self.event_queue.push_back(Event::Key {
                span: segment_span,
                tag: None,
                payload: Some(Cow::Borrowed(*segment)),
                kind: ScalarKind::Bare,
            });

            if i < depth - 1 {
                self.event_queue.push_back(Event::ObjectStart {
                    span: segment_span,
                    separator: Separator::Newline,
                });
            }

            current_offset += segment_len + 1;
        }

        // Emit value
        if atoms.len() == 1 {
            self.event_queue.push_back(Event::Unit { span: path_span });
        } else if atoms.len() >= 2 {
            self.emit_atom_as_value(&atoms[1]);
        }

        if atoms.len() > 2 {
            self.event_queue.push_back(Event::Error {
                span: atoms[2].span,
                kind: ParseErrorKind::TooManyAtoms,
            });
        }

        // Close nested structures
        for i in (0..depth).rev() {
            if i < depth - 1 {
                self.event_queue
                    .push_back(Event::ObjectEnd { span: path_span });
            }
            self.event_queue.push_back(Event::EntryEnd);
        }
    }

    /// Emit path error.
    fn emit_path_error(&mut self, err: PathError, span: Span) {
        let kind = match err {
            PathError::Duplicate { original } => ParseErrorKind::DuplicateKey { original },
            PathError::Reopened { closed_path } => ParseErrorKind::ReopenedPath { closed_path },
            PathError::NestIntoTerminal { terminal_path } => {
                ParseErrorKind::NestIntoTerminal { terminal_path }
            }
        };
        self.event_queue.push_back(Event::Error { span, kind });
    }

    /// Emit an atom as a key event.
    fn emit_atom_as_key(&mut self, atom: &Atom<'src>) {
        match &atom.content {
            AtomContent::Scalar { value, kind } => {
                // Validate escapes for quoted scalars
                if *kind == ScalarKind::Quoted {
                    self.emit_escape_errors(value, atom.span);
                }
                self.event_queue.push_back(Event::Key {
                    span: atom.span,
                    tag: None,
                    payload: Some(process_scalar(value, *kind)),
                    kind: *kind,
                });
            }
            AtomContent::Unit => {
                self.event_queue.push_back(Event::Key {
                    span: atom.span,
                    tag: None,
                    payload: None,
                    kind: ScalarKind::Bare,
                });
            }
            AtomContent::Tag {
                name,
                payload,
                invalid_name,
            } => {
                if *invalid_name {
                    self.event_queue.push_back(Event::Error {
                        span: atom.span,
                        kind: ParseErrorKind::InvalidTagName,
                    });
                }

                match payload {
                    None => {
                        self.event_queue.push_back(Event::Key {
                            span: atom.span,
                            tag: Some(name),
                            payload: None,
                            kind: ScalarKind::Bare,
                        });
                    }
                    Some(inner) => match &inner.content {
                        AtomContent::Scalar { value, kind } => {
                            if *kind == ScalarKind::Quoted {
                                self.emit_escape_errors(value, inner.span);
                            }
                            self.event_queue.push_back(Event::Key {
                                span: atom.span,
                                tag: Some(name),
                                payload: Some(process_scalar(value, *kind)),
                                kind: *kind,
                            });
                        }
                        AtomContent::Unit => {
                            self.event_queue.push_back(Event::Key {
                                span: atom.span,
                                tag: Some(name),
                                payload: None,
                                kind: ScalarKind::Bare,
                            });
                        }
                        _ => {
                            self.event_queue.push_back(Event::Error {
                                span: inner.span,
                                kind: ParseErrorKind::InvalidKey,
                            });
                        }
                    },
                }
            }
            AtomContent::Object { .. }
            | AtomContent::Sequence { .. }
            | AtomContent::Attributes(_)
            | AtomContent::Error { .. } => {
                self.event_queue.push_back(Event::Error {
                    span: atom.span,
                    kind: ParseErrorKind::InvalidKey,
                });
            }
        }
    }

    /// Emit an atom as a value.
    fn emit_atom_as_value(&mut self, atom: &Atom<'src>) {
        match &atom.content {
            AtomContent::Scalar { value, kind } => {
                if *kind == ScalarKind::Quoted {
                    self.emit_escape_errors(value, atom.span);
                }
                self.event_queue.push_back(Event::Scalar {
                    span: atom.span,
                    value: process_scalar(value, *kind),
                    kind: *kind,
                });
            }
            AtomContent::Unit => {
                self.event_queue.push_back(Event::Unit { span: atom.span });
            }
            AtomContent::Tag {
                name,
                payload,
                invalid_name,
            } => {
                if *invalid_name {
                    self.event_queue.push_back(Event::Error {
                        span: atom.span,
                        kind: ParseErrorKind::InvalidTagName,
                    });
                }
                self.event_queue.push_back(Event::TagStart {
                    span: atom.span,
                    name,
                });
                if let Some(inner) = payload {
                    self.emit_atom_as_value(inner);
                }
                self.event_queue.push_back(Event::TagEnd);
            }
            AtomContent::Object {
                entries,
                duplicate_key_spans,
                dangling_doc_comment_spans,
                unclosed,
            } => {
                self.event_queue.push_back(Event::ObjectStart {
                    span: atom.span,
                    separator: Separator::Newline,
                });

                if *unclosed {
                    self.event_queue.push_back(Event::Error {
                        span: atom.span,
                        kind: ParseErrorKind::UnclosedObject,
                    });
                }

                for (original, dup) in duplicate_key_spans {
                    self.event_queue.push_back(Event::Error {
                        span: *dup,
                        kind: ParseErrorKind::DuplicateKey {
                            original: *original,
                        },
                    });
                }

                for span in dangling_doc_comment_spans {
                    self.event_queue.push_back(Event::Error {
                        span: *span,
                        kind: ParseErrorKind::DanglingDocComment,
                    });
                }

                for entry in entries {
                    if let Some((span, text)) = &entry.doc_comment {
                        self.event_queue
                            .push_back(Event::DocComment { span: *span, text });
                    }
                    self.event_queue.push_back(Event::EntryStart);
                    self.emit_atom_as_key(&entry.key);
                    self.emit_atom_as_value(&entry.value);
                    if let Some(span) = entry.too_many_atoms_span {
                        self.event_queue.push_back(Event::Error {
                            span,
                            kind: ParseErrorKind::TooManyAtoms,
                        });
                    }
                    self.event_queue.push_back(Event::EntryEnd);
                }

                self.event_queue
                    .push_back(Event::ObjectEnd { span: atom.span });
            }
            AtomContent::Sequence {
                elements,
                unclosed,
                comma_spans,
            } => {
                self.event_queue
                    .push_back(Event::SequenceStart { span: atom.span });

                if *unclosed {
                    self.event_queue.push_back(Event::Error {
                        span: atom.span,
                        kind: ParseErrorKind::UnclosedSequence,
                    });
                }

                for span in comma_spans {
                    self.event_queue.push_back(Event::Error {
                        span: *span,
                        kind: ParseErrorKind::CommaInSequence,
                    });
                }

                for elem in elements {
                    self.emit_atom_as_value(elem);
                }

                self.event_queue
                    .push_back(Event::SequenceEnd { span: atom.span });
            }
            AtomContent::Attributes(attrs) => {
                self.event_queue.push_back(Event::ObjectStart {
                    span: atom.span,
                    separator: Separator::Comma,
                });

                for attr in attrs {
                    self.event_queue.push_back(Event::EntryStart);
                    self.event_queue.push_back(Event::Key {
                        span: attr.key_span,
                        tag: None,
                        payload: Some(Cow::Borrowed(attr.key)),
                        kind: ScalarKind::Bare,
                    });
                    self.emit_atom_as_value(&attr.value);
                    self.event_queue.push_back(Event::EntryEnd);
                }

                self.event_queue
                    .push_back(Event::ObjectEnd { span: atom.span });
            }
            AtomContent::Error { .. } => {
                self.event_queue.push_back(Event::Error {
                    span: atom.span,
                    kind: ParseErrorKind::UnexpectedToken,
                });
            }
        }
    }

    /// Emit errors for invalid escape sequences in a quoted string.
    fn emit_escape_errors(&mut self, text: &str, span: Span) {
        for (offset, seq) in validate_escapes(text) {
            let error_start = span.start + offset as u32;
            let error_span = Span::new(error_start, error_start + seq.len() as u32);
            self.event_queue.push_back(Event::Error {
                span: error_span,
                kind: ParseErrorKind::InvalidEscape(seq),
            });
        }
    }
}

// ============================================================================
// Atom types
// ============================================================================

#[derive(Debug, Clone)]
struct Atom<'src> {
    span: Span,
    content: AtomContent<'src>,
}

#[derive(Debug, Clone)]
enum AtomContent<'src> {
    Scalar {
        value: Cow<'src, str>,
        kind: ScalarKind,
    },
    Unit,
    Tag {
        name: &'src str,
        payload: Option<Box<Atom<'src>>>,
        invalid_name: bool,
    },
    Object {
        entries: Vec<ObjectEntry<'src>>,
        duplicate_key_spans: Vec<(Span, Span)>,
        dangling_doc_comment_spans: Vec<Span>,
        unclosed: bool,
    },
    Sequence {
        elements: Vec<Atom<'src>>,
        unclosed: bool,
        comma_spans: Vec<Span>,
    },
    Attributes(Vec<AttributeEntry<'src>>),
    Error {
        message: &'static str,
    },
}

#[derive(Debug, Clone)]
struct ObjectEntry<'src> {
    key: Atom<'src>,
    value: Atom<'src>,
    doc_comment: Option<(Span, &'src str)>,
    too_many_atoms_span: Option<Span>,
}

#[derive(Debug, Clone)]
struct AttributeEntry<'src> {
    key: &'src str,
    key_span: Span,
    value: Atom<'src>,
}

// ============================================================================
// Key comparison
// ============================================================================

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum KeyValue {
    Scalar(String),
    Unit,
    Tagged {
        name: String,
        payload: Option<Box<KeyValue>>,
    },
}

impl KeyValue {
    fn from_atom(atom: &Atom<'_>, _input: &str) -> Self {
        match &atom.content {
            AtomContent::Scalar { value, kind } => {
                KeyValue::Scalar(process_scalar(value, *kind).into_owned())
            }
            AtomContent::Unit => KeyValue::Unit,
            AtomContent::Tag { name, payload, .. } => KeyValue::Tagged {
                name: (*name).to_string(),
                payload: payload
                    .as_ref()
                    .map(|p| Box::new(KeyValue::from_atom(p, _input))),
            },
            AtomContent::Object { .. } => KeyValue::Scalar("{}".into()),
            AtomContent::Sequence { .. } => KeyValue::Scalar("()".into()),
            AtomContent::Attributes(_) => KeyValue::Scalar("{}".into()),
            AtomContent::Error { .. } => KeyValue::Scalar("<error>".into()),
        }
    }

    fn to_string(&self) -> String {
        match self {
            KeyValue::Scalar(s) => s.clone(),
            KeyValue::Unit => "@".to_string(),
            KeyValue::Tagged { name, .. } => format!("@{}", name),
        }
    }
}

// ============================================================================
// Path tracking
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathValueKind {
    Object,
    Terminal,
}

#[derive(Debug, Clone)]
enum PathError {
    Duplicate { original: Span },
    Reopened { closed_path: Vec<String> },
    NestIntoTerminal { terminal_path: Vec<String> },
}

#[derive(Default, Clone)]
struct PathState {
    current_path: Vec<String>,
    closed_paths: HashSet<Vec<String>>,
    assigned_paths: HashMap<Vec<String>, (Span, PathValueKind)>,
}

impl PathState {
    fn check_and_update(
        &mut self,
        path: &[String],
        span: Span,
        value_kind: PathValueKind,
    ) -> Result<(), PathError> {
        // Check for duplicate
        if let Some(&(original, _)) = self.assigned_paths.get(path) {
            return Err(PathError::Duplicate { original });
        }

        // Check prefixes
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

        // Close paths beyond common prefix
        let common_len = self
            .current_path
            .iter()
            .zip(path.iter())
            .take_while(|(a, b)| a == b)
            .count();

        for i in common_len..self.current_path.len() {
            let closed: Vec<String> = self.current_path[..=i].to_vec();
            self.closed_paths.insert(closed);
        }

        // Record intermediate segments as objects
        for i in 1..path.len() {
            let prefix = path[..i].to_vec();
            self.assigned_paths
                .entry(prefix)
                .or_insert((span, PathValueKind::Object));
        }

        // Update state
        self.assigned_paths
            .insert(path.to_vec(), (span, value_kind));
        self.current_path = path.to_vec();

        Ok(())
    }
}

// ============================================================================
// Helpers
// ============================================================================

/// Check if a tag name is valid.
fn is_valid_tag_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Process a scalar value (handle escapes for quoted strings).
fn process_scalar<'a>(value: &'a Cow<'a, str>, kind: ScalarKind) -> Cow<'a, str> {
    match kind {
        ScalarKind::Bare | ScalarKind::Raw | ScalarKind::Heredoc => value.clone(),
        ScalarKind::Quoted => unescape_quoted(value),
    }
}

/// Unescape a quoted string.
fn unescape_quoted<'a>(text: &'a Cow<'a, str>) -> Cow<'a, str> {
    if !text.contains('\\') {
        return text.clone();
    }

    let mut result = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => result.push('\n'),
                Some('r') => result.push('\r'),
                Some('t') => result.push('\t'),
                Some('\\') => result.push('\\'),
                Some('"') => result.push('"'),
                Some('u') => match chars.peek() {
                    Some('{') => {
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
                    }
                    Some(c) if c.is_ascii_hexdigit() => {
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
                        }
                    }
                    _ => {}
                },
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

/// Validate escape sequences in a quoted string.
fn validate_escapes(text: &str) -> Vec<(usize, String)> {
    let mut errors = Vec::new();
    let mut chars = text.char_indices().peekable();

    while let Some((i, c)) = chars.next() {
        if c == '\\' {
            let escape_start = i;
            match chars.next() {
                Some((_, 'n' | 'r' | 't' | '\\' | '"')) => {}
                Some((_, 'u')) => match chars.peek() {
                    Some((_, '{')) => {
                        chars.next();
                        let mut valid = true;
                        let mut found_close = false;
                        for (_, c) in chars.by_ref() {
                            if c == '}' {
                                found_close = true;
                                break;
                            }
                            if !c.is_ascii_hexdigit() {
                                valid = false;
                            }
                        }
                        if !found_close || !valid {
                            let end = chars.peek().map(|(i, _)| *i).unwrap_or(text.len());
                            let seq = &text[escape_start..end.min(escape_start + 12)];
                            errors.push((escape_start + 1, seq.to_string()));
                        }
                    }
                    Some((_, c)) if c.is_ascii_hexdigit() => {
                        let mut count = 1;
                        while count < 4 {
                            match chars.peek() {
                                Some((_, c)) if c.is_ascii_hexdigit() => {
                                    chars.next();
                                    count += 1;
                                }
                                _ => break,
                            }
                        }
                        if count != 4 {
                            let end = chars.peek().map(|(i, _)| *i).unwrap_or(text.len());
                            let seq = &text[escape_start..end];
                            errors.push((escape_start + 1, seq.to_string()));
                        }
                    }
                    _ => {
                        errors.push((escape_start + 1, "\\u".to_string()));
                    }
                },
                Some((_, c)) => {
                    errors.push((escape_start + 1, format!("\\{}", c)));
                }
                None => {
                    errors.push((escape_start + 1, "\\".to_string()));
                }
            }
        }
    }

    errors
}

#[cfg(test)]
mod tests;
