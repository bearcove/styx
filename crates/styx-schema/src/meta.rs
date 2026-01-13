//! Bundled meta-schema.
//!
//! The meta-schema is the schema that describes STYX schema files themselves.
//! It's bundled as a static string and deserialized at runtime to validate
//! that facet-styx can handle our own schema format.

/// The STYX meta-schema source.
pub const META_SCHEMA_SOURCE: &str = r#"
meta {
  id https://styx-lang.org/schemas/schema
  version 2026-01-11
  description "Schema for STYX schema files"
}

schema {
  /// The root structure of a schema file.
  @ {
    /// Schema metadata (required).
    meta @Meta
    /// External schema imports (optional).
    imports @optional(@map(@string @string))
    /// Type definitions: @ for document root, strings for named types.
    schema @map(@union(@string @unit) @Schema)
  }

  /// Schema metadata.
  Meta {
    /// Unique identifier for the schema (URL recommended).
    id @string
    /// Schema version (date or semver).
    version @string
    /// Human-readable description.
    description @optional(@string)
  }

  /// A type constraint.
  Schema @union(
    @string      /// Literal value constraint.
    @            /// Type reference (any tag with unit payload).
    @Object      /// Object schema: {field @type}
    @Sequence    /// Sequence schema: (@type)
    @Union       /// Union: @union(@A @B)
    @Optional    /// Optional: @optional(@T)
    @Enum        /// Enum: @enum{a, b {x @type}}
    @Map         /// Map: @map(@K @V)
    @Flatten     /// Flatten: @flatten(@Type)
  )

  /// Object schema: maps keys to type constraints. The unit key (@) is reserved for "additional fields".
  Object @map(@union(@string @unit) @Schema)

  /// Sequence schema: all elements match the inner type.
  Sequence (@Schema)

  /// Union: matches any of the listed types.
  Union (@Schema)

  /// Optional: value of type T or absent.
  Optional @Schema

  /// Enum: variant names with optional payloads.
  Enum @map(@string @union(@unit @Object))

  /// Map: @map(@V) for string keys, @map(@K @V) for explicit key type.
  Map @union(
    (@Schema)
    (@Schema @Schema)
  )

  /// Flatten: inline fields from another type.
  Flatten @
}
"#;
