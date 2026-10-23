#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod engine;
mod excel;
mod proto;
mod supervisor;
mod values;
mod xlsx_out;

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, UNIX_EPOCH};

use serde_json::{json, Value as J};
use tauri::ipc::{InvokeBody, Request, Response};
use tauri::{Emitter, Manager, State};
use tauri_plugin_dialog::DialogExt;

use crate::supervisor::Supervisor;

const DATA_EXTS: &[&str] = &["csv", "tsv", "txt", "parquet", "xlsx", "xlsm", "xls", "xlsb", "ods"];
const PROJECT_EXT: &str = "duckbench";

struct Host {
    sup: Arc<Supervisor>,
    work: PathBuf,
    autosave: PathBuf,
    granted: Mutex<HashSet<PathBuf>>,
    out_dirs: Mutex<HashSet<PathBuf>>,
    launch: Mutex<Vec<String>>,
    watched: Arc<Mutex<HashMap<PathBuf, (u64, u64)>>>,
    cancelled_copies: Arc<Mutex<HashSet<String>>>,
}

impl Host {
    fn grant(&self, p: &Path) {
        self.granted.lock().unwrap_or_else(|e| e.into_inner()).insert(norm(p));
    }
    fn check(&self, p: &Path) -> Result<PathBuf, String> {
        let n = norm(p);
        let in_work = n.starts_with(norm(&self.work)) && !p.display().to_string().contains("..");
        let in_out_dir = p.extension().map(|e| e.to_string_lossy().eq_ignore_ascii_case("csv")).unwrap_or(false)
            && p.parent().map(|d| self.out_dirs.lock().unwrap_or_else(|e| e.into_inner()).contains(&norm(d))).unwrap_or(false);
        if in_work || in_out_dir || self.granted.lock().unwrap_or_else(|e| e.into_inner()).contains(&n) {
            Ok(PathBuf::from(p))
        } else {
            Err("Duckbench can only use files you picked yourself. Choose the file again.".into())
        }
    }
}

fn norm(p: &Path) -> PathBuf {
    PathBuf::from(p.display().to_string().to_lowercase().replace('/', "\\"))
}

fn es<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

fn safe_name(name: &str) -> Result<String, String> {
    if name.is_empty() || name.contains(['/', '\\', ':']) || name.contains("..") {
        return Err(format!("Invalid file name: {name}"));
    }
    Ok(name.to_string())
}

fn header(req: &Request<'_>, key: &str) -> Result<String, String> {
    let raw = req.headers().get(key).and_then(|v| v.to_str().ok()).ok_or(format!("Missing {key}"))?;
    urlencoding::decode(raw).map(|s| s.into_owned()).map_err(es)
}

fn raw_body<'a>(req: &'a Request<'_>) -> Result<&'a [u8], String> {
    match req.body() {
        InvokeBody::Raw(bytes) => Ok(bytes.as_slice()),
        _ => Err("Expected raw bytes".into()),
    }
}

fn describe(p: &Path) -> J {
    let meta = fs::metadata(p).ok();
    let modified = meta
        .as_ref()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    json!({
        "path": p.display().to_string(),
        "name": p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
        "size": meta.as_ref().map(|m| m.len()).unwrap_or(0),
        "lastModified": modified,
        "exists": meta.is_some()
    })
}

fn atomic_write(dest: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut tmp = dest.as_os_str().to_owned();
    tmp.push(".partial");
    let tmp = PathBuf::from(tmp);
    let mut f = fs::File::create(&tmp).map_err(es)?;
    f.write_all(bytes).map_err(es)?;
    f.sync_all().map_err(es)?;
    drop(f);
    fs::rename(&tmp, dest).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        es(e)
    })
}

fn atomic_move(src: &Path, dest: &Path) -> Result<(), String> {
    if fs::rename(src, dest).is_ok() {
        return Ok(());
    }
    let mut tmp = dest.as_os_str().to_owned();
    tmp.push(".partial");
    let tmp = PathBuf::from(tmp);
    fs::copy(src, &tmp).map_err(es)?;
    fs::OpenOptions::new().write(true).open(&tmp).and_then(|f| f.sync_all()).map_err(es)?;
    fs::rename(&tmp, dest).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        es(e)
    })?;
    let _ = fs::remove_file(src);
    Ok(())
}

