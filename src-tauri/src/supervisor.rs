use std::collections::HashMap;
use std::io::BufReader;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde_json::{json, Value as J};
use tauri::{AppHandle, Emitter};

use crate::proto::{read_frame, write_frame};

const RESTARTED: &str = "The engine stopped unexpectedly and was restarted. Your steps are safe \u{2014} Duckbench is reloading your data.";

struct Live {
    stdin: ChildStdin,
    child: Child,
    generation: u64,
}

pub struct Supervisor {
    work: PathBuf,
    spill: PathBuf,
    cache: PathBuf,
    live: Mutex<Option<Live>>,
    next_id: AtomicU64,
    generation: AtomicU64,
    pending: Arc<Mutex<HashMap<u64, Sender<J>>>>,
    tickets: Mutex<HashMap<String, u64>>,
    info: Mutex<J>,
    app: OnceLock<AppHandle>,
}

impl Supervisor {
    pub fn new(work: PathBuf, spill: PathBuf, cache: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            work,
            spill,
            cache,
            live: Mutex::new(None),
            next_id: AtomicU64::new(1),
            generation: AtomicU64::new(0),
            pending: Arc::new(Mutex::new(HashMap::new())),
            tickets: Mutex::new(HashMap::new()),
            info: Mutex::new(J::Null),
            app: OnceLock::new(),
        })
    }

    pub fn attach(&self, app: AppHandle) {
        let _ = self.app.set(app);
    }

    pub fn info(self: &Arc<Self>) -> Result<J, String> {
        self.ensure()?;
        let mut info = self.info.lock().unwrap_or_else(|e| e.into_inner()).clone();
        info["generation"] = json!(self.generation.load(Ordering::SeqCst));
        Ok(info)
    }

    fn spawn(self: &Arc<Self>) -> Result<Live, String> {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let cfg = json!({ "work": self.work, "spill": self.spill, "cache": self.cache }).to_string();
        let mut cmd = Command::new(exe);
        cmd.arg("--engine").arg(cfg).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000);
        }
        let mut child = cmd.spawn().map_err(|e| format!("Couldn\u{2019}t start the engine: {e}"))?;
        let stdin = child.stdin.take().ok_or("Engine has no input pipe")?;
        let mut stdout = BufReader::new(child.stdout.take().ok_or("Engine has no output pipe")?);
        let hello: J = read_frame(&mut stdout)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .ok_or("The engine exited during start-up.")?;
        if hello["ok"] != json!(true) {
            let _ = child.kill();
            return Err(hello["error"].as_str().unwrap_or("The engine failed to start.").to_string());
        }
        *self.info.lock().unwrap_or_else(|e| e.into_inner()) = hello["result"].clone();
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let me = self.clone();
        std::thread::spawn(move || me.read_loop(stdout, generation));
        Ok(Live { stdin, child, generation })
    }

    fn ensure(self: &Arc<Self>) -> Result<(), String> {
        let mut live = self.live.lock().unwrap_or_else(|e| e.into_inner());
        if live.is_none() {
            *live = Some(self.spawn()?);
        }
        Ok(())
    }

    fn read_loop(self: Arc<Self>, mut stdout: BufReader<ChildStdout>, generation: u64) {
        while let Ok(buf) = read_frame(&mut stdout) {
            let Ok(msg) = serde_json::from_slice::<J>(&buf) else { continue };
            let Some(id) = msg["id"].as_u64() else { continue };
            if msg["event"].as_str() == Some("progress") {
                self.forward_progress(id, &msg);
                continue;
            }
            let tx = self.pending.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
            if let Some(tx) = tx {
                let _ = tx.send(msg);
            }
        }
        self.on_exit(generation);
    }

    fn forward_progress(&self, id: u64, msg: &J) {
        let Some(app) = self.app.get() else { return };
        let ticket = self
            .tickets
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find(|(_, v)| **v == id)
            .map(|(k, _)| k.clone());
        if let Some(t) = ticket {
            let _ = app.emit("engine-progress", json!({ "ticket": t, "done": msg["done"], "total": msg["total"] }));
        }
    }

    fn on_exit(self: &Arc<Self>, generation: u64) {
        {
            let mut live = self.live.lock().unwrap_or_else(|e| e.into_inner());
            match live.as_mut() {
                Some(l) if l.generation == generation => {
                    let _ = l.child.kill();
                    let _ = l.child.wait();
                    *live = None;
                }
                _ => return,
            }
        }
        let failed: Vec<Sender<J>> = self.pending.lock().unwrap_or_else(|e| e.into_inner()).drain().map(|(_, v)| v).collect();
        for tx in failed {
            let _ = tx.send(json!({ "ok": false, "error": RESTARTED, "restarted": true }));
        }
        self.tickets.lock().unwrap_or_else(|e| e.into_inner()).clear();
        std::thread::sleep(Duration::from_millis(250));
        let ok = self.ensure().is_ok();
        if let Some(app) = self.app.get() {
            let _ = app.emit("engine-restarted", json!({ "ok": ok }));
        }
    }

    pub fn request(self: &Arc<Self>, mut req: J, ticket: Option<String>) -> Result<J, String> {
        self.ensure()?;
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        req["id"] = json!(id);
        let (tx, rx) = channel();
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).insert(id, tx);
        if let Some(t) = &ticket {
            self.tickets.lock().unwrap_or_else(|e| e.into_inner()).insert(t.clone(), id);
        }
        let sent = {
            let mut live = self.live.lock().unwrap_or_else(|e| e.into_inner());
            match live.as_mut() {
                Some(l) => write_frame(&mut l.stdin, &serde_json::to_vec(&req).unwrap_or_default()).is_ok(),
                None => false,
            }
        };
        if !sent {
            self.pending.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
        }
        let reply = if sent { rx.recv().ok() } else { None };
        if let Some(t) = &ticket {
            self.tickets.lock().unwrap_or_else(|e| e.into_inner()).remove(t);
        }
        let reply = reply.unwrap_or_else(|| json!({ "ok": false, "error": RESTARTED, "restarted": true }));
        if reply["ok"] == json!(true) {
            Ok(reply["result"].clone())
        } else {
            Err(reply["error"].as_str().unwrap_or("Unknown engine error").to_string())
        }
    }

    pub fn cancel(self: &Arc<Self>, ticket: &str) {
        let Some(target) = self.tickets.lock().unwrap_or_else(|e| e.into_inner()).get(ticket).copied() else { return };
        let msg = json!({ "op": "cancel", "target": target });
        let mut live = self.live.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(l) = live.as_mut() {
            let _ = write_frame(&mut l.stdin, &serde_json::to_vec(&msg).unwrap_or_default());
        }
    }
}
