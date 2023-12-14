//! This is an entire parser and interpreter for a dynamically-typed Rust-like expression-oriented
//! programming language. This is taken from the chumsky examples.
//!
//! The goal of this mini language is the definition of LLM supported software. This should produce
//! an AST that can be translated to our graph representation.

use ariadne::{Color, Fmt, Label, Report, ReportKind, Source};
use chumsky::{prelude::*, stream::Stream};
use std::io::Cursor;
use std::{collections::HashMap, env, fmt, fs};

pub type Span = std::ops::Range<usize>;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Token {
    Null,
    Bool(bool),
    Num(String),
    Str(String),
    Op(String),
    Ctrl(char),
    Ident(String),
    Fn,
    Use,
    Let,
    Print,
    If,
    Else,
}

impl fmt::Display for Token {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Token::Null => write!(f, "null"),
            Token::Bool(x) => write!(f, "{}", x),
            Token::Num(n) => write!(f, "{}", n),
            Token::Str(s) => write!(f, "{}", s),
            Token::Op(s) => write!(f, "{}", s),
            Token::Ctrl(c) => write!(f, "{}", c),
            Token::Ident(s) => write!(f, "{}", s),
            Token::Fn => write!(f, "fn"),
            Token::Use => write!(f, "use"),
            Token::Let => write!(f, "let"),
            Token::Print => write!(f, "print"),
            Token::If => write!(f, "if"),
            Token::Else => write!(f, "else"),
        }
    }
}

fn lexer() -> impl Parser<char, Vec<(Token, Span)>, Error = Simple<char>> {
    // A parser for numbers
    let num = text::int(10)
        .chain::<char, _, _>(just('.').chain(text::digits(10)).or_not().flatten())
        .collect::<String>()
        .map(Token::Num);

    // A parser for strings
    let str_ = just('"')
        .ignore_then(filter(|c| *c != '"').repeated())
        .then_ignore(just('"'))
        .collect::<String>()
        .map(Token::Str);

    // A parser for operators
    let op = one_of("+-*/!=")
        .repeated()
        .at_least(1)
        .collect::<String>()
        .map(|op| Token::Op(op))
        .or(just("|>").map(|_| Token::Op("|>".to_string())));

    // A parser for control characters (delimiters, semicolons, etc.)
    let ctrl = one_of("()[]{};,").map(|c| Token::Ctrl(c));

    // A parser for identifiers and keywords
    let ident = text::ident().map(|ident: String| match ident.as_str() {
        "fn" => Token::Fn,
        "use" => Token::Use,
        "let" => Token::Let,
        "print" => Token::Print,
        "if" => Token::If,
        "else" => Token::Else,
        "true" => Token::Bool(true),
        "false" => Token::Bool(false),
        "null" => Token::Null,
        _ => Token::Ident(ident),
    });

    // A single token can be one of the above
    let token = num
        .or(str_)
        .or(op)
        .or(ctrl)
        .or(ident)
        .recover_with(skip_then_retry_until([]));

    let comment = just("//").then(take_until(just('\n'))).padded();

    token
        .map_with_span(|tok, span| (tok, span))
        .padded_by(comment.repeated())
        .padded()
        .repeated()
}

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    List(Vec<Value>),
    Func(String),
}

impl Value {
    pub(crate) fn num(self, span: Span) -> Result<f64, Error> {
        if let Value::Num(x) = self {
            Ok(x)
        } else {
            Err(Error {
                span,
                msg: format!("'{}' is not a number", self),
            })
        }
    }
}

