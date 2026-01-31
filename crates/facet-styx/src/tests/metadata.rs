use super::super::*;
use facet::Facet;
use facet_reflect::Span;
use facet_testhelpers::test;

struct ParseTest<'a> {
    source: &'a str,
}

impl<'a> ParseTest<'a> {
    fn new<T: Facet<'static>>(source: &'a str, f: impl FnOnce(&Self, T)) {
        let test = Self { source };
        let parsed: T = from_str(source).unwrap();
        f(&test, parsed);
    }

    #[track_caller]
    fn assert_is<T, E>(&self, meta: &WithMeta<T>, expected: E, span_text: &str)
    where
        T: PartialEq + std::fmt::Debug,
        E: Into<T>,
    {
        assert_eq!(meta.value, expected.into(), "value mismatch");
        let span = meta.span.expect("expected span to be present");
        let actual = &self.source[span.offset as usize..(span.offset + span.len) as usize];
        assert_eq!(actual, span_text, "span mismatch");
    }
}

/// A metadata container that captures both span and doc metadata.
///
/// This is useful for validation errors that need to point back to source locations,
/// while also preserving doc comments.
#[derive(Debug, Clone, Facet)]
#[facet(metadata_container)]
pub struct WithMeta<T> {
    pub value: T,
    #[facet(metadata = "span")]
    pub span: Option<Span>,
    #[facet(metadata = "doc")]
    pub doc: Option<Vec<String>>,
}

impl<T: PartialEq> PartialEq for WithMeta<T> {
    fn eq(&self, other: &Self) -> bool {
        self.value == other.value
    }
}

impl<T: Eq> Eq for WithMeta<T> {}

impl<T: std::hash::Hash> std::hash::Hash for WithMeta<T> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.value.hash(state);
    }
}

/// Reference test demonstrating the `ParseTest` harness conventions:
///
/// - Always use raw string literals (`r#"..."#`) for source input
/// - Always use actual newlines, never `\n` escapes
/// - Use `ParseTest::new(source, |t, parsed| { ... })` to parse and test
/// - Use `t.assert_is(&field, expected_value, "span_text")` to check both value and span
/// - For strings, `expected_value` can be `&str` (converts via `Into`)
/// - For integers, suffix literals to match the type (e.g., `8080u16`)
#[test]
fn test_spanned_doc_as_struct_field() {
    #[derive(Facet, Debug)]
    struct Config {
        name: WithMeta<String>,
        port: WithMeta<u16>,
    }

    ParseTest::new(
        r#"
name myapp
port 8080
"#,
        |t, c: Config| {
            t.assert_is(&c.name, "myapp", "myapp");
            t.assert_is(&c.port, 8080u16, "8080");
        },
    );
}

#[test]
fn test_spanned_doc_as_struct_field_with_docs() {
    #[derive(Facet, Debug)]
    struct Config {
        name: WithMeta<String>,
    }

    ParseTest::new(
        r#"
/// The application name
name myapp
"#,
        |t, c: Config| {
            t.assert_is(&c.name, "myapp", "myapp");
            assert!(c.name.doc.is_some());
        },
    );
}

#[test]
fn test_spanned_doc_as_map_value() {
    use indexmap::IndexMap;

    #[derive(Facet, Debug)]
    struct Config {
        #[facet(flatten)]
        items: IndexMap<String, WithMeta<String>>,
    }

    ParseTest::new(
        r#"
foo bar
baz qux
"#,
        |t, c: Config| {
            assert_eq!(c.items.len(), 2);
            t.assert_is(c.items.get("foo").unwrap(), "bar", "bar");
            t.assert_is(c.items.get("baz").unwrap(), "qux", "qux");
        },
    );
}

#[test]
fn test_spanned_doc_as_map_key() {
    use indexmap::IndexMap;

    #[derive(Facet, Debug)]
    struct Config {
        #[facet(flatten)]
        items: IndexMap<WithMeta<String>, String>,
    }

    ParseTest::new(
        r#"
foo bar
baz qux
"#,
        |t, c: Config| {
            assert_eq!(c.items.len(), 2);
            let keys: Vec<_> = c.items.keys().collect();
            t.assert_is(keys[0], "foo", "foo");
            t.assert_is(keys[1], "baz", "baz");
        },
    );
}

