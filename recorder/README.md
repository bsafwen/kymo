# Kymo recorder

One small program that replaced windows_exporter, nvidia_gpu_exporter, VictoriaMetrics and Grafana on 2026-10-20.
It reads the PC every 10 seconds (by default), keeps its history on disk as long as its settings say, and
serves both the data and the dashboards on **http://127.0.0.1:21223**. It runs as the Windows service
**Kymo** (see `..\README.md`).

How it was built, in four phases:
1. The collector, storage and API, run next to the previous stack and compared figure by figure.
2. The dashboards.
3. The desktop card (`..\desktop\card\KymoCard.cs`), fed from memory.
4. The switch-over:
   - the service;
   - seek points in the history files;
   - the previous stack's history imported;
   - the previous stack removed.

## Desktop card
The card asks `/api/card` for everything it shows in one small answer (about 6 KB):
- **Current values:** the last 30 s.
- **5-minute averages.**
- **Newest sample.**
- **Last hour:** one point per minute.
- **Top apps:** by CPU over 5 minutes, and by memory and commit in the newest minute.
- **Status:** of the recording and its sources.
- **What to show:** the Desktop card settings (tiles, tiles per row, charts, lines, clock, opacity, lock), so a change on the Settings page reaches the card with its next answer. Its place and size are the card's own (`%LOCALAPPDATA%\Kymo\card-placement.txt`).

The program keeps these figures in memory: the last hour at 10 s and the last 5 minutes of apps. It reads them from the history once at start-up, which took 3 ms, so an answer reads no file. Each answer costs the recorder about 0.5 ms of CPU, almost all of it the HTTP connection.

The card asks once for each new sample, a second after the sample is due:
- **Normally:** every 10 s.
- **After 5 minutes without mouse or keyboard input:** every 30 s.
- **While it is covered, the PC is locked or the screen is off:** never.
- **While the recorder is unreachable or starting:** every 5 s.

The old card queried VictoriaMetrics 28 times every 2 s.

### Card status
- **`status`:**
  - `ok`;
  - `starting`: no sample yet, for at most a minute awake;
  - `resuming`: the newest sample only starts afresh after start-up or sleep, so the next one brings the figures back. Also when the PC slept since the newest sample. The recorder measures time awake without sleep (QueryUnbiasedInterruptTime), so a night asleep never reads as a stopped recorder;
  - `stale`: a minute awake without a sample.
- **Figures:**
  - The current ones (`now`, `avg5`, `last`, `top`) come only with a current sample. Otherwise the answer carries only the last-hour lines.
  - After the clock is set back, everything kept from the old timeline is dropped.
- **`sources`**, given while the newest sample is current:
  - `counters` (CPU and memory), `disk` (the system drive) and `gpu`: `ok` or `failing`;
  - `gpu` is `none` when there is no NVIDIA driver or GPU;
  - `apps`: `waiting` until a minute of apps is due.
- **`error`:** why a write to the history is failing. Samples and apps are tracked apart, and each clears with the next good write of its kind.
- **Missing counters and NVML:** counter sets missing when the recorder started, such as a provider not ready at boot, are tried again every 5 minutes on the monotonic clock. So are CPU power and NVML.

## Dashboards
Open **http://127.0.0.1:21223**. There are three pages, with the same panels and formulas as the Grafana dashboards:
- **This PC:**
  - **Right now:** 16 tiles.
  - **CPU:** usage, time by kind, each logical processor, effective clock, power and limit, waiting threads.
  - **Memory:** composition, commit, paging, kernel pools, page file.
  - **Disks:** throughput, operations, latency, active time, queue, free space.
  - **Network:** traffic, errors.
  - **GPU:** engines, VRAM, temperature and fan, power, clocks, why it slows down.
  - **System and recording:** processes, coverage, history size, the recorder's own cost, uptime.
- **Apps:** top apps by CPU (with "all other apps"), memory, I/O and commit, plus the table of every app in the range.
- **Process explorer:** pick apps (type a name) to see their CPU, CPU split, memory, commit, I/O and instances, plus the full app table.
- **Settings:** the recorder's settings (below), the history on disk per tier, and clearing history.

How to use them:
- Pick a range at the top, or drag across any chart to zoom; the browser's Back undoes a zoom.
- Purple dashed lines mark Windows starts.
- Pages refresh every 10 s for ranges up to 3 h, less often for longer ranges, and not at all while the tab is hidden (or at the pace the Display setting gives). The Settings page never refreshes by itself, and asks before you leave it with unsaved changes.
- App CPU is a % of the whole CPU (all logical processors), like Task Manager.

