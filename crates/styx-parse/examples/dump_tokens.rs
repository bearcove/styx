use styx_parse::Tokenizer;

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

    for tok in Tokenizer::new(source) {
        println!(
            "{:15} {:25} @ {:?}",
            format!("{:?}", tok.kind),
            format!("{:?}", tok.text),
            tok.span
        );
    }
}
