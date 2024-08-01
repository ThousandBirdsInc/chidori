use dirs::home_dir;
use std::env;
use std::io::{self, Write};
use std::path::PathBuf;
use std::str::FromStr;
use rye::lock::LockOptions;
use target_lexicon::{OperatingSystem, Triple};
use rye::sync::{sync, SyncMode, SyncOptions};
use rye::utils::CommandOutput;

fn install_python() -> Result<(), anyhow::Error> {
    rye::platform::init();
    sync(SyncOptions {
        output: CommandOutput::Quiet,
        dev: true,
        mode: SyncMode::PythonOnly,
        force: false,
        no_lock: false,
        lock_options: LockOptions::default(),
        pyproject: Some(PathBuf::from("./pyproject.toml")),
    })
}

fn add_extension_module_link_args(triple: &Triple) -> io::Result<()> {
    let mut writer = io::stdout();
    match triple.operating_system {
        OperatingSystem::Darwin => {
            writeln!(writer, "cargo:rustc-cdylib-link-arg=-undefined")?;
            writeln!(writer, "cargo:rustc-cdylib-link-arg=dynamic_lookup")?;
            writeln!(writer, "cargo:rustc-link-search=native=/opt/homebrew/Cellar/libiconv/1.17/lib")?;
            writeln!(writer, "cargo:rustc-link-search=native=/opt/homebrew/Cellar/python@3.11/3.11.9_1/Frameworks/Python.framework/Versions/3.11/lib")?;
            writeln!(writer, "cargo:rustc-link-lib=dylib=python3.11")?;
            println!("cargo:warning=Linking against Python 3.11");

            // Assuming the toolchain directory is part of the RUSTUP_HOME environment variable
            let home_directory = home_dir().expect("Could not find the home directory");

            let rustup_home = env::var("RUSTUP_HOME").unwrap_or_else(|_| {
                let default_rustup_path = home_directory.join(".rustup");
                eprintln!(
                    "RUSTUP_HOME not set. Using default: {:?}",
                    default_rustup_path
                );
                default_rustup_path.display().to_string()
            });

            let toolchain_path = format!(
                "{}/toolchains/nightly-aarch64-apple-darwin/lib",
                rustup_home
            );

            // Setting the RUSTFLAGS environment variable
            println!(
                "cargo:rustc-env=RUSTFLAGS=-C link-arg=-Wl,-rpath,{}",
                toolchain_path
            );

            // Optional: print out the toolchain path for debugging
            println!("Using toolchain path: {}", toolchain_path);
        }
        _ if triple == &Triple::from_str("wasm32-unknown-emscripten").unwrap() => {
            writeln!(writer, "cargo:rustc-cdylib-link-arg=-sSIDE_MODULE=2")?;
            writeln!(writer, "cargo:rustc-cdylib-link-arg=-sWASM_BIGINT")?;
        }
        _ => {}
    }
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // TODO: use rye to install a static version of python and link against that to simplify installation
    // install_python().unwrap();
    let target_triple = env::var("TARGET").expect("TARGET was not set");
    let triple = Triple::from_str(&target_triple).expect("Invalid target triple");
    add_extension_module_link_args(&triple);
    pyo3_build_config::add_extension_module_link_args();
    Ok(())
}
