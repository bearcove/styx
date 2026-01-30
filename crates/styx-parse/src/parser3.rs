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

    /// Emit unit key (@ with no name), then read value.
    EmitUnitKeyValue { at_span: Span },

    /// Emit Key event, then read value token.
    EmitKey {
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

/// Context for nested structures.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Context {
    /// Inside an object. `implicit` = true for the root object.
    Object { implicit: bool },
    /// Inside a sequence.
    Sequence,
    /// Inside an attribute object (the `{...}` that holds attr key-value pairs).
    AttrObject,
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
                        TokenKind::LineComment => {
                            // Emit comment, stay in ExpectEntry
                            self.state = State::ExpectEntry;
                            return Some(Event::Comment {
                                span: t.span,
                                text: t.text,
                            });
                        }

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
                            // Check for dotted path (e.g., "a.b.c")
                            let text = self.text_at(t.span);
                            if text.contains('.') {
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
                                Some(Context::Object { implicit: true }) => {
                                    // Replace implicit root with explicit root
                                    self.context_stack.pop();
                                    self.context_stack.push(Context::Object { implicit: false });
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
                            self.state = State::EmitUnitKeyValue { at_span: t.span };
                            return Some(Event::EntryStart);
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
                    self.state = State::EmitKey { key_span, key_kind };
                    return Some(Event::EntryStart);
                }

                State::EmitUnitKeyValue { at_span } => {
                    // Emit Key with no payload, then read value
                    // First read next token - check if it's immediately after @ (tag)
                    let t = self.lexer.next_token();

                    // If bare scalar immediately after @, it's a tag name
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
                        // Valid tag - process it
                        self.state = State::AfterTagStart { tag_span: t.span };
                        return Some(Event::TagStart {
                            span: Span::new(at_span.start, t.span.end),
                            name: tag_name,
                        });
                    }

                    // Skip whitespace if needed
                    let t = if t.kind == TokenKind::Whitespace {
                        self.next_token_skip_ws()
                    } else {
                        t
                    };

                    match t.kind {
                        TokenKind::BareScalar => {
                            self.state = State::AfterBareScalarValue { value_span: t.span };
                            return Some(Event::Key {
                                span: at_span,
                                tag: None,
                                payload: None,
                                kind: ScalarKind::Bare,
                            });
                        }
                        TokenKind::QuotedScalar => {
                            self.state = State::EmitScalarValue {
                                span: t.span,
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
                            self.state = State::EmitObjectStartValue { span: t.span };
                            return Some(Event::Key {
                                span: at_span,
                                tag: None,
                                payload: None,
                                kind: ScalarKind::Bare,
                            });
                        }
                        TokenKind::LParen => {
                            self.state = State::EmitSequenceStartValue { span: t.span };
                            return Some(Event::Key {
                                span: at_span,
                                tag: None,
                                payload: None,
                                kind: ScalarKind::Bare,
                            });
                        }
                        TokenKind::At => {
                            // Tag value
                            self.state = State::EmitTagStart { tag_span: t.span };
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
                                span: t.span,
                                kind: ParseErrorKind::UnexpectedToken,
                            });
                        }
                    }
                }

                State::EmitKey { key_span, key_kind } => {
                    // Get the key text
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

                        TokenKind::Newline | TokenKind::Eof | TokenKind::Comma => {
                            // Normal value - emit scalar now
                            self.state = State::EmitEntryEnd;
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
                                TokenKind::Newline
                                | TokenKind::Eof
                                | TokenKind::Comma
                                | TokenKind::RBrace
                                | TokenKind::RParen
                                | TokenKind::LineComment => {
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
                                    Some(Context::Object { implicit: false }) => {
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
                            // Close object
                            self.context_stack.pop();
                            self.state = State::EmitEntryEnd;
                            return Some(Event::ObjectEnd { span: t.span });
                        }
                        TokenKind::RParen => {
                            // Close sequence
                            self.context_stack.pop();
                            self.state = State::EmitEntryEnd;
                            return Some(Event::SequenceEnd { span: t.span });
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
                                    self.context_stack.pop();
                                    self.state = State::EmitEntryEnd;
                                    return Some(Event::ObjectEnd { span: next.span });
                                }
                                TokenKind::RParen => {
                                    self.context_stack.pop();
                                    self.state = State::EmitEntryEnd;
                                    return Some(Event::SequenceEnd { span: next.span });
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
                    // After EntryEnd, check what context we're in
                    match self.context_stack.last() {
                        Some(Context::Object { .. }) => {
                            self.state = State::ExpectEntry;
                        }
                        Some(Context::Sequence) => {
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

                State::EmitEntryEndThenObjectEnd { rbrace_span } => {
                    self.state = State::EmitObjectEndAfterEntry { rbrace_span };
                    return Some(Event::EntryEnd);
                }

                State::EmitObjectEndAfterEntry { rbrace_span } => {
                    self.context_stack.pop();
                    self.state = State::EmitEntryEnd;
                    return Some(Event::ObjectEnd { span: rbrace_span });
                }

                State::EmitEntryEndThenSeqEnd { rparen_span } => {
                    self.state = State::EmitSeqEndAfterEntry { rparen_span };
                    return Some(Event::EntryEnd);
                }

                State::EmitSeqEndAfterEntry { rparen_span } => {
                    self.context_stack.pop();
                    self.state = State::EmitEntryEnd;
                    return Some(Event::SequenceEnd { span: rparen_span });
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
                            self.context_stack.push(Context::Object { implicit: false });
                            self.state = State::ExpectEntry;
                            return Some(Event::ObjectStart {
                                span: t.span,
                                separator: Separator::Comma,
                            });
                        }

                        TokenKind::LParen if t.span.start == tag_span.end => {
                            // @tag(...) - sequence payload
                            self.context_stack.push(Context::Sequence);
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

                        // Whitespace or other - tag has no payload (implicit unit)
                        _ => {
                            // Go back to appropriate context
                            match self.context_stack.last() {
                                Some(Context::Object { .. }) => {
                                    self.state = State::EmitEntryEnd;
                                }
                                Some(Context::Sequence) => {
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
                        Some(Context::Sequence) => {
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
                    self.context_stack.pop();
                    self.state = State::EmitEntryEnd;
                    return Some(Event::SequenceEnd { span: rparen_span });
                }

                State::EmitTagEndThenObjEnd { rbrace_span } => {
                    // Close object after tag
                    self.context_stack.pop();
                    self.state = State::EmitEntryEnd;
                    return Some(Event::ObjectEnd { span: rbrace_span });
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
                            self.context_stack.push(Context::Sequence);
                            self.state = State::ExpectSeqElem;
                            return Some(Event::SequenceStart { span: t.span });
                        }
                        TokenKind::LBrace => {
                            // Object value for attribute - use regular obj handling
                            self.context_stack.push(Context::Object { implicit: false });
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
                        self.state = State::EmitDottedPathKey {
                            key_span: segment_span,
                            full_span,
                            depth,
                        };
                        return Some(Event::EntryStart);
                    } else {
                        // Not last - emit EntryStart, Key, ObjectStart, continue
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
                    self.context_stack.push(Context::Object { implicit: false });
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
                            self.state = State::CloseDottedPath { depth };
                            return Some(Event::Unit { span: full_span });
                        }
                        TokenKind::LBrace => {
                            self.context_stack.push(Context::Object { implicit: false });
                            self.state = State::ExpectEntry;
                            return Some(Event::ObjectStart {
                                span: t.span,
                                separator: Separator::Comma,
                            });
                        }
                        TokenKind::LParen => {
                            self.context_stack.push(Context::Sequence);
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
                        // Attribute chain
                        self.context_stack.push(Context::AttrObject);
                        self.state = State::EmitAttrEntryStart {
                            attr_key_span: value_span,
                        };
                        return Some(Event::ObjectStart {
                            span: value_span,
                            separator: Separator::Comma,
                        });
                    } else {
                        // Normal scalar value
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
                    self.state = State::CloseDottedPath { depth };
                    return Some(Event::Scalar { span, value, kind });
                }

                State::CloseDottedPath { depth } => {
                    if depth == 0 {
                        self.state = State::EmitEntryEnd;
                        return Some(Event::EntryEnd);
                    } else {
                        // Close one nested object
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
                            self.context_stack.push(Context::Object { implicit: false });
                            self.state = State::ExpectEntry;
                            return Some(Event::ObjectStart {
                                span: t.span,
                                separator: Separator::Comma,
                            });
                        }

                        TokenKind::LParen => {
                            self.context_stack.push(Context::Sequence);
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
                            self.context_stack.push(Context::Object { implicit: false });
                            self.state = State::ExpectEntry;
                            return Some(Event::ObjectStart {
                                span: t.span,
                                separator: Separator::Comma,
                            });
                        }

                        TokenKind::LParen if t.span.start == tag_span.end => {
                            self.context_stack.push(Context::Sequence);
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
