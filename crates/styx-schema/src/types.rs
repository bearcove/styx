//! Schema type definitions derived from the meta-schema.
//!
//! These types are deserialized from STYX schema files using facet-styx.
//!
//! NOTE: The current implementation is a simplified version. The full
//! meta-schema uses structural polymorphism (a Schema can be a string,
//! a tag, an object, etc.) which doesn't map directly to Rust enums.
//! A production implementation would need custom deserialization logic.

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
    /// (The meta-schema uses `@union(@string @unit)` but we normalize unit to "@")
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
/// - @Object (object schema)
/// - @Sequence (sequence schema)
/// - @Union (union type)
/// - @Optional (optional field)
/// - @Enum (enum type)
/// - @Map (map type)
/// - @Flatten (flatten directive)
///
/// For facet deserialization, enum variants must match the tag names
/// that appear in the data. Tags like `@string` become the scalar "string"
/// which facet looks up as a variant name.
#[derive(Facet, Debug, Clone)]
#[repr(u8)]
pub enum Schema {
    // Built-in type references (common ones)
    /// @string type reference
    #[facet(rename = "string")]
    String,
    /// @boolean type reference
    #[facet(rename = "boolean")]
    Boolean,
    /// @u8 type reference
    #[facet(rename = "u8")]
    U8,
    /// @u16 type reference
    #[facet(rename = "u16")]
    U16,
    /// @u32 type reference
    #[facet(rename = "u32")]
    U32,
    /// @u64 type reference
    #[facet(rename = "u64")]
    U64,
    /// @i8 type reference
    #[facet(rename = "i8")]
    I8,
    /// @i16 type reference
    #[facet(rename = "i16")]
    I16,
    /// @i32 type reference
    #[facet(rename = "i32")]
    I32,
    /// @i64 type reference
    #[facet(rename = "i64")]
    I64,
    /// @any type reference
    #[facet(rename = "any")]
    Any,

    // Composite type constructors
    /// Optional type: @optional(@T)
    #[facet(rename = "optional")]
    Optional(Vec<Schema>),
    /// Union type: @union(@A @B ...)
    #[facet(rename = "union")]
    Union(Vec<Schema>),
    /// Map type: @map(@V) or @map(@K @V)
    #[facet(rename = "map")]
    Map(Vec<Schema>),
    /// Flatten directive: @flatten(@Type)
    #[facet(rename = "flatten")]
    Flatten(Vec<Schema>),
    /// Enum type: @enum{...}
    #[facet(rename = "enum")]
    Enum(EnumSchema),
}

/// Object schema definition.
///
/// In the meta-schema this is: `Object @map(@union(@string @unit) @Schema)`
/// We use String keys where "@" represents the additional fields schema.
#[derive(Facet, Debug, Clone)]
pub struct ObjectSchema {
    /// All fields including the special "@" key for additional fields.
    pub fields: HashMap<String, Schema>,
}

impl ObjectSchema {
    /// Get the schema for additional fields (the "@" key), if present.
    pub fn additional_fields(&self) -> Option<&Schema> {
        self.fields.get("@")
    }

    /// Get a named field's schema.
    pub fn field(&self, name: &str) -> Option<&Schema> {
        self.fields.get(name)
    }
}

/// Enum schema definition.
///
/// In the meta-schema: `Enum @map(@string @union(@unit @Object))`
#[derive(Facet, Debug, Clone)]
pub struct EnumSchema {
    /// Variant definitions.
    /// Unit variants have no payload (value is unit), struct variants have an ObjectSchema.
    pub variants: HashMap<String, EnumVariant>,
}

/// An enum variant.
#[derive(Facet, Debug, Clone)]
#[repr(u8)]
pub enum EnumVariant {
    /// Unit variant (no payload).
    Unit,
    /// Struct variant with fields.
    Struct(ObjectSchema),
}
