# Duckbench — Windows desktop (Tauri 2 + native DuckDB)

Duckbench is a point-and-click data-cleaning workbench (Power-Query style) that compiles every step to DuckDB SQL. This repo is the **Windows-only native desktop app**. It uses the same UI as the web version but runs a native DuckDB engine written in Rust in its own process, so it has no browser-tab limits.

## Layout
```
app/src/index.html                  the app UI (also still runs in a browser)
app/src/platform/desktop.js         desktop bridge (does nothing outside Tauri)
app/src/vendor/xlsx-0.20.3.full.min.js   SheetJS (web version only; desktop reads Excel in Rust)
src-tauri/src/main.rs               host: window, dialogs, file access list, projects, autosave, commands
src-tauri/src/supervisor.rs         starts, talks to and restarts the engine process
src-tauri/src/engine.rs             engine process: DuckDB, main + background lanes, cancel
src-tauri/src/excel.rs              calamine Excel reader (xlsx/xlsm/xls/xlsb/ods)
src-tauri/src/xlsx_out.rs           streaming XLSX writer (rust_xlsxwriter, constant memory)
src-tauri/src/values.rs             DuckDB value → JSON (same shapes the UI already uses)
src-tauri/src/proto.rs              length-prefixed frames over the pipes
src-tauri/tauri.release.conf.json   signing + updater settings for release builds
.github/workflows/windows.yml       CI: web file check + Windows installer build on every push and PR
.github/workflows/release.yml       tag push (v*) builds the installer and publishes a GitHub release
```

## Build
1. Install Node 20+, Rust stable (MSVC), and the Visual Studio C++ Build Tools.
2. `npm install`, then `npm run icons`
3. `npm run dev` to develop, or `npm run build` to create the NSIS installer in `src-tauri/target/release/bundle/nsis/`.

The first build compiles DuckDB from source, which takes several minutes. Builds after that are cached.

## CI and pre-alpha releases
**CI** (`windows.yml`) runs on every push to `main`, every pull request, and on demand. The `web` job checks that the browser version's files are intact. The `desktop` job builds the NSIS installer on a Windows runner and uploads it as the `duckbench-windows-installer` artifact (kept 14 days). The first run compiles DuckDB from source and takes a while; later runs use the Rust cache.

**Releases** (`release.yml`) build the same installer and publish it on the repository's Releases page, with a SHA-256 checksum file. Push a tag to start one:
```
git tag v0.1.0-alpha.1
git push origin v0.1.0-alpha.1
```
- The version in the tag is written into the installer at build time, so you don't edit version numbers by hand.
- A tag with a suffix (`-alpha.1`, `-beta.1`, `-rc.1`) is published as a **pre-release**. A plain tag like `v0.1.0` is a normal release.
- It can also be started from the Actions tab (**Run workflow**) with a tag name.
- These builds are **unsigned and have no auto-update**. Windows SmartScreen shows a warning on first run.

### Signed release with auto-update (later)
`release.yml` does not sign or publish `latest.json` yet. These steps prepare for it; `npm run build:release` uses them locally.
1. `npm run updater-key` creates a key pair. Put the public key in `src-tauri/tauri.release.conf.json`, and store the private key and its password as the GitHub secrets `TAURI_SIGNING_PRIVATE_KEY` / `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`.
2. In the same file, replace `REPLACE_OWNER/REPLACE_REPO` with your GitHub repository.
3. For Azure Trusted Signing (removes the SmartScreen warning), set the secrets `AZURE_CLIENT_ID`, `AZURE_CLIENT_SECRET`, `AZURE_TENANT_ID` and the variables `AZURE_SIGNING_ENDPOINT`, `AZURE_SIGNING_ACCOUNT`, `AZURE_SIGNING_PROFILE`.

Local release build: `npm run build:release`. Plain `npm run build` includes no updater and needs no keys.

