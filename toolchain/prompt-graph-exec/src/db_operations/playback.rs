use crate::db_operations::{CURRENT_PLAYBACK_FRAME_KEY, IS_PLAYING_PREFIX};
use crate::db_operations;

// =================
// Handling where a user session is observing, where they are resuming execution and playback
pub fn get_is_playing_status(tree: &sled::Tree) -> Option<bool> {
    tree.get(is_playing_prefix()).unwrap().map(|x| x.to_vec()[0] == 1)
}

pub fn pause_execution_at_frame(tree: &sled::Tree, frame: u64) {
    tree.insert(is_playing_prefix(), &[0]).unwrap();
    tree.insert(current_playback_frame_prefix(), &frame.to_be_bytes()).unwrap();
}

pub fn play_execution_at_frame(tree: &sled::Tree, frame: u64) {
    tree.insert(is_playing_prefix(), &[1]).unwrap();
    tree.insert(current_playback_frame_prefix(), &frame.to_be_bytes()).unwrap();
}

// ==================
// Subscriptions to changes in the database - can be used to stream
pub fn subscribe_to_playback_state(tree: sled::Tree) -> sled::Subscriber {
    tree.watch_prefix(is_playing_prefix())
}

/// Playback state is global, it would be far to complicated for users to manage separate playback
/// state across multiple execution branches.
fn is_playing_prefix() -> Vec<u8> {
    db_operations::encode_into_slice((IS_PLAYING_PREFIX)).unwrap()
}


/// Playback frame is used to store the frame the user expects to be currently observing
/// when paused, no subsequent changes on this branch beyond this frame will be executed.
/// When a user plays from an earlier frame, we expect to re-emit all changes from that frame
/// to the user.
fn current_playback_frame_prefix() -> Vec<u8> {
    db_operations::encode_into_slice((CURRENT_PLAYBACK_FRAME_KEY)).unwrap()
}

#[cfg(test)]
mod tests {
    use sled::Config;

    fn test_() {
        let db = Config::new().temporary(true).flush_every_ms(None).open().unwrap();
        let tree = db.open_tree("test").unwrap();
    }
}
