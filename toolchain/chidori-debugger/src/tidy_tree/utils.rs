macro_rules! nocheck_mut {
    ($ptr:expr) => {{
        let temp: *mut _ = &mut $ptr;
        unsafe { &mut *temp }
    }};
}

use chidori_core::cells::{CellTypes, LLMPromptCell};
use chidori_core::execution::primitives::serialized_value::RkyvSerializedValue;
pub(crate) use nocheck_mut;
