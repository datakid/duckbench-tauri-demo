use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

use calamine::{open_workbook_auto, Data, Reader};
use chrono::{Duration, NaiveDateTime, Timelike};
use serde_json::{Map, Number, Value as J};

use crate::proto::es;

const MAX_SAFE_F: f64 = 9_007_199_254_740_991.0;

pub fn sheet_names(path: &Path) -> Result<Vec<String>, String> {
    let wb = open_workbook_auto(path).map_err(es)?;
    Ok(wb.sheet_names().to_vec())
}

fn round_to_second(dt: NaiveDateTime) -> NaiveDateTime {
    let r = dt + Duration::microseconds(500_000);
    r.with_nanosecond(0).unwrap_or(r)
}

fn date_text(dt: NaiveDateTime) -> String {
    let dt = round_to_second(dt);
    if dt.hour() == 0 && dt.minute() == 0 && dt.second() == 0 {
        dt.format("%Y-%m-%d").to_string()
    } else {
        dt.format("%Y-%m-%d %H:%M:%S").to_string()
    }
}

fn num(f: f64) -> J {
    if !f.is_finite() {
        return J::Null;
    }
    if f.fract() == 0.0 && f.abs() <= MAX_SAFE_F {
        return J::from(f as i64);
    }
    Number::from_f64(f).map(J::Number).unwrap_or(J::Null)
}

fn cell(v: &Data) -> J {
    match v {
        Data::Empty => J::Null,
        Data::String(s) => J::String(s.clone()),
        Data::Int(i) => J::from(*i),
        Data::Float(f) => num(*f),
        Data::Bool(b) => J::Bool(*b),
        Data::DateTime(d) => d
            .as_datetime()
            .map(|x| J::String(date_text(x)))
            .unwrap_or_else(|| num(d.as_f64())),
        Data::DateTimeIso(s) | Data::DurationIso(s) => J::String(s.clone()),
        Data::Error(_) => J::Null,
    }
}

fn is_blank(v: &Data) -> bool {
    match v {
        Data::Empty | Data::Error(_) => true,
        Data::String(s) => s.is_empty(),
        _ => false,
    }
}

fn header_text(v: &Data) -> Option<String> {
    let text = match cell(v) {
        J::Null => return None,
        J::String(s) => s,
        other => other.to_string(),
    };
    let t = text.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

fn dedupe(base: String, used: &[String]) -> String {
    if !used.contains(&base) {
        return base;
    }
    let mut i = 2;
    while used.contains(&format!("{base}_{i}")) {
        i += 1;
    }
    format!("{base}_{i}")
}

pub fn to_json(
    path: &Path,
    sheet: Option<&str>,
    skip: usize,
    header: bool,
    dest: &Path,
    progress: &dyn Fn(u64, u64) -> bool,
) -> Result<usize, String> {
    let mut wb = open_workbook_auto(path).map_err(es)?;
    let name = match sheet {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => wb.sheet_names().first().cloned().ok_or("This workbook has no sheets.")?,
    };
    let range = wb.worksheet_range(&name).map_err(es)?;
    let width = range.width();
    let rows: Vec<&[Data]> = range
        .rows()
        .skip(skip)
        .filter(|r| r.iter().any(|c| !is_blank(c)))
        .collect();

    let (names, body): (Vec<String>, &[&[Data]]) = if header && !rows.is_empty() {
        let mut used: Vec<String> = Vec::with_capacity(width);
        for i in 0..width {
            let base = rows[0].get(i).and_then(header_text).unwrap_or_else(|| format!("column{i}"));
            let n = dedupe(base, &used);
            used.push(n);
        }
        (used, &rows[1..])
    } else {
        ((0..width).map(|i| format!("column{i}")).collect(), &rows[..])
    };

    let total = body.len() as u64;
    if !progress(0, total) {
        return Err("Import cancelled.".into());
    }
    let mut out = BufWriter::with_capacity(1 << 20, File::create(dest).map_err(es)?);
    out.write_all(b"[").map_err(es)?;
    for (ri, row) in body.iter().enumerate() {
        if ri % 2_000 == 0 && ri > 0 && !progress(ri as u64, total) {
            return Err("Import cancelled.".into());
        }
        if ri > 0 {
            out.write_all(b",").map_err(es)?;
        }
        let mut obj = Map::with_capacity(names.len());
        for (i, n) in names.iter().enumerate() {
            obj.insert(n.clone(), row.get(i).map(cell).unwrap_or(J::Null));
        }
        serde_json::to_writer(&mut out, &J::Object(obj)).map_err(es)?;
    }
    out.write_all(b"]").map_err(es)?;
    out.flush().map_err(es)?;
    progress(total, total);
    Ok(body.len())
}
