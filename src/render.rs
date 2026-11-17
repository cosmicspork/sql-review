//! Render a result as a terminal table or an export format.

use crate::value::{Cell, ResultSet};

/// The longest a cell may be in `table` output, as consulta's polars
/// `fmt_str_lengths=200` had it.
const TABLE_CELL_MAX: usize = 200;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum Format {
    Csv,
    Json,
    Markdown,
    Md,
    #[default]
    Table,
    Tsv,
}

impl std::str::FromStr for Format {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        <Self as clap::ValueEnum>::from_str(s, false).map_err(|_| {
            format!("unknown format {s:?}; choose one of: csv, json, markdown, md, table, tsv")
        })
    }
}

pub fn render(rs: &ResultSet, format: Format) -> String {
    match format {
        Format::Table => to_table(rs),
        Format::Csv => to_delimited(rs, b','),
        Format::Tsv => to_delimited(rs, b'\t'),
        Format::Json => rs.rows_json(),
        Format::Markdown | Format::Md => to_markdown(rs),
    }
}

fn to_table(rs: &ResultSet) -> String {
    if rs.columns.is_empty() {
        return "(no rows)".into();
    }
    let cell = |c: &Cell| {
        let text = c.text().unwrap_or_else(|| "null".into());
        let text = text.replace('\n', "\\n");
        if text.chars().count() > TABLE_CELL_MAX {
            let mut cut: String = text.chars().take(TABLE_CELL_MAX).collect();
            cut.push('…');
            cut
        } else {
            text
        }
    };
    let body: Vec<Vec<String>> = rs
        .rows
        .iter()
        .map(|r| r.iter().map(cell).collect())
        .collect();
    let widths: Vec<usize> = rs
        .columns
        .iter()
        .enumerate()
        .map(|(i, name)| {
            body.iter()
                .map(|r| r[i].chars().count())
                .chain([name.chars().count()])
                .max()
                .unwrap_or(0)
        })
        .collect();
    let rule = |l: &str, m: &str, r: &str, fill: &str| {
        let parts: Vec<String> = widths.iter().map(|w| fill.repeat(w + 2)).collect();
        format!("{l}{}{r}", parts.join(m))
    };
    let line = |cells: &[String]| {
        let parts: Vec<String> = cells
            .iter()
            .zip(&widths)
            .map(|(c, w)| format!(" {c}{} ", " ".repeat(w - c.chars().count())))
            .collect();
        format!("│{}│", parts.join("┆"))
    };
    let mut out = vec![rule("┌", "┬", "┐", "─"), line(&rs.columns)];
    out.push(rule("╞", "╪", "╡", "═"));
    out.extend(body.iter().map(|r| line(r)));
    out.push(rule("└", "┴", "┘", "─"));
    out.join("\n")
}

fn to_delimited(rs: &ResultSet, delimiter: u8) -> String {
    let mut w = csv::WriterBuilder::new()
        .delimiter(delimiter)
        .from_writer(Vec::new());
    // Writing to a Vec cannot fail.
    w.write_record(&rs.columns).expect("write to memory");
    for row in &rs.rows {
        w.write_record(row.iter().map(|c| c.text().unwrap_or_default()))
            .expect("write to memory");
    }
    let bytes = w.into_inner().expect("flush to memory");
    let text = String::from_utf8(bytes).expect("csv of UTF-8 cells is UTF-8");
    text.trim_end_matches('\n').to_string()
}

