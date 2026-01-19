//! Generate Styx schema syntax from Rust types using facet reflection.
//!
//! This module creates `.styx` schema files from Rust types, keeping
//! the schema and type definitions in sync.

extern crate alloc;

use alloc::collections::BTreeSet;
use alloc::string::String;
use alloc::vec::Vec;
use core::ptr::NonNull;

use facet::{
    Def, DefaultSource, EnumType, Facet, Field, FieldFlags, Peek, PtrConst, PtrMut, PtrUninit,
    Shape, ShapeLayout, StructKind, Type, UserType,
};
use facet_styx::peek_to_string;
use styx_format::{FormatOptions, format_value};
use styx_parse::Separator;
use styx_tree::{Entry, Object, Payload, Sequence, Tag, Value};

/// Try to get the default value for a field as a styx Value.
/// Returns None if the field has no default or if serialization fails.
fn field_default_value(field: &Field) -> Option<Value> {
    let default_source = field.default?;
    let shape = field.shape.get();

    // Allocate memory for the value
    let layout = match shape.layout {
        ShapeLayout::Sized(l) => l,
        ShapeLayout::Unsized => return None,
    };
    if layout.size() == 0 {
        // Zero-sized type - can't really serialize a meaningful default
        return None;
    }

    let ptr = unsafe { alloc::alloc::alloc(layout) };
    if ptr.is_null() {
        return None;
    }
    let ptr = unsafe { NonNull::new_unchecked(ptr) };

    // Initialize with the default value
    let ptr_uninit = PtrUninit::new(ptr.as_ptr());
    match default_source {
        DefaultSource::Custom(default_fn) => {
            unsafe { default_fn(ptr_uninit) };
        }
        DefaultSource::FromTrait => {
            let ptr_mut = unsafe { ptr_uninit.assume_init() };
            if unsafe { shape.call_default_in_place(ptr_mut) }.is_none() {
                // No default implementation available
                unsafe { alloc::alloc::dealloc(ptr.as_ptr(), layout) };
                return None;
            }
        }
    }

    // Create a Peek to serialize
    let ptr_const = PtrConst::new(ptr.as_ptr());
    let peek = unsafe { Peek::unchecked_new(ptr_const, shape) };

    // Serialize to styx string
    let styx_str = peek_to_string(peek).ok()?;

    // Drop the value and free memory
    unsafe {
        shape.call_drop_in_place(PtrMut::new(ptr.as_ptr()));
        alloc::alloc::dealloc(ptr.as_ptr(), layout);
    }

    // Use the serialized string as a scalar value
    Some(Value::scalar(&styx_str))
}

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
pub fn to_styx_schema<T: Facet<'static>>() -> String {
    let mut generator = StyxSchemaGenerator::new();
    generator.add_root::<T>();
    let value = generator.finish_value();
    format_value(&value, FormatOptions::default())
}

/// Generate a Styx schema as a Value tree from a Facet type.
pub fn to_styx_schema_value<T: Facet<'static>>() -> Value {
    let mut generator = StyxSchemaGenerator::new();
    generator.add_root::<T>();
    generator.finish_value()
}

/// Generator for Styx schema definitions.
pub struct StyxSchemaGenerator {
    /// Type definitions: (name, value, doc_comment)
    /// Name is None for root type, Some(name) for named types
    definitions: Vec<(Option<String>, Value, Option<String>)>,
    /// Types already generated (by type identifier)
    generated: BTreeSet<&'static str>,
    /// Types queued for generation: (name_to_use, shape)
    queue: Vec<(Option<String>, &'static Shape)>,
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

    /// Add a root type (with None as name, serialized as unit `@`).
    pub fn add_root<T: Facet<'static>>(&mut self) {
        self.queue.push((None, T::SHAPE));
    }

