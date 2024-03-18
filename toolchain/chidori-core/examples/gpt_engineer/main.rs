use std::path::Path;
use chidori_core::sdk::entry::{Chidori};
fn main() {
    let current_file = env!("CARGO_MANIFEST_DIR");
    let current_file_path = Path::new(current_file);
    let relative_path = current_file_path.join("./");

    let mut env = Chidori::new();
    env.load(&relative_path);
    env.state.render_dependency_graph();
    env.run();
}