## How it works
**Two processes.** `duckbench.exe` opens the window. It starts a second copy of itself with `--engine`, and that copy is the only one that runs DuckDB. They talk over stdin/stdout using length-prefixed JSON frames.
- If DuckDB crashes, only the engine process dies. The supervisor fails any queries in flight, starts a new engine, and sends `engine-restarted`. The UI then reopens your source files by path, re-runs your steps, puts back the selected step and query, and shows "nothing was lost".
- If the window closes, the engine sees its input pipe close and exits, so no stray process is left running.

**Two lanes.** The engine keeps two connections to the same in-memory database:
- The **main** lane runs previews, the pipeline, imports and exports.
- The **background** lane runs the join match report and column profiling, so they never make the grid wait.
- Temporary TEMP tables became regular tables so both lanes can see them.

**Cancel.**
- Every query has a ticket. A newer preview automatically cancels the older one still running for the same view, using DuckDB's interrupt.
- Queries that take longer than 600 ms show a **Cancel query** button in the status bar. **Esc** also cancels.

**Files are read in place.**
- The native Open dialog returns real paths.
- CSV/TSV/Parquet files are hard-linked into the engine's work folder when they're on the same drive, so the file isn't copied at all. Otherwise they're copied by Rust. DuckDB reads them from there.
- Nothing goes through JavaScript memory, so multi-GB files work.
- Files dragged in from Explorer are streamed to disk in 8 MB pieces.

**Excel.**
- calamine reads .xlsx, .xlsm, .xls, .xlsb and .ods directly into newline-free JSON.
- It follows the same rules as the web importer: skip rows, drop blank rows, first row as header, empty headers become `columnN`, duplicate names get `_2`, dates become ISO text, and error cells become empty. The resulting columns match the web version.

**Exports stream to disk.**
- CSV, Parquet (ZSTD), JSON and JSONL are written with DuckDB `COPY` straight into the work folder, then moved to your chosen path. The CSV formula-injection guard and export column order stay the same.
- XLSX is written by rust_xlsxwriter in constant-memory mode, with real dates and numbers and the 1,048,575-row cap.
- Memory use stays flat whatever the row count.
- The file is written as `*.partial` and then renamed, so a failed export never leaves a broken file behind.

**Batch runs.** On desktop you choose an output folder once. Each result goes there as `<name>_transformed.csv`.

**Projects.**
- `.duckbench` files store the recipe plus source paths, size and modified time, but not the data itself. Ctrl+S saves, Ctrl+Shift+S saves as, and Ctrl+O opens.
- Recent projects are listed on the start screen.
- Double-clicking a `.duckbench` file opens it, in the window that's already running if there is one.
- If a source file has moved, the project offers **Locate file…** to relink it, and your steps are kept.
- The web version's IndexedDB session is replaced by an **autosave** file (`%LOCALAPPDATA%\com.duckbench.desktop\autosave.duckbench`). It's written with fsync and an atomic rename after every change, so a crash never loses your steps.

**Security.**
- The host keeps a list of allowed files: only files you picked, dropped, opened from a project, or passed in at launch. Every command that reads or writes checks against it.
- The engine sets `allowed_directories` to its work and spill folders, turns on `enable_external_access=false` and `lock_configuration=true`, and turns off extension installs.
- As a result, a Custom SQL step in an imported project can't read `C:\Users\…\anything`.
- This lockdown is tested at startup. If a DuckDB build doesn't support it, the engine runs without it and prints a warning.

**Virtualized grid (desktop).**
- The first 200 rows appear right away. Straight after, the full result is saved in the background as a table with a row-number column.
- The grid then renders only the rows on screen plus a buffer of about 30, in pages of 500 rows. At most 24 pages are kept in memory.
- Scrolling millions of rows keeps the page small. Pages not loaded yet show placeholder rows until they arrive.
- PgUp/PgDn and Ctrl+Home/End move through the whole result. Shift-click and Ctrl+A select across the whole result too, using row IDs fetched from the engine.
- The browser version still shows the first 200 rows.
- Mixed Arabic and English text in cells and headers displays in the right direction.