    /// Add a type to generate using its Rust type identifier as name.
    pub fn add_type<T: Facet<'static>>(&mut self) {
        self.add_shape(T::SHAPE);
    }

    /// Add a shape to generate.
    fn add_shape(&mut self, shape: &'static Shape) {
        if !self.generated.contains(shape.type_identifier) {
            self.queue
                .push((Some(shape.type_identifier.to_string()), shape));
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

        // Sort definitions for stable output (root None first, then alphabetically)
        self.definitions.sort_by(|a, b| a.0.cmp(&b.0));

        // Build output object
        let entries: Vec<Entry> = self
            .definitions
            .into_iter()
            .map(|(name, value, doc)| Entry {
                key: match name {
                    None => Value::unit(),
                    Some(s) => scalar(&s),
                },
                value,
                doc_comment: doc,
            })
            .collect();

        object_value(entries, Separator::Newline)
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
                        let mut field_type = self.type_for_shape(field.shape.get());

                        // Wrap with @default if field has a default value
                        if let Some(default_value) = field_default_value(field) {
                            // Format: @default(value @type)
                            field_type = tag(
                                "default",
                                Some(Value {
                                    tag: None,
                                    payload: Some(Payload::Sequence(Sequence {
                                        items: vec![default_value, field_type],
                                        span: None,
                                    })),
                                    span: None,
                                }),
                            );
                        }

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

                tag("object", Some(object_value(entries, Separator::Comma)))
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
                            Some(object_value(field_entries, Separator::Comma)),
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

        tag("enum", Some(object_value(entries, Separator::Comma)))
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
                let is_string_key = matches!(
                    &key_type,
                    Value { tag: Some(t), payload: None, .. } if t.name == "string"
                );

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
    Value::scalar(text)
}

fn tag(name: &str, payload: Option<Value>) -> Value {
    // Build a tagged value: @name or @name(payload)
    let tag = Tag {
        name: name.to_string(),
        span: None,
    };
    match payload {
        None => Value {
            tag: Some(tag),
            payload: None,
            span: None,
        },
        Some(v) => {
            // Use the payload directly from the inner value
            // If it's an object/sequence, use that payload; otherwise wrap scalar in sequence
            let inner_payload = match v.payload {
                Some(Payload::Object(o)) => Payload::Object(o),
                Some(Payload::Sequence(s)) => Payload::Sequence(s),
                Some(Payload::Scalar(s)) => Payload::Sequence(Sequence {
                    items: vec![Value {
                        tag: v.tag,
                        payload: Some(Payload::Scalar(s)),
                        span: None,
                    }],
                    span: None,
                }),
                None => {
                    // Inner value is unit or just a tag - wrap it
                    Payload::Sequence(Sequence {
                        items: vec![v],
                        span: None,
                    })
                }
            };
            Value {
                tag: Some(tag),
                payload: Some(inner_payload),
                span: None,
            }
        }
    }
}

fn sequence(items: Vec<Value>) -> Value {
    Value::seq(items)
}

fn object_value(entries: Vec<Entry>, separator: Separator) -> Value {
    Value {
        tag: None,
        payload: Some(Payload::Object(Object {
            entries,
            separator,
            span: None,
        })),
        span: None,
    }
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

        let schema = to_styx_schema::<User>();
        insta::assert_snapshot!(schema);
    }

    #[test]
    fn test_optional_field() {
        #[derive(Facet)]
        struct Config {
            required: String,
            optional: Option<String>,
        }

        let schema = to_styx_schema::<Config>();
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

        let schema = to_styx_schema::<Status>();
        insta::assert_snapshot!(schema);
    }

    #[test]
    fn test_vec() {
        #[derive(Facet)]
        struct Data {
            items: Vec<String>,
        }

        let schema = to_styx_schema::<Data>();
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

        let schema = to_styx_schema::<Outer>();
        insta::assert_snapshot!(schema);
    }

    #[test]
    fn test_hashmap() {
        use std::collections::HashMap;

        #[derive(Facet)]
        struct Registry {
            entries: HashMap<String, i32>,
        }

        let schema = to_styx_schema::<Registry>();
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

        let schema = to_styx_schema::<Message>();
        insta::assert_snapshot!(schema);
    }

    #[test]
    fn test_boxed_field() {
        #[derive(Facet)]
        struct Node {
            value: i32,
            next: Option<Box<Node>>,
        }

        let schema = to_styx_schema::<Node>();
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

        let schema = to_styx_schema::<ApiResponse>();
        insta::assert_snapshot!(schema);
    }

    #[test]
    fn test_schema_file_type() {
        use crate::types::SchemaFile;

        let schema = to_styx_schema::<SchemaFile>();
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

        let schema = to_styx_schema::<User>();
        insta::assert_snapshot!(schema);
    }

    #[test]
    fn test_field_with_default() {
        #[derive(Facet)]
        struct Config {
            name: String,
            #[facet(default = 8080)]
            port: u16,
            #[facet(default = "localhost".to_string())]
            host: String,
        }

        let schema = to_styx_schema::<Config>();
        insta::assert_snapshot!(schema);
    }
}