fn project_paths(v: &J) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(p) = v["project"]["main"]["path"].as_str() {
        out.push(PathBuf::from(p));
    }
    if let Some(list) = v["project"]["sources"].as_array() {
        for s in list {
            if let Some(p) = s["path"].as_str() {
                out.push(PathBuf::from(p));
            }
        }
    }
    out
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T, String> + Send + 'static) -> Result<T, String> {
    tauri::async_runtime::spawn_blocking(f).await.map_err(|e| format!("Task failed: {e}"))?
}

#[tauri::command]
async fn engine_info(host: State<'_, Host>) -> Result<J, String> {
    let sup = host.sup.clone();
    let work = host.work.display().to_string();
    let mut info = blocking(move || sup.info()).await?;
    info["workDir"] = json!(work);
    Ok(info)
}

fn is_data_file(p: &Path) -> bool {
    let ext = p.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    DATA_EXTS.contains(&ext.as_str()) && p.is_file()
}

#[tauri::command]
fn adopt_paths(paths: Vec<String>, host: State<'_, Host>) -> Vec<J> {
    paths
        .iter()
        .map(|p| {
            let pb = PathBuf::from(p);
            if is_data_file(&pb) {
                host.grant(&pb);
            }
            describe(&pb)
        })
        .collect()
}

#[tauri::command]
async fn db_query(sql: String, lane: Option<String>, ticket: Option<String>, host: State<'_, Host>) -> Result<J, String> {
    let sup = host.sup.clone();
    let req = json!({ "op": "query", "sql": sql, "lane": lane.unwrap_or_else(|| "main".into()) });
    blocking(move || sup.request(req, ticket)).await
}

#[tauri::command]
fn db_cancel(ticket: String, host: State<'_, Host>) {
    host.sup.cancel(&ticket);
}

#[tauri::command]
fn db_cancel_copy(ticket: String, host: State<'_, Host>) {
    host.cancelled_copies.lock().unwrap_or_else(|e| e.into_inner()).insert(ticket);
}

#[tauri::command]
async fn db_exec(sql: String, lane: Option<String>, ticket: Option<String>, host: State<'_, Host>) -> Result<J, String> {
    let sup = host.sup.clone();
    let req = json!({ "op": "exec", "sql": sql, "lane": lane.unwrap_or_else(|| "main".into()) });
    blocking(move || sup.request(req, ticket)).await
}

#[tauri::command]
async fn export_xlsx(sql: String, name: String, total: Option<u64>, ticket: Option<String>, host: State<'_, Host>) -> Result<J, String> {
    let name = safe_name(&name)?;
    let sup = host.sup.clone();
    blocking(move || sup.request(json!({ "op": "export_xlsx", "sql": sql, "name": name, "total": total.unwrap_or(0) }), ticket)).await
}

#[tauri::command]
async fn excel_sheets(path: String, host: State<'_, Host>) -> Result<J, String> {
    let path = host.check(Path::new(&path))?;
    let sup = host.sup.clone();
    blocking(move || sup.request(json!({ "op": "excel_sheets", "path": path, "lane": "bg" }), None)).await
}

#[tauri::command]
async fn excel_to_json(path: String, name: String, sheet: Option<String>, skip_rows: Option<u64>, first_row_header: Option<bool>, ticket: Option<String>, host: State<'_, Host>) -> Result<J, String> {
    let path = host.check(Path::new(&path))?;
    let name = safe_name(&name)?;
    let sup = host.sup.clone();
    let req = json!({
        "op": "excel_json", "path": path, "name": name, "sheet": sheet,
        "skipRows": skip_rows.unwrap_or(0), "firstRowHeader": first_row_header.unwrap_or(true)
    });
    blocking(move || sup.request(req, ticket)).await
}

fn open_elsewhere_for_writing(p: &Path) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        match fs::OpenOptions::new().read(true).share_mode(0x1).open(p) {
            Ok(_) => false,
            Err(e) => e.raw_os_error() == Some(32),
        }
    }
    #[cfg(not(windows))]
    {
        let _ = p;
        false
    }
}

