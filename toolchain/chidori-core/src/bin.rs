#[macro_use]
extern crate clap;
extern crate env_logger;
#[macro_use]
extern crate log;

use clap::{App, Arg};
use regex::Regex;
use std::fs;
use std::{
    path::Path,
    time::{Duration, Instant},
};

fn main() {
    env_logger::init();
    let app = App::new("chidori")
        .version(crate_version!())
        .author(crate_authors!())
        .about("Runs an notebook directory.")
        .arg(
            Arg::with_name("folder")
                .help("Folder to scan")
                .required(true),
        );
    let matches = app.get_matches();

    let folder = Path::new(matches.value_of("folder").unwrap());
    if folder.exists() && folder.is_dir() {
        println!("Parsing folder of python code: {folder:?}");
        let t1 = Instant::now();
        let parsed_files = parse_folder(folder).unwrap();
        let t2 = Instant::now();
    } else {
        println!("{folder:?} is not a folder.");
    }
}

fn parse_folder(path: &Path) -> std::io::Result<Vec<ParsedFile>> {
    let mut res = vec![];
    for entry in path.read_dir()? {
        let entry = entry?;
        let metadata = entry.metadata()?;

        let path = entry.path();
        if metadata.is_dir() {
            res.extend(parse_folder(&path)?);
        }

        if metadata.is_file() && path.extension().and_then(|s| s.to_str()) == Some("md") {
            let parsed_file = parse_markdown_file(&path);
            match &parsed_file.result {
                Ok(_) => {}
                Err(y) => error!("Error in file {:?} {:?}", path, y),
            }

            res.push(parsed_file);
        }
    }
    Ok(res)
}

fn extract_code_blocks(file_path: &str) -> Vec<String> {
    let content = fs::read_to_string(file_path).expect("Something went wrong reading the file");

    let re = Regex::new(r"```.*?\n(.*?)```").unwrap();
    re.captures_iter(&content)
        .filter_map(|cap| cap.get(1))
        .map(|m| m.as_str().trim().to_string())
        .collect()
}

fn parse_markdown_file(filename: &Path) -> ParsedFile {
    info!("Parsing file {:?}", filename);
    match std::fs::read_to_string(filename) {
        Err(e) => ParsedFile {
            // filename: Box::new(filename.to_path_buf()),
            // code: "".to_owned(),
            num_lines: 0,
            result: Err(e.to_string()),
        },
        Ok(source) => {
            let num_lines = source.lines().count();
            let result = extract_code_blocks(&source);
            ParsedFile {
                // filename: Box::new(filename.to_path_buf()),
                // code: source.to_string(),
                num_lines,
                result,
            }
        }
    }
}

struct ParsedFile {
    // filename: Box<PathBuf>,
    // code: String,
    num_lines: usize,
    result: Vec<String>,
}
