//! Port of `desktop-runtime/templateEngine.cjs`.
//!
//! desktop runtime expanded templates in the main process (`parseTemplate`) before
//! rendering a document or sending a message; the Rust shell must produce the
//! same output. The JS engine is a small, `eval`-free template language:
//!
//! ```text
//!  $order.id                  shorthand variable (dot paths only, HTML-escaped)
//!  ${order.qty * order.price} full expression, may include filters (escaped)
//!  {{order.customerName}}     same as ${...} (HTML-escaped)
//!  {{{order.rawHtmlNote}}}    triple-brace = unescaped / raw HTML
//!  {{#each order.items as item}} ... {{/each}}   loop over an array
//!  {{#if order.total > 100}} ... {{else}} ... {{/if}}   conditional
//!  {{order.total | currency:'USD'}}   pipe filters
//! ```
//!
//! The port keeps JavaScript's semantics deliberately: `+` string coercion,
//! loose `==`, array/object truthiness, `Object.create` scope fall-through for
//! `{{#each}}`, and the generated `username`/`date`/`digi_no`/… variables.
//!
//! ## Known divergences
//! - `usernameFromEmail` cannot run `String.prototype.normalize('NFKD')`
//!   without a Unicode crate, so [`strip_diacritics`] folds the common Latin-1
//!   and Latin-Extended letters to their base character instead of decomposing
//!   every code point. ASCII local-parts (the overwhelmingly common case) are
//!   byte-for-byte identical.
//! - The `currency` filter implements the `en-US` `Intl.NumberFormat` layout
//!   directly (symbol table + grouping) instead of carrying the ICU data; the
//!   `locale` argument falls back to the currency code when it is not `en-US`.
//! - JS number → string for magnitudes ≥ 1e21 (which render as `1e+21`) is
//!   printed in full.

use std::collections::{BTreeMap, HashSet};
use std::sync::OnceLock;

use chrono::{DateTime, Datelike, Local, NaiveDate, NaiveDateTime, TimeZone, Timelike, Utc};
use regex::{Captures, Regex};
use serde_json::{Map, Value, json};

// ---------------------------------------------------------------------------
// Runtime values
// ---------------------------------------------------------------------------

/// A template value. Mirrors the JSON shapes the JS engine sees, plus the
/// `SafeString` marker the `safe` filter returns (which bypasses HTML
/// escaping).
#[derive(Clone, Debug, PartialEq)]
enum Val {
    Undef,
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Val>),
    Obj(BTreeMap<String, Val>),
    Safe(String),
}

impl Val {
    fn str(value: impl Into<String>) -> Self {
        Val::Str(value.into())
    }
}

fn from_json(value: &Value) -> Val {
    match value {
        Value::Null => Val::Null,
        Value::Bool(flag) => Val::Bool(*flag),
        Value::Number(number) => Val::Num(number.as_f64().unwrap_or(f64::NAN)),
        Value::String(text) => Val::Str(text.clone()),
        Value::Array(items) => Val::Arr(items.iter().map(from_json).collect()),
        Value::Object(map) => Val::Obj(
            map.iter()
                .map(|(key, item)| (key.clone(), from_json(item)))
                .collect(),
        ),
    }
}

/// Re-serialise a value the way `JSON.stringify` would (used by `| json`).
fn to_json(value: &Val) -> Value {
    match value {
        Val::Undef => Value::Null,
        Val::Null => Value::Null,
        Val::Bool(flag) => Value::Bool(*flag),
        Val::Num(number) => {
            // `JSON.stringify` prints whole numbers without a decimal point.
            if number.is_finite()
                && number.fract() == 0.0
                && number.abs() < 9.007_199_254_740_992e15
            {
                Value::Number(serde_json::Number::from(*number as i64))
            } else {
                serde_json::Number::from_f64(*number)
                    .map(Value::Number)
                    .unwrap_or(Value::Null)
            }
        }
        Val::Str(text) | Val::Safe(text) => Value::String(text.clone()),
        Val::Arr(items) => Value::Array(items.iter().map(to_json).collect()),
        Val::Obj(map) => Value::Object(
            map.iter()
                .map(|(key, item)| (key.clone(), to_json(item)))
                .collect(),
        ),
    }
}

// ---------------------------------------------------------------------------
// JavaScript coercion helpers
// ---------------------------------------------------------------------------

fn is_nullish(value: &Val) -> bool {
    matches!(value, Val::Undef | Val::Null)
}

fn js_num_to_string(number: f64) -> String {
    if number.is_nan() {
        return "NaN".to_string();
    }
    if number == f64::INFINITY {
        return "Infinity".to_string();
    }
    if number == f64::NEG_INFINITY {
        return "-Infinity".to_string();
    }
    if number == 0.0 {
        return "0".to_string();
    }
    if number.fract() == 0.0 && number.abs() < 1e21 {
        if number.abs() < 9.007_199_254_740_992e15 {
            return format!("{}", number as i64);
        }
        return format!("{number:.0}");
    }
    format!("{number}")
}

/// `String(value)` — note arrays join their elements, converting `null` /
/// `undefined` to the empty string.
fn js_string(value: &Val) -> String {
    match value {
        Val::Undef => "undefined".to_string(),
        Val::Null => "null".to_string(),
        Val::Bool(flag) => flag.to_string(),
        Val::Num(number) => js_num_to_string(*number),
        Val::Str(text) | Val::Safe(text) => text.clone(),
        Val::Arr(items) => items
            .iter()
            .map(|item| {
                if is_nullish(item) {
                    String::new()
                } else {
                    js_string(item)
                }
            })
            .collect::<Vec<_>>()
            .join(","),
        Val::Obj(_) => "[object Object]".to_string(),
    }
}

fn parse_js_number(raw: &str) -> f64 {
    let text = raw.trim();
    if text.is_empty() {
        return 0.0;
    }
    match text {
        "Infinity" | "+Infinity" => return f64::INFINITY,
        "-Infinity" => return f64::NEG_INFINITY,
        _ => {}
    }
    let lower = text.to_ascii_lowercase();
    if let Some(hex) = lower.strip_prefix("0x") {
        return i64::from_str_radix(hex, 16)
            .map(|v| v as f64)
            .unwrap_or(f64::NAN);
    }
    if let Some(bin) = lower.strip_prefix("0b") {
        return i64::from_str_radix(bin, 2)
            .map(|v| v as f64)
            .unwrap_or(f64::NAN);
    }
    if let Some(oct) = lower.strip_prefix("0o") {
        return i64::from_str_radix(oct, 8)
            .map(|v| v as f64)
            .unwrap_or(f64::NAN);
    }
    // Reject anything Rust's float parser accepts that JS's does not
    // (`inf`, `nan`, …). `Number("nan")` is `NaN` in JS, not a float.
    if !text
        .chars()
        .all(|c| c.is_ascii_digit() || matches!(c, '+' | '-' | '.' | 'e' | 'E'))
    {
        return f64::NAN;
    }
    text.parse::<f64>().unwrap_or(f64::NAN)
}

fn js_number(value: &Val) -> f64 {
    match value {
        Val::Undef => f64::NAN,
        Val::Null => 0.0,
        Val::Bool(flag) => {
            if *flag {
                1.0
            } else {
                0.0
            }
        }
        Val::Num(number) => *number,
        Val::Str(text) | Val::Safe(text) => parse_js_number(text),
        Val::Arr(items) => {
            if items.is_empty() {
                0.0
            } else if items.len() == 1 {
                js_number(&items[0])
            } else {
                f64::NAN
            }
        }
        Val::Obj(_) => f64::NAN,
    }
}

fn truthy(value: &Val) -> bool {
    match value {
        Val::Undef | Val::Null => false,
        Val::Bool(flag) => *flag,
        Val::Num(number) => *number != 0.0 && !number.is_nan(),
        Val::Str(text) => !text.is_empty(),
        Val::Arr(items) => !items.is_empty(),
        Val::Obj(_) => true,
        Val::Safe(_) => true,
    }
}

fn is_objectish(value: &Val) -> bool {
    matches!(value, Val::Arr(_) | Val::Obj(_))
}

fn to_primitive(value: &Val) -> Val {
    match value {
        Val::Arr(_) => Val::Str(js_string(value)),
        Val::Obj(_) => Val::Str("[object Object]".to_string()),
        other => other.clone(),
    }
}

