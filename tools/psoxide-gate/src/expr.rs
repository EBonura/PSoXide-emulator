//! Integer expressions over guest RAM, for `kind = "expr"` conditions.
//!
//! A guest should publish raw values (positions, hit points, timers, phases) and let the gate
//! derive "nearest foe", "can strike" or "a window is open" from them, instead of linking that
//! maths into the shipping disc. The language is deliberately small and guest-like: every value
//! is a signed 64-bit integer (wrapping, never a panic), there is no float, and a given
//! expression over a given RAM image always gives the same answer.
//!
//! ```text
//! expr    := cond
//! cond    := or ( '?' expr ':' expr )?
//! or      := and ( '||' and )*                 short-circuit, yields 0 or 1
//! and     := cmp ( '&&' cmp )*                 short-circuit, yields 0 or 1
//! cmp     := bitor ( ( '==' | '!=' | '<' | '<=' | '>' | '>=' ) bitor )?   (not chainable)
//! bitor   := bitxor ( '|' bitxor )*
//! bitxor  := bitand ( '^' bitand )*
//! bitand  := shift ( '&' shift )*
//! shift   := sum ( ( '<<' | '>>' ) sum )*      '>>' is arithmetic; the count must be 0..=63
//! sum     := product ( ( '+' | '-' ) product )*
//! product := unary ( ( '*' | '/' | '%' ) unary )*    '/' and '%' truncate; zero is an error
//! unary   := ( '-' | '!' ) unary | primary
//! primary := NUMBER | '(' expr ')' | VAR | READ | CALL | REDUCE | LET
//! READ    := ( 'u8' | 'i8' | 'u16' | 'i16' | 'u32' | 'i32' ) '(' address ')'
//! CALL    := 'abs' '(' expr ')' | ( 'min' | 'max' ) '(' expr ( ',' expr )+ ')'
//! REDUCE  := ( 'any' | 'all' | 'count' | 'sum' | 'min' | 'max' )
//!            '(' VAR 'in' expr '..' expr ':' expr ')'
//! LET     := 'let' VAR '=' expr ( ',' VAR '=' expr )* 'in' expr    (extends as far right as it can)
//! ```
//!
//! A number is decimal or `0x` hex with optional `_` separators. A comparison or logical
//! operator yields 1 or 0, and an expression holds when its value is not 0.
//!
//! Memory is read only through `READ`, whose argument is an *address expression*. A symbol
//! (a link-map name, a `[addr]` table entry, or a name quoted in backticks for odd characters)
//! is its address and may appear only inside a read: `i32(HK_PLAYER_X)`. A bare symbol anywhere
//! else is an error, so the common slip of comparing an address instead of its contents cannot
//! pass. Inside a read of width W, `base[i]` means `base + i * W` (an array of that type);
//! records use explicit arithmetic: `i32(ACTORS + 40 * i + 8)`.
//!
//! `any`/`all`/`count`/`sum`/`min`/`max` run a variable over the half-open range `lo..hi` (both
//! bounds are evaluated once, so `0..u8(COUNT)` works). `any` and `all` stop early. An empty
//! range gives `any` 0, `all` 1, `count` and `sum` 0, `min` 2147483647 and `max` -2147483648
//! (the guest's i32 bounds). A `let` binding or loop variable takes one of [`MAX_VARS`] slots while in scope, and one evaluation may run at
//! most [`LOOP_BUDGET`] iterations in total.
//!
//! Evaluation is a tree walk over addresses already bound to the symbol table, so a per-tick
//! condition costs a few dozen nanoseconds.

use std::fmt::Write as _;

/// Most names (loop variables and `let` bindings) in scope at once.
pub const MAX_VARS: usize = 8;
/// Most loop iterations one evaluation may run, a guard against `0..4000000000`.
pub const LOOP_BUDGET: u32 = 100_000;
/// Deepest parenthesis and operator nesting the parser accepts.
const MAX_DEPTH: usize = 64;
/// Reads remembered for the explanation of a result.
const MAX_TRACED_READS: usize = 16;

/// Guest memory as an expression sees it.
pub trait Memory {
    /// The `width` bytes (1, 2 or 4) at `addr` as a little-endian unsigned number, or `None`
    /// when the address is outside main RAM.
    fn load(&self, addr: u32, width: u8) -> Option<u32>;
}

/// How a read widens its bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    U8,
    I8,
    U16,
    I16,
    U32,
    I32,
}

impl Kind {
    fn from_name(name: &str) -> Option<Kind> {
        Some(match name {
            "u8" => Kind::U8,
            "i8" => Kind::I8,
            "u16" => Kind::U16,
            "i16" => Kind::I16,
            "u32" => Kind::U32,
            "i32" => Kind::I32,
            _ => return None,
        })
    }

    fn width(self) -> u8 {
        match self {
            Kind::U8 | Kind::I8 => 1,
            Kind::U16 | Kind::I16 => 2,
            Kind::U32 | Kind::I32 => 4,
        }
    }

