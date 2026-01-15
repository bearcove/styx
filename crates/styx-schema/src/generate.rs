//! Generate Styx schema syntax from Rust types using facet reflection.
//!
//! This module creates `.styx` schema files from Rust types, keeping
//! the schema and type definitions in sync.

extern crate alloc;

use alloc::collections::BTreeSet;
use alloc::string::String;
use alloc::vec::Vec;

use facet::{Def, EnumType, Facet, Field, FieldFlags, Shape, StructKind, Type, UserType};
use styx_format::{FormatOptions, format_value};
use styx_parse::{ScalarKind, Separator};
use styx_tree::{Entry, Object, Scalar, Sequence, Tagged, Value};

/// Generate a Styx schema string from a Facet type.
///
/// # Example
///
/// ```ignore
/// use facet::Facet;
/// use styx_schema::generate::to_styx_schema;
///
/// #[derive(Facet)]
/// struct User {
///     name: String,
///     age: u32,
/// }
///
/// let schema = to_styx_schema::<User>("User");
/// // User @object{ name @string, age @int }
/// ```
pub fn to_styx_schema<T: Facet<'static>>(root_name: &str) -> String {
    let mut generator = StyxSchemaGenerator::new();
    generator.add_root::<T>(root_name);
    let value = generator.finish_value();
    format_value(&value, FormatOptions::default())
}

/// Generate a Styx schema as a Value tree from a Facet type.
pub fn to_styx_schema_value<T: Facet<'static>>(root_name: &str) -> Value {
    let mut generator = StyxSchemaGenerator::new();
    generator.add_root::<T>(root_name);
    generator.finish_value()
}

/// Generator for Styx schema definitions.
pub struct StyxSchemaGenerator {
    /// Type definitions: (name, value, doc_comment)
    definitions: Vec<(String, Value, Option<String>)>,
    /// Types already generated (by type identifier)
    generated: BTreeSet<&'static str>,
    /// Types queued for generation: (name_to_use, shape)
    queue: Vec<(String, &'static Shape)>,
}

impl Default for StyxSchemaGenerator {
    fn default() -> Self {
        Self::new()
    }
}

impl StyxSchemaGenerator {
    /// Create a new Styx schema generator.
    pub const fn new() -> Self {
        Self {
            definitions: Vec::new(),
            generated: BTreeSet::new(),
            queue: Vec::new(),
        }
    }

    /// Add a root type with a custom name.
    pub fn add_root<T: Facet<'static>>(&mut self, name: &str) {
        self.queue.push((name.to_string(), T::SHAPE));
    }