#[test]
fn test_spanned_doc_as_map_key_and_value() {
    use indexmap::IndexMap;

    #[derive(Facet, Debug)]
    struct Config {
        #[facet(flatten)]
        items: IndexMap<WithMeta<String>, WithMeta<String>>,
    }

    ParseTest::new(
        r#"
foo bar
baz qux
"#,
        |t, c: Config| {
            assert_eq!(c.items.len(), 2);
            let (key, val) = c.items.get_index(0).unwrap();
            t.assert_is(key, "foo", "foo");
            t.assert_is(val, "bar", "bar");
            let (key, val) = c.items.get_index(1).unwrap();
            t.assert_is(key, "baz", "baz");
            t.assert_is(val, "qux", "qux");
        },
    );
}

#[test]
fn test_spanned_doc_in_array() {
    #[derive(Facet, Debug)]
    struct Config {
        items: Vec<WithMeta<String>>,
    }

    ParseTest::new(
        r#"
items (alpha beta gamma)
"#,
        |t, c: Config| {
            assert_eq!(c.items.len(), 3);
            t.assert_is(&c.items[0], "alpha", "alpha");
            t.assert_is(&c.items[1], "beta", "beta");
            t.assert_is(&c.items[2], "gamma", "gamma");
        },
    );
}

#[test]
fn test_spanned_doc_in_nested_struct() {
    #[derive(Facet, Debug)]
    struct Inner {
        value: WithMeta<i32>,
    }

    #[derive(Facet, Debug)]
    struct Outer {
        inner: Inner,
    }

    ParseTest::new(
        r#"
inner { value 42 }
"#,
        |t, c: Outer| {
            t.assert_is(&c.inner.value, 42, "42");
        },
    );
}

#[test]
fn test_spanned_doc_with_option_present() {
    #[derive(Facet, Debug)]
    struct Config {
        name: Option<WithMeta<String>>,
    }

    ParseTest::new(
        r#"
name hello
"#,
        |t, c: Config| {
            t.assert_is(c.name.as_ref().unwrap(), "hello", "hello");
        },
    );
}

#[test]
fn test_spanned_doc_with_option_absent() {
    #[derive(Facet, Debug)]
    struct Config {
        name: Option<WithMeta<String>>,
        other: String,
    }

    ParseTest::new(
        r#"
other world
"#,
        |_t, c: Config| {
            assert!(c.name.is_none());
            assert_eq!(c.other, "world");
        },
    );
}

#[test]
fn test_spanned_doc_with_integers() {
    #[derive(Facet, Debug)]
    struct Numbers {
        a: WithMeta<i32>,
        b: WithMeta<u64>,
        c: WithMeta<i8>,
    }

    ParseTest::new(
        r#"
a -42
b 999
c 127
"#,
        |t, c: Numbers| {
            t.assert_is(&c.a, -42, "-42");
            t.assert_is(&c.b, 999u64, "999");
            t.assert_is(&c.c, 127i8, "127");
        },
    );
}

#[test]
fn test_spanned_doc_with_booleans() {
    #[derive(Facet, Debug)]
    struct Flags {
        enabled: WithMeta<bool>,
        debug: WithMeta<bool>,
    }

    ParseTest::new(
        r#"
enabled true
debug false
"#,
        |t, c: Flags| {
            t.assert_is(&c.enabled, true, "true");
            t.assert_is(&c.debug, false, "false");
        },
    );
}

#[test]
fn test_spanned_doc_in_flattened_map_inline() {
    use indexmap::IndexMap;

    #[derive(Facet, Debug)]
    struct Config {
        #[facet(flatten)]
        items: IndexMap<WithMeta<String>, WithMeta<String>>,
    }

    ParseTest::new(
        r#"
{foo bar, baz qux}
"#,
        |t, c: Config| {
            assert_eq!(c.items.len(), 2);
            let (key, val) = c.items.get_index(0).unwrap();
            t.assert_is(key, "foo", "foo");
            t.assert_is(val, "bar", "bar");
            let (key, val) = c.items.get_index(1).unwrap();
            t.assert_is(key, "baz", "baz");
            t.assert_is(val, "qux", "qux");
        },
    );
}
