use styx_parse::{Lexer, Tokenizer};

fn main() {
    println!("=== Tokens for @tag@ ===");
    for tok in Tokenizer::new("@tag@") {
        println!("{:?}", tok);
    }

    println!("\n=== Lexemes for @tag@ ===");
    for lex in Lexer::new("@tag@") {
        println!("{:?}", lex);
    }
}