fn strict_eq(left: &Val, right: &Val) -> bool {
    match (left, right) {
        (Val::Undef, Val::Undef) => true,
        (Val::Null, Val::Null) => true,
        (Val::Bool(a), Val::Bool(b)) => a == b,
        (Val::Num(a), Val::Num(b)) => a == b,
        (Val::Str(a), Val::Str(b)) => a == b,
        (Val::Safe(a), Val::Safe(b)) => a == b,
        _ => false,
    }
}

fn loose_eq(left: &Val, right: &Val) -> bool {
    if is_nullish(left) && is_nullish(right) {
        return true;
    }
    if is_nullish(left) || is_nullish(right) {
        return false;
    }
    let left = to_primitive(left);
    let right = to_primitive(right);
    if is_objectish(&left) || is_objectish(&right) {
        return strict_eq(&left, &right);
    }
    match (&left, &right) {
        (Val::Bool(_), _) | (_, Val::Bool(_)) => js_number(&left) == js_number(&right),
        (Val::Num(a), Val::Num(b)) => a == b,
        (Val::Str(a), Val::Str(b)) => a == b,
        (Val::Num(a), Val::Str(b)) | (Val::Str(b), Val::Num(a)) => *a == parse_js_number(b),
        (Val::Safe(a), Val::Num(b)) | (Val::Num(b), Val::Safe(a)) => *b == parse_js_number(a),
        (Val::Safe(a), Val::Str(b)) | (Val::Str(b), Val::Safe(a)) => a == b,
        (Val::Safe(a), Val::Safe(b)) => a == b,
        _ => false,
    }
}

fn relational(op: &str, left: &Val, right: &Val) -> bool {
    let both_strings = matches!(left, Val::Str(_)) && matches!(right, Val::Str(_));
    if both_strings {
        let a: Vec<u16> = js_string(left).encode_utf16().collect();
        let b: Vec<u16> = js_string(right).encode_utf16().collect();
        return match op {
            "<" => a < b,
            ">" => a > b,
            "<=" => a <= b,
            _ => a >= b,
        };
    }
    let a = js_number(left);
    let b = js_number(right);
    if a.is_nan() || b.is_nan() {
        return false;
    }
    match op {
        "<" => a < b,
        ">" => a > b,
        "<=" => a <= b,
        _ => a >= b,
    }
}

fn js_add(left: &Val, right: &Val) -> Val {
    let string_concat = matches!(left, Val::Str(_)) || matches!(right, Val::Str(_));
    if string_concat {
        let l = if is_nullish(left) {
            String::new()
        } else {
            js_string(left)
        };
        let r = if is_nullish(right) {
            String::new()
        } else {
            js_string(right)
        };
        Val::Str(format!("{l}{r}"))
    } else {
        Val::Num(js_number(left) + js_number(right))
    }
}

// ---------------------------------------------------------------------------
// Expression tokenizer
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
enum Token {
    Num(f64),
    Str(String),
    Bool(bool),
    Null,
    Ident(String),
    Op(String),
}

fn tokenize_expression(expr: &str) -> Result<Vec<Token>, String> {
    let chars: Vec<char> = expr.chars().collect();
    let len = chars.len();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < len {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        if c == '"' || c == '\'' {
            let quote = c;
            let mut j = i + 1;
            let mut text = String::new();
            while j < len && chars[j] != quote {
                if chars[j] == '\\' && j + 1 < len {
                    text.push(chars[j + 1]);
                    j += 2;
                } else {
                    text.push(chars[j]);
                    j += 1;
                }
            }
            if j >= len {
                return Err("Unterminated string literal in expression".to_string());
            }
            tokens.push(Token::Str(text));
            i = j + 1;
            continue;
        }
        if c.is_ascii_digit() || (c == '.' && i + 1 < len && chars[i + 1].is_ascii_digit()) {
            let mut j = i;
            while j < len && (chars[j].is_ascii_digit() || chars[j] == '.') {
                j += 1;
            }
            let slice: String = chars[i..j].iter().collect();
            tokens.push(Token::Num(slice.parse::<f64>().unwrap_or(f64::NAN)));
            i = j;
            continue;
        }
        if c.is_ascii_alphabetic() || c == '_' || c == '@' {
            let mut j = i + 1;
            while j < len && (chars[j].is_ascii_alphanumeric() || chars[j] == '_') {
                j += 1;
            }
            let word: String = chars[i..j].iter().collect();
            match word.as_str() {
                "true" => tokens.push(Token::Bool(true)),
                "false" => tokens.push(Token::Bool(false)),
                "null" | "undefined" => tokens.push(Token::Null),
                _ => tokens.push(Token::Ident(word)),
            }
            i = j;
            continue;
        }
        let three: String = chars[i..(i + 3).min(len)].iter().collect();
        let two: String = chars[i..(i + 2).min(len)].iter().collect();
        if three == "===" || three == "!==" {
            tokens.push(Token::Op(three));
            i += 3;
            continue;
        }
        if ["==", "!=", "<=", ">=", "&&", "||"].contains(&two.as_str()) {
            tokens.push(Token::Op(two));
            i += 2;
            continue;
        }
        if "()+-*/%<>!.[],".contains(c) {
            tokens.push(Token::Op(c.to_string()));
            i += 1;
            continue;
        }
        return Err(format!(
            "Unexpected character \"{c}\" in expression: {expr}"
        ));
    }
    Ok(tokens)
}

// ---------------------------------------------------------------------------
// Expression parser (recursive descent, no eval)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
enum Node {
    Literal(Val),
    Path(Vec<PathPart>),
    Not(Box<Node>),
    Neg(Box<Node>),
    And(Box<Node>, Box<Node>),
    Or(Box<Node>, Box<Node>),
    Binary(String, Box<Node>, Box<Node>),
}

#[derive(Clone, Debug)]
enum PathPart {
    Key(String),
    Computed(Box<Node>),
}

struct Parser<'a> {
    tokens: Vec<Token>,
    pos: usize,
    source: &'a str,
}

impl<'a> Parser<'a> {
    fn new(tokens: Vec<Token>, source: &'a str) -> Self {
        Self {
            tokens,
            pos: 0,
            source,
        }
    }

    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn advance(&mut self) -> Option<Token> {
        let token = self.tokens.get(self.pos).cloned();
        if token.is_some() {
            self.pos += 1;
        }
        token
    }

    fn is_op(&self, op: &str) -> bool {
        matches!(self.peek(), Some(Token::Op(value)) if value == op)
    }

    fn expect_op(&mut self, op: &str) -> Result<(), String> {
        if self.is_op(op) {
            self.pos += 1;
            Ok(())
        } else {
            Err(format!("Expected \"{op}\" in expression: {}", self.source))
        }
    }

    fn parse_primary(&mut self) -> Result<Node, String> {
        let token = self
            .peek()
            .cloned()
            .ok_or_else(|| format!("Unexpected end of expression: {}", self.source))?;
        match token {
            Token::Num(_) | Token::Str(_) | Token::Bool(_) | Token::Null => {
                self.pos += 1;
                Ok(Node::Literal(token_to_value(&token)))
            }
            Token::Op(op) if op == "(" => {
                self.pos += 1;
                let node = self.parse_or()?;
                self.expect_op(")")?;
                Ok(node)
            }
            Token::Op(op) if op == "!" => {
                self.pos += 1;
                Ok(Node::Not(Box::new(self.parse_unary()?)))
            }
            Token::Op(op) if op == "-" => {
                self.pos += 1;
                Ok(Node::Neg(Box::new(self.parse_unary()?)))
            }
            Token::Ident(word) => {
                self.pos += 1;
                let mut path = vec![PathPart::Key(word)];
                while let Some(Token::Op(op)) = self.peek() {
                    if op == "." {
                        self.pos += 1;
                        match self.advance() {
                            Some(Token::Ident(next)) => path.push(PathPart::Key(next)),
                            _ => {
                                return Err(format!(
                                    "Expected identifier after \".\" in: {}",
                                    self.source
                                ));
                            }
                        }
                    } else if op == "[" {
                        self.pos += 1;
                        let index = self.parse_or()?;
                        self.expect_op("]")?;
                        path.push(PathPart::Computed(Box::new(index)));
                    } else {
                        break;
                    }
                }
                Ok(Node::Path(path))
            }
            _ => Err(format!("Unexpected token in expression: {}", self.source)),
        }
    }

    fn parse_unary(&mut self) -> Result<Node, String> {
        self.parse_primary()
    }

