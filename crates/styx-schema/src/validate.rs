//! Schema validation for Styx documents.
//!
//! Validates `styx_tree::Value` instances against `Schema` definitions.

use std::collections::HashSet;

use styx_tree::Value;

use crate::error::{ValidationError, ValidationErrorKind, ValidationResult};
use crate::types::{
    EnumSchema, FlattenSchema, MapSchema, ObjectSchema, OptionalSchema, Schema, SchemaFile,
    SeqSchema, UnionSchema,
};

/// Validator for Styx documents.
pub struct Validator<'a> {
    /// The schema file containing type definitions.
    schema_file: &'a SchemaFile,
}

impl<'a> Validator<'a> {
    /// Create a new validator with the given schema.
    pub fn new(schema_file: &'a SchemaFile) -> Self {
        Self { schema_file }
    }

    /// Validate a document against the schema's root type.
    pub fn validate_document(&self, doc: &Value) -> ValidationResult {
        // Look up the root schema (key "@")
        match self.schema_file.schema.get("@") {
            Some(root_schema) => self.validate_value(doc, root_schema, ""),
            None => {
                let mut result = ValidationResult::ok();
                result.error(ValidationError::new(
                    "",
                    ValidationErrorKind::SchemaError {
                        reason: "no root type (@) defined in schema".into(),
                    },
                    "schema has no root type definition",
                ));
                result
            }
        }
    }

    /// Validate a value against a specific named type.
    pub fn validate_as_type(&self, value: &Value, type_name: &str) -> ValidationResult {
        match self.schema_file.schema.get(type_name) {
            Some(schema) => self.validate_value(value, schema, ""),
            None => {
                let mut result = ValidationResult::ok();
                result.error(ValidationError::new(
                    "",
                    ValidationErrorKind::UnknownType {
                        name: type_name.into(),
                    },
                    format!("unknown type '{type_name}'"),
                ));
                result
            }
        }
    }

    /// Validate a value against a schema.
    pub fn validate_value(&self, value: &Value, schema: &Schema, path: &str) -> ValidationResult {
        match schema {
            Schema::Literal(expected) => self.validate_literal(value, expected, path),
            Schema::Type { name } => self.validate_type_ref(value, name.as_deref(), path),
            Schema::Object(obj_schema) => self.validate_object(value, obj_schema, path),
            Schema::Seq(seq_schema) => self.validate_seq(value, seq_schema, path),
            Schema::Union(union_schema) => self.validate_union(value, union_schema, path),
            Schema::Optional(opt_schema) => self.validate_optional(value, opt_schema, path),
            Schema::Enum(enum_schema) => self.validate_enum(value, enum_schema, path),
            Schema::Map(map_schema) => self.validate_map(value, map_schema, path),
            Schema::Flatten(flatten_schema) => self.validate_flatten(value, flatten_schema, path),
        }
    }

    /// Validate a literal value.
    fn validate_literal(&self, value: &Value, expected: &str, path: &str) -> ValidationResult {
        let mut result = ValidationResult::ok();

        match value {
            Value::Scalar(s) if s.text == expected => {
                // Exact match
            }
            Value::Scalar(s) => {
                result.error(
                    ValidationError::new(
                        path,
                        ValidationErrorKind::InvalidValue {
                            reason: format!("expected literal '{expected}', got '{}'", s.text),
                        },
                        format!("expected '{expected}', got '{}'", s.text),
                    )
                    .with_span(s.span),
                );
            }
            _ => {
                result.error(ValidationError::new(
                    path,
                    ValidationErrorKind::ExpectedScalar,
                    format!("expected literal '{expected}', got non-scalar"),
                ));
            }
        }

        result
    }