    fn widen(self, raw: u32) -> i64 {
        match self {
            Kind::U8 | Kind::U16 | Kind::U32 => i64::from(raw),
            Kind::I8 => i64::from(raw as u8 as i8),
            Kind::I16 => i64::from(raw as u16 as i16),
            Kind::I32 => i64::from(raw as i32),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Binary {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Shl,
    Shr,
    BitAnd,
    BitOr,
    BitXor,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reduce {
    Any,
    All,
    Count,
    Sum,
    Min,
    Max,
}

#[derive(Debug, Clone, PartialEq)]
enum Node {
    Num(i64),
    /// A loop variable, by nesting slot.
    Var(usize),
    /// A symbol's address, until [`Expr::bind`] replaces it with a number.
    Sym(String),
    Neg(Box<Node>),
    Not(Box<Node>),
    Binary(Binary, Box<Node>, Box<Node>),
    And(Box<Node>, Box<Node>),
    Or(Box<Node>, Box<Node>),
    Cond(Box<Node>, Box<Node>, Box<Node>),
    Read(Kind, Box<Node>),
    /// `let name = value in body`, the name living in `slot`.
    Let {
        slot: usize,
        value: Box<Node>,
        body: Box<Node>,
    },
    Abs(Box<Node>),
    Min(Vec<Node>),
    Max(Vec<Node>),
    Reduce {
        op: Reduce,
        slot: usize,
        lo: Box<Node>,
        hi: Box<Node>,
        body: Box<Node>,
    },
}

// ---------------------------------------------------------------------------------------------
// Lexer

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Num(i64),
    Ident(String),
    /// A backtick-quoted symbol name.
    Quoted(String),
    Punct(&'static str),
}

const PUNCT: &[&str] = &[
    "<<", ">>", "<=", ">=", "==", "!=", "&&", "||", "..", "=", "+", "-", "*", "/", "%", "&", "|",
    "^", "<", ">", "!", "(", ")", "[", "]", ",", ":", "?",
];

fn lex(src: &str) -> Result<Vec<(Tok, usize)>, String> {
    let bytes = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c.is_ascii_whitespace() {
            i += 1;
        } else if c.is_ascii_digit() {
            let start = i;
            let (radix, mut j) = if c == b'0' && matches!(bytes.get(i + 1), Some(b'x' | b'X')) {
                (16, i + 2)
            } else {
                (10, i)
            };
            let digits_at = j;
            while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                j += 1;
            }
            let digits: String = src[digits_at..j].chars().filter(|c| *c != '_').collect();
            let value = i64::from_str_radix(&digits, radix)
                .map_err(|_| format!("bad number `{}` at column {}", &src[start..j], start + 1))?;
            out.push((Tok::Num(value), start));
            i = j;
        } else if c.is_ascii_alphabetic() || c == b'_' {
            let start = i;
            let mut j = i;
            loop {
                while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                    j += 1;
                }
                // `a::b` is one name (a Rust path in the link map).
                if src[j..].starts_with("::")
                    && bytes
                        .get(j + 2)
                        .is_some_and(|b| b.is_ascii_alphabetic() || *b == b'_')
                {
                    j += 2;
                } else {
                    break;
                }
            }
            out.push((Tok::Ident(src[start..j].to_string()), start));
            i = j;
        } else if c == b'`' {
            let Some(len) = src[i + 1..].find('`') else {
                return Err(format!("unterminated ` at column {}", i + 1));
            };
            if len == 0 {
                return Err(format!("empty `` name at column {}", i + 1));
            }
            out.push((Tok::Quoted(src[i + 1..i + 1 + len].to_string()), i));
            i += len + 2;
        } else if let Some(p) = PUNCT.iter().find(|p| src[i..].starts_with(**p)) {
            out.push((Tok::Punct(p), i));
            i += p.len();
        } else {
            let ch = src[i..].chars().next().unwrap_or('?');
            return Err(format!("unexpected `{ch}` at column {}", i + 1));
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// Parser

struct Parser<'a> {
    src: &'a str,
    toks: Vec<(Tok, usize)>,
    at: usize,
    depth: usize,
    /// Names of the loop variables in scope; a name's slot is its index.
    scope: Vec<String>,
    /// How many `READ` arguments enclose the cursor (symbols are legal only inside one).
    in_read: usize,
    /// The element width `base[i]` scales by: set inside a read's argument.
    scale: Option<i64>,
}

impl<'a> Parser<'a> {
    fn column(&self) -> usize {
        self.toks.get(self.at).map_or(self.src.len(), |(_, p)| *p) + 1
    }

    fn err<T>(&self, what: &str) -> Result<T, String> {
        Err(format!("{what} at column {}", self.column()))
    }

    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.at).map(|(t, _)| t)
    }

    fn peek_at(&self, ahead: usize) -> Option<&Tok> {
        self.toks.get(self.at + ahead).map(|(t, _)| t)
    }

    fn eat(&mut self, p: &str) -> bool {
        if matches!(self.peek(), Some(Tok::Punct(q)) if *q == p) {
            self.at += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, p: &str) -> Result<(), String> {
        if self.eat(p) {
            Ok(())
        } else {
            self.err(&format!("expected `{p}`"))
        }
    }

    fn enter(&mut self) -> Result<(), String> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return self.err("expression nests too deeply");
        }
        Ok(())
    }

    fn expr(&mut self) -> Result<Node, String> {
        self.enter()?;
        let cond = self.or()?;
        let node = if self.eat("?") {
            let yes = self.expr()?;
            self.expect(":")?;
            let no = self.expr()?;
            Node::Cond(Box::new(cond), Box::new(yes), Box::new(no))
        } else {
            cond
        };
        self.depth -= 1;
        Ok(node)
    }

    fn or(&mut self) -> Result<Node, String> {
        let mut lhs = self.and()?;
        while self.eat("||") {
            lhs = Node::Or(Box::new(lhs), Box::new(self.and()?));
        }
        Ok(lhs)
    }

    fn and(&mut self) -> Result<Node, String> {
        let mut lhs = self.compare()?;
        while self.eat("&&") {
            lhs = Node::And(Box::new(lhs), Box::new(self.compare()?));
        }
        Ok(lhs)
    }

    fn compare(&mut self) -> Result<Node, String> {
        let lhs = self.bit_or()?;
        let table = [
            ("==", Binary::Eq),
            ("!=", Binary::Ne),
            ("<=", Binary::Le),
            (">=", Binary::Ge),
            ("<", Binary::Lt),
            (">", Binary::Gt),
        ];
        for (p, op) in table {
            if self.eat(p) {
                let rhs = self.bit_or()?;
                if table
                    .iter()
                    .any(|(q, _)| matches!(self.peek(), Some(Tok::Punct(r)) if r == q))
                {
                    return self.err("comparisons do not chain; join them with `&&`");
                }
                return Ok(Node::Binary(op, Box::new(lhs), Box::new(rhs)));
            }
        }
        Ok(lhs)
    }

    fn bit_or(&mut self) -> Result<Node, String> {
        let mut lhs = self.bit_xor()?;
        while self.eat("|") {
            lhs = Node::Binary(Binary::BitOr, Box::new(lhs), Box::new(self.bit_xor()?));
        }
        Ok(lhs)
    }

    fn bit_xor(&mut self) -> Result<Node, String> {
        let mut lhs = self.bit_and()?;
        while self.eat("^") {
            lhs = Node::Binary(Binary::BitXor, Box::new(lhs), Box::new(self.bit_and()?));
        }
        Ok(lhs)
    }

    fn bit_and(&mut self) -> Result<Node, String> {
        let mut lhs = self.shift()?;
        while self.eat("&") {
            lhs = Node::Binary(Binary::BitAnd, Box::new(lhs), Box::new(self.shift()?));
        }
        Ok(lhs)
    }

    fn shift(&mut self) -> Result<Node, String> {
        let mut lhs = self.sum()?;
        loop {
            let op = if self.eat("<<") {
                Binary::Shl
            } else if self.eat(">>") {
                Binary::Shr
            } else {
                return Ok(lhs);
            };
            lhs = Node::Binary(op, Box::new(lhs), Box::new(self.sum()?));
        }
    }

    fn sum(&mut self) -> Result<Node, String> {
        let mut lhs = self.product()?;
        loop {
            let op = if self.eat("+") {
                Binary::Add
            } else if self.eat("-") {
                Binary::Sub
            } else {
                return Ok(lhs);
            };
            lhs = Node::Binary(op, Box::new(lhs), Box::new(self.product()?));
        }
    }

    fn product(&mut self) -> Result<Node, String> {
        let mut lhs = self.unary()?;
        loop {
            let op = if self.eat("*") {
                Binary::Mul
            } else if self.eat("/") {
                Binary::Div
            } else if self.eat("%") {
                Binary::Rem
            } else {
                return Ok(lhs);
            };
            lhs = Node::Binary(op, Box::new(lhs), Box::new(self.unary()?));
        }
    }

    fn unary(&mut self) -> Result<Node, String> {
        if self.eat("-") {
            self.enter()?;
            let inner = self.unary()?;
            self.depth -= 1;
            // `-5` is the number, so `i32(X - 5)` and `-2147483648` read naturally.
            return Ok(match inner {
                Node::Num(n) => Node::Num(n.wrapping_neg()),
                other => Node::Neg(Box::new(other)),
            });
        }
        if self.eat("!") {
            self.enter()?;
            let inner = self.unary()?;
            self.depth -= 1;
            return Ok(Node::Not(Box::new(inner)));
        }
        self.postfix()
    }