    fn parse_mul(&mut self) -> Result<Node, String> {
        let mut node = self.parse_unary()?;
        while let Some(Token::Op(op)) = self.peek() {
            if ["*", "/", "%"].contains(&op.as_str()) {
                let op = op.clone();
                self.pos += 1;
                node = Node::Binary(op, Box::new(node), Box::new(self.parse_unary()?));
            } else {
                break;
            }
        }
        Ok(node)
    }

    fn parse_add(&mut self) -> Result<Node, String> {
        let mut node = self.parse_mul()?;
        while let Some(Token::Op(op)) = self.peek() {
            if ["+", "-"].contains(&op.as_str()) {
                let op = op.clone();
                self.pos += 1;
                node = Node::Binary(op, Box::new(node), Box::new(self.parse_mul()?));
            } else {
                break;
            }
        }
        Ok(node)
    }

    fn parse_rel(&mut self) -> Result<Node, String> {
        let mut node = self.parse_add()?;
        while let Some(Token::Op(op)) = self.peek() {
            if ["<", ">", "<=", ">="].contains(&op.as_str()) {
                let op = op.clone();
                self.pos += 1;
                node = Node::Binary(op, Box::new(node), Box::new(self.parse_add()?));
            } else {
                break;
            }
        }
        Ok(node)
    }

    fn parse_eq(&mut self) -> Result<Node, String> {
        let mut node = self.parse_rel()?;
        while let Some(Token::Op(op)) = self.peek() {
            if ["==", "!=", "===", "!=="].contains(&op.as_str()) {
                let op = op.clone();
                self.pos += 1;
                node = Node::Binary(op, Box::new(node), Box::new(self.parse_rel()?));
            } else {
                break;
            }
        }
        Ok(node)
    }

    fn parse_and(&mut self) -> Result<Node, String> {
        let mut node = self.parse_eq()?;
        loop {
            match self.peek() {
                Some(Token::Op(op)) if op == "&&" => {
                    self.pos += 1;
                    node = Node::And(Box::new(node), Box::new(self.parse_eq()?));
                }
                _ => break,
            }
        }
        Ok(node)
    }

    fn parse_or(&mut self) -> Result<Node, String> {
        let mut node = self.parse_and()?;
        loop {
            match self.peek() {
                Some(Token::Op(op)) if op == "||" => {
                    self.pos += 1;
                    node = Node::Or(Box::new(node), Box::new(self.parse_and()?));
                }
                _ => break,
            }
        }
        Ok(node)
    }
}

fn token_to_value(token: &Token) -> Val {
    match token {
        Token::Num(number) => Val::Num(*number),
        Token::Str(text) => Val::Str(text.clone()),
        Token::Bool(flag) => Val::Bool(*flag),
        Token::Null => Val::Null,
        Token::Ident(word) => Val::Str(word.clone()),
        Token::Op(op) => Val::Str(op.clone()),
    }
}

fn parse_expression(tokens: Vec<Token>, source: &str) -> Result<Node, String> {
    let len = tokens.len();
    let mut parser = Parser::new(tokens, source);
    let node = parser.parse_or()?;
    if parser.pos < len {
        return Err(format!(
            "Unexpected trailing tokens in expression: {source}"
        ));
    }
    Ok(node)
}

// ---------------------------------------------------------------------------
// Path resolution + evaluation
// ---------------------------------------------------------------------------

fn get_key(value: &Val, key: &str) -> Val {
    match value {
        Val::Obj(map) => map.get(key).cloned().unwrap_or(Val::Undef),
        Val::Arr(items) => {
            if key == "length" {
                Val::Num(items.len() as f64)
            } else if let Ok(index) = key.parse::<usize>() {
                items.get(index).cloned().unwrap_or(Val::Undef)
            } else {
                Val::Undef
            }
        }
        Val::Str(text) => {
            if key == "length" {
                Val::Num(text.encode_utf16().count() as f64)
            } else if let Ok(index) = key.parse::<usize>() {
                text.chars()
                    .nth(index)
                    .map(|c| Val::str(c.to_string()))
                    .unwrap_or(Val::Undef)
            } else {
                Val::Undef
            }
        }
        _ => Val::Undef,
    }
}

fn index_value(value: &Val, index: &Val) -> Val {
    match index {
        Val::Num(_) | Val::Str(_) | Val::Safe(_) => get_key(value, &js_string(index)),
        _ => Val::Undef,
    }
}

fn resolve_path(parts: &[PathPart], context: &Val) -> Val {
    let mut value = context.clone();
    for part in parts {
        if is_nullish(&value) {
            return Val::Undef;
        }
        match part {
            PathPart::Key(key) => value = get_key(&value, key),
            PathPart::Computed(node) => {
                let index = eval(node, context).unwrap_or(Val::Undef);
                value = index_value(&value, &index);
            }
        }
    }
    value
}

fn eval(node: &Node, context: &Val) -> Result<Val, String> {
    match node {
        Node::Literal(value) => Ok(value.clone()),
        Node::Path(parts) => Ok(resolve_path(parts, context)),
        Node::Not(inner) => Ok(Val::Bool(!truthy(&eval(inner, context)?))),
        Node::Neg(inner) => Ok(Val::Num(-js_number(&eval(inner, context)?))),
        Node::And(left, right) => {
            let value = eval(left, context)?;
            if truthy(&value) {
                eval(right, context)
            } else {
                Ok(value)
            }
        }
        Node::Or(left, right) => {
            let value = eval(left, context)?;
            if truthy(&value) {
                Ok(value)
            } else {
                eval(right, context)
            }
        }
        Node::Binary(op, left, right) => {
            let l = eval(left, context)?;
            let r = eval(right, context)?;
            Ok(match op.as_str() {
                "+" => js_add(&l, &r),
                "-" => Val::Num(js_number(&l) - js_number(&r)),
                "*" => Val::Num(js_number(&l) * js_number(&r)),
                "/" => Val::Num(js_number(&l) / js_number(&r)),
                "%" => Val::Num(js_number(&l) % js_number(&r)),
                "==" => {
                    // `l === undefined ? r === undefined : l == r`
                    if matches!(l, Val::Undef) {
                        Val::Bool(matches!(r, Val::Undef))
                    } else {
                        Val::Bool(loose_eq(&l, &r))
                    }
                }
                "!=" => Val::Bool(!loose_eq(&l, &r)),
                "===" => Val::Bool(strict_eq(&l, &r)),
                "!==" => Val::Bool(!strict_eq(&l, &r)),
                "<" | ">" | "<=" | ">=" => Val::Bool(relational(op, &l, &r)),
                other => return Err(format!("Unsupported operator \"{other}\"")),
            })
        }
    }
}

fn eval_expression(expr: &str, context: &Val) -> Result<Val, String> {
    let tokens = tokenize_expression(expr.trim())?;
    if tokens.is_empty() {
        return Ok(Val::Undef);
    }
    let node = parse_expression(tokens, expr)?;
    eval(&node, context)
}

// ---------------------------------------------------------------------------
// Filters
// ---------------------------------------------------------------------------

fn to_fixed(number: f64, decimals: i32) -> String {
    let decimals = decimals.max(0) as usize;
    format!("{number:.decimals$}")
}

fn month_names() -> &'static [&'static str; 12] {
    &[
        "January",
        "February",
        "March",
        "April",
        "May",
        "June",
        "July",
        "August",
        "September",
        "October",
        "November",
        "December",
    ]
}

fn format_date_value(value: &Val, format: &str) -> String {
    let Some(date) = to_local_datetime(value) else {
        // `Number.isNaN(date.getTime())` → return the raw value as a string.
        return match value {
            Val::Undef => String::new(),
            other => js_string(other),
        };
    };
    let hours24 = date.hour();
    let hours12 = ((hours24 + 11) % 12) + 1;
    let token_map = |token: &str| -> String {
        match token {
            "YYYY" => date.year().to_string(),
            "YY" => format!("{:02}", date.year().rem_euclid(100)),
            "MMMM" => month_names()[(date.month() - 1) as usize].to_string(),
            "MMM" => month_names()[(date.month() - 1) as usize][..3].to_string(),
            "MM" => format!("{:02}", date.month()),
            "M" => date.month().to_string(),
            "DD" => format!("{:02}", date.day()),
            "D" => date.day().to_string(),
            "HH" => format!("{hours24:02}"),
            "hh" => format!("{hours12:02}"),
            "h" => hours12.to_string(),
            "mm" => format!("{:02}", date.minute()),
            "ss" => format!("{:02}", date.second()),
            _ => (if hours24 < 12 { "AM" } else { "PM" }).to_string(),
        }
    };
    format_token_regex()
        .replace_all(format, |caps: &regex::Captures| token_map(&caps[0]))
        .into_owned()
}

