# Styx Crates

## Dependency Graph

```
              ┌─────────────────┐
              │ styx-tokenizer  │
              └────────┬────────┘
                       │
             ┌─────────┴─────────┐
             │                   │
             ▼                   ▼
       ┌───────────┐       ┌───────────┐
       │  styx-cst │       │styx-parse │
       └─────┬─────┘       └─────┬─────┘
             │                   │
             ▼                   ▼
       ┌───────────┐       ┌───────────┐
       │styx-format│       │ styx-tree │
       └─────┬─────┘       └───────────┘
             │
             ▼
       ┌───────────┐
       │  styx-lsp │ (also uses styx-cst directly)
       └───────────┘
```

## Concerns

### styx-tokenizer

Produces tokens from source. Tokens are spans with kinds like `BareScalar`, `QuotedScalar`, `OpenBrace`, `Comma`, `Newline`, etc. Completely lossless - every byte is accounted for.

```bash
$ echo 'server { host localhost, port 8080 }' > test.styx
$ styx tokens test.styx
Token { kind: BareScalar, span: Span { start: 0, end: 6 }, text: "server" }
Token { kind: Whitespace, span: Span { start: 6, end: 7 }, text: " " }
Token { kind: LBrace, span: Span { start: 7, end: 8 }, text: "{" }
Token { kind: Whitespace, span: Span { start: 8, end: 9 }, text: " " }
Token { kind: BareScalar, span: Span { start: 9, end: 13 }, text: "host" }
...
```

### styx-cst

Lossless concrete syntax tree built on rowan. Preserves everything: whitespace, comments, exact scalar representation, separator style. Has its own parser that consumes tokens directly.

Used for exactly two things:
1. **Formatting** - reformatting source while preserving style choices
2. **IDE/LSP** - go to definition, hover, completions, diagnostics

The `Separator` enum here includes `Mixed` because CST preserves what was actually written.

### styx-parse

Produces a stream of semantic events: `DocumentStart`, `ObjectStart`, `EntryStart`, `Scalar`, etc. This is for building semantic trees, not for preserving syntax.

Does NOT care about:
- Whitespace
- Comments (except doc comments which are semantic)
- Whether commas or newlines were used as separators

```bash
$ styx lexemes test.styx
Scalar { span: Span { start: 0, end: 6 }, value: "server", kind: Bare }
ObjectStart { span: Span { start: 7, end: 8 } }
Scalar { span: Span { start: 9, end: 13 }, value: "host", kind: Bare }
Scalar { span: Span { start: 14, end: 23 }, value: "localhost", kind: Bare }
Comma { span: Span { start: 23, end: 24 } }
...

$ styx events test.styx
DocumentStart
EntryStart
Key { span: Span { start: 0, end: 6 }, tag: None, payload: Some("server"), kind: Bare }
ObjectStart { span: Span { start: 7, end: 36 }, separator: Comma }
EntryStart
Key { span: Span { start: 9, end: 13 }, tag: None, payload: Some("host"), kind: Bare }
Scalar { span: Span { start: 14, end: 23 }, value: "localhost", kind: Bare }
EntryEnd
...
```

### styx-tree

Semantic document tree built from styx-parse events. An `Object` is just a map of entries, a `Sequence` is just a list of values. No syntactic baggage.

Use styx-tree when you need to:
- Read configuration values
- Deserialize into Rust types
- Programmatically build documents

```bash
$ styx tree test.styx
Object {
  key: Scalar("server")
  value:
    Object {
      key: Scalar("host")
      value: Scalar("localhost")
      key: Scalar("port")
      value: Scalar("8080")
    }
}
```

### styx-format

Serializes values to styx text. Uses heuristics to decide formatting (inline vs multiline, etc.).

For reformatting existing source, use styx-cst instead - it preserves the original style.
