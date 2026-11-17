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

/// Named parameters, keyed by placeholder name without the colon.
pub type Params = BTreeMap<String, Param>;
