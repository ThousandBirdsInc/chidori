use chidori_js::Engine;
fn main() {
    let src = std::fs::read_to_string(std::env::args().nth(1).unwrap()).unwrap();
    let mut e = Engine::new();
    match e.eval(&src) {
        Ok(v) => { for l in e.console() { println!("{l}"); } println!("=> {}", e.vm.to_string_lossy(&v)); }
        Err(err) => { for l in e.console() { println!("{l}"); } println!("ERROR: {err}"); }
    }
}
