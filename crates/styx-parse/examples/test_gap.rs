use styx_parse::Lexer;

fn main() {
    println!("=== @tag{{}} (no space) ===");
    for lex in Lexer::new("@tag{}") {
        println!("{:?}", lex);
    }

    println!("\n=== @tag {{}} (with space) ===");
    for lex in Lexer::new("@tag {}") {
        println!("{:?}", lex);
    }
}
