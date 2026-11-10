//! Writes the wire fixtures from the code of the tree it is built in. It exists to run in a worktree of tag
//! `v1.0.0`, never in a newer tree: the fixtures are the bytes that 1.0 produced.
//!
//! In a worktree of the tag, copy the folder `crates/swapdesk-lan/tests/wire_samples/` into the same place and
//! this file to `crates/swapdesk-lan/examples/generate.rs` (the `#[path]` below finds the table from there),
//! change nothing else, and run
//!
//! ```text
//! cargo run -p swapdesk-lan --example generate -- <output folder>
//! ```
//!
//! then copy the output folder over `tests/fixtures/v1.0.0/` of the current tree.

#[path = "../tests/wire_samples/mod.rs"]
mod wire_samples;

use std::collections::HashSet;
use std::path::PathBuf;
use std::{env, fs};

use wire_samples::{file_name, index_text, samples, to_hex_text};

fn main() {
    let out = PathBuf::from(env::args().nth(1).expect("usage: generate <output folder>"));
    fs::create_dir_all(&out).expect("create the output folder");
    let samples = samples();
    let mut names = HashSet::new();
    for s in &samples {
        assert!(names.insert(s.name.clone()), "duplicate sample name {}", s.name);
        let bytes = s.item.encode().unwrap_or_else(|e| panic!("{}: {e}", s.name));
        // The code that wrote the bytes must read them back into the sample.
        let back = s.item.decode_like(&bytes).unwrap_or_else(|e| panic!("{}: {e}", s.name));
        assert_eq!(back, s.item, "{} does not round-trip", s.name);
        fs::write(out.join(file_name(&s.name)), to_hex_text(&bytes)).expect("write a fixture");
    }
    fs::write(out.join("INDEX.txt"), index_text(&samples)).expect("write the index");
    println!("{} fixtures written", samples.len());
}
