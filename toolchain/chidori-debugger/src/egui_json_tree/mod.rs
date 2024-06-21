//! An interactive JSON tree visualiser for `egui`, with search and highlight functionality.
//!
//! ```
//! use egui::{Color32};
//! use egui_json_tree::{DefaultExpand, JsonTree, JsonTreeStyle};
//!
//! # egui::__run_test_ui(|ui| {
//! let value = serde_json::json!({ "foo": "bar", "fizz": [1, 2, 3]});
//!
//! // Simple:
//! JsonTree::new("simple-tree", &value).show(ui);
//!
//! // Customised:
//! let response = JsonTree::new("customised-tree", &value)
//!     .style(JsonTreeStyle {
//!         bool_color: Color32::YELLOW,
//!         ..Default::default()
//!     })
//!     .default_expand(DefaultExpand::All)
//!     .response_callback(|response, json_pointer_string| {
//!       // Handle interactions within the JsonTree.
//!     })
//!     .abbreviate_root(true) // Show {...} when the root object is collapsed.
//!     .show(ui);
//!
//! // Reset the expanded state of all arrays/objects to respect the `default_expand` setting.
//! response.reset_expanded(ui);
//! # });
//! ```
//! [`JsonTree`] can visualise any type that implements [`ToJsonTreeValue`](trait@value::ToJsonTreeValue).
//! Implementations to support [`serde_json::Value`](serde_json::Value) (enabled by default by the crate feature `serde_json`)
//! and `simd_json::owned::Value` (optionally enabled by the crate feature `simd_json`)
//! are provided with this crate.
//! If you wish to use a different JSON type, see the [`value`](mod@value) module,
//! and disable default features in your `Cargo.toml` if you do not need the [`serde_json`](serde_json) dependency.
mod default_expand;
mod delimiters;
mod node;
mod response;
mod search;
mod style;
mod tree;

pub use response::JsonTreeResponse;
pub use style::JsonTreeStyle;
pub mod value;
pub use default_expand::DefaultExpand;
pub use tree::JsonTree;
