//! Lightweight PascalScript-compatible interpreter for the "Script Pascal" batch rename rule.
//! Supports `var`, `begin ... end.`, `if/then/else`, `for/to/downto`, `while`, `:=`, arithmetic,
//! string concatenation (`+`), `FileName` (read/write), `FilePath`, `FileIndex`, and standard
//! ReNamer helpers (`WideExtractBaseName`, `WideExtractFileExt`, `WideReplaceStr`, `WideUpperCase`,
//! `WideLowerCase`, `WideCaseCapitalize`, `WideCopy`, `WideDelete`, `WideInsert`, `WidePos`,
//! `WideLength`, `WideTrim`, `IntToStr`, `StrToInt`, `StrToIntDef`, `MetaTag`).

use std::collections::HashMap;

use openexplorer_i18n::{t, tr};

use crate::model::{Entry, Location, shown};
use crate::rules_meta;

#[derive(Clone, Debug, PartialEq)]
enum Val {
    Str(String),
    Int(i64),
    Bool(bool),
}

impl Val {
    fn as_str(&self) -> String {
        match self {
            Self::Str(s) => s.clone(),
            Self::Int(i) => i.to_string(),
            Self::Bool(b) => if *b { "True".into() } else { "False".into() },
        }
    }

    fn as_int(&self) -> i64 {
        match self {
            Self::Int(i) => *i,
            Self::Str(s) => s.trim().parse().unwrap_or(0),
            Self::Bool(b) => i64::from(*b),
        }
    }

