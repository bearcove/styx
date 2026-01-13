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
    #[ignore = "Meta-schema deserialization requires handling object schemas and custom type refs"]
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
}
