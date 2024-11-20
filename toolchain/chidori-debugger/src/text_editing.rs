/// Support reconciliation of the text editor state between graph, notebook and file system.

use automerge::{Automerge, AutomergeError, ObjType, ReadDoc, transaction::Transactable};
use std::collections::VecDeque;

fn main() -> Result<(), AutomergeError> {
    // Create a new document
    let mut doc = Automerge::new();

    // Initialize document with text
    let initial_text = "Hello world! This is a collaborative document.";
    let text_obj = doc.put_object(
        doc.get_root()?,
        "text",
        ObjType::Text
    )?;

    // Insert initial text
    doc.splice_text(
        text_obj,
        0,
        0,
        initial_text
    )?;

    println!("Initial document:");
    print_document(&doc)?;

    // Function to edit a range of text
    fn edit_text_range(
        doc: &mut Automerge,
        start: usize,
        delete_count: usize,
        insertion: Option<&str>
    ) -> Result<(), AutomergeError> {
        let text_obj = doc.get_root()?.get("text")?;

        // Delete the specified range if delete_count > 0
        if delete_count > 0 {
            doc.splice_text(
                text_obj,
                start,
                delete_count,
                ""
            )?;
        }

        // Insert new text if provided
        if let Some(text) = insertion {
            doc.splice_text(
                text_obj,
                start,
                0,
                text
            )?;
        }

        Ok(())
    }

    // Example 1: Replace "world" with "everyone"
    edit_text_range(&mut doc, 6, 5, Some("everyone"))?;
    println!("\nAfter replacing 'world' with 'everyone':");
    print_document(&doc)?;

    // Simulate concurrent editing by creating a second document
    let mut doc2 = doc.clone();

    // Doc1: Insert text
    edit_text_range(&mut doc, 19, 0, Some(" really"))?;
    println!("\nDoc1 after inserting ' really':");
    print_document(&doc)?;

    // Doc2: Make a different change
    edit_text_range(&mut doc2, 33, 8, Some("shared"))?;
    println!("\nDoc2 after changing 'document' to 'shared':");
    print_document(&doc2)?;

    // Merge the documents
    doc.merge(&mut doc2)?;
    println!("\nAfter merging changes:");
    print_document(&doc)?;

    // Delete text without insertion
    edit_text_range(&mut doc, 19, 7, None)?;
    println!("\nAfter deleting ' really':");
    print_document(&doc)?;

    // Function to get text from a specific range
    fn get_text_range(
        doc: &Automerge,
        start: usize,
        length: usize
    ) -> Result<String, AutomergeError> {
        let text_obj = doc.get_root()?.get("text")?;
        let mut chars = VecDeque::new();
        doc.text(text_obj, &mut chars)?;

        let end = start.saturating_add(length).min(chars.len());
        Ok(chars.iter().skip(start).take(end - start).collect())
    }

    // Example of getting a specific range
    let range_text = get_text_range(&doc, 6, 9)?;
    println!("\nText range (positions 6-15): {}", range_text);

    Ok(())
}

// Helper function to print the current document state
fn print_document(doc: &Automerge) -> Result<(), AutomergeError> {
    let text_obj = doc.get_root()?.get("text")?;
    let mut chars = VecDeque::new();
    doc.text(text_obj, &mut chars)?;
    println!("{}", chars.iter().collect::<String>());
    Ok(())
}