    fn postfix(&mut self) -> Result<Node, String> {
        let mut node = self.primary()?;
        while matches!(self.peek(), Some(Tok::Punct("["))) {
            let Some(scale) = self.scale else {
                return self.err("`[index]` is only valid inside a read such as i32(TABLE[3])");
            };
            self.at += 1;
            let saved = self.scale.take();
            let index = self.expr()?;
            self.scale = saved;
            self.expect("]")?;
            let step = Node::Binary(Binary::Mul, Box::new(index), Box::new(Node::Num(scale)));
            node = Node::Binary(Binary::Add, Box::new(node), Box::new(step));
        }
        Ok(node)
    }

    fn primary(&mut self) -> Result<Node, String> {
        let column = self.column();
        let Some(tok) = self.peek().cloned() else {
            return self.err("expression ends early");
        };
        match tok {
            Tok::Num(n) => {
                self.at += 1;
                Ok(Node::Num(n))
            }
            Tok::Punct("(") => {
                self.at += 1;
                let inner = self.expr()?;
                self.expect(")")?;
                Ok(inner)
            }
            Tok::Quoted(name) => {
                self.at += 1;
                self.symbol(name, column)
            }
            Tok::Ident(name) => {
                self.at += 1;
                if matches!(self.peek(), Some(Tok::Punct("("))) {
                    if let Some(kind) = Kind::from_name(&name) {
                        return self.read(kind);
                    }
                    if name == "let" {
                        return self.err("`let` takes no parentheses: let x = e in body");
                    }
                    match name.as_str() {
                        "abs" => return self.abs(),
                        "min" | "max" | "any" | "all" | "count" | "sum" => return self.call(&name),
                        _ => {}
                    }
                }
                if name == "let" {
                    return self.let_in();
                }
                if let Some(slot) = self.scope.iter().rposition(|v| *v == name) {
                    return Ok(Node::Var(slot));
                }
                self.symbol(name, column)
            }
            Tok::Punct(p) => self.err(&format!("unexpected `{p}`")),
        }
    }

    fn symbol(&self, name: String, column: usize) -> Result<Node, String> {
        if self.in_read == 0 {
            return Err(format!(
                "`{name}` at column {column} is an address; read it with u32({name}) \
                 (or i32, u16, i16, u8, i8)"
            ));
        }
        Ok(Node::Sym(name))
    }

    fn read(&mut self, kind: Kind) -> Result<Node, String> {
        self.expect("(")?;
        self.in_read += 1;
        let saved = self.scale.replace(i64::from(kind.width()));
        let address = self.expr()?;
        self.scale = saved;
        self.in_read -= 1;
        self.expect(")")?;
        Ok(Node::Read(kind, Box::new(address)))
    }

    /// `let a = e1, b = e2 in body`: each binding sees the ones before it.
    fn let_in(&mut self) -> Result<Node, String> {
        let mark = self.scope.len();
        let mut binds = Vec::new();
        loop {
            let Some(Tok::Ident(name)) = self.peek().cloned() else {
                return self.err("expected a name after `let`");
            };
            if matches!(name.as_str(), "in" | "let")
                || Kind::from_name(&name).is_some()
                || self.scope.contains(&name)
            {
                return self.err(&format!("`{name}` cannot be bound here"));
            }
            self.at += 1;
            self.expect("=")?;
            let value = self.expr()?;
            if self.scope.len() >= MAX_VARS {
                return self.err(&format!(
                    "at most {MAX_VARS} names (loop variables and lets) fit in scope"
                ));
            }
            binds.push((self.scope.len(), value));
            self.scope.push(name);
            if !self.eat(",") {
                break;
            }
        }
        if !matches!(self.peek(), Some(Tok::Ident(kw)) if kw == "in") {
            return self.err("expected `in` after the let bindings");
        }
        self.at += 1;
        let mut node = self.expr()?;
        self.scope.truncate(mark);
        while let Some((slot, value)) = binds.pop() {
            node = Node::Let {
                slot,
                value: Box::new(value),
                body: Box::new(node),
            };
        }
        Ok(node)
    }

    fn abs(&mut self) -> Result<Node, String> {
        self.expect("(")?;
        let inner = self.expr()?;
        self.expect(")")?;
        Ok(Node::Abs(Box::new(inner)))
    }

    /// `min`, `max`, `any`, `all`, `count`, `sum`: a plain call or a loop reduction.
    fn call(&mut self, name: &str) -> Result<Node, String> {
        self.expect("(")?;
        let is_loop = matches!(
            (self.peek(), self.peek_at(1)),
            (Some(Tok::Ident(_)), Some(Tok::Ident(kw))) if kw == "in"
        );
        if !is_loop {
            if matches!(name, "any" | "all" | "count" | "sum") {
                return self.err(&format!("{name} needs a loop: {name}(i in 0..N: expr)"));
            }
            let mut args = vec![self.expr()?];
            while self.eat(",") {
                args.push(self.expr()?);
            }
            self.expect(")")?;
            if args.len() < 2 {
                return self.err(&format!("{name} needs two or more values"));
            }
            return Ok(if name == "min" {
                Node::Min(args)
            } else {
                Node::Max(args)
            });
        }
        let Some(Tok::Ident(var)) = self.peek().cloned() else {
            return self.err("expected a loop variable");
        };
        if Kind::from_name(&var).is_some() || self.scope.contains(&var) {
            return self.err(&format!("loop variable `{var}` clashes with another name"));
        }
        if self.scope.len() >= MAX_VARS {
            return self.err(&format!(
                "at most {MAX_VARS} names (loop variables and lets) fit in scope"
            ));
        }
        self.at += 2;
        let lo = self.expr()?;
        self.expect("..")?;
        let hi = self.expr()?;
        self.expect(":")?;
        let slot = self.scope.len();
        self.scope.push(var);
        let body = self.expr()?;
        self.scope.pop();
        self.expect(")")?;
        let op = match name {
            "any" => Reduce::Any,
            "all" => Reduce::All,
            "count" => Reduce::Count,
            "sum" => Reduce::Sum,
            "min" => Reduce::Min,
            _ => Reduce::Max,
        };
        Ok(Node::Reduce {
            op,
            slot,
            lo: Box::new(lo),
            hi: Box::new(hi),
            body: Box::new(body),
        })
    }
}

// ---------------------------------------------------------------------------------------------
// Public API: parse, bind, evaluate

/// A parsed expression whose symbols are still names.
#[derive(Debug, Clone, PartialEq)]
pub struct Expr {
    root: Node,
}

impl Expr {
    /// Parse `src`. Errors name the column.
    pub fn parse(src: &str) -> Result<Expr, String> {
        let toks = lex(src)?;
        if toks.is_empty() {
            return Err("empty expression".into());
        }
        let mut p = Parser {
            src,
            toks,
            at: 0,
            depth: 0,
            scope: Vec::new(),
            in_read: 0,
            scale: None,
        };
        let root = p.expr()?;
        if p.at != p.toks.len() {
            return p.err("unexpected input");
        }
        Ok(Expr { root })
    }