    /// Validate a type reference.
    fn validate_type_ref(
        &self,
        value: &Value,
        type_name: Option<&str>,
        path: &str,
    ) -> ValidationResult {
        let mut result = ValidationResult::ok();

        match type_name {
            None => {
                // Unit type reference (@) - value must be unit
                if !value.is_unit() {
                    result.error(ValidationError::new(
                        path,
                        ValidationErrorKind::TypeMismatch {
                            expected: "unit".into(),
                            got: value_type_name(value).into(),
                        },
                        "expected unit value",
                    ));
                }
            }
            Some("string") => {
                // @string - any scalar
                if !matches!(value, Value::Scalar(_)) {
                    result.error(ValidationError::new(
                        path,
                        ValidationErrorKind::ExpectedScalar,
                        format!("expected string, got {}", value_type_name(value)),
                    ));
                }
            }
            Some("int") | Some("integer") => {
                // @int - scalar that parses as integer
                match value {
                    Value::Scalar(s) => {
                        if s.text.parse::<i64>().is_err() {
                            result.error(
                                ValidationError::new(
                                    path,
                                    ValidationErrorKind::InvalidValue {
                                        reason: "not a valid integer".into(),
                                    },
                                    format!("'{}' is not a valid integer", s.text),
                                )
                                .with_span(s.span),
                            );
                        }
                    }
                    _ => {
                        result.error(ValidationError::new(
                            path,
                            ValidationErrorKind::ExpectedScalar,
                            format!("expected integer, got {}", value_type_name(value)),
                        ));
                    }
                }
            }
            Some("float") | Some("number") => {
                // @float - scalar that parses as float
                match value {
                    Value::Scalar(s) => {
                        if s.text.parse::<f64>().is_err() {
                            result.error(
                                ValidationError::new(
                                    path,
                                    ValidationErrorKind::InvalidValue {
                                        reason: "not a valid number".into(),
                                    },
                                    format!("'{}' is not a valid number", s.text),
                                )
                                .with_span(s.span),
                            );
                        }
                    }
                    _ => {
                        result.error(ValidationError::new(
                            path,
                            ValidationErrorKind::ExpectedScalar,
                            format!("expected number, got {}", value_type_name(value)),
                        ));
                    }
                }
            }
            Some("bool") | Some("boolean") => {
                // @bool - scalar that is true/false
                match value {
                    Value::Scalar(s) => {
                        if s.text != "true" && s.text != "false" {
                            result.error(
                                ValidationError::new(
                                    path,
                                    ValidationErrorKind::InvalidValue {
                                        reason: "not a valid boolean".into(),
                                    },
                                    format!(
                                        "'{}' is not a valid boolean (expected true/false)",
                                        s.text
                                    ),
                                )
                                .with_span(s.span),
                            );
                        }
                    }
                    _ => {
                        result.error(ValidationError::new(
                            path,
                            ValidationErrorKind::ExpectedScalar,
                            format!("expected boolean, got {}", value_type_name(value)),
                        ));
                    }
                }
            }
            Some("unit") => {
                // @unit - must be unit value
                if !value.is_unit() {
                    result.error(ValidationError::new(
                        path,
                        ValidationErrorKind::TypeMismatch {
                            expected: "unit".into(),
                            got: value_type_name(value).into(),
                        },
                        "expected unit value",
                    ));
                }
            }
            Some("any") => {
                // @any - anything goes
            }
            Some(name) => {
                // Named type reference - look up in schema
                if let Some(type_schema) = self.schema_file.schema.get(name) {
                    result.merge(self.validate_value(value, type_schema, path));
                } else {
                    result.error(ValidationError::new(
                        path,
                        ValidationErrorKind::UnknownType { name: name.into() },
                        format!("unknown type '{name}'"),
                    ));
                }
            }
        }

        result
    }

    /// Validate an object schema.
    fn validate_object(
        &self,
        value: &Value,
        schema: &ObjectSchema,
        path: &str,
    ) -> ValidationResult {
        let mut result = ValidationResult::ok();

        let obj = match value {
            Value::Object(o) => o,
            _ => {
                result.error(ValidationError::new(
                    path,
                    ValidationErrorKind::ExpectedObject,
                    format!("expected object, got {}", value_type_name(value)),
                ));
                return result;
            }
        };

        // Track which schema fields have been seen
        let mut seen_fields: HashSet<&str> = HashSet::new();

        // Check for additional fields schema (key "@")
        let additional_schema = schema.0.get("@");

        // Validate each field in the document
        for entry in &obj.entries {
            let key_str = match &entry.key {
                Value::Scalar(s) => s.text.as_str(),
                Value::Unit => "@",
                _ => {
                    result.error(ValidationError::new(
                        path,
                        ValidationErrorKind::InvalidValue {
                            reason: "object keys must be scalars or unit".into(),
                        },
                        "invalid object key",
                    ));
                    continue;
                }
            };

            let field_path = if path.is_empty() {
                key_str.to_string()
            } else {
                format!("{path}.{key_str}")
            };

            seen_fields.insert(key_str);

            // Look up field in schema
            if let Some(field_schema) = schema.0.get(key_str) {
                result.merge(self.validate_value(&entry.value, field_schema, &field_path));
            } else if let Some(add_schema) = additional_schema {
                // Validate against additional fields schema
                result.merge(self.validate_value(&entry.value, add_schema, &field_path));
            } else {
                // Unknown field and no additional fields allowed
                result.error(ValidationError::new(
                    &field_path,
                    ValidationErrorKind::UnknownField {
                        field: key_str.into(),
                    },
                    format!("unknown field '{key_str}'"),
                ));
            }
        }

        // Check for missing required fields
        for (field_name, field_schema) in &schema.0 {
            if field_name == "@" {
                // Skip the additional fields marker
                continue;
            }

            if !seen_fields.contains(field_name.as_str()) {
                // Check if field is optional
                if !matches!(field_schema, Schema::Optional(_)) {
                    let field_path = if path.is_empty() {
                        field_name.clone()
                    } else {
                        format!("{path}.{field_name}")
                    };
                    result.error(ValidationError::new(
                        &field_path,
                        ValidationErrorKind::MissingField {
                            field: field_name.clone(),
                        },
                        format!("missing required field '{field_name}'"),
                    ));
                }
            }
        }

        result
    }

