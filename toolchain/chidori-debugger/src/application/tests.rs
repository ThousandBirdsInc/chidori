#[cfg(test)]
mod tests {
    use crate::application::types::*;
    use crate::application::state::*;
    use std::fs;
    use tempfile::TempDir;
    use chidori_core::cells::{CellTypes, CodeCell, SupportedLanguage, TextRange};
    use chidori_core::execution::primitives::identifiers::OperationId;
    use chidori_core::sdk::interactive_chidori_wrapper::CellHolder;
    use chidori_core::uuid::Uuid;
    use std::sync::{Arc, Mutex};

    fn create_test_cell(op_id: OperationId, path: &str, range: TextRange, body: &str, is_dirty: bool) -> CellHolder {
        let cell = CodeCell {
            backing_file_reference: Some(chidori_core::cells::BackingFileReference {
                path: path.to_string(),
                text_range: Some(range),
            }),
            // Add any other required fields with their default values
            name: None,
            language: SupportedLanguage::PyO3,
            source_code: body.to_string(),
            function_invocation: None,
        };
        
        CellHolder {
            op_id,
            cell: CellTypes::Code(cell, TextRange::default()),
            is_dirty_editor: is_dirty,
        }
    }

    #[test]
    fn test_save_notebook_single_file() {
        // Create a temporary directory
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test.py");
        
        // Create initial file content
        let initial_content = "def hello():\n    print('hello')\n\ndef world():\n    print('world')\n";
        fs::write(&file_path, initial_content).unwrap();

        // Create ChidoriState with a modified cell
        let mut state = ChidoriState::default();
        let op_id = Uuid::now_v7();
        
        // Calculate the correct range for the first function
        let first_func_range = TextRange { 
            start: 0, 
            end: initial_content.find("\n\ndef").unwrap_or(initial_content.len())
        };
        
        let cell_holder = create_test_cell(
            op_id,
            file_path.to_str().unwrap(),
            first_func_range,
            "def hello():\n    print('modified')",
            true
        );

        let cell_state = CellState {
            cell: Some(cell_holder),
            ..Default::default()
        };

        state.local_cell_state.insert(op_id, Arc::new(Mutex::new(cell_state)));

        // Save the notebook
        state.save_notebook();

        // Verify the file content
        let final_content = fs::read_to_string(&file_path).unwrap();
        let expected_content = "def hello():\n    print('modified')\n\ndef world():\n    print('world')\n";
        assert_eq!(final_content, expected_content);
    }

    #[test]
    fn test_save_notebook_multiple_files() {
        let temp_dir = TempDir::new().unwrap();
        let file1_path = temp_dir.path().join("file1.py");
        let file2_path = temp_dir.path().join("file2.py");

        // Create initial file contents with explicit newlines
        let file1_content = "def func1():\n    return 1\n";
        let file2_content = "def func2():\n    return 2\n";
        fs::write(&file1_path, file1_content).unwrap();
        fs::write(&file2_path, file2_content).unwrap();

        let mut state = ChidoriState::default();
        let op_id1 = Uuid::now_v7();
        let op_id2 = Uuid::now_v7();

        // Create modified cells with correct ranges and content
        let cell_holder1 = create_test_cell(
            op_id1,
            file1_path.to_str().unwrap(),
            TextRange { start: 0, end: file1_content.len() },
            "def func1():\n    return 'modified1'\n",
            true
        );

        let cell_holder2 = create_test_cell(
            op_id2,
            file2_path.to_str().unwrap(),
            TextRange { start: 0, end: file2_content.len() },
            "def func2():\n    return 'modified2'\n",
            true
        );

        state.local_cell_state.insert(op_id1, Arc::new(Mutex::new(CellState {
            cell: Some(cell_holder1),
            ..Default::default()
        })));

        state.local_cell_state.insert(op_id2, Arc::new(Mutex::new(CellState {
            cell: Some(cell_holder2),
            ..Default::default()
        })));

        // Save the notebook
        state.save_notebook();

        // Verify both files' content
        let content1 = fs::read_to_string(&file1_path).unwrap();
        let content2 = fs::read_to_string(&file2_path).unwrap();

        assert_eq!(content1, "def func1():\n    return 'modified1'\n");
        assert_eq!(content2, "def func2():\n    return 'modified2'\n");
    }

    #[test]
    fn test_save_notebook_non_dirty_cells() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test.py");
        
        let initial_content = "def test():\n    return 1\n";
        fs::write(&file_path, initial_content).unwrap();

        let mut state = ChidoriState::default();
        let op_id = Uuid::now_v7();

        // Create a cell that is not dirty
        let cell_holder = create_test_cell(
            op_id,
            file_path.to_str().unwrap(),
            TextRange { start: 0, end: 21 },
            "def test():\n    return 'modified'\n",
            false // Not dirty
        );

        state.local_cell_state.insert(op_id, Arc::new(Mutex::new(CellState {
            cell: Some(cell_holder),
            ..Default::default()
        })));

        // Save the notebook
        state.save_notebook();

        // Verify the file content remains unchanged
        let final_content = fs::read_to_string(&file_path).unwrap();
        assert_eq!(final_content, initial_content);
    }

    #[test]
    fn test_save_notebook_invalid_range() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test.py");
        
        let initial_content = "def test():\n    return 1\n";
        fs::write(&file_path, initial_content).unwrap();

        let mut state = ChidoriState::default();
        let op_id = Uuid::now_v7();

        // Create a cell with an invalid range
        let cell_holder = create_test_cell(
            op_id,
            file_path.to_str().unwrap(),
            TextRange { start: 1000, end: 2000 }, // Invalid range
            "def test():\n    return 'modified'\n",
            true
        );

        state.local_cell_state.insert(op_id, Arc::new(Mutex::new(CellState {
            cell: Some(cell_holder),
            ..Default::default()
        })));

        // Save should not panic and file should remain unchanged
        state.save_notebook();

        let final_content = fs::read_to_string(&file_path).unwrap();
        assert_eq!(final_content, initial_content);
    }
} 