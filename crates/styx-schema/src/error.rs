//! Validation error types.

use styx_parse::Span;

/// Result of validating a document against a schema.
#[derive(Debug, Clone)]
pub struct ValidationResult {
    /// Validation errors (must be empty for validation to pass).
    pub errors: Vec<ValidationError>,
    /// Validation warnings (non-fatal issues).
    pub warnings: Vec<ValidationWarning>,
}

impl ValidationResult {
    /// Create an empty (passing) result.
    pub fn ok() -> Self {
        Self {
            errors: Vec::new(),
            warnings: Vec::new(),
        }
    }

    /// Check if validation passed (no errors).
    pub fn is_valid(&self) -> bool {
        self.errors.is_empty()
    }

    /// Add an error.
    pub fn error(&mut self, error: ValidationError) {
        self.errors.push(error);
    }

    /// Add a warning.
    pub fn warning(&mut self, warning: ValidationWarning) {
        self.warnings.push(warning);
    }

    /// Merge another result into this one.
    pub fn merge(&mut self, other: ValidationResult) {
        self.errors.extend(other.errors);
        self.warnings.extend(other.warnings);
    }
}

/// A validation error.
#[derive(Debug, Clone)]
pub struct ValidationError {
    /// Path to the error location (e.g., "server.tls.cert").
    pub path: String,
    /// Source span in the document.
    pub span: Option<Span>,
    /// Error kind.
    pub kind: ValidationErrorKind,
    /// Human-readable message.
    pub message: String,
}

impl ValidationError {
    /// Create a new validation error.
    pub fn new(
        path: impl Into<String>,
        kind: ValidationErrorKind,
        message: impl Into<String>,
    ) -> Self {
        Self {
            path: path.into(),
            span: None,
            kind,
            message: message.into(),
        }
    }

    /// Set the span.
    pub fn with_span(mut self, span: Option<Span>) -> Self {
        self.span = span;
        self
    }
}

impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.path.is_empty() {
            write!(f, "{}", self.message)
        } else {
            write!(f, "{}: {}", self.path, self.message)
        }
    }
}

impl std::error::Error for ValidationError {}

/// Kinds of validation errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidationErrorKind {
    /// Missing required field in object.
    MissingField { field: String },
    /// Unknown field in object (when additional fields not allowed).
    UnknownField { field: String },
    /// Type mismatch.
    TypeMismatch { expected: String, got: String },
    /// Invalid value for type.
    InvalidValue { reason: String },
    /// Unknown type reference in schema.
    UnknownType { name: String },
    /// Invalid enum variant.
    InvalidVariant { expected: Vec<String>, got: String },
    /// Union match failed (value didn't match any variant).
    UnionMismatch { tried: Vec<String> },
    /// Expected object, got something else.
    ExpectedObject,
    /// Expected sequence, got something else.
    ExpectedSequence,
    /// Expected scalar, got something else.
    ExpectedScalar,
    /// Expected tagged value.
    ExpectedTagged,
    /// Wrong tag name.
    WrongTag { expected: String, got: String },
    /// Schema error (invalid schema definition).
    SchemaError { reason: String },
}

/// A validation warning (non-fatal).
#[derive(Debug, Clone)]
pub struct ValidationWarning {
    /// Path to the warning location.
    pub path: String,
    /// Source span in the document.
    pub span: Option<Span>,
    /// Warning kind.
    pub kind: ValidationWarningKind,
    /// Human-readable message.
    pub message: String,
}

impl ValidationWarning {
    /// Create a new validation warning.
    pub fn new(
        path: impl Into<String>,
        kind: ValidationWarningKind,
        message: impl Into<String>,
    ) -> Self {
        Self {
            path: path.into(),
            span: None,
            kind,
            message: message.into(),
        }
    }
}

/// Kinds of validation warnings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidationWarningKind {
    /// Deprecated field or type.
    Deprecated { reason: String },
    /// Field will be ignored.
    IgnoredField { field: String },
}
