use chidori_js::Engine;

// Allocator experiment knob, off by default — measured net-negative on this
// suite (fewer instructions on alloc-heavy workloads, but ~9% slower
// wall-clock geomean; see the `mimalloc` feature comment in Cargo.toml).
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