    /// Replace every symbol by its address using `resolve`, and fold constant arithmetic.
    pub fn bind(
        &self,
        mut resolve: impl FnMut(&str) -> Result<u32, String>,
    ) -> Result<Program, String> {
        let mut symbols = Vec::new();
        let root = bind_node(&self.root, &mut resolve, &mut symbols)?;
        symbols.sort_by_key(|(_, a)| *a);
        symbols.dedup();
        Ok(Program { root, symbols })
    }
}

fn bind_node(
    node: &Node,
    resolve: &mut impl FnMut(&str) -> Result<u32, String>,
    symbols: &mut Vec<(String, u32)>,
) -> Result<Node, String> {
    let mut sub = |n: &Node| bind_node(n, resolve, symbols).map(Box::new);
    Ok(match node {
        Node::Num(_) | Node::Var(_) => node.clone(),
        Node::Sym(name) => {
            let addr = resolve(name)?;
            symbols.push((name.clone(), addr));
            Node::Num(i64::from(addr))
        }
        Node::Neg(a) => match *sub(a)? {
            Node::Num(n) => Node::Num(n.wrapping_neg()),
            other => Node::Neg(Box::new(other)),
        },
        Node::Not(a) => match *sub(a)? {
            Node::Num(n) => Node::Num(i64::from(n == 0)),
            other => Node::Not(Box::new(other)),
        },
        Node::Binary(op, a, b) => {
            let (a, b) = (sub(a)?, sub(b)?);
            match (&*a, &*b) {
                (Node::Num(x), Node::Num(y)) => match apply(*op, *x, *y) {
                    Ok(v) => Node::Num(v),
                    // Leave a failing constant (1 / 0) for the evaluator to report.
                    Err(_) => Node::Binary(*op, a, b),
                },
                _ => Node::Binary(*op, a, b),
            }
        }
        Node::And(a, b) => Node::And(sub(a)?, sub(b)?),
        Node::Or(a, b) => Node::Or(sub(a)?, sub(b)?),
        Node::Cond(c, a, b) => Node::Cond(sub(c)?, sub(a)?, sub(b)?),
        Node::Read(k, a) => Node::Read(*k, sub(a)?),
        Node::Let { slot, value, body } => Node::Let {
            slot: *slot,
            value: sub(value)?,
            body: sub(body)?,
        },
        Node::Abs(a) => Node::Abs(sub(a)?),
        Node::Min(v) => Node::Min(
            v.iter()
                .map(|n| sub(n).map(|b| *b))
                .collect::<Result<_, _>>()?,
        ),
        Node::Max(v) => Node::Max(
            v.iter()
                .map(|n| sub(n).map(|b| *b))
                .collect::<Result<_, _>>()?,
        ),
        Node::Reduce {
            op,
            slot,
            lo,
            hi,
            body,
        } => Node::Reduce {
            op: *op,
            slot: *slot,
            lo: sub(lo)?,
            hi: sub(hi)?,
            body: sub(body)?,
        },
    })
}

/// One memory read an evaluation made, kept to explain a result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Read {
    /// Address read.
    pub addr: u32,
    /// Bytes read, 1, 2 or 4.
    pub width: u8,
    /// The widened value.
    pub value: i64,
}

/// An expression with its symbols bound to addresses, ready to evaluate.
#[derive(Debug, Clone)]
pub struct Program {
    root: Node,
    /// Symbols the expression names, sorted by address, for explaining reads.
    symbols: Vec<(String, u32)>,
}

struct Eval<'a, M: Memory> {
    mem: &'a M,
    vars: [i64; MAX_VARS],
    budget: u32,
    trace: Option<&'a mut Vec<Read>>,
}

impl Program {
    /// Evaluate against `mem`: the expression's value, or why it has none (an address outside
    /// main RAM, a division by zero, a loop over budget).
    pub fn eval(&self, mem: &impl Memory) -> Result<i64, String> {
        Eval {
            mem,
            vars: [0; MAX_VARS],
            budget: LOOP_BUDGET,
            trace: None,
        }
        .node(&self.root)
    }

    /// [`Program::eval`] that also returns the first reads it made (to explain the value).
    pub fn eval_traced(&self, mem: &impl Memory) -> (Result<i64, String>, Vec<Read>) {
        let mut reads = Vec::new();
        let result = Eval {
            mem,
            vars: [0; MAX_VARS],
            budget: LOOP_BUDGET,
            trace: Some(&mut reads),
        }
        .node(&self.root);
        (result, reads)
    }

    /// `name+offset=value` for each read, naming the nearest symbol at or below the address
    /// (within 4 KiB) among those the expression mentions.
    pub fn describe_reads(&self, reads: &[Read]) -> String {
        let mut out = String::new();
        for (i, r) in reads.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            let near = self
                .symbols
                .iter()
                .rev()
                .find(|(_, a)| *a <= r.addr && r.addr - *a < 0x1000);
            match near {
                Some((name, a)) if r.addr == *a => {
                    let _ = write!(out, "{name}={}", r.value);
                }
                Some((name, a)) => {
                    let _ = write!(out, "{name}+{:#x}={}", r.addr - *a, r.value);
                }
                None => {
                    let _ = write!(out, "@{:#010x}={}", r.addr, r.value);
                }
            }
        }
        out
    }
}

fn apply(op: Binary, a: i64, b: i64) -> Result<i64, String> {
    Ok(match op {
        Binary::Add => a.wrapping_add(b),
        Binary::Sub => a.wrapping_sub(b),
        Binary::Mul => a.wrapping_mul(b),
        Binary::Div | Binary::Rem if b == 0 => return Err("division by zero".into()),
        Binary::Div => a.wrapping_div(b),
        Binary::Rem => a.wrapping_rem(b),
        Binary::Shl | Binary::Shr => {
            let count = u32::try_from(b)
                .ok()
                .filter(|c| *c < 64)
                .ok_or_else(|| format!("shift count {b} is not within 0..=63"))?;
            if op == Binary::Shl {
                a.wrapping_shl(count)
            } else {
                a >> count
            }
        }
        Binary::BitAnd => a & b,
        Binary::BitOr => a | b,
        Binary::BitXor => a ^ b,
        Binary::Eq => i64::from(a == b),
        Binary::Ne => i64::from(a != b),
        Binary::Lt => i64::from(a < b),
        Binary::Le => i64::from(a <= b),
        Binary::Gt => i64::from(a > b),
        Binary::Ge => i64::from(a >= b),
    })
}

