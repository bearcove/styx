use std::borrow::Cow;

use crate::event::{Event, ParseErrorKind, ScalarKind, Separator};
use crate::span::Span;
use crate::token::TokenKind;
use crate::tokenizer::Tokenizer;

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

    /// Expression mode - expect a single value (no implicit root).
    ExpectExprValue,

    /// Inside an object, expecting an entry (or closing brace/EOF).
    ExpectEntry,

    /// Emit EntryStart, then go to EmitKey.
    EmitEntryStart {
        key_span: Span,
        key_kind: ScalarKind,
    },

    /// Emit unit key (@ with no name), then read value.
    EmitUnitKeyValue { at_span: Span },

    /// Emit DuplicateKey error for unit key, then continue to EmitUnitKeyValueWithToken.
    EmitDuplicateUnitKeyError {
        at_span: Span,
        original_span: Span,
        next_token_kind: TokenKind,
        next_token_span: Span,
    },

    /// Emit DuplicateKey error for tagged key (@foo), then continue to AfterTaggedKey.
    EmitDuplicateTaggedKeyError {
        at_span: Span,
        tag_span: Span,
        original_span: Span,
    },

    /// After emitting tagged key (@foo), read the value.
    AfterTaggedKey { full_span: Span, tag_span: Span },

    /// Emit unit key with the already-read token info.
    EmitUnitKeyValueWithToken {
        at_span: Span,
        token_kind: TokenKind,
        token_span: Span,
    },

    /// Emit Key event, then read value token.
    EmitKey {
        key_span: Span,
        key_kind: ScalarKind,
    },

    /// Emit DuplicateKey error, then continue to EmitKeyAfterDuplicateCheck.
    EmitDuplicateKeyError {
        key_span: Span,
        key_kind: ScalarKind,
        original_span: Span,
    },

    /// Emit Key event after duplicate check is done (skips re-checking).
    EmitKeyAfterDuplicateCheck {
        key_span: Span,
        key_kind: ScalarKind,
    },

    /// Emit Scalar value, then EntryEnd (non-bare scalars).
    EmitScalarValue { span: Span, kind: ScalarKind },

    /// Emit bare scalar value, but may need to check for `>` (attribute).
    EmitBareScalarValue { span: Span },

    /// After emitting bare scalar, check for `>` (attribute chain).
    AfterBareScalarValue { value_span: Span },

    /// Emit Unit value (for key without value), then EntryEnd.
    EmitUnitValue { span: Span },

    /// Emit Unit, then Error (for `>` with spaces).
    EmitUnitThenError { unit_span: Span, error_span: Span },

    /// Emit Error, then EntryEnd.
    EmitErrorThenEntryEnd { error_span: Span },

    /// Emit TooManyAtoms error, then EntryEnd.
    EmitTooManyAtomsThenEntryEnd { error_span: Span },

    /// Emit Unit value, then EntryEnd, then ObjectEnd.
    EmitUnitThenEntryEndThenObjectEnd { unit_span: Span, rbrace_span: Span },

    /// Emit EntryEnd after Unit, then ObjectEnd.
    EmitEntryEndAfterUnitThenObjectEnd { rbrace_span: Span },

    /// Emit ObjectEnd after an error (like dangling doc comment).
    EmitObjectEndAfterError { rbrace_span: Span },

    /// Emit ObjectStart for explicit root after doc comment.
    EmitExplicitRootAfterDocComment { lbrace_span: Span },

    /// Handle chained doc comments - we have another doc comment to process.
    ProcessNextDocComment { doc_span: Span },

    /// Emit EntryEnd and check/update separator style.
    EmitEntryEndWithSeparator {
        separator: SeparatorStyle,
        sep_span: Span,
    },

    /// Emit scalar value, then TooManyAtoms error for extra atom.
    EmitScalarThenTooManyAtoms { value_span: Span, error_span: Span },

    /// After emitting quoted scalar, check for TooManyAtoms on same line.
    AfterQuotedScalarValue { value_span: Span },

    /// Emit invalid escape errors for a quoted string, then emit the scalar.
    EmitInvalidEscapes {
        span: Span,
        kind: ScalarKind,
        /// Offsets within the string (relative to span.start + 1 for the opening quote)
        errors: Vec<(usize, usize)>,
        error_index: usize,
    },

    /// Emit invalid escape errors for a quoted KEY, then continue to EmitKey.
    EmitInvalidEscapesInKey {
        key_span: Span,
        key_kind: ScalarKind,
        errors: Vec<(usize, usize)>,
        error_index: usize,
    },

    /// Emit EntryEnd, then go back to ExpectEntry.
    EmitEntryEnd,

    /// Emit EntryEnd, then ObjectEnd (value followed by `}`).
    EmitEntryEndThenObjectEnd { rbrace_span: Span },

    /// Emit ObjectEnd after EntryEnd.
    EmitObjectEndAfterEntry { rbrace_span: Span },

    /// Emit EntryEnd, then SequenceEnd (value followed by `)`).
    EmitEntryEndThenSeqEnd { rparen_span: Span },

    /// Emit SequenceEnd after EntryEnd.
    EmitSeqEndAfterEntry { rparen_span: Span },

    /// Emit ObjectStart as a value (nested object).
    EmitObjectStartValue { span: Span },

    /// Emit SequenceStart as a value.
    EmitSequenceStartValue { span: Span },

    /// Inside a sequence, expecting an element.
    ExpectSeqElem,

    /// Process a token we already peeked as a sequence element.
    /// Used when we've consumed a token while checking for TooManyAtoms but
    /// discovered we're in a sequence where multiple elements are valid.
    ProcessPeekedSeqElem {
        peeked_kind: TokenKind,
        peeked_span: Span,
    },

    /// Emit TagStart, then check for payload.
    EmitTagStart { tag_span: Span },

    /// After emitting TagStart, check for payload.
    AfterTagStart { tag_span: Span },

    /// Emit TagEnd after a tag with no payload or after payload.
    EmitTagEnd,

    /// Emit Unit for explicit @tag@, then TagEnd.
    EmitTagEndWithUnit { unit_span: Span },

    /// Emit TagEnd, then emit SequenceEnd (for `)` after tag in seq).
    EmitTagEndThenSeqEnd { rparen_span: Span },

    /// Emit TagEnd, then emit ObjectEnd (for `}` after tag in obj).
    EmitTagEndThenObjEnd { rbrace_span: Span },

    /// We saw `key>` - emit ObjectStart for attribute object.
    EmitAttrObjectStart { main_key_span: Span },

    /// Emit EntryStart for attribute.
    EmitAttrEntryStart { attr_key_span: Span },

    /// Emit Key for attribute.
    EmitAttrKey { attr_key_span: Span },

    /// After emitting attr key, read the value.
    ExpectAttrValue { attr_key_span: Span },

    /// We have a bare scalar that might be attr value or next attr key.
    /// Don't emit yet - check for `>`.
    AfterAttrBareValue { value_span: Span },

    /// Emit scalar value for attribute, then check for more.
    EmitAttrScalarValue { span: Span, kind: ScalarKind },

    /// After attr value emitted, emit EntryEnd then check for more attrs.
    EmitAttrEntryEnd,

    /// After attr EntryEnd, check for more `>` or close.
    AfterAttrEntryEnd,

    /// Emitting dotted path segments (e.g., "a.b.c" -> nested objects).
    /// `offset` is byte position within the token, `depth` is how many objects we've pushed.
    EmitDottedPath {
        full_span: Span,
        offset: u32,
        depth: u32,
    },

    /// Emit Key for dotted path segment, then read value (last segment).
    EmitDottedPathKey {
        key_span: Span,
        full_span: Span,
        depth: u32,
    },

    /// Emit Key for dotted path segment, then ObjectStart (not last segment).
    EmitDottedPathKeyThenObject {
        key_span: Span,
        full_span: Span,
        next_offset: u32,
        depth: u32,
    },

    /// Emit ObjectStart for dotted path nesting.
    EmitDottedPathObject {
        key_span: Span,
        full_span: Span,
        next_offset: u32,
        depth: u32,
    },

    /// After all dotted path segments emitted, read value for innermost key.
    AfterDottedPathKey { full_span: Span, depth: u32 },

    /// After reading bare value in dotted path, check for `>`.
    AfterDottedPathBareValue { value_span: Span, depth: u32 },

    /// Emit scalar value for dotted path.
    EmitDottedPathScalarValue {
        span: Span,
        kind: ScalarKind,
        depth: u32,
    },

    /// Close the dotted path objects after value.
    CloseDottedPath { depth: u32 },

    /// Emit EntryEnd then continue closing dotted path.
    EmitEntryEndThenCloseDotted { depth: u32 },

    /// Inside sequence that is an attribute value. After close, check for more attrs.
    ExpectSeqElemInAttr,

    /// Emit SequenceEnd for attr value, then check for more attrs.
    EmitSeqEndInAttr { rparen_span: Span },

    /// Inside object that is an attribute value. After close, check for more attrs.
    ExpectEntryInAttr,

    /// Emit ObjectEnd for attr value, then check for more attrs.
    EmitObjEndInAttr { rbrace_span: Span },

    /// Emit TagStart for tag inside attr value.
    EmitTagStartInAttr { tag_span: Span },

    /// After TagStart in attr, check for payload.
    AfterTagStartInAttr { tag_span: Span },

    /// Emit TagEnd for tag in attr, then check for more attrs.
    EmitTagEndInAttr,

    /// Emit Unit for @tag@, then TagEnd in attr.
    EmitUnitThenTagEndInAttr { unit_span: Span },

    /// Close the attribute object.
    EmitAttrObjectEnd,

    /// Close attr object, then emit EntryEnd, then start new entry.
    EmitAttrObjectEndThenNewEntry {
        next_key_span: Span,
        next_key_kind: ScalarKind,
    },

    /// After attr object closed, emit EntryEnd then new EntryStart.
    EmitEntryEndThenNewEntry {
        next_key_span: Span,
        next_key_kind: ScalarKind,
    },

    /// Close attr object, then close parent object.
    EmitAttrObjectEndThenClose { rbrace_span: Span },

    /// After attr object closed, close parent object.
    EmitParentObjectEnd { rbrace_span: Span },

    /// Emit DocumentEnd, then Done.
    EmitDocumentEnd,

    /// Done - return None forever.
    Done,
}

/// Separator style used in an object.
#[derive(Debug, Clone, Copy, PartialEq)]
enum SeparatorStyle {
    /// Not yet determined.
    Unknown,
    /// Using commas.
    Comma,
    /// Using newlines.
    Newline,
}

/// Context for nested structures.
#[derive(Debug, Clone, PartialEq)]
enum Context {
    /// Inside an object. `implicit` = true for the root object.
    Object {
        implicit: bool,
        separator: SeparatorStyle,
        /// Keys seen in this object: normalized_key -> original span (for duplicate detection).
        seen_keys: std::collections::HashMap<String, Span>,
        /// If true, this object is a tag payload and needs TagEnd emitted after ObjectEnd.
        is_tag_payload: bool,
        /// If true, at least one entry has been started in this object.
        has_entries: bool,
    },
    /// Inside a sequence. `is_tag_payload` indicates this is a tag payload.
    Sequence { is_tag_payload: bool },
    /// Inside an attribute object (the `{...}` that holds attr key-value pairs).
    AttrObject,
}

#[derive(Clone)]
pub struct Parser3<'src> {
    input: &'src str,
    lexer: Tokenizer<'src>,
    state: State,
    /// Stack of nested contexts (objects/sequences).
    context_stack: Vec<Context>,
    /// Current path of keys (for tracking terminals and reopens).
    current_path: Vec<String>,
    /// Paths that have received a terminal value (scalar/unit).
    terminal_paths: std::collections::HashSet<Vec<String>>,
    /// Paths that have been explicitly closed (with `}`).
    closed_paths: std::collections::HashSet<Vec<String>>,
    /// Depth of dotted path nesting (to track when to close paths).
    dotted_depth: u32,
    /// For each implicit object path from dotted paths, track the last child key.
    /// When a new child is added at the same level, the previous child's subtree is closed.
    /// Maps parent_path -> last_child_key
    implicit_children: std::collections::HashMap<Vec<String>, String>,
    // WE DO NOT PEEK
    // WE DO NOT UNPEEK
    // WE DO NOT BUFFER EVENTS
    // WE DO NOT COLLECT ALL TOKENS
    // WE DO NOT COLLECT ALL EVENTS
    // WE ARE A PULL PARSER, FULLY STREAMING, WITH A STATE MACHINE
    // AND THAT IS ALL.
}

impl<'src> Parser3<'src> {
    /// Create a new parser in document mode (implicit root object).
    pub fn new(source: &'src str) -> Self {
        Self {
            input: source,
            lexer: Tokenizer::new(source),
            state: State::Start,
            context_stack: Vec::new(),
            current_path: Vec::new(),
            terminal_paths: std::collections::HashSet::new(),
            closed_paths: std::collections::HashSet::new(),
            dotted_depth: 0,
            implicit_children: std::collections::HashMap::new(),
        }
    }