    fn as_bool(&self) -> bool {
        match self {
            Self::Bool(b) => *b,
            Self::Int(i) => *i != 0,
            Self::Str(s) => !s.is_empty() && !s.eq_ignore_ascii_case("false"),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Ident(String),
    Str(String),
    Int(i64),
    Assign, // :=
    Eq,
    Ne, // <>
    Le,
    Ge,
    Lt,
    Gt,
    Plus,
    Minus,
    Mul,
    Div,
    LParen,
    RParen,
    Comma,
    Semi,
    Colon,
    Dot,
}

fn tokenize(src: &str) -> Result<Vec<Tok>, String> {
    let bytes = src.as_bytes();
    let mut i = 0usize;
    let mut out = Vec::new();
    while i < bytes.len() {
        let c = bytes[i] as char;
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        // Comments: // ... or { ... } or (* ... *)
        if src[i..].starts_with("//") {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if c == '{' {
            i += 1;
            while i < bytes.len() && bytes[i] != b'}' {
                i += 1;
            }
            i += 1;
            continue;
        }
        if src[i..].starts_with("(*") {
            i += 2;
            while i + 1 < bytes.len() && !src[i..].starts_with("*)") {
                i += 1;
            }
            i += 2;
            continue;
        }
        if c == '\'' {
            i += 1;
            let mut s = String::new();
            let mut closed = false;
            while i < bytes.len() {
                if bytes[i] == b'\'' {
                    if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                        s.push('\'');
                        i += 2;
                    } else {
                        i += 1;
                        closed = true;
                        break;
                    }
                } else {
                    let ch = src[i..].chars().next().unwrap();
                    s.push(ch);
                    i += ch.len_utf8();
                }
            }
            if !closed {
                return Err(t("String não fechada.", "Unterminated string literal.").into());
            }
            out.push(Tok::Str(s));
            continue;
        }
        if c.is_ascii_digit() {
            let start = i;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            let n: i64 = src[start..i].parse().map_err(|_| t("Número inválido.", "Invalid integer."))?;
            out.push(Tok::Int(n));
            continue;
        }
        if c.is_ascii_alphabetic() || c == '_' {
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            out.push(Tok::Ident(src[start..i].to_string()));
            continue;
        }
        if src[i..].starts_with(":=") {
            out.push(Tok::Assign);
            i += 2;
        } else if src[i..].starts_with("<>") {
            out.push(Tok::Ne);
            i += 2;
        } else if src[i..].starts_with("<=") {
            out.push(Tok::Le);
            i += 2;
        } else if src[i..].starts_with(">=") {
            out.push(Tok::Ge);
            i += 2;
        } else {
            let t_tok = match c {
                '=' => Tok::Eq,
                '<' => Tok::Lt,
                '>' => Tok::Gt,
                '+' => Tok::Plus,
                '-' => Tok::Minus,
                '*' => Tok::Mul,
                '/' => Tok::Div,
                '(' => Tok::LParen,
                ')' => Tok::RParen,
                ',' => Tok::Comma,
                ';' => Tok::Semi,
                ':' => Tok::Colon,
                '.' => Tok::Dot,
                _ => return Err(tr!("Caractere inesperado '{c}'", "Unexpected character '{c}'")),
            };
            out.push(t_tok);
            i += 1;
        }
    }
    Ok(out)
}

#[derive(Clone, Debug)]
enum Expr {
    Lit(Val),
    Var(String),
    Call(String, Vec<Expr>),
    UnOp(bool, Box<Expr>), // true = not, false = neg
    BinOp(BinOp, Box<Expr>, Box<Expr>),
}

#[derive(Clone, Copy, Debug)]
enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
}

#[derive(Clone, Debug)]
enum Stmt {
    Block(Vec<Stmt>),
    Assign(String, Expr),
    Call(String, Vec<Expr>),
    If(Expr, Box<Stmt>, Option<Box<Stmt>>),
    For { var: String, from: Expr, to: Expr, down: bool, body: Box<Stmt> },
    While(Expr, Box<Stmt>),
}

pub struct CompiledScript {
    body: Stmt,
}

pub fn compile(src: &str) -> Result<CompiledScript, String> {
    let toks = tokenize(src)?;
    let mut p = Parser { toks: &toks, pos: 0 };
    // Optional `var ...;` header before `begin ... end.`
    if p.eat_kw("var") {
        while !p.is_kw("begin") && !p.eof() {
            p.pos += 1;
        }
    }
    let body = if p.is_kw("begin") {
        p.parse_block()?
    } else {
        let mut stmts = Vec::new();
        while !p.eof() {
            stmts.push(p.parse_stmt()?);
            p.eat_semi();
        }
        Stmt::Block(stmts)
    };
    let _ = p.eat(&Tok::Dot);
    Ok(CompiledScript { body })
}

struct Parser<'a> {
    toks: &'a [Tok],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn eof(&self) -> bool {
        self.pos >= self.toks.len()
    }
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }
    fn is_kw(&self, kw: &str) -> bool {
        matches!(self.peek(), Some(Tok::Ident(s)) if s.eq_ignore_ascii_case(kw))
    }
    fn eat_kw(&mut self, kw: &str) -> bool {
        if self.is_kw(kw) {
            self.pos += 1;
            true
        } else {
            false
        }
    }
    fn eat(&mut self, t: &Tok) -> bool {
        if self.peek() == Some(t) {
            self.pos += 1;
            true
        } else {
            false
        }
    }
    fn eat_semi(&mut self) {
        while self.eat(&Tok::Semi) {}
    }

    fn parse_block(&mut self) -> Result<Stmt, String> {
        if !self.eat_kw("begin") {
            return Err(t("Esperado 'begin'.", "Expected 'begin'.").into());
        }
        let mut stmts = Vec::new();
        while !self.eof() && !self.is_kw("end") {
            stmts.push(self.parse_stmt()?);
            self.eat_semi();
        }
        if !self.eat_kw("end") {
            return Err(t("Esperado 'end'.", "Expected 'end'.").into());
        }
        Ok(Stmt::Block(stmts))
    }

    fn parse_stmt(&mut self) -> Result<Stmt, String> {
        if self.is_kw("begin") {
            return self.parse_block();
        }
        if self.eat_kw("if") {
            let cond = self.parse_expr()?;
            if !self.eat_kw("then") {
                return Err(t("Esperado 'then' após 'if'.", "Expected 'then' after 'if'.").into());
            }
            let then_s = Box::new(self.parse_stmt()?);
            let else_s = if self.eat_kw("else") { Some(Box::new(self.parse_stmt()?)) } else { None };
            return Ok(Stmt::If(cond, then_s, else_s));
        }
        if self.eat_kw("while") {
            let cond = self.parse_expr()?;
            if !self.eat_kw("do") {
                return Err(t("Esperado 'do' após 'while'.", "Expected 'do' after 'while'.").into());
            }
            let body = Box::new(self.parse_stmt()?);
            return Ok(Stmt::While(cond, body));
        }
        if self.eat_kw("for") {
            let Some(Tok::Ident(var)) = self.peek().cloned() else {
                return Err(t("Esperada variável após 'for'.", "Expected variable after 'for'.").into());
            };
            self.pos += 1;
            if !self.eat(&Tok::Assign) {
                return Err(t("Esperado ':=' no laço 'for'.", "Expected ':=' in 'for' loop.").into());
            }
            let from = self.parse_expr()?;
            let down = if self.eat_kw("downto") {
                true
            } else if self.eat_kw("to") {
                false
            } else {
                return Err(t("Esperado 'to' ou 'downto'.", "Expected 'to' or 'downto'.").into());
            };
            let to = self.parse_expr()?;
            if !self.eat_kw("do") {
                return Err(t("Esperado 'do' no laço 'for'.", "Expected 'do' in 'for' loop.").into());
            }
            let body = Box::new(self.parse_stmt()?);
            return Ok(Stmt::For { var, from, to, down, body });
        }
        let Some(Tok::Ident(name)) = self.peek().cloned() else {
            return Err(t("Comando inválido no script.", "Invalid statement in script.").into());
        };
        self.pos += 1;
        if self.eat(&Tok::Assign) {
            let val = self.parse_expr()?;
            Ok(Stmt::Assign(name, val))
        } else if self.eat(&Tok::LParen) {
            let args = self.parse_args()?;
            Ok(Stmt::Call(name, args))
        } else {
            Ok(Stmt::Call(name, Vec::new()))
        }
    }

    fn parse_args(&mut self) -> Result<Vec<Expr>, String> {
        let mut args = Vec::new();
        if !self.eat(&Tok::RParen) {
            loop {
                args.push(self.parse_expr()?);
                if self.eat(&Tok::RParen) {
                    break;
                }
                if !self.eat(&Tok::Comma) {
                    return Err(t("Esperada ',' ou ')' nos argumentos.", "Expected ',' or ')' in arguments.").into());
                }
            }
        }
        Ok(args)
    }

    fn parse_expr(&mut self) -> Result<Expr, String> {
        self.parse_or()
    }
    fn parse_or(&mut self) -> Result<Expr, String> {
        let mut lhs = self.parse_and()?;
        while self.eat_kw("or") {
            let rhs = self.parse_and()?;
            lhs = Expr::BinOp(BinOp::Or, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }
    fn parse_and(&mut self) -> Result<Expr, String> {
        let mut lhs = self.parse_cmp()?;
        while self.eat_kw("and") {
            let rhs = self.parse_cmp()?;
            lhs = Expr::BinOp(BinOp::And, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }
    fn parse_cmp(&mut self) -> Result<Expr, String> {
        let mut lhs = self.parse_add()?;
        loop {
            let op = match self.peek() {
                Some(Tok::Eq) => BinOp::Eq,
                Some(Tok::Ne) => BinOp::Ne,
                Some(Tok::Lt) => BinOp::Lt,
                Some(Tok::Le) => BinOp::Le,
                Some(Tok::Gt) => BinOp::Gt,
                Some(Tok::Ge) => BinOp::Ge,
                _ => break,
            };
            self.pos += 1;
            let rhs = self.parse_add()?;
            lhs = Expr::BinOp(op, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }
    fn parse_add(&mut self) -> Result<Expr, String> {
        let mut lhs = self.parse_mul()?;
        loop {
            let op = match self.peek() {
                Some(Tok::Plus) => BinOp::Add,
                Some(Tok::Minus) => BinOp::Sub,
                _ => break,
            };
            self.pos += 1;
            let rhs = self.parse_mul()?;
            lhs = Expr::BinOp(op, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }
    fn parse_mul(&mut self) -> Result<Expr, String> {
        let mut lhs = self.parse_unary()?;
        loop {
            let op = if self.eat(&Tok::Mul) {
                BinOp::Mul
            } else if self.eat(&Tok::Div) || self.eat_kw("div") {
                BinOp::Div
            } else if self.eat_kw("mod") {
                BinOp::Mod
            } else {
                break;
            };
            let rhs = self.parse_unary()?;
            lhs = Expr::BinOp(op, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }
    fn parse_unary(&mut self) -> Result<Expr, String> {
        if self.eat_kw("not") {
            return Ok(Expr::UnOp(true, Box::new(self.parse_unary()?)));
        }
        if self.eat(&Tok::Minus) {
            return Ok(Expr::UnOp(false, Box::new(self.parse_unary()?)));
        }
        self.parse_primary()
    }
    fn parse_primary(&mut self) -> Result<Expr, String> {
        match self.peek().cloned() {
            Some(Tok::Str(s)) => {
                self.pos += 1;
                Ok(Expr::Lit(Val::Str(s)))
            }
            Some(Tok::Int(n)) => {
                self.pos += 1;
                Ok(Expr::Lit(Val::Int(n)))
            }
            Some(Tok::Ident(name)) => {
                self.pos += 1;
                if name.eq_ignore_ascii_case("true") {
                    return Ok(Expr::Lit(Val::Bool(true)));
                }
                if name.eq_ignore_ascii_case("false") {
                    return Ok(Expr::Lit(Val::Bool(false)));
                }
                if self.eat(&Tok::LParen) {
                    let args = self.parse_args()?;
                    Ok(Expr::Call(name, args))
                } else {
                    Ok(Expr::Var(name))
                }
            }
            Some(Tok::LParen) => {
                self.pos += 1;
                let e = self.parse_expr()?;
                if !self.eat(&Tok::RParen) {
                    return Err(t("Esperado ')'.", "Expected ')'.").into());
                }
                Ok(e)
            }
            _ => Err(t("Expressão inválida no script.", "Invalid expression in script.").into()),
        }
    }
}

pub fn run(compiled: &CompiledScript, current_name: &str, idx: usize, entry: &Entry, loc: &Location) -> Result<String, String> {
    let mut env = Env {
        vars: HashMap::new(),
        entry,
        loc,
        steps: 0,
    };
    let path_str = loc.local_path(entry).map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|| shown(&entry.name));
    env.vars.insert("filename".into(), Val::Str(current_name.to_string()));
    env.vars.insert("widefilename".into(), Val::Str(current_name.to_string()));
    env.vars.insert("filepath".into(), Val::Str(path_str));
    env.vars.insert("fileindex".into(), Val::Int(idx as i64 + 1));

    env.exec(&compiled.body)?;
    Ok(env.vars.get("filename").map(Val::as_str).unwrap_or_else(|| current_name.to_string()))
}

struct Env<'a> {
    vars: HashMap<String, Val>,
    entry: &'a Entry,
    loc: &'a Location,
    steps: usize,
}

impl<'a> Env<'a> {
    fn tick(&mut self) -> Result<(), String> {
        self.steps += 1;
        if self.steps > 50_000 {
            Err(t("Script excedeu o limite de iterações.", "Script exceeded iteration limit.").into())
        } else {
            Ok(())
        }
    }

    fn exec(&mut self, s: &Stmt) -> Result<(), String> {
        self.tick()?;
        match s {
            Stmt::Block(list) => {
                for st in list {
                    self.exec(st)?;
                }
            }
            Stmt::Assign(name, expr) => {
                let v = self.eval(expr)?;
                let key = name.to_ascii_lowercase();
                if key == "widefilename" {
                    self.vars.insert("filename".into(), v.clone());
                }
                self.vars.insert(key, v);
            }
            Stmt::Call(name, args) => {
                self.call_proc(name, args)?;
            }
            Stmt::If(c, t, e) => {
                if self.eval(c)?.as_bool() {
                    self.exec(t)?;
                } else if let Some(eb) = e {
                    self.exec(eb)?;
                }
            }
            Stmt::While(c, b) => {
                while self.eval(c)?.as_bool() {
                    self.exec(b)?;
                }
            }
            Stmt::For { var, from, to, down, body } => {
                let start = self.eval(from)?.as_int();
                let end = self.eval(to)?.as_int();
                let key = var.to_ascii_lowercase();
                if *down {
                    let mut i = start;
                    while i >= end {
                        self.vars.insert(key.clone(), Val::Int(i));
                        self.exec(body)?;
                        i -= 1;
                    }
                } else {
                    let mut i = start;
                    while i <= end {
                        self.vars.insert(key.clone(), Val::Int(i));
                        self.exec(body)?;
                        i += 1;
                    }
                }
            }
        }
        Ok(())
    }

    fn eval(&mut self, e: &Expr) -> Result<Val, String> {
        match e {
            Expr::Lit(v) => Ok(v.clone()),
            Expr::Var(n) => Ok(self.vars.get(&n.to_ascii_lowercase()).cloned().unwrap_or(Val::Str(String::new()))),
            Expr::Call(n, args) => self.call_fn(n, args),
            Expr::UnOp(is_not, sub) => {
                let v = self.eval(sub)?;
                if *is_not { Ok(Val::Bool(!v.as_bool())) } else { Ok(Val::Int(-v.as_int())) }
            }
            Expr::BinOp(op, l, r) => {
                let lv = self.eval(l)?;
                let rv = self.eval(r)?;
                Ok(match op {
                    BinOp::Add => match (&lv, &rv) {
                        (Val::Int(a), Val::Int(b)) => Val::Int(a + b),
                        _ => Val::Str(format!("{}{}", lv.as_str(), rv.as_str())),
                    },
                    BinOp::Sub => Val::Int(lv.as_int() - rv.as_int()),
                    BinOp::Mul => Val::Int(lv.as_int() * rv.as_int()),
                    BinOp::Div => Val::Int(if rv.as_int() == 0 { 0 } else { lv.as_int() / rv.as_int() }),
                    BinOp::Mod => Val::Int(if rv.as_int() == 0 { 0 } else { lv.as_int() % rv.as_int() }),
                    BinOp::Eq => Val::Bool(lv.as_str() == rv.as_str()),
                    BinOp::Ne => Val::Bool(lv.as_str() != rv.as_str()),
                    BinOp::Lt => Val::Bool(lv.as_int() < rv.as_int()),
                    BinOp::Le => Val::Bool(lv.as_int() <= rv.as_int()),
                    BinOp::Gt => Val::Bool(lv.as_int() > rv.as_int()),
                    BinOp::Ge => Val::Bool(lv.as_int() >= rv.as_int()),
                    BinOp::And => Val::Bool(lv.as_bool() && rv.as_bool()),
                    BinOp::Or => Val::Bool(lv.as_bool() || rv.as_bool()),
                })
            }
        }
    }

    fn call_proc(&mut self, name: &str, args: &[Expr]) -> Result<(), String> {
        let lower = name.to_ascii_lowercase();
        match lower.as_str() {
            "widedelete" | "delete" if args.len() == 3 => {
                if let Expr::Var(vname) = &args[0] {
                    let s = self.vars.get(&vname.to_ascii_lowercase()).map(Val::as_str).unwrap_or_default();
                    let idx = (self.eval(&args[1])?.as_int() - 1).max(0) as usize;
                    let cnt = self.eval(&args[2])?.as_int().max(0) as usize;
                    let mut chars: Vec<char> = s.chars().collect();
                    let end = (idx + cnt).min(chars.len());
                    if idx < chars.len() {
                        chars.drain(idx..end);
                    }
                    self.vars.insert(vname.to_ascii_lowercase(), Val::Str(chars.into_iter().collect()));
                }
                Ok(())
            }
            "wideinsert" | "insert" if args.len() == 3 => {
                let ins = self.eval(&args[0])?.as_str();
                if let Expr::Var(vname) = &args[1] {
                    let s = self.vars.get(&vname.to_ascii_lowercase()).map(Val::as_str).unwrap_or_default();
                    let idx = (self.eval(&args[2])?.as_int() - 1).max(0) as usize;
                    let mut chars: Vec<char> = s.chars().collect();
                    let at = idx.min(chars.len());
                    for (off, c) in ins.chars().enumerate() {
                        chars.insert(at + off, c);
                    }
                    self.vars.insert(vname.to_ascii_lowercase(), Val::Str(chars.into_iter().collect()));
                }
                Ok(())
            }
            _ => {
                let _ = self.call_fn(name, args)?;
                Ok(())
            }
        }
    }

    fn call_fn(&mut self, name: &str, args: &[Expr]) -> Result<Val, String> {
        let mut vals = Vec::with_capacity(args.len());
        for a in args {
            vals.push(self.eval(a)?);
        }
        let s = |i: usize| vals.get(i).map(Val::as_str).unwrap_or_default();
        let n = |i: usize| vals.get(i).map(Val::as_int).unwrap_or(0);
        let lower = name.to_ascii_lowercase();
        Ok(match lower.as_str() {
            "wideextractbasename" | "extractbasename" => {
                let x = s(0);
                Val::Str(match x.rfind('.') {
                    Some(i) if i > 0 => x[..i].to_string(),
                    _ => x,
                })
            }
            "wideextractfileext" | "extractfileext" => {
                let x = s(0);
                Val::Str(match x.rfind('.') {
                    Some(i) if i > 0 => x[i..].to_string(),
                    _ => String::new(),
                })
            }
            "wideextractfilename" | "extractfilename" => {
                let x = s(0);
                Val::Str(x.rsplit(['\\', '/']).next().unwrap_or(&x).to_string())
            }
            "widereplacestr" | "replacestr" => Val::Str(s(0).replace(&s(1), &s(2))),
            "widereplacetext" | "replacetext" => {
                let pat = regex::escape(&s(1));
                let re = regex::RegexBuilder::new(&pat).case_insensitive(true).build().ok();
                Val::Str(re.map_or_else(|| s(0), |r| r.replace_all(&s(0), regex::NoExpand(&s(2))).into_owned()))
            }
            "wideuppercase" | "uppercase" => Val::Str(s(0).to_uppercase()),
            "widelowercase" | "lowercase" => Val::Str(s(0).to_lowercase()),
            "widecasecapitalize" => Val::Str(capitalize_words(&s(0))),
            "widetrim" | "trim" => Val::Str(s(0).trim().to_string()),
            "widelength" | "length" => Val::Int(s(0).chars().count() as i64),
            "widepos" | "pos" => {
                let sub = s(0);
                let hay = s(1);
                Val::Int(hay.find(&sub).map_or(0, |b| hay[..b].chars().count() as i64 + 1))
            }
            "widecopy" | "copy" => {
                let hay = s(0);
                let start = (n(1) - 1).max(0) as usize;
                let cnt = n(2).max(0) as usize;
                Val::Str(hay.chars().skip(start).take(cnt).collect())
            }
            "inttostr" => Val::Str(n(0).to_string()),
            "strtoint" => Val::Int(s(0).trim().parse().unwrap_or(0)),
            "strtointdef" => Val::Int(s(0).trim().parse().unwrap_or_else(|_| n(1))),
            "calculatemetatagformat" | "metatag" => Val::Str(rules_meta::expand_tags(&s(0), self.entry, self.loc)),
            _ => return Err(tr!("Função desconhecida: {name}", "Unknown function: {name}")),
        })
    }
}

fn capitalize_words(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut cap = true;
    for ch in s.chars() {
        if !ch.is_alphanumeric() {
            cap = true;
            out.push(ch);
        } else if cap {
            for uc in ch.to_uppercase() {
                out.push(uc);
            }
            cap = false;
        } else {
            for lc in ch.to_lowercase() {
                out.push(lc);
            }
        }
    }
    out
}

pub const SCRIPT_PRESETS: &[(&str, &str, &str)] = &[
    (
        "Numerar arquivos (01, 02...)",
        "Number files (01, 02...)",
        "begin\n  FileName := IntToStr(FileIndex) + ' - ' + FileName;\nend.",
    ),
    (
        "Substituir texto e converter para minúsculas",
        "Replace text and lowercase",
        "begin\n  FileName := WideLowerCase(WideReplaceStr(FileName, ' ', '_'));\nend.",
    ),
    (
        "Inserir data de modificação (Meta Tag)",
        "Insert modified date (Meta Tag)",
        "begin\n  FileName := MetaTag(':File_DateModified:') + ' - ' + FileName;\nend.",
    ),
    (
        "Remover texto entre parênteses",
        "Strip text in parentheses",
        "var\n  a, b: Integer;\nbegin\n  a := WidePos('(', FileName);\n  b := WidePos(')', FileName);\n  if (a > 0) and (b > a) then\n    WideDelete(FileName, a, b - a + 1);\nend.",
    ),
];
