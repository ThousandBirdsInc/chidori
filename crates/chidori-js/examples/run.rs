use chidori_js::Engine;

// glibc malloc is 20%+ of executed instructions on allocation-heavy workloads
// (json_roundtrip, string_build); mimalloc halves the allocator's share. Kept
// behind a feature so the default build stays free of C code — the benchmark
// harness (benchmarks/run.mjs) builds with `--features mimalloc`.
#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() {
    let src = std::fs::read_to_string(std::env::args().nth(1).unwrap()).unwrap();
    let mut e = Engine::new();
    match e.eval(&src) {
        Ok(v) => {
            for l in e.console() {
                println!("{l}");
            }
            println!("=> {}", e.vm.to_string_lossy(&v));
        }
        Err(err) => {
            for l in e.console() {
                println!("{l}");
            }
            println!("ERROR: {err}");
        }
    }
}
