//! The `Requires` boolean expression grammar.
//!
//! Grammar: variable tokens joined by `AND` / `OR`, with `!` (or `NOT`) for
//! negation. Precedence: `! > AND > OR`. No parentheses.
//!
//! ```text
//! expr  := and ( "OR" and )*
//! and   := atom ( "AND" atom )*
//! atom  := "!" atom | IDENT
//! ```
//!
//! An empty / whitespace-only expression evaluates to `true` (no gate).

use crate::error::{Result, RitzError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expr {
    Var(String),
    Not(Box<Expr>),
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
}

impl Expr {
    /// Evaluate against a truthiness lookup for variable names.
    pub fn eval(&self, lookup: &dyn Fn(&str) -> bool) -> bool {
        match self {
            Expr::Var(name) => lookup(name),
            Expr::Not(inner) => !inner.eval(lookup),
            Expr::And(a, b) => a.eval(lookup) && b.eval(lookup),
            Expr::Or(a, b) => a.eval(lookup) || b.eval(lookup),
        }
    }

    /// All variable names referenced (for validation / dependency tracking).
    pub fn referenced_vars(&self) -> Vec<String> {
        let mut out = Vec::new();
        self.collect(&mut out);
        out
    }

    fn collect(&self, out: &mut Vec<String>) {
        match self {
            Expr::Var(name) => out.push(name.clone()),
            Expr::Not(inner) => inner.collect(out),
            Expr::And(a, b) | Expr::Or(a, b) => {
                a.collect(out);
                b.collect(out);
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    And,
    Or,
    Not,
    Ident(String),
}

fn is_ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == ':'
}

fn tokenize(s: &str) -> Result<Vec<Tok>> {
    let mut toks = Vec::new();
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
        } else if c == '!' {
            toks.push(Tok::Not);
            i += 1;
        } else if is_ident_char(c) {
            let start = i;
            while i < chars.len() && is_ident_char(chars[i]) {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            match word.as_str() {
                "AND" => toks.push(Tok::And),
                "OR" => toks.push(Tok::Or),
                "NOT" => toks.push(Tok::Not),
                _ => toks.push(Tok::Ident(word)),
            }
        } else {
            return Err(RitzError::Condition(format!(
                "unexpected character `{c}` in condition `{s}`"
            )));
        }
    }
    Ok(toks)
}

struct Parser {
    toks: Vec<Tok>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }

    fn next(&mut self) -> Option<Tok> {
        let t = self.toks.get(self.pos).cloned();
        if t.is_some() {
            self.pos += 1;
        }
        t
    }

    fn parse_or(&mut self) -> Result<Expr> {
        let mut left = self.parse_and()?;
        while matches!(self.peek(), Some(Tok::Or)) {
            self.next();
            let right = self.parse_and()?;
            left = Expr::Or(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_and(&mut self) -> Result<Expr> {
        let mut left = self.parse_atom()?;
        while matches!(self.peek(), Some(Tok::And)) {
            self.next();
            let right = self.parse_atom()?;
            left = Expr::And(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_atom(&mut self) -> Result<Expr> {
        match self.next() {
            Some(Tok::Not) => Ok(Expr::Not(Box::new(self.parse_atom()?))),
            Some(Tok::Ident(name)) => Ok(Expr::Var(name)),
            Some(other) => Err(RitzError::Condition(format!(
                "expected variable, found `{other:?}`"
            ))),
            None => Err(RitzError::Condition("unexpected end of condition".into())),
        }
    }
}

/// Parse a condition string into an [`Expr`]. Empty input yields `None`
/// (meaning "always true").
pub fn parse(s: &str) -> Result<Option<Expr>> {
    let toks = tokenize(s)?;
    if toks.is_empty() {
        return Ok(None);
    }
    let mut parser = Parser { toks, pos: 0 };
    let expr = parser.parse_or()?;
    if parser.pos != parser.toks.len() {
        return Err(RitzError::Condition(format!(
            "trailing tokens in condition `{s}`"
        )));
    }
    Ok(Some(expr))
}

/// Convenience: parse and evaluate. Empty / `None` requires => `true`.
pub fn eval_opt(requires: Option<&str>, lookup: &dyn Fn(&str) -> bool) -> Result<bool> {
    match requires {
        None => Ok(true),
        Some(s) => match parse(s)? {
            None => Ok(true),
            Some(expr) => Ok(expr.eval(lookup)),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn lookup_from(truthy: &[&str]) -> impl Fn(&str) -> bool {
        let set: HashSet<String> = truthy.iter().map(|s| s.to_string()).collect();
        move |name: &str| set.contains(name)
    }

    fn ev(expr: &str, truthy: &[&str]) -> bool {
        eval_opt(Some(expr), &lookup_from(truthy)).unwrap()
    }

    #[test]
    fn empty_is_true() {
        assert!(eval_opt(None, &lookup_from(&[])).unwrap());
        assert!(eval_opt(Some("   "), &lookup_from(&[])).unwrap());
    }

    #[test]
    fn single_var() {
        assert!(ev("a", &["a"]));
        assert!(!ev("a", &[]));
    }

    #[test]
    fn negation() {
        assert!(ev("!a", &[]));
        assert!(!ev("!a", &["a"]));
        assert!(ev("!!a", &["a"]));
    }

    #[test]
    fn and_or() {
        assert!(ev("a AND b", &["a", "b"]));
        assert!(!ev("a AND b", &["a"]));
        assert!(ev("a OR b", &["a"]));
        assert!(!ev("a OR b", &[]));
    }

    #[test]
    fn precedence_and_binds_tighter_than_or() {
        // a OR (b AND c)
        assert!(ev("a OR b AND c", &["a"]));
        assert!(!ev("a OR b AND c", &["b"]));
        assert!(ev("a OR b AND c", &["b", "c"]));
    }

    #[test]
    fn combined_with_negation() {
        // fsr_enabled AND !native_res
        assert!(ev("fsr_enabled AND !native_res", &["fsr_enabled"]));
        assert!(!ev("fsr_enabled AND !native_res", &["fsr_enabled", "native_res"]));
    }

    #[test]
    fn global_identifiers_parse() {
        assert!(ev("global:hdr_on", &["global:hdr_on"]));
    }

    #[test]
    fn referenced_vars_collects_all() {
        let expr = parse("a AND !b OR global:c").unwrap().unwrap();
        let vars = expr.referenced_vars();
        assert_eq!(vars, vec!["a", "b", "global:c"]);
    }

    #[test]
    fn bad_input_errors() {
        assert!(parse("a AND").is_err());
        assert!(parse("AND a").is_err());
        assert!(parse("a @ b").is_err());
    }
}