fn copy_with_progress(src: &Path, dest: &Path, emit: &dyn Fn(u64, u64), cancelled: &dyn Fn() -> bool) -> Result<(), String> {
    let total = fs::metadata(src).map(|m| m.len()).unwrap_or(0);
    let mut from = fs::File::open(src).map_err(|e| format!("Couldn\u{2019}t read {}: {e}", src.display()))?;
    let mut to = std::io::BufWriter::with_capacity(4 << 20, fs::File::create(dest).map_err(es)?);
    let mut buf = vec![0u8; 4 << 20];
    let mut done: u64 = 0;
    let mut last = Instant::now();
    emit(0, total);
    loop {
        if cancelled() {
            drop(to);
            let _ = fs::remove_file(dest);
            return Err("Import cancelled.".into());
        }
        let n = from.read(&mut buf).map_err(es)?;
        if n == 0 {
            break;
        }
        to.write_all(&buf[..n]).map_err(es)?;
        done += n as u64;
        if last.elapsed() >= Duration::from_millis(200) {
            last = Instant::now();
            emit(done, total);
        }
    }
    to.flush().map_err(es)?;
    emit(total.max(done), total.max(done));
    Ok(())
}

#[tauri::command]
async fn stage_file(app: tauri::AppHandle, path: String, name: String, ticket: Option<String>, host: State<'_, Host>) -> Result<J, String> {
    let src = host.check(Path::new(&path))?;
    let dest = host.work.join(safe_name(&name)?);
    let cancels = host.cancelled_copies.clone();
    blocking(move || {
        let _ = fs::remove_file(&dest);
        let busy = open_elsewhere_for_writing(&src);
        if !busy && fs::hard_link(&src, &dest).is_ok() {
            return Ok(json!({ "mode": "link" }));
        }
        let t = ticket.clone().unwrap_or_default();
        let emit = |done: u64, total: u64| {
            if !t.is_empty() {
                let _ = app.emit("engine-progress", json!({ "ticket": t, "done": done, "total": total }));
            }
        };
        let cancelled = || !t.is_empty() && cancels.lock().unwrap_or_else(|e| e.into_inner()).remove(&t);
        copy_with_progress(&src, &dest, &emit, &cancelled)?;
        Ok(json!({ "mode": if busy { "snapshot" } else { "copy" } }))
    })
    .await
}

fn stamp(p: &Path) -> (u64, u64) {
    fs::metadata(p)
        .map(|m| {
            let t = m.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_millis() as u64).unwrap_or(0);
            (m.len(), t)
        })
        .unwrap_or((0, 0))
}

#[tauri::command]
fn watch_sources(paths: Vec<String>, host: State<'_, Host>) {
    let mut w = host.watched.lock().unwrap_or_else(|e| e.into_inner());
    let next: HashMap<PathBuf, (u64, u64)> = paths
        .into_iter()
        .map(PathBuf::from)
        .filter(|p| host.check(p).is_ok())
        .map(|p| {
            let s = w.get(&p).copied().unwrap_or_else(|| stamp(&p));
            (p, s)
        })
        .collect();
    *w = next;
}

fn start_watcher(app: tauri::AppHandle, watched: Arc<Mutex<HashMap<PathBuf, (u64, u64)>>>) {
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(2));
        let snapshot: Vec<(PathBuf, (u64, u64))> = watched.lock().unwrap_or_else(|e| e.into_inner()).iter().map(|(k, v)| (k.clone(), *v)).collect();
        for (p, old) in snapshot {
            let now = stamp(&p);
            if now == old || now == (0, 0) {
                continue;
            }
            std::thread::sleep(Duration::from_millis(600));
            let settled = stamp(&p);
            if settled != now {
                continue;
            }
            if let Some(v) = watched.lock().unwrap_or_else(|e| e.into_inner()).get_mut(&p) {
                *v = settled;
            }
            let _ = app.emit("source-changed", describe(&p));
        }
    });
}

#[tauri::command]
async fn work_write(req: Request<'_>, host: State<'_, Host>) -> Result<(), String> {
    use std::io::Write;
    let name = safe_name(&header(&req, "x-name")?)?;
    let append = req.headers().get("x-append").is_some();
    let bytes = raw_body(&req)?;
    let path = host.work.join(name);
    let mut f = fs::OpenOptions::new().create(true).write(true).append(append).truncate(!append).open(path).map_err(es)?;
    f.write_all(bytes).map_err(es)
}

#[tauri::command]
async fn work_read(name: String, host: State<'_, Host>) -> Result<Response, String> {
    fs::read(host.work.join(safe_name(&name)?)).map(Response::new).map_err(es)
}

