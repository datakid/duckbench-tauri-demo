(function () {
  const T = window.__TAURI__;
  if (!T || !T.core) return;
  const invoke = T.core.invoke;
  const listen = T.event && T.event.listen;
  const enc = new TextEncoder();
  const hdr = (h) => ({ headers: Object.fromEntries(Object.entries(h).map(([k, v]) => [k, encodeURIComponent(v)])) });
  const toBytes = async (x) => {
    if (x instanceof Uint8Array) return x;
    if (x instanceof ArrayBuffer) return new Uint8Array(x);
    if (typeof x === 'string') return enc.encode(x);
    return new Uint8Array(await x.arrayBuffer());
  };
  const asError = (e) => (e instanceof Error ? e : new Error(String(e)));
  const call = async (cmd, args, opts) => { try { return await invoke(cmd, args, opts); } catch (e) { throw asError(e); } };

  class NativeFile {
    constructor(info) {
      this.native = true;
      this.path = info.path;
      this.name = info.name;
      this.size = info.size || 0;
      this.lastModified = info.lastModified || 0;
      this.exists = info.exists !== false;
      this.type = '';
    }
    async arrayBuffer() { return await call('read_path', { path: this.path }); }
    async text() { return new TextDecoder().decode(await this.arrayBuffer()); }
    toJSON() { return { path: this.path, name: this.name, size: this.size, lastModified: this.lastModified }; }
  }

  function wrapResult(r) {
    const columns = r.columns || [];
    const rows = r.rows || [];
    return {
      numRows: rows.length,
      schema: { fields: columns.map((name) => ({ name })) },
      getChildAt: (i) => ({ length: rows.length, get: (k) => (rows[k] ? rows[k][i] : null) }),
      toArray: () => rows.map((row) => ({ toJSON: () => Object.fromEntries(columns.map((c, i) => [c, row[i]])) })),
    };
  }

  let seq = 0;
  const inflight = new Map();
  const progressHandlers = new Map();
  if (listen) listen('engine-progress', (e) => {
    const p = e.payload || {};
    const fn = progressHandlers.get(p.ticket);
    if (fn) fn(Number(p.done) || 0, Number(p.total) || 0);
  });
  async function tracked(cmd, args, onProgress, lane = 'main', kind = 'job') {
    const ticket = `${kind}${++seq}`;
    if (onProgress) progressHandlers.set(ticket, onProgress);
    inflight.set(ticket, { lane, kind });
    syncCancelButton();
    try { return await call(cmd, { ...args, ticket }); }
    finally {
      progressHandlers.delete(ticket);
      inflight.delete(ticket);
      syncCancelButton();
    }
  }
  const latestByKey = new Map();
  let cancelBtn = null;
  let cancelTimer = null;

  function mainInflight() { return [...inflight.values()].filter((q) => q.lane !== 'bg'); }
  function syncCancelButton() {
    const busy = mainInflight().length > 0;
    clearTimeout(cancelTimer);
    if (!busy) { if (cancelBtn) cancelBtn.hidden = true; return; }
    cancelTimer = setTimeout(() => {
      if (!mainInflight().length) return;
      if (!cancelBtn) {
        const bar = document.getElementById('statusBar');
        if (!bar) return;
        cancelBtn = document.createElement('button');
        cancelBtn.type = 'button';
        cancelBtn.className = 'statusbar-cancel';
        cancelBtn.textContent = 'Cancel query';
        cancelBtn.title = 'Stop the query that is running now (Esc)';
        cancelBtn.addEventListener('click', () => api.cancelAll());
        bar.appendChild(cancelBtn);
      }
      cancelBtn.hidden = false;
    }, 600);
  }

  async function runQuery(sql, opts = {}) {
    const lane = opts.lane === 'bg' ? 'bg' : 'main';
    const ticket = `t${++seq}`;
    if (opts.key) {
      const prev = latestByKey.get(opts.key);
      if (prev && inflight.has(prev)) invoke('db_cancel', { ticket: prev }).catch(() => {});
      latestByKey.set(opts.key, ticket);
    }
    inflight.set(ticket, { lane });
    syncCancelButton();
    try {
      return wrapResult(await call('db_query', { sql, lane, ticket }));
    } finally {
      inflight.delete(ticket);
      if (opts.key && latestByKey.get(opts.key) === ticket) latestByKey.delete(opts.key);
      syncCancelButton();
    }
  }

  const conn = {
    query: (sql, opts) => runQuery(sql, opts),
    exec: (sql, opts = {}) => tracked('db_exec', { sql, lane: opts.lane === 'bg' ? 'bg' : 'main' }, null, opts.lane === 'bg' ? 'bg' : 'main'),
    async close() {},
  };

  const db = {
    async registerFileBuffer(name, buffer) { await call('work_write', await toBytes(buffer), hdr({ 'x-name': name })); },
    async registerFileText(name, text) { await call('work_write', enc.encode(text), hdr({ 'x-name': name })); },
    async dropFile(name) { await call('work_drop', { name }); },
    async copyFileToBuffer(name) { return new Uint8Array(await call('work_read', { name })); },
    async terminate() {},
  };

  let workDir = '';
  const api = {
    NativeFile,
    async workPath(name) {
      if (!workDir) workDir = (await call('engine_info')).workDir || '';
      return `${workDir.replace(/[\\/]+$/, '')}\\${name}`;
    },
    isNativeFile: (f) => !!(f && f.native && f.path),
    async open() {
      const info = await call('engine_info');
      workDir = info.workDir || workDir;
      if (!info.locked) console.warn('Duckbench: DuckDB file-access lockdown is not active on this build.');
      return { db, conn, version: info.version, locked: info.locked, threads: info.threads, generation: info.generation, cache: !!info.cache };
    },
    cancelAll() {
      for (const [ticket, q] of inflight) {
        if (q.lane === 'bg') continue;
        if (q.kind === 'copy') invoke('db_cancel_copy', { ticket }).catch(() => {});
        else invoke('db_cancel', { ticket }).catch(() => {});
      }
    },
    cancelKey(key) {
      const t = latestByKey.get(key);
      if (t && inflight.has(t)) invoke('db_cancel', { ticket: t }).catch(() => {});
    },
    watch(paths) { invoke('watch_sources', { paths: paths.filter(Boolean) }).catch(() => {}); },
    async pickFiles({ multiple = false, kind = 'data' } = {}) {
      const list = await call('pick_files', { multiple, kind });
      return list.map((i) => new NativeFile(i));
    },
    async pickSavePath(name) { return await call('pick_save_path', { name }); },
    async pickFolder() { return await call('pick_folder'); },
    async fileInfo(path) { return new NativeFile(await call('file_info', { path })); },
    async adoptPaths(paths) { return (await call('adopt_paths', { paths })).map((i) => new NativeFile(i)); },
    async stage(file, name, onProgress) { return await tracked('stage_file', { path: file.path, name }, onProgress, 'main', 'copy'); },
    async streamToWork(file, name) {
      const CHUNK = 8 * 1024 * 1024;
      for (let off = 0; off < file.size || off === 0; off += CHUNK) {
        const part = new Uint8Array(await file.slice(off, off + CHUNK).arrayBuffer());
        await call('work_write', part, hdr(off === 0 ? { 'x-name': name } : { 'x-name': name, 'x-append': '1' }));
        if (!file.size) break;
      }
    },
    async excelSheets(file) { return await call('excel_sheets', { path: file.path }); },
    async excelToJson(file, name, opts = {}, onProgress) {
      return await tracked('excel_to_json', { path: file.path, name, sheet: opts.sheet || null, skipRows: Number(opts.skipRows) || 0, firstRowHeader: opts.firstRowHeader !== false }, onProgress);
    },
    async exportXlsx(sql, name, total, onProgress) { return await tracked('export_xlsx', { sql, name, total: Number(total) || 0 }, onProgress); },
    async workToPath(name, dest) { return await call('work_to_path', { name, dest }); },
    async writePath(path, data) { return await call('write_path', await toBytes(data), hdr({ 'x-path': path })); },
    async saveBlob(blob, name) {
      const dest = await call('pick_save_path', { name });
      if (!dest) return null;
      await call('write_path', await toBytes(blob), hdr({ 'x-path': dest }));
      return dest;
    },
    async openProject(path) {
      const r = await call('open_project', { path });
      return { text: r.text, file: r.file, files: r.files.map((i) => new NativeFile(i)) };
    },
    async autosaveWrite(text) { return await call('autosave_write', { text }); },
    async autosaveRead() {
      const r = await call('autosave_read');
      return r ? { text: r.text, files: r.files.map((i) => new NativeFile(i)) } : null;
    },
    async autosaveClear() { return await call('autosave_clear'); },
    async launchFiles() { return (await call('launch_paths')).map((i) => new NativeFile(i)); },
    async checkUpdate() { try { return await invoke('check_update'); } catch { return null; } },
    async installUpdate() { return await call('install_update'); },
    on(event, fn) { if (listen) listen(event, (e) => fn(e.payload)); },
  };
  window.duckbenchNative = api;

  document.documentElement.classList.add('is-desktop');
  setTimeout(() => { invoke('engine_info').then((i) => { workDir = i.workDir || workDir; }).catch(() => {}); }, 0);

  document.addEventListener('keydown', (e) => {
    const k = e.key.toLowerCase();
    const mod = e.ctrlKey || e.metaKey;
    if (k === 'f5' || k === 'f3' || k === 'f7' || (mod && ['r', 'p', 'g', 'u', 'j', 'h', 'n', 't', 'w', '+', '-', '=', '0'].includes(k))) e.preventDefault();
    if (k === 'escape' && mainInflight().length && cancelBtn && !cancelBtn.hidden) api.cancelAll();
  }, true);
  window.addEventListener('wheel', (e) => { if (e.ctrlKey) e.preventDefault(); }, { passive: false });
  document.addEventListener('contextmenu', (e) => {
    if (!e.target.closest('input, textarea, [contenteditable="true"]')) e.preventDefault();
  });
})();
