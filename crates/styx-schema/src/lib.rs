//! STYX Schema crate - eat your own dog food.
//!
//! This crate defines the schema types and bundles the meta-schema,
//! deserializing it with facet-styx to validate the implementation.

pub mod error;
pub mod meta;
pub mod types;
pub mod validate;

pub use error::{ValidationError, ValidationErrorKind, ValidationResult, ValidationWarning};
pub use meta::META_SCHEMA_SOURCE;
pub use types::*;
pub use validate::{Validator, validate, validate_as};

#[cfg(test)]
mod tests {
    use super::*;
    use facet::Facet;
    use facet_testhelpers::test;

    /// Wrapper struct for testing Schema deserialization.
    /// Styx documents are implicitly objects, so we need a field to hold the value.
    #[derive(Facet, Debug)]
    struct Doc {
        v: Schema,
    }

    /// Test deserializing a simple tagged enum variant
    #[test]
    fn test_seq_variant() {
        // v @seq(...) should deserialize to Schema::Seq
        let source = "v @seq(@seq())";
        tracing::trace!(?source, "parsing");
        let result: Result<Doc, _> = facet_styx::from_str(source);
        tracing::trace!(?result, "parsed");
        let doc = result.unwrap();
        assert!(matches!(doc.v, Schema::Seq(_)));
    }

    /// Test that unknown tags fall back to the Type variant
    #[test]
    fn test_type_ref_fallback() {
        // v @string should fall back to Schema::Type
        let source = "v @string";
        tracing::trace!(?source, "parsing");
        let result: Result<Doc, _> = facet_styx::from_str(source);
        tracing::trace!(?result, "parsed");
        let doc = result.unwrap();
        assert!(matches!(doc.v, Schema::Type { .. }));
        if let Schema::Type { name } = doc.v {
            assert_eq!(name, Some("string".into()));
        }
    }

    /// Test deserializing an enum schema
    #[test]
    fn test_enum_schema() {
        // An enum with two variants: one with type ref, one with object payload
        let source = "v @enum{ ok @unit error @object{message @string} }";
        tracing::trace!(?source, "parsing");
        let result: Result<Doc, _> = facet_styx::from_str(source);
        tracing::trace!(?result, "parsed");
        let doc = result.expect("Failed to deserialize enum schema");
        if let Schema::Enum(ref e) = doc.v {
            for (k, v) in e.0.iter() {
                tracing::trace!(key = ?k, value = ?v, "enum variant");
            }
        }
        assert!(matches!(doc.v, Schema::Enum(_)));
        // Verify the inner types are captured correctly
        if let Schema::Enum(e) = doc.v {
            let ok_schema = e.0.get("ok").expect("should have 'ok' variant");
            assert!(
                matches!(ok_schema, Schema::Type { name } if *name == Some("unit".into())),
                "ok should be Type {{ name: Some(\"unit\") }}, got {:?}",
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