#[tauri::command]
fn work_drop(name: String, host: State<'_, Host>) -> Result<(), String> {
    match fs::remove_file(host.work.join(safe_name(&name)?)) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(es(e)),
        _ => Ok(()),
    }
}

#[tauri::command]
async fn work_to_path(name: String, dest: String, host: State<'_, Host>) -> Result<J, String> {
    let src = host.work.join(safe_name(&name)?);
    let dest = host.check(Path::new(&dest))?;
    blocking(move || {
        atomic_move(&src, &dest)?;
        Ok(describe(&dest))
    })
    .await
}

#[tauri::command]
async fn pick_files(app: tauri::AppHandle, multiple: bool, kind: String, host: State<'_, Host>) -> Result<Vec<J>, String> {
    let mut d = app.dialog().file();
    d = match kind.as_str() {
        "project" => d.add_filter("Duckbench project or recipe", &[PROJECT_EXT, "json"]),
        _ => d.add_filter("Data files", DATA_EXTS),
    };
    let picked: Vec<PathBuf> = if multiple {
        d.blocking_pick_files().unwrap_or_default().into_iter().filter_map(|f| f.into_path().ok()).collect()
    } else {
        d.blocking_pick_file().and_then(|f| f.into_path().ok()).into_iter().collect()
    };
    Ok(picked.iter().map(|p| {
        host.grant(p);
        describe(p)
    }).collect())
}

#[tauri::command]
async fn pick_save_path(app: tauri::AppHandle, name: String, host: State<'_, Host>) -> Result<Option<String>, String> {
    let ext = Path::new(&name).extension().map(|e| e.to_string_lossy().to_string()).unwrap_or_default();
    let mut d = app.dialog().file().set_file_name(&name);
    if !ext.is_empty() {
        d = d.add_filter(ext.to_uppercase(), &[ext.as_str()]);
    }
    let Some(path) = d.blocking_save_file().and_then(|f| f.into_path().ok()) else { return Ok(None) };
    host.grant(&path);
    Ok(Some(path.display().to_string()))
}

#[tauri::command]
async fn pick_folder(app: tauri::AppHandle, host: State<'_, Host>) -> Result<Option<String>, String> {
    let Some(dir) = app.dialog().file().blocking_pick_folder().and_then(|f| f.into_path().ok()) else { return Ok(None) };
    host.out_dirs.lock().unwrap_or_else(|e| e.into_inner()).insert(norm(&dir));
    Ok(Some(dir.display().to_string()))
}

#[tauri::command]
async fn write_path(req: Request<'_>, host: State<'_, Host>) -> Result<J, String> {
    let dest = host.check(Path::new(&header(&req, "x-path")?))?;
    atomic_write(&dest, raw_body(&req)?)?;
    Ok(describe(&dest))
}

#[tauri::command]
async fn read_path(path: String, host: State<'_, Host>) -> Result<Response, String> {
    let p = host.check(Path::new(&path))?;
    fs::read(p).map(Response::new).map_err(es)
}

#[tauri::command]
async fn open_project(path: String, host: State<'_, Host>) -> Result<J, String> {
    let p = PathBuf::from(&path);
    let ext = p.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    if ext != PROJECT_EXT && ext != "json" {
        return Err("That isn’t a Duckbench project or recipe file.".into());
    }
    host.grant(&p);
    let text = fs::read_to_string(&p).map_err(es)?;
    let v: J = serde_json::from_str(&text).map_err(|e| format!("Not a valid project file: {e}"))?;
    let files: Vec<J> = project_paths(&v).iter().map(|f| {
        if is_data_file(f) {
            host.grant(f);
        }
        describe(f)
    }).collect();
    Ok(json!({ "text": text, "files": files, "file": describe(&p) }))
}

#[tauri::command]
async fn autosave_write(text: String, host: State<'_, Host>) -> Result<(), String> {
    atomic_write(&host.autosave, text.as_bytes())
}

#[tauri::command]
fn autosave_read(host: State<'_, Host>) -> Result<Option<J>, String> {
    let Ok(text) = fs::read_to_string(&host.autosave) else { return Ok(None) };
    let Ok(v) = serde_json::from_str::<J>(&text) else { return Ok(None) };
    let files: Vec<J> = project_paths(&v).iter().map(|f| {
        if is_data_file(f) {
            host.grant(f);
        }
        describe(f)
    }).collect();
    Ok(Some(json!({ "text": text, "files": files })))
}

