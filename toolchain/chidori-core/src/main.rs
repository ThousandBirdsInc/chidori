#![allow(warnings)]
#![feature(is_sorted)]
#![feature(thread_id_value)]
#![feature(generic_nonzero)]

use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::sync::{mpsc, Arc, Mutex};

mod cells;
mod execution;
mod library;
mod sdk;
mod utils;

pub use tokio;
pub use uuid;
use chidori_core::sdk::chidori::Chidori;
pub use chidori_static_analysis;
pub use chidori_prompt_format;

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Run the application
    Run {
        /// Path to the configuration file
        #[arg(short, long)]
        load: PathBuf,
    },
    /// Run tests
    Test {
        /// Path to the test directory
        #[arg(short, long)]
        test_dir: PathBuf,
        /// Verbose output
        #[arg(short, long)]
        verbose: bool,
    },
    /// Deploy the application
    Deploy {
        /// Target environment
        #[arg(short, long)]
        environment: String,
        /// Path to the deployment script
        #[arg(short, long)]
        script: PathBuf,
    },
}

async fn run_command(config: &PathBuf) {
    let runtime = tokio::runtime::Handle::current();

    let (trace_event_sender, trace_event_receiver) = mpsc::channel();
    let (runtime_event_sender, runtime_event_receiver) = mpsc::channel();

    let mut chidori = Chidori::new_with_events(
            trace_event_sender,
            runtime_event_sender,
        );

    runtime.spawn(async move {
        loop {
            let mut instance = chidori.get_instance().unwrap();
            let _await_ready = instance.wait_until_ready().await;
            let result = instance.run().await;
            match result {
                Ok(_) => {
                    println!("Instance completed execution and closed successfully.");
                    break;
                }
                Err(e) => {
                    println!("Error occurred: {}, retrying...", e);
                }
            }
        }
    });

    // Here you can add any additional setup or processing needed for the run command
    println!("Chidori instance is running in the background.");

    // Keep the main thread alive
    tokio::signal::ctrl_c().await.expect("Failed to listen for ctrl+c");
    println!("Received shutdown signal. Terminating...");
}

fn main() {
    let cli = Cli::parse();

    match &cli.command {
        Some(Commands::Run { load }) => {
            println!("Running the application with config file: {:?}", load);
            run_command(load);
        }
        Some(Commands::Test { test_dir, verbose }) => {
            println!("Running tests in directory: {:?}", test_dir);
            println!("Verbose mode: {}", verbose);
            // Add your test logic here
        }
        Some(Commands::Deploy { environment, script }) => {
            println!("Deploying to environment: {}", environment);
            println!("Using deployment script: {:?}", script);
            // Add your deployment logic here
        }
        None => {
            println!("No command was used");
        }
    }
}