fn format_token_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new("YYYY|YY|MMMM|MMM|MM|M|DD|D|HH|hh|h|mm|ss|A").expect("date token regex")
    })
}

fn currency_symbol(code: &str) -> Option<&'static str> {
    Some(match code {
        "USD" | "CAD" | "AUD" | "NZD" | "SGD" | "HKD" => "$",
        "EUR" => "€",
        "GBP" => "£",
        "JPY" => "¥",
        "CNY" => "¥",
        "INR" => "₹",
        "KRW" => "₩",
        "BRL" => "R$",
        "ZAR" => "R",
        "RUB" => "₽",
        "TRY" => "₺",
        "PLN" => "zł",
        "SEK" | "NOK" | "DKK" => "kr",
        "CHF" => "CHF",
        _ => return None,
    })
}

fn currency_decimals(code: &str) -> usize {
    match code {
        "JPY" | "KRW" | "VND" | "CLP" | "ISK" => 0,
        _ => 2,
    }
}

fn group_thousands(integer: &str) -> String {
    let digits: Vec<char> = integer.chars().collect();
    let mut out = String::new();
    for (index, ch) in digits.iter().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            out.push(',');
        }
        out.push(*ch);
    }
    out
}

fn format_currency(number: f64, code: &str) -> String {
    let decimals = currency_decimals(code);
    let negative = number < 0.0;
    let absolute = number.abs();
    let fixed = format!("{absolute:.decimals$}");
    let (integer, fraction) = match fixed.split_once('.') {
        Some((integer, fraction)) => (integer.to_string(), fraction.to_string()),
        None => (fixed, String::new()),
    };
    let grouped = group_thousands(&integer);
    let body = if decimals == 0 {
        grouped
    } else {
        format!("{grouped}.{fraction}")
    };
    let Some(symbol) = currency_symbol(code) else {
        return format!("{}{} {}", if negative { "-" } else { "" }, code, body);
    };
    // Intl places the sign before the symbol.
    format!("{}{symbol}{body}", if negative { "-" } else { "" })
}

fn apply_filter(name: &str, value: Val, args: &[Val], expr: &str) -> Result<Val, String> {
    Ok(match name {
        "upper" => Val::Str(js_string(&value).to_uppercase()),
        "lower" => Val::Str(js_string(&value).to_lowercase()),
        "capitalize" => {
            let text = js_string(&value);
            let mut chars = text.chars();
            match chars.next() {
                None => Val::Str(text),
                Some(first) => {
                    let head: String = first.to_uppercase().collect();
                    Val::Str(format!("{head}{}", chars.as_str()))
                }
            }
        }
        "trim" => Val::Str(js_string(&value).trim().to_string()),
        "currency" => {
            let code = args
                .first()
                .map(|arg| js_string(arg))
                .unwrap_or_else(|| "USD".to_string());
            let locale = args
                .get(1)
                .map(|arg| js_string(arg))
                .unwrap_or_else(|| "en-US".to_string());
            let number = js_number(&value);
            let number = if number.is_finite() && number != 0.0 {
                number
            } else {
                0.0
            };
            let code_upper = code.to_uppercase();
            if locale != "en-US"
                || code_upper.len() != 3
                || !code_upper.chars().all(|c| c.is_ascii_alphabetic())
            {
                Val::Str(format!("{code} {}", to_fixed(number, 2)))
            } else {
                Val::Str(format_currency(number, &code_upper))
            }
        }
        "number" => {
            let decimals = args.first().map(js_number).unwrap_or(2.0);
            let number = js_number(&value);
            if number.is_finite() {
                Val::Str(to_fixed(number, decimals as i32))
            } else {
                Val::Str(String::new())
            }
        }
        "round" => {
            let decimals = args.first().map(js_number).unwrap_or(0.0);
            let number = js_number(&value);
            if !number.is_finite() {
                Val::Str(String::new())
            } else {
                let factor = 10f64.powf(decimals);
                Val::Str(js_num_to_string((number * factor).round() / factor))
            }
        }
        "date" => {
            let format = args
                .first()
                .map(|arg| js_string(arg))
                .unwrap_or_else(|| "YYYY-MM-DD".to_string());
            Val::Str(format_date_value(&value, &format))
        }
        "default" => {
            let fallback = args
                .first()
                .cloned()
                .unwrap_or_else(|| Val::Str(String::new()));
            let empty_string = matches!(&value, Val::Str(text) if text.is_empty());
            if is_nullish(&value) || empty_string {
                fallback
            } else {
                value
            }
        }
        "json" => {
            if matches!(value, Val::Undef) {
                Val::Undef
            } else {
                Val::Str(to_json(&value).to_string())
            }
        }
        "safe" => Val::Safe(js_string(&value)),
        other => {
            return Err(format!(
                "Unknown template filter \"{other}\" in \"{{{{{expr}}}}}\""
            ));
        }
    })
}

// ---------------------------------------------------------------------------
// Date conversion
// ---------------------------------------------------------------------------

fn local_from_millis(millis: i64) -> Option<DateTime<Local>> {
    DateTime::<Utc>::from_timestamp_millis(millis).map(|dt| dt.with_timezone(&Local))
}

fn parse_date_string(text: &str) -> Option<DateTime<Local>> {
    if let Ok(date) = NaiveDate::parse_from_str(text, "%Y-%m-%d") {
        // JS parses a bare `YYYY-MM-DD` as UTC midnight.
        let naive = date.and_hms_opt(0, 0, 0)?;
        return Some(Utc.from_utc_datetime(&naive).with_timezone(&Local));
    }
    if let Ok(datetime) = DateTime::parse_from_rfc3339(text) {
        return Some(datetime.with_timezone(&Local));
    }
    for format in [
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%dT%H:%M:%S%.f",
    ] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(text, format) {
            return Local.from_local_datetime(&naive).single();
        }
    }
    // US `M/D/YYYY`, which JS treats as local midnight.
    for format in ["%m/%d/%Y", "%m/%d/%y"] {
        if let Ok(date) = NaiveDate::parse_from_str(text, format) {
            let naive = date.and_hms_opt(0, 0, 0)?;
            return Local.from_local_datetime(&naive).single();
        }
    }
    None
}

/// Convert a template value into a local `DateTime`, mirroring
/// `new Date(value)` for the shapes Novamail passes (epoch millis, ISO strings
/// and US slash dates). `None` means an invalid date.
fn to_local_datetime(value: &Val) -> Option<DateTime<Local>> {
    match value {
        Val::Num(number) => local_from_millis(*number as i64),
        Val::Str(text) => parse_date_string(text),
        Val::Safe(text) => parse_date_string(text),
        Val::Null => local_from_millis(0),
        Val::Bool(flag) => local_from_millis(if *flag { 1 } else { 0 }),
        Val::Undef => None,
        _ => None,
    }
}

/// `dd/mm/yyyy` for the generated `date` variable.
fn format_generated_date(value: &Val) -> String {
    match to_local_datetime(value) {
        Some(date) => format!("{:02}/{}/{}", date.day(), date.month(), date.year()),
        None => String::new(),
    }
}

fn now_generated_date() -> String {
    let now = Local::now();
    format!("{:02}/{}/{}", now.day(), now.month(), now.year())
}

// ---------------------------------------------------------------------------
// `usernameFromEmail`
// ---------------------------------------------------------------------------

const GIVEN_NAME_PREFIXES_RAW: &str = "
  aaliyah aamir aaron abbas abdul abdullah abdulrahman adil adnan ahmed aisha akash alif ali alina amir amna anika anisa anwar arif arman arafat asif ashik atik ayesha aziz
  benjamin bilal brad brian carlos chris christian daniel david deepak dipu emily fatima farhan fahim faisal frank gabriel george hasan hassan hossain ibrahim imran ismail jack james jane jason john jose joseph joy julia karim kevin khalid laura leo liam lily lucas mahmud maria mark mary matthew michael mizan mohammad mohammed monir nafis nancy nathan nicolas nikhil nora nusrat oliver omar osman patrick paul priya rachel rahim rakib reza robert saad sadaf saiful samuel sara sarah shafiq sohan steven tanvir tamim tasnim thomas tina waleed william yusuf zubair
";

const NON_NAME_PARTS_RAW: &str = "account accounts admin app bd blog business c cse contact co company com dev developer edu email facebook finance git gmail help hr info instagram job jobs linkedin mail marketing me official online personal photo photography private sales service site student support team tech technology university web work www x";

