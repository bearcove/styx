use styx_parse::Lexer;

fn main() {
    let source = r#"server {
    host localhost
    port 8080
}
tags (web prod @env"staging")
config name>app @flag
a.b.c value
@tag{x 1}
"#;

    for lex in Lexer::new(source) {
        println!("{:?}", lex);
    }
}
