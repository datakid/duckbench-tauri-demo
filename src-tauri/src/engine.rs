use std::cell::Cell;
use std::collections::HashSet;
use std::fs;
use std::time::{Duration, Instant};
use std::io::Stdout;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};

use duckdb::{Connection, InterruptHandle};
use serde_json::{json, Value as J};

use crate::excel;
use crate::proto::{read_frame, sql_str, write_frame};
use crate::values::run_query;
use crate::xlsx_out::write_xlsx;

fn sql_path(p: &Path) -> String {
    sql_str(&p.display().to_string())
}

fn base_sql(spill: &Path, threads: usize) -> Vec<String> {
    vec![
        format!("SET temp_directory='{}'", sql_path(spill)),
        format!("SET threads={}", threads.max(1)),
        "SET preserve_insertion_order=true".into(),
        "SET autoinstall_known_extensions=false".into(),
        "SET autoload_known_extensions=true".into(),
    ]
}

fn lockdown_sql(work: &Path, spill: &Path, cache: &Path) -> Vec<String> {
    vec![
        format!(
            "SET allowed_directories=['{}\\', '{}\\', '{}\\']",
            sql_path(work),
            sql_path(spill),
            sql_path(cache)
        ),
        "SET enable_external_access=false".into(),
        "SET lock_configuration=true".into(),
    ]
}

fn apply(conn: &Connection, stmts: &[String]) -> Result<(), duckdb::Error> {
    for s in stmts {
        conn.execute_batch(s)?;
    }
    Ok(())
}

fn lockdown_works(work: &Path, spill: &Path, cache: &Path, threads: usize) -> bool {
    let probe = work.join("__probe_in.csv");
    if fs::write(&probe, "a\n1\n").is_err() {
        return false;
    }
    let result = (|| -> Result<i64, duckdb::Error> {
        let c = Connection::open_in_memory()?;
        apply(&c, &base_sql(spill, threads))?;
        apply(&c, &lockdown_sql(work, spill, cache))?;
        c.execute_batch("COPY (SELECT 1 AS a) TO '__probe_out.csv' (FORMAT CSV)")?;
        c.query_row("SELECT count(*) FROM read_csv('__probe_in.csv')", [], |r| r.get(0))
    })();
    let _ = fs::remove_file(&probe);
    let _ = fs::remove_file(work.join("__probe_out.csv"));
    matches!(result, Ok(1))
}

const CACHE_INDEX_SQL: &str = "CREATE TABLE IF NOT EXISTS dbcache.__index (key VARCHAR PRIMARY KEY, tbl VARCHAR NOT NULL, rejects BIGINT DEFAULT 0, rows BIGINT DEFAULT 0, used TIMESTAMP DEFAULT current_timestamp)";

fn attach_cache(conn: &Connection, cache: &Path) -> bool {
    if fs::create_dir_all(cache).is_err() {
        return false;
    }
    let file = cache.join("workspace.duckdb");
    let try_attach = || -> Result<(), duckdb::Error> {
        conn.execute_batch(&format!("ATTACH '{}' AS dbcache", sql_path(&file)))?;
        conn.execute_batch(CACHE_INDEX_SQL)
    };
    if try_attach().is_ok() {
        return true;
    }
    let _ = conn.execute_batch("DETACH DATABASE IF EXISTS dbcache");
    let _ = fs::remove_file(&file);
    let _ = fs::remove_file(cache.join("workspace.duckdb.wal"));
    try_attach().is_ok()
}

struct Lane {
    conn: Mutex<Connection>,
    interrupt: Arc<InterruptHandle>,
    current: AtomicU64,
}

struct Engine {
    main: Lane,
    bg: Lane,
    work: PathBuf,
    cancelled: Mutex<HashSet<u64>>,
}

impl Engine {
    fn is_cancelled(&self, id: u64) -> bool {
        self.cancelled.lock().unwrap_or_else(|e| e.into_inner()).contains(&id)
    }
    fn forget(&self, id: u64) {
        self.cancelled.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
    }
    fn cancel(&self, target: u64) {
        self.cancelled.lock().unwrap_or_else(|e| e.into_inner()).insert(target);
        for lane in [&self.main, &self.bg] {
            if lane.current.load(Ordering::SeqCst) == target {
                lane.interrupt.interrupt();
            }
        }
    }
}

fn send(out: &Mutex<Stdout>, msg: &J) {
    if let Ok(bytes) = serde_json::to_vec(msg) {
        let mut o = out.lock().unwrap_or_else(|e| e.into_inner());
        if write_frame(&mut *o, &bytes).is_err() {
            std::process::exit(0);
        }
    }
}

fn reply(out: &Mutex<Stdout>, id: &J, result: Result<J, String>) {
    let msg = match result {
        Ok(v) => json!({ "id": id, "ok": true, "result": v }),
        Err(e) => json!({ "id": id, "ok": false, "error": e }),
    };
    send(out, &msg);
}

fn safe_work_name(name: &str) -> Result<&str, String> {
    if name.is_empty() || name.contains(['/', '\\', ':']) || name.contains("..") {
        return Err(format!("Invalid file name: {name}"));
    }
    Ok(name)
}

