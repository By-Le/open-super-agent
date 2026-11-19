//! 一个 SQLite 连接，全进程共用（Python 版的 memory.conn）。可重入的锁：同一个线程里嵌套调用不会自己锁死自己；
//! 不能拿着它跨 await（guard 不是 Send，编译器会拦）。

use parking_lot::{ReentrantMutex, ReentrantMutexGuard};
use rusqlite::types::{ToSqlOutput, Value as SqlValue, ValueRef};
use rusqlite::{Connection, Params, ToSql};
use serde_json::{Value, json};
use std::sync::LazyLock;

static CONN: LazyLock<ReentrantMutex<Connection>> = LazyLock::new(|| {
    let p = &*crate::config::DB_PATH;
    if let Some(d) = p.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let c = Connection::open(p).unwrap_or_else(|e| panic!("打不开数据库 {}：{e}", p.display()));
    let _ = c.busy_timeout(std::time::Duration::from_secs(5));
    ReentrantMutex::new(c)
});

pub fn conn() -> ReentrantMutexGuard<'static, Connection> {
    CONN.lock()
}

fn to_json(v: ValueRef) -> Value {
    match v {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) => json!(i),
        ValueRef::Real(f) => json!(f),
        ValueRef::Text(t) => Value::String(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) => Value::String(String::from_utf8_lossy(b).into_owned()),
    }
}

/// 查询，每行是一列列的 JSON 值
pub fn rows(sql: &str, params: impl Params) -> Vec<Vec<Value>> {
    let c = conn();
    let mut st = match c.prepare_cached(sql) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("SQL 出错：{e}\n{sql}");
            return vec![];
        }
    };
    let n = st.column_count();
    let mut out = vec![];
    let mut q = match st.query(params) {
        Ok(q) => q,
        Err(e) => {
            tracing::error!("SQL 出错：{e}\n{sql}");
            return vec![];
        }
    };
    loop {
        match q.next() {
            Ok(Some(r)) => out.push((0..n).map(|i| r.get_ref(i).map(to_json).unwrap_or(Value::Null)).collect()),
            Ok(None) => break,
            Err(e) => {
                tracing::error!("SQL 出错：{e}\n{sql}");
                break;
            }
        }
    }
    out
}

pub fn row(sql: &str, params: impl Params) -> Option<Vec<Value>> {
    rows(sql, params).into_iter().next()
}

/// 第一行第一列（没有是 Null）
pub fn scalar(sql: &str, params: impl Params) -> Value {
    row(sql, params).and_then(|r| r.into_iter().next()).unwrap_or(Value::Null)
}

pub fn scalar_i64(sql: &str, params: impl Params) -> i64 {
    crate::py::as_int(&scalar(sql, params)).unwrap_or(0)
}

pub fn exists(sql: &str, params: impl Params) -> bool {
    row(sql, params).is_some()
}

/// 执行，返回改了几行
pub fn exec(sql: &str, params: impl Params) -> usize {
    let c = conn();
    match c.prepare_cached(sql).and_then(|mut s| s.execute(params)) {
        Ok(n) => n,
        Err(e) => {
            tracing::error!("SQL 出错：{e}\n{sql}");
            0
        }
    }
}

/// 执行，出错返回错误（迁移、要知道成没成的地方用）
pub fn try_exec(sql: &str, params: impl Params) -> rusqlite::Result<usize> {
    conn().execute(sql, params)
}

/// 插入，返回 lastrowid
pub fn insert(sql: &str, params: impl Params) -> i64 {
    let c = conn();
    match c.prepare_cached(sql).and_then(|mut s| s.execute(params)) {
        Ok(_) => c.last_insert_rowid(),
        Err(e) => {
            tracing::error!("SQL 出错：{e}\n{sql}");
            0
        }
    }
}

/// 执行好几条（迁移用）
pub fn batch(sql: &str) {
    if let Err(e) = conn().execute_batch(sql) {
        tracing::error!("SQL 出错：{e}\n{sql}");
    }
}

/// 一个表现在有哪些列
pub fn columns(table: &str) -> Vec<String> {
    rows(&format!("pragma table_info({table})"), [])
        .into_iter()
        .filter_map(|r| r.get(1).and_then(|v| v.as_str().map(String::from)))
        .collect()
}

pub fn add_column(table: &str, col: &str, decl: &str) -> bool {
    if columns(table).iter().any(|c| c == col) {
        return false;
    }
    batch(&format!("alter table {table} add column {col} {decl}"));
    true
}

/// 把 JSON 值当 SQL 参数用（数字、字符串、null；别的存成 JSON 文字）
pub struct J<'a>(pub &'a Value);

impl ToSql for J<'_> {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(match self.0 {
            Value::Null => ToSqlOutput::Owned(SqlValue::Null),
            Value::Bool(b) => ToSqlOutput::Owned(SqlValue::Integer(*b as i64)),
            Value::Number(n) => match n.as_i64() {
                Some(i) => ToSqlOutput::Owned(SqlValue::Integer(i)),
                None => ToSqlOutput::Owned(SqlValue::Real(n.as_f64().unwrap_or(0.0))),
            },
            Value::String(s) => ToSqlOutput::Borrowed(ValueRef::Text(s.as_bytes())),
            other => ToSqlOutput::Owned(SqlValue::Text(crate::py::dumps_u(other))),
        })
    }
}

/// 一行里第 i 列的取值小工具
pub trait RowExt {
    fn i(&self, i: usize) -> i64;
    fn oi(&self, i: usize) -> Option<i64>;
    fn s(&self, i: usize) -> String;
    fn f(&self, i: usize) -> f64;
    fn j(&self, i: usize) -> Value;
}

impl RowExt for Vec<Value> {
    fn i(&self, i: usize) -> i64 {
        self.get(i).and_then(crate::py::as_int).unwrap_or(0)
    }
    fn oi(&self, i: usize) -> Option<i64> {
        self.get(i).and_then(crate::py::as_int)
    }
    fn s(&self, i: usize) -> String {
        match self.get(i) {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Null) | None => String::new(),
            Some(v) => crate::py::str_of(v),
        }
    }
    fn f(&self, i: usize) -> f64 {
        self.get(i).and_then(|v| v.as_f64()).unwrap_or(0.0)
    }
    /// 存成 JSON 文字的列，解析出来（空的是 {}）
    fn j(&self, i: usize) -> Value {
        match self.get(i) {
            Some(Value::String(s)) if !s.is_empty() => serde_json::from_str(s).unwrap_or(json!({})),
            _ => json!({}),
        }
    }
}