    /// Create a new parser in expression mode (single value, no implicit root).
    ///
    /// Expression mode parses a single value rather than an implicit root object.
    /// Use this for parsing embedded values like default values in schemas.
    pub fn new_expr(source: &'src str) -> Self {
        Self {
            input: source,
            lexer: Tokenizer::new(source),
            state: State::ExpectExprValue,
            context_stack: Vec::new(),
            current_path: Vec::new(),
            terminal_paths: std::collections::HashSet::new(),
            closed_paths: std::collections::HashSet::new(),
            dotted_depth: 0,
            implicit_children: std::collections::HashMap::new(),
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

    /// Skip whitespace and newlines (but not comments).
    fn next_token_skip_ws_nl(&mut self) -> crate::token::Token<'src> {
        loop {
            let t = self.lexer.next_token();
            match t.kind {
                TokenKind::Whitespace | TokenKind::Newline => continue,
                _ => return t,
            }
        }
    }

    /// Check if a tag name is valid.
    /// Tag names cannot start with digit, hyphen, or dot, and cannot contain dots.
    fn is_valid_tag_name(&self, name: &str) -> bool {
        if name.is_empty() {
            return false;
        }
        let first = name.chars().next().unwrap();
        if first.is_ascii_digit() || first == '-' || first == '.' {
            return false;
        }
        if name.contains('.') {
            return false;
        }
        true
    }

    /// Find invalid escape sequences in a quoted string.
    /// Returns a list of (byte_offset_within_inner, length) for each invalid escape.
    fn find_invalid_escapes(&self, text: &str) -> Vec<(usize, usize)> {
        let inner = &text[1..text.len() - 1];
        let mut invalid = Vec::new();
        let mut i = 0;
        let bytes = inner.as_bytes();

        while i < bytes.len() {
            if bytes[i] == b'\\' {
                let escape_start = i;
                i += 1;
                if i >= bytes.len() {
                    invalid.push((escape_start, 1));
                    break;
                }
                match bytes[i] {
                    b'n' | b'r' | b't' | b'\\' | b'"' => {
                        i += 1;
                    }
                    b'u' => {
                        // Unicode escape - \uXXXX or \u{X...}
                        i += 1;
                        if i < bytes.len() && bytes[i] == b'{' {
                            // \u{...} - skip to }
                            while i < bytes.len() && bytes[i] != b'}' {
                                i += 1;
                            }
                            if i < bytes.len() {
                                i += 1; // skip '}'
                            }
                        } else {
                            // \uXXXX - skip 4 hex digits
                            let mut count = 0;
                            while i < bytes.len() && count < 4 {
                                if bytes[i].is_ascii_hexdigit() {
                                    i += 1;
                                    count += 1;
                                } else {
                                    break;
                                }
                            }
                        }
                    }
                    _ => {
                        // Invalid escape
                        invalid.push((escape_start, 2));
                        i += 1;
                    }
                }
            } else {
                i += 1;
            }
        }

        invalid
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

    /// Check if a key is a duplicate in the current object context.
    /// Returns Some(original_span) if this is a duplicate, None if it's new.
    /// Also records the key in the seen_keys map.
    fn check_and_record_key(&mut self, normalized_key: &str, key_span: Span) -> Option<Span> {
        if let Some(Context::Object { seen_keys, .. }) = self.context_stack.last_mut() {
            if let Some(&original_span) = seen_keys.get(normalized_key) {
                return Some(original_span); // duplicate
            }
            seen_keys.insert(normalized_key.to_string(), key_span);
        }
        None
    }

    /// Check if a dotted path can be used. Returns Some error if:
    /// - Any prefix is a terminal (NestIntoTerminal)
    /// - Any prefix was previously closed (ReopenedPath)
    fn check_dotted_path(&self, segments: &[&str]) -> Option<ParseErrorKind> {
        // Build up path segment by segment, checking for terminals and closed paths
        let mut check_path = self.current_path.clone();

        for (i, &seg) in segments.iter().enumerate() {
            check_path.push(seg.to_string());

            // Check if this prefix is a terminal (can't nest into it)
            if i < segments.len() - 1 {
                // Not the last segment - check if it's a terminal
                if self.terminal_paths.contains(&check_path) {
                    return Some(ParseErrorKind::NestIntoTerminal {
                        terminal_path: check_path,
                    });
                }
                // Also check if this prefix was closed (can't extend a closed path)
                if self.closed_paths.contains(&check_path) {
                    return Some(ParseErrorKind::ReopenedPath {
                        closed_path: check_path,
                    });
                }
            }
        }

        // Check if the full path was previously closed
        if self.closed_paths.contains(&check_path) {
            return Some(ParseErrorKind::ReopenedPath {
                closed_path: check_path,
            });
        }

        None
    }

    /// Record that we're processing a dotted path, and close any previous sibling
    /// branches that had explicit objects.
    fn record_dotted_path_and_close_siblings(&mut self, segments: &[&str]) {
        let mut parent_path = self.current_path.clone();

        for (i, &seg) in segments.iter().enumerate() {
            // Check if parent had a different child and close that branch if needed
            // This applies to ALL segments, not just intermediate ones
            if let Some(last_child) = self.implicit_children.get(&parent_path) {
                if last_child != seg {
                    // Parent had a different child - mark that branch as closed
                    // if it had any descendants with explicit {}
                    let mut old_child_path = parent_path.clone();
                    old_child_path.push(last_child.clone());

                    // Check if old_child_path or any of its descendants are in closed_paths
                    let has_closed_descendants = self.closed_paths.iter().any(|p| {
                        p.len() >= old_child_path.len()
                            && p[..old_child_path.len()] == old_child_path[..]
                    });

                    if has_closed_descendants {
                        // Mark the old child path as closed
                        self.closed_paths.insert(old_child_path);
                    }
                }
            }

            // Record this child for this parent (for all segments except the last,
            // which will get its own entry when it has a value)
            if i < segments.len() - 1 {
                self.implicit_children
                    .insert(parent_path.clone(), seg.to_string());
            }

            parent_path.push(seg.to_string());
        }
    }

    /// Mark the current path as terminal (received a scalar/unit value).
    fn mark_path_terminal(&mut self) {
        if !self.current_path.is_empty() {
            self.terminal_paths.insert(self.current_path.clone());
        }
    }

    /// Mark the current path as closed (object closed with `}`).
    fn mark_path_closed(&mut self) {
        if !self.current_path.is_empty() {
            self.closed_paths.insert(self.current_path.clone());
        }
    }

    /// Push a key segment onto the current path.
    fn push_path_segment(&mut self, key: &str) {
        self.current_path.push(key.to_string());
    }

    /// Pop a key segment from the current path.
    fn pop_path_segment(&mut self) {
        self.current_path.pop();
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
                    // Push implicit root context but don't emit ObjectStart for it.
                    // The TreeBuilder creates its own implicit root from the entries.
                    self.context_stack.push(Context::Object {
                        implicit: true,
                        separator: SeparatorStyle::Unknown,
                        seen_keys: std::collections::HashMap::new(),
                        is_tag_payload: false,
                        has_entries: false,
                    });
                    self.state = State::ExpectEntry;
                    return Some(Event::DocumentStart);
                }

                State::EmitRootObjectStart => {
                    // This state is no longer used for implicit root.
                    // It may still be used if we need to emit an explicit root object.
                    self.context_stack.push(Context::Object {
                        implicit: true,
                        separator: SeparatorStyle::Unknown,
                        seen_keys: std::collections::HashMap::new(),
                        is_tag_payload: false,
                        has_entries: false,
                    });
                    self.state = State::ExpectEntry;
                    return Some(Event::ObjectStart {
                        span: Span::new(0, 0),
                        separator: Separator::Newline,
                    });
                }

                State::ExpectExprValue => {
                    // Expression mode: parse a single value without implicit root object.
                    // Skip leading whitespace/newlines.
                    let t = self.next_token_skip_ws_nl();

                    match t.kind {
                        TokenKind::BareScalar => {
                            self.state = State::Done;
                            return Some(Event::Scalar {
                                span: t.span,
                                value: Cow::Borrowed(self.text_at(t.span)),
                                kind: ScalarKind::Bare,
                            });
                        }
                        TokenKind::QuotedScalar => {
                            let text = self.text_at(t.span);
                            self.state = State::Done;
                            return Some(Event::Scalar {
                                span: t.span,
                                value: self.unescape_quoted(text),
                                kind: ScalarKind::Quoted,
                            });
                        }
                        TokenKind::LBrace => {
                            // Explicit object as value
                            self.context_stack.push(Context::Object {
                                implicit: false,
                                separator: SeparatorStyle::Unknown,
                                seen_keys: std::collections::HashMap::new(),
                                is_tag_payload: false,
                                has_entries: false,
                            });
                            self.state = State::ExpectEntry;
                            return Some(Event::ObjectStart {
                                span: t.span,
                                separator: Separator::Comma,
                            });
                        }
                        TokenKind::LParen => {
                            // Sequence as value
                            self.context_stack.push(Context::Sequence {
                                is_tag_payload: false,
                            });
                            self.state = State::ExpectSeqElem;
                            return Some(Event::SequenceStart { span: t.span });
                        }
                        TokenKind::At => {
                            // Tag as value
                            self.state = State::EmitTagStart { tag_span: t.span };
                            continue;
                        }
                        TokenKind::Eof => {
                            // Empty expression
                            self.state = State::Done;
                            return Some(Event::Unit {
                                span: self.eof_span(),
                            });
                        }
                        _ => {
                            self.state = State::Done;
                            return Some(Event::Error {
                                span: t.span,
                                kind: ParseErrorKind::UnexpectedToken,
                            });
                        }
                    }
                }

                State::ExpectEntry => {
                    let t = self.next_token_skip_ws_nl();

                    match t.kind {
                        TokenKind::LineComment => {
                            // Emit comment, stay in ExpectEntry
                            self.state = State::ExpectEntry;
                            return Some(Event::Comment {
                                span: t.span,
                                text: t.text,
                            });
                        }

                        TokenKind::Eof => {
                            // End of input - check if we have unclosed structures
                            match self.context_stack.last() {
                                Some(Context::Object { implicit: true, .. }) => {
                                    // Implicit root - OK, just close it
                                    self.context_stack.pop();
                                    self.state = State::EmitDocumentEnd;
                                    continue;
                                }
                                Some(Context::Object {
                                    implicit: false, ..
                                }) => {
                                    // Explicit object not closed - error
                                    self.state = State::EmitDocumentEnd;
                                    return Some(Event::Error {
                                        span: self.eof_span(),
                                        kind: ParseErrorKind::UnclosedObject,
                                    });
                                }
                                Some(Context::Sequence { .. }) => {
                                    // Sequence not closed - error
                                    self.state = State::EmitDocumentEnd;
                                    return Some(Event::Error {
                                        span: self.eof_span(),
                                        kind: ParseErrorKind::UnclosedSequence,
                                    });
                                }
                                Some(Context::AttrObject) => {
                                    // Attr object not closed - error
                                    self.state = State::EmitDocumentEnd;
                                    return Some(Event::Error {
                                        span: self.eof_span(),
                                        kind: ParseErrorKind::UnclosedObject,
                                    });
                                }
                                None => {
                                    self.state = State::EmitDocumentEnd;
                                    continue;
                                }
                            }
                        }

                        TokenKind::RBrace => {
                            // Close explicit object
                            match self.context_stack.pop() {
                                Some(Context::Object {
                                    implicit: false,
                                    is_tag_payload,
                                    ..
                                }) => {
                                    // If we're closing the value object of a dotted path,
                                    // mark the current path as closed
                                    if self.dotted_depth > 0 && !self.current_path.is_empty() {
                                        self.mark_path_closed();
                                    }
                                    // Determine next state based on whether we're at root or nested
                                    if self.context_stack.is_empty() {
                                        // This was the root object - go to document end
                                        self.state = State::EmitDocumentEnd;
                                    } else if is_tag_payload {
                                        // Tag payload - emit TagEnd after ObjectEnd
                                        self.state = State::EmitTagEnd;
                                    } else {
                                        // Nested object - emit EntryEnd
                                        self.state = State::EmitEntryEnd;
                                    }
                                    return Some(Event::ObjectEnd { span: t.span });
                                }
                                Some(Context::Object {
                                    implicit: true,
                                    separator,
                                    seen_keys,
                                    is_tag_payload,
                                    has_entries,
                                }) => {
                                    // Can't close implicit root with }
                                    self.context_stack.push(Context::Object {
                                        implicit: true,
                                        separator,
                                        seen_keys,
                                        is_tag_payload,
                                        has_entries,
                                    });
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
                            // Check for dotted path (e.g., "a.b.c")
                            let text = self.text_at(t.span);
                            if text.contains('.') {
                                // Check for path errors before processing
                                let segments: Vec<&str> = text.split('.').collect();
                                if let Some(error_kind) = self.check_dotted_path(&segments) {
                                    // Skip to end of entry and emit error
                                    loop {
                                        let skip = self.lexer.next_token();
                                        match skip.kind {
                                            TokenKind::Newline
                                            | TokenKind::Eof
                                            | TokenKind::Comma
                                            | TokenKind::RBrace => break,
                                            _ => continue,
                                        }
                                    }
                                    self.state = State::ExpectEntry;
                                    return Some(Event::Error {
                                        span: t.span,
                                        kind: error_kind,
                                    });
                                }
                                // Record this path and close any sibling branches that had explicit {}
                                self.record_dotted_path_and_close_siblings(&segments);
                                self.state = State::EmitDottedPath {
                                    full_span: t.span,
                                    offset: 0, // offset within the token
                                    depth: 0,
                                };
                                continue;
                            }
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

                        TokenKind::LBrace => {
                            // Explicit object at entry level
                            // If we're at implicit root with no entries yet, this becomes an explicit root
                            // Otherwise it's an error (can't have nested object without a key)
                            match self.context_stack.last() {
                                Some(Context::Object {
                                    implicit: true,
                                    has_entries: false,
                                    ..
                                }) => {
                                    // Replace implicit root with explicit root
                                    self.context_stack.pop();
                                    self.context_stack.push(Context::Object {
                                        implicit: false,
                                        separator: SeparatorStyle::Unknown,
                                        seen_keys: std::collections::HashMap::new(),
                                        is_tag_payload: false,
                                        has_entries: false,
                                    });
                                    self.state = State::ExpectEntry;
                                    return Some(Event::ObjectStart {
                                        span: t.span,
                                        separator: Separator::Comma,
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

                        TokenKind::At => {
                            // @ as a unit key - emit Key with no payload
                            if let Some(Context::Object { has_entries, .. }) =
                                self.context_stack.last_mut()
                            {
                                *has_entries = true;
                            }
                            self.state = State::EmitUnitKeyValue { at_span: t.span };
                            return Some(Event::EntryStart);
                        }

                        TokenKind::DocComment => {
                            // Check if doc comment is followed by a valid entry
                            // (not EOF, not close brace)
                            let doc_span = t.span;
                            let doc_text = t.text;
                            let next = self.next_token_skip_ws_nl();
                            match next.kind {
                                TokenKind::Eof => {
                                    // Dangling doc comment at EOF
                                    self.state = State::EmitDocumentEnd;
                                    return Some(Event::Error {
                                        span: doc_span,
                                        kind: ParseErrorKind::DanglingDocComment,
                                    });
                                }
                                TokenKind::RBrace => {
                                    // Dangling doc comment before close brace
                                    // Pop the context and emit error, then handle the brace
                                    match self.context_stack.pop() {
                                        Some(Context::Object {
                                            implicit: false, ..
                                        }) => {
                                            self.state = State::EmitEntryEnd;
                                            // Need to emit error, then ObjectEnd
                                            // But we need to emit error first, then handle }
                                            // This is getting complex - let's emit error and
                                            // "put back" the } by storing it
                                            self.state = State::EmitObjectEndAfterError {
                                                rbrace_span: next.span,
                                            };
                                            return Some(Event::Error {
                                                span: doc_span,
                                                kind: ParseErrorKind::DanglingDocComment,
                                            });
                                        }
                                        Some(ctx) => {
                                            // Put context back and emit error
                                            self.context_stack.push(ctx);
                                            self.state = State::ExpectEntry;
                                            return Some(Event::Error {
                                                span: doc_span,
                                                kind: ParseErrorKind::DanglingDocComment,
                                            });
                                        }
                                        None => {
                                            self.state = State::EmitDocumentEnd;
                                            return Some(Event::Error {
                                                span: doc_span,
                                                kind: ParseErrorKind::DanglingDocComment,
                                            });
                                        }
                                    }
                                }
                                TokenKind::BareScalar => {
                                    // Check for dotted path
                                    let text = self.text_at(next.span);
                                    if text.contains('.') {
                                        // Check for path errors before processing
                                        let segments: Vec<&str> = text.split('.').collect();
                                        if let Some(error_kind) = self.check_dotted_path(&segments)
                                        {
                                            // Skip to end of entry and emit error
                                            loop {
                                                let skip = self.lexer.next_token();
                                                match skip.kind {
                                                    TokenKind::Newline
                                                    | TokenKind::Eof
                                                    | TokenKind::Comma
                                                    | TokenKind::RBrace => break,
                                                    _ => continue,
                                                }
                                            }
                                            self.state = State::ExpectEntry;
                                            return Some(Event::Error {
                                                span: next.span,
                                                kind: error_kind,
                                            });
                                        }
                                        // Record this path and close any sibling branches that had explicit {}
                                        self.record_dotted_path_and_close_siblings(&segments);
                                        self.state = State::EmitDottedPath {
                                            full_span: next.span,
                                            offset: 0,
                                            depth: 0,
                                        };
                                    } else {
                                        self.state = State::EmitEntryStart {
                                            key_span: next.span,
                                            key_kind: ScalarKind::Bare,
                                        };
                                    }
                                    return Some(Event::DocComment {
                                        span: doc_span,
                                        text: doc_text,
                                    });
                                }
                                TokenKind::QuotedScalar => {
                                    self.state = State::EmitEntryStart {
                                        key_span: next.span,
                                        key_kind: ScalarKind::Quoted,
                                    };
                                    return Some(Event::DocComment {
                                        span: doc_span,
                                        text: doc_text,
                                    });
                                }
                                TokenKind::At => {
                                    self.state = State::EmitUnitKeyValue { at_span: next.span };
                                    return Some(Event::DocComment {
                                        span: doc_span,
                                        text: doc_text,
                                    });
                                }
                                TokenKind::LBrace => {
                                    // Doc comment before explicit root object
                                    // Replace implicit root with explicit
                                    match self.context_stack.last() {
                                        Some(Context::Object {
                                            implicit: true,
                                            has_entries: false,
                                            ..
                                        }) => {
                                            self.context_stack.pop();
                                            self.context_stack.push(Context::Object {
                                                implicit: false,
                                                separator: SeparatorStyle::Unknown,
                                                seen_keys: std::collections::HashMap::new(),
                                                is_tag_payload: false,
                                                has_entries: false,
                                            });
                                            self.state = State::ExpectEntry;
                                            // Emit doc comment, then will emit ObjectStart
                                            // But we already consumed the {, so we need a state for it
                                            self.state = State::EmitExplicitRootAfterDocComment {
                                                lbrace_span: next.span,
                                            };
                                            return Some(Event::DocComment {
                                                span: doc_span,
                                                text: doc_text,
                                            });
                                        }
                                        _ => {
                                            self.state = State::ExpectEntry;
                                            return Some(Event::Error {
                                                span: doc_span,
                                                kind: ParseErrorKind::DanglingDocComment,
                                            });
                                        }
                                    }
                                }
                                TokenKind::DocComment => {
                                    // Multiple doc comments in a row - emit this one and process next
                                    self.state = State::ProcessNextDocComment {
                                        doc_span: next.span,
                                    };
                                    return Some(Event::DocComment {
                                        span: doc_span,
                                        text: doc_text,
                                    });
                                }
                                _ => {
                                    // Doc comment followed by something unexpected
                                    self.state = State::ExpectEntry;
                                    return Some(Event::Error {
                                        span: doc_span,
                                        kind: ParseErrorKind::DanglingDocComment,
                                    });
                                }
                            }
                        }

                        TokenKind::HeredocStart => {
                            // Heredocs are not allowed as keys
                            // Skip to end of heredoc
                            loop {
                                let skip = self.lexer.next_token();
                                match skip.kind {
                                    TokenKind::HeredocEnd | TokenKind::Eof => break,
                                    _ => continue,
                                }
                            }
                            self.state = State::ExpectEntry;
                            return Some(Event::Error {
                                span: t.span,
                                kind: ParseErrorKind::InvalidKey,
                            });
                        }

                        TokenKind::Comma => {
                            // Comma as separator between entries - just skip it
                            // (The entry may have ended due to something other than a comma,
                            // e.g., a closing brace for a nested value)
                            self.state = State::ExpectEntry;
                            continue;
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
                    // For quoted keys, check for invalid escapes first
                    if key_kind == ScalarKind::Quoted {
                        let text = self.text_at(key_span);
                        let invalid_escapes = self.find_invalid_escapes(text);
                        if !invalid_escapes.is_empty() {
                            self.state = State::EmitInvalidEscapesInKey {
                                key_span,
                                key_kind,
                                errors: invalid_escapes,
                                error_index: 0,
                            };
                            continue;
                        }
                    }
                    // Mark that we've started an entry in the current object
                    if let Some(Context::Object { has_entries, .. }) = self.context_stack.last_mut()
                    {
                        *has_entries = true;
                    }
                    self.state = State::EmitKey { key_span, key_kind };
                    return Some(Event::EntryStart);
                }

                State::EmitUnitKeyValue { at_span } => {
                    // First read next token - check if it's immediately after @ (tag)
                    let t = self.lexer.next_token();

                    // If bare scalar immediately after @, it's a tag name like @foo
                    if t.kind == TokenKind::BareScalar && t.span.start == at_span.end {
                        // This is actually a tag like @SomeTag or @Some.Type
                        let tag_name = self.text_at(t.span);
                        // Validate tag name
                        if !self.is_valid_tag_name(tag_name) {
                            // Invalid tag name
                            loop {
                                let skip = self.lexer.next_token();
                                match skip.kind {
                                    TokenKind::Newline | TokenKind::Eof => break,
                                    _ => continue,
                                }
                            }
                            self.state = State::ExpectEntry;
                            return Some(Event::Error {
                                span: t.span,
                                kind: ParseErrorKind::InvalidTagName,
                            });
                        }

                        // Check for duplicate tagged key - use "@tagname" as the key
                        let full_span = Span::new(at_span.start, t.span.end);
                        let key_name = format!("@{}", tag_name);
                        if let Some(original_span) = self.check_and_record_key(&key_name, full_span)
                        {
                            // It's a duplicate - emit error first
                            self.state = State::EmitDuplicateTaggedKeyError {
                                at_span,
                                tag_span: t.span,
                                original_span,
                            };
                            continue;
                        }

                        // Tagged key - emit Key event with tag, then read value
                        self.state = State::AfterTaggedKey {
                            full_span,
                            tag_span: t.span,
                        };
                        return Some(Event::Key {
                            span: full_span,
                            tag: Some(tag_name),
                            payload: None,
                            kind: ScalarKind::Bare,
                        });
                    }

                    // Not a tagged key - it's a unit key @
                    // Check for duplicate unit key (empty string key)
                    if let Some(original_span) = self.check_and_record_key("", at_span) {
                        // It's a duplicate - emit error first, then come back
                        self.state = State::EmitDuplicateUnitKeyError {
                            at_span,
                            original_span,
                            next_token_kind: t.kind,
                            next_token_span: t.span,
                        };
                        continue;
                    }

                    // Continue to actual key emission (with the token we already read)
                    self.state = State::EmitUnitKeyValueWithToken {
                        at_span,
                        token_kind: t.kind,
                        token_span: t.span,
                    };
                    continue;
                }

                State::EmitDuplicateUnitKeyError {
                    at_span,
                    original_span,
                    next_token_kind,
                    next_token_span,
                } => {
                    // Emit the DuplicateKey error, then continue to EmitUnitKeyValueWithToken
                    self.state = State::EmitUnitKeyValueWithToken {
                        at_span,
                        token_kind: next_token_kind,
                        token_span: next_token_span,
                    };
                    return Some(Event::Error {
                        span: at_span,
                        kind: ParseErrorKind::DuplicateKey {
                            original: original_span,
                        },
                    });
                }

                State::EmitDuplicateTaggedKeyError {
                    at_span,
                    tag_span,
                    original_span,
                } => {
                    // Emit the DuplicateKey error, then continue to AfterTaggedKey
                    let full_span = Span::new(at_span.start, tag_span.end);
                    self.state = State::AfterTaggedKey {
                        full_span,
                        tag_span,
                    };
                    return Some(Event::Error {
                        span: full_span,
                        kind: ParseErrorKind::DuplicateKey {
                            original: original_span,
                        },
                    });
                }

                State::AfterTaggedKey {
                    full_span,
                    tag_span,
                } => {
                    // After emitting tagged key, read the value
                    let t = self.next_token_skip_ws();

                    match t.kind {
                        TokenKind::BareScalar => {
                            self.state = State::AfterBareScalarValue { value_span: t.span };
                            return Some(Event::Scalar {
                                span: t.span,
                                value: Cow::Borrowed(self.text_at(t.span)),
                                kind: ScalarKind::Bare,
                            });
                        }
                        TokenKind::QuotedScalar => {
                            let text = self.text_at(t.span);
                            self.state = State::EmitEntryEnd;
                            return Some(Event::Scalar {
                                span: t.span,
                                value: self.unescape_quoted(text),
                                kind: ScalarKind::Quoted,
                            });
                        }
                        TokenKind::Newline | TokenKind::Eof => {
                            self.state = State::EmitEntryEnd;
                            return Some(Event::Unit { span: full_span });
                        }
                        TokenKind::RBrace => {
                            // Key with unit value, then close brace
                            self.state = State::EmitUnitThenEntryEndThenObjectEnd {
                                unit_span: full_span,
                                rbrace_span: t.span,
                            };
                            continue;
                        }
                        TokenKind::Comma => {
                            // Key with unit value, then comma (entry separator)
                            self.state = State::EmitEntryEndWithSeparator {
                                separator: SeparatorStyle::Comma,
                                sep_span: t.span,
                            };
                            return Some(Event::Unit { span: full_span });
                        }
                        TokenKind::LBrace => {
                            // Go to the state that will emit ObjectStart
                            self.state = State::EmitObjectStartValue { span: t.span };
                            continue;
                        }
                        TokenKind::LParen => {
                            // Go to the state that will emit SequenceStart
                            self.state = State::EmitSequenceStartValue { span: t.span };
                            continue;
                        }
                        TokenKind::At => {
                            // Tag value - go to the state that will read tag name and emit TagStart
                            self.state = State::EmitTagStart { tag_span: t.span };
                            continue;
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

                State::EmitUnitKeyValueWithToken {
                    at_span,
                    token_kind,
                    token_span,
                } => {
                    // Skip whitespace if needed - but we already have the token info
                    let (t_kind, t_span) = if token_kind == TokenKind::Whitespace {
                        let next = self.next_token_skip_ws();
                        (next.kind, next.span)
                    } else {
                        (token_kind, token_span)
                    };

                    match t_kind {
                        TokenKind::BareScalar => {
                            self.state = State::AfterBareScalarValue { value_span: t_span };
                            return Some(Event::Key {
                                span: at_span,
                                tag: None,
                                payload: None,
                                kind: ScalarKind::Bare,
                            });
                        }
                        TokenKind::QuotedScalar => {
                            self.state = State::EmitScalarValue {
                                span: t_span,
                                kind: ScalarKind::Quoted,
                            };
                            return Some(Event::Key {
                                span: at_span,
                                tag: None,
                                payload: None,
                                kind: ScalarKind::Bare,
                            });
                        }
                        TokenKind::Newline | TokenKind::Eof => {
                            self.state = State::EmitUnitValue { span: at_span };
                            return Some(Event::Key {
                                span: at_span,
                                tag: None,
                                payload: None,
                                kind: ScalarKind::Bare,
                            });
                        }
                        TokenKind::LBrace => {
                            self.state = State::EmitObjectStartValue { span: t_span };
                            return Some(Event::Key {
                                span: at_span,
                                tag: None,
                                payload: None,
                                kind: ScalarKind::Bare,
                            });
                        }
                        TokenKind::LParen => {
                            self.state = State::EmitSequenceStartValue { span: t_span };
                            return Some(Event::Key {
                                span: at_span,
                                tag: None,
                                payload: None,
                                kind: ScalarKind::Bare,
                            });
                        }
                        TokenKind::At => {
                            // Tag value
                            self.state = State::EmitTagStart { tag_span: t_span };
                            return Some(Event::Key {
                                span: at_span,
                                tag: None,
                                payload: None,
                                kind: ScalarKind::Bare,
                            });
                        }
                        _ => {
                            self.state = State::EmitEntryEnd;
                            return Some(Event::Error {
                                span: t_span,
                                kind: ParseErrorKind::UnexpectedToken,
                            });
                        }
                    }
                }

                State::EmitDuplicateKeyError {
                    key_span,
                    key_kind,
                    original_span,
                } => {
                    // Emit the DuplicateKey error, then continue to EmitKeyAfterDuplicateCheck
                    // (which skips the duplicate check)
                    self.state = State::EmitKeyAfterDuplicateCheck { key_span, key_kind };
                    return Some(Event::Error {
                        span: key_span,
                        kind: ParseErrorKind::DuplicateKey {
                            original: original_span,
                        },
                    });
                }

                State::EmitKey { key_span, key_kind } => {
                    // Get the key text and check for duplicates
                    let key_text = self.text_at(key_span);
                    let normalized_key = match key_kind {
                        ScalarKind::Quoted => self.unescape_quoted(key_text),
                        _ => Cow::Borrowed(key_text),
                    };

                    // Check for duplicate key
                    if let Some(original_span) =
                        self.check_and_record_key(&normalized_key, key_span)
                    {
                        // It's a duplicate - emit error first, then come back
                        self.state = State::EmitDuplicateKeyError {
                            key_span,
                            key_kind,
                            original_span,
                        };
                        continue;
                    }

                    // Continue to the actual key emission logic
                    self.state = State::EmitKeyAfterDuplicateCheck { key_span, key_kind };
                    continue;
                }

                State::EmitKeyAfterDuplicateCheck { key_span, key_kind } => {
                    // Get the key text (duplicate check already done)
                    let key_text = self.text_at(key_span);
                    let key_payload = match key_kind {
                        ScalarKind::Quoted => self.unescape_quoted(key_text),
                        _ => Cow::Borrowed(key_text),
                    };

                    // Read next token - check for whitespace first
                    let first_token = self.lexer.next_token();
                    let (t, had_whitespace) = if first_token.kind == TokenKind::Whitespace {
                        (self.next_token_skip_ws(), true)
                    } else {
                        (first_token, false)
                    };
                    trace!(token = ?t, had_whitespace, "EmitKey got token");

                    match t.kind {
                        TokenKind::BareScalar => {
                            // Don't emit yet - check for `>` first
                            self.state = State::AfterBareScalarValue { value_span: t.span };
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
                            self.state = State::EmitUnitThenEntryEndThenObjectEnd {
                                unit_span: key_span,
                                rbrace_span: t.span,
                            };
                            return Some(Event::Key {
                                span: key_span,
                                tag: None,
                                payload: Some(key_payload),
                                kind: key_kind,
                            });
                        }

                        TokenKind::LBrace => {
                            // Nested object as value - check for missing whitespace
                            if !had_whitespace && key_kind == ScalarKind::Bare {
                                // key{} without whitespace is an error
                                self.state = State::EmitObjectStartValue { span: t.span };
                                return Some(Event::Error {
                                    span: t.span,
                                    kind: ParseErrorKind::MissingWhitespaceBeforeBlock,
                                });
                            }
                            self.state = State::EmitObjectStartValue { span: t.span };
                            return Some(Event::Key {
                                span: key_span,
                                tag: None,
                                payload: Some(key_payload),
                                kind: key_kind,
                            });
                        }

                        TokenKind::LParen => {
                            // Sequence as value - check for missing whitespace
                            if !had_whitespace && key_kind == ScalarKind::Bare {
                                // key() without whitespace is an error
                                self.state = State::EmitSequenceStartValue { span: t.span };
                                return Some(Event::Error {
                                    span: t.span,
                                    kind: ParseErrorKind::MissingWhitespaceBeforeBlock,
                                });
                            }
                            self.state = State::EmitSequenceStartValue { span: t.span };
                            return Some(Event::Key {
                                span: key_span,
                                tag: None,
                                payload: Some(key_payload),
                                kind: key_kind,
                            });
                        }

                        TokenKind::At => {
                            // Tagged value - emit Key first, then handle tag
                            self.state = State::EmitTagStart { tag_span: t.span };
                            return Some(Event::Key {
                                span: key_span,
                                tag: None,
                                payload: Some(key_payload),
                                kind: key_kind,
                            });
                        }

                        TokenKind::Gt => {
                            // `>` with space before it - not an attribute, just error
                            // But we need to emit Key first, then Unit, then error
                            // Actually, let's treat this as "key with unit value, followed by error"
                            self.state = State::EmitUnitThenError {
                                unit_span: key_span,
                                error_span: t.span,
                            };
                            return Some(Event::Key {
                                span: key_span,
                                tag: None,
                                payload: Some(key_payload),
                                kind: key_kind,
                            });
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
                    // For quoted strings, check for invalid escapes first
                    if kind == ScalarKind::Quoted {
                        let text = self.text_at(span);
                        let invalid_escapes = self.find_invalid_escapes(text);
                        if !invalid_escapes.is_empty() {
                            self.state = State::EmitInvalidEscapes {
                                span,
                                kind,
                                errors: invalid_escapes,
                                error_index: 0,
                            };
                            continue;
                        }
                    }

                    let text = self.text_at(span);
                    let value = match kind {
                        ScalarKind::Quoted => self.unescape_quoted(text),
                        _ => Cow::Borrowed(text),
                    };
                    // Check for TooManyAtoms after the value
                    self.state = State::AfterQuotedScalarValue { value_span: span };
                    return Some(Event::Scalar { span, value, kind });
                }

                State::EmitBareScalarValue { span } => {
                    // Just emit the scalar - we know it's a value
                    self.state = State::EmitEntryEnd;
                    return Some(Event::Scalar {
                        span,
                        value: Cow::Borrowed(self.text_at(span)),
                        kind: ScalarKind::Bare,
                    });
                }

                State::AfterBareScalarValue { value_span } => {
                    // We have a pending bare scalar (not yet emitted). Check for `>`.
                    let t = self.lexer.next_token();

                    match t.kind {
                        TokenKind::Gt if t.span.start == value_span.end => {
                            // Attribute! Pending scalar is attr key, not value.
                            // Emit ObjectStart, then handle attr entry.
                            self.context_stack.push(Context::AttrObject);
                            self.state = State::EmitAttrEntryStart {
                                attr_key_span: value_span,
                            };
                            return Some(Event::ObjectStart {
                                span: value_span,
                                separator: Separator::Comma,
                            });
                        }

                        TokenKind::RBrace => {
                            // Value followed by close brace - emit scalar, then close
                            self.state = State::EmitEntryEndThenObjectEnd {
                                rbrace_span: t.span,
                            };
                            return Some(Event::Scalar {
                                span: value_span,
                                value: Cow::Borrowed(self.text_at(value_span)),
                                kind: ScalarKind::Bare,
                            });
                        }

                        TokenKind::RParen => {
                            // Value followed by close paren - emit scalar, then close seq
                            self.state = State::EmitEntryEndThenSeqEnd {
                                rparen_span: t.span,
                            };
                            return Some(Event::Scalar {
                                span: value_span,
                                value: Cow::Borrowed(self.text_at(value_span)),
                                kind: ScalarKind::Bare,
                            });
                        }

                        TokenKind::Newline | TokenKind::Eof => {
                            // Normal value with newline separator - emit scalar now
                            self.state = State::EmitEntryEndWithSeparator {
                                separator: SeparatorStyle::Newline,
                                sep_span: t.span,
                            };
                            return Some(Event::Scalar {
                                span: value_span,
                                value: Cow::Borrowed(self.text_at(value_span)),
                                kind: ScalarKind::Bare,
                            });
                        }

                        TokenKind::Comma => {
                            // Normal value with comma separator
                            self.state = State::EmitEntryEndWithSeparator {
                                separator: SeparatorStyle::Comma,
                                sep_span: t.span,
                            };
                            return Some(Event::Scalar {
                                span: value_span,
                                value: Cow::Borrowed(self.text_at(value_span)),
                                kind: ScalarKind::Bare,
                            });
                        }

                        TokenKind::Whitespace => {
                            // After value and whitespace, check what follows
                            // If it's another scalar on the same line, that's TooManyAtoms
                            let next = self.next_token_skip_ws();
                            match next.kind {
                                TokenKind::Newline | TokenKind::Eof => {
                                    self.state = State::EmitEntryEndWithSeparator {
                                        separator: SeparatorStyle::Newline,
                                        sep_span: next.span,
                                    };
                                    return Some(Event::Scalar {
                                        span: value_span,
                                        value: Cow::Borrowed(self.text_at(value_span)),
                                        kind: ScalarKind::Bare,
                                    });
                                }
                                TokenKind::Comma => {
                                    self.state = State::EmitEntryEndWithSeparator {
                                        separator: SeparatorStyle::Comma,
                                        sep_span: next.span,
                                    };
                                    return Some(Event::Scalar {
                                        span: value_span,
                                        value: Cow::Borrowed(self.text_at(value_span)),
                                        kind: ScalarKind::Bare,
                                    });
                                }
                                TokenKind::RBrace => {
                                    // Value, then close object
                                    self.state = State::EmitEntryEndThenObjectEnd {
                                        rbrace_span: next.span,
                                    };
                                    return Some(Event::Scalar {
                                        span: value_span,
                                        value: Cow::Borrowed(self.text_at(value_span)),
                                        kind: ScalarKind::Bare,
                                    });
                                }
                                TokenKind::RParen => {
                                    // Value, then close sequence
                                    self.state = State::EmitEntryEndThenSeqEnd {
                                        rparen_span: next.span,
                                    };
                                    return Some(Event::Scalar {
                                        span: value_span,
                                        value: Cow::Borrowed(self.text_at(value_span)),
                                        kind: ScalarKind::Bare,
                                    });
                                }
                                TokenKind::LineComment => {
                                    // OK - entry ends here
                                    self.state = State::EmitEntryEnd;
                                    return Some(Event::Scalar {
                                        span: value_span,
                                        value: Cow::Borrowed(self.text_at(value_span)),
                                        kind: ScalarKind::Bare,
                                    });
                                }
                                TokenKind::BareScalar | TokenKind::QuotedScalar => {
                                    // Too many atoms! Emit scalar value, then error for the extra atom.
                                    self.state = State::EmitScalarThenTooManyAtoms {
                                        value_span,
                                        error_span: next.span,
                                    };
                                    continue;
                                }
                                _ => {
                                    self.state = State::EmitEntryEnd;
                                    return Some(Event::Scalar {
                                        span: value_span,
                                        value: Cow::Borrowed(self.text_at(value_span)),
                                        kind: ScalarKind::Bare,
                                    });
                                }
                            }
                        }

                        _ => {
                            // Unexpected - emit error
                            self.state = State::EmitEntryEnd;
                            return Some(Event::Error {
                                span: t.span,
                                kind: ParseErrorKind::TooManyAtoms,
                            });
                        }
                    }
                }

                State::EmitUnitValue { span } => {
                    self.state = State::EmitEntryEnd;
                    return Some(Event::Unit { span });
                }

                State::EmitUnitThenError {
                    unit_span,
                    error_span,
                } => {
                    self.state = State::EmitErrorThenEntryEnd { error_span };
                    return Some(Event::Unit { span: unit_span });
                }

                State::EmitErrorThenEntryEnd { error_span } => {
                    self.state = State::EmitEntryEnd;
                    return Some(Event::Error {
                        span: error_span,
                        kind: ParseErrorKind::UnexpectedToken,
                    });
                }

                State::EmitTooManyAtomsThenEntryEnd { error_span } => {
                    // After TooManyAtoms, skip to end of line/object/sequence
                    // to avoid cascading errors
                    loop {
                        let t = self.lexer.next_token();
                        match t.kind {
                            TokenKind::Newline | TokenKind::Eof => break,
                            TokenKind::RBrace => {
                                // Need to handle close brace
                                match self.context_stack.last() {
                                    Some(Context::Object {
                                        implicit: false, ..
                                    }) => {
                                        self.context_stack.pop();
                                        // Don't emit ObjectEnd here - we'll do it after EntryEnd
                                        // Actually, this is complex. Just break and let normal flow handle it.
                                        break;
                                    }
                                    _ => break,
                                }
                            }
                            TokenKind::RParen => break,
                            _ => continue, // Skip other tokens
                        }
                    }
                    self.state = State::EmitEntryEnd;
                    return Some(Event::Error {
                        span: error_span,
                        kind: ParseErrorKind::TooManyAtoms,
                    });
                }

                State::EmitUnitThenEntryEndThenObjectEnd {
                    unit_span,
                    rbrace_span,
                } => {
                    self.state = State::EmitEntryEndAfterUnitThenObjectEnd { rbrace_span };
                    return Some(Event::Unit { span: unit_span });
                }

                State::EmitEntryEndAfterUnitThenObjectEnd { rbrace_span } => {
                    self.state = State::EmitObjectEndAfterEntry { rbrace_span };
                    return Some(Event::EntryEnd);
                }

                State::EmitObjectEndAfterError { rbrace_span } => {
                    self.state = State::EmitEntryEnd;
                    return Some(Event::ObjectEnd { span: rbrace_span });
                }

                State::EmitExplicitRootAfterDocComment { lbrace_span } => {
                    self.state = State::ExpectEntry;
                    return Some(Event::ObjectStart {
                        span: lbrace_span,
                        separator: Separator::Comma,
                    });
                }

                State::ProcessNextDocComment { doc_span } => {
                    // We have a doc comment to emit, then check what follows
                    let doc_text = self.text_at(doc_span);
                    let next = self.next_token_skip_ws_nl();
                    match next.kind {
                        TokenKind::Eof => {
                            self.state = State::EmitDocumentEnd;
                            return Some(Event::Error {
                                span: doc_span,
                                kind: ParseErrorKind::DanglingDocComment,
                            });
                        }
                        TokenKind::RBrace => match self.context_stack.pop() {
                            Some(Context::Object {
                                implicit: false, ..
                            }) => {
                                self.state = State::EmitObjectEndAfterError {
                                    rbrace_span: next.span,
                                };
                                return Some(Event::Error {
                                    span: doc_span,
                                    kind: ParseErrorKind::DanglingDocComment,
                                });
                            }
                            Some(ctx) => {
                                self.context_stack.push(ctx);
                                self.state = State::ExpectEntry;
                                return Some(Event::Error {
                                    span: doc_span,
                                    kind: ParseErrorKind::DanglingDocComment,
                                });
                            }
                            None => {
                                self.state = State::EmitDocumentEnd;
                                return Some(Event::Error {
                                    span: doc_span,
                                    kind: ParseErrorKind::DanglingDocComment,
                                });
                            }
                        },
                        TokenKind::BareScalar => {
                            let text = self.text_at(next.span);
                            if text.contains('.') {
                                // Check for path errors before processing
                                let segments: Vec<&str> = text.split('.').collect();
                                if let Some(error_kind) = self.check_dotted_path(&segments) {
                                    // Skip to end of entry and emit error
                                    loop {
                                        let skip = self.lexer.next_token();
                                        match skip.kind {
                                            TokenKind::Newline
                                            | TokenKind::Eof
                                            | TokenKind::Comma
                                            | TokenKind::RBrace => break,
                                            _ => continue,
                                        }
                                    }
                                    self.state = State::ExpectEntry;
                                    return Some(Event::Error {
                                        span: next.span,
                                        kind: error_kind,
                                    });
                                }
                                // Record this path and close any sibling branches that had explicit {}
                                self.record_dotted_path_and_close_siblings(&segments);
                                self.state = State::EmitDottedPath {
                                    full_span: next.span,
                                    offset: 0,
                                    depth: 0,
                                };
                            } else {
                                self.state = State::EmitEntryStart {
                                    key_span: next.span,
                                    key_kind: ScalarKind::Bare,
                                };
                            }
                            return Some(Event::DocComment {
                                span: doc_span,
                                text: doc_text,
                            });
                        }
                        TokenKind::QuotedScalar => {
                            self.state = State::EmitEntryStart {
                                key_span: next.span,
                                key_kind: ScalarKind::Quoted,
                            };
                            return Some(Event::DocComment {
                                span: doc_span,
                                text: doc_text,
                            });
                        }
                        TokenKind::DocComment => {
                            self.state = State::ProcessNextDocComment {
                                doc_span: next.span,
                            };
                            return Some(Event::DocComment {
                                span: doc_span,
                                text: doc_text,
                            });
                        }
                        _ => {
                            self.state = State::ExpectEntry;
                            return Some(Event::Error {
                                span: doc_span,
                                kind: ParseErrorKind::DanglingDocComment,
                            });
                        }
                    }
                }

                State::EmitScalarThenTooManyAtoms {
                    value_span,
                    error_span,
                } => {
                    self.state = State::EmitTooManyAtomsThenEntryEnd { error_span };
                    return Some(Event::Scalar {
                        span: value_span,
                        value: Cow::Borrowed(self.text_at(value_span)),
                        kind: ScalarKind::Bare,
                    });
                }

                State::EmitInvalidEscapesInKey {
                    key_span,
                    key_kind,
                    errors,
                    error_index,
                } => {
                    if error_index < errors.len() {
                        let (offset, len) = errors[error_index];
                        let error_start = key_span.start + 1 + offset as u32;
                        let error_span = Span::new(error_start, error_start + len as u32);
                        let escape_text = self.text_at(error_span).to_string();

                        self.state = State::EmitInvalidEscapesInKey {
                            key_span,
                            key_kind,
                            errors,
                            error_index: error_index + 1,
                        };
                        return Some(Event::Error {
                            span: error_span,
                            kind: ParseErrorKind::InvalidEscape(escape_text),
                        });
                    } else {
                        // All errors emitted, continue to emit EntryStart and Key
                        if let Some(Context::Object { has_entries, .. }) =
                            self.context_stack.last_mut()
                        {
                            *has_entries = true;
                        }
                        self.state = State::EmitKey { key_span, key_kind };
                        return Some(Event::EntryStart);
                    }
                }

                State::EmitInvalidEscapes {
                    span,
                    kind,
                    errors,
                    error_index,
                } => {
                    if error_index < errors.len() {
                        let (offset, len) = errors[error_index];
                        // Offset is relative to inner string (after opening quote)
                        let error_start = span.start + 1 + offset as u32;
                        let error_span = Span::new(error_start, error_start + len as u32);

                        // Get the escape sequence text
                        let escape_text = self.text_at(error_span).to_string();

                        self.state = State::EmitInvalidEscapes {
                            span,
                            kind,
                            errors,
                            error_index: error_index + 1,
                        };
                        return Some(Event::Error {
                            span: error_span,
                            kind: ParseErrorKind::InvalidEscape(escape_text),
                        });
                    } else {
                        // All errors emitted, now emit the scalar
                        let text = self.text_at(span);
                        let value = match kind {
                            ScalarKind::Quoted => self.unescape_quoted(text),
                            _ => Cow::Borrowed(text),
                        };
                        self.state = State::AfterQuotedScalarValue { value_span: span };
                        return Some(Event::Scalar { span, value, kind });
                    }
                }

                State::AfterQuotedScalarValue { value_span: _ } => {
                    // Check for TooManyAtoms - another scalar on same line
                    let t = self.lexer.next_token();

                    match t.kind {
                        TokenKind::Newline | TokenKind::Eof | TokenKind::Comma => {
                            // OK - entry ends
                            self.state = State::EmitEntryEnd;
                            continue;
                        }
                        TokenKind::RBrace => {
                            // Close object - need EntryEnd first, then ObjectEnd
                            // Use existing state machine flow which handles tag payloads
                            self.state = State::EmitEntryEndThenObjectEnd {
                                rbrace_span: t.span,
                            };
                            continue;
                        }
                        TokenKind::RParen => {
                            // Close sequence - need EntryEnd first, then SequenceEnd
                            // Use existing state machine flow which handles tag payloads
                            self.state = State::EmitEntryEndThenSeqEnd {
                                rparen_span: t.span,
                            };
                            continue;
                        }
                        TokenKind::Whitespace => {
                            // Check what follows whitespace
                            let next = self.next_token_skip_ws();
                            match next.kind {
                                TokenKind::Newline
                                | TokenKind::Eof
                                | TokenKind::Comma
                                | TokenKind::LineComment => {
                                    self.state = State::EmitEntryEnd;
                                    continue;
                                }
                                TokenKind::RBrace => {
                                    // Need EntryEnd first, then ObjectEnd
                                    self.state = State::EmitEntryEndThenObjectEnd {
                                        rbrace_span: next.span,
                                    };
                                    continue;
                                }
                                TokenKind::RParen => {
                                    // Need EntryEnd first, then SequenceEnd
                                    self.state = State::EmitEntryEndThenSeqEnd {
                                        rparen_span: next.span,
                                    };
                                    continue;
                                }
                                TokenKind::BareScalar | TokenKind::QuotedScalar => {
                                    // TooManyAtoms!
                                    self.state = State::EmitTooManyAtomsThenEntryEnd {
                                        error_span: next.span,
                                    };
                                    continue;
                                }
                                _ => {
                                    self.state = State::EmitEntryEnd;
                                    return Some(Event::Error {
                                        span: next.span,
                                        kind: ParseErrorKind::UnexpectedToken,
                                    });
                                }
                            }
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

                State::EmitEntryEnd => {
                    // If we're finishing a dotted path value, continue closing
                    if self.dotted_depth > 0 {
                        // Pop the key from the path
                        self.pop_path_segment();
                        // Continue closing the dotted path
                        self.state = State::CloseDottedPath {
                            depth: self.dotted_depth,
                        };
                        self.dotted_depth = 0; // Reset
                        return Some(Event::EntryEnd);
                    }

                    // Normal case: after EntryEnd, check what context we're in
                    match self.context_stack.last() {
                        Some(Context::Object { .. }) => {
                            self.state = State::ExpectEntry;
                        }
                        Some(Context::Sequence { .. }) => {
                            self.state = State::ExpectSeqElem;
                        }
                        Some(Context::AttrObject) => {
                            self.state = State::AfterAttrEntryEnd;
                        }
                        None => {
                            self.state = State::EmitDocumentEnd;
                        }
                    }
                    return Some(Event::EntryEnd);
                }

                State::EmitEntryEndWithSeparator {
                    separator: new_sep,
                    sep_span,
                } => {
                    // Check separator consistency
                    // Get current separator style without moving the context
                    let (implicit, current_sep) = match self.context_stack.last() {
                        Some(Context::Object {
                            implicit,
                            separator,
                            ..
                        }) => (*implicit, *separator),
                        _ => {
                            // Not in an object - just go to ExpectEntry
                            self.state = State::ExpectEntry;
                            return Some(Event::EntryEnd);
                        }
                    };

                    match current_sep {
                        SeparatorStyle::Unknown => {
                            // First separator - record it by updating the context
                            if let Some(Context::Object { separator, .. }) =
                                self.context_stack.last_mut()
                            {
                                *separator = new_sep;
                            }
                            self.state = State::ExpectEntry;
                        }
                        _ if current_sep == new_sep => {
                            // Same separator - OK
                            self.state = State::ExpectEntry;
                        }
                        _ => {
                            // Mixed separators - error!
                            // But we still need to continue, so go to ExpectEntry
                            self.state = State::ExpectEntry;
                            return Some(Event::Error {
                                span: sep_span,
                                kind: ParseErrorKind::MixedSeparators,
                            });
                        }
                    }

                    let _ = implicit; // suppress warning
                    return Some(Event::EntryEnd);
                }

                State::EmitEntryEndThenObjectEnd { rbrace_span } => {
                    self.state = State::EmitObjectEndAfterEntry { rbrace_span };
                    return Some(Event::EntryEnd);
                }

                State::EmitObjectEndAfterEntry { rbrace_span } => {
                    let is_tag_payload = match self.context_stack.pop() {
                        Some(Context::Object { is_tag_payload, .. }) => is_tag_payload,
                        _ => false,
                    };
                    if self.context_stack.is_empty() {
                        // This was the root explicit object - no outer entry to close
                        self.state = State::EmitDocumentEnd;
                    } else if is_tag_payload {
                        self.state = State::EmitTagEnd;
                    } else {
                        self.state = State::EmitEntryEnd;
                    }
                    return Some(Event::ObjectEnd { span: rbrace_span });
                }

                State::EmitEntryEndThenSeqEnd { rparen_span } => {
                    self.state = State::EmitSeqEndAfterEntry { rparen_span };
                    return Some(Event::EntryEnd);
                }

                State::EmitSeqEndAfterEntry { rparen_span } => {
                    let is_tag_payload = match self.context_stack.pop() {
                        Some(Context::Sequence { is_tag_payload }) => is_tag_payload,
                        _ => false,
                    };
                    if self.context_stack.is_empty() {
                        // This was the root explicit sequence - no outer entry to close
                        self.state = State::EmitDocumentEnd;
                    } else if is_tag_payload {
                        self.state = State::EmitTagEnd;
                    } else {
                        self.state = State::EmitEntryEnd;
                    }
                    return Some(Event::SequenceEnd { span: rparen_span });
                }

                State::EmitObjectStartValue { span } => {
                    self.context_stack.push(Context::Object {
                        implicit: false,
                        separator: SeparatorStyle::Unknown,
                        seen_keys: std::collections::HashMap::new(),
                        is_tag_payload: false,
                        has_entries: false,
                    });
                    self.state = State::ExpectEntry;
                    return Some(Event::ObjectStart {
                        span,
                        separator: Separator::Comma, // Explicit objects use comma
                    });
                }

                State::EmitSequenceStartValue { span } => {
                    self.context_stack.push(Context::Sequence {
                        is_tag_payload: false,
                    });
                    self.state = State::ExpectSeqElem;
                    return Some(Event::SequenceStart { span });
                }

                State::EmitTagStart { tag_span } => {
                    // Read tag name (bare scalar immediately after @)
                    let t = self.lexer.next_token();

                    match t.kind {
                        TokenKind::BareScalar if t.span.start == tag_span.end => {
                            let full_text = self.text_at(t.span);
                            // Extract tag name - may contain trailing @ for explicit unit
                            let (tag_name, has_trailing_at) =
                                if let Some(at_pos) = full_text.find('@') {
                                    (&full_text[..at_pos], true)
                                } else {
                                    (full_text, false)
                                };

                            let name_end = t.span.start + tag_name.len() as u32;

                            // Validate tag name
                            if !self.is_valid_tag_name(tag_name) {
                                // Invalid tag name - emit error, skip to end of line
                                loop {
                                    let skip = self.lexer.next_token();
                                    match skip.kind {
                                        TokenKind::Newline | TokenKind::Eof => break,
                                        _ => continue,
                                    }
                                }
                                self.state = State::EmitEntryEnd;
                                // Error span is just the tag name part, not the @
                                return Some(Event::Error {
                                    span: t.span,
                                    kind: ParseErrorKind::InvalidTagName,
                                });
                            }

                            if has_trailing_at {
                                // @tag@ - explicit unit payload
                                self.state = State::EmitTagEndWithUnit {
                                    unit_span: Span::new(name_end, name_end + 1),
                                };
                                return Some(Event::TagStart {
                                    span: Span::new(tag_span.start, name_end),
                                    name: tag_name,
                                });
                            }

                            self.state = State::AfterTagStart {
                                tag_span: Span::new(t.span.start, name_end),
                            };
                            return Some(Event::TagStart {
                                span: Span::new(tag_span.start, name_end),
                                name: tag_name,
                            });
                        }
                        _ => {
                            // @ not followed by identifier - just @ as unit value
                            self.state = State::EmitEntryEnd;
                            return Some(Event::Unit { span: tag_span });
                        }
                    }
                }

                State::AfterTagStart { tag_span } => {
                    // Check for payload (immediately following, no whitespace)
                    let t = self.lexer.next_token();
                    trace!(token = ?t, "AfterTagStart");

                    match t.kind {
                        TokenKind::LBrace if t.span.start == tag_span.end => {
                            // @tag{...} - object payload
                            self.context_stack.push(Context::Object {
                                implicit: false,
                                separator: SeparatorStyle::Unknown,
                                seen_keys: std::collections::HashMap::new(),
                                is_tag_payload: true,
                                has_entries: false,
                            });
                            self.state = State::ExpectEntry;
                            return Some(Event::ObjectStart {
                                span: t.span,
                                separator: Separator::Comma,
                            });
                        }

                        TokenKind::LParen if t.span.start == tag_span.end => {
                            // @tag(...) - sequence payload
                            self.context_stack.push(Context::Sequence {
                                is_tag_payload: true,
                            });
                            self.state = State::ExpectSeqElem;
                            return Some(Event::SequenceStart { span: t.span });
                        }

                        TokenKind::BareScalar if t.span.start == tag_span.end => {
                            // @tag"value" - scalar payload
                            self.state = State::EmitTagEnd;
                            return Some(Event::Scalar {
                                span: t.span,
                                value: Cow::Borrowed(self.text_at(t.span)),
                                kind: ScalarKind::Bare,
                            });
                        }

                        TokenKind::QuotedScalar if t.span.start == tag_span.end => {
                            // @tag"value" - quoted scalar payload
                            let text = self.text_at(t.span);
                            self.state = State::EmitTagEnd;
                            return Some(Event::Scalar {
                                span: t.span,
                                value: self.unescape_quoted(text),
                                kind: ScalarKind::Quoted,
                            });
                        }

                        TokenKind::At if t.span.start == tag_span.end => {
                            // @tag@ - explicit unit payload
                            self.state = State::EmitTagEnd;
                            return Some(Event::Unit { span: t.span });
                        }

                        // `)` after tag - close the tag, then close sequence
                        TokenKind::RParen => {
                            self.state = State::EmitTagEndThenSeqEnd {
                                rparen_span: t.span,
                            };
                            return Some(Event::TagEnd);
                        }

                        // `}` after tag - close the tag, then close object
                        TokenKind::RBrace => {
                            self.state = State::EmitTagEndThenObjEnd {
                                rbrace_span: t.span,
                            };
                            return Some(Event::TagEnd);
                        }

                        // Whitespace - tag has no immediate payload, but check for TooManyAtoms
                        TokenKind::Whitespace => {
                            // Check if there's another token on this line (TooManyAtoms)
                            let next = self.next_token_skip_ws();
                            match next.kind {
                                TokenKind::Newline | TokenKind::Eof | TokenKind::Comma => {
                                    // OK - entry ends
                                    match self.context_stack.last() {
                                        Some(Context::Object { .. }) => {
                                            self.state = State::EmitEntryEnd;
                                        }
                                        Some(Context::Sequence { .. }) => {
                                            self.state = State::ExpectSeqElem;
                                        }
                                        Some(Context::AttrObject) => {
                                            self.state = State::EmitEntryEnd;
                                        }
                                        None => {
                                            self.state = State::EmitDocumentEnd;
                                        }
                                    }
                                    return Some(Event::TagEnd);
                                }
                                TokenKind::RBrace => {
                                    // Tag value, then close object
                                    self.state = State::EmitTagEndThenObjEnd {
                                        rbrace_span: next.span,
                                    };
                                    return Some(Event::TagEnd);
                                }
                                TokenKind::RParen => {
                                    // Tag value, then close sequence
                                    self.state = State::EmitTagEndThenSeqEnd {
                                        rparen_span: next.span,
                                    };
                                    return Some(Event::TagEnd);
                                }
                                TokenKind::LineComment => {
                                    // Comment after tag - OK
                                    match self.context_stack.last() {
                                        Some(Context::Object { .. }) => {
                                            self.state = State::EmitEntryEnd;
                                        }
                                        Some(Context::Sequence { .. }) => {
                                            self.state = State::ExpectSeqElem;
                                        }
                                        Some(Context::AttrObject) => {
                                            self.state = State::EmitEntryEnd;
                                        }
                                        None => {
                                            self.state = State::EmitDocumentEnd;
                                        }
                                    }
                                    return Some(Event::TagEnd);
                                }
                                _ => {
                                    // In sequence context, whitespace-separated elements are valid
                                    if matches!(
                                        self.context_stack.last(),
                                        Some(Context::Sequence { .. })
                                    ) {
                                        // This is the next sequence element - need to re-process it
                                        // Store the token info we peeked and emit TagEnd
                                        self.state = State::ProcessPeekedSeqElem {
                                            peeked_kind: next.kind,
                                            peeked_span: next.span,
                                        };
                                        return Some(Event::TagEnd);
                                    }

                                    // TooManyAtoms - another token on the same line
                                    // Skip to end of line
                                    loop {
                                        let skip = self.lexer.next_token();
                                        match skip.kind {
                                            TokenKind::Newline | TokenKind::Eof => break,
                                            TokenKind::RBrace | TokenKind::RParen => break,
                                            _ => continue,
                                        }
                                    }
                                    self.state = State::EmitEntryEnd;
                                    return Some(Event::Error {
                                        span: next.span,
                                        kind: ParseErrorKind::TooManyAtoms,
                                    });
                                }
                            }
                        }

                        // Newline or other - tag has no payload (implicit unit)
                        _ => {
                            // Go back to appropriate context
                            match self.context_stack.last() {
                                Some(Context::Object { .. }) => {
                                    self.state = State::EmitEntryEnd;
                                }
                                Some(Context::Sequence { .. }) => {
                                    self.state = State::ExpectSeqElem;
                                }
                                Some(Context::AttrObject) => {
                                    self.state = State::EmitEntryEnd;
                                }
                                None => {
                                    self.state = State::EmitDocumentEnd;
                                }
                            }
                            return Some(Event::TagEnd);
                        }
                    }
                }

                State::EmitTagEnd => {
                    // After tag, go back to appropriate context
                    match self.context_stack.last() {
                        Some(Context::Object { .. }) => {
                            self.state = State::EmitEntryEnd;
                        }
                        Some(Context::Sequence { .. }) => {
                            self.state = State::ExpectSeqElem;
                        }
                        Some(Context::AttrObject) => {
                            self.state = State::EmitEntryEnd;
                        }
                        None => {
                            self.state = State::EmitDocumentEnd;
                        }
                    }
                    return Some(Event::TagEnd);
                }

                State::EmitTagEndWithUnit { unit_span } => {
                    // Emit Unit first, then go to EmitTagEnd
                    self.state = State::EmitTagEnd;
                    return Some(Event::Unit { span: unit_span });
                }

                State::EmitTagEndThenSeqEnd { rparen_span } => {
                    // Close sequence after tag
                    let is_tag_payload = match self.context_stack.pop() {
                        Some(Context::Sequence { is_tag_payload }) => is_tag_payload,
                        _ => false,
                    };
                    if self.context_stack.is_empty() {
                        // This was the root explicit sequence - no outer entry to close
                        self.state = State::EmitDocumentEnd;
                    } else if is_tag_payload {
                        // Sequence was a tag payload - emit TagEnd after
                        self.state = State::EmitTagEnd;
                    } else {
                        // Normal case - close the entry containing the sequence
                        self.state = State::EmitEntryEnd;
                    }
                    return Some(Event::SequenceEnd { span: rparen_span });
                }

                State::EmitTagEndThenObjEnd { rbrace_span } => {
                    // After tag ends, we need to close the entry first, then the object
                    // Sequence: TagEnd (already emitted) -> EntryEnd -> ObjectEnd
                    self.state = State::EmitObjectEndAfterEntry { rbrace_span };
                    return Some(Event::EntryEnd);
                }

                State::EmitAttrObjectStart { main_key_span: _ } => {
                    // This state is unused - we go directly to EmitAttrEntryStart
                    unreachable!("EmitAttrObjectStart should not be reached");
                }

                State::EmitAttrEntryStart { attr_key_span } => {
                    // Emit EntryStart, then Key
                    self.state = State::EmitAttrKey { attr_key_span };
                    return Some(Event::EntryStart);
                }

                State::EmitAttrKey { attr_key_span } => {
                    // Emit Key, then expect value
                    let key_text = self.text_at(attr_key_span);
                    self.state = State::ExpectAttrValue { attr_key_span };
                    return Some(Event::Key {
                        span: attr_key_span,
                        tag: None,
                        payload: Some(Cow::Borrowed(key_text)),
                        kind: ScalarKind::Bare,
                    });
                }

                State::ExpectAttrValue { attr_key_span: _ } => {
                    // Read the attribute value
                    let t = self.lexer.next_token();
                    match t.kind {
                        TokenKind::BareScalar => {
                            // Don't emit yet - check for `>`
                            self.state = State::AfterAttrBareValue { value_span: t.span };
                            continue;
                        }
                        TokenKind::QuotedScalar => {
                            self.state = State::EmitAttrScalarValue {
                                span: t.span,
                                kind: ScalarKind::Quoted,
                            };
                            continue;
                        }
                        TokenKind::LParen => {
                            // Sequence value for attribute - use regular seq handling
                            // When it closes, EmitEntryEnd will see AttrObject context
                            self.context_stack.push(Context::Sequence {
                                is_tag_payload: false,
                            });
                            self.state = State::ExpectSeqElem;
                            return Some(Event::SequenceStart { span: t.span });
                        }
                        TokenKind::LBrace => {
                            // Object value for attribute - use regular obj handling
                            self.context_stack.push(Context::Object {
                                implicit: false,
                                separator: SeparatorStyle::Unknown,
                                seen_keys: std::collections::HashMap::new(),
                                is_tag_payload: false,
                                has_entries: false,
                            });
                            self.state = State::ExpectEntry;
                            return Some(Event::ObjectStart {
                                span: t.span,
                                separator: Separator::Comma,
                            });
                        }
                        TokenKind::At => {
                            // Tag value for attribute
                            self.state = State::EmitTagStartInAttr { tag_span: t.span };
                            continue;
                        }
                        _ => {
                            self.state = State::EmitAttrObjectEnd;
                            return Some(Event::Error {
                                span: t.span,
                                kind: ParseErrorKind::ExpectedValue,
                            });
                        }
                    }
                }

                State::AfterAttrBareValue { value_span } => {
                    // Check for `>` immediately after
                    let t = self.lexer.next_token();
                    match t.kind {
                        TokenKind::Gt if t.span.start == value_span.end => {
                            // This is another attribute key! Emit value, close entry, start new.
                            // But wait - we need to emit the PREVIOUS value first.
                            // value_span is actually the NEXT attr key, not a value.
                            // We need to emit Unit for the previous attr, then start new entry.
                            self.state = State::EmitAttrEntryStart {
                                attr_key_span: value_span,
                            };
                            return Some(Event::Unit { span: value_span });
                        }
                        _ => {
                            // Normal value - emit it
                            self.state = State::EmitAttrEntryEnd;
                            return Some(Event::Scalar {
                                span: value_span,
                                value: Cow::Borrowed(self.text_at(value_span)),
                                kind: ScalarKind::Bare,
                            });
                        }
                    }
                }

                State::EmitAttrScalarValue { span, kind } => {
                    let text = self.text_at(span);
                    let value = match kind {
                        ScalarKind::Quoted => self.unescape_quoted(text),
                        _ => Cow::Borrowed(text),
                    };
                    self.state = State::EmitAttrEntryEnd;
                    return Some(Event::Scalar { span, value, kind });
                }

                State::EmitAttrEntryEnd => {
                    self.state = State::AfterAttrEntryEnd;
                    return Some(Event::EntryEnd);
                }

                State::AfterAttrEntryEnd => {
                    // Check for more attributes or close.
                    // Attributes look like: key1>val1 key2>val2
                    // So we need to see: whitespace, then bare scalar, then `>`.
                    let t = self.next_token_skip_ws();
                    match t.kind {
                        TokenKind::BareScalar => {
                            // Could be another attr key - check for `>`
                            let next = self.lexer.next_token();
                            if next.kind == TokenKind::Gt && next.span.start == t.span.end {
                                // Yes, another attribute!
                                self.state = State::EmitAttrEntryStart {
                                    attr_key_span: t.span,
                                };
                                continue;
                            } else {
                                // Not an attribute - close attr object, this is next entry
                                // But we consumed the token! Need to handle it.
                                // This bare scalar is the NEXT key in the parent object.
                                // We need to close attr object, close current entry,
                                // then start a new entry with this key.
                                // Store the key info in state.
                                self.state = State::EmitAttrObjectEndThenNewEntry {
                                    next_key_span: t.span,
                                    next_key_kind: ScalarKind::Bare,
                                };
                                continue;
                            }
                        }
                        TokenKind::Newline | TokenKind::Eof => {
                            // Done with attributes
                            self.state = State::EmitAttrObjectEnd;
                            continue;
                        }
                        TokenKind::RBrace => {
                            // Close attr object, then close parent object
                            self.state = State::EmitAttrObjectEndThenClose {
                                rbrace_span: t.span,
                            };
                            continue;
                        }
                        _ => {
                            self.state = State::EmitAttrObjectEnd;
                            return Some(Event::Error {
                                span: t.span,
                                kind: ParseErrorKind::UnexpectedToken,
                            });
                        }
                    }
                }

                State::EmitDottedPath {
                    full_span,
                    offset,
                    depth,
                } => {
                    let text = self.text_at(full_span);
                    let remaining = &text[offset as usize..];

                    // Validate: leading dot is an error
                    if remaining.starts_with('.') {
                        self.state = State::CloseDottedPath { depth };
                        return Some(Event::Error {
                            span: full_span,
                            kind: ParseErrorKind::InvalidKey,
                        });
                    }

                    // Find the next segment
                    let (segment, next_offset) = if let Some(dot_pos) = remaining.find('.') {
                        (&remaining[..dot_pos], offset + dot_pos as u32 + 1)
                    } else {
                        (remaining, full_span.end - full_span.start) // last segment
                    };

                    // Validate: empty segment is an error (shouldn't happen with above check)
                    if segment.is_empty() {
                        self.state = State::CloseDottedPath { depth };
                        return Some(Event::Error {
                            span: full_span,
                            kind: ParseErrorKind::InvalidKey,
                        });
                    }

                    // Validate: trailing dot (segment ends at a dot with nothing after)
                    let after_segment = &remaining[segment.len()..];
                    if after_segment == "." {
                        // Trailing dot with no segment after
                        self.state = State::CloseDottedPath { depth };
                        return Some(Event::Error {
                            span: full_span,
                            kind: ParseErrorKind::InvalidKey,
                        });
                    }

                    let segment_start = full_span.start + offset;
                    let segment_end = segment_start + segment.len() as u32;
                    let segment_span = Span::new(segment_start, segment_end);

                    let is_last = next_offset >= full_span.end - full_span.start;

                    if is_last {
                        // Last segment - emit EntryStart and Key, then read value
                        if let Some(Context::Object { has_entries, .. }) =
                            self.context_stack.last_mut()
                        {
                            *has_entries = true;
                        }
                        self.state = State::EmitDottedPathKey {
                            key_span: segment_span,
                            full_span,
                            depth,
                        };
                        return Some(Event::EntryStart);
                    } else {
                        // Not last - emit EntryStart, Key, ObjectStart, continue
                        if let Some(Context::Object { has_entries, .. }) =
                            self.context_stack.last_mut()
                        {
                            *has_entries = true;
                        }
                        self.state = State::EmitDottedPathKeyThenObject {
                            key_span: segment_span,
                            full_span,
                            next_offset,
                            depth,
                        };
                        return Some(Event::EntryStart);
                    }
                }

                State::EmitDottedPathKey {
                    key_span,
                    full_span,
                    depth,
                } => {
                    let key_text = self.text_at(key_span);
                    // Track path - this is the innermost key
                    self.push_path_segment(key_text);
                    self.state = State::AfterDottedPathKey { full_span, depth };
                    return Some(Event::Key {
                        span: key_span,
                        tag: None,
                        payload: Some(Cow::Borrowed(key_text)),
                        kind: ScalarKind::Bare,
                    });
                }

                State::EmitDottedPathKeyThenObject {
                    key_span,
                    full_span,
                    next_offset,
                    depth,
                } => {
                    let key_text = self.text_at(key_span);
                    // Track path - intermediate key
                    self.push_path_segment(key_text);
                    self.state = State::EmitDottedPathObject {
                        key_span,
                        full_span,
                        next_offset,
                        depth,
                    };
                    return Some(Event::Key {
                        span: key_span,
                        tag: None,
                        payload: Some(Cow::Borrowed(key_text)),
                        kind: ScalarKind::Bare,
                    });
                }

                State::EmitDottedPathObject {
                    key_span,
                    full_span,
                    next_offset,
                    depth,
                } => {
                    self.context_stack.push(Context::Object {
                        implicit: false,
                        separator: SeparatorStyle::Unknown,
                        seen_keys: std::collections::HashMap::new(),
                        is_tag_payload: false,
                        has_entries: false,
                    });
                    self.state = State::EmitDottedPath {
                        full_span,
                        offset: next_offset,
                        depth: depth + 1,
                    };
                    return Some(Event::ObjectStart {
                        span: key_span,
                        separator: Separator::Newline,
                    });
                }

                State::AfterDottedPathKey { full_span, depth } => {
                    // Read the value for the innermost key
                    let t = self.next_token_skip_ws();

                    match t.kind {
                        TokenKind::BareScalar => {
                            // Check for `>` (attribute)
                            self.state = State::AfterDottedPathBareValue {
                                value_span: t.span,
                                depth,
                            };
                            continue;
                        }
                        TokenKind::QuotedScalar => {
                            self.state = State::EmitDottedPathScalarValue {
                                span: t.span,
                                kind: ScalarKind::Quoted,
                                depth,
                            };
                            continue;
                        }
                        TokenKind::Newline | TokenKind::Eof => {
                            // Mark the current path as terminal (unit value)
                            self.mark_path_terminal();
                            self.state = State::CloseDottedPath { depth };
                            return Some(Event::Unit { span: full_span });
                        }
                        TokenKind::LBrace => {
                            // Explicit object as value for dotted path
                            // Store the depth so we can close properly later
                            self.dotted_depth = depth;
                            self.context_stack.push(Context::Object {
                                implicit: false,
                                separator: SeparatorStyle::Unknown,
                                seen_keys: std::collections::HashMap::new(),
                                is_tag_payload: false,
                                has_entries: false,
                            });
                            self.state = State::ExpectEntry;
                            return Some(Event::ObjectStart {
                                span: t.span,
                                separator: Separator::Comma,
                            });
                        }
                        TokenKind::LParen => {
                            // Sequence as value for dotted path
                            self.dotted_depth = depth;
                            self.context_stack.push(Context::Sequence {
                                is_tag_payload: false,
                            });
                            self.state = State::ExpectSeqElem;
                            return Some(Event::SequenceStart { span: t.span });
                        }
                        TokenKind::At => {
                            self.state = State::EmitTagStart { tag_span: t.span };
                            continue;
                        }
                        _ => {
                            self.state = State::CloseDottedPath { depth };
                            return Some(Event::Error {
                                span: t.span,
                                kind: ParseErrorKind::UnexpectedToken,
                            });
                        }
                    }
                }

                State::AfterDottedPathBareValue { value_span, depth } => {
                    // Check for `>` immediately after
                    let t = self.lexer.next_token();
                    if t.kind == TokenKind::Gt && t.span.start == value_span.end {
                        // Attribute chain - save depth for closing later
                        self.dotted_depth = depth;
                        self.context_stack.push(Context::AttrObject);
                        self.state = State::EmitAttrEntryStart {
                            attr_key_span: value_span,
                        };
                        return Some(Event::ObjectStart {
                            span: value_span,
                            separator: Separator::Comma,
                        });
                    } else {
                        // Normal scalar value - mark as terminal
                        self.mark_path_terminal();
                        self.state = State::CloseDottedPath { depth };
                        return Some(Event::Scalar {
                            span: value_span,
                            value: Cow::Borrowed(self.text_at(value_span)),
                            kind: ScalarKind::Bare,
                        });
                    }
                }

                State::EmitDottedPathScalarValue { span, kind, depth } => {
                    let text = self.text_at(span);
                    let value = match kind {
                        ScalarKind::Quoted => self.unescape_quoted(text),
                        _ => Cow::Borrowed(text),
                    };
                    // Mark the current path as terminal (has a scalar value)
                    self.mark_path_terminal();
                    self.state = State::CloseDottedPath { depth };
                    return Some(Event::Scalar { span, value, kind });
                }

                State::CloseDottedPath { depth } => {
                    if depth == 0 {
                        // Pop the innermost key from path
                        self.pop_path_segment();
                        self.state = State::EmitEntryEnd;
                        return Some(Event::EntryEnd);
                    } else {
                        // Close one nested IMPLICIT object from dotted path parsing.
                        // Don't mark as closed - only explicit {} objects get marked closed.
                        // Just pop the path segment and context.
                        self.pop_path_segment();
                        self.context_stack.pop();
                        self.state = State::EmitEntryEndThenCloseDotted { depth: depth - 1 };
                        return Some(Event::ObjectEnd {
                            span: self.eof_span(),
                        });
                    }
                }

                State::EmitEntryEndThenCloseDotted { depth } => {
                    self.state = State::CloseDottedPath { depth };
                    return Some(Event::EntryEnd);
                }

                State::ExpectSeqElemInAttr => {
                    let t = self.next_token_skip_ws_nl();

                    match t.kind {
                        TokenKind::LineComment => {
                            self.state = State::ExpectSeqElemInAttr;
                            return Some(Event::Comment {
                                span: t.span,
                                text: t.text,
                            });
                        }

                        TokenKind::RParen => {
                            // End of sequence - go back to attr checking
                            self.context_stack.pop();
                            self.state = State::AfterAttrEntryEnd;
                            return Some(Event::SequenceEnd { span: t.span });
                        }

                        TokenKind::Eof => {
                            return Some(Event::Error {
                                span: self.eof_span(),
                                kind: ParseErrorKind::UnclosedSequence,
                            });
                        }

                        TokenKind::BareScalar => {
                            self.state = State::ExpectSeqElemInAttr;
                            return Some(Event::Scalar {
                                span: t.span,
                                value: Cow::Borrowed(self.text_at(t.span)),
                                kind: ScalarKind::Bare,
                            });
                        }

                        TokenKind::QuotedScalar => {
                            self.state = State::ExpectSeqElemInAttr;
                            let text = self.text_at(t.span);
                            return Some(Event::Scalar {
                                span: t.span,
                                value: self.unescape_quoted(text),
                                kind: ScalarKind::Quoted,
                            });
                        }

                        TokenKind::LBrace => {
                            self.context_stack.push(Context::Object {
                                implicit: false,
                                separator: SeparatorStyle::Unknown,
                                seen_keys: std::collections::HashMap::new(),
                                is_tag_payload: false,
                                has_entries: false,
                            });
                            self.state = State::ExpectEntry;
                            return Some(Event::ObjectStart {
                                span: t.span,
                                separator: Separator::Comma,
                            });
                        }

                        TokenKind::LParen => {
                            self.context_stack.push(Context::Sequence {
                                is_tag_payload: false,
                            });
                            self.state = State::ExpectSeqElemInAttr;
                            return Some(Event::SequenceStart { span: t.span });
                        }

                        _ => {
                            return Some(Event::Error {
                                span: t.span,
                                kind: ParseErrorKind::UnexpectedToken,
                            });
                        }
                    }
                }

                State::EmitSeqEndInAttr { rparen_span } => {
                    self.context_stack.pop();
                    self.state = State::AfterAttrEntryEnd;
                    return Some(Event::SequenceEnd { span: rparen_span });
                }

                State::ExpectEntryInAttr => {
                    let t = self.next_token_skip_ws_nl();

                    match t.kind {
                        TokenKind::RBrace => {
                            // End of object - go back to attr checking
                            self.context_stack.pop();
                            self.state = State::AfterAttrEntryEnd;
                            return Some(Event::ObjectEnd { span: t.span });
                        }

                        TokenKind::Eof => {
                            return Some(Event::Error {
                                span: self.eof_span(),
                                kind: ParseErrorKind::UnclosedObject,
                            });
                        }

                        TokenKind::BareScalar => {
                            // Normal entry in a nested object - use regular entry flow
                            // The close will be handled by ExpectEntry seeing RBrace
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

                        _ => {
                            return Some(Event::Error {
                                span: t.span,
                                kind: ParseErrorKind::UnexpectedToken,
                            });
                        }
                    }
                }

                State::EmitObjEndInAttr { rbrace_span } => {
                    self.context_stack.pop();
                    self.state = State::AfterAttrEntryEnd;
                    return Some(Event::ObjectEnd { span: rbrace_span });
                }

                State::EmitTagStartInAttr { tag_span } => {
                    // Read tag name (bare scalar immediately after @)
                    let t = self.lexer.next_token();

                    match t.kind {
                        TokenKind::BareScalar if t.span.start == tag_span.end => {
                            let full_text = self.text_at(t.span);
                            let (tag_name, has_trailing_at) =
                                if let Some(at_pos) = full_text.find('@') {
                                    (&full_text[..at_pos], true)
                                } else {
                                    (full_text, false)
                                };

                            let name_end = t.span.start + tag_name.len() as u32;

                            if has_trailing_at {
                                // @tag@ - explicit unit payload
                                self.state = State::EmitTagEndInAttr;
                                // Emit TagStart, then Unit, then TagEnd
                                // Actually need intermediate state for Unit
                                self.state = State::EmitUnitThenTagEndInAttr {
                                    unit_span: Span::new(name_end, name_end + 1),
                                };
                                return Some(Event::TagStart {
                                    span: Span::new(tag_span.start, name_end),
                                    name: tag_name,
                                });
                            }

                            self.state = State::AfterTagStartInAttr {
                                tag_span: Span::new(t.span.start, name_end),
                            };
                            return Some(Event::TagStart {
                                span: Span::new(tag_span.start, name_end),
                                name: tag_name,
                            });
                        }
                        _ => {
                            // @ not followed by identifier - just @ as unit value
                            self.state = State::EmitAttrEntryEnd;
                            return Some(Event::Unit { span: tag_span });
                        }
                    }
                }

                State::AfterTagStartInAttr { tag_span } => {
                    // Check for payload
                    let t = self.lexer.next_token();

                    match t.kind {
                        TokenKind::LBrace if t.span.start == tag_span.end => {
                            self.context_stack.push(Context::Object {
                                implicit: false,
                                separator: SeparatorStyle::Unknown,
                                seen_keys: std::collections::HashMap::new(),
                                is_tag_payload: true,
                                has_entries: false,
                            });
                            self.state = State::ExpectEntry;
                            return Some(Event::ObjectStart {
                                span: t.span,
                                separator: Separator::Comma,
                            });
                        }

                        TokenKind::LParen if t.span.start == tag_span.end => {
                            self.context_stack.push(Context::Sequence {
                                is_tag_payload: true,
                            });
                            self.state = State::ExpectSeqElem;
                            return Some(Event::SequenceStart { span: t.span });
                        }

                        TokenKind::BareScalar if t.span.start == tag_span.end => {
                            self.state = State::EmitTagEndInAttr;
                            return Some(Event::Scalar {
                                span: t.span,
                                value: Cow::Borrowed(self.text_at(t.span)),
                                kind: ScalarKind::Bare,
                            });
                        }

                        TokenKind::QuotedScalar if t.span.start == tag_span.end => {
                            let text = self.text_at(t.span);
                            self.state = State::EmitTagEndInAttr;
                            return Some(Event::Scalar {
                                span: t.span,
                                value: self.unescape_quoted(text),
                                kind: ScalarKind::Quoted,
                            });
                        }

                        // No payload - implicit unit
                        _ => {
                            self.state = State::AfterAttrEntryEnd;
                            return Some(Event::TagEnd);
                        }
                    }
                }

                State::EmitTagEndInAttr => {
                    self.state = State::AfterAttrEntryEnd;
                    return Some(Event::TagEnd);
                }

                State::EmitUnitThenTagEndInAttr { unit_span } => {
                    self.state = State::EmitTagEndInAttr;
                    return Some(Event::Unit { span: unit_span });
                }

                State::EmitAttrObjectEnd => {
                    self.context_stack.pop();
                    self.state = State::EmitEntryEnd;
                    return Some(Event::ObjectEnd {
                        span: self.eof_span(),
                    });
                }

                State::EmitAttrObjectEndThenNewEntry {
                    next_key_span,
                    next_key_kind,
                } => {
                    self.context_stack.pop();
                    self.state = State::EmitEntryEndThenNewEntry {
                        next_key_span,
                        next_key_kind,
                    };
                    return Some(Event::ObjectEnd {
                        span: self.eof_span(),
                    });
                }

                State::EmitEntryEndThenNewEntry {
                    next_key_span,
                    next_key_kind,
                } => {
                    self.state = State::EmitEntryStart {
                        key_span: next_key_span,
                        key_kind: next_key_kind,
                    };
                    return Some(Event::EntryEnd);
                }

                State::EmitAttrObjectEndThenClose { rbrace_span } => {
                    self.context_stack.pop();
                    self.state = State::EmitParentObjectEnd { rbrace_span };
                    return Some(Event::ObjectEnd {
                        span: self.eof_span(),
                    });
                }

                State::EmitParentObjectEnd { rbrace_span } => {
                    self.context_stack.pop();
                    self.state = State::EmitEntryEnd;
                    return Some(Event::ObjectEnd { span: rbrace_span });
                }

                State::ExpectSeqElem => {
                    let t = self.next_token_skip_ws_nl();

                    match t.kind {
                        TokenKind::LineComment => {
                            self.state = State::ExpectSeqElem;
                            return Some(Event::Comment {
                                span: t.span,
                                text: t.text,
                            });
                        }

                        TokenKind::RParen => {
                            // End of sequence
                            let is_tag_payload = match self.context_stack.pop() {
                                Some(Context::Sequence { is_tag_payload }) => is_tag_payload,
                                _ => false,
                            };
                            if is_tag_payload {
                                self.state = State::EmitTagEnd;
                            } else {
                                self.state = State::EmitEntryEnd;
                            }
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
                            self.context_stack.push(Context::Object {
                                implicit: false,
                                separator: SeparatorStyle::Unknown,
                                seen_keys: std::collections::HashMap::new(),
                                is_tag_payload: false,
                                has_entries: false,
                            });
                            self.state = State::ExpectEntry;
                            return Some(Event::ObjectStart {
                                span: t.span,
                                separator: Separator::Comma,
                            });
                        }

                        TokenKind::LParen => {
                            // Nested sequence
                            self.context_stack.push(Context::Sequence {
                                is_tag_payload: false,
                            });
                            self.state = State::ExpectSeqElem;
                            return Some(Event::SequenceStart { span: t.span });
                        }

                        TokenKind::At => {
                            // Tag in sequence - go to tag handling
                            self.state = State::EmitTagStart { tag_span: t.span };
                            continue;
                        }

                        _ => {
                            return Some(Event::Error {
                                span: t.span,
                                kind: ParseErrorKind::UnexpectedToken,
                            });
                        }
                    }
                }

                State::ProcessPeekedSeqElem {
                    peeked_kind,
                    peeked_span,
                } => {
                    // We already consumed this token while checking for TooManyAtoms,
                    // but we're in a sequence where multiple elements are valid.
                    // Process it as a sequence element.
                    match peeked_kind {
                        TokenKind::BareScalar => {
                            self.state = State::ExpectSeqElem;
                            return Some(Event::Scalar {
                                span: peeked_span,
                                value: Cow::Borrowed(self.text_at(peeked_span)),
                                kind: ScalarKind::Bare,
                            });
                        }

                        TokenKind::QuotedScalar => {
                            self.state = State::ExpectSeqElem;
                            let text = self.text_at(peeked_span);
                            return Some(Event::Scalar {
                                span: peeked_span,
                                value: self.unescape_quoted(text),
                                kind: ScalarKind::Quoted,
                            });
                        }

                        TokenKind::At => {
                            // Tag in sequence
                            let next = self.lexer.next_token();
                            if next.kind == TokenKind::BareScalar
                                && next.span.start == peeked_span.end
                            {
                                // @TagName - valid tag
                                let tag_span = Span::new(peeked_span.start, next.span.end);
                                self.state = State::AfterTagStart { tag_span };
                                return Some(Event::TagStart {
                                    span: tag_span,
                                    name: self.text_at(next.span),
                                });
                            } else {
                                // @ alone is unit
                                self.state = State::ExpectSeqElem;
                                return Some(Event::Unit { span: peeked_span });
                            }
                        }

                        TokenKind::LBrace => {
                            // Nested object
                            self.context_stack.push(Context::Object {
                                implicit: false,
                                separator: SeparatorStyle::Unknown,
                                seen_keys: std::collections::HashMap::new(),
                                is_tag_payload: false,
                                has_entries: false,
                            });
                            self.state = State::ExpectEntry;
                            return Some(Event::ObjectStart {
                                span: peeked_span,
                                separator: Separator::Comma,
                            });
                        }

                        TokenKind::LParen => {
                            // Nested sequence
                            self.context_stack.push(Context::Sequence {
                                is_tag_payload: false,
                            });
                            self.state = State::ExpectSeqElem;
                            return Some(Event::SequenceStart { span: peeked_span });
                        }

                        TokenKind::RParen => {
                            // End of sequence
                            let is_tag_payload = match self.context_stack.pop() {
                                Some(Context::Sequence { is_tag_payload }) => is_tag_payload,
                                _ => false,
                            };
                            if is_tag_payload {
                                self.state = State::EmitTagEnd;
                            } else {
                                self.state = State::EmitEntryEnd;
                            }
                            return Some(Event::SequenceEnd { span: peeked_span });
                        }

                        _ => {
                            self.state = State::ExpectSeqElem;
                            return Some(Event::Error {
                                span: peeked_span,
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
