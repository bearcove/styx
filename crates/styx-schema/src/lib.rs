//! STYX Schema crate - eat your own dog food.
//!
//! This crate defines the schema types and bundles the meta-schema,
//! deserializing it with facet-styx to validate the implementation.

pub mod meta;
pub mod types;

pub use meta::META_SCHEMA_SOURCE;
pub use types::*;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_meta_schema() {
        // First, let's just see if we can parse the meta-schema as STYX
        let result = styx_tree::parse(META_SCHEMA_SOURCE);
        match &result {
            Ok(value) => {
                eprintln!("Parse succeeded!");
                eprintln!("Value: {value:#?}");
            }
            Err(e) => {
                eprintln!("Parse error: {e:?}");
                panic!("Meta-schema should parse without errors");
            }
        }
    }

    #[test]
    fn test_deserialize_meta_only() {
        // Try deserializing just the Meta struct first
        let source = r#"
            id https://example.com/test
            version 2026-01-11
            description "Test schema"
        "#;
        let result: Result<Meta, _> = facet_styx::from_str(source);
        match result {
            Ok(meta) => {
                eprintln!(
                    "Meta deserialized: id={}, version={}",
                    meta.id, meta.version
                );
            }
            Err(e) => {
                eprintln!("Meta deserialization error: {e}");
                panic!("Failed to deserialize Meta: {e}");
            }
        }
    }

    #[test]
    fn test_deserialize_simple_schema_file() {
        // Try a minimal schema file
        let source = r#"
            meta {
                id https://example.com/test
                version 2026-01-11
            }
            schema {
            }
        "#;
        let result: Result<SchemaFile, _> = facet_styx::from_str(source);
        match result {
            Ok(schema) => {
                eprintln!("SchemaFile deserialized!");
                eprintln!(
                    "Meta: id={}, version={}",
                    schema.meta.id, schema.meta.version
                );
                eprintln!("Schema entries: {}", schema.schema.len());
            }
            Err(e) => {
                eprintln!("SchemaFile deserialization error: {e}");
                panic!("Failed to deserialize SchemaFile: {e}");
            }
        }
    }

    #[test]
    fn test_deserialize_meta_schema() {
        // Try to deserialize the meta-schema into our SchemaFile type
        // This currently fails because:
        // 1. Object schemas like `{ field @type }` need special handling
        // 2. Custom type references like `@Meta`, `@Schema` are not in our enum
        // 3. The @ (unit) key needs special handling for map keys
        let result: Result<SchemaFile, _> = facet_styx::from_str(META_SCHEMA_SOURCE);
        match result {
            Ok(schema) => {
                eprintln!("Deserialized successfully!");
                eprintln!(
                    "Meta: id={}, version={}",
                    schema.meta.id, schema.meta.version
                );
                eprintln!("Schema entries: {:?}", schema.schema.len());
            }
            Err(e) => {
                eprintln!("Deserialization error: {e}");
                panic!("Failed to deserialize meta-schema: {e}");
            }
        }
    }

    #[test]
    fn test_deserialize_schema_with_type_ref() {
        // Test with a simple type reference in schema
        // Schema enum expects @TypeRef as a tagged value
        let source = r#"
            meta {
                id https://example.com/test
                version 2026-01-11
            }
            schema {
                Foo @string
            }
        "#;
        let result: Result<SchemaFile, _> = facet_styx::from_str(source);
        match result {
            Ok(schema) => {
                eprintln!("SchemaFile deserialized!");
                eprintln!("Schema entries: {:?}", schema.schema);
            }
            Err(e) => {
                eprintln!("Deserialization error: {e}");
                panic!("Failed: {e}");
            }
        }
    }

    #[test]
    fn test_deserialize_hashmap_schema() {
        // Test just HashMap<String, Schema>
        use std::collections::HashMap;
        let source = r#"
            Foo @string
        "#;
        let result: Result<HashMap<String, Schema>, _> = facet_styx::from_str(source);
        match result {
            Ok(map) => {
                eprintln!("HashMap deserialized: {:?}", map);
            }
            Err(e) => {
                eprintln!("Deserialization error: {e}");
                panic!("Failed: {e}");
            }
        }
    }

    #[test]
    fn test_debug_events() {
        use facet_format::FormatParser;
        // Test with a subset of meta-schema that should work
        let source = r#"
meta {
  id test
  version 1.0
}

schema {
  Meta {
    id @string
  }
}
"#;
        let mut parser = facet_styx::StyxParser::new(source);
        eprintln!("Parsing:\n{}", source);
        eprintln!("---");
        let mut count = 0;
        loop {
            match parser.next_event() {
                Ok(Some(event)) => {
                    eprintln!("Event {}: {:?}", count, event);
                    count += 1;
                }
                Ok(None) => {
                    eprintln!("Done after {} events", count);
                    break;
                }
                Err(e) => {
                    eprintln!("Error after {} events: {:?}", count, e);
                    panic!("Parser error: {e}");
                }
            }
        }
    }

    #[test]
    fn test_nested_object_schema() {
        // Test struct wrapper for tagged values
        #[derive(facet::Facet, Debug)]
        struct OptionalWrapper {
            optional: Vec<String>,
        }

        #[derive(facet::Facet, Debug)]
        struct Test {
            value: OptionalWrapper,
        }

        // First test with explicit braces - this should work
        let source = "value { optional (hello) }";
        eprintln!("Testing explicit braces: {}", source);
        let result: Result<Test, _> = facet_styx::from_str(source);
        match result {
            Ok(test) => {
                eprintln!("Test deserialized: {:?}", test);
            }
            Err(e) => {
                eprintln!("Test error: {e}");
                panic!("Failed: {e}");
            }
        }

        // Now test with tag syntax - should produce same events
        let source2 = "value @optional(hello)";
        eprintln!("\nTesting tag syntax: {}", source2);
        let result2: Result<Test, _> = facet_styx::from_str(source2);
        match result2 {
            Ok(test) => {
                eprintln!("Test deserialized: {:?}", test);
            }
            Err(e) => {
                eprintln!("Test error: {e}");
                panic!("Failed: {e}");
            }
        }
    }

    #[test]
    fn test_debug_at_object() {
        use facet_format::FormatParser;
        // Debug the @ { ... } construct
        let source = r#"
schema {
  @ {
    meta @Meta
  }
}
"#;
        let mut parser = facet_styx::StyxParser::new(source);
        eprintln!("Parsing:\n{}", source);
        eprintln!("---");
        let mut count = 0;
        loop {
            match parser.next_event() {
                Ok(Some(event)) => {
                    eprintln!("Event {}: {:?}", count, event);
                    count += 1;
                }
                Ok(None) => {
                    eprintln!("Done after {} events", count);
                    break;
                }
                Err(e) => {
                    eprintln!("Error after {} events: {:?}", count, e);
                    panic!("Parser error: {e}");
                }
            }
        }
    }
}
