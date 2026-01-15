//! Schema type definitions derived from the meta-schema.
//!
//! These types are deserialized from STYX schema files using facet-styx.

use std::collections::HashMap;

use facet::Facet;

/// A complete schema file.
#[derive(Facet, Debug, Clone)]
pub struct SchemaFile {
    /// Schema metadata (required).
    pub meta: Meta,
    /// External schema imports (optional).
    /// Maps namespace prefixes to external schema locations.
    pub imports: Option<HashMap<String, String>>,
    /// Type definitions.
    /// Keys are type names as strings. The key "@" represents the document root.
    pub schema: HashMap<String, Schema>,
}

/// Schema metadata.
#[derive(Facet, Debug, Clone)]
pub struct Meta {
    /// Unique identifier for the schema (URL recommended).
    pub id: String,
    /// Schema version (date or semver).
    pub version: String,
    /// Human-readable description.
    pub description: Option<String>,
}

/// A type constraint (corresponds to Schema @enum{...} in the meta-schema).
///
/// This is a tagged enum - each variant corresponds to a STYX tag like
/// `@object`, `@seq`, `@union`, etc.
#[derive(Facet, Debug, Clone)]
#[facet(rename_all = "lowercase")]
#[repr(u8)]
pub enum Schema {
    /// Literal value constraint (a scalar value that must match exactly).
    Literal(String),

    /// Object schema: @object{field @type, @ @type}.
    Object(ObjectSchema),

    /// Sequence schema: @seq(@type).
    Seq(SeqSchema),

    /// Union: @union(@A @B ...).
    Union(UnionSchema),

    /// Optional: @optional(@T).
    Optional(OptionalSchema),

    /// Enum: @enum{variant, variant @object{...}}.
    Enum(EnumSchema),

    /// Map: @map(@V) or @map(@K @V).
    Map(MapSchema),

    /// Flatten: @flatten(@Type).
    Flatten(FlattenSchema),

    /// Type reference (any tag with unit payload, e.g., @string, @MyType).
    /// This is the fallback for unknown tags.
    /// `name` is `None` for unit tags (`@`), `Some("...")` for named tags.
    #[facet(other)]
    Type {
        #[facet(tag)]
        name: Option<String>,
    },
}

/// Object schema: @object{field @Schema, @ @Schema}.
/// Maps field names to their type constraints.
/// The key "@" (unit) represents additional fields.
#[derive(Facet, Debug, Clone)]
#[repr(transparent)]
pub struct ObjectSchema(pub HashMap<String, Schema>);

/// Sequence schema: @seq(@Schema).
/// All elements must match the inner schema.
#[derive(Facet, Debug, Clone)]
#[repr(transparent)]
pub struct SeqSchema(pub Vec<Schema>);

/// Union schema: @union(@A @B ...).
/// Value must match one of the listed types.
#[derive(Facet, Debug, Clone)]
#[repr(transparent)]
pub struct UnionSchema(pub Vec<Schema>);

/// Optional schema: @optional(@T).
/// Field can be absent or match the inner type.
#[derive(Facet, Debug, Clone)]
#[repr(transparent)]
pub struct OptionalSchema(pub Vec<Schema>);

/// Enum schema: @enum{variant @Type, variant @object{...}}.
/// Maps variant names to their payload schemas.
#[derive(Facet, Debug, Clone)]
#[repr(transparent)]
pub struct EnumSchema(pub HashMap<String, Schema>);

/// Map schema: @map(@V) or @map(@K @V).
/// The sequence contains 1 element (value type) or 2 elements (key type, value type).
#[derive(Facet, Debug, Clone)]
#[repr(transparent)]
pub struct MapSchema(pub Vec<Schema>);

/// Flatten schema: @flatten(@Type).
/// Inlines fields from another type.
#[derive(Facet, Debug, Clone)]
#[repr(transparent)]
pub struct FlattenSchema(pub Vec<Schema>);
