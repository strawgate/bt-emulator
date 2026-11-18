//! Text BTQL in its SQL form, as sent by the Ruby SDK:
//! `SELECT * FROM <type>('<id>') [WHERE <expr>] [LIMIT <n>]`.
//! The WHERE clause is translated into the same filter tree the structured
//! form uses, so evaluation is shared (see `btql::matches`).

use serde_json::{Value, json};

use crate::btql::Query;

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Word(String),
    Str(String),
    Num(Value),
    Sym(&'static str),
}

fn tokenize(src: &str) -> Result<Vec<Tok>, String> {
    let chars: Vec<char> = src.chars().collect();
    let (mut out, mut i) = (Vec::new(), 0);
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
        } else if c == '\'' {
            let mut s = String::new();
            i += 1;
            loop {
                match chars.get(i) {
                    None => return Err("unterminated string literal".into()),
                    Some('\'') if chars.get(i + 1) == Some(&'\'') => {
                        s.push('\'');
                        i += 2;
                    }
                    Some('\'') => break,
                    Some(&ch) => {
                        s.push(ch);
                        i += 1;
                    }
                }
            }
            i += 1;
            out.push(Tok::Str(s));
        } else if c.is_ascii_digit()
            || (c == '-' && chars.get(i + 1).is_some_and(char::is_ascii_digit))
        {
            let start = i;
            i += 1;
            while chars
                .get(i)
                .is_some_and(|c| c.is_ascii_digit() || *c == '.')
            {
                i += 1;
            }
            let text: String = chars[start..i].iter().collect();
            let num: Value =
                serde_json::from_str(&text).map_err(|_| format!("invalid number {text}"))?;
            out.push(Tok::Num(num));
        } else if c.is_alphabetic() || c == '_' {
            let start = i;
            while chars
                .get(i)
                .is_some_and(|c| c.is_alphanumeric() || *c == '_')
            {
                i += 1;
            }
            out.push(Tok::Word(chars[start..i].iter().collect()));
        } else {
            let two: String = chars[i..(i + 2).min(chars.len())].iter().collect();
            let sym = ["!=", "<>", "<=", ">="].into_iter().find(|s| *s == two);
            let sym = sym.or_else(|| {
                ["=", "<", ">", "(", ")", ",", ".", "*"]
                    .into_iter()
                    .find(|s| s.starts_with(c))
            });
            let sym = sym.ok_or_else(|| format!("unexpected character {c:?}"))?;
            i += sym.len();
            out.push(Tok::Sym(sym));
        }
    }
    Ok(out)
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
        self.pos += 1;
        t
    }

    fn keyword(&mut self, kw: &str) -> bool {
        let hit = matches!(self.peek(), Some(Tok::Word(w)) if w.eq_ignore_ascii_case(kw));
        self.pos += usize::from(hit);
        hit
    }

    fn expect_keyword(&mut self, kw: &str) -> Result<(), String> {
        self.keyword(kw)
            .then_some(())
            .ok_or_else(|| format!("expected {kw}"))
    }

    fn symbol(&mut self, sym: &str) -> bool {
        let hit = self.peek() == Some(&Tok::Sym(sym_static(sym)));
        self.pos += usize::from(hit);
        hit
    }

    fn expect_symbol(&mut self, sym: &str) -> Result<(), String> {
        self.symbol(sym)
            .then_some(())
            .ok_or_else(|| format!("expected `{sym}`"))
    }

    fn or(&mut self) -> Result<Value, String> {
        let mut children = vec![self.and()?];
        while self.keyword("or") {
            children.push(self.and()?);
        }
        Ok(if children.len() == 1 {
            children.remove(0)
        } else {
            json!({"op": "or", "children": children})
        })
    }

    fn and(&mut self) -> Result<Value, String> {
        let mut children = vec![self.not()?];
        while self.keyword("and") {
            children.push(self.not()?);
        }
        Ok(if children.len() == 1 {
            children.remove(0)
        } else {
            json!({"op": "and", "children": children})
        })
    }

    fn not(&mut self) -> Result<Value, String> {
        if self.keyword("not") {
            return Ok(json!({"op": "not", "expr": self.not()?}));
        }
        self.comparison()
    }

    fn comparison(&mut self) -> Result<Value, String> {
        let left = self.primary()?;
        if self.keyword("is") {
            let negated = self.keyword("not");
            self.expect_keyword("null")?;
            return Ok(json!({"op": if negated { "isnotnull" } else { "isnull" }, "expr": left}));
        }
        let negated = self.keyword("not");
        if self.keyword("in") {
            self.expect_symbol("(")?;
            let mut values = vec![self.literal()?];
            while self.symbol(",") {
                values.push(self.literal()?);
            }
            self.expect_symbol(")")?;
            let expr =
                json!({"op": "in", "left": left, "right": {"op": "literal", "value": values}});
            return Ok(if negated {
                json!({"op": "not", "expr": expr})
            } else {
                expr
            });
        }
        if negated {
            return Err("expected IN after NOT".into());
        }
        let op = match self.peek() {
            Some(Tok::Sym("=")) => "eq",
            Some(Tok::Sym("!=" | "<>")) => "ne",
            Some(Tok::Sym("<")) => "lt",
            Some(Tok::Sym("<=")) => "le",
            Some(Tok::Sym(">")) => "gt",
            Some(Tok::Sym(">=")) => "ge",
            _ => return Ok(left),
        };
        self.pos += 1;
        Ok(json!({"op": op, "left": left, "right": self.primary()?}))
    }

    fn literal(&mut self) -> Result<Value, String> {
        match self.primary()? {
            Value::Object(o) if o["op"] == "literal" => Ok(o["value"].clone()),
            _ => Err("expected a literal".into()),
        }
    }

    fn primary(&mut self) -> Result<Value, String> {
        match self.next() {
            Some(Tok::Sym("(")) => {
                let e = self.or()?;
                self.expect_symbol(")")?;
                Ok(e)
            }
            Some(Tok::Str(s)) => Ok(json!({"op": "literal", "value": s})),
            Some(Tok::Num(n)) => Ok(json!({"op": "literal", "value": n})),
            Some(Tok::Word(w)) if w.eq_ignore_ascii_case("null") => {
                Ok(json!({"op": "literal", "value": null}))
            }
            Some(Tok::Word(w)) if w.eq_ignore_ascii_case("true") => {
                Ok(json!({"op": "literal", "value": true}))
            }
            Some(Tok::Word(w)) if w.eq_ignore_ascii_case("false") => {
                Ok(json!({"op": "literal", "value": false}))
            }
            Some(Tok::Word(w)) => {
                let mut path = vec![w];
                while self.symbol(".") {
                    match self.next() {
                        Some(Tok::Word(w)) => path.push(w),
                        _ => return Err("expected a field name after `.`".into()),
                    }
                }
                Ok(json!({"op": "ident", "name": path}))
            }
            other => Err(format!("unexpected token {other:?}")),
        }
    }
}