const PREFIX_PARTS_RAW: &str = "dr miss mr mrs ms prof";

fn given_name_prefixes() -> &'static Vec<String> {
    static PREFIXES: OnceLock<Vec<String>> = OnceLock::new();
    PREFIXES.get_or_init(|| {
        let mut names: Vec<String> = GIVEN_NAME_PREFIXES_RAW
            .split_whitespace()
            .map(str::to_string)
            .collect();
        names.sort_by(|left, right| right.len().cmp(&left.len()));
        names
    })
}

fn non_name_parts() -> &'static HashSet<String> {
    static PARTS: OnceLock<HashSet<String>> = OnceLock::new();
    PARTS.get_or_init(|| {
        NON_NAME_PARTS_RAW
            .split_whitespace()
            .map(str::to_string)
            .collect()
    })
}

fn prefix_parts() -> &'static HashSet<String> {
    static PARTS: OnceLock<HashSet<String>> = OnceLock::new();
    PARTS.get_or_init(|| {
        PREFIX_PARTS_RAW
            .split_whitespace()
            .map(str::to_string)
            .collect()
    })
}

fn regex_angle_address() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"<([^<>]+)>").expect("angle address regex"))
}

fn regex_lower_upper() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"([a-z])([A-Z])").expect("camel boundary regex"))
}

fn regex_upper_upper_lower() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"([A-Z])([A-Z][a-z])").expect("acronym boundary regex"))
}

fn regex_digits() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\d+").expect("digit regex"))
}

fn regex_non_letter() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"[^\p{L}]+").expect("non-letter regex"))
}

fn regex_number_token() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\$number(\d+)\b").expect("number token regex"))
}

/// Best-effort replacement for `normalize('NFKD').replace(/\p{M}/gu, '')`.
///
/// Without a Unicode normalisation crate we fold the common Latin-1 and
/// Latin-Extended code points to their base letter, which is what NFKD +
/// mark-stripping produces for them.
fn strip_diacritics(input: &str) -> String {
    input
        .chars()
        .map(|c| match c {
            'À' | 'Á' | 'Â' | 'Ã' | 'Ä' | 'Å' | 'Ā' | 'Ă' | 'Ą' => 'A',
            'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' | 'ă' | 'ą' => 'a',
            'Æ' => 'A',
            'æ' => 'a',
            'Ç' | 'Ć' | 'Ĉ' | 'Ċ' | 'Č' => 'C',
            'ç' | 'ć' | 'ĉ' | 'ċ' | 'č' => 'c',
            'Ď' | 'Đ' => 'D',
            'ď' | 'đ' => 'd',
            'È' | 'É' | 'Ê' | 'Ë' | 'Ē' | 'Ĕ' | 'Ė' | 'Ę' | 'Ě' => 'E',
            'è' | 'é' | 'ê' | 'ë' | 'ē' | 'ĕ' | 'ė' | 'ę' | 'ě' => 'e',
            'Ĝ' | 'Ğ' | 'Ġ' | 'Ģ' => 'G',
            'ĝ' | 'ğ' | 'ġ' | 'ģ' => 'g',
            'Ĥ' | 'Ħ' => 'H',
            'ĥ' | 'ħ' => 'h',
            'Ì' | 'Í' | 'Î' | 'Ï' | 'Ĩ' | 'Ī' | 'Ĭ' | 'Į' | 'İ' => 'I',
            'ì' | 'í' | 'î' | 'ï' | 'ĩ' | 'ī' | 'ĭ' | 'į' | 'ı' => 'i',
            'Ĵ' => 'J',
            'ĵ' => 'j',
            'Ķ' => 'K',
            'ķ' => 'k',
            'Ĺ' | 'Ļ' | 'Ľ' | 'Ŀ' | 'Ł' => 'L',
            'ĺ' | 'ļ' | 'ľ' | 'ŀ' | 'ł' => 'l',
            'Ñ' | 'Ń' | 'Ņ' | 'Ň' => 'N',
            'ñ' | 'ń' | 'ņ' | 'ň' => 'n',
            'Ò' | 'Ó' | 'Ô' | 'Õ' | 'Ö' | 'Ø' | 'Ō' | 'Ŏ' | 'Ő' => 'O',
            'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'ō' | 'ŏ' | 'ő' => 'o',
            'Œ' => 'O',
            'œ' => 'o',
            'Ŕ' | 'Ŗ' | 'Ř' => 'R',
            'ŕ' | 'ŗ' | 'ř' => 'r',
            'Ś' | 'Ŝ' | 'Ş' | 'Š' => 'S',
            'ś' | 'ŝ' | 'ş' | 'š' => 's',
            'Ţ' | 'Ť' | 'Ŧ' => 'T',
            'ţ' | 'ť' | 'ŧ' => 't',
            'Ù' | 'Ú' | 'Û' | 'Ü' | 'Ũ' | 'Ū' | 'Ŭ' | 'Ů' | 'Ű' | 'Ų' => 'U',
            'ù' | 'ú' | 'û' | 'ü' | 'ũ' | 'ū' | 'ŭ' | 'ů' | 'ű' | 'ų' => 'u',
            'Ŵ' => 'W',
            'ŵ' => 'w',
            'Ý' | 'Ŷ' | 'Ÿ' => 'Y',
            'ý' | 'ÿ' | 'ŷ' => 'y',
            'Ź' | 'Ż' | 'Ž' => 'Z',
            'ź' | 'ż' | 'ž' => 'z',
            other => other,
        })
        .collect()
}

fn title_case_name_part(word: &str) -> String {
    let lower = word.to_lowercase();
    let mut chars = lower.chars();
    match chars.next() {
        None => lower,
        Some(first) => {
            let head: String = first.to_uppercase().collect();
            format!("{head}{}", chars.as_str())
        }
    }
}

fn split_joined_name(word: &str) -> Vec<String> {
    let prefixes = given_name_prefixes();
    if prefixes.iter().any(|name| name == word) {
        return vec![word.to_string()];
    }
    let found = prefixes
        .iter()
        .find(|name| word.starts_with(name.as_str()) && word.len() - name.len() >= 3);
    match found {
        Some(prefix) => vec![prefix.clone(), word[prefix.len()..].to_string()],
        None => vec![word.to_string()],
    }
}

fn username_from_email(email: &str) -> String {
    let mut address = email.trim().to_string();
    if let Some(caps) = regex_angle_address().captures(&address) {
        address = caps[1].trim().to_string();
    }
    let at_index = address.rfind('@');
    let local = match at_index {
        Some(index) => &address[..index],
        None => address.as_str(),
    };
    let local_part = local.split('+').next().unwrap_or("");
    if local_part.is_empty() {
        return String::new();
    }

    let step1 = regex_lower_upper().replace_all(local_part, "$1 $2");
    let step2 = regex_upper_upper_lower().replace_all(&step1, "$1 $2");
    let step3 = strip_diacritics(&step2);
    let step4 = regex_digits().replace_all(&step3, " ");
    let mut separated: Vec<String> = regex_non_letter()
        .split(&step4)
        .map(|part| part.to_lowercase())
        .filter(|part| !part.is_empty())
        .collect();

    let prefix_parts = prefix_parts();
    while let Some(first) = separated.first() {
        if prefix_parts.contains(first) {
            separated.remove(0);
        } else {
            break;
        }
    }
    let non_name_parts = non_name_parts();
    while let Some(last) = separated.last() {
        if non_name_parts.contains(last) {
            separated.pop();
        } else {
            break;
        }
    }

    separated
        .into_iter()
        .filter(|part| !non_name_parts.contains(part))
        .flat_map(|part| split_joined_name(&part))
        .filter(|part| !part.is_empty())
        .take(3)
        .map(|part| title_case_name_part(&part))
        .collect::<Vec<_>>()
        .join(" ")
}

// ---------------------------------------------------------------------------
// Generated identifiers
// ---------------------------------------------------------------------------

fn rand_unit() -> f64 {
    let bytes = uuid::Uuid::new_v4().into_bytes();
    let raw = u64::from_le_bytes(bytes[0..8].try_into().expect("8 bytes"));
    // 53 random bits → [0, 1), mirroring Math.random().
    (raw >> 11) as f64 / (1u64 << 53) as f64
}

fn digits(length: u32) -> String {
    let min = 10f64.powi(length as i32 - 1);
    let value = min + (rand_unit() * 9.0 * min).floor();
    js_num_to_string(value)
}

