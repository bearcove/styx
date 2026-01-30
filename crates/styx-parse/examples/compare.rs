fn main() {
    let input = r#"{label ": BIGINT" line 4}"#;

    println!("=== Parser2 ===");
    let events = styx_parse::Parser2::new(input).parse_to_vec();
    for e in &events {
        println!("{:?}", e);
    }
}
