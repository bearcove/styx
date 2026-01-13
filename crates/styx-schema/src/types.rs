//! Schema type definitions derived from the meta-schema.
//!
//! These types are deserialized from STYX schema files using facet-styx.
//!
//! The meta-schema uses structural polymorphism - the shape of the data
//! determines the type, not an explicit discriminator. We use `#[facet(untagged)]`
//! to enable this kind of deserialization.

use std::collections::HashMap;

use facet::Facet;

/// A complete schema file.
///
/// Corresponds to the root structure in the meta-schema:
/// ```styx
/// @ {
///   meta @Meta
///   imports @optional(@map(@string @string))
///   schema @map(@union(@string @unit) @Schema)
/// }
/// ```
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

/// A type constraint.
///
/// In the meta-schema, Schema is defined as a union of:
/// - A scalar string (literal value constraint)
/// - A tag (type reference like @string, @MyType)
/// - An object (object schema)
/// - A sequence (sequence schema)
/// - Special tags like @union, @optional, @map, @enum, @flatten
///
/// For now, we use a simplified representation that matches what facet can deserialize.
/// Tags like `@optional(@string)` become objects `{ optional: [@string] }`.
#[derive(Facet, Debug, Clone)]
#[facet(untagged)]
#[repr(u8)]
pub enum Schema {
    // Composite type constructors with payloads
    // These are structs with a single field matching the tag name
    /// Optional type: @optional(@T) → { optional: [@T] }
    Optional(OptionalSchema),
    /// Union type: @union(@A @B) → { union: [@A, @B] }
    Union(UnionSchema),
    /// Map type: @map(@K @V) → { map: [@K, @V] }
    Map(MapSchema),
    /// Flatten directive: @flatten(@T) → { flatten: [@T] }
    Flatten(FlattenSchema),
    /// Enum type: @enum{...} → { enum: {...} }
    Enum(EnumTagSchema),

    // Object schema - an object literal like { field @type }
    /// Object schema with field definitions
    Object(HashMap<String, Schema>),

    // Sequence schema - a sequence literal like (@type)
    /// Sequence schema
    Sequence(Vec<Schema>),

    // Type references (tags like @string, @MyType)
    /// Type reference - any tag that doesn't match above patterns
    TypeRef(String),
}

/// Optional type wrapper: @optional(@T)
#[derive(Facet, Debug, Clone)]
pub struct OptionalSchema {
    pub optional: Vec<Schema>,
}

/// Union type wrapper: @union(@A @B ...)
#[derive(Facet, Debug, Clone)]
pub struct UnionSchema {
    pub union: Vec<Schema>,
}

/// Map type wrapper: @map(@K @V)
#[derive(Facet, Debug, Clone)]
pub struct MapSchema {
    pub map: Vec<Schema>,
}

/// Flatten type wrapper: @flatten(@T)
#[derive(Facet, Debug, Clone)]
pub struct FlattenSchema {
    pub flatten: Vec<Schema>,
}

/// Enum type wrapper: @enum{...}
#[derive(Facet, Debug, Clone)]
pub struct EnumTagSchema {
    #[facet(rename = "enum")]
    pub enum_variants: EnumSchema,
}

/// Helper type alias for object schemas.
/// In the meta-schema this is: `Object @map(@union(@string @unit) @Schema)`
pub type ObjectSchema = HashMap<String, Schema>;

/// Enum schema definition.
/// In the meta-schema: `Enum @map(@string @union(@unit @Object))`
/// Maps variant names to their payload types (unit for no payload, object for struct variants).
pub type EnumSchema = HashMap<String, EnumVariant>;

/// An enum variant.
#[derive(Facet, Debug, Clone)]
#[facet(untagged)]
#[repr(u8)]
pub enum EnumVariant {
    /// Struct variant with fields.
    Struct(ObjectSchema),
    /// Unit variant (no payload).
    Unit,
}