// ---------------------------------------------------------------------------
// Template tokenizer + tree
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
enum TemplateToken {
    Text(String),
    Var { expr: String, escape: bool },
    BlockOpen { block: String, args: String },
    Else,
    BlockClose { block: String },
}

#[derive(Clone, Debug)]
enum TplNode {
    Text(String),
    Var {
        expr: String,
        escape: bool,
    },
    Block {
        block: String,
        args: String,
        children: Vec<TplNode>,
        else_children: Option<Vec<TplNode>>,
    },
}

fn master_tag_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(concat!(
            r"\{\{\{(?P<raw>[\s\S]+?)\}\}\}",
            r"|\{\{#(?P<blockType>each|if)\s+(?P<blockArgs>[\s\S]+?)\}\}",
            r"|\{\{else\}\}",
            r"|\{\{/(?P<closeType>each|if)\}\}",
            r"|\{\{(?P<expr>[\s\S]+?)\}\}",
            r"|\$\{(?P<dollarExpr>[\s\S]+?)\}",
            r"|\$(?P<dollarPath>[A-Za-z_][A-Za-z0-9_]*(?:\.[A-Za-z_][A-Za-z0-9_]*|\[\d+\])*)",
        ))
        .expect("master tag regex")
    })
}

fn tokenize_template(template: &str) -> Vec<TemplateToken> {
    let mut tokens = Vec::new();
    let mut last_index = 0;
    for caps in master_tag_regex().captures_iter(template) {
        let whole = caps.get(0).expect("whole match");
        if whole.start() > last_index {
            tokens.push(TemplateToken::Text(
                template[last_index..whole.start()].to_string(),
            ));
        }
        if let Some(raw) = caps.name("raw") {
            tokens.push(TemplateToken::Var {
                expr: raw.as_str().to_string(),
                escape: false,
            });
        } else if let Some(block_type) = caps.name("blockType") {
            tokens.push(TemplateToken::BlockOpen {
                block: block_type.as_str().to_string(),
                args: caps
                    .name("blockArgs")
                    .map(|m| m.as_str().trim().to_string())
                    .unwrap_or_default(),
            });
        } else if whole.as_str() == "{{else}}" {
            tokens.push(TemplateToken::Else);
        } else if let Some(close_type) = caps.name("closeType") {
            tokens.push(TemplateToken::BlockClose {
                block: close_type.as_str().to_string(),
            });
        } else if let Some(expr) = caps.name("expr") {
            tokens.push(TemplateToken::Var {
                expr: expr.as_str().to_string(),
                escape: true,
            });
        } else if let Some(expr) = caps.name("dollarExpr") {
            tokens.push(TemplateToken::Var {
                expr: expr.as_str().to_string(),
                escape: true,
            });
        } else if let Some(path) = caps.name("dollarPath") {
            tokens.push(TemplateToken::Var {
                expr: path.as_str().to_string(),
                escape: true,
            });
        }
        last_index = whole.end();
    }
    if last_index < template.len() {
        tokens.push(TemplateToken::Text(template[last_index..].to_string()));
    }
    tokens
}

struct Frame {
    block: Option<String>,
    args: String,
    children: Vec<TplNode>,
    else_children: Option<Vec<TplNode>>,
    in_else: bool,
}

impl Frame {
    fn root() -> Self {
        Self {
            block: None,
            args: String::new(),
            children: Vec::new(),
            else_children: None,
            in_else: false,
        }
    }

    fn branch(&mut self) -> &mut Vec<TplNode> {
        if self.in_else {
            self.else_children.get_or_insert_with(Vec::new).as_mut()
        } else {
            &mut self.children
        }
    }
}

fn build_tree(tokens: Vec<TemplateToken>) -> Result<Vec<TplNode>, String> {
    let mut stack = vec![Frame::root()];
    for token in tokens {
        match token {
            TemplateToken::Text(value) => stack
                .last_mut()
                .unwrap()
                .branch()
                .push(TplNode::Text(value)),
            TemplateToken::Var { expr, escape } => stack
                .last_mut()
                .unwrap()
                .branch()
                .push(TplNode::Var { expr, escape }),
            TemplateToken::BlockOpen { block, args } => {
                stack.push(Frame {
                    block: Some(block),
                    args,
                    children: Vec::new(),
                    else_children: None,
                    in_else: false,
                });
            }
            TemplateToken::Else => {
                let top = stack.last_mut().unwrap();
                if top.block.as_deref() != Some("if") {
                    return Err("{{else}} found outside an {{#if}} block".to_string());
                }
                top.else_children = Some(Vec::new());
                top.in_else = true;
            }
            TemplateToken::BlockClose { block } => {
                let top = stack.pop().unwrap();
                if top.block.as_deref() != Some(block.as_str()) {
                    return Err(format!(
                        "Mismatched {{/{block}}} — check your template's {{#each}}/{{#if}} tags are balanced"
                    ));
                }
                let node = TplNode::Block {
                    block: block.clone(),
                    args: top.args,
                    children: top.children,
                    else_children: top.else_children,
                };
                stack.last_mut().unwrap().branch().push(node);
            }
        }
    }
    if stack.len() != 1 {
        return Err(
            "Unclosed {{#each}} or {{#if}} block — every {{#each}}/{{#if}} needs a matching {{/each}}/{{/if}}"
                .to_string(),
        );
    }
    Ok(stack.pop().unwrap().children)
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

fn split_top_level(input: &str, separator: char) -> Vec<String> {
    let chars: Vec<char> = input.chars().collect();
    let mut parts: Vec<String> = Vec::new();
    let mut depth: i32 = 0;
    let mut current = String::new();
    let mut in_string: Option<char> = None;
    for index in 0..chars.len() {
        let c = chars[index];
        if let Some(quote) = in_string {
            current.push(c);
            if c == quote && (index == 0 || chars[index - 1] != '\\') {
                in_string = None;
            }
            continue;
        }
        if c == '"' || c == '\'' {
            in_string = Some(c);
            current.push(c);
            continue;
        }
        if c == '(' || c == '[' {
            depth += 1;
        }
        if c == ')' || c == ']' {
            depth -= 1;
        }
        if c == separator && depth == 0 {
            parts.push(current.clone());
            current.clear();
            continue;
        }
        current.push(c);
    }
    parts.push(current);
    let mut trimmed: Vec<String> = parts.iter().map(|part| part.trim().to_string()).collect();
    if trimmed.len() > 1 && trimmed.last().map(|s| s.is_empty()).unwrap_or(false) {
        trimmed.pop();
    }
    trimmed
}

fn render_var(expr: &str, escape: bool, context: &Val) -> Result<String, String> {
    let parts = split_top_level(expr, '|');
    let raw_expr = parts.first().cloned().unwrap_or_default();
    let mut value = eval_expression(&raw_expr, context)
        .map_err(|error| format!("Template variable error in \"{{{{{expr}}}}}\": {error}"))?;
    for part in parts.iter().skip(1) {
        let segments = split_top_level(part, ':');
        let name = segments.first().map(|s| s.trim()).unwrap_or("");
        let mut args = Vec::new();
        for arg in segments.iter().skip(1) {
            args.push(eval_expression(arg, context)?);
        }
        value = apply_filter(name, value, &args, expr)?;
    }
    if let Val::Safe(text) = &value {
        return Ok(text.clone());
    }
    if is_nullish(&value) {
        return Ok(String::new());
    }
    let text = js_string(&value);
    Ok(if escape { escape_html(&text) } else { text })
}

fn type_of(value: &Val) -> &'static str {
    match value {
        Val::Undef => "undefined",
        Val::Null => "object",
        Val::Bool(_) => "boolean",
        Val::Num(_) => "number",
        Val::Str(_) => "string",
        Val::Arr(_) | Val::Obj(_) | Val::Safe(_) => "object",
    }
}

fn each_args_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^([\s\S]+?)(?:\s+as\s+([A-Za-z_][A-Za-z0-9_]*))?$").expect("each args regex")
    })
}

