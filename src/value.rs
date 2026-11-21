//! What a query returns and what it binds, independent of the backend.

use std::collections::BTreeMap;

use serde_json::Value;

/// One value in a result row.
#[derive(Debug, Clone, PartialEq)]
pub enum Cell {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
    /// Raw bytes (BLOB, RAW, VARBINARY). Rendered as lowercase hex everywhere.
    Bytes(Vec<u8>),
}

impl Cell {
    /// The cell as text, or `None` for NULL. Bytes become lowercase hex.
    pub fn text(&self) -> Option<String> {
        match self {
            Cell::Null => None,
            Cell::Bool(b) => Some(b.to_string()),
            Cell::Int(i) => Some(i.to_string()),
            Cell::Float(f) => Some(f.to_string()),
            Cell::Text(s) => Some(s.clone()),
            Cell::Bytes(b) => Some(hex::encode(b)),
        }
    }

    pub fn to_json(&self) -> Value {
        match self {
            Cell::Null => Value::Null,
            Cell::Bool(b) => Value::Bool(*b),
            Cell::Int(i) => Value::from(*i),
            // NaN and infinities have no JSON form; serde_json makes them null.
            Cell::Float(f) => Value::from(*f),
            Cell::Text(s) => Value::String(s.clone()),
            Cell::Bytes(b) => Value::String(hex::encode(b)),
        }
    }
}

/// Columns in the order the query named them, and rows in that order too.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResultSet {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Cell>>,
    /// More rows existed past the limit.
    pub truncated: bool,
}

impl ResultSet {
    /// The rows as a JSON array of objects, keys in column order.
    ///
    /// Written by hand: `serde_json::Map` sorts its keys, and a result's
    /// columns must keep the order the query gave them.
    pub fn rows_json(&self) -> String {
        let mut out = String::from("[");
        for (i, row) in self.rows.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push('{');
            for (j, (name, cell)) in self.columns.iter().zip(row).enumerate() {
                if j > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String(name.clone()).to_string());
                out.push(':');
                out.push_str(&cell.to_json().to_string());
            }
            out.push('}');
        }
        out.push(']');
        out
    }

    /// A hash of the rows as a set: sha256 over the column names and each
    /// row's JSON, rows sorted, so the order a backend returns them in
    /// doesn't matter.
    pub fn content_hash(&self) -> String {
        use sha2::{Digest, Sha256};
        let mut rows: Vec<String> = self
            .rows
            .iter()
            .map(|r| Value::from(r.iter().map(Cell::to_json).collect::<Vec<_>>()).to_string())
            .collect();
        rows.sort();
        let mut h = Sha256::new();
        h.update(Value::from(self.columns.clone()).to_string());
        for r in rows {
            h.update(b"\n");
            h.update(r);
        }
        hex::encode(h.finalize())
    }

    /// The distinct values of the column named `column`, as text, in the
    /// order they first appear. Empty when there is no such column.
    pub fn distinct_text(&self, column: &str) -> Vec<String> {
        let Some(i) = self
            .columns
            .iter()
            .position(|c| c.eq_ignore_ascii_case(column))
        else {
            return Vec::new();
        };
        let mut out: Vec<String> = Vec::new();
        for row in &self.rows {
            let v = row.get(i).and_then(Cell::text).unwrap_or_default();
            if !out.contains(&v) {
                out.push(v);
            }
        }
        out
    }

    /// The shape the MCP tools answer with:
    /// `{"columns":[…],"rows":[{…}],"row_count":n,"truncated":bool}`.
    pub fn to_tool_json(&self) -> String {
        format!(
            "{{\"columns\":{},\"rows\":{},\"row_count\":{},\"truncated\":{}}}",
            Value::from(self.columns.clone()),
            self.rows_json(),
            self.rows.len(),
            self.truncated
        )
    }
}

/// A value bound to a `:name` placeholder.
#[derive(Debug, Clone, PartialEq)]
pub enum Param {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
}

impl Param {
    /// A JSON scalar as a parameter. Arrays and objects have no binding.
    pub fn from_json(v: &Value) -> Result<Self, String> {
        Ok(match v {
            Value::Null => Param::Null,
            Value::Bool(b) => Param::Bool(*b),
            Value::Number(n) => match n.as_i64() {
                Some(i) => Param::Int(i),
                None => Param::Float(n.as_f64().ok_or("number out of range")?),
            },
            Value::String(s) => Param::Text(s.clone()),
            other => return Err(format!("a parameter must be a scalar, not {other}")),
        })
    }
}

impl Param {
    pub fn to_json(&self) -> Value {
        match self {
            Param::Null => Value::Null,
            Param::Bool(b) => Value::Bool(*b),
            Param::Int(i) => Value::from(*i),
            Param::Float(f) => Value::from(*f),
            Param::Text(s) => Value::String(s.clone()),
        }
    }
}

/// Named parameters, keyed by placeholder name without the colon.
pub type Params = BTreeMap<String, Param>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_hash_ignores_row_order_but_not_values() {
        let rs = |rows: Vec<Vec<Cell>>| ResultSet {
            columns: vec!["a".into(), "b".into()],
            rows,
            truncated: false,
        };
        let one = rs(vec![
            vec![Cell::Int(1), Cell::Text("x".into())],
            vec![Cell::Int(2), Cell::Null],
        ]);
        let swapped = rs(vec![
            vec![Cell::Int(2), Cell::Null],
            vec![Cell::Int(1), Cell::Text("x".into())],
        ]);
        let changed = rs(vec![
            vec![Cell::Int(1), Cell::Text("y".into())],
            vec![Cell::Int(2), Cell::Null],
        ]);
        assert_eq!(one.content_hash(), swapped.content_hash());
        assert_ne!(one.content_hash(), changed.content_hash());
        assert_ne!(rs(vec![]).content_hash(), one.content_hash());
    }
}