    /// Add a type to generate using its Rust type identifier as name.
    pub fn add_type<T: Facet<'static>>(&mut self) {
        self.add_shape(T::SHAPE);
    }

    /// Add a shape to generate.
    fn add_shape(&mut self, shape: &'static Shape) {
        if !self.generated.contains(shape.type_identifier) {
            self.queue.push((shape.type_identifier.to_string(), shape));
        }
    }

    /// Finish generation and return the schema as a Value tree.
    pub fn finish_value(mut self) -> Value {
        // Process queue until empty
        while let Some((name, shape)) = self.queue.pop() {
            if self.generated.contains(shape.type_identifier) {
                continue;
            }
            self.generated.insert(shape.type_identifier);
            if let Some((value, doc)) = self.generate_shape(shape) {
                self.definitions.push((name, value, doc));
            }
        }

        // Sort definitions for stable output (root "@" first, then alphabetically)
        self.definitions.sort_by(|a, b| {
            if a.0 == "@" {
                core::cmp::Ordering::Less
            } else if b.0 == "@" {
                core::cmp::Ordering::Greater
            } else {
                a.0.cmp(&b.0)
            }
        });

        // Build output object
        let entries: Vec<Entry> = self
            .definitions
            .into_iter()
            .map(|(name, value, doc)| Entry {
                key: scalar(&name),
                value,
                doc_comment: doc,
            })
            .collect();

        Value::Object(Object {
            entries,
            separator: Separator::Newline,
            span: None,
        })
    }

    /// Finish generation and return the Styx schema string.
    pub fn finish(self) -> String {
        let value = self.finish_value();
        format_value(&value, FormatOptions::default())
    }

    /// Generate a schema definition for a shape. Returns None for inline types.
    fn generate_shape(&mut self, shape: &'static Shape) -> Option<(Value, Option<String>)> {
        // Handle transparent wrappers - don't generate, they inline
        if shape.inner.is_some() {
            return None;
        }

        // Extract doc comment
        let doc = clean_doc(shape.doc);

        match &shape.ty {
            Type::User(UserType::Struct(st)) => {
                Some((self.generate_struct(shape, st.fields, st.kind), doc))
            }
            Type::User(UserType::Enum(en)) => Some((self.generate_enum(shape, en), doc)),
            _ => {
                // For other types, generate inline
                Some((self.type_for_shape(shape), doc))
            }
        }
    }

    fn generate_struct(
        &mut self,
        _shape: &'static Shape,
        fields: &'static [Field],
        kind: StructKind,
    ) -> Value {
        match kind {
            StructKind::Unit => tag("unit", None),
            StructKind::TupleStruct if fields.len() == 1 => {
                // Newtype - inline the inner type
                self.type_for_shape(fields[0].shape.get())
            }
            StructKind::Tuple if fields.len() == 1 => {
                // 1-tuple - just use the element type (unwrap the tuple)
                self.type_for_shape(fields[0].shape.get())
            }
            StructKind::TupleStruct | StructKind::Tuple => {
                // Multi-element tuple as sequence
                let items: Vec<Value> = fields
                    .iter()
                    .map(|f| self.type_for_shape(f.shape.get()))
                    .collect();
                tag("seq", Some(sequence(items)))
            }
            StructKind::Struct => {
                let entries: Vec<Entry> = fields
                    .iter()
                    .filter(|f| !f.flags.contains(FieldFlags::SKIP))
                    .map(|field| {
                        let field_name = field.effective_name();
                        let field_type = self.type_for_shape(field.shape.get());

                        // Extract field doc comment
                        let doc = clean_doc(field.doc);

                        // Handle special "@" field name for catch-all
                        let key = if field_name.is_empty() {
                            "@"
                        } else {
                            field_name
                        };

                        Entry {
                            key: scalar(key),
                            value: field_type,
                            doc_comment: doc,
                        }
                    })
                    .collect();

                tag(
                    "object",
                    Some(Value::Object(Object {
                        entries,
                        separator: Separator::Comma,
                        span: None,
                    })),
                )
            }
        }
    }

    fn generate_enum(&mut self, _shape: &'static Shape, enum_type: &EnumType) -> Value {
        let entries: Vec<Entry> = enum_type
            .variants
            .iter()
            .map(|variant| {
                let variant_name = variant.effective_name();

                // Extract variant doc comment
                let doc = clean_doc(variant.doc);

                let value = match variant.data.kind {
                    StructKind::Unit => {
                        // Unit variant
                        tag("unit", None)
                    }
                    StructKind::TupleStruct if variant.data.fields.len() == 1 => {
                        // Newtype variant
                        self.type_for_shape(variant.data.fields[0].shape.get())
                    }
                    StructKind::TupleStruct | StructKind::Tuple => {
                        // Tuple variant as sequence
                        let items: Vec<Value> = variant
                            .data
                            .fields
                            .iter()
                            .map(|f| self.type_for_shape(f.shape.get()))
                            .collect();
                        tag("seq", Some(sequence(items)))
                    }
                    StructKind::Struct => {
                        // Struct variant as object
                        let field_entries: Vec<Entry> = variant
                            .data
                            .fields
                            .iter()
                            .map(|field| {
                                let field_name = field.effective_name();
                                let field_type = self.type_for_shape(field.shape.get());
                                Entry {
                                    key: scalar(field_name),
                                    value: field_type,
                                    doc_comment: None,
                                }
                            })
                            .collect();

                        tag(
                            "object",
                            Some(Value::Object(Object {
                                entries: field_entries,
                                separator: Separator::Comma,
                                span: None,
                            })),
                        )
                    }
                };

                Entry {
                    key: scalar(variant_name),
                    value,
                    doc_comment: doc,
                }
            })
            .collect();

        tag(
            "enum",
            Some(Value::Object(Object {
                entries,
                separator: Separator::Comma,
                span: None,
            })),
        )
    }

    fn type_for_shape(&mut self, shape: &'static Shape) -> Value {
        // Check Def first
        match &shape.def {
            Def::Scalar => self.scalar_type(shape),
            Def::Option(opt) => tag("optional", Some(self.type_for_shape(opt.t))),
            Def::List(list) => tag("seq", Some(self.type_for_shape(list.t))),
            Def::Array(arr) => tag("seq", Some(self.type_for_shape(arr.t))),
            Def::Set(set) => tag("seq", Some(self.type_for_shape(set.t))),
            Def::Map(map) => {
                // Check if key is string - if so, just value type
                // Otherwise key + value
                let key_type = self.type_for_shape(map.k);
                let value_type = self.type_for_shape(map.v);

                // Check if key is @string
                let is_string_key = matches!(&key_type, Value::Tagged(t) if t.tag == "string" && t.payload.is_none());

                if is_string_key {
                    tag("map", Some(value_type))
                } else {
                    tag("map", Some(sequence(vec![key_type, value_type])))
                }
            }
            Def::Pointer(ptr) => {
                // Smart pointers are transparent
                if let Some(pointee) = ptr.pointee {
                    self.type_for_shape(pointee)
                } else {
                    tag("any", None)
                }
            }
            Def::Undefined => {
                // Check if it's a transparent wrapper first
                if let Some(inner) = shape.inner {
                    return self.type_for_shape(inner);
                }

                // User-defined types - queue for generation and return reference
                match &shape.ty {
                    Type::User(UserType::Struct(st)) => {
                        // Tuples should be inlined, not referenced
                        match st.kind {
                            StructKind::Tuple if st.fields.len() == 1 => {
                                // 1-tuple: inline the element
                                self.type_for_shape(st.fields[0].shape.get())
                            }
                            StructKind::Tuple => {
                                // Multi-element tuple: inline as sequence
                                let items: Vec<Value> = st
                                    .fields
                                    .iter()
                                    .map(|f| self.type_for_shape(f.shape.get()))
                                    .collect();
                                tag("seq", Some(sequence(items)))
                            }
                            _ => {
                                // Regular struct: queue for generation
                                self.add_shape(shape);
                                tag(shape.type_identifier, None)
                            }
                        }
                    }
                    Type::User(UserType::Enum(_)) => {
                        self.add_shape(shape);
                        tag(shape.type_identifier, None)
                    }
                    _ => tag("any", None),
                }
            }
            _ => {
                // For other defs, check if it's a transparent wrapper
                if let Some(inner) = shape.inner {
                    self.type_for_shape(inner)
                } else {
                    tag("any", None)
                }
            }
        }
    }

    fn scalar_type(&self, shape: &'static Shape) -> Value {
        let tag_name = match shape.type_identifier {
            // Strings
            "String" | "str" | "&str" | "Cow" => "string",

            // Booleans
            "bool" => "bool",

            // Integers (all become @int in Styx)
            "u8" | "u16" | "u32" | "u64" | "u128" | "usize" | "i8" | "i16" | "i32" | "i64"
            | "i128" | "isize" => "int",

            // Floats
            "f32" | "f64" => "float",

            // Char as string
            "char" => "string",

            // Unit
            "()" => "unit",

            // Unknown scalar
            _ => "any",
        };

        tag(tag_name, None)
    }
}

