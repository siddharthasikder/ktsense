//! Renders the extracted skeleton for one or more files, so extraction can be eyeballed against
//! the source before a snapshot is committed.
//!
//! Run with: cargo run -p ktsense-syntax --example outline_file -- [--private] <file>...

use std::env;
use std::fs;

use ktsense_core::{render_markdown, RenderOptions};

fn main() -> anyhow::Result<()> {
    let mut options = RenderOptions::default().with_doc();
    let mut paths = Vec::new();
    for argument in env::args().skip(1) {
        match argument.as_str() {
            "--private" => options = options.with_private(),
            "--lines" => options = options.with_lines(),
            _ => paths.push(argument),
        }
    }

    for path in paths {
        let source = fs::read_to_string(&path)?;
        let skeleton = ktsense_syntax::extract(&path, &source)?;
        print!("{}", render_markdown(&skeleton, &options));
        println!();
    }
    Ok(())
}
