// KymoCard: the live status card of this PC, drawn on the desktop.
//
// A card-sized window with per-pixel transparency (UpdateLayeredWindow) kept just above the desktop,
// below every other window. It reads the recorder (Kymo, http://127.0.0.1:21223/api/card: one
// small answer from the recorder's memory, last-hour lines included) once each new sample is in, every
// 10 seconds (every 30 seconds after 5 minutes without input), and only while it can be seen: covered
// by windows, locked, or with the screen off, it neither fetches nor draws. Click the card to open the
// dashboards, right-click it for its settings (what it shows: the recorder's Settings page, see Look). Drag
// it to move it, drag its edges or Ctrl + scroll over it to resize it (its place and size:
// %LOCALAPPDATA%\Kymo\card-placement.txt, see Spot). Built with the C# compiler that ships with Windows
// (C# 5, .NET Framework 4.8): see build.ps1.
// Log: %LOCALAPPDATA%\Kymo\card.log
//
// KymoCard.exe                      run (one instance per user session)
// KymoCard.exe --snapshot out.png [key=value ...]   render one card to a PNG and exit (for checks), with
//                                   settings to try (as in /api/card "card": columns=3 charts=false)
// KymoCard.exe --selftest <folder>  check moving the card with a second card (exit code: failed checks)
// KymoCard.exe --trace              also log every fetch and every pause/resume
// KymoCard.exe --trace --always-visible   fetch and draw even while covered (to check the timing)

using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.Drawing;
using System.Drawing.Drawing2D;
using System.Drawing.Imaging;
using System.Drawing.Text;
using System.Globalization;
using System.IO;
using System.Linq;
using System.Net;
using System.Runtime.InteropServices;
using System.Text;
using System.Threading;
using System.Web.Script.Serialization;
using System.Windows.Forms;
using Microsoft.Win32;

