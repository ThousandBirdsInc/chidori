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
use tracing::info;
pub use uuid;
use chidori_core::sdk::chidori::Chidori;
use chidori_core::sdk::entry::PlaybackState;
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
    // /// Run tests
    // Test {
    //     /// Path to the test directory
    //     #[arg(short, long)]
    //     test_dir: PathBuf,
    //     /// Verbose output
    //     #[arg(short, long)]
    //     verbose: bool,
    // },
    // /// Deploy the application
    // Deploy {
    //     /// Target environment
    //     #[arg(short, long)]
    //     environment: String,
    //     /// Path to the deployment script
    //     #[arg(short, long)]
    //     script: PathBuf,
    // },
}

async fn run_command(run_directory: &PathBuf) -> anyhow::Result<()> {
    let runtime = tokio::runtime::Handle::current();

    let (trace_event_sender, trace_event_receiver) = mpsc::channel();
    let (runtime_event_sender, runtime_event_receiver) = mpsc::channel();

    let mut chidori = Chidori::new_with_events(
        trace_event_sender,
        runtime_event_sender,
    );

    let run_directory_clone = run_directory.clone();
    runtime.spawn(async move {
        loop {
            let mut instance = chidori.get_instance().unwrap();
            let _await_ready = instance.wait_until_ready().await;
            chidori.load_md_directory(&run_directory_clone).unwrap();
            let result = instance.run(PlaybackState::Running).await;
            match result {
                Ok(_) => {
                    info!("Instance completed execution and closed successfully.");
                    break;
                }
                Err(e) => {
                    info!("Error occurred: {}, retrying...", e);
                }
            }
        }
    });

    // Here you can add any additional setup or processing needed for the run command
    info!("Chidori instance is running in the background.");

    // Keep the main thread alive
    tokio::signal::ctrl_c().await.expect("Failed to listen for ctrl+c");
    info!("Received shutdown signal. Terminating...");
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()>{
    let cli = Cli::parse();

    match &cli.command {
        Some(Commands::Run { load }) => {
            info!("Running Chidori with target src directory: {:?}", load);
            run_command(load).await
        }
        // Some(Commands::Test { test_dir, verbose }) => {
        //     println!("Running tests in directory: {:?}", test_dir);
        //     println!("Verbose mode: {}", verbose);
        //     // Add your test logic here
        // }
        // Some(Commands::Deploy { environment, script }) => {
        //     println!("Deploying to environment: {}", environment);
        //     println!("Using deployment script: {:?}", script);
        //     // Add your deployment logic here
        // }
        None => {
            println!("No command was used");
            Ok(())
        }
    }
}