fn render_block(
    block: &str,
    args: &str,
    children: &[TplNode],
    else_children: &Option<Vec<TplNode>>,
    context: &Val,
) -> Result<String, String> {
    match block {
        "if" => {
            let condition = eval_expression(args, context).map_err(|error| {
                format!("Template condition error in \"{{{{#if {args}}}}}\": {error}")
            })?;
            if truthy(&condition) {
                render_nodes(children, context)
            } else if let Some(otherwise) = else_children {
                render_nodes(otherwise, context)
            } else {
                Ok(String::new())
            }
        }
        "each" => {
            let caps = each_args_regex().captures(args).ok_or_else(|| {
                format!(
                    "Invalid {{#each {args}}} syntax — use {{#each path}} or {{#each path as item}}"
                )
            })?;
            let path_expr = caps[1].to_string();
            let alias = caps.get(2).map(|m| m.as_str().to_string());
            let mut list = eval_expression(path_expr.trim(), context).map_err(|error| {
                format!("Template loop error in \"{{{{#each {args}}}}}\": {error}")
            })?;
            if is_nullish(&list) {
                list = Val::Arr(Vec::new());
            }
            let Val::Arr(items) = list else {
                return Err(format!(
                    "{{{{#each {args}}}}} expects an array, got {}",
                    type_of(&list)
                ));
            };
            let mut out = String::new();
            let last_index = items.len().saturating_sub(1);
            for (index, item) in items.into_iter().enumerate() {
                let mut scope = match context {
                    Val::Obj(map) => map.clone(),
                    _ => BTreeMap::new(),
                };
                if let Some(ref alias_name) = alias {
                    scope.insert(alias_name.clone(), item);
                } else if matches!(item, Val::Obj(_)) {
                    if let Val::Obj(fields) = item {
                        for (key, value) in fields {
                            scope.insert(key, value);
                        }
                    }
                } else {
                    scope.insert("this".to_string(), item);
                }
                scope.insert("@index".to_string(), Val::Num(index as f64));
                scope.insert("@first".to_string(), Val::Bool(index == 0));
                scope.insert("@last".to_string(), Val::Bool(index == last_index));
                out.push_str(&render_nodes(children, &Val::Obj(scope))?);
            }
            Ok(out)
        }
        other => Err(format!("Unknown block type \"{other}\"")),
    }
}

fn render_nodes(nodes: &[TplNode], context: &Val) -> Result<String, String> {
    let mut out = String::new();
    for node in nodes {
        match node {
            TplNode::Text(value) => out.push_str(value),
            TplNode::Var { expr, escape } => out.push_str(&render_var(expr, *escape, context)?),
            TplNode::Block {
                block,
                args,
                children,
                else_children,
            } => out.push_str(&render_block(
                block,
                args,
                children,
                else_children,
                context,
            )?),
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Render a template string against a data object.
///
/// Mirrors `parseTemplate(template, data)` from `desktop-runtime/templateEngine.cjs`.
pub fn parse_template(template: &str, data: &Value) -> Result<String, String> {
    let data_object = match data {
        Value::Null => None,
        Value::Object(map) => Some(map),
        _ => return Err("parseTemplate: data must be an object".to_string()),
    };

    let tokens = tokenize_template(template);
    let tree = build_tree(tokens)?;

    let mut context: BTreeMap<String, Val> = BTreeMap::new();
    if let Some(map) = data_object {
        for (key, value) in map {
            context.insert(key.clone(), from_json(value));
        }
    }

    let recipient_email = match context.get("email") {
        Some(Val::Str(text)) => text.clone(),
        _ => String::new(),
    };
    let sender_email = match context.get("sender_email") {
        Some(Val::Str(text)) => text.clone(),
        _ => match context.get("senderEmail") {
            Some(Val::Str(text)) => text.clone(),
            _ => String::new(),
        },
    };

    let username_missing = context
        .get("username")
        .map(|value| is_nullish(value) || js_string(value).trim().is_empty())
        .unwrap_or(true);
    if username_missing {
        context.insert(
            "username".to_string(),
            Val::Str(username_from_email(&recipient_email)),
        );
    }
    let sender_username_missing = context
        .get("sender_username")
        .map(|value| is_nullish(value) || js_string(value).trim().is_empty())
        .unwrap_or(true);
    if sender_username_missing {
        context.insert(
            "sender_username".to_string(),
            Val::Str(username_from_email(&sender_email)),
        );
    }

    context.insert(
        "eaddress".to_string(),
        Val::Str(if recipient_email.is_empty() {
            String::new()
        } else {
            format!("@{}", recipient_email.split('@').next().unwrap_or(""))
        }),
    );

    let date = match context.get("date") {
        None => now_generated_date(),
        Some(value) => format_generated_date(value),
    };
    context.insert("date".to_string(), Val::Str(date));

    if !context.contains_key("digi_no") {
        context.insert("digi_no".to_string(), Val::Str(digits(10)));
    }
    if !context.contains_key("invoice_no") {
        context.insert(
            "invoice_no".to_string(),
            Val::Str(format!("INV-{}", digits(8))),
        );
    }
    let order_id_missing = context
        .get("order_id")
        .map(|value| is_nullish(value) || js_string(value).is_empty())
        .unwrap_or(true);
    if order_id_missing {
        context.insert("order_id".to_string(), Val::Str(digits(8)));
    }
    let transaction_missing = context
        .get("transaction_id")
        .map(|value| is_nullish(value) || js_string(value).is_empty())
        .unwrap_or(true);
    if transaction_missing {
        context.insert("transaction_id".to_string(), Val::Str(digits(12)));
    }
    let customer_missing = context
        .get("customer_id")
        .map(|value| is_nullish(value) || js_string(value).is_empty())
        .unwrap_or(true);
    if customer_missing {
        context.insert("customer_id".to_string(), Val::Str(digits(10)));
    }

    for caps in regex_number_token().captures_iter(template) {
        let length: u32 = caps[1].parse().unwrap_or(6);
        let key = format!("number{length}");
        if !context.contains_key(&key) {
            context.insert(key, Val::Str(digits(length)));
        }
    }
    if !context.contains_key("number6") {
        context.insert("number6".to_string(), Val::Str(digits(6)));
    }

    render_nodes(&tree, &Val::Obj(context))
}

// ---------------------------------------------------------------------------
// Responsive email container
// ---------------------------------------------------------------------------

/// A container narrower than this is detail design (icon cells, spacer
/// columns); the main email container is wider, so only it goes fluid.
const RESPONSIVE_CONTAINER_MIN_PX: u32 = 480;

/// The fluid container cap for the sent email. `width:100%` fills a Gmail
/// reading pane (~700-800 px) or an Office page box (~756 px) naturally and
/// the cap keeps every screen wider than that looking like one centered
/// column; margin/align rules are left alone so existing centering stays.
const RESPONSIVE_CONTAINER_MAX_PX: &str = "1000px";

fn responsive_tag_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(?is)<(table|div)\b[^<>]*>"#).expect("tag regex is valid")
    })
}

fn responsive_width_attr_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // NOTE: the `regex` crate has no backreferences, so the quoted form is
        // matched with an alternation — `(["']?)(\d+)\1` panics at regex
        // construction ("backreferences are not supported").
        Regex::new(r#"(?i)\bwidth\s*=\s*(?:"(\d{1,4})"|'(\d{1,4})')"#)
            .expect("width attr regex is valid")
    })
}

fn responsive_css_width_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)width\s*:\s*(\d{1,4})(px)").expect("css width regex is valid")
    })
}

fn responsive_style_attr_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(?i)style\s*=\s*["']"#).expect("style attr regex is valid")
    })
}

fn responsive_style_block_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?is)<style\b[^>]*>.*?</style>").expect("style block regex is valid")
    })
}

/// Rewrites `width:Npx` declarations at least [`RESPONSIVE_CONTAINER_MIN_PX`]
/// wide into the fluid pair. A preceding `-` (min-/max-width) keeps the
/// declaration untouched.
fn widen_css_widths(source: &str) -> String {
    let regexes = responsive_css_width_regex();
    regexes
        .replace_all(source, |caps: &Captures| {
            let bounds = caps.get(0).map_or(0, |m| m.start());
            let preceded_by_minus = bounds > 0
                && source
                    .as_bytes()
                    .get(bounds - 1)
                    .is_some_and(|byte| *byte == b'-');
            let number: u32 = caps[1].parse().unwrap_or(0);
            if !preceded_by_minus && number >= RESPONSIVE_CONTAINER_MIN_PX {
                format!("width:100%;max-width:{}", RESPONSIVE_CONTAINER_MAX_PX)
            } else {
                caps[0].to_string()
            }
        })
        .into_owned()
}