    /// Validate a sequence schema.
    fn validate_seq(&self, value: &Value, schema: &SeqSchema, path: &str) -> ValidationResult {
        let mut result = ValidationResult::ok();

        let seq = match value {
            Value::Sequence(s) => s,
            _ => {
                result.error(ValidationError::new(
                    path,
                    ValidationErrorKind::ExpectedSequence,
                    format!("expected sequence, got {}", value_type_name(value)),
                ));
                return result;
            }
        };

        // Schema should have exactly one element type
        if schema.0.is_empty() {
            result.error(ValidationError::new(
                path,
                ValidationErrorKind::SchemaError {
                    reason: "seq schema must have element type".into(),
                },
                "invalid seq schema: missing element type",
            ));
            return result;
        }

        let element_schema = &schema.0[0];

        // Validate each element
        for (i, item) in seq.items.iter().enumerate() {
            let item_path = format!("{path}[{i}]");
            result.merge(self.validate_value(item, element_schema, &item_path));
        }

        result
    }

    /// Validate a union schema.
    fn validate_union(&self, value: &Value, schema: &UnionSchema, path: &str) -> ValidationResult {
        let mut result = ValidationResult::ok();

        if schema.0.is_empty() {
            result.error(ValidationError::new(
                path,
                ValidationErrorKind::SchemaError {
                    reason: "union must have at least one variant".into(),
                },
                "invalid union schema: no variants",
            ));
            return result;
        }

        // Try each variant until one succeeds
        let mut tried = Vec::new();
        for variant in &schema.0 {
            let variant_result = self.validate_value(value, variant, path);
            if variant_result.is_valid() {
                return ValidationResult::ok();
            }
            tried.push(schema_type_name(variant));
        }

        // None matched
        result.error(ValidationError::new(
            path,
            ValidationErrorKind::UnionMismatch { tried },
            format!(
                "value doesn't match any union variant (tried: {})",
                schema
                    .0
                    .iter()
                    .map(schema_type_name)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ));

        result
    }

    /// Validate an optional schema.
    fn validate_optional(
        &self,
        value: &Value,
        schema: &OptionalSchema,
        path: &str,
    ) -> ValidationResult {
        // Optional always passes for present values - just validate the inner type
        if schema.0.is_empty() {
            return ValidationResult::ok();
        }

        self.validate_value(value, &schema.0[0], path)
    }

    /// Validate an enum schema.
    fn validate_enum(&self, value: &Value, schema: &EnumSchema, path: &str) -> ValidationResult {
        let mut result = ValidationResult::ok();

        // Value should be a tagged value
        let (tag, payload) = match value {
            Value::Tagged(t) => (t.tag.as_str(), t.payload.as_deref()),
            Value::Unit => {
                // Unit can be a variant name if there's a unit variant in the enum
                // This is a bit unusual - typically enums are @variant or @variant{...}
                result.error(ValidationError::new(
                    path,
                    ValidationErrorKind::ExpectedTagged,
                    "expected tagged value for enum",
                ));
                return result;
            }
            _ => {
                result.error(ValidationError::new(
                    path,
                    ValidationErrorKind::ExpectedTagged,
                    format!(
                        "expected tagged value for enum, got {}",
                        value_type_name(value)
                    ),
                ));
                return result;
            }
        };

        // Look up the variant
        let expected_variants: Vec<String> = schema.0.keys().cloned().collect();

        match schema.0.get(tag) {
            Some(variant_schema) => {
                // Validate the payload against the variant schema
                match (payload, variant_schema) {
                    (None, Schema::Type { name: Some(n) }) if n == "unit" => {
                        // @variant with unit payload schema - OK
                    }
                    (None, Schema::Type { name: None }) => {
                        // @variant with @ schema - OK (unit)
                    }
                    (Some(p), _) => {
                        let variant_path = if path.is_empty() {
                            tag.to_string()
                        } else {
                            format!("{path}.{tag}")
                        };
                        result.merge(self.validate_value(p, variant_schema, &variant_path));
                    }
                    (None, _) => {
                        result.error(ValidationError::new(
                            path,
                            ValidationErrorKind::TypeMismatch {
                                expected: schema_type_name(variant_schema),
                                got: "unit".into(),
                            },
                            format!("variant '{tag}' requires a payload"),
                        ));
                    }
                }
            }
            None => {
                result.error(ValidationError::new(
                    path,
                    ValidationErrorKind::InvalidVariant {
                        expected: expected_variants,
                        got: tag.into(),
                    },
                    format!(
                        "unknown enum variant '{tag}' (expected one of: {})",
                        schema.0.keys().cloned().collect::<Vec<_>>().join(", ")
                    ),
                ));
            }
        }

        result
    }

    /// Validate a map schema.
    fn validate_map(&self, value: &Value, schema: &MapSchema, path: &str) -> ValidationResult {
        let mut result = ValidationResult::ok();

        let obj = match value {
            Value::Object(o) => o,
            _ => {
                result.error(ValidationError::new(
                    path,
                    ValidationErrorKind::ExpectedObject,
                    format!("expected map (object), got {}", value_type_name(value)),
                ));
                return result;
            }
        };

        // Map schema has 1 element (value type) or 2 elements (key type, value type)
        let (key_schema, value_schema) = match schema.0.len() {
            1 => (None, &schema.0[0]),
            2 => (Some(&schema.0[0]), &schema.0[1]),
            _ => {
                result.error(ValidationError::new(
                    path,
                    ValidationErrorKind::SchemaError {
                        reason: "map schema must have 1 or 2 type arguments".into(),
                    },
                    "invalid map schema",
                ));
                return result;
            }
        };

        // Validate each entry
        for entry in &obj.entries {
            let key_str = match &entry.key {
                Value::Scalar(s) => s.text.as_str(),
                _ => {
                    result.error(ValidationError::new(
                        path,
                        ValidationErrorKind::InvalidValue {
                            reason: "map keys must be scalars".into(),
                        },
                        "invalid map key",
                    ));
                    continue;
                }
            };

            // Validate key if key schema specified
            if let Some(ks) = key_schema {
                result.merge(self.validate_value(&entry.key, ks, path));
            }

            // Validate value
            let entry_path = if path.is_empty() {
                key_str.to_string()
            } else {
                format!("{path}.{key_str}")
            };
            result.merge(self.validate_value(&entry.value, value_schema, &entry_path));
        }

        result
    }

    /// Validate a flatten schema.
    fn validate_flatten(
        &self,
        value: &Value,
        schema: &FlattenSchema,
        path: &str,
    ) -> ValidationResult {
        let mut result = ValidationResult::ok();

        // Flatten references another type - resolve and validate
        // The schema is stored as a vec with the type reference
        if schema.0.is_empty() {
            result.error(ValidationError::new(
                path,
                ValidationErrorKind::SchemaError {
                    reason: "flatten schema must reference a type".into(),
                },
                "invalid flatten schema",
            ));
            return result;
        }

        // Validate against the referenced type
        result.merge(self.validate_value(value, &schema.0[0], path));

        result
    }
}

/// Get a human-readable name for a value type.
fn value_type_name(value: &Value) -> &'static str {
    match value {
        Value::Scalar(_) => "scalar",
        Value::Unit => "unit",
        Value::Tagged(_) => "tagged",
        Value::Sequence(_) => "sequence",
        Value::Object(_) => "object",
    }
}

/// Get a human-readable name for a schema type.
fn schema_type_name(schema: &Schema) -> String {
    match schema {
        Schema::Literal(s) => format!("literal({s})"),
        Schema::Type { name: None } => "unit".into(),
        Schema::Type { name: Some(n) } => n.clone(),
        Schema::Object(_) => "object".into(),
        Schema::Seq(_) => "seq".into(),
        Schema::Union(_) => "union".into(),
        Schema::Optional(_) => "optional".into(),
        Schema::Enum(_) => "enum".into(),
        Schema::Map(_) => "map".into(),
        Schema::Flatten(_) => "flatten".into(),
    }
}

/// Convenience function to validate a document against a schema.
pub fn validate(doc: &Value, schema: &SchemaFile) -> ValidationResult {
    let validator = Validator::new(schema);
    validator.validate_document(doc)
}

/// Convenience function to validate a value against a named type.
pub fn validate_as(value: &Value, schema: &SchemaFile, type_name: &str) -> ValidationResult {
    let validator = Validator::new(schema);
    validator.validate_as_type(value, type_name)
}
