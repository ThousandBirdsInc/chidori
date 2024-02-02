pub type OperationId = usize;

// TODO: we will want to intern these strings for performance reasons
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum DependencyReference {
    Positional(usize),
    Keyword(String),
    Global(String),
    FunctionInvocation(String),
    Ordering,
}

pub type TimestampOfWrite = usize;
