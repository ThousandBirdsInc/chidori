//! Dev-only bytecode disassembler: `cargo run --example dump_ops -- file.js`
//! Prints each compiled function's ops with indices, for inspecting what the
//! kernel pass will see. Not part of the public surface.

fn dump(proto: &chidori_js::bytecode::FuncProto, depth: usize) {
    let pad = "  ".repeat(depth);
    println!(
        "{pad}fn {:?} locals={} cells={} strict={}",
        proto.name, proto.num_locals, proto.num_cells, proto.is_strict
    );
    for (i, op) in proto.code.iter().enumerate() {
        println!("{pad}  {i:4}  {op:?}");
    }
    for c in &proto.consts {
        if let chidori_js::bytecode::Const::Func(p) = c {
            dump(p, depth + 1);
        }
    }
}

fn main() {
    let path = std::env::args().nth(1).expect("usage: dump_ops <file.js>");
    let src = std::fs::read_to_string(&path).unwrap();
    let proto = chidori_js::compiler::compile_script(&src).unwrap();
    dump(&proto, 0);
}
