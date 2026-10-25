# Kymo guide

One small program, `kymo` (Rust, no dependencies, source in `recorder\`), runs as the Windows service **Kymo**. It does three things:
- **Records:** it reads the whole PC (CPU, memory, disks, network, GPU, power and every app) every 10 seconds by default, and keeps its history in `C:\ProgramData\Kymo\history`.
- **Serves the dashboards:** **http://127.0.0.1:21223** shows This PC, Apps, Process explorer and Settings.
- **Feeds the desktop card:** the live status card on your desktop (top right until you drag it elsewhere).

It was written to replace a stack of four services (windows_exporter, nvidia_gpu_exporter, VictoriaMetrics and Grafana). On the same PC that stack used about 3 % of a CPU core and 870 MB of memory; this program uses about 0.2 % and 30 MB.

## Always running
- **At boot:** the service starts before anyone signs in (automatic start, not delayed).
- **After a crash:** Windows starts it again after 5 s, then 10 s, then every minute.
- **If it hangs:** if its recording loop stops moving for 3 minutes while the PC is awake (a driver call that never returns), or its web server stops, it ends itself and Windows starts it again.
- **Missing readings:** readings missing at boot are tried again every 5 minutes. These are performance counters, CPU power and the GPU driver.
- **Not recorded:** nothing is recorded while the PC is off or asleep. Those periods show as gaps.
- **Check it after any reboot:** `pwsh -File .\verify.ps1`, no admin rights needed. It shows:
  - the service and its recovery settings;
  - how many seconds after boot recording started;
  - every gap of the last 7 days with its cause (PC off, asleep, or not explained).
- **Prove the recovery:** `test-resilience.ps1`, run elevated. It kills the service and checks that Windows brings it back, then stops and starts it.

## On your desktop
- **Live status card**, top right at first:
  - a green **ALL GOOD** bar, or orange or red **N ISSUES** with one line per problem and the app most responsible;
  - six tiles, each with the last 30 seconds and the last hour: CPU, memory, commit, GPU, disk C: and network;
  - the top apps by CPU and memory, and whether recording is OK. *Memory Compression* is never the top app by memory: it holds other apps' memory, compressed when RAM runs short (Task Manager leaves it out of its Processes tab too).
  - It updates with each new sample (every 10 s by default), or every 30 s after 5 minutes without input. It pauses while it is covered, the PC is locked or the screen is off.
  - **Show desktop** (Win+D) brings it to the front.
  - Click it to open the dashboards. Right-click it to open its settings (below, under Desktop card).
  - **Drag it** to move it anywhere on the screen. It keeps its distance from the nearest corner, so it stays put when the taskbar or the scaling changes, and it grows away from that corner when issue lines appear. Near its usual margin or a screen edge it snaps to it.
  - **Resize it** by an edge or a corner (the pointer shows arrows there):
    - **An edge** stretches it that way only, wider or taller; the opposite edge stays put. The tiles and their charts take the room, and the text keeps its size. It never gets narrower or shorter than it is made, nor runs past the end of the screen (room is kept for all its issue lines), and within 8 px of its made size it goes back to it.
    - **A corner**, or Ctrl + scroll over it, scales the whole card, text included, from 60 % up to 250 % of its usual size, never bigger than fits on the screen with all its issue lines; the opposite corner stays put, and it stops at 100 % when you pass close to it. If a later change makes it too big (another layout in the settings), it is drawn smaller until it fits again.
  - **Its transparency:** hold Shift and scroll over it: its background gets 5 % more or less opaque a notch, from 40 to 100 % (the text stays solid). It is the card's *Background opacity* setting, so the Settings page shows the change and the card keeps it after a restart. If the recorder does not answer, the card keeps it and sends it again once the recorder is back.
  - Its place, size and stretch are kept in `%LOCALAPPDATA%\Kymo\card-placement.txt`. Delete that file to put the card back top right at its usual size.
  - **What it shows** (tiles, tiles per row, charts, issue lines, the lines below the tiles, the clock, opacity) is set on the Settings page, under Desktop card; the card follows with its next sample (every 10 s by default). There you can also lock its place and size. The card keeps its last settings (`card-look.txt`, next to its place), so it starts as you set it up even before the recorder answers.
- **Process list**, next to the card at first: every process of this PC, as Task Manager's Processes tab shows them.
  - Columns: name (with the program's icon), PID, CPU, memory, I/O and GPU; threads, handles and commit can be added. Over each column, the PC's total: CPU and memory in %, I/O in MB/s, GPU in %. Cells are shaded the more a process uses: CPU against a quarter of the PC, memory and commit against a tenth of the RAM, I/O against 50 MB/s, GPU against half of it.
    - **CPU:** its share of all the CPU, as Task Manager counts it.
    - **Memory:** its private working set, the memory only it uses that is in RAM now (Task Manager's Memory column). *Memory Compression* is not in the list, as in Task Manager's Processes tab: it holds other apps' memory, compressed. Its size shows in the list's title instead ("6.0 GB compressed"), when the list is wide enough (a narrow list keeps the time first).
    - **I/O:** what it reads and writes per second, files, devices and the network together, in MB/s (GB/s from 1000 MB/s). Windows tells a program run as the user what each process reads and writes, but not which part of it goes to the disk: this is not Task Manager's Disk column.
    - **GPU:** its busiest GPU engine (3D, video, copy and so on), from the same counters as Task Manager.
    - Network and power use per process are not shown: Windows gives them only to programs running as administrator.
  - **One row per app:** the processes of one program make one row, with their count, added up. Click it to see each process under it, with its PID; click it again to fold it. Services that do not let a user read their program show under the name of their file in System32 (for example *Host Process for Windows Services*).
  - **Sort:** click a column title; again for the other way. It is sorted by memory at first, the biggest first.
  - **More rows:** scroll over it; pull its bottom edge to show more at once.
  - It is read again every 2 s by default, and only while it can be seen, like the card. It costs about a millisecond each time.
  - **Move, stretch, scale and transparency** as the card: drag it to move it; an edge stretches it (a wider name column, more rows), a corner or Ctrl + scroll scales all of it; Shift + scroll sets its opacity (its own, 85 % at first). Its place, size, opacity and sort are kept in `%LOCALAPPDATA%\Kymo\processes-placement.txt`; delete it to put the list back next to the card.
  - **What it shows** (shown or not, its columns, one row per app, how often it is read, the lock of its place and size) is set on the Settings page, under Process list. Right-click it to go there.
- **What counts as an issue** (the defaults; change them on the Settings page, under Alerts; the dashboards' tiles use the same):

  | Check | Orange | Red |
  |---|---|---|
  | CPU busy, 5-minute average | 75 % | 90 % |
  | Memory in use | 85 % | 92 % |
  | Commit, compared with the most it can reach (RAM + the most the system-managed page file can grow to) | 85 % | 95 % |
  | Paging (pages read per second, 5 min) | 1,500 | 5,000 |
  | Free space on the card's disk (C: unless set otherwise) | under 25 GB | under 10 GB |
  | Disk latency, 5 min | 50 ms | 200 ms |
  | GPU temperature | 78 °C | 83 °C |

  Also flagged:
  - the disk busy over 90 % for 5 minutes;
  - GPU memory over 95 %;
  - GPU thermal or hardware slowdown;
  - CPU performance limit under 90 %;
  - network errors;
  - in red:
    - the recorder unreachable, or without a sample after a minute awake;
    - a write to the history failing;
    - performance counters, disk or app readings failing;
    - GPU readings failing while the NVIDIA driver is installed;
  - in orange, for a few seconds: the recorder starting, or resuming after sleep.

  **The app behind an issue**, at the end of its line ("top: ..."), where one app can be named:
  - CPU busy: the busiest app over the same 5 minutes. Memory in use: the app using the most (private working set). Commit: the app with the most private bytes.
  - Heavy paging: the app with the most hard page faults over 5 minutes, with its share of the PC's page reads ("top: X 60 %"). A hard fault is one read from the disk, the page file or a mapped file, of one page or a few, so it is compared with the PC's page reads, not with the pages/s figure. An app is named only when it makes at least a fifth of the reads: reads through the file cache (a big file copy, a scan) are charged to no app by Windows, and the line then names none rather than a wrong one. The recorder counts faults per app in memory only, so it names one from about a minute after it starts.
  - Never *Memory Compression*, for memory, commit or paging: what it holds is other apps' memory, compressed, and its hard faults read that memory back from the page file. The next app is named instead, if one qualifies.
  - Disk busy or slow: the app reading and writing the most over 5 minutes ("top I/O"), from 1 MB/s. Windows counts an app's I/O for files (cached reads included), devices and the network together, and none for paging, so it points at the likely app rather than proving it was this disk.
  - GPU hot, or its memory full: the app busiest on the GPU (a fifth of it at least), or with the most GPU memory (256 MB at least), from the counters Task Manager uses, on the GPU the temperature and memory are about (the one with the most dedicated memory in use; an integrated GPU's apps are left out). The card reads GPU use while the GPU is warm or busy and for 5 minutes after, so the app is known when the issue shows; GPU memory while it is full.
  - The others (a disk almost full, the CPU held back by power or heat, a GPU slowdown, network errors) have no one app behind them, and name none.
- **The dashboards open by themselves** in your default browser after you sign in.
- **Setup:** two tasks under your account, "Kymo desktop card" and "Kymo open dashboards". Set them up with `pwsh -File desktop\install-desktop.ps1`, without admin rights; `install.ps1` runs it too.
- **Card log:** `%LOCALAPPDATA%\Kymo\card.log`.

## The dashboards
http://127.0.0.1:21223 has four pages.
- **This PC:** the right-now tiles, then CPU (overall, per logical processor, clocks, power and limit), memory, disks, network, GPU, and the recorder's own coverage and cost.
- **Apps:** the busiest apps by CPU, memory, I/O and commit, and a table of every app in the range. *Memory Compression* is left out of the memory and commit charts (it holds other apps' memory, compressed); the table still lists it, marked as such.
- **Process explorer:** pick apps to see their CPU, memory, commit, I/O and instances over time.
- **Settings:** see below.

How to use them:
- Pick a range, or drag across a chart to zoom. Back undoes a zoom.
- Purple dashed lines mark Windows starts.
- App CPU is a % of the whole CPU, like Task Manager.

The details of every series, the API and the file format are in `recorder\README.md`.

## Settings
http://127.0.0.1:21223/#/settings. Every field offers only valid values, each shows its default, and fields tied together move with each other: shorten the hourly summaries below the 1-minute ones and the minutes follow, with a note saying so. The recorder checks every change again before it applies and saves it.

| Group | Fields | Defaults |
|---|---|---|
| Recording | how often the PC is read (1 s to 1 min, always a divisor of a minute); each logical processor, each network adapter, the NVIDIA GPU, CPU power and apps on or off | every 10 s, all on |
| Retention | how long each tier is kept; a ceiling for the whole history (when it is reached the oldest files go first: samples, then apps per minute, minutes, apps per hour, hours) | samples 90 days, 1-minute summaries 2 years, hourly 10 years, apps per minute 6 months, apps per hour 10 years; 10 GB |
| Alerts | the orange and red thresholds of the card and the tiles (table above) | as above |
| Desktop card | each of the six tiles on or off; the disk it shows; 1, 2 or 3 tiles per row; the last-hour charts; how many issue lines (0 to 6, red ones first); the top apps line, the status line and the clock; background opacity (40 to 100 %); lock its place and size | all tiles; the system drive; 2 per row; charts on; 6 lines; all lines and the clock on; 85 %; unlocked |
| Process list | shown on the desktop or not; the PID, CPU, memory, I/O, GPU, threads, handles and commit columns; one row per app; read every 1, 2 or 5 s; its place and size locked | shown; PID, CPU, memory, I/O and GPU; one row per app; every 2 s; not locked |
| Display | how often the dashboards refresh | automatic |

- **Saving** applies at once, without a restart: a new interval from the next sample, a shorter retention right away. Before you save, the page says which files a shorter retention deletes and estimates how big the history will grow.
- **Clear history:** everything, the system or the apps; older than a week up to 2 years, or all of it. It deletes whole files (days, months for the hourly ones) and asks twice.
- **History on disk:** each tier's files, size, period and retention, the total, the free space and the folder.
- The settings live in `C:\ProgramData\Kymo\history\settings.conf`. A missing or invalid line means the default.

## History
- Recorded every 10 seconds by default, with 1-minute and 1-hour summaries.
- Each file covers one day (one month for the hourly ones). How long each kind is kept is set under Settings; the oldest files go first.
- History from a VictoriaMetrics stack can be imported (per minute and per hour): see "Storage" in `recorder\README.md`. Zooming into an imported period shows minute values.
- **Size:** the Settings page shows what the history takes now and what it will settle at with the chosen retention.

## Install, update, remove
- **Install or update:** `pwsh -File .\install.ps1`. Run it as yourself, not elevated.
  - It builds and tests the program, asks once for administrator rights (UAC), installs or updates the service, then sets up the desktop tasks.
  - Add `-Test` to run the resilience test after it.
  - The history is kept.
  - `-RemoveOldStack` is only for migrating from the earlier Grafana-based stack: it uninstalls Grafana, VictoriaMetrics and the two exporters.
- **Remove:** `pwsh -File .\uninstall.ps1`, elevated. It removes only the recorder's own files. Add `-DeleteHistory` to delete the history too.
- **Install log:** `C:\ProgramData\Kymo\logs`.
- **Recorder log:** `C:\ProgramData\Kymo\history\kymo.log`.

## Where things are

| Path | What |
|---|---|
| This repository | `install.ps1`, `uninstall.ps1`, `verify.ps1`, `test-resilience.ps1`, `desktop\` (the card, the process list and their installer), `recorder\` (the program's source and README) |
| `C:\Program Files\Kymo\kymo.exe` | The installed program (only administrators can change it) |
| `C:\ProgramData\Kymo\history\` | The history and the recorder's log (written by the service only; readable by you) |
| `%LOCALAPPDATA%\Kymo\card.log` | The log of the desktop card and the process list |
| `%LOCALAPPDATA%\Kymo\card-placement.txt`, `processes-placement.txt` | Where the card and the process list sit, their size (and the list's opacity and sort) |
| `%LOCALAPPDATA%\Kymo\card-look.txt` | The card's and the list's last settings, for a start before the recorder answers |

## Security
- Everything listens on 127.0.0.1 only, and no firewall rule is added.
- The API answers only requests addressed to 127.0.0.1 or localhost, which blocks DNS rebinding.
- Changes (settings, clearing history) are accepted only from the dashboards' own page: a POST with the header `X-Kymo: 1` and, when the browser sends one, this origin. A page on another site cannot send them.
- The dashboards load nothing from the internet, and a Content-Security-Policy allows only this origin.
- The service runs as LocalService, with its own service SID that may write only its history folder. If a reading is missing under that account, the installer moves it to LocalSystem and says so; `C:\ProgramData\Kymo\install-state.json` records which.
