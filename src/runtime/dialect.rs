use starlark::syntax::Dialect;

/// The Starlark dialect the studio emits. Starts from `Dialect::Standard` and
/// flips on f-strings, which the visual editor produces for interpolated
/// prompt templates (`f"hello {name}"`).
pub fn studio_dialect() -> Dialect {
    Dialect {
        enable_f_strings: true,
        ..Dialect::Standard
    }
}
