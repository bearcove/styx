# Handoff: Binary Schema Embedding & Extraction for Styx

## Completed
- Created `styx-embed` crate with `embed_inline!`, `embed_file!`, `embed_files!` macros
- Created `styx-embed-macros` proc macro crate using unsynn 0.3 (not syn!)
- Binary format: `STYX_SCHEMAS_V1\0` + count(u16) + [decompressed_len(u32) + compressed_len(u32) + blake3(32) + lz4_data]...
- Extraction works in both debug and release mode (scans for magic, tries all occurrences)
- Tests pass: `cargo nextest run -p styx-embed --lib`
- Examples work: `cargo run -p styx-embed --example roundtrip --features mmap`
- GitHub issue #4 updated with binary extraction design

## Active Work

### Origin
User asked:
> "hey do we support everything we say we support in @docs/content/tools/schema-distribution.md?"
> "the cli part. do we do that? where do we cache schemas? do we expose them over LSP so people can still jump to definition if the schema is provided by a CLI?"

Then evolved to:
> "we can have binaries export schemas as strings without even calling them"
> "we'd just scan the binary and look for well-known patterns"

This led to creating `styx-embed` for zero-execution schema discovery.

### The Problem
I was adding `generate_schema::<T>()` to facet-styx for build script use. The docs show:
```rust
// build.rs
fn main() {
    facet_styx::generate_schema::<MyConfig>("schema.styx");
}

// src/main.rs
styx_embed::embed_file!(concat!(env!("OUT_DIR"), "/schema.styx"));
```

I created `crates/facet-styx/src/schema_gen.rs` but **I ACCIDENTALLY OVERWROTE IT** with a stub when trying to fix compile errors. The original had ~170 lines of code walking facet's `Shape` and `Def` types to generate Styx schema syntax.

### Current State
- Branch: `main` (no branch created yet)
- No PR yet
- Issue: #4 (https://github.com/bearcove/styx/issues/4)

**What's working:**
- `styx-embed` crate is complete and functional
- `styx-embed-macros` proc macro works with unsynn 0.3

**What's broken:**
- `facet-styx/src/schema_gen.rs` was overwritten with a broken stub
- The stub references `shape.type_name` incorrectly (it's a function, not a string)
- Build fails: `cargo build -p facet-styx`

### Technical Context

The facet API for introspection:
- `T::SHAPE` gives you a `&'static Shape`
- `Shape.ty` is `Type` enum: `Type::Primitive(PrimitiveType)`, `Type::User(UserType)`, etc.
- `Shape.def` is `Def` enum: `Def::Scalar`, `Def::Struct(StructDef)`, `Def::Option(OptionDef)`, etc.
- `StructDef.fields` gives you fields with `.name`, `.shape()`, `.doc`
- Type name is NOT `shape.name` - there's no such field. Use `shape.type_name` which is a `TypeNameFn` that needs to be called with a formatter

The original schema_gen.rs had:
```rust
fn generate_type_schema(output: &mut String, shape: &facet_core::Shape, indent: &str) {
    match &shape.def {
        Def::Struct(struct_def) => {
            for field in struct_def.fields {
                // field.name, field.doc, field.shape()
            }
        }
        Def::Option(opt_def) => { /* opt_def.t */ }
        Def::List(list_def) => { /* list_def.t */ }
        // etc
    }
}

fn map_shape_to_styx(shape: &facet_core::Shape) -> String {
    match &shape.def {
        Def::Scalar => match shape.ty {
            Type::Primitive(PrimitiveType::String) => "@string",
            Type::Primitive(PrimitiveType::Bool) => "@bool",
            // etc
        }
    }
}
```

Facet source is at: `/Users/amos/.cargo/git/checkouts/facet-2961151dee48b078/6ed2939/facet-core/src/types/`
- `shape.rs` - Shape struct
- `def/mod.rs` - Def enum
- `ty/mod.rs` - Type enum with Primitive, User, Sequence, Pointer variants
- `ty/struct_.rs` - StructDef, Field
- `ty/primitive.rs` - PrimitiveType enum

### Success Criteria
1. `cargo build -p facet-styx` passes
2. `facet_styx::generate_schema::<T>("schema.styx")` writes a valid schema to OUT_DIR
3. `facet_styx::schema_from_type::<T>()` returns a Styx schema string
4. Schema includes: meta block with type name, struct fields, nested types, Option/Vec/Map handling
5. Doc comments from Facet types appear in schema

### Files to Touch
- `crates/facet-styx/src/schema_gen.rs` - **RESTORE AND FIX** - currently broken stub
- `crates/facet-styx/src/lib.rs:56` - exports `generate_schema`, `schema_from_type` (already done)

### Decisions Made
- Use unsynn 0.3 for proc macros, NOT syn (user explicitly said "no syn please, unsynn only")
- Binary format uses LZ4 + BLAKE3 for compression and integrity
- `embed_inline!` for literal strings, `embed_file!` for reading from disk
- Build script pattern for type-derived schemas (keeps them in sync automatically)

### What NOT to Do
- Don't use syn - user explicitly rejected it
- Don't execute binaries to get schemas - that was the old approach, we scan instead
- The `@dump-styx-schema` CLI approach in the docs is obsolete - we do binary scanning now

### Blockers/Gotchas
- Facet's `Shape.type_name` is NOT a string - it's `Option<TypeNameFn>` which is a function pointer
- Facet uses nested enums: `Type::Primitive(PrimitiveType::Bool)` not `Type::Bool`
- `Def::Struct` doesn't exist - it might be under `Type::User(UserType::Struct(StructType))`
- Check the actual facet source, my assumptions about the API were wrong

## Bootstrap
```bash
# Check current state
cargo build -p facet-styx 2>&1 | head -30

# Once fixed, test
cargo nextest run -p styx-embed --lib
cargo run -p styx-embed --example roundtrip --features mmap

# Look at facet types
cat /Users/amos/.cargo/git/checkouts/facet-2961151dee48b078/6ed2939/facet-core/src/types/def/mod.rs | head -100
```
