use anyhow::Result;
use std::io::{Cursor, Read};
use zip;

// TODO:
//   * zipfile in message
//   * zipfile over http
//   * http - load webpage
//   * http - load json
//   * sqlite - database proxy
//   * arbitrary changes pushed by the host environment

pub fn extract_zip(bytes: &[u8]) -> Result<bool> {
    let cursor = Cursor::new(bytes);
    let mut zip = zip::ZipArchive::new(cursor)?;
    for i in 0..zip.len() {
        let mut file = zip.by_index(i)?;
        if file.is_dir() {
            continue;
        }
        if file.name().contains("__MACOSX") || file.name().contains(".DS_Store") {
            continue;
        }
        let mut buffer = Vec::new();
        file.read_to_end(&mut buffer).expect("Failed to read file");
        let string = String::from_utf8_lossy(&buffer);
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use indoc::indoc;
    use std::fs::File;

    #[test]
    fn test_exec_load_node_zip_bytes() -> Result<()> {
        // Open the file in read-only mode
        let mut file = File::open("./tests/data/files_and_dirs.zip")?;
        let mut buffer = Vec::new();
        file.read_to_end(&mut buffer)?;
        extract_zip(&buffer);
        Ok(())
    }
}
