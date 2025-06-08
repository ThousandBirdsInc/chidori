use std::collections::HashMap;
use epaint::{
    text::{
        cursor::{CCursor, PCursor},
        TAB_SIZE,
    },
    Galley,
};
use crate::text_selection::{
    text_cursor_state::{
        byte_index_from_char_index, ccursor_next_word, ccursor_previous_word, find_line_start,
        slice_char_range,
    },
    CursorRange, CCursorRange,
};
use super::TextBuffer;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum VimMode {
    Normal,
    Insert,
    Visual,
    VisualLine,
    Command,
    OperatorPending,
}

impl Default for VimMode {
    fn default() -> Self {
        VimMode::Normal
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum VimOperator {
    Delete,
    Yank,
    Change,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum VimMotion {
    Left,
    Right,
    Up,
    Down,
    WordForward,
    WordBackward,
    WordEnd,
    LineStart,
    LineEnd,
    FirstLine,
    LastLine,
    LineForward,  // j motion for operators
    LineBackward, // k motion for operators
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TextObject {
    InnerWord,
    AroundWord,
    InnerParens,
    AroundParens,
    InnerBrackets,
    AroundBrackets,
    InnerBraces,
    AroundBraces,
    InnerQuotes,
    AroundQuotes,
    InnerDoubleQuotes,
    AroundDoubleQuotes,
}

#[derive(Debug, Clone)]
pub struct VimCommand {
    pub count: Option<u32>,
    pub operator: Option<VimOperator>,
    pub motion: Option<VimMotion>,
    pub text_object: Option<TextObject>,
    pub is_line_operation: bool, // for dd, yy, cc
}

impl Default for VimCommand {
    fn default() -> Self {
        Self {
            count: None,
            operator: None,
            motion: None,
            text_object: None,
            is_line_operation: false,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct VimState {
    pub mode: VimMode,
    pub command_buffer: String,
    pub last_command: String,
    pub current_command: VimCommand,
    pub search_pattern: Option<String>,
    pub registers: HashMap<char, String>,
    pub visual_start: Option<CCursor>,
    pub mark_positions: HashMap<char, CCursor>,
    pub last_insert_text: String,
    pub command_line_mode: bool,
    pub command_line_input: String,
}

impl VimState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn enter_mode(&mut self, mode: VimMode) {
        self.mode = mode;
        
        match mode {
            VimMode::Normal => {
                self.command_buffer.clear();
                self.current_command = VimCommand::default();
                self.visual_start = None;
                self.command_line_mode = false;
                self.command_line_input.clear();
            }
            VimMode::Insert => {}
            VimMode::Visual | VimMode::VisualLine => {}
            VimMode::Command => {
                self.command_line_mode = true;
                self.command_line_input.clear();
            }
            VimMode::OperatorPending => {}
        }
    }

    pub fn is_insert_mode(&self) -> bool {
        matches!(self.mode, VimMode::Insert)
    }

    pub fn is_normal_mode(&self) -> bool {
        matches!(self.mode, VimMode::Normal)
    }

    pub fn is_visual_mode(&self) -> bool {
        matches!(self.mode, VimMode::Visual | VimMode::VisualLine)
    }

    pub fn is_operator_pending(&self) -> bool {
        matches!(self.mode, VimMode::OperatorPending)
    }

    pub fn add_to_command_buffer(&mut self, ch: char) {
        self.command_buffer.push(ch);
    }

    pub fn clear_command_buffer(&mut self) {
        self.command_buffer.clear();
        self.current_command = VimCommand::default();
        if self.is_operator_pending() {
            self.enter_mode(VimMode::Normal);
        }
    }

    pub fn set_count(&mut self, count: u32) {
        self.current_command.count = Some(count);
    }

    pub fn get_count(&self) -> u32 {
        self.current_command.count.unwrap_or(1)
    }

    pub fn set_operator(&mut self, operator: VimOperator) {
        self.current_command.operator = Some(operator);
        self.enter_mode(VimMode::OperatorPending);
    }

    pub fn complete_command(&mut self) -> Option<VimCommand> {
        if self.current_command.operator.is_some() && 
           (self.current_command.motion.is_some() || 
            self.current_command.text_object.is_some() || 
            self.current_command.is_line_operation) {
            let cmd = self.current_command.clone();
            self.enter_mode(VimMode::Normal);
            Some(cmd)
        } else {
            None
        }
    }
}

pub struct VimMotions;

impl VimMotions {
    pub fn new() -> Self {
        VimMotions
    }

    // Test helper to check if an operator is valid
    #[cfg(test)]
    pub fn is_valid_operator(c: char) -> bool {
        matches!(c, 'd' | 'y' | 'c' | 'r' | 's')
    }

    // Test helper to check if a motion is valid  
    #[cfg(test)]
    pub fn is_valid_motion(c: char) -> bool {
        matches!(c, 'h' | 'j' | 'k' | 'l' | 'w' | 'b' | 'e' | '0' | '$' | 'g')
    }

    /// Handle vim motion in normal mode
    pub fn handle_normal_mode_key(
        vim_state: &mut VimState,
        key_char: char,
        text: &mut dyn TextBuffer,
        galley: &Galley,
        cursor_range: &mut CursorRange,
    ) -> bool {
        let current_cursor = cursor_range.primary.ccursor;

        // Parse numbers for count
        if key_char.is_ascii_digit() && key_char != '0' {
            let digit = key_char.to_digit(10).unwrap();
            let current_count = vim_state.current_command.count.unwrap_or(0);
            vim_state.set_count(current_count * 10 + digit);
            vim_state.add_to_command_buffer(key_char);
            return true;
        }

        // Handle operators first
        if vim_state.is_normal_mode() {
            match key_char {
                'd' => {
                    if vim_state.command_buffer == "d" {
                        // dd - delete line
                        vim_state.current_command.is_line_operation = true;
                        if let Some(cmd) = vim_state.complete_command() {
                            Self::execute_command(&cmd, text, galley, cursor_range);
                        }
                        return true;
                    } else {
                        vim_state.set_operator(VimOperator::Delete);
                        vim_state.add_to_command_buffer(key_char);
                        return true;
                    }
                }
                'y' => {
                    if vim_state.command_buffer == "y" {
                        // yy - yank line
                        vim_state.current_command.is_line_operation = true;
                        if let Some(cmd) = vim_state.complete_command() {
                            Self::execute_command(&cmd, text, galley, cursor_range);
                        }
                        return true;
                    } else {
                        vim_state.set_operator(VimOperator::Yank);
                        vim_state.add_to_command_buffer(key_char);
                        return true;
                    }
                }
                'c' => {
                    if vim_state.command_buffer == "c" {
                        // cc - change line
                        vim_state.current_command.is_line_operation = true;
                        if let Some(cmd) = vim_state.complete_command() {
                            Self::execute_command(&cmd, text, galley, cursor_range);
                        }
                        return true;
                    } else {
                        vim_state.set_operator(VimOperator::Change);
                        vim_state.add_to_command_buffer(key_char);
                        return true;
                    }
                }
                _ => {}
            }
        }

        // Handle motions (both in normal mode and operator-pending mode)
        let motion = match key_char {
            'h' => Some(VimMotion::Left),
            'l' => Some(VimMotion::Right),
            'j' => Some(VimMotion::Down),
            'k' => Some(VimMotion::Up),
            'w' => Some(VimMotion::WordForward),
            'b' => Some(VimMotion::WordBackward),
            'e' => Some(VimMotion::WordEnd),
            '$' => Some(VimMotion::LineEnd),
            'G' => Some(VimMotion::LastLine),
            '0' => {
                if vim_state.current_command.count.is_none() {
                    Some(VimMotion::LineStart)
                } else {
                    vim_state.add_to_command_buffer(key_char);
                    return true;
                }
            }
            'g' => {
                if vim_state.command_buffer.ends_with('g') {
                    Some(VimMotion::FirstLine)
                } else {
                    vim_state.add_to_command_buffer(key_char);
                    return true;
                }
            }
            _ => None,
        };

        if let Some(motion) = motion {
            vim_state.current_command.motion = Some(motion);
            
            if vim_state.is_operator_pending() {
                // Complete the operator+motion command
                if let Some(cmd) = vim_state.complete_command() {
                    Self::execute_command(&cmd, text, galley, cursor_range);
                }
            } else {
                // Just move the cursor
                Self::execute_motion(motion, vim_state.get_count(), galley, text, cursor_range);
                vim_state.clear_command_buffer();
            }
            return true;
        }

        // Handle text objects (only in operator-pending mode)
        if vim_state.is_operator_pending() {
            let text_object = match key_char {
                'w' if vim_state.command_buffer.ends_with('i') => Some(TextObject::InnerWord),
                'w' if vim_state.command_buffer.ends_with('a') => Some(TextObject::AroundWord),
                '(' | ')' if vim_state.command_buffer.ends_with('i') => Some(TextObject::InnerParens),
                '(' | ')' if vim_state.command_buffer.ends_with('a') => Some(TextObject::AroundParens),
                '[' | ']' if vim_state.command_buffer.ends_with('i') => Some(TextObject::InnerBrackets),
                '[' | ']' if vim_state.command_buffer.ends_with('a') => Some(TextObject::AroundBrackets),
                '{' | '}' if vim_state.command_buffer.ends_with('i') => Some(TextObject::InnerBraces),
                '{' | '}' if vim_state.command_buffer.ends_with('a') => Some(TextObject::AroundBraces),
                '\'' if vim_state.command_buffer.ends_with('i') => Some(TextObject::InnerQuotes),
                '\'' if vim_state.command_buffer.ends_with('a') => Some(TextObject::AroundQuotes),
                '"' if vim_state.command_buffer.ends_with('i') => Some(TextObject::InnerDoubleQuotes),
                '"' if vim_state.command_buffer.ends_with('a') => Some(TextObject::AroundDoubleQuotes),
                'i' | 'a' => {
                    vim_state.add_to_command_buffer(key_char);
                    return true;
                }
                _ => None,
            };

            if let Some(text_object) = text_object {
                vim_state.current_command.text_object = Some(text_object);
                if let Some(cmd) = vim_state.complete_command() {
                    Self::execute_command(&cmd, text, galley, cursor_range);
                }
                return true;
            }
        }

        // Handle other commands only in normal mode
        if vim_state.is_normal_mode() {
            match key_char {
                // Mode changes
                'i' => {
                    vim_state.enter_mode(VimMode::Insert);
                    return true;
                }
                'I' => {
                    Self::execute_motion(VimMotion::LineStart, 1, galley, text, cursor_range);
                    vim_state.enter_mode(VimMode::Insert);
                    return true;
                }
                'a' => {
                    Self::execute_motion(VimMotion::Right, 1, galley, text, cursor_range);
                    vim_state.enter_mode(VimMode::Insert);
                    return true;
                }
                'A' => {
                    Self::execute_motion(VimMotion::LineEnd, 1, galley, text, cursor_range);
                    vim_state.enter_mode(VimMode::Insert);
                    return true;
                }
                'o' => {
                    Self::execute_motion(VimMotion::LineEnd, 1, galley, text, cursor_range);
                    let mut new_cursor = cursor_range.primary.ccursor;
                    text.insert_text_at(&mut new_cursor, "\n", usize::MAX);
                    cursor_range.primary = galley.from_ccursor(new_cursor);
                    cursor_range.secondary = cursor_range.primary;
                    vim_state.enter_mode(VimMode::Insert);
                    return true;
                }
                'O' => {
                    Self::execute_motion(VimMotion::LineStart, 1, galley, text, cursor_range);
                    let mut new_cursor = cursor_range.primary.ccursor;
                    text.insert_text_at(&mut new_cursor, "\n", usize::MAX);
                    new_cursor = CCursor { index: new_cursor.index - 1, prefer_next_row: false };
                    cursor_range.primary = galley.from_ccursor(new_cursor);
                    cursor_range.secondary = cursor_range.primary;
                    vim_state.enter_mode(VimMode::Insert);
                    return true;
                }
                'v' => {
                    vim_state.visual_start = Some(current_cursor);
                    vim_state.enter_mode(VimMode::Visual);
                    return true;
                }
                'V' => {
                    vim_state.visual_start = Some(current_cursor);
                    vim_state.enter_mode(VimMode::VisualLine);
                    return true;
                }
                ':' => {
                    vim_state.enter_mode(VimMode::Command);
                    return true;
                }
                
                // Direct editing commands
                'x' => {
                    let count = vim_state.get_count();
                    for _ in 0..count {
                        text.delete_next_char(current_cursor);
                    }
                    vim_state.clear_command_buffer();
                    return true;
                }
                'X' => {
                    let count = vim_state.get_count();
                    for _ in 0..count {
                        text.delete_previous_char(current_cursor);
                    }
                    vim_state.clear_command_buffer();
                    return true;
                }
                'u' => {
                    // Undo - this would need to be implemented with the undoer
                    vim_state.clear_command_buffer();
                    return true;
                }
                _ => {}
            }
        }

        // If we get here, the key wasn't handled
        vim_state.clear_command_buffer();
        false
    }

    fn execute_command(
        cmd: &VimCommand,
        text: &mut dyn TextBuffer,
        galley: &Galley,
        cursor_range: &mut CursorRange,
    ) {
        let current_cursor = cursor_range.primary.ccursor;
        let count = cmd.count.unwrap_or(1);

        if cmd.is_line_operation {
            match cmd.operator {
                Some(VimOperator::Delete) => {
                    for _ in 0..count {
                        Self::delete_line(text, galley, current_cursor);
                    }
                }
                Some(VimOperator::Yank) => {
                    // TODO: Implement yank line
                }
                Some(VimOperator::Change) => {
                    for _ in 0..count {
                        Self::delete_line(text, galley, current_cursor);
                    }
                    // TODO: Enter insert mode
                }
                None => {}
            }
            return;
        }

        if let Some(motion) = cmd.motion {
            let range = Self::get_motion_range(motion, count, galley, text, current_cursor);
            
            match cmd.operator {
                Some(VimOperator::Delete) => {
                    text.delete_char_range(range.start..range.end);
                    cursor_range.primary = galley.from_ccursor(CCursor { index: range.start, prefer_next_row: false });
                    cursor_range.secondary = cursor_range.primary;
                }
                Some(VimOperator::Yank) => {
                    // TODO: Store in register
                }
                Some(VimOperator::Change) => {
                    text.delete_char_range(range.start..range.end);
                    cursor_range.primary = galley.from_ccursor(CCursor { index: range.start, prefer_next_row: false });
                    cursor_range.secondary = cursor_range.primary;
                    // TODO: Enter insert mode
                }
                None => {}
            }
        }

        if let Some(text_obj) = cmd.text_object {
            let range = Self::get_text_object_range(text_obj, galley, text, current_cursor);
            
            match cmd.operator {
                Some(VimOperator::Delete) => {
                    text.delete_char_range(range.start..range.end);
                    cursor_range.primary = galley.from_ccursor(CCursor { index: range.start, prefer_next_row: false });
                    cursor_range.secondary = cursor_range.primary;
                }
                Some(VimOperator::Yank) => {
                    // TODO: Store in register
                }
                Some(VimOperator::Change) => {
                    text.delete_char_range(range.start..range.end);
                    cursor_range.primary = galley.from_ccursor(CCursor { index: range.start, prefer_next_row: false });
                    cursor_range.secondary = cursor_range.primary;
                    // TODO: Enter insert mode
                }
                None => {}
            }
        }
    }

    fn execute_motion(
        motion: VimMotion,
        count: u32,
        galley: &Galley,
        text: &dyn TextBuffer,
        cursor_range: &mut CursorRange,
    ) {
        let current_cursor = cursor_range.primary.ccursor;
        
        let new_cursor = match motion {
            VimMotion::Left => Self::move_left(current_cursor, count),
            VimMotion::Right => Self::move_right(current_cursor, text.as_str(), count),
            VimMotion::Up => Self::move_up(galley, current_cursor, count),
            VimMotion::Down => Self::move_down(galley, current_cursor, count),
            VimMotion::WordForward => Self::move_word_forward(text.as_str(), current_cursor, count),
            VimMotion::WordBackward => Self::move_word_backward(text.as_str(), current_cursor, count),
            VimMotion::WordEnd => Self::move_word_end(text.as_str(), current_cursor, count),
            VimMotion::LineStart => Self::move_line_start(galley, current_cursor),
            VimMotion::LineEnd => Self::move_line_end(galley, current_cursor),
            VimMotion::FirstLine => CCursor { index: 0, prefer_next_row: false },
            VimMotion::LastLine => CCursor { 
                index: text.as_str().chars().count(), 
                prefer_next_row: false 
            },
            VimMotion::LineForward => Self::move_down(galley, current_cursor, count),
            VimMotion::LineBackward => Self::move_up(galley, current_cursor, count),
        };

        cursor_range.primary = galley.from_ccursor(new_cursor);
        cursor_range.secondary = cursor_range.primary;
    }

    fn get_motion_range(
        motion: VimMotion,
        count: u32,
        galley: &Galley,
        text: &dyn TextBuffer,
        cursor: CCursor,
    ) -> std::ops::Range<usize> {
        let start = cursor.index;
        
        let end_cursor = match motion {
            VimMotion::Left => Self::move_left(cursor, count),
            VimMotion::Right => Self::move_right(cursor, text.as_str(), count),
            VimMotion::Up => Self::move_up(galley, cursor, count),
            VimMotion::Down => Self::move_down(galley, cursor, count),
            VimMotion::WordForward => Self::move_word_forward(text.as_str(), cursor, count),
            VimMotion::WordBackward => Self::move_word_backward(text.as_str(), cursor, count),
            VimMotion::WordEnd => Self::move_word_end(text.as_str(), cursor, count),
            VimMotion::LineStart => Self::move_line_start(galley, cursor),
            VimMotion::LineEnd => Self::move_line_end(galley, cursor),
            VimMotion::FirstLine => CCursor { index: 0, prefer_next_row: false },
            VimMotion::LastLine => CCursor { 
                index: text.as_str().chars().count(), 
                prefer_next_row: false 
            },
            VimMotion::LineForward => Self::move_down(galley, cursor, count),
            VimMotion::LineBackward => Self::move_up(galley, cursor, count),
        };

        let end = end_cursor.index;
        
        if start <= end {
            start..end
        } else {
            end..start
        }
    }

    fn get_text_object_range(
        text_obj: TextObject,
        galley: &Galley,
        text: &dyn TextBuffer,
        cursor: CCursor,
    ) -> std::ops::Range<usize> {
        match text_obj {
            TextObject::InnerWord | TextObject::AroundWord => {
                let word_start = ccursor_previous_word(text.as_str(), cursor);
                let word_end = ccursor_next_word(text.as_str(), cursor);
                word_start.index..word_end.index
            }
            _ => {
                // TODO: Implement other text objects
                cursor.index..cursor.index
            }
        }
    }

    // Motion implementations (same as before)
    fn move_left(cursor: CCursor, count: u32) -> CCursor {
        CCursor {
            index: cursor.index.saturating_sub(count as usize),
            prefer_next_row: false,
        }
    }

    fn move_right(cursor: CCursor, text: &str, count: u32) -> CCursor {
        let max_index = text.chars().count();
        CCursor {
            index: (cursor.index + count as usize).min(max_index.saturating_sub(1)),
            prefer_next_row: false,
        }
    }

    fn move_up(galley: &Galley, cursor: CCursor, count: u32) -> CCursor {
        let current_pos = galley.from_ccursor(cursor);
        let target_row = current_pos.pcursor.paragraph.saturating_sub(count as usize);
        let new_pcursor = PCursor {
            paragraph: target_row,
            offset: current_pos.pcursor.offset,
            prefer_next_row: false,
        };
        galley.from_pcursor(new_pcursor).ccursor
    }

    fn move_down(galley: &Galley, cursor: CCursor, count: u32) -> CCursor {
        let current_pos = galley.from_ccursor(cursor);
        let max_row = galley.rows.len().saturating_sub(1);
        let target_row = (current_pos.pcursor.paragraph + count as usize).min(max_row);
        let new_pcursor = PCursor {
            paragraph: target_row,
            offset: current_pos.pcursor.offset,
            prefer_next_row: false,
        };
        galley.from_pcursor(new_pcursor).ccursor
    }

    fn move_word_forward(text: &str, cursor: CCursor, count: u32) -> CCursor {
        let mut current = cursor;
        for _ in 0..count {
            current = ccursor_next_word(text, current);
        }
        current
    }

    fn move_word_backward(text: &str, cursor: CCursor, count: u32) -> CCursor {
        let mut current = cursor;
        for _ in 0..count {
            current = ccursor_previous_word(text, current);
        }
        current
    }

    fn move_word_end(text: &str, cursor: CCursor, count: u32) -> CCursor {
        let mut current = cursor;
        for _ in 0..count {
            // Move to next word, then back one character
            current = ccursor_next_word(text, current);
            if current.index > 0 {
                current.index -= 1;
            }
        }
        current
    }

    fn move_line_start(galley: &Galley, cursor: CCursor) -> CCursor {
        let current_pos = galley.from_ccursor(cursor);
        let new_pcursor = PCursor {
            paragraph: current_pos.pcursor.paragraph,
            offset: 0,
            prefer_next_row: false,
        };
        galley.from_pcursor(new_pcursor).ccursor
    }

    fn move_line_end(galley: &Galley, cursor: CCursor) -> CCursor {
        let current_pos = galley.from_ccursor(cursor);
        let new_pcursor = PCursor {
            paragraph: current_pos.pcursor.paragraph,
            offset: usize::MAX, // Will be clamped to end of line
            prefer_next_row: false,
        };
        galley.from_pcursor(new_pcursor).ccursor
    }

    fn delete_line(text: &mut dyn TextBuffer, galley: &Galley, cursor: CCursor) {
        let current_pos = galley.from_ccursor(cursor);
        let line_start = PCursor {
            paragraph: current_pos.pcursor.paragraph,
            offset: 0,
            prefer_next_row: false,
        };
        let line_end = PCursor {
            paragraph: current_pos.pcursor.paragraph,
            offset: usize::MAX,
            prefer_next_row: false,
        };
        
        let start_ccursor = galley.from_pcursor(line_start).ccursor;
        let end_ccursor = galley.from_pcursor(line_end).ccursor;
        
        text.delete_char_range(start_ccursor.index..end_ccursor.index);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_state() -> VimState {
        VimState {
            mode: VimMode::Normal,
            command_buffer: String::new(),
            last_command: String::new(),
            current_command: VimCommand::default(),
            search_pattern: None,
            registers: HashMap::new(),
            visual_start: None,
            mark_positions: HashMap::new(),
            last_insert_text: String::new(),
            command_line_mode: false,
            command_line_input: String::new(),
        }
    }

    #[test]
    fn test_vim_mode_transitions() {
        let mut state = create_test_state();
        
        // Start in normal mode
        assert_eq!(state.mode, VimMode::Normal);
        
        // Enter insert mode
        state.enter_mode(VimMode::Insert);
        assert!(state.is_insert_mode());
        
        // Enter visual mode
        state.enter_mode(VimMode::Visual);
        assert!(state.is_visual_mode());
        
        // Enter operator pending mode
        state.enter_mode(VimMode::OperatorPending);
        assert!(state.is_operator_pending());
    }

    #[test]
    fn test_operator_detection() {
        assert!(VimMotions::is_valid_operator('d'));
        assert!(VimMotions::is_valid_operator('y'));
        assert!(VimMotions::is_valid_operator('c'));
        assert!(!VimMotions::is_valid_operator('h'));
        assert!(!VimMotions::is_valid_operator('x'));
    }

    #[test]
    fn test_motion_detection() {
        assert!(VimMotions::is_valid_motion('h'));
        assert!(VimMotions::is_valid_motion('j'));
        assert!(VimMotions::is_valid_motion('w'));
        assert!(VimMotions::is_valid_motion('$'));
        assert!(!VimMotions::is_valid_motion('d'));
        assert!(!VimMotions::is_valid_motion('i'));
    }

    #[test]
    fn test_count_setting() {
        let mut state = create_test_state();
        
        // Test setting count
        state.set_count(5);
        assert_eq!(state.get_count(), 5);
        
        state.set_count(123);
        assert_eq!(state.get_count(), 123);
        
        // Clear count
        state.clear_command_buffer();
        assert_eq!(state.get_count(), 1); // default count
    }

    #[test]
    fn test_operator_setting() {
        let mut state = create_test_state();
        
        // Test setting operator
        state.set_operator(VimOperator::Delete);
        assert_eq!(state.current_command.operator, Some(VimOperator::Delete));
        assert!(state.is_operator_pending());
        
        state.set_operator(VimOperator::Yank);
        assert_eq!(state.current_command.operator, Some(VimOperator::Yank));
        
        state.set_operator(VimOperator::Change);
        assert_eq!(state.current_command.operator, Some(VimOperator::Change));
    }

    #[test]
    fn test_command_completion() {
        let mut state = create_test_state();
        
        // Set up a complete command (operator + motion)
        state.set_operator(VimOperator::Delete);
        state.current_command.motion = Some(VimMotion::WordForward);
        
        // Should complete the command
        let completed = state.complete_command();
        assert!(completed.is_some());
        
        let cmd = completed.unwrap();
        assert_eq!(cmd.operator, Some(VimOperator::Delete));
        assert_eq!(cmd.motion, Some(VimMotion::WordForward));
        
        // State should return to normal mode
        assert!(state.is_normal_mode());
    }

    #[test]
    fn test_line_operations() {
        let mut state = create_test_state();
        
        // Set up a line operation (dd, yy, cc)
        state.set_operator(VimOperator::Delete);
        state.current_command.is_line_operation = true;
        
        let completed = state.complete_command();
        assert!(completed.is_some());
        
        let cmd = completed.unwrap();
        assert_eq!(cmd.operator, Some(VimOperator::Delete));
        assert!(cmd.is_line_operation);
        assert!(cmd.motion.is_none()); // Line operations don't need motions
    }

    #[test]
    fn test_command_buffer_operations() {
        let mut state = create_test_state();
        
        // Test adding to command buffer
        state.add_to_command_buffer('d');
        assert_eq!(state.command_buffer, "d");
        
        state.add_to_command_buffer('w');
        assert_eq!(state.command_buffer, "dw");
        
        // Test clearing command buffer
        state.clear_command_buffer();
        assert!(state.command_buffer.is_empty());
        assert_eq!(state.current_command.operator, None);
        assert_eq!(state.current_command.motion, None);
        assert!(state.is_normal_mode());
    }
} 