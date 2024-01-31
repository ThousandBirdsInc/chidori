pub type OperationId = usize;

// TODO: we will want to intern these strings for performance reasons
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ArgumentIndex {
    Positional(usize),
    Keyword(String),
    Global(String),
}
pub type TimestampOfWrite = usize;