#[tauri::command]
fn autosave_clear(host: State<'_, Host>) {
    let _ = fs::remove_file(&host.autosave);
}

#[tauri::command]
fn file_info(path: String, host: State<'_, Host>) -> Result<J, String> {
    let p = host.check(Path::new(&path))?;
    Ok(describe(&p))
}

#[tauri::command]
fn launch_paths(host: State<'_, Host>) -> Vec<J> {
    std::mem::take(&mut *host.launch.lock().unwrap_or_else(|e| e.into_inner()))
        .into_iter()
        .map(|p| {
            let pb = PathBuf::from(&p);
            host.grant(&pb);
            describe(&pb)
        })
        .collect()
}

#[cfg(feature = "updater")]
#[tauri::command]
async fn check_update(app: tauri::AppHandle) -> Result<Option<String>, String> {
    use tauri_plugin_updater::UpdaterExt;
    let update = app.updater().map_err(es)?.check().await.map_err(es)?;
    Ok(update.map(|u| u.version))
}

#[cfg(feature = "updater")]
#[tauri::command]
async fn install_update(app: tauri::AppHandle) -> Result<(), String> {
    use tauri_plugin_updater::UpdaterExt;
    if let Some(u) = app.updater().map_err(es)?.check().await.map_err(es)? {
        u.download_and_install(|_, _| {}, || {}).await.map_err(es)?;
        app.restart();
    }
    Ok(())
}

#[cfg(not(feature = "updater"))]
#[tauri::command]
async fn check_update() -> Result<Option<String>, String> {
    Ok(None)
}

#[cfg(not(feature = "updater"))]
#[tauri::command]
async fn install_update() -> Result<(), String> {
    Err("This build has no updater.".into())
}

fn file_args(args: &[String]) -> Vec<String> {
    args.iter().skip(1).filter(|a| !a.starts_with('-') && Path::new(a).is_file()).cloned().collect()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("--engine") {
        engine::run(args.get(2).map(String::as_str).unwrap_or("{}"));
        return;
    }

    let builder = tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, argv, _cwd| {
            let paths = file_args(&argv);
            if let Some(host) = app.try_state::<Host>() {
                for p in &paths {
                    host.grant(Path::new(p));
                }
            }
            let files: Vec<J> = paths.iter().map(|p| describe(Path::new(p))).collect();
            if !files.is_empty() {
                let _ = app.emit("open-paths", files);
            }
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.unminimize();
                let _ = w.set_focus();
            }
        }))
        .plugin(tauri_plugin_dialog::init());

    #[cfg(feature = "updater")]
    let builder = builder.plugin(tauri_plugin_updater::Builder::new().build());

    builder
        .setup(move |app| {
            let base = app.path().app_local_data_dir()?;
            let work = base.join("work");
            let spill = base.join("spill");
            let _ = fs::remove_dir_all(&work);
            let _ = fs::remove_dir_all(&spill);
            fs::create_dir_all(&work)?;
            fs::create_dir_all(&spill)?;
            let sup = Supervisor::new(work.clone(), spill, base.join("cache"));
            sup.attach(app.handle().clone());
            let watched = Arc::new(Mutex::new(HashMap::new()));
            start_watcher(app.handle().clone(), watched.clone());
            app.manage(Host {
                sup,
                work,
                autosave: base.join("autosave.duckbench"),
                granted: Mutex::new(HashSet::new()),
                out_dirs: Mutex::new(HashSet::new()),
                watched,
                cancelled_copies: Arc::new(Mutex::new(HashSet::new())),
                launch: Mutex::new(file_args(&args)),
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            engine_info,
            db_query,
            db_cancel,
            db_cancel_copy,
            db_exec,
            watch_sources,
            export_xlsx,
            excel_sheets,
            excel_to_json,
            stage_file,
            work_write,
            work_read,
            work_drop,
            work_to_path,
            pick_files,
            pick_save_path,
            pick_folder,
            write_path,
            read_path,
            open_project,
            file_info,
            adopt_paths,
            autosave_write,
            autosave_read,
            autosave_clear,
            launch_paths,
            check_update,
            install_update
        ])
        .run(tauri::generate_context!())
        .expect("error while running Duckbench");
}