fn to_markdown(rs: &ResultSet) -> String {
    if rs.columns.is_empty() {
        return "(no rows)".into();
    }
    let mut lines = vec![
        format!("| {} |", rs.columns.join(" | ")),
        format!("| {} |", vec!["---"; rs.columns.len()].join(" | ")),
    ];
    for row in &rs.rows {
        let cells: Vec<String> = row
            .iter()
            .map(|c| c.text().unwrap_or_default().replace('|', "\\|"))
            .collect();
        lines.push(format!("| {} |", cells.join(" | ")));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> ResultSet {
        ResultSet {
            columns: vec!["app_id".into(), "email".into(), "score".into()],
            rows: vec![
                vec![Cell::Int(1), Cell::Text("a@x.com".into()), Cell::Int(10)],
                vec![Cell::Int(2), Cell::Text("b@y.net".into()), Cell::Int(30)],
                vec![Cell::Int(3), Cell::Text("c@z.org".into()), Cell::Int(20)],
            ],
            truncated: false,
        }
    }

    fn one(name: &str, cell: Cell) -> ResultSet {
        ResultSet {
            columns: vec![name.into()],
            rows: vec![vec![cell]],
            truncated: false,
        }
    }

    #[test]
    fn csv() {
        let out = render(&sample(), Format::Csv);
        assert_eq!(out.lines().next(), Some("app_id,email,score"));
        assert!(out.contains("a@x.com"));
        assert!(!out.ends_with('\n'));
    }

    #[test]
    fn tsv() {
        let out = render(&sample(), Format::Tsv);
        assert!(out.lines().next().unwrap().contains('\t'));
    }

    #[test]
    fn json_roundtrips() {
        let rows: Vec<serde_json::Value> =
            serde_json::from_str(&render(&sample(), Format::Json)).unwrap();
        assert_eq!(rows[0]["email"], "a@x.com");
        assert_eq!(rows.len(), 3);
    }

    #[test]
    fn json_keeps_column_order() {
        let rs = ResultSet {
            columns: vec!["z".into(), "a".into()],
            rows: vec![vec![Cell::Int(1), Cell::Int(2)]],
            truncated: false,
        };
        assert_eq!(render(&rs, Format::Json), r#"[{"z":1,"a":2}]"#);
    }

    #[test]
    fn markdown() {
        let out = render(&sample(), Format::Markdown);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "| app_id | email | score |");
        assert_eq!(lines[1], "| --- | --- | --- |");
        assert!(lines[2].starts_with("| 1 | a@x.com |"));
        assert_eq!(render(&sample(), Format::Md), out);
    }

    #[test]
    fn markdown_escapes_pipes() {
        let out = render(&one("note", Cell::Text("a|b".into())), Format::Markdown);
        assert!(out.contains("a\\|b"));
    }

    #[test]
    fn table_nonempty() {
        let out = render(&sample(), Format::Table);
        assert!(out.contains("email"));
        assert!(out.contains("a@x.com"));
        assert!(out.starts_with('┌'));
    }

    #[test]
    fn table_without_columns_says_so() {
        assert_eq!(render(&ResultSet::default(), Format::Table), "(no rows)");
        assert_eq!(render(&ResultSet::default(), Format::Markdown), "(no rows)");
    }

    #[test]
    fn table_cuts_long_cells() {
        let out = render(&one("t", Cell::Text("x".repeat(500))), Format::Table);
        assert!(out.contains(&format!("{}…", "x".repeat(200))));
        assert!(!out.contains(&"x".repeat(201)));
    }

    #[test]
    fn unknown_format_is_an_error() {
        let err = "yaml".parse::<Format>().unwrap_err();
        assert!(err.contains("unknown format"));
    }

    #[test]
    fn null_bytes_and_floats() {
        let rs = ResultSet {
            columns: vec!["n".into(), "b".into(), "f".into(), "t".into()],
            rows: vec![vec![
                Cell::Null,
                Cell::Bytes(vec![0xde, 0xad]),
                Cell::Float(1.5),
                Cell::Bool(true),
            ]],
            truncated: false,
        };
        assert_eq!(
            render(&rs, Format::Json),
            r#"[{"n":null,"b":"dead","f":1.5,"t":true}]"#
        );
        assert_eq!(render(&rs, Format::Csv), "n,b,f,t\n,dead,1.5,true");
        assert_eq!(
            render(&rs, Format::Markdown).lines().nth(2),
            Some("|  | dead | 1.5 | true |")
        );
        assert!(render(&rs, Format::Table).contains("│ null ┆ dead ┆ 1.5 ┆ true │"));
    }
}
