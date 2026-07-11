//! Dev-only bytecode disassembler: `cargo run --example dump_ops -- file.js`
//! Prints each compiled function's ops with indices, for inspecting what the
//! kernel pass will see — and, with `--kernels`, the translated loop/function
//! kernels' KOps themselves. Not part of the public surface.

fn dump(proto: &chidori_js::bytecode::FuncProto, depth: usize, kernels: bool) {
    let pad = "  ".repeat(depth);
    println!(
        "{pad}fn {:?} locals={} cells={} strict={}",
        proto.name, proto.num_locals, proto.num_cells, proto.is_strict
    );
    for (i, op) in proto.code.iter().enumerate() {
        println!("{pad}  {i:4}  {op:?}");
    }
    if kernels {
        for (ki, k) in proto.kernels.iter().enumerate() {
            println!(
                "{pad}  loop kernel {ki}: n_regs={} locals={:?} oslots={:?}",
                k.n_regs, k.locals, k.oslots
            );
            for (i, op) in k.code.iter().enumerate() {
                println!("{pad}    {i:4}  {op:?}");
            }
        }
        if let Some(k) = &proto.fn_kernel {
            println!(
                "{pad}  fn kernel: n_regs={} locals={:?} uv_writes={:?}",
                k.n_regs, k.locals, k.uv_writes
            );
            for (i, op) in k.code.iter().enumerate() {
                println!("{pad}    {i:4}  {op:?}");
            }
        }
    }
    for c in &proto.consts {
        if let chidori_js::bytecode::Const::Func(p) = c {
            dump(p, depth + 1, kernels);
        }
    }
}

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let kernels = args.iter().any(|a| a == "--kernels");
    args.retain(|a| a != "--kernels");
    let path = args.first().expect("usage: dump_ops [--kernels] <file.js>");
    let src = std::fs::read_to_string(path).unwrap();
    let proto = if kernels {
        chidori_js::compiler::compile_script_kernels(&src, true).unwrap()
    } else {
        chidori_js::compiler::compile_script(&src).unwrap()
    };
    dump(&proto, 0, kernels);
}