namespace Kymo
{
    static class Native
    {
        [StructLayout(LayoutKind.Sequential)] public struct POINT { public int X, Y; public POINT(int x, int y) { X = x; Y = y; } }
        [StructLayout(LayoutKind.Sequential)] public struct SIZE { public int CX, CY; public SIZE(int cx, int cy) { CX = cx; CY = cy; } }
        [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
        [StructLayout(LayoutKind.Sequential, Pack = 1)] public struct BLENDFUNCTION { public byte Op, Flags, Alpha, Format; }
        [StructLayout(LayoutKind.Sequential)] public struct WINDOWPOS { public IntPtr Hwnd, InsertAfter; public int X, Y, CX, CY; public uint Flags; }
        [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
        public struct MONITORINFO { public int Size; public RECT Monitor, Work; public uint Flags; }
        [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
        public struct MONITORINFOEX { public int Size; public RECT Monitor, Work; public uint Flags; [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 32)] public string Device; }
        [StructLayout(LayoutKind.Sequential)] public struct LASTINPUTINFO { public int Size; public uint Time; }
        [StructLayout(LayoutKind.Sequential)] public struct POWERBROADCAST_SETTING { public Guid Setting; public int Length; public int Data; }

        public delegate void WinEventProc(IntPtr hook, uint ev, IntPtr hwnd, int obj, int child, uint thread, uint time);
        public delegate bool MonitorEnumProc(IntPtr mon, IntPtr dc, IntPtr rect, IntPtr data);

        [DllImport("user32.dll")] public static extern bool UpdateLayeredWindow(IntPtr h, IntPtr dst, ref POINT pt, ref SIZE sz, IntPtr src, ref POINT srcPt, int key, ref BLENDFUNCTION bf, int flags);
        [DllImport("user32.dll")] public static extern IntPtr GetDC(IntPtr h);
        [DllImport("user32.dll")] public static extern int ReleaseDC(IntPtr h, IntPtr dc);
        [DllImport("gdi32.dll")] public static extern IntPtr CreateCompatibleDC(IntPtr dc);
        [DllImport("gdi32.dll")] public static extern bool DeleteDC(IntPtr dc);
        [DllImport("gdi32.dll")] public static extern IntPtr SelectObject(IntPtr dc, IntPtr o);
        [DllImport("gdi32.dll")] public static extern bool DeleteObject(IntPtr o);
        [DllImport("gdi32.dll")] public static extern IntPtr CreateRectRgn(int l, int t, int r, int b);
        [DllImport("gdi32.dll")] public static extern int CombineRgn(IntPtr dst, IntPtr a, IntPtr b, int mode);
        [DllImport("user32.dll")] public static extern IntPtr GetTopWindow(IntPtr h);
        [DllImport("user32.dll")] public static extern IntPtr GetWindow(IntPtr h, uint cmd);
        [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
        [DllImport("user32.dll")] public static extern bool IsIconic(IntPtr h);
        [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
        [DllImport("user32.dll", EntryPoint = "GetWindowLongPtrW")] public static extern IntPtr GetWindowLongPtr(IntPtr h, int i);
        [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr h, IntPtr after, int x, int y, int cx, int cy, uint flags);
        [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr h, int cmd);
        [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern IntPtr FindWindow(string cls, string title);
        [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern int GetClassName(IntPtr h, StringBuilder s, int n);
        [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
        [DllImport("user32.dll")] public static extern IntPtr MonitorFromPoint(POINT pt, uint flags);
        [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern bool GetMonitorInfo(IntPtr mon, ref MONITORINFO info);
        [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern bool GetMonitorInfo(IntPtr mon, ref MONITORINFOEX info);
        [DllImport("user32.dll")] public static extern bool EnumDisplayMonitors(IntPtr dc, IntPtr clip, MonitorEnumProc proc, IntPtr data);
        [DllImport("user32.dll")] public static extern uint GetMessagePos();
        [DllImport("user32.dll")] public static extern bool GetCursorPos(out POINT pt);
        [DllImport("user32.dll")] public static extern IntPtr SendMessage(IntPtr h, int msg, IntPtr w, IntPtr l);
        [DllImport("shcore.dll")] public static extern int GetDpiForMonitor(IntPtr mon, int type, out uint dpiX, out uint dpiY);
        [DllImport("user32.dll")] public static extern bool SetProcessDpiAwarenessContext(IntPtr ctx);
        [DllImport("user32.dll")] public static extern bool GetLastInputInfo(ref LASTINPUTINFO info);
        [DllImport("user32.dll")] public static extern IntPtr SetWinEventHook(uint min, uint max, IntPtr mod, WinEventProc proc, uint pid, uint tid, uint flags);
        [DllImport("user32.dll")] public static extern bool UnhookWinEvent(IntPtr hook);
        [DllImport("user32.dll")] public static extern IntPtr RegisterPowerSettingNotification(IntPtr h, ref Guid setting, int flags);
        [DllImport("wtsapi32.dll")] public static extern bool WTSRegisterSessionNotification(IntPtr h, int flags);
        [DllImport("dwmapi.dll")] public static extern int DwmGetWindowAttribute(IntPtr h, int attr, out int value, int size);
        [DllImport("dwmapi.dll")] public static extern int DwmGetWindowAttribute(IntPtr h, int attr, out RECT value, int size);
        [DllImport("user32.dll")] public static extern bool GetLayeredWindowAttributes(IntPtr h, out uint key, out byte alpha, out uint flags);
        [DllImport("wtsapi32.dll", EntryPoint = "WTSQuerySessionInformationW")] public static extern bool WTSQuerySessionInformation(IntPtr server, int session, int infoClass, out IntPtr buffer, out int bytes);
        [DllImport("wtsapi32.dll")] public static extern void WTSFreeMemory(IntPtr p);
        [DllImport("kernel32.dll")] public static extern IntPtr GetCurrentProcess();
        [DllImport("kernel32.dll")] public static extern bool SetProcessInformation(IntPtr process, int infoClass, ref uint info, int size);

        public const int GWL_STYLE = -16, GWL_EXSTYLE = -20;
        public const long WS_EX_TRANSPARENT = 0x20, WS_EX_TOOLWINDOW = 0x80, WS_EX_LAYERED = 0x80000, WS_EX_NOACTIVATE = 0x8000000, WS_EX_TOPMOST = 0x8;
        public const uint GW_HWNDNEXT = 2, GW_HWNDPREV = 3, GW_OWNER = 4;
        public const uint LWA_COLORKEY = 1, LWA_ALPHA = 2;
        public const uint SWP_NOSIZE = 0x1, SWP_NOMOVE = 0x2, SWP_NOZORDER = 0x4, SWP_NOACTIVATE = 0x10, SWP_SHOWWINDOW = 0x40, SWP_NOOWNERZORDER = 0x200;
        public static readonly IntPtr HWND_TOP = IntPtr.Zero, HWND_BOTTOM = new IntPtr(1), HWND_TOPMOST = new IntPtr(-1), HWND_NOTOPMOST = new IntPtr(-2);
        public const int NULLREGION = 1, RGN_DIFF = 4;
        public const int DWMWA_EXTENDED_FRAME_BOUNDS = 9, DWMWA_CLOAKED = 14;

        public static string ClassOf(IntPtr h) { var s = new StringBuilder(256); GetClassName(h, s, 256); return s.ToString(); }
        public static long ExStyle(IntPtr h) { return GetWindowLongPtr(h, GWL_EXSTYLE).ToInt64(); }
        public static bool Cloaked(IntPtr h) { int c; return DwmGetWindowAttribute(h, DWMWA_CLOAKED, out c, 4) == 0 && c != 0; }
        public static bool Shown(IntPtr h) { return IsWindowVisible(h) && !IsIconic(h) && !Cloaked(h); }
        public static RECT Bounds(IntPtr h)
        {
            RECT r;
            if (DwmGetWindowAttribute(h, DWMWA_EXTENDED_FRAME_BOUNDS, out r, Marshal.SizeOf(typeof(RECT))) != 0) GetWindowRect(h, out r);
            return r;
        }
        // Other desktop widgets and click-through or see-through overlays: not windows that hide ours.
        public static bool IsWidgetOrOverlay(IntPtr h)
        {
            long ex = ExStyle(h);
            if ((ex & WS_EX_TRANSPARENT) != 0) return true;
            if ((ex & WS_EX_TOOLWINDOW) != 0 && (ex & WS_EX_NOACTIVATE) != 0) return true;
            if ((ex & WS_EX_LAYERED) == 0) return false;
            // Layered: see-through, unless it is an app window drawn fully opaque (some Electron apps set alpha 255).
            if ((ex & WS_EX_TOOLWINDOW) != 0 || GetWindow(h, GW_OWNER) != IntPtr.Zero) return true;
            uint key, flags; byte alpha;
            if (!GetLayeredWindowAttributes(h, out key, out alpha, out flags)) return true;   // per-pixel content (UpdateLayeredWindow)
            return (flags & LWA_COLORKEY) != 0 || ((flags & LWA_ALPHA) != 0 && alpha < 255);
        }
        public static bool IsDesktop(IntPtr h) { string c = ClassOf(h); return c == "Progman" || c == "WorkerW"; }

        public static IntPtr PrimaryMonitor() { return MonitorFromPoint(new POINT(0, 0), 1); }   // MONITOR_DEFAULTTOPRIMARY
        public static IntPtr MonitorAt(Point p) { return MonitorFromPoint(new POINT(p.X, p.Y), 2); }   // MONITOR_DEFAULTTONEAREST
        public static string DeviceOf(IntPtr mon)
        {
            var mi = new MONITORINFOEX { Size = Marshal.SizeOf(typeof(MONITORINFOEX)) };
            return GetMonitorInfo(mon, ref mi) ? mi.Device ?? "" : "";
        }
        // The monitor with this device name (\\.\DISPLAY1), or IntPtr.Zero if it is not connected.
        public static IntPtr FindMonitor(string device)
        {
            if (string.IsNullOrEmpty(device)) return IntPtr.Zero;
            IntPtr found = IntPtr.Zero;
            MonitorEnumProc each = (mon, dc, rect, data) =>
            {
                if (string.Equals(DeviceOf(mon), device, StringComparison.OrdinalIgnoreCase)) { found = mon; return false; }
                return true;
            };
            EnumDisplayMonitors(IntPtr.Zero, IntPtr.Zero, each, IntPtr.Zero);
            GC.KeepAlive(each);
            return found;
        }

        // The session's real lock state (WTSSessionInfoEx). Lock notifications only report later changes.
        public static bool SessionLocked()
        {
            IntPtr buf; int bytes;
            if (!WTSQuerySessionInformation(IntPtr.Zero, -1, 25, out buf, out bytes)) return false;
            try { return bytes >= 20 && Marshal.ReadInt32(buf, 0) == 1 && Marshal.ReadInt32(buf, 16) == 0; }   // level 1; SessionFlags 0 = locked
            finally { WTSFreeMemory(buf); }
        }
    }

    // Intervals use a monotonic clock: the wall clock jumps (summer time ends, time sync), which would stall the refresh.
    static class Clock
    {
        static readonly Stopwatch Watch = Stopwatch.StartNew();
        public static double Now { get { return Watch.Elapsed.TotalSeconds; } }
    }

    sealed class Issue { public string Level, Text; public Issue(string l, string t) { Level = l; Text = t; } }

    // What the card shows and how: the recorder's settings (the dashboards' Settings page, Desktop card), sent with
    // /api/card as "card"; these defaults until it sends them. Its place and size are the card's own (Spot).
    sealed class Look
    {
        public static readonly string[] TileKeys = { "cpu", "memory", "commit", "gpu", "disk", "network" };
        public readonly bool[] Tiles = { true, true, true, true, true, true };
        public int Columns = 2, IssueLines = 6, Opacity = 85;
        public bool Charts = true, TopApps = true, Status = true, Clock = true, Locked;

        static string B(bool v) { return v ? "true" : "false"; }

        // As kept in card-look.txt: key=value lines with /api/card's keys.
        public string ToText()
        {
            var sb = new StringBuilder("# The Kymo card's settings as the recorder last sent them (Settings page, Desktop card): the card\r\n# starts with them until the recorder answers. Change them on the Settings page, not here.\r\n");
            for (int i = 0; i < TileKeys.Length; i++) sb.Append(TileKeys[i] + "=" + B(Tiles[i]) + "\r\n");
            sb.Append("columns=" + Columns + "\r\nissue_lines=" + IssueLines + "\r\nopacity=" + Opacity + "\r\ncharts=" + B(Charts) + "\r\ntop_apps=" + B(TopApps) +
                "\r\nstatus=" + B(Status) + "\r\nclock=" + B(Clock) + "\r\nlocked=" + B(Locked) + "\r\n");
            return sb.ToString();
        }

        // key=value pairs (lines of a file, or --snapshot arguments) as JSON would give them: true/false, whole numbers, text.
        public static Dictionary<string, object> Pairs(IEnumerable<string> lines)
        {
            var m = new Dictionary<string, object>();
            foreach (var line in lines)
            {
                int eq = line.IndexOf('=');
                if (line.StartsWith("#") || eq < 0) continue;
                string k = line.Substring(0, eq).Trim(), v = line.Substring(eq + 1).Trim(); bool b; int n;
                m[k] = bool.TryParse(v, out b) ? (object)b : int.TryParse(v, NumberStyles.Integer, CultureInfo.InvariantCulture, out n) ? (object)n : v;
            }
            return m;
        }

        public static Look Read(string file)
        {
            try { return File.Exists(file) ? From(Pairs(File.ReadAllLines(file))) : new Look(); }
            catch (Exception e) { WidgetWindow.Log("reading the card's settings: " + e.Message); return new Look(); }
        }

        public static Look From(Dictionary<string, object> m)
        {
            var l = new Look();
            Func<string, bool, bool> flag = (k, d) => { object o; return m.TryGetValue(k, out o) && o is bool ? (bool)o : d; };
            Func<string, int, int, int, int> num = (k, d, min, max) =>
            {
                object o;
                if (!m.TryGetValue(k, out o) || !(o is int || o is long || o is decimal || o is double)) return d;
                double v = Convert.ToDouble(o, CultureInfo.InvariantCulture);
                return v >= min && v <= max && v == Math.Floor(v) ? (int)v : d;
            };
            for (int i = 0; i < TileKeys.Length; i++) l.Tiles[i] = flag(TileKeys[i], true);
            l.Columns = num("columns", 2, 1, 3);
            l.IssueLines = num("issue_lines", 6, 0, 6);
            l.Opacity = num("opacity", 85, 40, 100);
            l.Charts = flag("charts", true); l.TopApps = flag("top_apps", true); l.Status = flag("status", true);
            l.Clock = flag("clock", true); l.Locked = flag("locked", false);
            return l;
        }
    }

    sealed class Snapshot
    {
        public bool Up = true;
        public readonly Dictionary<string, double> V = new Dictionary<string, double>();
        public readonly List<Issue> Issues = new List<Issue>();
        public string TopCpu = "", TopMem = "";
        public Dictionary<string, double[]> Series = new Dictionary<string, double[]>();
        public string Drive = "C:";
        public string Status = "";             // the recorder's: ok, starting, resuming, stale
        public bool Reached;                   // the recorder answered
        public string Failure = "";            // no usable answer: busy, misread or unreachable
        public double LastSampleTs = double.NaN;   // after a failure: the newest sample seen before it
        public bool Saving = true;             // its writes to the history succeed
        public double SampleTs = double.NaN;   // Unix seconds of the newest sample
        public double Interval = 10;           // seconds between samples
        public DateTime At = DateTime.Now;     // the newest sample's time, local
        public string GpuState = "";           // the recorder's GPU source: ok, failing or off (switched off in its settings)
        // The alert thresholds: the recorder's settings (/api/card "limits"), these defaults until it sends them.
        public readonly Dictionary<string, double> Limits = new Dictionary<string, double>(Source.DefaultLimits);
        public Look Look = new Look();
        public bool HasLook;                   // the answer carried the card's settings (else the last ones stay)
        public double Get(string k) { double v; return V.TryGetValue(k, out v) ? v : double.NaN; }
        public double Lim(string k) { double v; return Limits.TryGetValue(k, out v) ? v : double.NaN; }
    }

    // Reads the recorder's card figures and decides what counts as an issue, with the thresholds of the
    // recorder's settings (the dashboards' Settings page, Alerts).
    sealed class Source
    {
        public const string Base = "http://127.0.0.1:21223";
        public static readonly Dictionary<string, double> DefaultLimits = new Dictionary<string, double> {
            { "cpu_warn", 75 }, { "cpu_crit", 90 }, { "mem_warn", 85 }, { "mem_crit", 92 }, { "commit_warn", 85 }, { "commit_crit", 95 },
            { "paging_warn", 1500 }, { "paging_crit", 5000 }, { "free_warn", 25 }, { "free_crit", 10 }, { "latency_warn", 50 },
            { "latency_crit", 200 }, { "gpu_temp_warn", 78 }, { "gpu_temp_crit", 83 }, { "disk_busy", 90 }, { "vram_full", 95 }, { "perf_limit", 90 },
        };
        static readonly CultureInfo Inv = CultureInfo.InvariantCulture;
        static readonly DateTime Epoch = new DateTime(1970, 1, 1, 0, 0, 0, DateTimeKind.Utc);
        readonly JavaScriptSerializer json = new JavaScriptSerializer { MaxJsonLength = 4 * 1024 * 1024 };
        double fixedPagefileMax = -1;          // bytes; -1 = system-managed (grows on demand)
        double configAt = -1e9;

        // The card's values: its name for each, then where /api/card has it ("now" = last 30 s, "avg5" = last
        // 5 minutes, "last" = newest sample) and the recorder's name for it.
        static readonly string[,] Figures = {
            { "cpu", "now", "cpu" }, { "cpu5", "avg5", "cpu" }, { "cpuW", "now", "cpu_power" }, { "limit5", "avg5", "cpu_limit" },
            { "mem", "now", "mem_used_pct" }, { "memTotal", "last", "mem_total" }, { "committed", "last", "committed" },
            { "commitLimit", "last", "commit_limit" }, { "pagefileLimit", "last", "pagefile_size" }, { "paging5", "avg5", "pages_in" },
            { "gpu", "now", "gpu" }, { "gpuTemp", "now", "gpu_temp" }, { "vram", "now", "vram_pct" }, { "gpuSlow", "now", "gpu_slow" },
            { "disk", "now", "disk_active" }, { "disk5", "avg5", "disk_active" }, { "readLat5", "avg5", "disk_read_latency_ms" },
            { "writeLat5", "avg5", "disk_write_latency_ms" }, { "diskFree", "last", "disk_free" }, { "diskSize", "last", "disk_size" },
            { "netIn", "now", "net_rx" }, { "netOut", "now", "net_tx" }, { "netErr5", "avg5", "net_errors" },
        };

        public static double UnixNow() { return (DateTime.UtcNow - Epoch).TotalSeconds; }
        public static DateTime Local(double unixSeconds) { return Epoch.AddSeconds(unixSeconds).ToLocalTime(); }

        static readonly Dictionary<string, string> Friendly = new Dictionary<string, string> {
            { "qemu-system-x86_64-headless", "Android emulator" }, { "qemu-system-x86_64", "Android emulator" }, { "MsMpEng", "Defender" },
            { "studio64", "Android Studio" }, { "msedgewebview2", "WebView2" }, { "Memory Compression", "Memory compression" },
        };
        static string Short(string name)
        {
            string f; if (Friendly.TryGetValue(name, out f)) return f;
            return name.Length > 20 ? name.Substring(0, 19) + "\u2026" : name;
        }

        static string Get(string path)
        {
            var req = (HttpWebRequest)WebRequest.Create(Base + path);
            req.Proxy = null; req.Timeout = 5000; req.ReadWriteTimeout = 5000;
            using (var resp = req.GetResponse())
            using (var r = new StreamReader(resp.GetResponseStream()))
                return r.ReadToEnd();
        }

        static Dictionary<string, object> Map(Dictionary<string, object> m, string k)
        {
            object o; return m != null && m.TryGetValue(k, out o) ? o as Dictionary<string, object> : null;
        }
        static double Num(Dictionary<string, object> m, string k)
        {
            object o; return m != null && m.TryGetValue(k, out o) && o != null ? Convert.ToDouble(o, Inv) : double.NaN;
        }
        static string Text(Dictionary<string, object> m, string k)
        {
            object o; return m != null && m.TryGetValue(k, out o) && o != null ? Convert.ToString(o, Inv) : "";
        }

        void RefreshConfig()
        {
            if (Clock.Now - configAt < 600) return;
            configAt = Clock.Now;
            // Page file: "?:\pagefile.sys" or a size of "0 0" means system-managed (it grows on demand).
            try
            {
                var v = Registry.GetValue(@"HKEY_LOCAL_MACHINE\SYSTEM\CurrentControlSet\Control\Session Manager\Memory Management", "PagingFiles", null) as string[];
                double max = 0; bool managed = false;
                if (v != null)
                    foreach (var line in v)
                    {
                        var parts = line.Trim().Split(new[] { ' ' }, StringSplitOptions.RemoveEmptyEntries);
                        if (parts.Length == 0) continue;
                        if (parts[0].StartsWith("?:") || parts.Length < 3 || (parts[1] == "0" && parts[2] == "0")) managed = true;
                        else max += double.Parse(parts[2], Inv) * 1048576;
                    }
                fixedPagefileMax = managed ? -1 : max;
            }
            catch { fixedPagefileMax = -1; }
        }

        public Snapshot Fetch()
        {
            RefreshConfig();
            var s = new Snapshot();
            try
            {
                string body = Get("/api/card");
                s.Reached = true;
                var root = (Dictionary<string, object>)json.DeserializeObject(body);
                double interval = Num(root, "interval_s");
                if (interval >= 1) s.Interval = interval;
                string drive = Text(root, "drive");
                if (drive.Length > 0) s.Drive = drive;
                s.SampleTs = Num(root, "ts");
                if (!double.IsNaN(s.SampleTs)) s.At = Local(s.SampleTs);
                var limits = Map(root, "limits");
                if (limits != null)
                    foreach (var k in DefaultLimits.Keys) { double v = Num(limits, k); if (!double.IsNaN(v)) s.Limits[k] = v; }
                var look = Map(root, "card");
                if (look != null) { s.Look = Look.From(look); s.HasLook = true; }

                // Starting: no sample yet. Resuming: after start-up or sleep, the next sample brings the figures back.
                s.Status = Text(root, "status");
                double age = Num(root, "age_s");
                if (s.Status == "starting") s.Issues.Add(new Issue("warn", "Recording is starting"));
                else if (s.Status == "resuming") s.Issues.Add(new Issue("warn", "Recording resumes"));
                else if (s.Status != "ok")
                {
                    s.Up = false;
                    s.Issues.Add(new Issue("crit", age < 0 ? "Recording waits: the clock went back" : "Recording stopped: no sample for " + Ago(age)));
                }
                var error = Map(root, "error");
                if (error != null) { s.Saving = false; s.Issues.Add(new Issue("crit", "Recording error: " + Text(error, "text"))); }
                var sources = Map(root, "sources");
                bool counters = Text(sources, "counters") != "failing";
                if (!counters) s.Issues.Add(new Issue("crit", "Performance counters failing: no CPU or memory readings"));
                if (counters && Text(sources, "disk") == "failing") s.Issues.Add(new Issue("crit", "Disk " + s.Drive + " readings failing"));
                s.GpuState = Text(sources, "gpu");
                if (s.GpuState == "failing") s.Issues.Add(new Issue("crit", "GPU readings failing (NVIDIA driver)"));
                if (Text(sources, "apps") == "failing") s.Issues.Add(new Issue("crit", "App readings failing: no top apps"));

                for (int i = 0; i < Figures.GetLength(0); i++)
                {
                    double v = Num(Map(root, Figures[i, 1]), Figures[i, 2]);
                    if (!double.IsNaN(v) && !double.IsInfinity(v)) s.V[Figures[i, 0]] = v;
                }
                foreach (var k in new[] { "netIn", "netOut" }) if (s.V.ContainsKey(k)) s.V[k] *= 8;   // bytes to bits

                // Commit against the most it can reach: RAM plus how far the page file can still grow.
                double ram = s.Get("memTotal"), committed = s.Get("committed");
                double commitMax = s.Get("commitLimit");
                if (fixedPagefileMax < 0 && !double.IsNaN(ram))
                {
                    double growTo = Math.Min(3 * ram, s.Get("diskSize") / 8);
                    double canGrow = s.Get("pagefileLimit") + s.Get("diskFree");
                    double pf = Math.Min(growTo, double.IsNaN(canGrow) ? growTo : canGrow);
                    if (!double.IsNaN(pf)) commitMax = ram + pf;
                }
                else if (fixedPagefileMax >= 0 && !double.IsNaN(ram)) commitMax = ram + fixedPagefileMax;
                if (!double.IsNaN(commitMax) && commitMax > 0) { s.V["commitMax"] = commitMax; if (!double.IsNaN(committed)) s.V["commitOfMax"] = 100 * committed / commitMax; }

                var top = Map(root, "top");
                Func<string, string> topText = (k) => { var a = Map(top, k); double v = Num(a, "value"); return a == null || double.IsNaN(v) ? "" : Short(Text(a, "name")) + " " + (k == "cpu" ? Pct(v) : Bytes(v)); };
                s.TopCpu = topText("cpu");
                s.TopMem = topText("mem");
                var topCommit = Map(top, "commit");
                AddIssues(s, Num(topCommit, "value"), Short(Text(topCommit, "name")));
                s.Series = Lines(Map(root, "hour"), s.Get("commitMax"));
            }
            catch (Exception e)
            {
                // Nothing of a failed or misread answer is shown.
                bool answered = s.Reached;
                var web = e as WebException;
                var resp = web != null ? web.Response as HttpWebResponse : null;
                var inner = e; while (inner.InnerException != null) inner = inner.InnerException;
                bool busy = resp != null && (int)resp.StatusCode == 503;
                if (resp != null) resp.Close();
                s = new Snapshot { Up = false, Failure = busy ? "busy" : answered ? "misread" : "unreachable" };
                s.Issues.Add(new Issue("crit", busy ? "Recorder busy: too many requests at once"
                    : (answered ? "Recorder answer not understood: " : "Recorder unreachable: ") + inner.Message));
            }
            return s;
        }

        // The last-hour lines, one point per minute: pairs (fraction of the hour at the minute's end, value);
        // minutes without data are left out. Commit is drawn as a % of the most it can reach.
        static Dictionary<string, double[]> Lines(Dictionary<string, object> hour, double commitMax)
        {
            var result = new Dictionary<string, double[]>();
            if (hour == null) return result;
            var lines = new[] {
                Tuple.Create("cpu", "cpu", 1.0), Tuple.Create("mem", "mem_used_pct", 1.0),
                Tuple.Create("commit", "committed", commitMax > 0 ? 100 / commitMax : double.NaN),
                Tuple.Create("gpu", "gpu", 1.0), Tuple.Create("disk", "disk_active", 1.0), Tuple.Create("net", "net_rx", 8.0),
            };
            foreach (var line in lines)
            {
                object o;
                if (double.IsNaN(line.Item3) || !hour.TryGetValue(line.Item2, out o)) continue;
                var values = o as object[];
                if (values == null || values.Length == 0) continue;
                var pts = new List<double>();
                for (int i = 0; i < values.Length; i++)
                {
                    if (values[i] == null) continue;
                    pts.Add((i + 1.0) / values.Length);
                    pts.Add(Convert.ToDouble(values[i], Inv) * line.Item3);
                }
                result[line.Item1] = pts.ToArray();
            }
            return result;
        }

        static string Ago(double s)
        {
            if (double.IsNaN(s)) return "a while";
            return s < 120 ? Whole(s) + " s" : s < 7200 ? Whole(s / 60) + " min" : Whole(s / 3600) + " h";
        }

        // Shown values are truncated, not rounded: every threshold is a whole number (or a 0.1 GB step), so the text
        // reaches a threshold only once the value does (84.6 % memory shows 85 % only when it turns orange).
        public static string Whole(double v) { return Math.Floor(v).ToString("0", Inv); }
        public static string Pct(double v) { return Whole(v) + " %"; }
        public static string Bytes(double b)
        {
            return b >= 1073741824 ? (Math.Floor(b / 1073741824 * 10) / 10).ToString("0.0", Inv) + " GB" : Whole(b / 1048576) + " MB";
        }
        public static string Bits(double b)
        {
            if (b >= 1e9) return (b / 1e9).ToString("0.0", Inv) + " Gb/s";
            if (b >= 1e6) return (b / 1e6).ToString("0.0", Inv) + " Mb/s";
            return (b / 1e3).ToString("0", Inv) + " kb/s";
        }
        public static string Level(double v, double warn, double crit) { return v >= crit ? "crit" : v >= warn ? "warn" : "ok"; }

        static void AddIssues(Snapshot s, double topCommit, string topCommitProc)
        {
            double v;
            const double GB = 1073741824;
            v = s.Get("cpu5"); if (!double.IsNaN(v) && v >= s.Lim("cpu_warn")) s.Issues.Add(new Issue(Level(v, s.Lim("cpu_warn"), s.Lim("cpu_crit")), "CPU busy " + Pct(v) + " for 5 min" + Top(s.TopCpu)));
            v = s.Get("mem"); if (!double.IsNaN(v) && v >= s.Lim("mem_warn")) s.Issues.Add(new Issue(Level(v, s.Lim("mem_warn"), s.Lim("mem_crit")), "Memory " + Pct(v) + " in use" + Top(s.TopMem)));
            v = s.Get("commitOfMax");
            if (!double.IsNaN(v) && v >= s.Lim("commit_warn"))
                s.Issues.Add(new Issue(Level(v, s.Lim("commit_warn"), s.Lim("commit_crit")), "Commit " + Bytes(s.Get("committed")) + " of at most " + Bytes(s.Get("commitMax")) +
                    (double.IsNaN(topCommit) ? "" : Top(topCommitProc + " " + Bytes(topCommit)))));
            v = s.Get("paging5"); if (!double.IsNaN(v) && v >= s.Lim("paging_warn")) s.Issues.Add(new Issue(Level(v, s.Lim("paging_warn"), s.Lim("paging_crit")), "Heavy paging: " + Whole(v) + " pages/s read from disk"));
            v = s.Get("diskFree"); if (!double.IsNaN(v) && v < s.Lim("free_warn") * GB) s.Issues.Add(new Issue(v < s.Lim("free_crit") * GB ? "crit" : "warn", s.Drive + " almost full: " + Bytes(v) + " free"));
            v = s.Get("disk5"); if (!double.IsNaN(v) && v >= s.Lim("disk_busy")) s.Issues.Add(new Issue("warn", "Disk " + s.Drive + " busy " + Pct(v) + " for 5 min"));
            double lat = Math.Max(Nz(s.Get("readLat5")), Nz(s.Get("writeLat5")));
            if (lat >= s.Lim("latency_warn")) s.Issues.Add(new Issue(Level(lat, s.Lim("latency_warn"), s.Lim("latency_crit")), "Disk " + s.Drive + " slow, " + Whole(lat) + " ms per operation"));
            v = s.Get("gpuTemp"); if (!double.IsNaN(v) && v >= s.Lim("gpu_temp_warn")) s.Issues.Add(new Issue(Level(v, s.Lim("gpu_temp_warn"), s.Lim("gpu_temp_crit")), "GPU hot: " + Whole(v) + " \u00b0C"));
            v = s.Get("vram"); if (!double.IsNaN(v) && v >= s.Lim("vram_full")) s.Issues.Add(new Issue("warn", "GPU memory full: " + Pct(v)));
            v = s.Get("gpuSlow"); if (!double.IsNaN(v) && v > 0) s.Issues.Add(new Issue("warn", "GPU slowed down (thermal or hardware limit)"));
            v = s.Get("limit5"); if (!double.IsNaN(v) && v < s.Lim("perf_limit")) s.Issues.Add(new Issue("warn", "CPU held back: performance limit " + Pct(v)));
            v = s.Get("netErr5"); if (!double.IsNaN(v) && v > 0) s.Issues.Add(new Issue("warn", "Network errors on an adapter"));
        }
        static double Nz(double v) { return double.IsNaN(v) ? 0 : v; }
        static string Top(string app) { return app.Length > 0 ? " · top: " + app : ""; }
    }

    // Draws the card into a premultiplied 32-bit bitmap (transparent around it).
    static class Card
    {
        static readonly CultureInfo Inv = CultureInfo.InvariantCulture;
        static readonly Color Ok = Color.FromArgb(63, 185, 80), Warn = Color.FromArgb(240, 160, 40), Crit = Color.FromArgb(248, 81, 73);
        static readonly Color Text = Color.FromArgb(236, 239, 243), Dim = Color.FromArgb(160, 168, 180);
        static readonly Color Back = Color.FromArgb(215, 16, 19, 25), TileBack = Color.FromArgb(48, 255, 255, 255), Edge = Color.FromArgb(70, 255, 255, 255);
        public const string Hint = "click: dashboards, right-click: settings";

        // Width in logical px for 1, 2 or 3 tiles per row (2 is the classic card: tiles 212 px wide).
        public static float WidthFor(int columns) { return columns <= 1 ? 300 : columns >= 3 ? 692 : 470; }

        // The issue lines shown: red ones first (in their order), as many as the settings allow.
        public static List<Issue> Shown(Snapshot s)
        {
            return s.Issues.OrderBy(i => i.Level == "crit" ? 0 : 1).Take(Math.Max(0, Math.Min(6, s.Look.IssueLines))).ToList();
        }

        static Color Of(string level) { return level == "crit" ? Crit : level == "warn" ? Warn : Ok; }
        static GraphicsPath Round(float x, float y, float w, float h, float r)
        {
            var p = new GraphicsPath(); float d = 2 * r;
            p.AddArc(x, y, d, d, 180, 90); p.AddArc(x + w - d, y, d, d, 270, 90);
            p.AddArc(x + w - d, y + h - d, d, d, 0, 90); p.AddArc(x, y + h - d, d, d, 90, 90); p.CloseFigure(); return p;
        }

        // Lines below the tiles: the top apps (on two lines when the card is one tile wide) and the status.
        static int FootLines(Look look) { return (look.TopApps ? (look.Columns <= 1 ? 2 : 1) : 0) + (look.Status ? 1 : 0); }

        // The card's size at scale k with `lines` issue lines: Render's layout, without drawing.
        public static Size Measure(Look look, int lines, float k)
        {
            Func<float, float> S = v => v * k;
            int cols = Math.Max(1, Math.Min(3, look.Columns));
            int rows = (look.Tiles.Count(t => t) + cols - 1) / cols;
            float gap = S(10), tileH = S(look.Charts ? 96 : 62);
            float tilesH = rows > 0 ? S(6) + rows * tileH + (rows - 1) * gap : 0;
            float footH = FootLines(look) * S(20);
            return new Size((int)Math.Ceiling(S(WidthFor(cols))),
                (int)Math.Ceiling(S(18) + S(30) + S(10) + S(36) + S(8) + lines * S(24) + tilesH + (footH > 0 ? S(14) + footH : 0) + S(18)));
        }

        public static Bitmap Render(Snapshot s, float k)
        {
            var look = s.Look;
            Func<float, float> S = v => v * k;
            var issues = Shown(s);
            int cols = Math.Max(1, Math.Min(3, look.Columns));
            var shown = Enumerable.Range(0, 6).Where(i => look.Tiles[i]).ToList();
            int rows = (shown.Count + cols - 1) / cols;
            float pad = S(18), gap = S(10), tileH = S(look.Charts ? 96 : 62);
            var size = Measure(look, issues.Count, k);
            int w = size.Width, h = size.Height;
            float tileW = (w - 2 * pad - (cols - 1) * gap) / cols;

            var bmp = new Bitmap(w, h, PixelFormat.Format32bppPArgb);
            using (var g = Graphics.FromImage(bmp))
            using (var fTitle = new Font("Segoe UI", S(20), FontStyle.Bold, GraphicsUnit.Pixel))
            using (var fClock = new Font("Segoe UI", S(14), GraphicsUnit.Pixel))
            using (var fBar = new Font("Segoe UI", S(16), FontStyle.Bold, GraphicsUnit.Pixel))
            using (var fIssue = new Font("Segoe UI", S(14), GraphicsUnit.Pixel))
            using (var fLabel = new Font("Segoe UI", S(12.5f), GraphicsUnit.Pixel))
            using (var fValue = new Font("Segoe UI", S(24), FontStyle.Bold, GraphicsUnit.Pixel))
            using (var fSub = new Font("Segoe UI", S(12), GraphicsUnit.Pixel))
            using (var fFoot = new Font("Segoe UI", S(11.5f), GraphicsUnit.Pixel))
            using (var text = new SolidBrush(Text))
            using (var dim = new SolidBrush(Dim))
            using (var right = new StringFormat { Alignment = StringAlignment.Far })
            using (var center = new StringFormat { Alignment = StringAlignment.Center, LineAlignment = StringAlignment.Center })
            using (var ellipsis = new StringFormat { Trimming = StringTrimming.EllipsisCharacter, FormatFlags = StringFormatFlags.NoWrap })
            {
                g.SmoothingMode = SmoothingMode.AntiAlias; g.PixelOffsetMode = PixelOffsetMode.HighQuality;
                g.TextRenderingHint = TextRenderingHint.AntiAliasGridFit;
                g.Clear(Color.Transparent);

                var back = Color.FromArgb((int)Math.Round(255 * Math.Max(40, Math.Min(100, look.Opacity)) / 100.0), Back);
                using (var card = Round(0.5f, 0.5f, w - 1, h - 1, S(16)))
                using (var b = new SolidBrush(back)) using (var pen = new Pen(Edge, 1)) { g.FillPath(b, card); g.DrawPath(pen, card); }

                float y = pad;
                g.DrawString("This PC", fTitle, text, pad, y);
                if (look.Clock) g.DrawString(DateTime.Now.ToString("HH:mm", Inv), fClock, dim, new RectangleF(pad, y + S(5), w - 2 * pad, S(24)), right);
                y += S(40);

                string worst = "ok";
                foreach (var i in s.Issues) { if (i.Level == "crit") worst = "crit"; else if (i.Level == "warn" && worst != "crit") worst = "warn"; }
                string label = !s.Up ? "MONITORING PROBLEM" : s.Issues.Count == 0 ? "ALL GOOD" : s.Issues.Count == 1 ? "1 ISSUE" : s.Issues.Count + " ISSUES";
                using (var bar = Round(pad, y, w - 2 * pad, S(36), S(10))) using (var b = new SolidBrush(Of(worst))) g.FillPath(b, bar);
                g.DrawString(label, fBar, Brushes.White, new RectangleF(pad, y, w - 2 * pad, S(36)), center);
                y += S(44);

                foreach (var i in issues)
                {
                    using (var b = new SolidBrush(Of(i.Level))) g.FillEllipse(b, pad + S(2), y + S(7), S(9), S(9));
                    g.DrawString(i.Text, fIssue, text, new RectangleF(pad + S(18), y, w - 2 * pad - S(18), S(22)), ellipsis);
                    y += S(24);
                }
                if (rows > 0) y += S(6);

                double commitMax = s.Get("commitMax"), committed = s.Get("committed");
                string commitSub = double.IsNaN(commitMax) || double.IsNaN(committed) ? "" :
                    Source.Whole(committed / 1073741824) + " of " + Source.Whole(commitMax / 1073741824) + " GB max";
                double cpu5 = s.Get("cpu5"), mem = s.Get("mem"), com = s.Get("commitOfMax"), temp = s.Get("gpuTemp"), disk5 = s.Get("disk5");
                double lat = Math.Max(Nz(s.Get("readLat5")), Nz(s.Get("writeLat5")));
                var tiles = new[] {
                    Tuple.Create("CPU", Fmt(s.Get("cpu"), Source.Pct), Fmt(s.Get("cpuW"), v => v.ToString("0", Inv) + " W"), Lv(cpu5, s.Lim("cpu_warn"), s.Lim("cpu_crit")), "cpu", 100.0),
                    Tuple.Create("Memory in use", Fmt(mem, Source.Pct), Fmt(s.Get("memTotal"), v => "of " + Source.Bytes(v)), Lv(mem, s.Lim("mem_warn"), s.Lim("mem_crit")), "mem", 100.0),
                    Tuple.Create("Commit", Fmt(com, Source.Pct), commitSub, Lv(com, s.Lim("commit_warn"), s.Lim("commit_crit")), "commit", 100.0),
                    Tuple.Create("GPU", s.GpuState == "off" ? "off" : Fmt(s.Get("gpu"), Source.Pct), Fmt(temp, v => Source.Whole(v) + " \u00b0C"), Lv(temp, s.Lim("gpu_temp_warn"), s.Lim("gpu_temp_crit")), "gpu", 100.0),
                    Tuple.Create("Disk " + s.Drive + " active", Fmt(s.Get("disk"), Source.Pct), lat > 0 ? (Math.Floor(lat * 10) / 10).ToString("0.0", Inv) + " ms" : "", !double.IsNaN(disk5) && disk5 >= s.Lim("disk_busy") ? "warn" : "ok", "disk", 100.0),
                    Tuple.Create("Network", Fmt(s.Get("netIn"), v => "\u2193 " + Source.Bits(v)), Fmt(s.Get("netOut"), v => "\u2191 " + Source.Bits(v)), "ok", "net", 0.0),
                };
                using (var tileBrush = new SolidBrush(TileBack))
                    for (int j = 0; j < shown.Count; j++)
                    {
                        var t = tiles[shown[j]];
                        float x = pad + (j % cols) * (tileW + gap), ty = y + (j / cols) * (tileH + gap);
                        using (var tp = Round(x, ty, tileW, tileH, S(10))) g.FillPath(tileBrush, tp);
                        g.DrawString(t.Item1, fLabel, dim, x + S(10), ty + S(7));
                        using (var vb = new SolidBrush(Of(t.Item4))) g.DrawString(t.Item2, fValue, vb, x + S(8), ty + S(22));
                        g.DrawString(t.Item3, fSub, dim, new RectangleF(x + S(6), ty + S(9), tileW - S(16), S(18)), right);
                        double[] pts;
                        if (look.Charts && s.Series.TryGetValue(t.Item5, out pts) && pts.Length >= 4) Spark(g, pts, t.Item6, Of(t.Item4), x + S(8), ty + tileH - S(8), tileW - S(16), S(30), S(1.6f));
                    }
                if (rows > 0) y += rows * tileH + (rows - 1) * gap;
                y += S(14);

                if (look.TopApps)
                {
                    var top = new List<string>();
                    if (s.TopCpu.Length > 0) top.Add("Top CPU: " + s.TopCpu);
                    if (s.TopMem.Length > 0) top.Add("Top memory: " + s.TopMem);
                    // One tile wide, the line has no room for both: one each.
                    if (cols == 1)
                        for (int i = 0; i < 2; i++) { if (i < top.Count) g.DrawString(top[i], fLabel, text, new RectangleF(pad, y, w - 2 * pad, S(20)), ellipsis); y += S(20); }
                    else { g.DrawString(string.Join("  \u00b7  ", top), fLabel, text, new RectangleF(pad, y, w - 2 * pad, S(20)), ellipsis); y += S(20); }
                }
                if (look.Status)
                {
                    string state = s.Failure == "busy" ? "Recorder busy" : s.Failure == "misread" ? "Recorder answer not understood"
                        : s.Failure.Length > 0 ? "Recorder unreachable" : !s.Up ? "Recording stopped" : !s.Saving ? "Recording error"
                        : s.Status == "starting" ? "Recording starting" : s.Status == "resuming" ? "Recording resumes" : "Recording OK";
                    string when = !double.IsNaN(s.SampleTs) ? "updated " + s.At.ToString("HH:mm:ss", Inv)
                        : !double.IsNaN(s.LastSampleTs) ? "last sample " + Source.Local(s.LastSampleTs).ToString("HH:mm:ss", Inv) : "no sample yet";
                    string foot = state + "  \u00b7  " + when;
                    // The hint only where it fits whole (one tile wide, or a long state): never cut in half.
                    if (g.MeasureString(foot + "  \u00b7  " + Hint, fFoot, new SizeF(100000, S(18)), ellipsis).Width <= w - 2 * pad) foot += "  \u00b7  " + Hint;
                    g.DrawString(foot, fFoot, dim, new RectangleF(pad, y, w - 2 * pad, S(18)), ellipsis);
                }
            }
            return bmp;
        }

        static string Fmt(double v, Func<double, string> f) { return double.IsNaN(v) ? "\u2013" : f(v); }
        static string Lv(double v, double warn, double crit) { return double.IsNaN(v) ? "ok" : Source.Level(v, warn, crit); }
        static double Nz(double v) { return double.IsNaN(v) ? 0 : v; }

        static void Spark(Graphics g, double[] pts, double max, Color c, float gx, float gy, float gw, float gh, float width)
        {
            int n = pts.Length / 2;
            if (max <= 0) { max = 1; for (int j = 0; j < n; j++) max = Math.Max(max, pts[2 * j + 1]); }
            var poly = new PointF[n];
            for (int j = 0; j < n; j++)
                poly[j] = new PointF(gx + gw * (float)Math.Min(1.0, pts[2 * j]), gy - gh * (float)Math.Min(1.0, Math.Max(0.0, pts[2 * j + 1] / max)));
            using (var area = new GraphicsPath())
            using (var fill = new SolidBrush(Color.FromArgb(60, c)))
            using (var pen = new Pen(c, width))
            {
                area.AddLines(poly);
                area.AddLine(poly[n - 1], new PointF(poly[n - 1].X, gy));
                area.AddLine(new PointF(poly[n - 1].X, gy), new PointF(poly[0].X, gy));
                area.CloseFigure();
                g.FillPath(fill, area);
                g.DrawLines(pen, poly);
            }
        }
    }

    // Where the card sits and how big it is: a corner of a monitor's work area and the card's distance from it,
    // in logical px (96 per inch), so it stays put when the taskbar, the resolution or the scaling changes, and
    // grows away from its corner when issue lines appear; and its size in % of the usual one. Kept in
    // card-placement.txt; none means the default, top right at 100 %.
    sealed class Spot
    {
        public const double Margin = 28, MinSize = 60, MaxSize = 250;
        public string Monitor = "";                  // device name, \\.\DISPLAY1; empty for the primary monitor
        public bool Right = true, Bottom;
        public double X = Margin, Y = Margin;
        public double Size = 100;

        public string Corner { get { return (Bottom ? "bottom" : "top") + "-" + (Right ? "right" : "left"); } }

        static string readError = "";

        // The spot in the file, the default if there is none or it does not make sense, or null if it could not be
        // read just now (being written, say): then try again later.
        public static Spot Load(string file)
        {
            var s = new Spot();
            string[] lines;
            try
            {
                if (!File.Exists(file)) return s;
                lines = File.ReadAllLines(file);
                readError = "";
            }
            catch (Exception e)
            {
                if (e.Message != readError) WidgetWindow.Log("reading the card position: " + e.Message);
                readError = e.Message;
                return null;
            }
            try
            {
                foreach (var line in lines)
                {
                    int eq = line.IndexOf('=');
                    if (line.StartsWith("#") || eq < 0) continue;
                    string key = line.Substring(0, eq).Trim(), value = line.Substring(eq + 1).Trim();
                    double v;
                    if (key == "monitor") s.Monitor = value;
                    else if (key == "corner")
                    {
                        if (value != "top-left" && value != "top-right" && value != "bottom-left" && value != "bottom-right") throw new FormatException("corner " + value);
                        s.Bottom = value.StartsWith("bottom"); s.Right = value.EndsWith("right");
                    }
                    else if ((key == "x" || key == "y") && double.TryParse(value, NumberStyles.Float, CultureInfo.InvariantCulture, out v) && v >= 0 && v <= 100000)
                    {
                        if (key == "x") s.X = v; else s.Y = v;
                    }
                    else if (key == "x" || key == "y") throw new FormatException(key + " " + value);
                    else if (key == "size")
                    {
                        if (!double.TryParse(value, NumberStyles.Float, CultureInfo.InvariantCulture, out v) || !(v >= MinSize && v <= MaxSize)) throw new FormatException("size " + value);
                        s.Size = Math.Round(v);
                    }
                }
                return s;
            }
            catch (Exception e) { WidgetWindow.Log("card position ignored (" + e.Message + "): back to the default"); return new Spot(); }
        }

        public void Save(string file)
        {
            string text = "# Where the Kymo card sits: the corner of the monitor's work area it keeps its distance from, in\r\n" +
                "# logical pixels (96 per inch), and its size in % of the usual one. Drag the card to move it, drag its\r\n" +
                "# edges (or Ctrl + scroll over it) to resize it; delete this file to put it back top right at 100 %.\r\n" +
                "monitor=" + Monitor + "\r\ncorner=" + Corner + "\r\n" +
                "x=" + X.ToString("0.#", CultureInfo.InvariantCulture) + "\r\ny=" + Y.ToString("0.#", CultureInfo.InvariantCulture) + "\r\n" +
                "size=" + Size.ToString("0", CultureInfo.InvariantCulture) + "\r\n";
            Write(file, text);
        }

        // Written whole to a new file, then swapped in.
        public static void Write(string file, string text)
        {
            Directory.CreateDirectory(Path.GetDirectoryName(file));
            string tmp = file + ".tmp";
            File.WriteAllText(tmp, text, Encoding.ASCII);
            if (File.Exists(file)) File.Replace(tmp, file, null, true); else File.Move(tmp, file);
        }
    }

    sealed class WidgetWindow : Form
    {
        public static string PositionFile = Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), @"Kymo\card-placement.txt");
        public static string LookFile = Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), @"Kymo\card-look.txt");
        // The card's settings in force: the recorder's last ones, kept in card-look.txt so that the card starts as it was
        // set up (locked included) even when the recorder does not answer yet.
        Look look;
        string lookSaved = "", lookError = "";       // card-look.txt's text as on disk; the last error saving it
        public static bool Test;                      // --selftest: no fetching, a made-up snapshot
        readonly Source source = new Source();
        readonly System.Windows.Forms.Timer timer = new System.Windows.Forms.Timer { Interval = 1000 };
        // After the desktop takes the foreground, Explorer raises it (Show desktop) a moment later: look again
        // every 150 ms for a second, so the card comes to the front without waiting for the next tick.
        readonly System.Windows.Forms.Timer burst = new System.Windows.Forms.Timer { Interval = 150 };
        int burstLeft;
        readonly Native.WinEventProc onForeground;
        IntPtr foregroundHook;
        Snapshot last;
        bool fetching, allowZ, locked, displayOff, paused;
        public static bool Trace, AlwaysVisible;
        double lastFetch = -1e9, lastOpen = -1e9;    // Clock.Now
        int late;                                    // answers in a row without the sample that should be in
        int failures;                                // failed requests in a row
        int drawnMinute = -1;                        // minute of the header clock last drawn
        float scale = 1;
        Rectangle work;
        int width, height;
        Spot spot;
        DateTime spotStamp;                          // card-placement.txt as last read or written (MinValue: none)
        // Dragging: the left button went down on the card (pressed); it became a drag once the mouse moved a few
        // pixels (dragging), else releasing it is a click. grab is where the card was held, as fractions of its size.
        bool pressed, dragging, moved, rightDown;
        Point pressAt, lastAt, dragPos;
        PointF grab;
        // Resizing: the edges held (1 left, 2 top, 4 right, 8 bottom; two of them at a corner), the card when it
        // started, and the size it has meanwhile (% of the usual one). Ctrl + scroll saves a second after the last notch.
        int zone;
        bool resizing;
        Rectangle startRect;
        double startSize, liveSize;
        double drawnSize = 100;                       // the size last drawn (smaller than its own when it would not fit)
        int wheelDelta;                               // wheel movement not yet turned into whole percents (120 = 10 %)
        readonly System.Windows.Forms.Timer wheelSave = new System.Windows.Forms.Timer { Interval = 1000 };
        long draws, fetches, pausedTicks, visibleTicks; double drawMs;
        double statsAt = Clock.Now;

        public WidgetWindow()
        {
            FormBorderStyle = FormBorderStyle.None; ShowInTaskbar = false; StartPosition = FormStartPosition.Manual;
            Text = "Kymo card";
            ReadSpot();
            look = Look.Read(LookFile);
            try { lookSaved = File.Exists(LookFile) ? look.ToText() : ""; } catch { }
            timer.Tick += delegate { Tick(); };
            wheelSave.Tick += delegate { wheelSave.Stop(); wheelDelta = 0; SaveSpot("resized"); };
            burst.Tick += delegate { KeepAboveDesktop(); if (--burstLeft <= 0 || Topmost(Handle)) burst.Stop(); };
            onForeground = OnForeground;
        }

        protected override CreateParams CreateParams
        {
            get
            {
                var cp = base.CreateParams;
                cp.ExStyle |= (int)(Native.WS_EX_LAYERED | Native.WS_EX_TOOLWINDOW | Native.WS_EX_NOACTIVATE);
                return cp;
            }
        }
        protected override bool ShowWithoutActivation { get { return true; } }

        protected override void OnHandleCreated(EventArgs e)
        {
            base.OnHandleCreated(e);
            Native.WTSRegisterSessionNotification(Handle, 0);
            locked = Native.SessionLocked();   // started on a locked session (after a crash, or a sign-in that locks at once)
            var display = new Guid("6FE69556-704A-47A0-8F24-C28D936FDA47");   // GUID_CONSOLE_DISPLAY_STATE
            Native.RegisterPowerSettingNotification(Handle, ref display, 0);
            foregroundHook = Native.SetWinEventHook(3, 3, IntPtr.Zero, onForeground, 0, 0, 0);   // EVENT_SYSTEM_FOREGROUND, out of context
        }

        protected override void OnShown(EventArgs e)
        {
            base.OnShown(e);
            Place();
            KeepAboveDesktop();
            if (Test) { last = new Snapshot { Up = true, Look = look }; Draw(); return; }
            Tick();
            timer.Start();
        }

        protected override void OnFormClosed(FormClosedEventArgs e)
        {
            if (foregroundHook != IntPtr.Zero) Native.UnhookWinEvent(foregroundHook);
            base.OnFormClosed(e);
        }

        // Work area and scale of the card's monitor, in physical pixels (the process is per-monitor DPI aware): the
        // monitor under the mouse while moving it, else the saved one, or the primary monitor if that one is gone.
        void Place()
        {
            IntPtr mon = dragging && !resizing ? Native.MonitorAt(lastAt) : Native.FindMonitor(spot.Monitor);
            if (mon == IntPtr.Zero) mon = Native.PrimaryMonitor();
            Area(mon, out work, out scale);
        }

        static void Area(IntPtr mon, out Rectangle work, out float scale)
        {
            var mi = new Native.MONITORINFO { Size = Marshal.SizeOf(typeof(Native.MONITORINFO)) };
            Native.GetMonitorInfo(mon, ref mi);
            work = Rectangle.FromLTRB(mi.Work.Left, mi.Work.Top, mi.Work.Right, mi.Work.Bottom);
            uint dx, dy;
            scale = Native.GetDpiForMonitor(mon, 0, out dx, out dy) == 0 ? dx / 96f : 1f;
        }

        int Px(double logical) { return (int)Math.Round(logical * scale); }

        // The size drawn (% of the usual one), and the drawing scale: the monitor's times that size.
        double SizeNow { get { return resizing ? liveSize : spot.Size; } }
        float K { get { return (float)(scale * SizeNow / 100); } }
        // Its place and size locked in the settings: no dragging, no resizing.
        bool PlaceLocked { get { return look.Locked; } }

        // The card's top-left corner: its spot, kept inside the work area (a smaller screen, a taller card).
        Point Origin()
        {
            if (dragging) return dragPos;
            int x = spot.Right ? work.Right - width - Px(spot.X) : work.Left + Px(spot.X);
            int y = spot.Bottom ? work.Bottom - height - Px(spot.Y) : work.Top + Px(spot.Y);
            return Inside(x, y);
        }

        Point Inside(int x, int y)
        {
            return new Point(Math.Max(work.Left, Math.Min(x, work.Right - width)), Math.Max(work.Top, Math.Min(y, work.Bottom - height)));
        }

        // ---- moving and resizing the card with the left button: held by an edge or a corner it is resized, anywhere
        // else it is moved; a click without moving still opens the dashboards.
        internal void Press(Point at)
        {
            Point o = Origin();
            pressed = true; dragging = resizing = moved = false; pressAt = lastAt = at;
            zone = ZoneAt(at);
            grab = new PointF(width > 0 ? (at.X - o.X) / (float)width : 0, height > 0 ? (at.Y - o.Y) / (float)height : 0);
        }

        internal void DragTo(Point at)
        {
            if (!pressed) return;
            int slop = Px(6);
            if (!dragging && Math.Abs(at.X - pressAt.X) <= slop && Math.Abs(at.Y - pressAt.Y) <= slop) return;
            moved = true;                                // not a click any more
            if (PlaceLocked && !dragging) return;        // locked in the settings: it stays put
            if (!dragging && zone != 0 && width > 0 && height > 0)
            {
                resizing = true;
                startRect = new Rectangle(Origin(), new Size(width, height));
                startSize = liveSize = Math.Round(drawnSize);   // what is shown: it may be drawn smaller than its size
                dragPos = startRect.Location;
            }
            else if (!dragging && width > 0 && height > 0)
            {
                // The card may have been drawn again since the press (new issue lines, the taskbar moved): held at
                // the same point of the card as it is shown now, so it does not jump on the first step.
                Point o = Origin();
                grab = new PointF((pressAt.X - o.X) / (float)width, (pressAt.Y - o.Y) / (float)height);
            }
            dragging = true; lastAt = at;
            if (resizing) { ResizeTo(at); return; }
            if (!Test) Cursor.Current = Cursors.SizeAll;
            float before = scale;
            Place();
            dragPos = DragOrigin();
            if (scale != before && last != null) { Draw(); return; }   // onto a monitor with other scaling: drawn again at its size
            Native.SetWindowPos(Handle, IntPtr.Zero, dragPos.X, dragPos.Y, 0, 0, Native.SWP_NOSIZE | Native.SWP_NOZORDER | Native.SWP_NOACTIVATE | Native.SWP_NOOWNERZORDER);
        }

        // Where the card goes while held at lastAt: inside the work area, and drawn to the usual margin (or the
        // edge) when it comes within 12 px of it, so the default place is easy to find again.
        Point DragOrigin()
        {
            int x = lastAt.X - (int)Math.Round(grab.X * width), y = lastAt.Y - (int)Math.Round(grab.Y * height);
            x = Snap(x, new[] { work.Left, work.Left + Px(Spot.Margin), work.Right - width - Px(Spot.Margin), work.Right - width });
            y = Snap(y, new[] { work.Top, work.Top + Px(Spot.Margin), work.Bottom - height - Px(Spot.Margin), work.Bottom - height });
            return Inside(x, y);
        }

        int Snap(int v, int[] lines)
        {
            int best = v, near = Px(12) + 1;
            foreach (int l in lines) if (Math.Abs(v - l) < near) { near = Math.Abs(v - l); best = l; }
            return best;
        }

        // The edges of the card at `at`: within a band 8 px wide (at its size) inside its border, and both edges of a
        // corner within 2.5 bands of it. 0 in the middle, outside the card, or when its place is locked.
        internal int ZoneAt(Point at)
        {
            if (PlaceLocked || width == 0 || height == 0) return 0;
            Point o = Origin();
            int band = Math.Max(4, (int)Math.Round(8 * K)), corner = band * 5 / 2;
            int l = at.X - o.X, t = at.Y - o.Y, r = o.X + width - 1 - at.X, b = o.Y + height - 1 - at.Y;
            if (l < 0 || t < 0 || r < 0 || b < 0) return 0;
            int z = (l < band ? 1 : 0) | (t < band ? 2 : 0) | (r < band ? 4 : 0) | (b < band ? 8 : 0);
            if ((z & 5) != 0) z |= (t < corner ? 2 : 0) | (b < corner ? 8 : 0);
            if ((z & 10) != 0) z |= (l < corner ? 1 : 0) | (r < corner ? 4 : 0);
            return z;
        }

        static Cursor CursorFor(int z)
        {
            if (z == 3 || z == 12) return Cursors.SizeNWSE;
            if (z == 6 || z == 9) return Cursors.SizeNESW;
            if (z == 1 || z == 4) return Cursors.SizeWE;
            if (z == 2 || z == 8) return Cursors.SizeNS;
            return Cursors.Default;
        }

        // Held by an edge or a corner at `at`: the card scales as a whole (its content keeps its proportions), from
        // 60 % to 250 % of the usual size and never beyond the work area, and stops at 100 % when it passes close.
        void ResizeTo(Point at)
        {
            if (!Test) Cursor.Current = CursorFor(zone);
            double w0 = startRect.Width, h0 = startRect.Height;
            double nw = w0 + ((zone & 4) != 0 ? at.X - pressAt.X : (zone & 1) != 0 ? pressAt.X - at.X : 0);
            double nh = h0 + ((zone & 8) != 0 ? at.Y - pressAt.Y : (zone & 2) != 0 ? pressAt.Y - at.Y : 0);
            bool across = (zone & 5) != 0, down = (zone & 10) != 0;
            // At a corner, the movement along the card's diagonal counts.
            double f = across && down ? (nw * w0 + nh * h0) / (w0 * w0 + h0 * h0) : across ? nw / w0 : nh / h0;
            // Up to what fits, or the size it started at if that is more (it may shrink from there without a jump).
            double fit = FitSize();
            double size = Math.Round(Math.Max(Spot.MinSize, Math.Min(Math.Min(Spot.MaxSize, Math.Max(fit, startSize)), startSize * f)));
            if (Math.Abs(size - 100) < 4 && fit >= 100) size = 100;
            if (size == liveSize) return;
            liveSize = size;
            if (last != null) Draw();
        }

        // The largest size (%) at which the card fits the work area with as many issue lines as it may show.
        double FitSize()
        {
            var most = Card.Measure(look, Math.Max(0, Math.Min(6, look.IssueLines)), scale);
            return Math.Floor(100 * Math.Min(work.Width / (double)most.Width, work.Height / (double)most.Height));
        }

        // While resizing: the corner or edge opposite the one held stays where it was; along an edge, so does the
        // side the card keeps to (its spot's corner).
        Point ResizeOrigin()
        {
            bool fixRight = (zone & 1) != 0 || ((zone & 5) == 0 && spot.Right);
            bool fixBottom = (zone & 2) != 0 || ((zone & 10) == 0 && spot.Bottom);
            return Inside(fixRight ? startRect.Right - width : startRect.Left, fixBottom ? startRect.Bottom - height : startRect.Top);
        }

        // The button is up (or the mouse was taken away): a move or a resize ends where the card is, measured from
        // the nearest corner of the work area it is on, and saved. Returns false for a click (no movement).
        internal bool Release()
        {
            if (!pressed) return false;
            pressed = false;
            if (!dragging) return moved;                 // moved on a locked card: neither a move nor a click
            if (!Test) Cursor.Current = Cursors.Default;
            bool wasResizing = resizing;
            dragging = resizing = false;
            // Measured on the monitor under the card's middle (the mouse may sit on another one, at the card's edge).
            Point o = dragPos;
            IntPtr mon = Native.MonitorAt(new Point(o.X + width / 2, o.Y + height / 2));
            Rectangle w; float k;
            Area(mon, out w, out k);
            var s = new Spot { Monitor = Native.DeviceOf(mon), Size = wasResizing ? liveSize : spot.Size };
            s.Right = o.X + width / 2.0 > w.Left + w.Width / 2.0;
            s.Bottom = o.Y + height / 2.0 > w.Top + w.Height / 2.0;
            s.X = Math.Max(0, Math.Round((s.Right ? w.Right - (o.X + width) : o.X - w.Left) / k, 1));
            s.Y = Math.Max(0, Math.Round((s.Bottom ? w.Bottom - (o.Y + height) : o.Y - w.Top) / k, 1));
            spot = s;
            wheelSave.Stop();
            SaveSpot(wasResizing ? "resized" : "moved");
            Place();
            if (last != null) Draw();
            return true;
        }

        void SaveSpot(string what)
        {
            try { spot.Save(PositionFile); spotStamp = Stamp(); }
            catch (Exception e) { Log("saving the card position: " + e.Message); }
            Log(what + ": " + spot.Corner + " " + spot.X.ToString(CultureInfo.InvariantCulture) + ", " + spot.Y.ToString(CultureInfo.InvariantCulture) +
                " at " + spot.Size.ToString(CultureInfo.InvariantCulture) + " % on " + spot.Monitor);
        }

        // Ctrl + scroll over the card: 10 % bigger or smaller a notch (120), around the corner it keeps to, in whole
        // percents however small the wheel's steps (touchpads, fine wheels); saved a second after the last step.
        internal void Wheel(int delta)
        {
            if (PlaceLocked || pressed || width == 0) return;
            if (Math.Sign(delta) != Math.Sign(wheelDelta)) wheelDelta = 0;
            wheelDelta += delta;
            int steps = wheelDelta / 12;
            if (steps == 0) return;
            wheelDelta -= steps * 12;
            // From the size shown (it may be drawn smaller than its own), never past what fits.
            double from = Math.Round(drawnSize);
            double size = Math.Max(Spot.MinSize, Math.Min(Math.Min(Spot.MaxSize, Math.Max(Math.Max(Spot.MinSize, FitSize()), from)), from + steps));
            if (steps > 0 ? size <= from : size >= from) return;
            spot.Size = size;
            if (last != null) Draw();
            wheelSave.Stop(); wheelSave.Start();
        }

        // Where the mouse was when the message being handled was made (screen, physical px; signed for monitors
        // left of or above the primary one), not where it is by the time the card gets to it.
        static Point MessageAt() { uint p = Native.GetMessagePos(); return new Point((short)(p & 0xFFFF), (short)(p >> 16)); }

        static DateTime Stamp()
        {
            try { return File.Exists(PositionFile) ? File.GetLastWriteTimeUtc(PositionFile) : DateTime.MinValue; }
            catch { return DateTime.MinValue; }
        }

        // False if the file could not be read just now: the card keeps its spot and tries again at the next tick.
        bool ReadSpot()
        {
            DateTime stamp = Stamp();
            var s = Spot.Load(PositionFile);
            if (s == null) { if (spot == null) spot = new Spot(); return false; }
            spot = s; spotStamp = stamp;
            return true;
        }

        // card-placement.txt changed or deleted by hand: the card goes to its new spot.
        internal void CheckSpotFile()
        {
            if (pressed || Stamp() == spotStamp) return;
            if (!ReadSpot()) return;
            Log("card position file changed: " + spot.Corner + " " + spot.X.ToString(CultureInfo.InvariantCulture) + ", " + spot.Y.ToString(CultureInfo.InvariantCulture));
            Place();
            if (last != null) Draw();
        }

        // For --selftest.
        internal Rectangle Bounds2 { get { return new Rectangle(Origin(), new Size(width, height)); } }
        internal Rectangle WindowRect { get { var r = Native.Bounds(Handle); return Rectangle.FromLTRB(r.Left, r.Top, r.Right, r.Bottom); } }
        internal Rectangle Work { get { return work; } }
        internal float DpiScale { get { return scale; } }
        internal Spot CurrentSpot { get { return spot; } }
        internal void ShowIssues(int n)
        {
            var s = new Snapshot { Up = true, Look = look };
            for (int i = 0; i < n; i++) s.Issues.Add(new Issue("warn", "Issue " + (i + 1)));
            last = s; Draw();
        }
        internal void SetLook(Look l) { look = last.Look = l; Draw(); }
        internal void Receive(Snapshot s) { Arrived(s); Draw(); }
        internal bool Held { get { return pressed; } }
        internal double SizeShown { get { return SizeNow; } }
        internal void MouseMovedWithButtonUp() { Native.SendMessage(Handle, 0x0200, IntPtr.Zero, IntPtr.Zero); }
        internal void CtrlWheel(int delta) { Native.SendMessage(Handle, 0x020A, (IntPtr)((delta << 16) | 0x8), IntPtr.Zero); }
        internal void FlushWheel() { if (wheelSave.Enabled) { wheelSave.Stop(); SaveSpot("resized"); } }

        void Tick()
        {
            if (Test) return;
            KeepAboveDesktop();
            CheckSpotFile();
            bool visible = AlwaysVisible || (!locked && !displayOff && !Covered());
            if (visible == paused && Trace) Log(visible ? "resumed" : "paused (" + (locked ? "locked" : displayOff ? "screen off" : "covered") + ")");
            paused = !visible;
            if (!visible) { pausedTicks++; LogStats(); return; }
            visibleTicks++;
            if (!fetching && Due()) StartFetch();
            else if (last != null && DateTime.Now.Minute != drawnMinute) Draw();   // the header clock's new minute
            LogStats();
        }

        static double IdleSeconds()
        {
            var li = new Native.LASTINPUTINFO { Size = Marshal.SizeOf(typeof(Native.LASTINPUTINFO)) };
            Native.GetLastInputInfo(ref li);
            return (unchecked((uint)Environment.TickCount - li.Time)) / 1000.0;
        }

        // Fetch once each new sample is in: the recorder samples every 10 s (interval_s), so a second after the next
        // sample's time; after 5 minutes without input, every third sample. A sample that should be in but is not is
        // asked for again each second, three times, then once per interval. A failed request is retried each second,
        // three times, while the last good answer stays up. While the recorder starts or resumes, every 2 s; while it
        // is unreachable or stopped, every 5 s. The monotonic clock bounds the wait if the wall clock jumps.
        bool Due()
        {
            double since = Clock.Now - lastFetch;
            if (since < 1) return false;
            if (last == null) return true;
            if (failures > 0 && last.Failure.Length == 0) return true;
            if (!last.Up) return since >= 5;
            if (last.Status != "ok" || double.IsNaN(last.SampleTs)) return since >= 2;
            double period = (IdleSeconds() > 300 ? 3 : 1) * last.Interval;
            if (late > 0) return late <= 3 || since >= period;
            return Source.UnixNow() >= last.SampleTs + period + 1 || since >= period + 2;
        }

        void StartFetch()
        {
            if (Test) return;
            fetching = true; lastFetch = Clock.Now; fetches++;
            if (Trace) Log("fetch");
            ThreadPool.QueueUserWorkItem(delegate
            {
                Snapshot s;
                try { s = source.Fetch(); }
                catch (Exception e) { s = new Snapshot { Up = false }; s.Issues.Add(new Issue("crit", "Widget error: " + e.Message)); Log("fetch: " + e); }
                try { BeginInvoke((MethodInvoker)delegate { fetching = false; Arrived(s); Draw(); }); } catch { }
            });
        }

        void Arrived(Snapshot s)
        {
            // A failed request right after a good, recent answer (the recorder busy for a moment): the good answer
            // stays up while it is retried.
            if (s.Failure.Length > 0 && ++failures <= 3 && last != null && last.Failure.Length == 0 && last.Up &&
                Source.UnixNow() - last.SampleTs <= 3 * last.Interval + 5)
            {
                if (Trace) Log("request failed (" + failures + "): " + s.Issues[0].Text);
                return;
            }
            if (s.Failure.Length == 0) failures = 0;
            else if (last != null) s.LastSampleTs = !double.IsNaN(last.SampleTs) ? last.SampleTs : last.LastSampleTs;
            // A failed answer (or one from an older recorder) keeps the card as it was set up.
            if (!s.HasLook) s.Look = look;
            else
            {
                if (s.Look.ToText() != look.ToText()) look = s.Look;
                // Saved when they differ from what is on disk: a save that failed is tried again with the next answer.
                string t = look.ToText();
                if (t != lookSaved)
                {
                    try { Spot.Write(LookFile, t); lookSaved = t; lookError = ""; }
                    catch (Exception e) { if (e.Message != lookError) Log("saving the card's settings: " + e.Message); lookError = e.Message; }
                }
            }
            // The same sample again while the next one should be in: the recorder is late.
            bool same = last != null && s.SampleTs == last.SampleTs;   // NaN never equals
            late = same && Source.UnixNow() >= s.SampleTs + s.Interval + 1 ? late + 1 : 0;
            if (Trace)
                Log((double.IsNaN(s.SampleTs) ? "no sample" : "sample of " + s.At.ToString("HH:mm:ss", CultureInfo.InvariantCulture)) +
                    (s.Issues.Count > 0 ? " (" + s.Issues[0].Text + ")" : "") + (late > 0 ? ", no newer one yet (" + late + ")" : ""));
            last = s;
        }

        void Draw()
        {
            if (last == null || IsDisposed) return;
            drawnMinute = DateTime.Now.Minute;
            var sw = Stopwatch.StartNew();
            // Never bigger than the work area: a size chosen while it fitted is drawn smaller for as long as the card would
            // not fit (more issue lines, another look from the settings), and comes back after; the saved size stays.
            float k = K;
            var full = Card.Measure(last.Look, Card.Shown(last).Count, k);
            if (full.Width > work.Width || full.Height > work.Height)
                k *= (float)(Math.Min(work.Width / (double)full.Width, work.Height / (double)full.Height) * 0.998);
            drawnSize = 100 * k / scale;
            using (var bmp = Card.Render(last, k))
            {
                width = bmp.Width; height = bmp.Height;
                // The size may have changed: still held at the same point, or by the same corner while resizing.
                if (dragging) dragPos = resizing ? ResizeOrigin() : DragOrigin();
                Present(bmp, Origin());
            }
            drawMs += sw.Elapsed.TotalMilliseconds; draws++;
        }

        void Present(Bitmap bmp, Point at)
        {
            IntPtr screen = Native.GetDC(IntPtr.Zero), mem = Native.CreateCompatibleDC(screen);
            IntPtr hbmp = bmp.GetHbitmap(Color.FromArgb(0)), old = Native.SelectObject(mem, hbmp);
            try
            {
                var size = new Native.SIZE(bmp.Width, bmp.Height); var src = new Native.POINT(0, 0); var dst = new Native.POINT(at.X, at.Y);
                var blend = new Native.BLENDFUNCTION { Op = 0, Flags = 0, Alpha = 255, Format = 1 };   // AC_SRC_OVER, AC_SRC_ALPHA
                Native.UpdateLayeredWindow(Handle, screen, ref dst, ref size, mem, ref src, 0, ref blend, 2);   // ULW_ALPHA
            }
            finally { Native.SelectObject(mem, old); Native.DeleteObject(hbmp); Native.DeleteDC(mem); Native.ReleaseDC(IntPtr.Zero, screen); }
        }

        // Fully hidden by real windows above it? Other widgets, click-through or see-through overlays and cloaked
        // windows do not count: the card might show through them, so it keeps drawing.
        bool Covered()
        {
            if (width == 0) return false;
            Point o = Origin();
            IntPtr rest = Native.CreateRectRgn(o.X, o.Y, o.X + width, o.Y + height);
            try
            {
                for (IntPtr h = Native.GetTopWindow(IntPtr.Zero); h != IntPtr.Zero; h = Native.GetWindow(h, Native.GW_HWNDNEXT))
                {
                    if (h == Handle) return false;            // everything after this is below the card
                    if (!Native.Shown(h) || Native.IsWidgetOrOverlay(h) || Native.IsDesktop(h)) continue;
                    var r = Native.Bounds(h);
                    if (r.Right <= r.Left || r.Bottom <= r.Top) continue;
                    IntPtr win = Native.CreateRectRgn(r.Left, r.Top, r.Right, r.Bottom);
                    int kind = Native.CombineRgn(rest, rest, win, Native.RGN_DIFF);
                    Native.DeleteObject(win);
                    if (kind == Native.NULLREGION) return true;
                }
                return false;
            }
            finally { Native.DeleteObject(rest); }
        }

        // Keep the card directly above the desktop: below every real window, above the wallpaper and icons.
        // Show desktop raises the desktop above every ordinary window (and Explorer keeps it there): the card then
        // goes topmost, in front of it, for as long as the desktop is in the foreground and still raised. A plain
        // click on the wallpaper makes the desktop the foreground window too, but leaves it at the bottom: the card
        // stays where it is.
        void KeepAboveDesktop()
        {
            if (!IsHandleCreated) return;
            if (!Native.IsWindowVisible(Handle)) Native.ShowWindow(Handle, 4);   // SW_SHOWNOACTIVATE
            IntPtr desk = Native.FindWindow("Progman", null);
            // Explorer restarting: under every window until the new desktop exists, then the walk below places the card.
            if (desk == IntPtr.Zero) { if (Topmost(Handle)) SetZ(Native.HWND_BOTTOM); return; }
            IntPtr fg = Native.GetForegroundWindow();
            if (fg != IntPtr.Zero && Native.IsDesktop(fg) && DesktopRaised(desk))
            {
                if (!Topmost(Handle))
                {
                    SetZ(Native.HWND_TOPMOST);
                    if (Trace) Log("show desktop: card in front");
                    if (last != null) Draw();
                    if (!fetching && Due()) StartFetch();
                }
                return;
            }
            if (!Topmost(Handle))
            {
                // Walk down from the card: only hidden windows and other widgets may sit between it and the desktop.
                for (IntPtr h = Native.GetWindow(Handle, Native.GW_HWNDNEXT); ; h = Native.GetWindow(h, Native.GW_HWNDNEXT))
                {
                    if (h == desk) return;                                   // in place
                    if (h == IntPtr.Zero) break;                             // the card is below the desktop
                    if (Native.Shown(h) && !Native.IsWidgetOrOverlay(h)) break;   // a real window is below the card
                }
            }
            else if (Trace) Log("show desktop over: card back down");
            // Insert right after the window above the desktop. If that one is topmost (or is the card), the
            // desktop heads the ordinary windows: the top of that band is the right place (and not topmost).
            IntPtr above = Native.GetWindow(desk, Native.GW_HWNDPREV);
            SetZ(above == IntPtr.Zero || above == Handle || Topmost(above) ? Native.HWND_NOTOPMOST : above);
        }

        static bool Topmost(IntPtr h) { return (Native.ExStyle(h) & Native.WS_EX_TOPMOST) != 0; }

        // Does the desktop head the ordinary windows (Show desktop)? Walk up from it to the topmost band: no shown real
        // window may sit in between. Widgets (the card included) and hidden or minimized windows do not count.
        static bool DesktopRaised(IntPtr desk)
        {
            for (IntPtr h = Native.GetWindow(desk, Native.GW_HWNDPREV); h != IntPtr.Zero; h = Native.GetWindow(h, Native.GW_HWNDPREV))
            {
                if (Topmost(h)) return true;
                if (Native.Shown(h) && !Native.IsWidgetOrOverlay(h) && !Native.IsDesktop(h)) return false;
            }
            return true;
        }

        void SetZ(IntPtr insertAfter)
        {
            if (insertAfter == Handle) return;
            allowZ = true;
            try { Native.SetWindowPos(Handle, insertAfter, 0, 0, 0, 0, Native.SWP_NOMOVE | Native.SWP_NOSIZE | Native.SWP_NOACTIVATE | Native.SWP_NOOWNERZORDER); }
            finally { allowZ = false; }
        }

        void OnForeground(IntPtr hook, uint ev, IntPtr hwnd, int obj, int child, uint thread, uint time)
        {
            bool wasTop = Topmost(Handle);
            KeepAboveDesktop();
            // Explorer reorders for a moment after the foreground changes, both into Show desktop and out of it.
            if ((hwnd != IntPtr.Zero && Native.IsDesktop(hwnd) && !Topmost(Handle)) || (wasTop && !Topmost(Handle))) { burstLeft = 7; burst.Start(); }
        }

        protected override void WndProc(ref Message m)
        {
            switch (m.Msg)
            {
                case 0x0046:   // WM_WINDOWPOSCHANGING: nobody else moves the card in the z-order (no "bring to front" on click)
                    if (!allowZ)
                    {
                        var wp = (Native.WINDOWPOS)Marshal.PtrToStructure(m.LParam, typeof(Native.WINDOWPOS));
                        wp.Flags |= Native.SWP_NOZORDER;
                        Marshal.StructureToPtr(wp, m.LParam, false);
                    }
                    break;
                case 0x0021:   // WM_MOUSEACTIVATE: never take the focus
                    m.Result = (IntPtr)3; return;      // MA_NOACTIVATE
                // The mouse: WinForms holds the capture from button down to button up, so a drag goes on outside the card.
                case 0x0201:   // WM_LBUTTONDOWN
                case 0x0203:   // WM_LBUTTONDBLCLK: the second press of a double-click
                    Press(MessageAt()); break;
                case 0x0200:   // WM_MOUSEMOVE; the button already up (its release went elsewhere): the drag ends there
                    if (pressed) { if ((m.WParam.ToInt64() & 1) == 0) Release(); else DragTo(MessageAt()); }   // MK_LBUTTON
                    break;
                case 0x0202:   // WM_LBUTTONUP: the end of a drag, or a click: open the dashboards, once for a double-click
                    if (pressed && !Release() && Clock.Now - lastOpen > 2 * SystemInformation.DoubleClickTime / 1000.0) { lastOpen = Clock.Now; Open("/"); }
                    break;
                case 0x0215:   // WM_CAPTURECHANGED: the mouse taken away mid-drag: the card stays where it is
                    if (pressed && m.LParam != Handle) Release();
                    break;
                case 0x001F:   // WM_CANCELMODE
                    if (pressed) Release();
                    break;
                case 0x0020:   // WM_SETCURSOR: the resize arrows over the card's edges and corners
                    if ((m.LParam.ToInt64() & 0xFFFF) == 1 && !pressed)   // HTCLIENT
                    {
                        Native.POINT p;
                        Cursor.Current = Native.GetCursorPos(out p) ? CursorFor(ZoneAt(new Point(p.X, p.Y))) : Cursors.Default;
                        m.Result = (IntPtr)1; return;
                    }
                    break;
                case 0x020A:   // WM_MOUSEWHEEL: with Ctrl held, bigger or smaller
                    if ((m.WParam.ToInt64() & 0x8) != 0)   // MK_CONTROL
                    {
                        Wheel((short)((m.WParam.ToInt64() >> 16) & 0xFFFF));
                        m.Result = IntPtr.Zero; return;
                    }
                    break;
                case 0x0204:   // WM_RBUTTONDOWN (not the second press of a double right-click)
                    rightDown = !pressed;
                    break;
                case 0x0205:   // WM_RBUTTONUP: the card's settings, if let go on the card; once, however fast the clicks
                    if (rightDown && new Rectangle(Origin(), new Size(width, height)).Contains(MessageAt()) && Clock.Now - lastOpen > 2 * SystemInformation.DoubleClickTime / 1000.0)
                    {
                        lastOpen = Clock.Now;
                        Open("/#/settings?group=desktop-card");
                    }
                    rightDown = false;
                    break;
                case 0x007E:   // WM_DISPLAYCHANGE
                    Place(); Draw(); break;
                case 0x02E0:   // WM_DPICHANGED: the card places itself; WinForms would move it to Windows' suggested rectangle
                    Place(); Draw(); m.Result = IntPtr.Zero; return;
                case 0x001A:   // WM_SETTINGCHANGE: the work area may have moved (taskbar)
                    if (m.WParam.ToInt64() == 0x2F) { Place(); Draw(); }
                    break;
                case 0x02B1:   // WM_WTSSESSION_CHANGE
                    int code = m.WParam.ToInt32();
                    if (code == 7) { locked = true; if (pressed) Release(); }      // WTS_SESSION_LOCK (mid-drag: dropped there)
                    else if (code == 8) { locked = false; Tick(); }                 // WTS_SESSION_UNLOCK
                    break;
                case 0x0218:   // WM_POWERBROADCAST
                    if (m.WParam.ToInt32() == 0x8013)                               // PBT_POWERSETTINGCHANGE
                    {
                        var ps = (Native.POWERBROADCAST_SETTING)Marshal.PtrToStructure(m.LParam, typeof(Native.POWERBROADCAST_SETTING));
                        displayOff = ps.Data == 0;
                        if (!displayOff) Tick();
                    }
                    break;
                case 0x0016:   // WM_ENDSESSION
                    if (m.WParam != IntPtr.Zero) Application.Exit();
                    break;
            }
            base.WndProc(ref m);
        }

        // A page of the dashboards in the default browser ("/", or "/#/settings?group=desktop-card").
        void Open(string page)
        {
            string url = Source.Base + page;
            if (Trace) Log("open " + url);
            try
            {
                // The browser inherits this process's priority: open it at normal priority.
                var me = Process.GetCurrentProcess(); var before = me.PriorityClass;
                me.PriorityClass = ProcessPriorityClass.Normal;
                Process.Start(new ProcessStartInfo(url) { UseShellExecute = true });
                me.PriorityClass = before;
            }
            catch (Exception e) { Log("open: " + e.Message); }
        }

        void LogStats()
        {
            if (Clock.Now - statsAt < 3600) return;
            statsAt = Clock.Now;
            Log(string.Format(CultureInfo.InvariantCulture, "last hour: {0} s visible, {1} s paused, {2} fetches, {3} draws, {4:0.0} ms per draw",
                visibleTicks, pausedTicks, fetches, draws, draws > 0 ? drawMs / draws : 0));
            visibleTicks = pausedTicks = fetches = draws = 0; drawMs = 0;
        }

        public static string LogFile = Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), @"Kymo\card.log");
        public static void Log(string text)
        {
            try
            {
                Directory.CreateDirectory(Path.GetDirectoryName(LogFile));
                var fi = new FileInfo(LogFile);
                if (fi.Exists && fi.Length > 256 * 1024) File.Delete(LogFile + ".1");
                if (fi.Exists && fi.Length > 256 * 1024) File.Move(LogFile, LogFile + ".1");
                File.AppendAllText(LogFile, DateTime.Now.ToString("yyyy-MM-dd HH:mm:ss", CultureInfo.InvariantCulture) + " " + text + Environment.NewLine);
            }
            catch { }
        }
    }

    // KymoCard.exe --selftest <folder>: moves a second card through the same press, drag and release steps the
    // mouse uses (the real mouse is not touched), with its position file and log in that folder. Exit code: the
    // number of failed checks. Needs a work area of at least 1000 x 900 logical px.
    static class SelfTest
    {
        static int failed;
        static void Check(bool ok, string what) { WidgetWindow.Log((ok ? "ok    " : "FAIL  ") + what); if (!ok) failed++; }
        static string R(Rectangle r) { return r.X + "," + r.Y + " " + r.Width + "x" + r.Height; }
        static Point Mid(Rectangle r) { return new Point(r.X + r.Width / 2, r.Y + r.Height / 2); }

        // Holds the card in its middle and moves the mouse by (dx, dy) in ten steps.
        static void Drag(WidgetWindow w, int dx, int dy)
        {
            Point from = Mid(w.Bounds2);
            w.Press(from);
            for (int i = 1; i <= 10; i++) w.DragTo(new Point(from.X + dx * i / 10, from.Y + dy * i / 10));
        }

        public static int Run(WidgetWindow w)
        {
            string file = WidgetWindow.PositionFile;
            Rectangle work = w.Work; float k = w.DpiScale;
            Func<double, int> P = v => (int)Math.Round(v * k);
            var b = w.Bounds2;
            Check(b.Width > 0 && b.Right == work.Right - P(28) && b.Top == work.Top + P(28), "no position file: top right, 28 px from the corner (" + R(b) + " in " + R(work) + ")");
            Check(w.WindowRect == b, "the window is where the card says (" + R(w.WindowRect) + ")");
            if (work.Width < P(1000) || work.Height < P(900)) { Check(false, "work area too small for the checks"); return failed; }

            // A click: the mouse moves less than the drag threshold.
            Point mid = Mid(b);
            w.Press(mid); w.DragTo(new Point(mid.X + P(3), mid.Y - P(3)));
            Check(!w.Release() && w.Bounds2 == b && w.WindowRect == b && !File.Exists(file), "a click does not move the card");

            // A drag left and down, its middle still in the top-right quarter of the work area (any screen size).
            int dx = Math.Min(P(400), Mid(b).X - (work.Left + work.Width / 2) - P(20));
            int dy = Math.Min(P(300), work.Top + work.Height / 2 - Mid(b).Y - P(20));
            Drag(w, -dx, dy);
            var held = w.WindowRect;
            Check(held.X == b.X - dx && held.Y == b.Y + dy, "the card follows the mouse (" + R(held) + ")");
            Check(w.Release(), "releasing the button ends the drag");
            var s = Spot.Load(file);
            double ex = (work.Right - held.Right) / (double)k, ey = (held.Top - work.Top) / (double)k;
            Check(File.Exists(file) && s.Corner == "top-right" && Math.Abs(s.X - ex) < 0.1 && Math.Abs(s.Y - ey) < 0.1 && s.Monitor.Length > 0,
                "saved: top-right " + Math.Round(ex, 1) + ", " + Math.Round(ey, 1) + " on " + s.Monitor + " (" + s.Corner + " " + s.X + ", " + s.Y + ")");
            Check(w.Bounds2 == held && w.WindowRect == held, "dropped where it was let go (" + R(w.WindowRect) + ")");
            w.CheckSpotFile();
            Check(w.Bounds2 == held, "its own save does not move it again");

            // Near the usual margin: drawn to it.
            var c = w.Bounds2;
            Drag(w, (work.Right - c.Width - P(36)) - c.X, 0);
            Check(w.WindowRect.Right == work.Right - P(28), "within 12 px of the margin it snaps to it (right edge " + w.WindowRect.Right + ")");
            w.Release();
            s = Spot.Load(file);
            Check(s.Corner == "top-right" && s.X == 28, "saved on the margin (" + s.Corner + " " + s.X + ", " + s.Y + ")");

            // Far past the bottom-right corner: kept inside the work area.
            Drag(w, P(5000), P(5000));
            var r = w.WindowRect;
            Check(r.Right == work.Right && r.Bottom == work.Bottom, "it cannot leave the work area (" + R(r) + ")");
            w.Release();
            s = Spot.Load(file);
            Check(s.Corner == "bottom-right" && s.X == 0 && s.Y == 0, "saved in the corner (" + s.Corner + " " + s.X + ", " + s.Y + ")");

            // Bottom left: issue lines make it taller upwards, its bottom-left corner stays.
            c = w.Bounds2;
            Drag(w, (work.Left + P(100)) - c.X, (work.Bottom - c.Height - P(60)) - c.Y);
            w.Release();
            s = Spot.Load(file);
            Check(s.Corner == "bottom-left" && Math.Abs(s.X - 100) < 0.2 && Math.Abs(s.Y - 60) < 0.2, "saved bottom-left 100, 60 (" + s.Corner + " " + s.X + ", " + s.Y + ")");
            int shortH = w.Bounds2.Height;
            w.ShowIssues(6);
            r = w.WindowRect;
            Check(r.Height > shortH && r.Left == work.Left + P(100) && r.Bottom == work.Bottom - P(60), "taller with issue lines, the same corner (" + R(r) + ")");
            w.ShowIssues(0);
            Check(w.WindowRect.Bottom == work.Bottom - P(60), "and back");

            // Drawn taller while held but before the drag starts: no jump at the first step.
            c = w.Bounds2;
            w.Press(Mid(c));
            w.ShowIssues(6);
            var tall = w.WindowRect;
            w.DragTo(new Point(Mid(c).X + P(10), Mid(c).Y));
            Check(w.WindowRect == new Rectangle(tall.X + P(10), tall.Y, tall.Width, tall.Height), "redrawn while held: it moves with the mouse, no jump (" + R(tall) + " -> " + R(w.WindowRect) + ")");
            w.Release();
            w.ShowIssues(0);

            // The button let go somewhere the card never heard of: the next move ends the drag.
            Drag(w, P(40), 0);
            var lost = w.WindowRect;
            w.MouseMovedWithButtonUp();
            Check(!w.Held && w.WindowRect == lost && Spot.Load(file).X > 100, "a move with the button up ends the drag where it was (" + R(lost) + ")");

            // The file changed by hand: the card follows; a bad file or none puts it back top right.
            File.WriteAllText(file, "monitor=\r\ncorner=top-left\r\nx=50\r\ny=40\r\n");
            File.SetLastWriteTimeUtc(file, DateTime.UtcNow.AddSeconds(-5));
            w.CheckSpotFile();
            Check(w.WindowRect.Location == new Point(work.Left + P(50), work.Top + P(40)), "edited by hand: top-left 50, 40 (" + R(w.WindowRect) + ")");
            File.WriteAllText(file, "corner=middle\r\n");
            File.SetLastWriteTimeUtc(file, DateTime.UtcNow.AddSeconds(-3));
            w.CheckSpotFile();
            Check(w.Bounds2.Location == b.Location, "a damaged file: back top right (" + R(w.Bounds2) + ")");
            File.Delete(file);
            w.CheckSpotFile();
            Check(w.WindowRect.Location == b.Location, "no file: top right (" + R(w.WindowRect) + ")");

            // A drag cut short (the mouse taken away): the card stays where it was let go.
            Drag(w, -P(200), P(100));
            var cut = w.WindowRect;
            Check(w.Release() && w.WindowRect == cut && File.Exists(file), "a drag cut short keeps its place (" + R(cut) + ")");

            // ---- resizing, from the default place
            File.Delete(file);
            w.CheckSpotFile();
            b = w.Bounds2;
            Point corner = new Point(b.Left + 2, b.Bottom - 3);
            Check(w.ZoneAt(corner) == 9 && w.ZoneAt(Mid(b)) == 0 && w.ZoneAt(new Point(b.Left + 2, Mid(b).Y)) == 1 && w.ZoneAt(new Point(b.Right - 2, b.Top + 2)) == 6,
                "edges and corners are handles, the middle is not");
            w.Press(corner);
            for (int i = 1; i <= 10; i++) w.DragTo(new Point(corner.X - P(94) * i / 10, corner.Y + P(98) * i / 10));
            var big = w.WindowRect;
            Check(w.SizeShown == 120 && big.Right == b.Right && big.Top == b.Top && Math.Abs(big.Width - 470 * k * 1.2) <= 1,
                "pulled out by its bottom-left corner: 120 %, its top-right corner stays (" + R(big) + ")");
            w.Release();
            s = Spot.Load(file);
            Check(s.Size == 120 && s.Corner == "top-right" && s.X == 28 && s.Y == 28 && w.WindowRect == big, "saved: 120 %, still 28 px from the top-right corner (" + s.Size + ")");
            corner = new Point(big.Left + 2, big.Bottom - 3);
            w.Press(corner);
            for (int i = 1; i <= 10; i++) w.DragTo(new Point(corner.X + P(80) * i / 10, corner.Y - P(84) * i / 10));
            w.Release();
            Check(Spot.Load(file).Size == 100 && w.WindowRect == b, "pushed back near 100 %: exactly 100 % (" + R(w.WindowRect) + ")");
            w.Press(new Point(b.Left + 2, b.Bottom - 3));
            for (int i = 1; i <= 10; i++) w.DragTo(new Point(b.Left + 2 + P(400) * i / 10, b.Bottom - 3 - P(400) * i / 10));
            Check(w.SizeShown == 60, "never under 60 % (" + w.SizeShown + ")");
            w.Release();

            File.Delete(file);
            w.CheckSpotFile();
            Point edge = new Point(b.Left + 2, Mid(b).Y);
            w.Press(edge);
            for (int i = 1; i <= 10; i++) w.DragTo(new Point(edge.X - P(47) * i / 10, edge.Y + P(200) * i / 10));
            var wide = w.WindowRect;
            Check(w.SizeShown == 110 && wide.Right == b.Right && wide.Top == b.Top, "pulled by its left edge: 110 % (up and down does not count), right edge and top stay (" + R(wide) + ")");
            w.Release();

            File.Delete(file);
            w.CheckSpotFile();
            w.CtrlWheel(120);
            var wheeled = w.WindowRect;
            Check(w.SizeShown == 110 && wheeled.Right == b.Right && wheeled.Top == b.Top, "Ctrl + scroll up: 110 %, around its top-right corner (" + R(wheeled) + ")");
            w.CtrlWheel(-240);
            Check(w.SizeShown == 90, "two notches down: 90 % (" + w.SizeShown + ")");
            w.FlushWheel();
            Check(Spot.Load(file).Size == 90, "saved after the last notch");
            for (int i = 0; i < 4; i++) w.CtrlWheel(3);
            Check(w.SizeShown == 91, "a touchpad's small steps add up: 4 x 3 = 1 % (" + w.SizeShown + ")");
            w.FlushWheel();

            // ---- never beyond the work area
            File.Delete(file);
            w.CheckSpotFile();
            var most = Card.Measure(new Look(), 6, k);
            double fit = Math.Floor(100 * Math.Min(work.Width / (double)most.Width, work.Height / (double)most.Height));
            corner = new Point(b.Left + 2, b.Bottom - 3);
            w.Press(corner);
            for (int i = 1; i <= 10; i++) w.DragTo(new Point(corner.X - P(3000) * i / 10, corner.Y + P(3000) * i / 10));
            Check(w.SizeShown == Math.Min(250, fit), "pulled far out: as big as fits with 6 issue lines, " + fit + " % (" + w.SizeShown + ")");
            w.Release();
            File.WriteAllText(file, "corner=top-right\r\nx=28\r\ny=28\r\nsize=250\r\n");
            File.SetLastWriteTimeUtc(file, DateTime.UtcNow.AddSeconds(-7));
            w.CheckSpotFile();
            int tall250 = w.WindowRect.Height;
            w.ShowIssues(6);
            r = w.WindowRect;
            Check(r.Height <= work.Height && r.Bottom <= work.Bottom && r.Top >= work.Top, "a size that no longer fits (250 % with 6 issue lines) is drawn smaller (" + R(r) + ")");
            w.ShowIssues(0);
            Check(w.WindowRect.Height == tall250 || tall250 > work.Height, "and back to its size when the lines go (" + R(w.WindowRect) + ")");
            // A size above what fits with all its issue lines, yet drawn whole now (no lines): resizing starts from
            // what is shown, without a jump down to that limit.
            var at100 = Card.Measure(new Look(), 0, k);
            double over = Math.Min(Spot.MaxSize, Math.Floor(100 * Math.Min(work.Width / (double)at100.Width, work.Height / (double)at100.Height)) - 1);
            if (over - 10 > fit)
            {
                File.WriteAllText(file, "corner=top-right\r\nx=28\r\ny=28\r\nsize=" + over.ToString(CultureInfo.InvariantCulture) + "\r\n");
                File.SetLastWriteTimeUtc(file, DateTime.UtcNow.AddSeconds(-9));
                w.CheckSpotFile();
                w.CtrlWheel(-120);
                Check(w.SizeShown == over - 10, "drawn whole at " + over + " %, above what fits with all lines: a notch down is 10 %, no jump (" + w.SizeShown + ")");
                w.FlushWheel();
                r = w.WindowRect;
                corner = new Point(r.Left + 2, r.Bottom - 3);
                w.Press(corner);
                for (int i = 1; i <= 10; i++) w.DragTo(new Point(corner.X - P(60) * i / 10, corner.Y + P(60) * i / 10));
                Check(w.SizeShown == over - 10, "pulled outwards it neither grows nor drops (" + w.SizeShown + ")");
                w.Release();
            }
            else WidgetWindow.Log("skip  no size between what fits with all issue lines (" + fit + " %) and without (" + over + " %) on this screen");
            File.Delete(file);
            w.CheckSpotFile();

            // ---- the card's settings, kept for when the recorder does not answer
            string lookFile = WidgetWindow.LookFile;
            w.Receive(new Snapshot { Up = true, HasLook = true, Look = new Look { Columns = 3, Locked = true } });
            var kept = Look.Read(lookFile);
            Check(File.Exists(lookFile) && kept.Columns == 3 && kept.Locked, "the recorder's settings are kept on disk");
            w.Receive(new Snapshot { Up = false, Failure = "unreachable" });
            Check(w.ZoneAt(new Point(w.Bounds2.Left + 2, w.Bounds2.Bottom - 3)) == 0 && w.Bounds2.Width == (int)Math.Ceiling(692 * k), "an answer without them keeps them: still 3 a row and locked");
            w.Receive(new Snapshot { Up = true, HasLook = true, Look = new Look() });
            Check(!Look.Read(lookFile).Locked && w.Bounds2.Width == b.Width, "new settings replace them");

            // ---- locked in the settings
            File.Delete(file);
            w.CheckSpotFile();
            w.SetLook(new Look { Locked = true });
            Check(w.ZoneAt(new Point(b.Left + 2, b.Bottom - 3)) == 0, "locked: no handles");
            w.Press(Mid(b));
            for (int i = 1; i <= 10; i++) w.DragTo(new Point(Mid(b).X - P(300) * i / 10, Mid(b).Y));
            Check(w.WindowRect == b && w.Release() && !File.Exists(file), "locked: a drag leaves it in place, and is not a click either");
            w.CtrlWheel(120);
            Check(w.SizeShown == 100 && w.WindowRect == b, "locked: Ctrl + scroll does nothing");
            w.SetLook(new Look());

            // ---- what it shows (the recorder's settings), drawn at the usual size
            Func<Look, Size> sizeOf = l => { using (var bmp = Card.Render(new Snapshot { Up = true, Look = l }, 1f)) return bmp.Size; };
            Check(sizeOf(new Look()) == new Size(470, 488), "the usual card: 470 x 488 (" + sizeOf(new Look()) + ")");
            Check(sizeOf(new Look { Columns = 3 }) == new Size(692, 382), "3 tiles a row: 692 x 382 (" + sizeOf(new Look { Columns = 3 }) + ")");
            Check(sizeOf(new Look { Columns = 1 }) == new Size(300, 826), "1 tile a row: 300 x 826, the top apps on two lines (" + sizeOf(new Look { Columns = 1 }) + ")");
            Check(sizeOf(new Look { Charts = false }) == new Size(470, 386), "no charts: shorter tiles (" + sizeOf(new Look { Charts = false }) + ")");
            var bare = new Look { TopApps = false, Status = false };
            for (int i = 0; i < 6; i++) bare.Tiles[i] = false;
            Check(sizeOf(bare) == new Size(470, 120), "no tiles, no lines below: the header and the bar only (" + sizeOf(bare) + ")");
            var four = new Look();
            four.Tiles[3] = four.Tiles[5] = false;
            Check(sizeOf(four) == new Size(470, 382), "4 tiles: 2 rows (" + sizeOf(four) + ")");
            using (var bmp = Card.Render(new Snapshot { Up = true, Look = new Look { Opacity = 60 } }, 1f))
                Check(Math.Abs(bmp.GetPixel(6, 300).A - 153) <= 2, "60 % opacity: the background lets the wallpaper through (alpha " + bmp.GetPixel(6, 300).A + ")");
            var mixed = new Snapshot { Up = true, Look = new Look { IssueLines = 1 } };
            mixed.Issues.Add(new Issue("warn", "a"));
            mixed.Issues.Add(new Issue("crit", "b"));
            var one = Card.Shown(mixed);
            Check(one.Count == 1 && one[0].Text == "b", "one issue line: the red one");
            var parsed = Look.From(new Dictionary<string, object> { { "columns", 3 }, { "gpu", false }, { "opacity", 37 }, { "locked", true }, { "charts", "yes" } });
            Check(parsed.Columns == 3 && !parsed.Tiles[3] && parsed.Tiles[0] && parsed.Opacity == 85 && parsed.Locked && parsed.Charts, "settings read from the recorder, wrong values left at their defaults");

            File.Delete(file);
            return failed;
        }
    }

    static class Program
    {
        [STAThread]
        static int Main(string[] args)
        {
            // Per-monitor DPI aware (v2), so sizes and positions are physical pixels and the card stays sharp.
            try { Native.SetProcessDpiAwarenessContext(new IntPtr(-4)); } catch { }
            // Normal memory priority: the task's priority 5 gives normal CPU and I/O priority but memory priority 4,
            // which a browser started from the card would inherit. 0 = ProcessMemoryPriority, 5 = MEMORY_PRIORITY_NORMAL.
            try { uint normal = 5; Native.SetProcessInformation(Native.GetCurrentProcess(), 0, ref normal, 4); } catch { }

            if (args.Length >= 2 && args[0] == "--snapshot")
            {
                var s = new Source().Fetch();
                // Settings to try instead of the recorder's: key=value as in /api/card "card" (columns=3 charts=false).
                if (args.Length > 2) s.Look = Look.From(Look.Pairs(args.Skip(2)));
                using (var bmp = Card.Render(s, 1.5f)) bmp.Save(args[1], ImageFormat.Png);
                return 0;
            }

            if (args.Length >= 1 && args[0] == "--selftest")
            {
                if (args.Length != 2) return 98;   // never the real card by mistake
                string dir = Path.GetFullPath(args[1]);
                Directory.CreateDirectory(dir);
                WidgetWindow.PositionFile = Path.Combine(dir, "card-placement.txt");
                WidgetWindow.LogFile = Path.Combine(dir, "card.log");
                WidgetWindow.LookFile = Path.Combine(dir, "card-look.txt");
                if (File.Exists(WidgetWindow.PositionFile)) File.Delete(WidgetWindow.PositionFile);
                if (File.Exists(WidgetWindow.LookFile)) File.Delete(WidgetWindow.LookFile);
                WidgetWindow.Test = true;
                int code = 0;
                var w = new WidgetWindow();
                w.Shown += (o, e) => w.BeginInvoke((MethodInvoker)delegate
                {
                    try { code = SelfTest.Run(w); }
                    catch (Exception ex) { WidgetWindow.Log("error: " + ex); code = 99; }
                    w.Close();
                });
                Application.Run(w);
                return code;
            }

            bool first;
            using (var mutex = new Mutex(true, @"Local\KymoCard", out first))
            {
                if (!first) return 0;      // already running in this session
                WidgetWindow.Trace = args.Contains("--trace");
                WidgetWindow.AlwaysVisible = args.Contains("--always-visible");
                Application.ThreadException += (o, e) => WidgetWindow.Log("error: " + e.Exception);
                AppDomain.CurrentDomain.UnhandledException += (o, e) => WidgetWindow.Log("fatal: " + e.ExceptionObject);
                WidgetWindow.Log("started");
                Application.Run(new WidgetWindow());
                WidgetWindow.Log("stopped");
            }
            return 0;
        }
    }
}
