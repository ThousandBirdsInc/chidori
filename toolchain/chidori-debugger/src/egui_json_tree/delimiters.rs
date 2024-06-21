pub struct Delimiters {
    pub collapsed: &'static str,
    pub collapsed_empty: &'static str,
    pub opening: &'static str,
    pub closing: &'static str,
}

pub const ARRAY_DELIMITERS: Delimiters = Delimiters {
    collapsed: "[...]",
    collapsed_empty: "[]",
    opening: "[",
    closing: "]",
};

pub const OBJECT_DELIMITERS: Delimiters = Delimiters {
    collapsed: "{...}",
    collapsed_empty: "{}",
    opening: "{",
    closing: "}",
};
