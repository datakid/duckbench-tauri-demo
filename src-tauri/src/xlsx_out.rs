use std::path::Path;

use duckdb::types::Value;
use duckdb::Connection;
use rust_xlsxwriter::{Format, Workbook, Worksheet};
use serde_json::{json, Value as J};

use crate::proto::es;
use crate::values::{micros, MAX_SAFE};

pub const XLSX_MAX_ROWS: u32 = 1_048_575;

fn put(ws: &mut Worksheet, r: u32, c: u16, v: Value, date_fmt: &Format, ts_fmt: &Format) -> Result<(), String> {
    let num = |ws: &mut Worksheet, x: f64| ws.write_number(r, c, x).map(|_| ()).map_err(es);
    let text = |ws: &mut Worksheet, s: String| ws.write_string(r, c, s).map(|_| ()).map_err(es);
    match v {
        Value::Null => Ok(()),
        Value::Boolean(b) => ws.write_boolean(r, c, b).map(|_| ()).map_err(es),
        Value::TinyInt(x) => num(ws, x as f64),
        Value::SmallInt(x) => num(ws, x as f64),
        Value::Int(x) => num(ws, x as f64),
        Value::UTinyInt(x) => num(ws, x as f64),
        Value::USmallInt(x) => num(ws, x as f64),
        Value::UInt(x) => num(ws, x as f64),
        Value::BigInt(x) if x.unsigned_abs() <= MAX_SAFE => num(ws, x as f64),
        Value::BigInt(x) => text(ws, x.to_string()),
        Value::UBigInt(x) if x <= MAX_SAFE => num(ws, x as f64),
        Value::UBigInt(x) => text(ws, x.to_string()),
        Value::HugeInt(x) => text(ws, x.to_string()),
        Value::Float(x) if x.is_finite() => num(ws, x as f64),
        Value::Double(x) if x.is_finite() => num(ws, x),
        Value::Float(_) | Value::Double(_) => Ok(()),
        Value::Decimal(d) => {
            let s = d.to_string();
            match s.parse::<f64>() {
                Ok(f) if f.abs() <= MAX_SAFE as f64 => num(ws, f),
                _ => text(ws, s),
            }
        }
        Value::Date32(days) => ws
            .write_number_with_format(r, c, days as f64 + 25_569.0, date_fmt)
            .map(|_| ())
            .map_err(es),
        Value::Timestamp(unit, t) => ws
            .write_number_with_format(r, c, micros(unit, t) as f64 / 86_400_000_000.0 + 25_569.0, ts_fmt)
            .map(|_| ())
            .map_err(es),
        Value::Text(s) | Value::Enum(s) => text(ws, s.chars().take(32_767).collect()),
        other => text(ws, format!("{:?}", other)),
    }
}

pub fn write_xlsx(conn: &Connection, sql: &str, dest: &Path, total: u64, progress: &dyn Fn(u64, u64) -> bool) -> Result<J, String> {
    let total = total.min(XLSX_MAX_ROWS as u64);
    let limited = format!("SELECT * FROM ({sql}) LIMIT {}", XLSX_MAX_ROWS as u64 + 1);
    let mut stmt = conn.prepare(&limited).map_err(es)?;
    let mut rows = stmt.query([]).map_err(es)?;
    let date_fmt = Format::new().set_num_format("yyyy-mm-dd");
    let ts_fmt = Format::new().set_num_format("yyyy-mm-dd hh:mm:ss");
    let mut wb = Workbook::new();
    let ws = wb.add_worksheet_with_constant_memory();
    let mut r: u32 = 1;
    let mut truncated = false;
    let mut names: Vec<String> = Vec::new();
    while let Some(row) = rows.next().map_err(es)? {
        if names.is_empty() {
            let st = row.as_ref();
            names = (0..st.column_count())
                .map(|i| st.column_name(i).map(|s| s.to_string()).unwrap_or_default())
                .collect();
            for (i, n) in names.iter().enumerate() {
                ws.write_string(0, i as u16, n).map_err(es)?;
            }
        }
        if r > XLSX_MAX_ROWS {
            truncated = true;
            break;
        }
        if r % 5_000 == 0 && !progress(r as u64, total) {
            return Err("Export cancelled.".into());
        }
        for i in 0..names.len() {
            put(ws, r, i as u16, row.get::<usize, Value>(i).unwrap_or(Value::Null), &date_fmt, &ts_fmt)?;
        }
        r += 1;
    }
    if names.is_empty() {
        let cols: Vec<String> = rows.as_ref().map(|s| s.column_names()).unwrap_or_default();
        for (i, n) in cols.iter().enumerate() {
            ws.write_string(0, i as u16, n).map_err(es)?;
        }
    }
    drop(rows);
    progress((r - 1) as u64, total.max((r - 1) as u64));
    wb.save(dest).map_err(es)?;
    Ok(json!({ "rows": r - 1, "truncated": truncated }))
}
