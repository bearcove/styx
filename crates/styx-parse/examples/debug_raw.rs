use styx_parse::{Lexer, Tokenizer};

fn main() {
    let input = r##"@rawr#"content"#"##;
    println!("Input: {:?}", input);

    println!("\n=== Tokens ===");
    for tok in Tokenizer::new(input) {
        println!("{:?}", tok);
    }

    println!("\n=== Lexemes ===");
    for lex in Lexer::new(input) {
        println!("{:?}", lex);
    }
}
