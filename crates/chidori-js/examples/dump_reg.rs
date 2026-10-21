//! Dump the REGISTER-tier bytecode (`FuncProto::reg`) of every function in a
//! script, plus a static count of adjacent register-op pairs — the
//! superinstruction-candidate view for the tier glue code actually runs on
//! (`examples/opstats.rs` covers the stack tier).
//!
//! Run: `cargo run -q --release --example dump_reg -p chidori-js -- file.js`
use std::collections::HashMap;

use chidori_js::bytecode::{Const, FuncProto};

fn name_of(op: &chidori_js::reg::ROp) -> String {
    let s = format!("{op:?}");
    s.split([' ', '{', '('])
        .next()
        .unwrap_or("")
        .to_string()
}

fn walk(
    proto: &FuncProto,
    depth: usize,
    pairs: &mut HashMap<(String, String), u64>,
    ops: &mut HashMap<String, u64>,
) {
    let pad = "  ".repeat(depth);
    match &proto.reg {
        Some(reg) => {
            println!(
                "{pad}fn {:?} [register tier: {} ops]",
                proto.name,
                reg.code.len()
            );
            let mut prev: Option<String> = None;
            for (i, op) in reg.code.iter().enumerate() {
                println!("{pad}  {i:4}  {op:?}");
                let n = name_of(op);
                *ops.entry(n.clone()).or_insert(0) += 1;
                if let Some(p) = prev {
                    *pairs.entry((p, n.clone())).or_insert(0) += 1;
                }
                prev = Some(n);
            }
        }
        None => println!("{pad}fn {:?} [stack tier only]", proto.name),
    }
    for c in &proto.consts {
        if let Const::Func(p) = c {
            walk(p, depth + 1, pairs, ops);
        }
    }
}

fn main() {
    let path = std::env::args().nth(1).expect("usage: dump_reg <file.js>");
    let src = std::fs::read_to_string(&path).expect("read");
    let proto = chidori_js::compiler::compile_script(&src).expect("compile");
    let mut pairs = HashMap::new();
    let mut ops = HashMap::new();
    walk(&proto, 0, &mut pairs, &mut ops);
    let mut ops: Vec<_> = ops.into_iter().collect();
    ops.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    println!("\n== static register-op counts");
    for (n, c) in ops.iter().take(30) {
        println!("{c:6}  {n}");
    }
    let mut pairs: Vec<_> = pairs.into_iter().collect();
    pairs.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    println!("\n== static adjacent pairs");
    for ((a, b), c) in pairs.iter().take(30) {
        println!("{c:6}  {a} -> {b}");
    }
}
