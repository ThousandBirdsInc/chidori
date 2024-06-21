use crate::egui_json_tree::{
    node::JsonTreeNode, value::ToJsonTreeValue, DefaultExpand, JsonTreeResponse, JsonTreeStyle,
};
use egui::{Id, Response, Ui};
use std::hash::Hash;

type ResponseCallback<'a> = dyn FnMut(Response, &String) + 'a;

#[derive(Default)]
pub struct JsonTreeConfig<'a> {
    pub(crate) style: JsonTreeStyle,
    pub(crate) default_expand: DefaultExpand<'a>,
    pub(crate) response_callback: Option<Box<ResponseCallback<'a>>>,
    pub(crate) abbreviate_root: bool,
}

/// An interactive JSON tree visualiser.
#[must_use = "You should call .show()"]
pub struct JsonTree<'a> {
    id: Id,
    value: &'a dyn ToJsonTreeValue,
    config: JsonTreeConfig<'a>,
}

impl<'a> JsonTree<'a> {
    /// Creates a new [`JsonTree`].
    /// `id` must be a globally unique identifier.
    pub fn new(id: impl Hash, value: &'a impl ToJsonTreeValue) -> Self {
        Self {
            id: Id::new(id),
            value,
            config: JsonTreeConfig::default(),
        }
    }

    /// Override colors for JSON syntax highlighting, and search match highlighting.
    pub fn style(mut self, style: JsonTreeStyle) -> Self {
        self.config.style = style;
        self
    }

    /// Override how the [`JsonTree`] expands arrays/objects by default.
    pub fn default_expand(mut self, default_expand: DefaultExpand<'a>) -> Self {
        self.config.default_expand = default_expand;
        self
    }

    /// Register a callback to handle interactions within a [`JsonTree`].
    /// - `Response`: The `Response` from rendering an array index, object key or value.
    /// - `&String`: A JSON pointer string.
    pub fn response_callback(
        mut self,
        response_callback: impl FnMut(Response, &String) + 'a,
    ) -> Self {
        self.config.response_callback = Some(Box::new(response_callback));
        self
    }

    /// Override whether a root array/object should show direct child elements when collapsed.
    ///
    /// If called with `true`, a collapsed root object would render as: `{...}`.
    ///
    /// Otherwise, a collapsed root object would render as: `{ "foo": "bar", "baz": {...} }`.
    pub fn abbreviate_root(mut self, abbreviate_root: bool) -> Self {
        self.config.abbreviate_root = abbreviate_root;
        self
    }

    /// Show the JSON tree visualisation within the `Ui`.
    pub fn show(self, ui: &mut Ui) -> JsonTreeResponse {
        JsonTreeNode::new(self.id, self.value).show_with_config(ui, self.config)
    }
}