/// Converts the main fixed-width containers of a rendered template into the
/// responsive `width:100%; max-width:1000px` pattern that renders fluidly in
/// the document engine and before email style inlining: `<table width="600">`-
/// style attributes, inline `style="...width:600px..."` declarations on tables
/// and divs, and class widths inside `<style>` blocks. Centering, spacing,
/// typography and element sizes below the container threshold stay untouched.
pub fn responsive_container_html(html: &str) -> String {
    if !html.to_ascii_lowercase().contains("width") {
        return html.to_string();
    }

    let tags = responsive_tag_regex();
    let width_attrs = responsive_width_attr_regex();
    let style_attrs = responsive_style_attr_regex();
    let max_ref = RESPONSIVE_CONTAINER_MAX_PX;

    let widened_tags = tags.replace_all(html, |caps: &Captures| {
        let markup = caps[0].to_string();
        // Plain width="600" attributes on the container element itself.
        let markup = width_attrs
            .replace_all(&markup, |m: &Captures| {
                let number: u32 = m
                    .get(1)
                    .or_else(|| m.get(2))
                    .map(|value| value.as_str().parse().unwrap_or(0))
                    .unwrap_or(0);
                if number >= RESPONSIVE_CONTAINER_MIN_PX {
                    "width=\"100%\"".to_string()
                } else {
                    m[0].to_string()
                }
            })
            .into_owned();
        // Fixed widths inside the element's inline style.
        let markup = widen_css_widths(&markup);
        // Make the fluid container capped so wide screens center it.
        if markup.to_ascii_lowercase().contains("max-width") {
            markup
        } else if let Some(attributed) = style_attrs.find(&markup) {
            // Insert after the opening quote of the style attribute.
            let insert_at = attributed.end();
            format!(
                "{}max-width:{};{}",
                &markup[..insert_at],
                max_ref,
                &markup[insert_at..]
            )
        } else if markup.ends_with('>') {
            format!(
                "{} style=\"max-width:{}\">",
                &markup[..markup.len() - 1],
                max_ref
            )
        } else {
            markup
        }
    });

    let style_blocks = responsive_style_block_regex();
    let widened_styles = style_blocks.replace_all(&widened_tags, |caps: &Captures| {
        widen_css_widths(&caps[0])
    });

    widened_styles.into_owned()
}

/// Re-fluidifies the container widths that the computed-style inlining step
/// baked as absolute pixels. A `width:100%` container renders at the layer it
/// sits in (reading pane, page box); when the browser computes it during
/// inlining, the *window's* pixel width lands in the inline style, which
/// would re-introduce a too-wide fixed container on the recipient's screen.
/// Any inlined `width` of at least [`RESPONSIVE_CONTAINER_MIN_PX`] becomes
/// `width:100%` again; `max-width` (inlined as 1000 px) keeps the cap.
pub fn clamp_fluid_inline_widths(html: &str) -> String {
    let regexes = responsive_css_width_regex();
    regexes
        .replace_all(html, |caps: &Captures| {
            let bounds = caps.get(0).map_or(0, |m| m.start());
            let preceded_by_minus = bounds > 0
                && html
                    .as_bytes()
                    .get(bounds - 1)
                    .is_some_and(|byte| *byte == b'-');
            let number: u32 = caps[1].parse().unwrap_or(0);
            if !preceded_by_minus && number >= RESPONSIVE_CONTAINER_MIN_PX {
                "width:100%".to_string()
            } else {
                caps[0].to_string()
            }
        })
        .into_owned()
}

// ---------------------------------------------------------------------------
// Self-test
// ---------------------------------------------------------------------------

/// Headless parity checks, driven by `NOVAMAIL_SELFTEST_MODE=template`.
pub fn selftest() -> Value {
    let mut report = Map::new();
    let mut passed = true;

    let check = |name: &str,
                 template: &str,
                 data: Value,
                 expected: &str,
                 report: &mut Map<String, Value>,
                 passed: &mut bool| {
        match parse_template(template, &data) {
            Ok(output) => {
                let ok = output == expected;
                *passed &= ok;
                report.insert(
                    name.to_string(),
                    json!({ "ok": ok, "got": output, "expected": expected }),
                );
            }
            Err(error) => {
                *passed = false;
                report.insert(name.to_string(), json!({ "ok": false, "error": error }));
            }
        }
    };

    // Plain interpolation, escaping and raw braces.
    check(
        "interpolation",
        "<p>{{name}} ($order.id)</p>",
        json!({ "name": "Ada & Sons", "order": { "id": 42 } }),
        // `$order.id` is a tag, so the `$` is consumed, leaving the literal parens.
        "<p>Ada &amp; Sons (42)</p>",
        &mut report,
        &mut passed,
    );
    check(
        "raw",
        "{{{note}}}|{{note}}",
        json!({ "note": "<b>hi</b>" }),
        "<b>hi</b>|&lt;b&gt;hi&lt;/b&gt;",
        &mut report,
        &mut passed,
    );
    check(
        "expression",
        "{{(qty * price) + 5}}",
        json!({ "qty": 3, "price": 4 }),
        "17",
        &mut report,
        &mut passed,
    );
    check(
        "dollar_expr",
        "${a + b}",
        json!({ "a": "x", "b": 2 }),
        "x2",
        &mut report,
        &mut passed,
    );

    // Comparison / logic.
    check(
        "if_else",
        "{{#if total > 100}}big{{else}}small{{/if}}",
        json!({ "total": 150 }),
        "big",
        &mut report,
        &mut passed,
    );
    check(
        "if_else_neg",
        "{{#if total > 100}}big{{else}}small{{/if}}",
        json!({ "total": 50 }),
        "small",
        &mut report,
        &mut passed,
    );
    check(
        "loose_eq",
        "{{#if a == 5}}yes{{/if}}",
        json!({ "a": "5" }),
        "yes",
        &mut report,
        &mut passed,
    );
    check(
        "logical_or",
        "{{#if name || fallback}}v{{/if}}",
        json!({ "fallback": "fb" }),
        "v",
        &mut report,
        &mut passed,
    );

    // Loops with item fields, @index and scope fall-through.
    check(
        "each_fields",
        "{{#each items as it}}[{{@index}}:{{it.name}}/{{@first}}/{{@last}}]{{/each}}",
        json!({ "items": [{ "name": "a" }, { "name": "b" }] }),
        "[0:a/true/false][1:b/false/true]",
        &mut report,
        &mut passed,
    );
    check(
        "each_implicit",
        "{{#each items}}{{name}}:{{order}};{{/each}}",
        json!({ "items": [{ "name": "a" }, { "name": "b" }], "order": "Z" }),
        "a:Z;b:Z;",
        &mut report,
        &mut passed,
    );
    check(
        "computed_index",
        "{{items[1]}}",
        json!({ "items": ["x", "y"] }),
        "y",
        &mut report,
        &mut passed,
    );

    // Filters.
    check(
        "filter_upper",
        "{{name | upper}}",
        json!({ "name": "ada" }),
        "ADA",
        &mut report,
        &mut passed,
    );
    check(
        "filter_currency",
        "{{amount | currency:'USD'}}",
        json!({ "amount": 1234.5 }),
        "$1,234.50",
        &mut report,
        &mut passed,
    );
    check(
        "filter_number",
        "{{amount | number:2}}",
        json!({ "amount": 3.14159 }),
        "3.14",
        &mut report,
        &mut passed,
    );
    check(
        "filter_default",
        "{{missing | default:'n/a'}}",
        json!({}),
        "n/a",
        &mut report,
        &mut passed,
    );
    check(
        "filter_safe",
        "{{html | safe}}",
        json!({ "html": "<i>x</i>" }),
        "<i>x</i>",
        &mut report,
        &mut passed,
    );
    check(
        "filter_json",
        "{{obj | json}}",
        json!({ "obj": { "a": 1 } }),
        // `| json` yields a plain string, so `{{...}}` HTML-escapes the quotes.
        "{&quot;a&quot;:1}",
        &mut report,
        &mut passed,
    );

    // Generated variable defaults (username derivation is deterministic).
    check(
        "username_derivation",
        "{{username}} @{{eaddress}}",
        json!({ "email": "john.doe@example.com" }),
        // `eaddress` already carries a leading `@`.
        "John Doe @@john.doe",
        &mut report,
        &mut passed,
    );

    // Structural errors surface with desktop runtime's wording.
    match parse_template("{{#if a}}x", &json!({ "a": true })) {
        Err(error) => {
            let ok = error.contains("Unclosed {{#each}} or {{#if}} block");
            passed &= ok;
            report.insert(
                "unclosed_block".to_string(),
                json!({ "ok": ok, "error": error }),
            );
        }
        Ok(output) => {
            passed = false;
            report.insert(
                "unclosed_block".to_string(),
                json!({ "ok": false, "got": output }),
            );
        }
    }

    report.insert("passed".to_string(), json!(passed));
    Value::Object(report)
}
