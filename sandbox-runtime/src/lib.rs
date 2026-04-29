//! Small infix expression language compiled to `wasm32-unknown-unknown` and
//! embedded into the host crate via `include_bytes!`. This is the language
//! layer behind the host's `exec_expr()` primitive.
//!
//! # Why this instead of postfix?
//!
//! The previous iteration of `sandbox-runtime` shipped a postfix calculator
//! (`2 3 +`). That proved out the "host ships source, sandbox evaluates with
//! fuel+memory bounds, host reads result" round trip, but infix with
//! variables is how LLMs actually write code. This file grows the runtime
//! into a small recursive-descent interpreter with `let`, `if/then/else`,
//! arithmetic, comparisons, and logical ops.
//!
//! # ABI
//!
//! The host reads `scratch_ptr() -> i32` and `scratch_cap() -> i32`, writes
//! up to `scratch_cap` bytes of UTF-8 source there, and calls `eval(len)`.
//! The return value packs `(result_ptr << 32) | result_len`; the host reads
//! `result_len` UTF-8 bytes from the exported `memory` at `result_ptr`.
//! Results prefixed with `ERR:` mean the program errored.
//!
//! # Language
//!
//! ```text
//! expr     = let_expr
//! let_expr = 'let' IDENT '=' expr 'in' expr
//!          | if_expr
//! if_expr  = 'if' expr 'then' expr 'else' expr
//!          | or_expr
//! or_expr  = and_expr ('||' and_expr)*
//! and_expr = eq_expr ('&&' eq_expr)*
//! eq_expr  = cmp_expr (('==' | '!=') cmp_expr)?
//! cmp_expr = add_expr (('<' | '<=' | '>' | '>=') add_expr)?
//! add_expr = mul_expr (('+' | '-') mul_expr)*
//! mul_expr = unary    (('*' | '/' | '%') unary)*
//! unary    = ('-' | '!') unary | atom
//! atom     = INT | 'true' | 'false' | IDENT | '(' expr ')'
//! ```
//!
//! Host-supplied vars are passed by prepending `let name = value in …`
//! chains in the host before shipping the source, so the parser doesn't
//! need a separate "environment" concept.

#![no_std]
#![no_main]

extern crate alloc;
use alloc::boxed::Box;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::panic::PanicInfo;

// ---------------------------------------------------------------------------
// Panic + allocator
// ---------------------------------------------------------------------------

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    core::arch::wasm32::unreachable()
}

mod bump {
    use core::alloc::{GlobalAlloc, Layout};
    use core::cell::UnsafeCell;

    const HEAP_SIZE: usize = 256 * 1024;

    #[repr(C, align(16))]
    struct Heap {
        bytes: UnsafeCell<[u8; HEAP_SIZE]>,
        next: UnsafeCell<usize>,
    }

    unsafe impl Sync for Heap {}

    static HEAP: Heap = Heap {
        bytes: UnsafeCell::new([0; HEAP_SIZE]),
        next: UnsafeCell::new(0),
    };

    pub struct BumpAlloc;

    unsafe impl GlobalAlloc for BumpAlloc {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            let next_ptr = HEAP.next.get();
            let start = *next_ptr;
            let aligned = (start + layout.align() - 1) & !(layout.align() - 1);
            let end = aligned + layout.size();
            if end > HEAP_SIZE {
                return core::ptr::null_mut();
            }
            *next_ptr = end;
            (HEAP.bytes.get() as *mut u8).add(aligned)
        }
        unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {}
    }

    /// Reset the bump heap between evaluations. Safe to call between
    /// `eval` invocations but not re-entrantly.
    pub fn reset() {
        unsafe {
            *HEAP.next.get() = 0;
        }
    }
}

#[global_allocator]
static ALLOCATOR: bump::BumpAlloc = bump::BumpAlloc;

// ---------------------------------------------------------------------------
// ABI scratch + output buffers
// ---------------------------------------------------------------------------

const SCRATCH_CAP: usize = 16 * 1024;
static mut SCRATCH: [u8; SCRATCH_CAP] = [0; SCRATCH_CAP];

const OUTPUT_CAP: usize = 256;
static mut OUTPUT: [u8; OUTPUT_CAP] = [0; OUTPUT_CAP];

#[no_mangle]
pub extern "C" fn scratch_ptr() -> i32 {
    core::ptr::addr_of!(SCRATCH) as i32
}

#[no_mangle]
pub extern "C" fn scratch_cap() -> i32 {
    SCRATCH_CAP as i32
}