impl<M: Memory> Eval<'_, M> {
    fn node(&mut self, node: &Node) -> Result<i64, String> {
        Ok(match node {
            Node::Num(n) => *n,
            Node::Var(slot) => self.vars[*slot],
            Node::Sym(name) => return Err(format!("symbol `{name}` was never bound")),
            Node::Neg(a) => self.node(a)?.wrapping_neg(),
            Node::Not(a) => i64::from(self.node(a)? == 0),
            Node::Binary(op, a, b) => {
                let (x, y) = (self.node(a)?, self.node(b)?);
                apply(*op, x, y)?
            }
            Node::And(a, b) => i64::from(self.node(a)? != 0 && self.node(b)? != 0),
            Node::Or(a, b) => i64::from(self.node(a)? != 0 || self.node(b)? != 0),
            Node::Cond(c, a, b) => {
                if self.node(c)? != 0 {
                    self.node(a)?
                } else {
                    self.node(b)?
                }
            }
            Node::Read(kind, a) => {
                let addr = self.node(a)? as u32;
                let width = kind.width();
                let Some(raw) = self.mem.load(addr, width) else {
                    return Err(format!("{addr:#010x} is not main RAM"));
                };
                let value = kind.widen(raw);
                if let Some(trace) = self.trace.as_mut() {
                    if trace.len() < MAX_TRACED_READS {
                        trace.push(Read { addr, width, value });
                    }
                }
                value
            }
            Node::Let { slot, value, body } => {
                self.vars[*slot] = self.node(value)?;
                self.node(body)?
            }
            Node::Abs(a) => self.node(a)?.wrapping_abs(),
            Node::Min(v) | Node::Max(v) => {
                let is_min = matches!(node, Node::Min(_));
                let mut best = self.node(&v[0])?;
                for n in &v[1..] {
                    let x = self.node(n)?;
                    best = if is_min { best.min(x) } else { best.max(x) };
                }
                best
            }
            Node::Reduce {
                op,
                slot,
                lo,
                hi,
                body,
            } => {
                let (lo, hi) = (self.node(lo)?, self.node(hi)?);
                let mut acc = match op {
                    Reduce::All => 1,
                    Reduce::Min => i64::from(i32::MAX),
                    Reduce::Max => i64::from(i32::MIN),
                    _ => 0,
                };
                let mut i = lo;
                while i < hi {
                    self.budget = self
                        .budget
                        .checked_sub(1)
                        .ok_or_else(|| format!("loop ran past {LOOP_BUDGET} iterations"))?;
                    self.vars[*slot] = i;
                    let v = self.node(body)?;
                    match op {
                        Reduce::Any if v != 0 => return Ok(1),
                        Reduce::All if v == 0 => return Ok(0),
                        Reduce::Count => acc += i64::from(v != 0),
                        Reduce::Sum => acc = acc.wrapping_add(v),
                        Reduce::Min => acc = acc.min(v),
                        Reduce::Max => acc = acc.max(v),
                        _ => {}
                    }
                    i += 1;
                }
                acc
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A RAM image at 0x8000_0000.
    struct Ram(Vec<u8>);

    impl Ram {
        fn new() -> Ram {
            Ram(vec![0; 0x1000])
        }
        fn put(&mut self, off: usize, bytes: &[u8]) {
            self.0[off..off + bytes.len()].copy_from_slice(bytes);
        }
    }

    impl Memory for Ram {
        fn load(&self, addr: u32, width: u8) -> Option<u32> {
            let off = addr.checked_sub(0x8000_0000)? as usize;
            let b = self.0.get(off..off + usize::from(width))?;
            Some(b.iter().rev().fold(0u32, |v, x| v << 8 | u32::from(*x)))
        }
    }

    fn symbols(name: &str) -> Result<u32, String> {
        match name {
            "A" => Ok(0x8000_0000),
            "B" => Ok(0x8000_0100),
            "TABLE" => Ok(0x8000_0200),
            "n::PATH" => Ok(0x8000_0300),
            "odd.name" => Ok(0x8000_0310),
            "HK_PLAYER_X" => Ok(0x8000_0020),
            "HK_PLAYER_Y" => Ok(0x8000_0024),
            "HK_FK_DX" => Ok(0x8000_0028),
            "HK_FK_PHASE" => Ok(0x8000_002c),
            "ACT" => Ok(0x8000_0400),
            "HK_ACT" => Ok(0x8000_0400),
            "HK_NAIL_BUSY" => Ok(0x8000_0030),
            other => Err(format!("symbol `{other}` not found")),
        }
    }

    fn run(src: &str, ram: &Ram) -> Result<i64, String> {
        Expr::parse(src)?.bind(symbols)?.eval(ram)
    }

    fn val(src: &str) -> i64 {
        run(src, &Ram::new()).unwrap()
    }

    #[test]
    fn arithmetic_and_precedence() {
        assert_eq!(val("1 + 2 * 3"), 7);
        assert_eq!(val("(1 + 2) * 3"), 9);
        assert_eq!(val("10 - 4 - 3"), 3);
        assert_eq!(val("-7 / 2"), -3);
        assert_eq!(val("-7 % 3"), -1);
        assert_eq!(val("1 << 4 | 3"), 19);
        assert_eq!(val("6 & 3 == 2"), 1);
        assert_eq!(val("0xff_ff & 0x0f0"), 0xf0);
        assert_eq!(val("-16 >> 2"), -4);
        assert_eq!(val("5 ^ 1"), 4);
        assert_eq!(val("--3"), 3);
        assert_eq!(val("!0 + !7"), 1);
    }

    #[test]
    fn comparisons_and_logic_yield_zero_or_one() {
        assert_eq!(val("3 < 4"), 1);
        assert_eq!(val("3 >= 4"), 0);
        assert_eq!(val("1 == 1 && 2 != 2"), 0);
        assert_eq!(val("0 || 5"), 1);
        assert_eq!(val("1 || 0 && 0"), 1);
        assert_eq!(val("1 ? 10 : 20"), 10);
        assert_eq!(val("0 ? 10 : 1 ? 30 : 40"), 30);
        assert!(Expr::parse("1 < 2 < 3").unwrap_err().contains("chain"));
    }

    #[test]
    fn functions() {
        assert_eq!(val("abs(-9)"), 9);
        assert_eq!(val("abs(0 - 2147483648)"), 2147483648);
        assert_eq!(val("min(4, -2, 9)"), -2);
        assert_eq!(val("max(4, -2, 9)"), 9);
        assert!(Expr::parse("min(4)").is_err());
    }

    #[test]
    fn typed_reads_widen_correctly() {
        let mut ram = Ram::new();
        ram.put(0, &[0xfe, 0xff, 0xff, 0xff]);
        ram.put(0x100, &[0x80, 0x00, 0x00, 0x80]);
        assert_eq!(run("u8(A)", &ram), Ok(0xfe));
        assert_eq!(run("i8(A)", &ram), Ok(-2));
        assert_eq!(run("u16(A)", &ram), Ok(0xfffe));
        assert_eq!(run("i16(A)", &ram), Ok(-2));
        assert_eq!(run("u32(A)", &ram), Ok(0xffff_fffe));
        assert_eq!(run("i32(A)", &ram), Ok(-2));
        assert_eq!(run("u32(B)", &ram), Ok(0x8000_0080));
        assert_eq!(run("i32(B)", &ram), Ok(-0x7fff_ff80));
        assert_eq!(run("u16(B + 2)", &ram), Ok(0x8000));
        assert_eq!(run("i16(B + 2)", &ram), Ok(-0x8000));
        // Unaligned reads work.
        assert_eq!(run("u16(A + 1)", &ram), Ok(0xffff));
        assert_eq!(run("u32(0x80000000)", &ram), Ok(0xffff_fffe));
    }

    #[test]
    fn symbols_are_names_with_paths_and_quotes() {
        let mut ram = Ram::new();
        ram.put(0x300, &[7]);
        ram.put(0x310, &[9]);
        assert_eq!(run("u8(n::PATH)", &ram), Ok(7));
        assert_eq!(run("u8(`odd.name`)", &ram), Ok(9));
        assert!(run("u8(NOPE)", &ram).unwrap_err().contains("NOPE"));
    }

    #[test]
    fn bare_symbols_are_rejected_with_advice() {
        let e = Expr::parse("A == 3").unwrap_err();
        assert!(e.contains("u32(A)"), "{e}");
        assert!(Expr::parse("u8(A) + B").is_err());
        assert!(Expr::parse("u8(A + B)").is_ok());
    }

    #[test]
    fn array_indexing_scales_by_the_read_width() {
        let mut ram = Ram::new();
        ram.put(0x200, &[1, 0, 2, 0, 3, 0, 4, 0]);
        ram.put(0x100, &[2]);
        assert_eq!(run("u16(TABLE[0])", &ram), Ok(1));
        assert_eq!(run("u16(TABLE[2])", &ram), Ok(3));
        // The index is another read.
        assert_eq!(run("u16(TABLE[u8(B)])", &ram), Ok(3));
        // Records use explicit arithmetic.
        assert_eq!(run("u16(TABLE + 4 * 1 + 2)", &ram), Ok(4));
        assert_eq!(run("u32(TABLE[1])", &ram), Ok(0x0004_0003));
        assert!(Expr::parse("TABLE[1]").is_err());
        assert!(Expr::parse("1[2]").is_err());
    }

    fn actors() -> Ram {
        // Five 12-byte records at TABLE: [alive u32, x i32, y i32].
        let mut ram = Ram::new();
        let rows: [(u32, i32, i32); 5] =
            [(1, 100, 5), (0, 40, 5), (1, -30, 5), (1, 500, 9), (0, 0, 0)];
        for (i, (alive, x, y)) in rows.iter().enumerate() {
            ram.put(0x200 + 12 * i, &alive.to_le_bytes());
            ram.put(0x204 + 12 * i, &x.to_le_bytes());
            ram.put(0x208 + 12 * i, &y.to_le_bytes());
        }
        ram.put(0x100, &5u32.to_le_bytes());
        ram.put(0x104, &10i32.to_le_bytes());
        ram
    }

    #[test]
    fn loops_reduce_over_a_table() {
        let ram = actors();
        let alive = "u32(TABLE + 12 * i)";
        let dx = "i32(TABLE + 12 * i + 4) - i32(B + 4)";
        assert_eq!(run(&format!("count(i in 0..5: {alive})"), &ram), Ok(3));
        assert_eq!(run(&format!("count(i in 0..u32(B): {alive})"), &ram), Ok(3));
        assert_eq!(
            run(&format!("any(i in 0..5: {alive} && {dx} < -35)"), &ram),
            Ok(1)
        );
        assert_eq!(
            run(&format!("any(i in 0..5: {alive} && {dx} > 1000)"), &ram),
            Ok(0)
        );
        assert_eq!(
            run(&format!("all(i in 0..5: !{alive} || {dx} < 600)"), &ram),
            Ok(1)
        );
        assert_eq!(run(&format!("all(i in 0..5: {alive})"), &ram), Ok(0));
        assert_eq!(
            run("sum(i in 0..4: i32(TABLE + 12 * i + 4))", &ram),
            Ok(610)
        );
        // Nearest live foe: min over |dx|, a large value for the dead.
        let nearest = format!("min(i in 0..5: {alive} ? abs({dx}) : 99999)");
        assert_eq!(run(&nearest, &ram), Ok(40));
        assert_eq!(run("max(i in 0..5: i)", &ram), Ok(4));
        // Empty ranges.
        assert_eq!(run("any(i in 3..3: 1)", &ram), Ok(0));
        assert_eq!(run("all(i in 3..3: 0)", &ram), Ok(1));
        assert_eq!(run("min(i in 3..3: 1)", &ram), Ok(2147483647));
        assert_eq!(run("max(i in 3..3: 1)", &ram), Ok(-2147483648));
        // Nested loops: pairs with the same y.
        assert_eq!(
            run(
                "sum(i in 0..4: count(j in 0..4: i != j \
                 && i32(TABLE + 12 * i + 8) == i32(TABLE + 12 * j + 8)))",
                &ram
            ),
            Ok(6)
        );
    }

    #[test]
    fn loop_rules() {
        assert!(Expr::parse("any(i in 0..3: 1) + i").is_err_and(|e| e.contains("address")));
        assert!(Expr::parse("any(0..3: 1)").is_err());
        assert!(Expr::parse("any(1)").is_err());
        assert!(Expr::parse("any(u8 in 0..3: 1)").is_err());
        assert!(Expr::parse("any(i in 0..3: any(i in 0..2: 1))").is_err());
        let opens: String = (0..9).map(|n| format!("any(v{n} in 0..1: ")).collect();
        let deep = format!("{opens}1{}", ")".repeat(9));
        assert!(Expr::parse(&deep).unwrap_err().contains("at most"));
        let err = run("count(i in 0..4000000000: 1)", &Ram::new()).unwrap_err();
        assert!(err.contains("iterations"), "{err}");
    }

    #[test]
    fn short_circuit_skips_bad_reads_and_division() {
        let ram = Ram::new();
        assert_eq!(run("0 && u8(0x10)", &ram), Ok(0));
        assert_eq!(run("1 || u8(0x10)", &ram), Ok(1));
        assert_eq!(run("1 ? 5 : u8(0x10)", &ram), Ok(5));
        assert_eq!(run("0 && 1 / 0", &ram), Ok(0));
        assert!(run("u8(0x10)", &ram).unwrap_err().contains("not main RAM"));
        assert!(run("1 / 0", &ram).unwrap_err().contains("division"));
        assert!(run("5 % 0", &ram).unwrap_err().contains("division"));
        assert!(run("1 << 64", &ram).unwrap_err().contains("shift"));
        assert!(run("1 << -1", &ram).unwrap_err().contains("shift"));
        assert_eq!(run("any(i in 1..3: i == 1 || u8(0x10))", &ram), Ok(1));
    }

    #[test]
    fn arithmetic_wraps_instead_of_panicking() {
        let ram = Ram::new();
        assert_eq!(run("9223372036854775807 + 1", &ram), Ok(i64::MIN));
        assert_eq!(run("abs(-9223372036854775807 - 1)", &ram), Ok(i64::MIN));
        assert_eq!(run("(-9223372036854775807 - 1) / -1", &ram), Ok(i64::MIN));
    }

    #[test]
    fn syntax_errors_name_a_column() {
        for (src, needle) in [
            ("", "empty"),
            ("1 +", "ends early"),
            ("(1", "expected `)`"),
            ("1 2", "unexpected input"),
            ("1 $ 2", "unexpected `$`"),
            ("0x", "bad number"),
            ("99999999999999999999", "bad number"),
            ("u8(`a)", "unterminated"),
            ("1 ? 2", "expected `:`"),
            ("u8(A", "expected `)`"),
        ] {
            let e = Expr::parse(src).unwrap_err();
            assert!(e.contains(needle), "{src}: {e}");
        }
        assert!(Expr::parse("u8(A) @ 2").unwrap_err().contains("column 7"));
        let deep = format!("{}1{}", "(".repeat(100), ")".repeat(100));
        assert!(Expr::parse(&deep).unwrap_err().contains("deeply"));
    }

    #[test]
    fn traces_explain_the_reads() {
        let mut ram = Ram::new();
        ram.put(0, &[5, 0, 0, 0]);
        ram.put(0x104, &[0xf0, 0xff, 0xff, 0xff]);
        let prog = Expr::parse("u32(A) > 3 && i32(B + 4) < 0")
            .unwrap()
            .bind(symbols)
            .unwrap();
        let (value, reads) = prog.eval_traced(&ram);
        assert_eq!(value, Ok(1));
        assert_eq!(prog.describe_reads(&reads), "A=5, B+0x4=-16");
        let (_, many) = Expr::parse("sum(i in 0..40: u8(A))")
            .unwrap()
            .bind(symbols)
            .unwrap()
            .eval_traced(&ram);
        assert_eq!(many.len(), MAX_TRACED_READS);
    }

    #[test]
    fn let_binds_names_in_order() {
        let ram = Ram::new();
        assert_eq!(run("let a = 3 in a * a", &ram), Ok(9));
        assert_eq!(run("let a = 3, b = a + 1 in a * b", &ram), Ok(12));
        assert_eq!(run("1 + let a = 2 in a * 10", &ram), Ok(21));
        assert_eq!(run("(let a = 2 in a) + (let a = 5 in a)", &ram), Ok(7));
        assert_eq!(run("sum(i in 0..3: let sq = i * i in sq + 1)", &ram), Ok(8));
        // A binding sees the loop variable and an earlier binding, not a later one.
        assert!(Expr::parse("let a = b, b = 1 in a")
            .unwrap_err()
            .contains("address"));
        for bad in [
            "let in 1 in 2",
            "let a in a",
            "let a = 1 a",
            "let a = 1, a = 2 in a",
            "let u8 = 1 in 1",
            "let(1)",
            "let a = 1 in",
        ] {
            assert!(Expr::parse(bad).is_err(), "{bad}");
        }
        // The names go out of scope after the body.
        assert!(Expr::parse("(let a = 1 in a) + a").is_err());
        let many = (0..9)
            .map(|n| format!("v{n} = {n}"))
            .collect::<Vec<_>>()
            .join(", ");
        assert!(Expr::parse(&format!("let {many} in v8"))
            .unwrap_err()
            .contains("at most"));
    }

    #[test]
    fn a_threat_left_sense_from_the_words_hk_publishes_today() {
        // Threat from the left: the False Knight (HK_FK_DX, Q16, 2147483647 when there is none)
        // is winding up (phase 1) or attacking (2) and stands within 14 units to the left.
        let threat = "let dx = i32(HK_FK_DX), ph = u32(HK_FK_PHASE) in \
                      dx < 0 && dx > -14 * 65536 && ph >= 1 && ph <= 2";
        let set = |dx: i32, phase: u32| {
            let mut ram = Ram::new();
            ram.put(0x28, &dx.to_le_bytes());
            ram.put(0x2c, &phase.to_le_bytes());
            run(threat, &ram)
        };
        assert_eq!(set(-10 * 65536, 2), Ok(1));
        assert_eq!(set(-10 * 65536, 1), Ok(1));
        assert_eq!(set(-10 * 65536, 0), Ok(0));
        assert_eq!(set(-10 * 65536, 3), Ok(0));
        assert_eq!(set(-14 * 65536, 2), Ok(0));
        assert_eq!(set(5 * 65536, 2), Ok(0));
        assert_eq!(set(i32::MAX, 2), Ok(0));
    }

    // ---- the walker's foe senses as expressions, against the Rust they replace ----

    const NONE: i32 = i32::MAX;
    const Q: i32 = 65536;
    // The constants of hk-psx game/src/enemies.rs (Q16).
    const FOE_REACH: i32 = 3 * Q;
    const FOE_BAND: i32 = 3 * Q;
    const FOE_RANGE: i32 = 16 * Q;
    const STRIKE_REACH: i32 = 3 * Q + 13_107;
    const STRIKE_UP: i32 = Q + 39_321;
    const STRIKE_OVERHEAD_X: i32 = Q + 26_214;
    const STRIKE_OVERHEAD_LOW: i32 = 19_660;
    const STRIKE_OVERHEAD_HIGH: i32 = 2 * Q + 52_428;
    const STRIKE_DOWN: i32 = 2 * Q + 26_214;
    const JUMP_STRIKE_X: i32 = 3 * Q;
    const JUMP_STRIKE_HIGH: i32 = 5 * Q;
    const FOCUS_SAFE_X: i32 = 10 * Q;
    const FOCUS_SAFE_Y: i32 = 5 * Q;

    #[derive(Debug, PartialEq, Eq)]
    struct Senses {
        left: i32,
        right: i32,
        live: i32,
        near: bool,
        strike_up: bool,
        jump_strike: bool,
        strike_left: bool,
        strike_right: bool,
    }

    /// `EnemyWorld::foe_senses` over a table of (x, y) for the live, bodied actors of the scene.
    fn reference(foes: &[Option<(i32, i32)>], x: i32, y: i32) -> Senses {
        let mut s = Senses {
            left: 0,
            right: 0,
            live: 0,
            near: false,
            strike_up: false,
            jump_strike: false,
            strike_left: false,
            strike_right: false,
        };
        let (mut left, mut right) = (i32::MAX, i32::MAX);
        for (ax, ay) in foes.iter().flatten() {
            s.live += 1;
            let (dx, dy) = (ax - x, ay - y);
            if dx.abs() <= FOCUS_SAFE_X && dy.abs() <= FOCUS_SAFE_Y {
                s.near = true;
            }
            if dx.abs() <= STRIKE_OVERHEAD_X
                && (STRIKE_OVERHEAD_LOW..=STRIKE_OVERHEAD_HIGH).contains(&dy)
            {
                s.strike_up = true;
            }
            if dx.abs() <= JUMP_STRIKE_X && dy > STRIKE_UP && dy <= JUMP_STRIKE_HIGH {
                s.jump_strike = true;
            }
            if (-STRIKE_DOWN..=STRIKE_UP).contains(&dy) && dx.abs() <= STRIKE_REACH {
                if dx < 0 {
                    s.strike_left = true;
                } else {
                    s.strike_right = true;
                }
            }
            if dy.abs() > FOE_BAND || dx.abs() > FOE_RANGE {
                continue;
            }
            let side = if dx < 0 { &mut left } else { &mut right };
            *side = (*side).min((dx.abs() - FOE_REACH).max(0));
        }
        let none = |gap: i32| if gap == i32::MAX { 0 } else { gap };
        s.left = none(left);
        s.right = none(right);
        s
    }

    /// An expression over the raw actor table: 32 slots of (x, y) i32 at ACT, x = i32::MAX when
    /// the slot holds no live actor with a body in this scene.
    fn over_actors(body: &str) -> String {
        format!(
            "any(i in 0..32: let ax = i32(ACT + 8 * i), dx = ax - i32(HK_PLAYER_X), \
             dy = i32(ACT + 8 * i + 4) - i32(HK_PLAYER_Y) in ax != 2147483647 && ({body}))"
        )
    }

    fn gap(side: &str) -> String {
        format!(
            "let g = min(i in 0..32: let ax = i32(ACT + 8 * i), dx = ax - i32(HK_PLAYER_X), \
             dy = i32(ACT + 8 * i + 4) - i32(HK_PLAYER_Y) in \
             ax != 2147483647 && abs(dy) <= 3 * 65536 && abs(dx) <= 16 * 65536 && {side} \
             ? max(abs(dx) - 3 * 65536, 0) : 2147483647) in g == 2147483647 ? 0 : g"
        )
    }

    #[test]
    fn foe_senses_as_expressions_match_the_rust_they_replace() {
        let live = "count(i in 0..32: i32(ACT + 8 * i) != 2147483647)";
        let near = over_actors("abs(dx) <= 10 * 65536 && abs(dy) <= 5 * 65536");
        let up = over_actors("abs(dx) <= 65536 + 26214 && dy >= 19660 && dy <= 2 * 65536 + 52428");
        let jump = over_actors("abs(dx) <= 3 * 65536 && dy > 65536 + 39321 && dy <= 5 * 65536");
        let swing = |side: &str| {
            over_actors(&format!(
                "dy >= -(2 * 65536 + 26214) && dy <= 65536 + 39321 \
                 && abs(dx) <= 3 * 65536 + 13107 && {side}"
            ))
        };
        let programs: Vec<Program> = [
            live.to_string(),
            near,
            up,
            jump,
            swing("dx < 0"),
            swing("dx >= 0"),
            gap("dx < 0"),
            gap("dx >= 0"),
        ]
        .iter()
        .map(|src| Expr::parse(src).unwrap().bind(symbols).unwrap())
        .collect();

        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        // Values that sit on, just inside and just outside every threshold above.
        let edges: Vec<i32> = [
            FOE_REACH,
            FOE_BAND,
            FOE_RANGE,
            STRIKE_REACH,
            STRIKE_UP,
            STRIKE_OVERHEAD_X,
            STRIKE_OVERHEAD_LOW,
            STRIKE_OVERHEAD_HIGH,
            STRIKE_DOWN,
            JUMP_STRIKE_X,
            JUMP_STRIKE_HIGH,
            FOCUS_SAFE_X,
            FOCUS_SAFE_Y,
            0,
        ]
        .iter()
        .flat_map(|t| (-1..=1).flat_map(move |d| [t + d, -(t + d)]))
        .collect();
        let pick = |next: &mut dyn FnMut() -> u64| -> i32 {
            if next().is_multiple_of(2) {
                (next() % (40 * Q as u64)) as i32 - 20 * Q
            } else {
                edges[(next() % edges.len() as u64) as usize]
            }
        };
        for case in 0..3000 {
            let (px, py) = ((next() % 4000) as i32 - 2000, (next() % 4000) as i32 - 2000);
            let mut foes = [None; 32];
            let mut ram = Ram::new();
            ram.put(0x20, &px.to_le_bytes());
            ram.put(0x24, &py.to_le_bytes());
            for (i, slot) in foes.iter_mut().enumerate() {
                let present = next() % 100 < (case % 7) * 8;
                let (x, y) = if present {
                    let f = (px + pick(&mut next), py + pick(&mut next));
                    *slot = Some(f);
                    f
                } else {
                    (NONE, 0)
                };
                ram.put(0x400 + 8 * i, &x.to_le_bytes());
                ram.put(0x404 + 8 * i, &y.to_le_bytes());
            }
            let want = reference(&foes, px, py);
            let got = |n: usize| programs[n].eval(&ram).unwrap();
            let got = Senses {
                live: got(0) as i32,
                near: got(1) != 0,
                strike_up: got(2) != 0,
                jump_strike: got(3) != 0,
                strike_left: got(4) != 0,
                strike_right: got(5) != 0,
                left: got(6) as i32,
                right: got(7) as i32,
            };
            assert_eq!(got, want, "case {case}: player ({px}, {py}) foes {foes:?}");
        }
    }

    #[test]
    fn the_documented_strike_example_binds_and_runs() {
        let src = "u32(HK_NAIL_BUSY) == 0 &&
          any(i in 0..32:
            let ax = i32(HK_ACT + 8 * i),
                dx = ax - i32(HK_PLAYER_X),
                dy = i32(HK_ACT + 8 * i + 4) - i32(HK_PLAYER_Y)
            in ax != 2147483647 && dx < 0 && abs(dx) <= 3 * 65536 + 13107
               && dy >= -(2 * 65536 + 26214) && dy <= 65536 + 39321)";
        let mut ram = Ram::new();
        for i in 0..32 {
            ram.put(0x400 + 8 * i, &i32::MAX.to_le_bytes());
        }
        ram.put(0x20, &(10 * 65536i32).to_le_bytes());
        let prog = Expr::parse(src).unwrap().bind(symbols).unwrap();
        assert_eq!(prog.eval(&ram), Ok(0));
        ram.put(0x400 + 8 * 5, &(8 * 65536i32).to_le_bytes());
        assert_eq!(prog.eval(&ram), Ok(1));
        ram.put(0x30, &1u32.to_le_bytes());
        assert_eq!(prog.eval(&ram), Ok(0));
    }

    /// Cost of one evaluation, for the numbers in TESTING.md. Run with
    /// `cargo test --release -p psoxide-gate expr_cost -- --ignored --nocapture`.
    #[test]
    #[ignore = "timing report, run by hand in release"]
    fn expr_cost() {
        let mut ram = Ram::new();
        for i in 0..32 {
            ram.put(0x400 + 8 * i, &(1000 * 65536 + i as i32).to_le_bytes());
            ram.put(0x404 + 8 * i, &(7 * 65536i32).to_le_bytes());
        }
        let table = |body: &str| over_actors(body);
        let cases = [
            ("one compare", "u32(HK_NAIL_BUSY) == 0".to_string()),
            (
                "threat from the left",
                "let dx = i32(HK_FK_DX), ph = u32(HK_FK_PHASE) in dx < 0 && dx > -14 * 65536 && ph >= 1 && ph <= 2".to_string(),
            ),
            (
                "strike left, 32 slots, no hit",
                table("dy >= -(2 * 65536 + 26214) && dy <= 65536 + 39321 && abs(dx) <= 3 * 65536 + 13107 && dx < 0"),
            ),
            ("nearest gap, 32 slots", gap("dx < 0")),
        ];
        for (name, src) in cases {
            let prog = Expr::parse(&src).unwrap().bind(symbols).unwrap();
            let runs = 200_000u32;
            let start = std::time::Instant::now();
            let mut sink = 0i64;
            for _ in 0..runs {
                sink = sink.wrapping_add(std::hint::black_box(&prog).eval(&ram).unwrap());
            }
            let ns = start.elapsed().as_nanos() as f64 / f64::from(runs);
            println!("{name}: {ns:.0} ns per evaluation (sink {sink})");
        }
    }
}
