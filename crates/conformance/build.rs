//! Translates the synthetic test programs into Rust at build time.

#[path = "src/asm.rs"]
#[allow(dead_code)]
mod asm;
#[path = "src/programs.rs"]
#[allow(dead_code)]
mod programs;

use std::fmt::Write;

fn main() {
    println!("cargo:rerun-if-changed=src/asm.rs");
    println!("cargo:rerun-if-changed=src/programs.rs");
    let mut out = String::new();
    let mut table =
        String::from("pub static PROGRAMS: &[(&str, fn(&mut ::n64rusty_runtime::Ctx))] = &[\n");
    for (n, p) in programs::all().iter().enumerate() {
        let funcs: Vec<_> = p
            .funcs
            .iter()
            .map(|(addr, words)| n64rusty_translator::Function {
                addr: *addr,
                words: words.clone(),
            })
            .collect();
        let src = n64rusty_translator::translate_module(&funcs)
            .unwrap_or_else(|e| panic!("translating {}: {e}", p.name));
        writeln!(out, "pub mod p{n} {{\n{src}}}\n").unwrap();
        writeln!(table, "    ({:?}, p{n}::register),", p.name).unwrap();
    }
    table.push_str("];\n");
    out.push_str(&table);
    let path = std::path::Path::new(&std::env::var("OUT_DIR").unwrap()).join("generated.rs");
    std::fs::write(path, out).unwrap();
}