#[no_mangle]
pub extern "C" fn eval(len: i32) -> i64 {
    // Reset the bump heap so successive calls on the same instance get a
    // fresh allocation space. (Today the host creates a new instance per
    // call, but this keeps us robust if that ever changes.)
    bump::reset();

    if len < 0 || (len as usize) > SCRATCH_CAP {
        return write_err("bad length");
    }
    let source_bytes = unsafe { &SCRATCH[..len as usize] };
    let source = match core::str::from_utf8(source_bytes) {
        Ok(s) => s,
        Err(_) => return write_err("non-utf8 source"),
    };

    match run(source) {
        Ok(Value::Int(v)) => write_i64(v),
        Ok(Value::Bool(b)) => write_str(if b { "true" } else { "false" }),
        Err(msg) => write_err(&msg),
    }
}

fn encode(ptr: *const u8, len: usize) -> i64 {
    let p = ptr as u32 as i64;
    (p << 32) | (len as i64 & 0xFFFF_FFFF)
}

fn write_err(msg: &str) -> i64 {
    let out = unsafe { &mut *core::ptr::addr_of_mut!(OUTPUT) };
    let mut n = 0usize;
    for &b in b"ERR:" {
        if n >= OUTPUT_CAP {
            break;
        }
        out[n] = b;
        n += 1;
    }
    for b in msg.bytes() {
        if n >= OUTPUT_CAP {
            break;
        }
        out[n] = b;
        n += 1;
    }
    encode(out.as_ptr(), n)
}

fn write_str(s: &str) -> i64 {
    let out = unsafe { &mut *core::ptr::addr_of_mut!(OUTPUT) };
    let bytes = s.as_bytes();
    let n = core::cmp::min(bytes.len(), OUTPUT_CAP);
    out[..n].copy_from_slice(&bytes[..n]);
    encode(out.as_ptr(), n)
}

fn write_i64(v: i64) -> i64 {
    let out = unsafe { &mut *core::ptr::addr_of_mut!(OUTPUT) };
    let mut buf = [0u8; 24];
    let mut n: usize = 0;
    let negative = v < 0;
    let mut mag: u64 = if negative {
        (v as i128).unsigned_abs() as u64
    } else {
        v as u64
    };
    if mag == 0 {
        buf[n] = b'0';
        n += 1;
    } else {
        while mag > 0 {
            buf[n] = b'0' + ((mag % 10) as u8);
            mag /= 10;
            n += 1;
        }
    }
    let mut idx = 0usize;
    if negative {
        out[idx] = b'-';
        idx += 1;
    }
    while n > 0 {
        n -= 1;
        out[idx] = buf[n];
        idx += 1;
    }
    encode(out.as_ptr(), idx)
}

// ---------------------------------------------------------------------------
// Lexer
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum Token {
    Int(i64),
    Ident(String),
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Lt,
    Le,
    Gt,
    Ge,
    EqEq,
    NotEq,
    AndAnd,
    OrOr,
    Bang,
    LParen,
    RParen,
    Assign, // single `=`
    KwLet,
    KwIn,
    KwIf,
    KwThen,
    KwElse,
    KwTrue,
    KwFalse,
    Eof,
}

