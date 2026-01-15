//! Generate the meta-schema from Rust types.
//!
//! Run with: cargo run -p styx-schema --bin gen-meta-schema > schema/meta.gen.styx

use styx_schema::{SchemaFile, to_styx_schema};

fn main() {
    let schema = to_styx_schema::<SchemaFile>("@");
    print!("{}", schema);
}