The pages are plain HTML, CSS and JavaScript in `ui\`, built into the program. The charts are drawn on canvas by the page's own code, with no libraries. They load nothing from the internet, and a Content-Security-Policy allows nothing but this origin.

To work on them without touching the recorder, use the read-only `serve` mode with the folder: `kymo.exe serve --data C:\ProgramData\Kymo\history --ui ui`.
- It listens on port 21224, and stops if it cannot. Reload the page after each edit.
- Run it from a copy of the program, not from `target\release`: a running copy there blocks the next build. `..\install.ps1` says so and stops.

## Running it
- **Install or update the service:** `pwsh -File ..\install.ps1`, run as yourself.
  - It builds and tests the program, then asks once for administrator rights.
  - It installs `C:\Program Files\Kymo\kymo.exe` as the service Kymo, with the history in `C:\ProgramData\Kymo\history`.
  - The service runs as LocalService with a service SID, starts automatically, and Windows restarts it after a failure.
- **Commands:**
  - `kymo service --data DIR [--port N]`: the service itself, which only the service control manager can start.
  - `kymo run --data DIR [--port N]`: the same in a console or a task, for tests. Only one recorder can run on the PC, because a `Global\` mutex stops a second one.
  - `kymo serve --data DIR [--port N] [--ui DIR]`: read-only, for the dashboards.
  - `kymo import --data DIR --file F [--replace]`: adds history exported by `import_vm.py` (see Storage). The recorder must be stopped.
  - `kymo snapshot` prints one sample as JSON.
  - `kymo probe` prints the memory each start-up step uses.
- **Hang watchdog:** if the recording loop stops moving for 3 minutes while the PC is awake, or the API thread ends, the process exits with code 3 and the service control manager starts it again.
- **Log:** `kymo.log` in the history folder, rotated at 1 MB.

## What it records

| Series | Source | What |
|---|---|---|
| `cpu.busy` `.user` `.privileged` `.interrupt` `.dpc` `.utility` `.clock` `.frequency` `.limit` | Performance counters (Processor Information) | Whole CPU. `busy` is what Task Manager shows; `utility` also counts turbo; `clock` is the effective MHz. |
| `cpu.core.NN.busy` `.utility` `.clock` | Same | Each logical processor |
| `sys.queue` `.switches` `.processes` `.threads` `.handles` `.boot` | Counters, process table | |
| `mem.*`: total, available, committed, commit_limit, free, modified, standby_*, pool_*, cache, pages_in, page_reads, pages_out, faults | Memory counters | |
| `pagefile.size` `.used` | System page file information | |
| `disk.<C:>.active` `.read_bps` `.write_bps` `.reads` `.writes` `.read_latency` `.write_latency` `.read_time` `.write_time` `.queue` `.size` `.free` | LogicalDisk counters, volume space | Each drive letter. Latency is per operation: none when idle. Over longer periods the API weights it by operations (`*_time` = seconds waited per second). |
| `net.<adapter>.rx_bps` `.tx_bps` `.bandwidth` `.rx_errors` `.tx_errors` `.rx_discards` `.tx_discards`, `net.rx_bps` `.tx_bps` | Network Interface counters | Each adapter, plus totals |
| `power.package` `.cores` | RAPL energy counters | CPU power in watts |
| `gpu.util` `.mem_util` `.enc_util` `.dec_util` `.mem_used` `.mem_total` `.mem_reserved` `.temp` `.power` `.power_limit` `.clock_*` `.fan` `.pstate` `.slow_*` | NVML, in-process | `slow_*` is the % of time each slowdown reason was active |
| `self.cpu` `.ws` `.private` `.collect` | This program | Its own cost, recorded like everything else |
| Apps, per name and minute: cpu, user, kernel (% of one core), read, write (bytes/s), ws, private (bytes), instances, seconds covered | Process table, one call | Same rules as the current stack's per-app history; the seconds covered weight partial minutes |

## The API
All endpoints answer GET requests on 127.0.0.1 only and return JSON.

| Endpoint | Returns |
|---|---|
| `/api/health` | `ok`, or `stale` with HTTP 503 when there has been no sample for 60 s |
| `/api/now`, `/api/series` | The latest sample, or only its keys and units |
| `/api/query?keys=cpu.busy,cpu.core.*.busy&from=&to=&step=` | Average and maximum per step. `*` matches within one part of a key. Up to 64 keys. |
| `/api/apps?from=&to=&sort=cpu\|cpu_max\|mem\|commit\|read\|write\|instances&limit=` | The apps table: CPU averaged over the whole range (time without data counts as 0, like the dashboards), peak CPU, peak memory and commit, data read and written, most instances |
| `/api/app?names=chrome,Code&field=cpu\|mem\|commit\|read\|write\|instances&from=&to=&step=` | Per-app series |
| `/api/stats` | Its own CPU and memory, collection times and history size |
| `/api/app?...&others=1` | Adds the series "all other apps", the sum of every app not named |
| `/api/card` | The desktop card's figures, from memory. Sections: `status`, `ts`, `interval_s`, `sources`, `error`, `limits` (the alert thresholds of the settings), `card` (the Desktop card settings without their `card_` prefix: tiles, columns, charts, issue_lines, top_apps, status, clock, opacity, locked), `processes` (the Process list settings without their `procs_` prefix: show, pid, cpu, memory, io, gpu, threads, handles, commit, group, interval, locked), `now` (last 30 s, or the newest sample at longer intervals), `avg5`, `last`, `hour` (60 one-minute points) and `top` (the app with the most CPU over 5 minutes, memory and commit now, and over 5 minutes the most I/O as `io`, bytes/s from 1 MB/s, and the most hard page faults as `faults`, per second with `share`, its % of the PC's page reads, named only from a fifth of them; faults are counted in memory only). See "Card status" below |
| `/api/settings` | The settings page's whole table: groups of fields with their kind (choice, range, switch), choices or bounds, default and value; the rules between fields; the drives the recorder sees |
| `/api/history` | Per tier: its files (period start, end, bytes), size and retention; the total, the ceiling, the free space, the folder, and whether this is a read-only copy |
| POST `/api/settings` | Form fields `key=value`: checked one by one and against the rules, then applied and saved. Answers the new table, or 400 with `error` and `field` |
| POST `/api/settings/defaults` | Every setting back to its default |
| POST `/api/history/delete` | `scope=all\|system\|apps` and `before=<Unix seconds, a day ago or earlier>` or `before=all`: deletes whole files whose period ended by then (all of them, the files being written included, with `all`). Answers the files and bytes deleted |

**Parameters:**
- Times are Unix seconds. `to` defaults to now, `from` to an hour earlier, `step` to about 300 points.
- Bucket *i* covers (start + i·step, start + (i+1)·step].

**Safety:**
- A request whose Host header is not `127.0.0.1:21223` or `localhost:21223` gets 403. Web pages using DNS rebinding cannot read it.
- A POST needs the header `X-Kymo: 1`, which a page on another site cannot send without the browser asking this server first (and it never agrees), and, when the browser sends an `Origin`, this origin. Otherwise 403. The read-only `serve` mode answers every POST with 409.

## Settings
- **Where:** `settings.conf` in the history folder, one `key = value` per line, written whole and renamed into place. A missing file, or a missing or invalid line, means the default; values that break a rule together fall back to their defaults (the log says which).
- **One table** in `src\settings.rs` holds every field: its group, label, help, kind and default. The Settings page is drawn from it, so it can only offer valid values; the rules between fields (retention of a finer tier at most that of the coarser one, orange below red) are checked by the page, which moves the tied field, and again by the recorder.
- **Saving** sends only the fields changed; the recording loop applies them to the settings in force (so two saves close together both count) and checks the rules again. If the loop does not take a change up within 30 s (60 s for a deletion), the page is told nothing was done, and the change is withdrawn, never done later.
- **Applied at once** by the recording loop:
  - the interval from the next sample (the card, the freshness checks and the gap detection follow it);
  - the sources: per-core and per-adapter series, NVML (unloaded when off), CPU power and apps;
  - retention: a shorter one deletes what it no longer keeps right away; then hourly as before;
  - the ceiling (GB = 2^30 bytes, as Windows counts): the oldest file of the first tier in the order samples, apps per minute, minutes, apps per hour, hours, until under it; the files being written are never deleted.
- Every change is logged with its old and new values.

## Storage
- **Files:** append-only, one per UTC period, in the history folder:
  - 10-second samples per day (`raw\`);
  - 1-minute summaries per day (`1m\`) and 1-hour summaries per month (`1h\`), each with average, maximum and the time their samples stand for (in 10-second units). Each sample weighs the time since the one before, so a stretch recorded at 1 s does not outweigh one at 10 s;
  - apps per minute per day (`apps\`) and per hour per month (`apps-1h\`), weighted by the seconds of data.
- **Encoding:** rows are delta-encoded varints, at a fixed resolution per unit, for example 0.01 % and 1 kB.
- **Mixed intervals:** each sample row stores the time it stands for (the interval it was taken at, in tenths of a second). Rows written before the interval became a setting hold 1; for them the time since the sample before counts (when that is a plausible step, at most a minute), else the pace so far. So after the interval changes, older samples still fill the finer buckets of a chart, while gaps stay gaps.
- **Seek points** (format version 3, in every file started since phase 4):
  - **What they are:** a RESET record sets the delta values back to zero before every hour of rows (every day in hourly files).
  - **How queries use them:** a query reads only record headers up to its range, then starts at the last RESET before it.
  - **Gain:** the last 30 s of a full day costs 2.3 ms instead of 22 ms (`cargo test --release -- --ignored --nocapture bench_seek`).
  - **After a clock change:** a RESET also records the latest row time so far. A query starts only where no earlier row is later than its range, so it stays correct after the clock goes back.
  - **Older files:** version 2 files are still read, and appended to without RESET records.
- **History imported from the previous stack:**
  - `python -I import_vm.py --out F [--until T]` exports every figure per minute from VictoriaMetrics, with the hourly rows made from them, for the system and the apps. It stops before this program's first minute.
    - Each minute is exactly (t - 60 s, t]: the per-app counters are flushed on each minute mark, so a 60 s window holds one flush.
    - A minute in which windows_exporter restarted (its rtc and mperf counters start again) has no CPU utility or clock rather than a wrong one.
    - Adapters are named as the recorder names them; one it never saw is left out and listed.
  - `kymo import` writes every file it changes again whole, rows in time order, and keeps the files as they were in a folder of each run's own, `<history>.before-import-<time>`. Times already present are skipped, so running it twice adds nothing; `--replace` lets the file's rows take the place of those inside its span (to redo an import).
  - The hour the recorder started in holds minutes of both: its hourly row is made again from all its minutes (also by a run after one that stopped halfway). If that hour is not over yet, the import says to run it again afterwards.
  - A query whose 10-second tier has no rows at the start of its range takes those minutes and hours from the coarser tiers, so the imported period shows at every zoom. Only a coarse row with no finer row anywhere in its period counts, so the partial hour written before a sleep never fills the gap after it.
  - **Check of the export (2026-10-20)** against an hour both stacks recorded:
    - system figures within 1 %, clocks within 8 % (bursty readings);
    - apps within a few percent on the same minutes; minute to minute, summed app CPU follows the system's CPU (correlation 0.76 of the changes).
- **Crash safety:**
  - Every record has a length and a CRC-32. A record cut short by a power cut is dropped when the file is reopened.
  - A damaged record with data after it is copied aside (`*.tail-<time>`) before the file is cut back; an unreadable file is set aside whole (`*.bad-<time>`). Set-aside files go with their period's retention, and first when the history is over its ceiling.
  - In a v3 file, a reader that meets a damaged record goes on at the next seek point, so every query of a day returns the same rows.
  - Summaries are rebuilt from the finer data at start-up, and a summary that could not be written is written again later.
- **Retention:** set per tier (Settings). Every hour, at most one file per tier whose period ended longer ago than its retention is deleted, and the ceiling is applied; right after the retention is shortened on the Settings page, everything it no longer keeps goes at once. Deletion is held back:
  - for the rest of the run after the clock jumps ahead by more than a day;
  - for a week when the newest data at start-up is more than 30 days old.
- **Clock changes:** times are UTC. A sample that comes across a clock change, or more than 25 s after the previous one (sleep), only starts afresh. Rates use a monotonic clock that counts sleep.

## Cost
Measured on this PC.
- **One sample:** about 6–11 ms for performance counters, 4–8 ms for 480 processes and 0.5 ms for the GPU, every 10 s. That is about 0.15–0.2 % of one CPU core.
- **Memory:** about 30 MB private. NVIDIA's NVML library alone is 20 MB, performance counters about 7 MB, the program 3 MB.
- **No dependencies:** Windows functions are declared by hand in `src\win.rs`. Building downloads nothing.

## First comparison (2026-10-20, 10:30–11:25, 5-minute buckets)
- **Figures:** 64 of 67 match: CPU, power, memory, page file, both disks (latency included), network, GPU readings and boot time. The other three:
  - **GPU graphics and memory clocks:** "close" and "different". Both sides see the same states (memory 405, 810, 5001, 10501 MHz). Single readings taken at different moments land on the high states more or less often.
  - **Processor queue:** 5.5 against 2.9. windows_exporter reads it during its own scrape, when its collectors are running: it sees an empty queue 18 % of the time, the trial 44 %.
- **Apps:** they match within a few percent. Short-lived processes (python, java) count higher in the trial, because it reads processes every 10 s against every 30 s, so fewer brief processes are missed.
- **Cost:**

  | | CPU, % of one core | Memory |
  |---|---|---|
  | Current stack | 2.8 % + about 0.26 % for the `nvidia-smi` runs | 868 MB |
  | Trial | 0.19 % | 23 MB working set, 31 MB private |

- **History:** about 0.3 MB per hour.

## Dashboard cost (2026-10-20)
- **One refresh of This PC, 3-hour range:** 4 requests. The main one asks for 56 key patterns in a single request; keep the list at 64 or fewer.
- **Per refresh:** about 25 ms of the recorder's CPU and 460 KB of JSON.
- **Overall:** at one refresh every 10 s, that is about 0.25 % of one core, only while the tab is open and visible. Grafana with VictoriaMetrics costs 3 % all the time.