fn handle(engine: &Engine, lane: &Lane, req: &J, progress: &dyn Fn(u64, u64) -> bool) -> Result<J, String> {
    let sql = req["sql"].as_str().unwrap_or("");
    match req["op"].as_str().unwrap_or("") {
        "query" => {
            let conn = lane.conn.lock().unwrap_or_else(|e| e.into_inner());
            run_query(&conn, sql)
        }
        "exec" => {
            let conn = lane.conn.lock().unwrap_or_else(|e| e.into_inner());
            match conn.execute_batch(sql) {
                Ok(_) => Ok(J::Null),
                Err(e) => {
                    let _ = conn.execute_batch("ROLLBACK");
                    Err(e.to_string())
                }
            }
        }
        "export_xlsx" => {
            let name = safe_work_name(req["name"].as_str().unwrap_or(""))?;
            let conn = lane.conn.lock().unwrap_or_else(|e| e.into_inner());
            write_xlsx(&conn, sql, &engine.work.join(name), req["total"].as_u64().unwrap_or(0), progress)
        }
        "excel_sheets" => {
            let path = PathBuf::from(req["path"].as_str().unwrap_or(""));
            excel::sheet_names(&path).map(|v| json!(v))
        }
        "excel_json" => {
            let path = PathBuf::from(req["path"].as_str().unwrap_or(""));
            let name = safe_work_name(req["name"].as_str().unwrap_or(""))?;
            let rows = excel::to_json(
                &path,
                req["sheet"].as_str(),
                req["skipRows"].as_u64().unwrap_or(0) as usize,
                req["firstRowHeader"].as_bool().unwrap_or(true),
                &engine.work.join(name),
                progress,
            )?;
            Ok(json!({ "name": name, "rows": rows }))
        }
        other => Err(format!("Unknown engine operation: {other}")),
    }
}

fn spawn_lane(engine: Arc<Engine>, out: Arc<Mutex<Stdout>>, rx: mpsc::Receiver<J>, bg: bool) {
    std::thread::spawn(move || {
        for req in rx {
            let lane = if bg { &engine.bg } else { &engine.main };
            let id = req["id"].as_u64().unwrap_or(0);
            lane.current.store(id, Ordering::SeqCst);
            let last = Cell::new(Instant::now());
            let first = Cell::new(true);
            let progress = |done: u64, total: u64| -> bool {
                if engine.is_cancelled(id) {
                    return false;
                }
                if first.get() || last.get().elapsed() >= Duration::from_millis(200) || (total > 0 && done >= total) {
                    first.set(false);
                    last.set(Instant::now());
                    send(&out, &json!({ "event": "progress", "id": id, "done": done, "total": total }));
                }
                true
            };
            let result = if engine.is_cancelled(id) {
                Err("Query cancelled.".to_string())
            } else {
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handle(&engine, lane, &req, &progress)))
                    .unwrap_or_else(|_| Err("The engine hit an internal error on this query.".into()))
            };
            lane.current.store(0, Ordering::SeqCst);
            let was_cancelled = engine.is_cancelled(id);
            engine.forget(id);
            let result = match result {
                Err(e) if was_cancelled => Err(format!("Query cancelled. ({e})")),
                other => other,
            };
            reply(&out, &req["id"], result);
        }
    });
}

pub fn run(config: &str) {
    let cfg: J = serde_json::from_str(config).unwrap_or(J::Null);
    let work = PathBuf::from(cfg["work"].as_str().unwrap_or("."));
    let spill = PathBuf::from(cfg["spill"].as_str().unwrap_or("."));
    let cache = PathBuf::from(cfg["cache"].as_str().unwrap_or("."));
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    let _ = std::env::set_current_dir(&work);
    let out = Arc::new(Mutex::new(std::io::stdout()));
    let locked = lockdown_works(&work, &spill, &cache, threads);

    let mut cache_ok = false;
    let opened = (|| -> Result<(Connection, Connection), duckdb::Error> {
        let main = Connection::open_in_memory()?;
        apply(&main, &base_sql(&spill, threads))?;
        cache_ok = attach_cache(&main, &cache);
        if locked {
            apply(&main, &lockdown_sql(&work, &spill, &cache))?;
        }
        let bg = main.try_clone()?;
        Ok((main, bg))
    })();
    let (main, bg) = match opened {
        Ok(x) => x,
        Err(e) => {
            reply(&out, &json!(0), Err(format!("DuckDB failed to start: {e}")));
            std::process::exit(2);
        }
    };
    let version: String = main.query_row("SELECT version()", [], |r| r.get(0)).unwrap_or_default();
    let engine = Arc::new(Engine {
        main: Lane { interrupt: main.interrupt_handle(), conn: Mutex::new(main), current: AtomicU64::new(0) },
        bg: Lane { interrupt: bg.interrupt_handle(), conn: Mutex::new(bg), current: AtomicU64::new(0) },
        work,
        cancelled: Mutex::new(HashSet::new()),
    });
    reply(&out, &json!(0), Ok(json!({ "version": version, "locked": locked, "threads": threads, "cache": cache_ok })));

    let (main_tx, main_rx) = mpsc::channel::<J>();
    let (bg_tx, bg_rx) = mpsc::channel::<J>();
    spawn_lane(engine.clone(), out.clone(), main_rx, false);
    spawn_lane(engine.clone(), out.clone(), bg_rx, true);

    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    while let Ok(buf) = read_frame(&mut input) {
        let Ok(req) = serde_json::from_slice::<J>(&buf) else { continue };
        if req["op"].as_str() == Some("cancel") {
            if let Some(target) = req["target"].as_u64() {
                engine.cancel(target);
            }
            continue;
        }
        let is_bg = req["lane"].as_str() == Some("bg");
        let _ = if is_bg { bg_tx.send(req) } else { main_tx.send(req) };
    }
    std::process::exit(0);
}
