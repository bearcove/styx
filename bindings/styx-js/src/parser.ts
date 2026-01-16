import { Lexer, Token, TokenType } from "./lexer.js";
import {
  Value,
  Scalar,
  Sequence,
  StyxObject,
  Entry,
  Document,
  Span,
  ParseError,
  Separator,
  ScalarKind,
} from "./types.js";

export class Parser {
  private lexer: Lexer;
  private current: Token;
  private peeked: Token | null = null;

  constructor(source: string) {
    this.lexer = new Lexer(source);
    this.current = this.lexer.nextToken();
  }

  private advance(): Token {
    const prev = this.current;
    if (this.peeked) {
      this.current = this.peeked;
      this.peeked = null;
    } else {
      this.current = this.lexer.nextToken();
    }
    return prev;
  }

  private peek(): Token {
    if (!this.peeked) {
      this.peeked = this.lexer.nextToken();
    }
    return this.peeked;
  }

  private check(...types: TokenType[]): boolean {
    return types.includes(this.current.type);
  }

  private expect(type: TokenType): Token {
    if (this.current.type !== type) {
      throw new ParseError(`expected ${type}, got ${this.current.type}`, this.current.span);
    }
    return this.advance();
  }

  parse(): Document {
    const entries: Entry[] = [];
    const start = this.current.span.start;

    while (!this.check("eof")) {
      const entry = this.parseEntry();
      if (entry) {
        entries.push(entry);
      }
    }

    return {
      entries,
      span: { start, end: this.current.span.end },
    };
  }

  private parseEntry(): Entry | null {
    // Skip any leading commas (for trailing comma support)
    while (this.check("comma")) {
      this.advance();
    }

    if (this.check("eof", "rbrace")) {
      return null;
    }

    const key = this.parseValue();
    const value = this.parseValue();

    return { key, value };
  }

  private parseValue(): Value {
    const start = this.current.span.start;

    // Unit value or tag
    if (this.check("at")) {
      const atToken = this.advance();
      return {
        span: { start: atToken.span.start, end: atToken.span.end },
      };
    }

    // Tag
    if (this.check("tag")) {
      const tagToken = this.advance();
      const tag = {
        name: tagToken.text,
        span: tagToken.span,
      };

      // Check for payload - only if immediately adjacent (no whitespace)
      // When there's a payload, span is the payload's span
      if (!this.current.hadWhitespaceBefore) {
        if (this.check("lbrace")) {
          const obj = this.parseObject();
          return { tag, payload: obj, span: obj.span };
        }
        if (this.check("lparen")) {
          const seq = this.parseSequence();
          return { tag, payload: seq, span: seq.span };
        }
        if (this.check("quoted")) {
          const scalar = this.parseScalar();
          return { tag, payload: scalar, span: scalar.span };
        }
        if (this.check("raw")) {
          const scalar = this.parseScalar();
          return { tag, payload: scalar, span: scalar.span };
        }
        if (this.check("heredoc")) {
          const scalar = this.parseScalar();
          return { tag, payload: scalar, span: scalar.span };
        }
        if (this.check("at")) {
          // Explicit unit payload @tag@
          this.advance();
          return { tag, span: { start, end: this.current.span.start } };
        }
      }

      // Tag with no payload
      return { tag, span: { start, end: tagToken.span.end } };
    }

    // Object
    if (this.check("lbrace")) {
      const obj = this.parseObject();
      return { payload: obj, span: obj.span };
    }

    // Sequence
    if (this.check("lparen")) {
      const seq = this.parseSequence();
      return { payload: seq, span: seq.span };
    }

    // Scalar
    const scalar = this.parseScalar();
    return { payload: scalar, span: scalar.span };
  }

  private parseScalar(): Scalar {
    const token = this.current;
    let kind: ScalarKind;

    switch (token.type) {
      case "scalar":
        kind = "bare";
        break;
      case "quoted":
        kind = "quoted";
        break;
      case "raw":
        kind = "raw";
        break;
      case "heredoc":
        kind = "heredoc";
        break;
      default:
        throw new ParseError(`expected scalar, got ${token.type}`, token.span);
    }

    this.advance();
    return {
      type: "scalar",
      text: token.text,
      kind,
      span: token.span,
    };
  }

  private parseObject(): StyxObject {
    const openBrace = this.expect("lbrace");
    const start = openBrace.span.start;
    const entries: Entry[] = [];
    let separator: Separator | null = null; // null = not yet determined

    // Check if there's a newline after the opening brace
    if (this.current.hadNewlineBefore) {
      separator = "newline";
    }

    while (!this.check("rbrace", "eof")) {
      const entry = this.parseEntry();
      if (entry) {
        entries.push(entry);
      }

      // Check separator
      if (this.check("comma")) {
        if (separator === "newline") {
          throw new ParseError(
            "mixed separators (use either commas or newlines)",
            this.current.span,
          );
        }
        separator = "comma";
        this.advance();
      } else if (!this.check("rbrace", "eof")) {
        if (separator === "comma") {
          throw new ParseError(
            "mixed separators (use either commas or newlines)",
            this.current.span,
          );
        }
        separator = "newline";
      }
    }

    // Default to comma if no separator was found (empty or single-entry objects)
    if (separator === null) {
      separator = "comma";
    }

    if (this.check("eof")) {
      throw new ParseError("unclosed object (missing `}`)", openBrace.span);
    }

    const end = this.expect("rbrace").span.end;
    return {
      type: "object",
      entries,
      separator,
      span: { start, end },
    };
  }

  private parseSequence(): Sequence {
    const openParen = this.expect("lparen");
    const start = openParen.span.start;
    const items: Value[] = [];

    while (!this.check("rparen", "eof")) {
      items.push(this.parseValue());
    }

    if (this.check("eof")) {
      throw new ParseError("unclosed sequence (missing `)`)", openParen.span);
    }

    const end = this.expect("rparen").span.end;
    return {
      type: "sequence",
      items,
      span: { start, end },
    };
  }
}

export function parse(source: string): Document {
  const parser = new Parser(source);
  return parser.parse();
}