fn lex(input: &str) -> Result<Vec<Token>, String> {
    let bytes = input.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;

    while i < bytes.len() {
        let b = bytes[i];
        if b == b' ' || b == b'\t' || b == b'\n' || b == b'\r' {
            i += 1;
            continue;
        }
        // Line comments starting with #
        if b == b'#' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }

        // Integer literal.
        if b.is_ascii_digit() {
            let start = i;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            let slice = core::str::from_utf8(&bytes[start..i]).unwrap();
            let n: i64 = slice.parse().map_err(|_| "int too large".to_string())?;
            out.push(Token::Int(n));
            continue;
        }

        // Identifier or keyword.
        if b.is_ascii_alphabetic() || b == b'_' {
            let start = i;
            while i < bytes.len()
                && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_')
            {
                i += 1;
            }
            let ident = core::str::from_utf8(&bytes[start..i]).unwrap();
            out.push(match ident {
                "let" => Token::KwLet,
                "in" => Token::KwIn,
                "if" => Token::KwIf,
                "then" => Token::KwThen,
                "else" => Token::KwElse,
                "true" => Token::KwTrue,
                "false" => Token::KwFalse,
                _ => Token::Ident(ident.to_string()),
            });
            continue;
        }

        // Multi-char operators first, then single-char.
        let two = if i + 1 < bytes.len() {
            &bytes[i..i + 2]
        } else {
            &bytes[i..i]
        };
        let tok_two: Option<Token> = match two {
            b"==" => Some(Token::EqEq),
            b"!=" => Some(Token::NotEq),
            b"<=" => Some(Token::Le),
            b">=" => Some(Token::Ge),
            b"&&" => Some(Token::AndAnd),
            b"||" => Some(Token::OrOr),
            _ => None,
        };
        if let Some(t) = tok_two {
            out.push(t);
            i += 2;
            continue;
        }

        let tok_one = match b {
            b'+' => Token::Plus,
            b'-' => Token::Minus,
            b'*' => Token::Star,
            b'/' => Token::Slash,
            b'%' => Token::Percent,
            b'<' => Token::Lt,
            b'>' => Token::Gt,
            b'!' => Token::Bang,
            b'(' => Token::LParen,
            b')' => Token::RParen,
            b'=' => Token::Assign,
            _ => return Err("bad character in source".to_string()),
        };
        out.push(tok_one);
        i += 1;
    }

    out.push(Token::Eof);
    Ok(out)
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum Expr {
    Int(i64),
    Bool(bool),
    Var(String),
    Neg(Box<Expr>),
    Not(Box<Expr>),
    BinOp(BinOp, Box<Expr>, Box<Expr>),
    If(Box<Expr>, Box<Expr>, Box<Expr>),
    Let(String, Box<Expr>, Box<Expr>),
}