impl std::fmt::Display for Value {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Self::Null => write!(f, "null"),
            Self::Bool(x) => write!(f, "{}", x),
            Self::Num(x) => write!(f, "{}", x),
            Self::Str(x) => write!(f, "{}", x),
            Self::List(xs) => write!(
                f,
                "[{}]",
                xs.iter()
                    .map(|x| x.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Self::Func(name) => write!(f, "<function: {}>", name),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum BinaryOp {
    Add,
    Sub,
    Mul,
    Div,
    Eq,
    NotEq,
    PipeOp,
}

pub type Spanned<T> = (T, Span);

// An expression node in the AST. Children are spanned so we can generate useful runtime errors.
#[derive(Debug, PartialEq)]
pub enum Expr {
    Error,
    Value(Value),
    List(Vec<Spanned<Self>>),
    Local(String),
    Let(String, Box<Spanned<Self>>, Box<Spanned<Self>>),
    // Used to chain blocks together.
    Then(Box<Spanned<Self>>, Box<Spanned<Self>>),
    Binary(Box<Spanned<Self>>, BinaryOp, Box<Spanned<Self>>),
    Call(Box<Spanned<Self>>, Vec<Spanned<Self>>),
    If(Box<Spanned<Self>>, Box<Spanned<Self>>, Box<Spanned<Self>>),
    Print(Box<Spanned<Self>>),
}

// An import node in the AST.
#[derive(Debug, PartialEq)]
pub struct Import {
    pub path: String,
    pub lang: String,
    pub alias: String,
}

// A function node in the AST.
#[derive(Debug, PartialEq)]
pub struct Func {
    pub args: Vec<String>,
    pub body: Spanned<Expr>,
}

fn expr_parser() -> impl Parser<Token, Spanned<Expr>, Error = Simple<Token>> + Clone {
    recursive(|expr| {
        let raw_expr = recursive(|raw_expr| {
            let val = select! {
                Token::Null => Expr::Value(Value::Null),
                Token::Bool(x) => Expr::Value(Value::Bool(x)),
                Token::Num(n) => Expr::Value(Value::Num(n.parse().unwrap())),
                Token::Str(s) => Expr::Value(Value::Str(s)),
            }
            .labelled("value");

            let ident = select! { Token::Ident(ident) => ident.clone() }.labelled("identifier");

            // A list of expressions
            let items = expr
                .clone()
                .separated_by(just(Token::Ctrl(',')))
                .allow_trailing();

            // A let expression
            let let_ = just(Token::Let)
                .ignore_then(ident)
                .then_ignore(just(Token::Op("=".to_string())))
                .then(raw_expr)
                .then_ignore(just(Token::Ctrl(';')))
                .then(expr.clone())
                .map(|((name, val), body)| Expr::Let(name, Box::new(val), Box::new(body)));

            let list = items
                .clone()
                .delimited_by(just(Token::Ctrl('[')), just(Token::Ctrl(']')))
                .map(Expr::List);

            // 'Atoms' are expressions that contain no ambiguity
            let atom = val
                .or(ident.map(Expr::Local))
                .or(let_)
                .or(list)
                // In Nano Rust, `print` is just a keyword, just like Python 2, for simplicity
                .or(just(Token::Print)
                    .ignore_then(
                        expr.clone()
                            .delimited_by(just(Token::Ctrl('(')), just(Token::Ctrl(')'))),
                    )
                    .map(|expr| Expr::Print(Box::new(expr))))
                .map_with_span(|expr, span| (expr, span))
                // Atoms can also just be normal expressions, but surrounded with parentheses
                .or(expr
                    .clone()
                    .delimited_by(just(Token::Ctrl('(')), just(Token::Ctrl(')'))))
                // Attempt to recover anything that looks like a parenthesised expression but contains errors
                .recover_with(nested_delimiters(
                    Token::Ctrl('('),
                    Token::Ctrl(')'),
                    [
                        (Token::Ctrl('['), Token::Ctrl(']')),
                        (Token::Ctrl('{'), Token::Ctrl('}')),
                    ],
                    |span| (Expr::Error, span),
                ))
                // Attempt to recover anything that looks like a list but contains errors
                .recover_with(nested_delimiters(
                    Token::Ctrl('['),
                    Token::Ctrl(']'),
                    [
                        (Token::Ctrl('('), Token::Ctrl(')')),
                        (Token::Ctrl('{'), Token::Ctrl('}')),
                    ],
                    |span| (Expr::Error, span),
                ));

            // Function calls have very high precedence so we prioritise them
            let call = atom
                .then(
                    items
                        .delimited_by(just(Token::Ctrl('(')), just(Token::Ctrl(')')))
                        .map_with_span(|args, span: Span| (args, span))
                        .repeated(),
                )
                .foldl(|f, args| {
                    let span = f.1.start..args.1.end;
                    (Expr::Call(Box::new(f), args.0), span)
                });

            // Product ops (multiply and divide) have equal precedence
            let op = just(Token::Op("*".to_string()))
                .to(BinaryOp::Mul)
                .or(just(Token::Op("/".to_string())).to(BinaryOp::Div));
            let product = call
                .clone()
                .then(op.then(call).repeated())
                .foldl(|a, (op, b)| {
                    let span = a.1.start..b.1.end;
                    (Expr::Binary(Box::new(a), op, Box::new(b)), span)
                });

            // Sum ops (add and subtract) have equal precedence
            let op = just(Token::Op("+".to_string()))
                .to(BinaryOp::Add)
                .or(just(Token::Op("-".to_string())).to(BinaryOp::Sub));
            let sum = product
                .clone()
                .then(op.then(product).repeated())
                .foldl(|a, (op, b)| {
                    let span = a.1.start..b.1.end;
                    (Expr::Binary(Box::new(a), op, Box::new(b)), span)
                });

            // Comparison ops (equal, not-equal) have equal precedence
            let op = just(Token::Op("==".to_string()))
                .to(BinaryOp::Eq)
                .or(just(Token::Op("!=".to_string())).to(BinaryOp::NotEq));
            let compare = sum
                .clone()
                .then(op.then(sum).repeated())
                .foldl(|a, (op, b)| {
                    let span = a.1.start..b.1.end;
                    (Expr::Binary(Box::new(a), op, Box::new(b)), span)
                });

            // Pipe operator ops have equal precedence
            let op = just(Token::Op("|>".to_string())).to(BinaryOp::PipeOp);
            let pipe = compare
                .clone()
                .then(op.then(compare).repeated())
                .foldl(|a, (op, b)| {
                    let span = a.1.start..b.1.end;
                    (Expr::Binary(Box::new(a), op, Box::new(b)), span)
                });

            pipe
        });

        // Blocks are expressions but delimited with braces
        let block = expr
            .clone()
            .delimited_by(just(Token::Ctrl('{')), just(Token::Ctrl('}')))
            // Attempt to recover anything that looks like a block but contains errors
            .recover_with(nested_delimiters(
                Token::Ctrl('{'),
                Token::Ctrl('}'),
                [
                    (Token::Ctrl('('), Token::Ctrl(')')),
                    (Token::Ctrl('['), Token::Ctrl(']')),
                ],
                |span| (Expr::Error, span),
            ));

        let if_ = recursive(|if_| {
            just(Token::If)
                .ignore_then(expr.clone())
                .then(block.clone())
                .then(
                    just(Token::Else)
                        .ignore_then(block.clone().or(if_))
                        .or_not(),
                )
                .map_with_span(|((cond, a), b), span: Span| {
                    (
                        Expr::If(
                            Box::new(cond),
                            Box::new(a),
                            Box::new(match b {
                                Some(b) => b,
                                // If an `if` expression has no trailing `else` block, we magic up one that just produces null
                                None => (Expr::Value(Value::Null), span.clone()),
                            }),
                        ),
                        span,
                    )
                })
        });

        // Both blocks and `if` are 'block expressions' and can appear in the place of statements
        let block_expr = block.or(if_).labelled("block");

        let block_chain = block_expr
            .clone()
            .then(block_expr.clone().repeated())
            .foldl(|a, b| {
                let span = a.1.start..b.1.end;
                (Expr::Then(Box::new(a), Box::new(b)), span)
            });

        block_chain
            // Expressions, chained by semicolons, are statements
            .or(raw_expr.clone())
            .then(just(Token::Ctrl(';')).ignore_then(expr.or_not()).repeated())
            .foldl(|a, b| {
                // This allows creating a span that covers the entire Then expression.
                // b_end is the end of b if it exists, otherwise it is the end of a.
                let a_start = a.1.start;
                let b_end = b.as_ref().map(|b| b.1.end).unwrap_or(a.1.end);
                (
                    Expr::Then(
                        Box::new(a),
                        Box::new(match b {
                            Some(b) => b,
                            // Since there is no b expression then its span is empty.
                            None => (Expr::Value(Value::Null), b_end..b_end),
                        }),
                    ),
                    a_start..b_end,
                )
            })
    })
}

fn funcs_parser() -> impl Parser<Token, HashMap<String, Func>, Error = Simple<Token>> + Clone {
    let ident = filter_map(|span, tok| match tok {
        Token::Ident(ident) => Ok(ident.clone()),
        _ => Err(Simple::expected_input_found(span, Vec::new(), Some(tok))),
    });

    // Argument lists are just identifiers separated by commas, surrounded by parentheses
    let args = ident
        .clone()
        .separated_by(just(Token::Ctrl(',')))
        .allow_trailing()
        .delimited_by(just(Token::Ctrl('(')), just(Token::Ctrl(')')))
        .labelled("function args");

    let func = just(Token::Fn)
        .ignore_then(
            ident
                .map_with_span(|name, span| (name, span))
                .labelled("function name"),
        )
        .then(args)
        .then(
            expr_parser()
                .delimited_by(just(Token::Ctrl('{')), just(Token::Ctrl('}')))
                // Attempt to recover anything that looks like a function body but contains errors
                .recover_with(nested_delimiters(
                    Token::Ctrl('{'),
                    Token::Ctrl('}'),
                    [
                        (Token::Ctrl('('), Token::Ctrl(')')),
                        (Token::Ctrl('['), Token::Ctrl(']')),
                    ],
                    |span| (Expr::Error, span),
                )),
        )
        .map(|((name, args), body)| (name, Func { args, body }))
        .labelled("function");

    func.repeated()
        .try_map(|fs, _| {
            let mut funcs = HashMap::new();
            for ((name, name_span), f) in fs {
                if funcs.insert(name.clone(), f).is_some() {
                    return Err(Simple::custom(
                        name_span.clone(),
                        format!("Function '{}' already exists", name),
                    ));
                }
            }
            Ok(funcs)
        })
        .then_ignore(end())
}

fn import_parser() -> impl Parser<Token, Spanned<Import>, Error = Simple<Token>> + Clone {
    let string_literal = select! { Token::Str(s) => s.clone() }.labelled("string literal");

    let import = just(Token::Use)
        .ignore_then(just(Token::Ident("import".to_string())))
        .then_ignore(just(Token::Ctrl('(')))
        .ignore_then(string_literal.clone()) // Source path
        .then_ignore(just(Token::Ctrl(',')))
        .then(string_literal) // Language
        .then_ignore(just(Token::Ctrl(')')))
        .then_ignore(just(Token::Ident("as".to_string())))
        .then(select! { Token::Ident(ident) => ident.clone() }) // Alias
        .then_ignore(just(Token::Ctrl(';')))
        .map_with_span(|((path, lang), alias), span| (Import { path, lang, alias }, span))
        .labelled("import statement");

    import
}

pub struct Error {
    pub span: Span,
    pub msg: String,
}

fn file_parser() -> impl Parser<Token, Program, Error = Simple<Token>> + Clone {
    // Define the parsers for imports and functions
    let import_p = import_parser();
    let funcs_p = funcs_parser();

    // The main parser should parse a sequence of imports and functions
    import_p
        .clone()
        .repeated() // Allows for multiple import statements
        .then(funcs_p.clone())
        .map(|(imports, funcs)| Program { imports, funcs })
        // Handle the end of the input
        .then_ignore(end())
}

// Define a structure to hold the parsed program
#[derive(Debug)]
pub struct Program {
    pub imports: Vec<Spanned<Import>>,
    pub funcs: HashMap<String, Func>,
}

pub fn parse(src: String) -> Result<Option<Program>, Vec<String>> {
    let (tokens, mut errs) = lexer().parse_recovery(src.as_str());

    let (ast, parse_errs) = if let Some(tokens) = tokens {
        //dbg!(tokens);
        let len = src.chars().count();
        let (ast, parse_errs) =
            file_parser().parse_recovery(Stream::from_iter(len..len + 1, tokens.into_iter()));

        (ast, parse_errs)
    } else {
        (None, Vec::new())
    };

    let formatted_errs = errs
        .into_iter()
        .map(|e| e.map(|c| c.to_string()))
        .chain(parse_errs.into_iter().map(|e| e.map(|tok| tok.to_string())))
        .map(|e| {
            let report = Report::build(ReportKind::Error, (), e.span().start);

            let report = match e.reason() {
                chumsky::error::SimpleReason::Unclosed { span, delimiter } => report
                    .with_message(format!(
                        "Unclosed delimiter {}",
                        delimiter.fg(Color::Yellow)
                    ))
                    .with_label(
                        Label::new(span.clone())
                            .with_message(format!(
                                "Unclosed delimiter {}",
                                delimiter.fg(Color::Yellow)
                            ))
                            .with_color(Color::Yellow),
                    )
                    .with_label(
                        Label::new(e.span())
                            .with_message(format!(
                                "Must be closed before this {}",
                                e.found()
                                    .unwrap_or(&"end of file".to_string())
                                    .fg(Color::Red)
                            ))
                            .with_color(Color::Red),
                    ),
                chumsky::error::SimpleReason::Unexpected => report
                    .with_message(format!(
                        "{}, expected {}",
                        if e.found().is_some() {
                            "Unexpected token in input"
                        } else {
                            "Unexpected end of input"
                        },
                        if e.expected().len() == 0 {
                            "something else".to_string()
                        } else {
                            e.expected()
                                .map(|expected| match expected {
                                    Some(expected) => expected.to_string(),
                                    None => "end of input".to_string(),
                                })
                                .collect::<Vec<_>>()
                                .join(", ")
                        }
                    ))
                    .with_label(
                        Label::new(e.span())
                            .with_message(format!(
                                "Unexpected token {}",
                                e.found()
                                    .unwrap_or(&"end of file".to_string())
                                    .fg(Color::Red)
                            ))
                            .with_color(Color::Red),
                    ),
                chumsky::error::SimpleReason::Custom(msg) => report.with_message(msg).with_label(
                    Label::new(e.span())
                        .with_message(format!("{}", msg.fg(Color::Red)))
                        .with_color(Color::Red),
                ),
            };

            let mut buffer = Cursor::new(Vec::new());
            report
                .finish()
                .write(Source::from(&src), &mut buffer)
                .unwrap();
            let s = String::from_utf8(buffer.into_inner()).expect("Found invalid UTF-8");
            s
        });

    Ok(ast)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn test_parsing_simple_program() {
        let result = parse(
            r#"
        fn main() {
            let x = 1;
            let y = 2;
            print(x + y);
            x |> 
            y |> 
            print(x + y);
        }
    "#
            .to_string(),
        );

        assert_eq!(result.is_ok(), true);
        let result = result.unwrap();
        assert_eq!(result.is_some(), true);
        let result = result.unwrap();

        if let Some(Func { args, body }) = result.funcs.get("main") {
            let (Expr::Let(n, bx, rest), _) = body  else { panic!("Unexpected expression structure") };
            let (Expr::Value(Value::Num(v)), _) = **bx else { panic!("Unexpected expression structure") };
            assert_eq!(n, &"x".to_string());
            assert_eq!(v, 1.0);

            let (Expr::Let(ref n, ref bx, ref rest), _) = **rest else { panic!("Unexpected expression structure") };
            let (Expr::Value(Value::Num(v)), _) = **bx else { panic!("Unexpected expression structure") };
            assert_eq!(n, &"y".to_string());
            assert_eq!(v, 2.0);

            let (Expr::Then(ref left, ref right), _) = **rest else { panic!("Unexpected expression structure") };
            let (Expr::Print(ref bx), _) = **left else { panic!("Unexpected expression structure") };
            let (Expr::Binary(ref left, ref op, ref right), _) = **bx else { panic!("Unexpected expression structure") };
            let (Expr::Local(ref n), _) = **left else { panic!("Unexpected expression structure") };
            let (Expr::Local(ref n2), _) = **right else { panic!("Unexpected expression structure") };
            assert_eq!(n, &"x".to_string());
            assert_eq!(n2, &"y".to_string());
            assert_eq!(op, &BinaryOp::Add);
        } else {
            panic!("Function 'main' not found");
        }
    }

    #[test]
    fn test_valid_import() {
        let parser = import_parser().then_ignore(end());
        let test_input = "use import(\"src/js/example\", \"javascript\") as z;";
        let len = test_input.chars().count();
        let (tokens, mut errs) = lexer().parse_recovery(test_input);
        let parsed = parser
            .parse(Stream::from_iter(len..len + 1, tokens.unwrap().into_iter()))
            .unwrap()
            .0;

        let expected = Import {
            path: "src/js/example".to_string(),
            lang: "javascript".to_string(),
            alias: "z".to_string(),
        };

        assert_eq!(parsed, expected);
    }

    #[test]
    fn test_invalid_import_missing_as() {
        let parser = import_parser().then_ignore(end());
        let test_input = "use import(\"src/js/example\", \"javascript\");";
        let len = test_input.chars().count();
        let (tokens, mut errs) = lexer().parse_recovery(test_input);
        let parsed = parser.parse(Stream::from_iter(len..len + 1, tokens.unwrap().into_iter()));

        assert!(parsed.is_err());
    }

    #[test]
    fn test_invalid_import_missing_quotes() {
        let parser = import_parser().then_ignore(end());
        let test_input = "use import(src/js/example, javascript) as z;";
        let len = test_input.chars().count();
        let (tokens, mut errs) = lexer().parse_recovery(test_input);
        let parsed = parser.parse(Stream::from_iter(len..len + 1, tokens.unwrap().into_iter()));
        assert!(parsed.is_err());
    }

    #[test]
    fn test_import_with_extra_tokens() {
        let parser = import_parser().then_ignore(end());
        let test_input = "use import(\"src/js/example\", \"javascript\") as z extra;";
        let len = test_input.chars().count();
        let (tokens, mut errs) = lexer().parse_recovery(test_input);
        let parsed = parser.parse(Stream::from_iter(len..len + 1, tokens.unwrap().into_iter()));

        assert!(parsed.is_err());
    }

    #[test]
    fn test_parsing_program_with_imports() {
        let result = parse(
            r#"
            use import("src/js/example", "javascript") as z;
            use import("src/prompt/example", "prompt") as p;
            use import("src/prompt/example", "text") as t;
            use import("src/py/example", "python") as py;
            
            fn main() {
                let x = 1;
                let y = 2;
                print(x + y);
            }
        "#
            .to_string(),
        );

        let result = result.unwrap();
        let result = result.unwrap();

        assert_eq!(
            Import {
                path: "src/js/example".into(),
                lang: "javascript".into(),
                alias: "z".into()
            },
            result.imports[0].0
        );
        assert_eq!(
            Import {
                path: "src/prompt/example".into(),
                lang: "prompt".into(),
                alias: "p".into()
            },
            result.imports[1].0
        );
        assert_eq!(
            Import {
                path: "src/prompt/example".into(),
                lang: "text".into(),
                alias: "t".into()
            },
            result.imports[2].0
        );
        assert_eq!(
            Import {
                path: "src/py/example".into(),
                lang: "python".into(),
                alias: "py".into()
            },
            result.imports[3].0
        );

        if let Some(Func { args, body }) = result.funcs.get("main") {
            let (Expr::Let(n, bx, rest), _) = body  else { panic!("Unexpected expression structure") };
            let (Expr::Value(Value::Num(v)), _) = **bx else { panic!("Unexpected expression structure") };
            assert_eq!(n, &"x".to_string());
            assert_eq!(v, 1.0);

            let (Expr::Let(ref n, ref bx, ref rest), _) = **rest else { panic!("Unexpected expression structure") };
            let (Expr::Value(Value::Num(v)), _) = **bx else { panic!("Unexpected expression structure") };
            assert_eq!(n, &"y".to_string());
            assert_eq!(v, 2.0);

            let (Expr::Then(ref left, ref right), _) = **rest else { panic!("Unexpected expression structure") };
            let (Expr::Print(ref bx), _) = **left else { panic!("Unexpected expression structure") };
            let (Expr::Binary(ref left, ref op, ref right), _) = **bx else { panic!("Unexpected expression structure") };
            let (Expr::Local(ref n), _) = **left else { panic!("Unexpected expression structure") };
            let (Expr::Local(ref n2), _) = **right else { panic!("Unexpected expression structure") };
            assert_eq!(n, &"x".to_string());
            assert_eq!(n2, &"y".to_string());
            assert_eq!(op, &BinaryOp::Add);
        } else {
            panic!("Function 'main' not found");
        }
    }
}