// Helper functions for building Value trees

/// Clean up doc comments from Rust - trim leading space from each line.
fn clean_doc(doc: &[&str]) -> Option<String> {
    if doc.is_empty() {
        return None;
    }
    let cleaned: Vec<&str> = doc
        .iter()
        .map(|line| line.strip_prefix(' ').unwrap_or(line))
        .collect();
    Some(cleaned.join("\n"))
}

fn scalar(text: &str) -> Value {
    Value::Scalar(Scalar {
        text: text.to_string(),
        kind: ScalarKind::Bare,
        span: None,
    })
}

fn tag(name: &str, payload: Option<Value>) -> Value {
    Value::Tagged(Tagged {
        tag: name.to_string(),
        payload: payload.map(Box::new),
        span: None,
    })
}

fn sequence(items: Vec<Value>) -> Value {
    Value::Sequence(Sequence { items, span: None })
}

#[cfg(test)]
mod tests {
    use super::*;
    use facet::Facet;
    use facet_testhelpers::test;

    #[test]
    fn test_simple_struct() {
        #[derive(Facet)]
        struct User {
            name: String,
            age: u32,
        }

        let schema = to_styx_schema::<User>("User");
        insta::assert_snapshot!(schema);
    }

    #[test]
    fn test_optional_field() {
        #[derive(Facet)]
        struct Config {
            required: String,
            optional: Option<String>,
        }

        let schema = to_styx_schema::<Config>("Config");
        insta::assert_snapshot!(schema);
    }

