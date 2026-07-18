//! Deterministic-instruction benchmark runner for the shapes work: runs one
//! named workload from the shared bench corpus once (compile excluded from
//! the interesting region is not needed for callgrind ratios).
#[path = "../benches/common/workloads.rs"]
mod workloads;

fn main() {
    let name = std::env::args().nth(1).expect("workload name");
    let (_, src) = workloads::WORKLOADS
        .iter()
        .find(|(n, _)| *n == name)
        .expect("unknown workload");
    let mut e = chidori_js::Engine::new();
    let v = e.eval(src).expect("workload must not throw");
    println!("{name}: {:?}", v);
}
