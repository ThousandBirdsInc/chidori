use std::ops::{Deref, DerefMut};

#[derive(Debug)]
pub struct CoreError(pub anyhow::Error);

impl Deref for CoreError {
    type Target = anyhow::Error;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for CoreError {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}
