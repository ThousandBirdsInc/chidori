#[derive(Default, Debug, Clone)]
/// Configuration for how a [`JsonTree`](crate::JsonTree) should expand arrays and objects by default.
pub enum DefaultExpand<'a> {
    /// Expand all arrays and objects.
    All,
    /// Collapse all arrays and objects.
    #[default]
    None,
    /// Expand arrays and objects according to how many levels deep they are nested:
    /// - `0` would expand a top-level array/object only,
    /// - `1` would expand a top-level array/object and any array/object that is a direct child,
    /// - `2` ...
    ///
    /// And so on.
    ToLevel(u8),
    /// Expand arrays and objects to display object keys and values,
    /// and array elements, that match the search term. Letter case is ignored. The matches are highlighted.
    /// If the search term is empty, nothing will be expanded by default.
    SearchResults(&'a str),
}