fn sym_static(s: &str) -> &'static str {
    [
        "!=", "<>", "<=", ">=", "=", "<", ">", "(", ")", ",", ".", "*",
    ]
    .into_iter()
    .find(|x| *x == s)
    .unwrap_or("")
}

pub fn parse(src: &str) -> Result<Query, String> {
    let mut p = Parser {
        toks: tokenize(src)?,
        pos: 0,
    };
    p.expect_keyword("select")?;
    p.expect_symbol("*")?;
    p.expect_keyword("from")?;
    let object_type = match p.next() {
        Some(Tok::Word(w)) => w,
        _ => return Err("expected an object type after FROM".into()),
    };
    p.expect_symbol("(")?;
    let object_id = match p.next() {
        Some(Tok::Str(s)) => s,
        _ => return Err("expected a quoted object id".into()),
    };
    p.expect_symbol(")")?;
    let filter = if p.keyword("where") {
        Some(p.or()?)
    } else {
        None
    };
    let limit = if p.keyword("limit") {
        match p.next() {
            Some(Tok::Num(n)) if n.is_u64() => n.as_u64().map(|n| n as usize),
            _ => return Err("LIMIT must be a non-negative integer".into()),
        }
    } else {
        None
    };
    if let Some(tok) = p.peek() {
        return Err(format!("unsupported BTQL clause starting at {tok:?}"));
    }
    Ok(Query {
        object_type,
        object_id,
        filter,
        limit,
        cursor: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::btql::matches;

    #[test]
    fn parses_the_ruby_sdk_trace_query() {
        let q = parse("SELECT * FROM experiment('e1') WHERE root_span_id = 'r' AND span_attributes.type != 'score' LIMIT 1000")
            .unwrap();
        assert_eq!(
            (q.object_type.as_str(), q.object_id.as_str(), q.limit),
            ("experiment", "e1", Some(1000))
        );
        let f = q.filter.unwrap();
        assert!(
            matches(
                &f,
                &json!({"root_span_id": "r", "span_attributes": {"type": "llm"}})
            )
            .unwrap()
        );
        assert!(
            !matches(
                &f,
                &json!({"root_span_id": "r", "span_attributes": {"type": "score"}})
            )
            .unwrap()
        );
    }

    #[test]
    fn expression_grammar() {
        // Fields named like keywords must not be read in place of the literals.
        let row =
            json!({"a": 1, "b": "it's", "c": null, "d": true, "n": -2.5, "null": 1, "false": true});
        let check = |sql: &str| {
            let q = parse(&format!("select * from dataset('d') where {sql}")).unwrap();
            matches(&q.filter.unwrap(), &row).unwrap()
        };
        assert!(check("a = 1 or a = 2"));
        assert!(check("(a = 2 or a = 1) and not a = 3"));
        assert!(check("b = 'it''s'"));
        assert!(check("c is null and a is not null"));
        assert!(check("a in (1, 2) and a not in (3)"));
        assert!(check("a <> 2 and a < 2 and a <= 1 and a > 0 and a >= 1"));
        assert!(check("d = true and c = null and n = -2.5"));
        assert!(!check("d = false"));
        assert!(check("d"));
        assert_eq!(parse("SELECT * FROM dataset('d')").unwrap().filter, None);
    }

    #[test]
    fn rejects_what_it_does_not_understand() {
        for (sql, err) in [
            ("select a from dataset('d')", "expected `*`"),
            ("select * dataset('d')", "expected from"),
            ("select * from 'x'", "object type"),
            ("select * from dataset(d)", "quoted object id"),
            ("select * from dataset('d'", "expected `)`"),
            ("select * from dataset('d') limit x", "LIMIT"),
            ("select * from dataset('d') limit 1.5", "LIMIT"),
            ("select * from dataset('d') limit -1", "LIMIT"),
            (
                "select * from dataset('d') order by id",
                "unsupported BTQL clause",
            ),
            ("select * from dataset('d') where a = 'x", "unterminated"),
            (
                "select * from dataset('d') where a ~ 1",
                "unexpected character",
            ),
            ("select * from dataset('d') where a is 1", "expected null"),
            ("select * from dataset('d') where a not = 1", "expected IN"),
            (
                "select * from dataset('d') where a in (b)",
                "expected a literal",
            ),
            ("select * from dataset('d') where a.", "field name"),
            ("select * from dataset('d') where (a = 1", "expected `)`"),
            ("select * from dataset('d') where", "unexpected token"),
            (
                "select * from dataset('d') where a = 1.2.3",
                "invalid number",
            ),
        ] {
            let got = parse(sql).unwrap_err();
            assert!(got.contains(err), "{sql}: {got}");
        }
        assert_eq!(sym_static("?"), "");
    }
}