    #[test]
    fn test_simple_enum() {
        #[derive(Facet)]
        #[repr(u8)]
        enum Status {
            Active,
            Inactive,
            Pending,
        }

        let schema = to_styx_schema::<Status>("Status");
        insta::assert_snapshot!(schema);
    }

    #[test]
    fn test_vec() {
        #[derive(Facet)]
        struct Data {
            items: Vec<String>,
        }

        let schema = to_styx_schema::<Data>("Data");
        insta::assert_snapshot!(schema);
    }

    #[test]
    fn test_nested_types() {
        #[derive(Facet)]
        struct Inner {
            value: i32,
        }

        #[derive(Facet)]
        struct Outer {
            inner: Inner,
            name: String,
        }

        let schema = to_styx_schema::<Outer>("Outer");
        insta::assert_snapshot!(schema);
    }

    #[test]
    fn test_hashmap() {
        use std::collections::HashMap;

        #[derive(Facet)]
        struct Registry {
            entries: HashMap<String, i32>,
        }

        let schema = to_styx_schema::<Registry>("Registry");
        insta::assert_snapshot!(schema);
    }

    #[test]
    fn test_enum_with_data() {
        #[derive(Facet)]
        #[repr(C)]
        #[allow(dead_code)]
        enum Message {
            Text(String),
            Number(i32),
            Compound { x: i32, y: i32 },
        }

        let schema = to_styx_schema::<Message>("Message");
        insta::assert_snapshot!(schema);
    }

    #[test]
    fn test_boxed_field() {
        #[derive(Facet)]
        struct Node {
            value: i32,
            next: Option<Box<Node>>,
        }

        let schema = to_styx_schema::<Node>("Node");
        insta::assert_snapshot!(schema);
    }

    #[test]
    fn test_rename_all() {
        #[derive(Facet)]
        #[facet(rename_all = "camelCase")]
        struct ApiResponse {
            user_name: String,
            created_at: String,
        }

        let schema = to_styx_schema::<ApiResponse>("ApiResponse");
        insta::assert_snapshot!(schema);
    }

    #[test]
    fn test_schema_file_type() {
        use crate::types::SchemaFile;

        let schema = to_styx_schema::<SchemaFile>("@");
        insta::assert_snapshot!(schema);
    }

    #[test]
    fn test_with_doc_comments() {
        /// A user in the system.
        #[derive(Facet)]
        struct User {
            /// The user's full name.
            name: String,
            /// Age in years.
            age: u32,
        }

        let schema = to_styx_schema::<User>("User");
        insta::assert_snapshot!(schema);
    }
}
