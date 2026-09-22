//! Writes a plugin the host will load, for tests that need one as a file.
//!
//! The api suite uploads wasm over http, so it needs a module on disk rather than a wat
//! string. Emitting it here keeps a binary out of the repository and keeps what is uploaded
//! the same module the host's own tests use.
//!
//! `cargo run -q -p ip-plugin --example emit_plugin -- <path>`

use std::path::PathBuf;

/// A response body plugin that hands back what it was given, with the abi the host checks for.
const PLUGIN: &str = r#"(module
  (memory (export "memory") 1)
  (func (export "alloc") (param i32) (result i32) (i32.const 1024))
  (func (export "dealloc") (param i32 i32))
  (func (export "transform") (param i32 i32) (result i64)
    (i64.or
      (i64.shl (i64.extend_i32_u (local.get 0)) (i64.const 32))
      (i64.extend_i32_u (local.get 1)))))"#;

fn main() {
    let Some(path) = std::env::args().nth(1).map(PathBuf::from) else {
        eprintln!("usage: emit_plugin <path>");
        std::process::exit(2);
    };
    let wasm = wat::parse_str(PLUGIN).expect("the plugin is wat the parser accepts");
    std::fs::write(&path, wasm).expect("the plugin could be written");
    println!("{}", path.display());
}
