use duckdb::types::{TimeUnit, Value};
use duckdb::Connection;
use serde_json::{json, Value as J};

use crate::proto::es;

pub const MAX_SAFE: u64 = 9_007_199_254_740_991;

pub fn micros(unit: TimeUnit, t: i64) -> i64 {
    match unit {
        TimeUnit::Second => t.saturating_mul(1_000_000),
        TimeUnit::Millisecond => t.saturating_mul(1_000),
        TimeUnit::Microsecond => t,
        TimeUnit::Nanosecond => t / 1_000,
    }
}

pub fn to_json(v: Value) -> J {
    match v {
        Value::Null => J::Null,
        Value::Boolean(b) => J::Bool(b),
        Value::TinyInt(x) => json!(x),
        Value::SmallInt(x) => json!(x),
        Value::Int(x) => json!(x),
        Value::UTinyInt(x) => json!(x),
        Value::USmallInt(x) => json!(x),
        Value::UInt(x) => json!(x),
        Value::BigInt(x) => {
            if x.unsigned_abs() <= MAX_SAFE {
                json!(x)
            } else {
                J::String(x.to_string())
            }
        }
        Value::UBigInt(x) => {
            if x <= MAX_SAFE {
                json!(x)
            } else {
                J::String(x.to_string())
            }
        }
        Value::HugeInt(x) => J::String(x.to_string()),
        Value::Float(x) => json!(x as f64),
        Value::Double(x) => json!(x),
        Value::Decimal(d) => {
            let s = d.to_string();
            s.parse::<f64>().map(|f| json!(f)).unwrap_or(J::String(s))
        }
        Value::Text(s) => J::String(s),
        Value::Enum(s) => J::String(s),
        Value::Date32(days) => chrono::NaiveDate::from_num_days_from_ce_opt(days + 719_163)
            .map(|d| J::String(d.format("%Y-%m-%d").to_string()))
            .unwrap_or(J::Null),
        Value::Timestamp(unit, t) => chrono::DateTime::from_timestamp_micros(micros(unit, t))
            .map(|dt| J::String(dt.naive_utc().format("%Y-%m-%d %H:%M:%S").to_string()))
            .unwrap_or(J::Null),
        other => J::String(format!("{:?}", other)),
    }
}

pub fn run_query(conn: &Connection, sql: &str) -> Result<J, String> {
    let mut stmt = conn.prepare(sql).map_err(es)?;
    let mut rows = stmt.query([]).map_err(es)?;
    let mut out: Vec<J> = Vec::new();
    let mut ncols = 0usize;
    while let Some(row) = rows.next().map_err(es)? {
        if ncols == 0 {
            ncols = row.as_ref().column_count();
        }
        let mut r = Vec::with_capacity(ncols);
        for i in 0..ncols {
            r.push(to_json(row.get::<usize, Value>(i).unwrap_or(Value::Null)));
        }
        out.push(J::Array(r));
    }
    let columns: Vec<String> = rows.as_ref().map(|s| s.column_names()).unwrap_or_default();
    Ok(json!({ "columns": columns, "rows": out }))
}
