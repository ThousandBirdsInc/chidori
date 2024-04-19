use thiserror::Error;

#[derive(Error, Debug)]
pub enum ChidoriError {
    #[error("unknown chidori error")]
    Unknown,
}