**Progress for long jobs.**
- A card at the bottom right appears after about 0.35 s. It shows a bar, percentage, elapsed time, time left, and **Cancel**.
- Copying a file shows bytes; Excel import and XLSX export show rows. Both report every 200 ms or so and check for cancel inside the loop.
- DuckDB `COPY` (CSV/Parquet/JSON exports) and table builds can't report progress from inside, so they show elapsed time with a moving bar. They can still be cancelled.

**Workspace cache (on-disk database).**
- The engine attaches `%LOCALAPPDATA%\com.duckbench.desktop\cache\workspace.duckdb`.
- Every import is saved there, keyed on file path, size, modified time, sheet, skip rows and header setting. Reopening a project or relaunching the app restores the table from the cache instead of re-reading the file.
- A changed file never matches an old cache entry, because its size or modified time differs.
- The 24 most recently used imports are kept. Older ones are dropped automatically.
- If the cache file is damaged, it's deleted and recreated.
- The on/off switch and **Clear cache** are in the help (?) panel.

**Source-file watcher.**
- The host checks the size and modified time of each open source file every 2 s. It waits for writes to finish before acting on a change.
- When a file changes, a banner offers **Refresh** or **Keep current data**. Refresh re-reads the file, clears the cached copies of later steps, and re-runs the steps.

**Files open in Excel.** If Excel has the file locked, Duckbench copies it to a snapshot with a progress bar and tells you it imported a snapshot.

**WebView2.** F5, Ctrl+R, P, G, U, J, H, N, T, W, Ctrl+zoom, Ctrl+wheel zoom, F3, F7 and the browser right-click menu are all blocked (except in text fields). The installer includes the WebView2 offline installer and installs for the current user only, with no admin rights.

## Join and dropdown fixes (shared by web and desktop)
- `normalizeDraft()` keeps what a dropdown shows equal to what's stored in the step, with automatic picks and a disabled "Choose…" placeholder. This fixes the soft-lock.
- "Add step" follows validation that runs on every change. **Retry loading columns** appears if loading them fails.
- Join keys are suggested automatically. A live match report shows "X of Y rows match" and a warning when repeated keys will multiply rows. It runs on the background lane on desktop.
- **Loose match** handles Arabic letter variants, diacritics, tatweel and Arabic-Indic digits. Text-vs-number keys are cast to text. The column profile has a **Join on this column…** button.

## Web entry points
- `app/src/index.html` opens the app in a browser (serve it over http).
- `app/src/index.html?selftest` runs the DuckDB-vs-Express parity self-tests.

## Not verified yet
- This code has **not been compiled here**. The Rust side needs its first `cargo build` on Windows. The likeliest fixes are small API-name mismatches in duckdb-rs, calamine 0.26, rust_xlsxwriter 0.80 or the Tauri 2 plugins.
- Things to test by hand: scrolling to the bottom of a 10M-row result; reopening a project shows "Reopening from workspace cache"; editing a source file in Excel and saving it brings up the Refresh banner; a 5 GB CSV opens and previews; Cancel stops a slow join; killing `duckbench.exe --engine` in Task Manager recovers the session; exports keep memory flat; a moved file can be relinked; double-clicking a `.duckbench` file opens it.

## Next steps
1. Arrow IPC instead of JSON for grid pages, to get more scroll speed on very wide tables.
2. Save the cached copies of later steps (checkpoints) in the workspace cache too, so reopening a long recipe skips the heavy steps.
3. Row-count progress for `COPY` exports, by splitting large exports into row ranges.

## Data
There's no server and no database service. Everything stays on the PC. Desktop keeps its autosave and recent-projects list under `%LOCALAPPDATA%`. The web version still uses IndexedDB.
