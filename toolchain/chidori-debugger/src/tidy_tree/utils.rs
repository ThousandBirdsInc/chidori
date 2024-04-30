macro_rules! nocheck_mut {
    ($ptr:expr) => {{
        let temp: *mut _ = &mut $ptr;
        unsafe { &mut *temp }
    }};
}

pub(crate) use nocheck_mut;
