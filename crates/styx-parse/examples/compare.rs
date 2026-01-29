fn main() {
    let input = r#"{label ": BIGINT" line 4}"#;

    println!("=== Parser (original) ===");
    let events1 = styx_parse::Parser::new(input).parse_to_vec();
    for e in &events1 {
        println!("{:?}", e);
    }

    println!("\n=== Parser2 (new) ===");
    let events2 = styx_parse::Parser2::new(input).parse_to_vec();
    for e in &events2 {
        println!("{:?}", e);
    }
}
