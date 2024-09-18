use uuid::Uuid;

pub type OperationId = Uuid;

// TODO: we will want to intern these strings for performance reasons
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum DependencyReference {
    Positional(usize),
    Keyword(String),
    Global(String),
    FunctionInvocation(String),
    Ordering,
}

pub type TimestampOfWrite = usize;
