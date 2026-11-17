//! Shared fixtures: a SQLite database like consulta's conftest.
#![allow(dead_code)]

use std::path::{Path, PathBuf};

/// A database with consulta's `apps` table: three rows, two in strm 1261.
pub fn apps_db(dir: &Path, name: &str) -> PathBuf {
    let path = dir.join(name);
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(
        "CREATE TABLE apps (app_id INTEGER, email TEXT, score INTEGER, strm TEXT);
         INSERT INTO apps VALUES (1, 'a@x.com', 10, '1261');
         INSERT INTO apps VALUES (2, 'b@y.net', 30, '1261');
         INSERT INTO apps VALUES (3, 'c@z.org', 20, '1265');",
    )
    .unwrap();
    path
}

pub fn count(path: &Path) -> i64 {
    rusqlite::Connection::open(path)
        .unwrap()
        .query_row("SELECT count(*) FROM apps", [], |r| r.get(0))
        .unwrap()
}