#[derive(Debug, Clone, Copy)]
enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
    NotEq,
    And,
    Or,
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> &Token {
        &self.tokens[self.pos]
    }
    fn bump(&mut self) -> Token {
        let t = self.tokens[self.pos].clone();
        self.pos += 1;
        t
    }
    fn eat(&mut self, pat: impl Fn(&Token) -> bool) -> bool {
        if pat(self.peek()) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn parse_expr(&mut self) -> Result<Expr, String> {
        match self.peek() {
            Token::KwLet => self.parse_let(),
            Token::KwIf => self.parse_if(),
            _ => self.parse_or(),
        }
    }

    fn parse_let(&mut self) -> Result<Expr, String> {
        self.bump(); // let
        let name = match self.bump() {
            Token::Ident(n) => n,
            _ => return Err("expected identifier after `let`".to_string()),
        };
        if !self.eat(|t| matches!(t, Token::Assign)) {
            return Err("expected `=` in let".to_string());
        }
        let value = self.parse_expr()?;
        if !self.eat(|t| matches!(t, Token::KwIn)) {
            return Err("expected `in` after let binding".to_string());
        }
        let body = self.parse_expr()?;
        Ok(Expr::Let(name, Box::new(value), Box::new(body)))
    }

    fn parse_if(&mut self) -> Result<Expr, String> {
        self.bump(); // if
        let cond = self.parse_expr()?;
        if !self.eat(|t| matches!(t, Token::KwThen)) {
            return Err("expected `then`".to_string());
        }
        let then_e = self.parse_expr()?;
        if !self.eat(|t| matches!(t, Token::KwElse)) {
            return Err("expected `else`".to_string());
        }
        let else_e = self.parse_expr()?;
        Ok(Expr::If(Box::new(cond), Box::new(then_e), Box::new(else_e)))
    }

    fn parse_or(&mut self) -> Result<Expr, String> {
        let mut lhs = self.parse_and()?;
        while matches!(self.peek(), Token::OrOr) {
            self.bump();
            let rhs = self.parse_and()?;
            lhs = Expr::BinOp(BinOp::Or, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn parse_and(&mut self) -> Result<Expr, String> {
        let mut lhs = self.parse_eq()?;
        while matches!(self.peek(), Token::AndAnd) {
            self.bump();
            let rhs = self.parse_eq()?;
            lhs = Expr::BinOp(BinOp::And, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn parse_eq(&mut self) -> Result<Expr, String> {
        let lhs = self.parse_cmp()?;
        let op = match self.peek() {
            Token::EqEq => Some(BinOp::Eq),
            Token::NotEq => Some(BinOp::NotEq),
            _ => None,
        };
        if let Some(op) = op {
            self.bump();
            let rhs = self.parse_cmp()?;
            return Ok(Expr::BinOp(op, Box::new(lhs), Box::new(rhs)));
        }
        Ok(lhs)
    }

    fn parse_cmp(&mut self) -> Result<Expr, String> {
        let lhs = self.parse_add()?;
        let op = match self.peek() {
            Token::Lt => Some(BinOp::Lt),
            Token::Le => Some(BinOp::Le),
            Token::Gt => Some(BinOp::Gt),
            Token::Ge => Some(BinOp::Ge),
            _ => None,
        };
        if let Some(op) = op {
            self.bump();
            let rhs = self.parse_add()?;
            return Ok(Expr::BinOp(op, Box::new(lhs), Box::new(rhs)));
        }
        Ok(lhs)
    }

    fn parse_add(&mut self) -> Result<Expr, String> {
        let mut lhs = self.parse_mul()?;
        loop {
            let op = match self.peek() {
                Token::Plus => BinOp::Add,
                Token::Minus => BinOp::Sub,
                _ => break,
            };
            self.bump();
            let rhs = self.parse_mul()?;
            lhs = Expr::BinOp(op, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn parse_mul(&mut self) -> Result<Expr, String> {
        let mut lhs = self.parse_unary()?;
        loop {
            let op = match self.peek() {
                Token::Star => BinOp::Mul,
                Token::Slash => BinOp::Div,
                Token::Percent => BinOp::Mod,
                _ => break,
            };
            self.bump();
            let rhs = self.parse_unary()?;
            lhs = Expr::BinOp(op, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn parse_unary(&mut self) -> Result<Expr, String> {
        match self.peek() {
            Token::Minus => {
                self.bump();
                Ok(Expr::Neg(Box::new(self.parse_unary()?)))
            }
            Token::Bang => {
                self.bump();
                Ok(Expr::Not(Box::new(self.parse_unary()?)))
            }
            _ => self.parse_atom(),
        }
    }

    fn parse_atom(&mut self) -> Result<Expr, String> {
        match self.bump() {
            Token::Int(n) => Ok(Expr::Int(n)),
            Token::KwTrue => Ok(Expr::Bool(true)),
            Token::KwFalse => Ok(Expr::Bool(false)),
            Token::Ident(name) => Ok(Expr::Var(name)),
            Token::LParen => {
                let inner = self.parse_expr()?;
                if !self.eat(|t| matches!(t, Token::RParen)) {
                    return Err("expected `)`".to_string());
                }
                Ok(inner)
            }
            other => Err({
                let mut s = String::from("unexpected token: ");
                s.push_str(&format_token(&other));
                s
            }),
        }
    }
}

fn format_token(t: &Token) -> String {
    match t {
        Token::Int(_) => "int".to_string(),
        Token::Ident(_) => "identifier".to_string(),
        Token::Plus => "+".to_string(),
        Token::Minus => "-".to_string(),
        Token::Star => "*".to_string(),
        Token::Slash => "/".to_string(),
        Token::Percent => "%".to_string(),
        Token::Lt => "<".to_string(),
        Token::Le => "<=".to_string(),
        Token::Gt => ">".to_string(),
        Token::Ge => ">=".to_string(),
        Token::EqEq => "==".to_string(),
        Token::NotEq => "!=".to_string(),
        Token::AndAnd => "&&".to_string(),
        Token::OrOr => "||".to_string(),
        Token::Bang => "!".to_string(),
        Token::LParen => "(".to_string(),
        Token::RParen => ")".to_string(),
        Token::Assign => "=".to_string(),
        Token::KwLet => "let".to_string(),
        Token::KwIn => "in".to_string(),
        Token::KwIf => "if".to_string(),
        Token::KwThen => "then".to_string(),
        Token::KwElse => "else".to_string(),
        Token::KwTrue => "true".to_string(),
        Token::KwFalse => "false".to_string(),
        Token::Eof => "<eof>".to_string(),
    }
}

// ---------------------------------------------------------------------------
// Evaluator
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
enum Value {
    Int(i64),
    Bool(bool),
}

fn eval_expr(e: &Expr, env: &mut Vec<(String, Value)>) -> Result<Value, String> {
    match e {
        Expr::Int(n) => Ok(Value::Int(*n)),
        Expr::Bool(b) => Ok(Value::Bool(*b)),
        Expr::Var(name) => {
            for (n, v) in env.iter().rev() {
                if n == name {
                    return Ok(*v);
                }
            }
            let mut msg = String::from("unbound variable `");
            msg.push_str(name);
            msg.push('`');
            Err(msg)
        }
        Expr::Neg(inner) => match eval_expr(inner, env)? {
            Value::Int(n) => Ok(Value::Int(n.checked_neg().ok_or("overflow")?)),
            _ => Err("type error: `-` on non-int".to_string()),
        },
        Expr::Not(inner) => match eval_expr(inner, env)? {
            Value::Bool(b) => Ok(Value::Bool(!b)),
            _ => Err("type error: `!` on non-bool".to_string()),
        },
        Expr::BinOp(op, l, r) => {
            let lv = eval_expr(l, env)?;
            // Short-circuit for logical ops so `false && <expr>` doesn't
            // evaluate the RHS (mirrors the rest of the world).
            if let BinOp::And = op {
                match lv {
                    Value::Bool(false) => return Ok(Value::Bool(false)),
                    Value::Bool(true) => {}
                    _ => return Err("type error: && on non-bool".to_string()),
                }
            }
            if let BinOp::Or = op {
                match lv {
                    Value::Bool(true) => return Ok(Value::Bool(true)),
                    Value::Bool(false) => {}
                    _ => return Err("type error: || on non-bool".to_string()),
                }
            }
            let rv = eval_expr(r, env)?;
            apply_binop(*op, lv, rv)
        }
        Expr::If(c, t, e) => match eval_expr(c, env)? {
            Value::Bool(true) => eval_expr(t, env),
            Value::Bool(false) => eval_expr(e, env),
            _ => Err("type error: if condition is not a bool".to_string()),
        },
        Expr::Let(name, value, body) => {
            let v = eval_expr(value, env)?;
            env.push((name.clone(), v));
            let result = eval_expr(body, env);
            env.pop();
            result
        }
    }
}

fn apply_binop(op: BinOp, l: Value, r: Value) -> Result<Value, String> {
    match (op, l, r) {
        (BinOp::Add, Value::Int(a), Value::Int(b)) => {
            Ok(Value::Int(a.checked_add(b).ok_or("overflow")?))
        }
        (BinOp::Sub, Value::Int(a), Value::Int(b)) => {
            Ok(Value::Int(a.checked_sub(b).ok_or("overflow")?))
        }
        (BinOp::Mul, Value::Int(a), Value::Int(b)) => {
            Ok(Value::Int(a.checked_mul(b).ok_or("overflow")?))
        }
        (BinOp::Div, Value::Int(a), Value::Int(b)) => {
            if b == 0 {
                return Err("div by zero".to_string());
            }
            Ok(Value::Int(a.checked_div(b).ok_or("overflow")?))
        }
        (BinOp::Mod, Value::Int(a), Value::Int(b)) => {
            if b == 0 {
                return Err("mod by zero".to_string());
            }
            Ok(Value::Int(a.checked_rem(b).ok_or("overflow")?))
        }
        (BinOp::Lt, Value::Int(a), Value::Int(b)) => Ok(Value::Bool(a < b)),
        (BinOp::Le, Value::Int(a), Value::Int(b)) => Ok(Value::Bool(a <= b)),
        (BinOp::Gt, Value::Int(a), Value::Int(b)) => Ok(Value::Bool(a > b)),
        (BinOp::Ge, Value::Int(a), Value::Int(b)) => Ok(Value::Bool(a >= b)),
        (BinOp::Eq, Value::Int(a), Value::Int(b)) => Ok(Value::Bool(a == b)),
        (BinOp::Eq, Value::Bool(a), Value::Bool(b)) => Ok(Value::Bool(a == b)),
        (BinOp::NotEq, Value::Int(a), Value::Int(b)) => Ok(Value::Bool(a != b)),
        (BinOp::NotEq, Value::Bool(a), Value::Bool(b)) => Ok(Value::Bool(a != b)),
        (BinOp::And, Value::Bool(a), Value::Bool(b)) => Ok(Value::Bool(a && b)),
        (BinOp::Or, Value::Bool(a), Value::Bool(b)) => Ok(Value::Bool(a || b)),
        _ => Err("type error: binop operand types don't match".to_string()),
    }
}

// ---------------------------------------------------------------------------
// Orchestration
// ---------------------------------------------------------------------------

fn run(source: &str) -> Result<Value, String> {
    let tokens = lex(source)?;
    let mut p = Parser { tokens, pos: 0 };
    let expr = p.parse_expr()?;
    if !matches!(p.peek(), Token::Eof) {
        return Err("extra tokens after expression".to_string());
    }
    let mut env: Vec<(String, Value)> = Vec::new();
    eval_expr(&expr, &mut env)
}

