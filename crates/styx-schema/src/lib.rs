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
    use facet_testhelpers::test;

    /// Test deserializing a simple tagged enum variant
    #[test]
    fn test_seq_variant() {
        // @seq(...) should deserialize to Schema::Seq
        let source = "@seq(@seq())";
        tracing::trace!(?source, "parsing");
        let result: Result<Schema, _> = facet_styx::from_str(source);
        tracing::trace!(?result, "parsed");
        let schema = result.unwrap();
        assert!(matches!(schema, Schema::Seq(_)));
    }

    /// Test that unknown tags fall back to the Type variant
    #[test]
    fn test_type_ref_fallback() {
        // @string should fall back to Schema::Type
        let source = "@string";
        tracing::trace!(?source, "parsing");
        let result: Result<Schema, _> = facet_styx::from_str(source);
        tracing::trace!(?result, "parsed");
        let schema = result.unwrap();
        assert!(matches!(schema, Schema::Type(_)));
        if let Schema::Type(name) = schema {
            assert_eq!(name, "string");
        }
    }

    /// Test deserializing an enum schema
    #[test]
    fn test_enum_schema() {
        // An enum with two variants: one with type ref, one with object payload
        let source = "@enum{ ok @unit error @object{message @string} }";
        tracing::trace!(?source, "parsing");
        let result: Result<Schema, _> = facet_styx::from_str(source);
        tracing::trace!(?result, "parsed");
        let schema = result.expect("Failed to deserialize enum schema");
        if let Schema::Enum(ref e) = schema {
            for (k, v) in e.0.iter() {
                tracing::trace!(key = ?k, value = ?v, "enum variant");
            }
        }
        assert!(matches!(schema, Schema::Enum(_)));
        // Verify the inner types are captured correctly
        if let Schema::Enum(e) = schema {
            let ok_schema = e.0.get("ok").expect("should have 'ok' variant");
            assert!(
                matches!(ok_schema, Schema::Type(s) if s == "unit"),
                "ok should be Type(\"unit\"), got {:?}",
                ok_schema
            );
        }
    }

    /// Test deserializing the full meta-schema
    #[test]
    fn test_deserialize_meta_schema() {
        tracing::trace!(source = META_SCHEMA_SOURCE, "parsing meta-schema");
        let result: Result<SchemaFile, _> = facet_styx::from_str(META_SCHEMA_SOURCE);
        tracing::trace!(?result, "parsed meta-schema");
        let schema_file = result.expect("Failed to deserialize meta-schema");

        // Verify metadata
        assert_eq!(schema_file.meta.id, "https://styx-lang.org/schemas/schema");
        assert_eq!(schema_file.meta.version, "2026-01-11");
        assert!(schema_file.meta.description.is_some());

        // Verify schema definitions exist
        assert!(
            schema_file.schema.contains_key("@"),
            "Should have root definition"
        );
        assert!(
            schema_file.schema.contains_key("Meta"),
            "Should have Meta definition"
        );
        assert!(
            schema_file.schema.contains_key("Schema"),
            "Should have Schema definition"
        );
    }
}
