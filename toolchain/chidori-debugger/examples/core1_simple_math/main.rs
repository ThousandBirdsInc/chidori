use std::path::Path;
use chidori_core::sdk::chidori::Chidori;
fn main() {
    let current_file = env!("CARGO_MANIFEST_DIR");
    let current_file_path = Path::new(current_file);
    let relative_path = current_file_path.join("./");
    let mut env = Chidori::new();
    env.load_md_directory(&relative_path);
    let mut s = env.get_instance().unwrap();
    s.run();
}