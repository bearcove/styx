//! STYX Schema crate - eat your own dog food.
//!
//! This crate defines the schema types and bundles the meta-schema,
//! deserializing it with facet-styx to validate the implementation.

pub mod error;
pub mod generate;
pub mod meta;
pub mod types;
pub mod validate;

pub use error::{ValidationError, ValidationErrorKind, ValidationResult, ValidationWarning};
pub use generate::{StyxSchemaGenerator, to_styx_schema};
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
        // v @seq(@string) should deserialize to Schema::Seq
        let source = "v @seq(@string)";
        tracing::trace!(?source, "parsing");
        let result: Result<Doc, _> = facet_styx::from_str(source);
        tracing::trace!(?result, "parsed");
        let doc = result.unwrap();
        assert!(matches!(doc.v, Schema::Seq(_)));
    }

    /// Test that unknown tags fall back to the Type variant
    #[test]
    fn test_type_ref_fallback() {
        // v @MyCustomType should fall back to Schema::Type (unknown variant)
        let source = "v @MyCustomType";
        tracing::trace!(?source, "parsing");
        let result: Result<Doc, _> = facet_styx::from_str(source);
        tracing::trace!(?result, "parsed");
        let doc = result.unwrap();
        assert!(matches!(doc.v, Schema::Type { .. }));
        if let Schema::Type { name } = doc.v {
            assert_eq!(name, Some("MyCustomType".into()));
        }
    }

    /// Test deserializing an enum schema
    #[test]
    fn test_enum_schema() {
        // An enum with two variants: one unit type, one with object payload
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
            // Now @unit is a built-in type, not a Type fallback
            assert!(
                matches!(ok_schema, Schema::Unit),
                "ok should be Schema::Unit, got {:?}",
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
        assert_eq!(schema_file.meta.version.as_deref(), Some("2026-01-11"));
        assert!(schema_file.meta.description.is_some());

        // Verify schema definitions exist
        assert!(
            schema_file.schema.contains_key(&None),
            "Should have root definition"
        );
        assert!(
            schema_file.schema.contains_key(&Some("Meta".to_string())),
            "Should have Meta definition"
        );
        assert!(
            schema_file.schema.contains_key(&Some("Schema".to_string())),
            "Should have Schema definition"
        );
    }
}
