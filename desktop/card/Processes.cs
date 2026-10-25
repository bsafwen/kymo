// The process list: every process of this PC, as Task Manager's Processes tab shows them, drawn on the desktop next to
// the card. It reads them itself, every 2 s by default and only while it can be seen (no admin rights needed):
// CPU, memory (private working set, Task Manager's "Memory"), I/O, GPU, threads, handles and commit. The processes of
// one program make one row (its count in brackets; a click shows each one). A click on a column title sorts by it.
// Network and power per process are not shown: Windows gives them only to programs running as administrator.
//
// Built with KymoCard.cs (build.ps1): it is a widget like the card (DeskWindow), started with it.

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
using System.Runtime.InteropServices;
using System.Text;
using System.Windows.Forms;

namespace Kymo
{
    static class ProcNative
    {
        [DllImport("ntdll.dll")] public static extern int NtQuerySystemInformation(int infoClass, IntPtr buffer, int length, out int returned);
        [DllImport("kernel32.dll")] public static extern bool GetSystemTimes(out long idle, out long kernel, out long user);
        [StructLayout(LayoutKind.Sequential)]
        public struct MEMORYSTATUSEX { public int Length, MemoryLoad; public ulong TotalPhys, AvailPhys, TotalPageFile, AvailPageFile, TotalVirtual, AvailVirtual, AvailExtendedVirtual; }
        [DllImport("kernel32.dll")] public static extern bool GlobalMemoryStatusEx(ref MEMORYSTATUSEX m);
        [DllImport("kernel32.dll")] public static extern IntPtr OpenProcess(int access, bool inherit, int pid);
        [DllImport("kernel32.dll")] public static extern bool CloseHandle(IntPtr h);
        [DllImport("kernel32.dll", CharSet = CharSet.Unicode, EntryPoint = "QueryFullProcessImageNameW")]
        public static extern bool QueryFullProcessImageName(IntPtr h, int flags, StringBuilder name, ref int size);
        [DllImport("pdh.dll", CharSet = CharSet.Unicode)] public static extern int PdhOpenQuery(string source, IntPtr user, out IntPtr query);
        [DllImport("pdh.dll", CharSet = CharSet.Unicode)] public static extern int PdhAddEnglishCounter(IntPtr query, string path, IntPtr user, out IntPtr counter);
        [DllImport("pdh.dll")] public static extern int PdhCollectQueryData(IntPtr query);
        [DllImport("pdh.dll", CharSet = CharSet.Unicode, EntryPoint = "PdhGetFormattedCounterArrayW")]
        public static extern int PdhGetFormattedCounterArray(IntPtr counter, uint format, ref int size, out int count, IntPtr items);
        [DllImport("pdh.dll")] public static extern int PdhCloseQuery(IntPtr query);
        [DllImport("kernel32.dll")] public static extern int GetActiveProcessorCount(ushort group);
        public const int STATUS_INFO_LENGTH_MISMATCH = unchecked((int)0xC0000004), PDH_MORE_DATA = unchecked((int)0x800007D2);
        public const uint PDH_FMT_DOUBLE = 0x200, PDH_FMT_NOCAP100 = 0x8000;
    }

    // One process at one reading.
    sealed class Proc
    {
        public int Pid;
        public long Created;              // FILETIME: with the PID, tells a process from a later one with the same PID
        public string Image = "";         // its file name, as the system gives it ("chrome.exe", "System")
        public string Path;               // its program's full path; null when it cannot be read (protected, gone)
        public double Cpu;                // % of all the CPU (all its cores together), as Task Manager counts it
        public double Memory;             // private working set, bytes
        public double Commit;             // private bytes
        public double Io;                 // bytes read and written per second (files, devices, network)
        public double Gpu = double.NaN;   // % of its busiest GPU engine; NaN when the GPU cannot be read
        public int Threads, Handles;
    }

    // Every process at one reading, and the PC's totals.
    sealed class ProcReading
    {
        public readonly List<Proc> All = new List<Proc>();
        public double Cpu, Memory;        // % of all the CPU, % of the RAM in use
        public double Io;                 // bytes per second, all processes
        public double Gpu = double.NaN;   // % of the busiest GPU engine
        public DateTime At = DateTime.Now;
    }

    // GPU use per process: the "GPU Engine" performance counters (Task Manager's source), one instance per process and
    // engine ("pid_1234_luid_0x0_0x1A2B_phys_0_eng_3_engtype_3D"). A process's figure is its busiest engine type, the
    // engines of one type added up; the PC's, the busiest engine type of all. Null when the counters cannot be read.
    sealed class GpuEngines : IDisposable
    {
        IntPtr query, counter;
        bool primed;

        public static GpuEngines Open()
        {
            IntPtr q, c;
            try
            {
                if (ProcNative.PdhOpenQuery(null, IntPtr.Zero, out q) != 0) return null;
                if (ProcNative.PdhAddEnglishCounter(q, @"\GPU Engine(*)\Utilization Percentage", IntPtr.Zero, out c) != 0) { ProcNative.PdhCloseQuery(q); return null; }
            }
            catch (Exception e) { DeskWindow.Log("process list: no GPU counters (" + e.Message + ")"); return null; }
            return new GpuEngines { query = q, counter = c };
        }

        // pid -> %, and the PC's total in `total`; null before the second reading (a rate needs two).
        public Dictionary<int, double> Read(out double total)
        {
            total = double.NaN;
            if (query == IntPtr.Zero || ProcNative.PdhCollectQueryData(query) != 0) return null;
            if (!primed) { primed = true; return null; }
            int size = 0, count;
            int rc = ProcNative.PdhGetFormattedCounterArray(counter, ProcNative.PDH_FMT_DOUBLE | ProcNative.PDH_FMT_NOCAP100, ref size, out count, IntPtr.Zero);
            if (rc != ProcNative.PDH_MORE_DATA || size <= 0) return null;
            IntPtr items = Marshal.AllocHGlobal(size);
            try
            {
                if (ProcNative.PdhGetFormattedCounterArray(counter, ProcNative.PDH_FMT_DOUBLE | ProcNative.PDH_FMT_NOCAP100, ref size, out count, items) != 0) return null;
                var perType = new Dictionary<string, double>();   // "pid|luid|phys|type" -> %
                var allType = new Dictionary<string, double>();   // "luid|phys|type" -> %
                int stride = IntPtr.Size == 8 ? 24 : 16;
                for (int i = 0; i < count; i++)
                {
                    IntPtr p = items + i * stride;
                    string name = Marshal.PtrToStringUni(Marshal.ReadIntPtr(p));
                    int status = Marshal.ReadInt32(p, IntPtr.Size);
                    double v = BitConverter.Int64BitsToDouble(Marshal.ReadInt64(p, IntPtr.Size == 8 ? 16 : 8));
                    if ((status != 0 && status != 1) || name == null || double.IsNaN(v) || v <= 0) continue;
                    int pid; string engine;
                    if (!Parse(name, out pid, out engine)) continue;
                    double x;
                    string key = pid.ToString(CultureInfo.InvariantCulture) + "|" + engine;
                    perType[key] = (perType.TryGetValue(key, out x) ? x : 0) + v;
                    allType[engine] = (allType.TryGetValue(engine, out x) ? x : 0) + v;
                }
                var byPid = new Dictionary<int, double>();
                foreach (var kv in perType)
                {
                    int pid = int.Parse(kv.Key.Substring(0, kv.Key.IndexOf('|')), CultureInfo.InvariantCulture);
                    double x;
                    byPid[pid] = Math.Min(100, Math.Max(byPid.TryGetValue(pid, out x) ? x : 0, kv.Value));
                }
                total = allType.Count == 0 ? 0 : Math.Min(100, allType.Values.Max());
                return byPid;
            }
            finally { Marshal.FreeHGlobal(items); }
        }

        // "pid_1234_luid_0x00000000_0x0000D1F2_phys_0_eng_0_engtype_3D": 1234 and "0x00000000_0x0000D1F2|0|3D".
        internal static bool Parse(string name, out int pid, out string engine)
        {
            pid = 0; engine = null;
            var parts = name.Split('_');
            int iPid = Array.IndexOf(parts, "pid"), iLuid = Array.IndexOf(parts, "luid"), iPhys = Array.IndexOf(parts, "phys"), iType = Array.IndexOf(parts, "engtype");
            if (iPid < 0 || iLuid < 0 || iPhys < 0 || iType < 0 || iPid + 1 >= parts.Length || iLuid + 2 >= parts.Length || iPhys + 1 >= parts.Length || iType + 1 >= parts.Length) return false;
            if (!int.TryParse(parts[iPid + 1], NumberStyles.Integer, CultureInfo.InvariantCulture, out pid)) return false;
            engine = parts[iLuid + 1] + "_" + parts[iLuid + 2] + "|" + parts[iPhys + 1] + "|" + string.Join("_", parts.Skip(iType + 1));
            return true;
        }

        public void Dispose() { if (query != IntPtr.Zero) { ProcNative.PdhCloseQuery(query); query = IntPtr.Zero; } }
    }

    // Reads every process in one call (NtQuerySystemInformation, SystemProcessInformation), as the recorder does: rates
    // (CPU, I/O) between two readings. The first reading only primes them.
    sealed class ProcSampler : IDisposable
    {
        struct Key : IEquatable<Key>
        {
            public int Pid; public long Created;
            public bool Equals(Key o) { return Pid == o.Pid && Created == o.Created; }
            public override bool Equals(object o) { return o is Key && Equals((Key)o); }
            public override int GetHashCode() { return Pid ^ Created.GetHashCode(); }
        }
        sealed class Prev { public long Cpu, Io; public string Path; public int Seen; }

        IntPtr buf; int bufLen = 1 << 20;
        readonly Dictionary<Key, Prev> prev = new Dictionary<Key, Prev>();
        readonly Stopwatch clock = Stopwatch.StartNew();
        double lastAt = -1;
        long lastIdle, lastKernel, lastUser;
        int generation;
        GpuEngines gpu;
        bool gpuTried;

        public ProcSampler() { buf = Marshal.AllocHGlobal(bufLen); }

        // The GPU counters could be opened (true before the first reading).
        public bool GpuKnown { get { return !gpuTried || gpu != null; } }

        public ProcReading Read()
        {
            if (!gpuTried) { gpuTried = true; gpu = GpuEngines.Open(); }
            int len;
            for (int i = 0; ; i++)
            {
                int rc = ProcNative.NtQuerySystemInformation(5, buf, bufLen, out len);
                if (rc == 0) break;
                if (rc != ProcNative.STATUS_INFO_LENGTH_MISMATCH || i >= 6) return null;
                Marshal.FreeHGlobal(buf);
                bufLen = Math.Max(len + 256 * 1024, bufLen * 2);
                buf = Marshal.AllocHGlobal(bufLen);
            }
            double now = clock.Elapsed.TotalSeconds, dt = lastAt < 0 ? 0 : now - lastAt;
            lastAt = now;
            generation++;
            // Every logical processor, in every processor group (over 64, Environment.ProcessorCount counts one group).
            int cores = 0;
            try { cores = ProcNative.GetActiveProcessorCount(0xFFFF); } catch { }
            if (cores <= 0) cores = Math.Max(1, Environment.ProcessorCount);
            var r = new ProcReading();

            long idle, kernel, user;
            if (ProcNative.GetSystemTimes(out idle, out kernel, out user))
            {
                long dk = kernel - lastKernel, du = user - lastUser, di = idle - lastIdle;
                if (dt > 0 && dk + du > 0) r.Cpu = Math.Max(0, Math.Min(100, 100.0 * (dk + du - di) / (dk + du)));
                lastIdle = idle; lastKernel = kernel; lastUser = user;
            }
            var ms = new ProcNative.MEMORYSTATUSEX { Length = Marshal.SizeOf(typeof(ProcNative.MEMORYSTATUSEX)) };
            if (ProcNative.GlobalMemoryStatusEx(ref ms) && ms.TotalPhys > 0) r.Memory = 100.0 * (ms.TotalPhys - ms.AvailPhys) / ms.TotalPhys;

            double gpuTotal = double.NaN;
            Dictionary<int, double> gpuByPid = null;
            if (gpu != null) { try { gpuByPid = gpu.Read(out gpuTotal); } catch (Exception e) { DeskWindow.Log("process list: GPU counters: " + e.Message); gpu.Dispose(); gpu = null; } }
            r.Gpu = gpuTotal;

            for (int off = 0; off + 0x100 <= len; )
            {
                IntPtr p = buf + off;
                int next = Marshal.ReadInt32(p, 0x00);
                int pid = (int)Marshal.ReadIntPtr(p, 0x50).ToInt64();
                if (pid != 0)   // the Idle process: the PC's idle time, not a process
                {
                    var k = new Key { Pid = pid, Created = Marshal.ReadInt64(p, 0x20) };
                    long cpu = Marshal.ReadInt64(p, 0x28) + Marshal.ReadInt64(p, 0x30);
                    long io = Marshal.ReadInt64(p, 0xE8) + Marshal.ReadInt64(p, 0xF0);
                    int nameLen = Marshal.ReadInt16(p, 0x38) & 0xFFFF;
                    IntPtr namePtr = Marshal.ReadIntPtr(p, 0x40);
                    var x = new Proc
                    {
                        Pid = pid, Created = k.Created,
                        Image = namePtr == IntPtr.Zero || nameLen == 0 ? "" : Marshal.PtrToStringUni(namePtr, nameLen / 2),
                        Memory = Marshal.ReadInt64(p, 0x08),
                        Commit = Marshal.ReadIntPtr(p, 0xC8).ToInt64(),
                        Threads = Marshal.ReadInt32(p, 0x04),
                        Handles = Marshal.ReadInt32(p, 0x60),
                    };
                    Prev pr;
                    if (prev.TryGetValue(k, out pr))
                    {
                        if (dt > 0)
                        {
                            x.Cpu = Math.Max(0, Math.Min(100, (cpu - pr.Cpu) / 1e7 / (dt * cores) * 100));
                            x.Io = Math.Max(0, (io - pr.Io) / dt);
                        }
                        x.Path = pr.Path;
                    }
                    else
                    {
                        pr = new Prev { Path = PathOf(pid, namePtr == IntPtr.Zero || nameLen == 0 ? "" : x.Image) };
                        prev[k] = pr;
                        x.Path = pr.Path;
                    }
                    pr.Cpu = cpu; pr.Io = io; pr.Seen = generation;
                    if (gpuByPid != null) { double g; x.Gpu = gpuByPid.TryGetValue(pid, out g) ? g : 0; }
                    if (x.Image.Length == 0) x.Image = pid == 4 ? "System" : "Process " + pid.ToString(CultureInfo.InvariantCulture);
                    r.Io += x.Io;
                    r.All.Add(x);
                }
                if (next == 0) break;
                off += next;
            }
            foreach (var gone in prev.Where(kv => kv.Value.Seen != generation).Select(kv => kv.Key).ToList()) prev.Remove(gone);
            return r;
        }

        // Its program's path, if this process lets itself be asked (PROCESS_QUERY_LIMITED_INFORMATION). Most services and
        // protected processes do not, to a program run as the user: then the file of that name in System32 (svchost.exe,
        // lsass.exe...), for its name and icon; else none.
        static string PathOf(int pid, string image)
        {
            if (pid == 4) return null;
            IntPtr h = ProcNative.OpenProcess(0x1000, false, pid);
            if (h != IntPtr.Zero)
            {
                try
                {
                    var sb = new StringBuilder(1024);
                    int size = sb.Capacity;
                    if (ProcNative.QueryFullProcessImageName(h, 0, sb, ref size)) return sb.ToString(0, size);
                }
                finally { ProcNative.CloseHandle(h); }
            }
            if (image.Length == 0 || image.IndexOfAny(System.IO.Path.GetInvalidFileNameChars()) >= 0 || !image.EndsWith(".exe", StringComparison.OrdinalIgnoreCase)) return null;
            foreach (var dir in new[] { Environment.SystemDirectory, System.IO.Path.Combine(Environment.SystemDirectory, "wbem") })
            {
                string f = System.IO.Path.Combine(dir, image);
                try { if (File.Exists(f)) return f; } catch { }
            }
            return null;
        }

        public void Dispose()
        {
            if (buf != IntPtr.Zero) { Marshal.FreeHGlobal(buf); buf = IntPtr.Zero; }
            if (gpu != null) { gpu.Dispose(); gpu = null; }
        }
    }

    // A program's name and icon, as Task Manager shows them: the file description of its exe ("Brave Browser"), else
    // its file name; the exe's icon. Read once per program.
    static class AppInfo
    {
        sealed class Info { public string Name; public Bitmap Icon; }
        static readonly Dictionary<string, Info> known = new Dictionary<string, Info>(StringComparer.OrdinalIgnoreCase);
        // Programs whose file a user cannot read, as Task Manager names them.
        static readonly Dictionary<string, string> Names = new Dictionary<string, string>(StringComparer.OrdinalIgnoreCase) {
            { "MsMpEng.exe", "Microsoft Defender Antimalware Service" }, { "NisSrv.exe", "Microsoft Network Realtime Inspection Service" },
            { "MpDefenderCoreService.exe", "Antimalware Core Service" }, { "SecurityHealthService.exe", "Windows Security Health Service" },
            { "vmmem", "Virtual machine memory" }, { "vmmemWSL", "Virtual machine memory (WSL)" },
        };
        static Bitmap generic;
        public static Bitmap Generic { get { return generic ?? (generic = SystemIcons.Application.ToBitmap()); } }

        static Info Of(Proc p)
        {
            Info i;
            string key = p.Path ?? p.Image;
            if (known.TryGetValue(key, out i)) return i;
            string named;
            i = new Info { Name = Names.TryGetValue(p.Image, out named) ? named : p.Image };
            if (p.Path != null)
            {
                try
                {
                    string d = FileVersionInfo.GetVersionInfo(p.Path).FileDescription;
                    if (!string.IsNullOrWhiteSpace(d)) i.Name = d.Trim();
                }
                catch { }
                try { using (var ic = System.Drawing.Icon.ExtractAssociatedIcon(p.Path)) if (ic != null) i.Icon = ic.ToBitmap(); }
                catch { }
            }
            known[key] = i;
            return i;
        }

        public static string Name(Proc p) { return Of(p).Name; }
        public static Bitmap Icon(Proc p) { return Of(p).Icon ?? Generic; }
    }

    // What the list shows, from the recorder's settings (Settings page, Process list), kept with the card's (Look).
    sealed class ProcLook
    {
        public static readonly string[] ColumnKeys = { "pid", "cpu", "memory", "io", "gpu", "threads", "handles", "commit" };
        public readonly bool[] Columns = { true, true, true, true, true, false, false, false };
        public bool Show = true, Group = true, Locked;
        public int Interval = 2;   // seconds between readings

        public string ToText(string prefix)
        {
            var sb = new StringBuilder();
            Func<bool, string> b = v => v ? "true" : "false";
            sb.Append(prefix + "show=" + b(Show) + "\r\n");
            for (int i = 0; i < ColumnKeys.Length; i++) sb.Append(prefix + ColumnKeys[i] + "=" + b(Columns[i]) + "\r\n");
            sb.Append(prefix + "group=" + b(Group) + "\r\n" + prefix + "interval=" + Interval.ToString(CultureInfo.InvariantCulture) + "\r\n" + prefix + "locked=" + b(Locked) + "\r\n");
            return sb.ToString();
        }

        // From JSON or key=value pairs, the keys with `prefix` ("" in /api/card's "processes", "procs_" in card-look.txt).
        public static ProcLook From(Dictionary<string, object> m, string prefix)
        {
            var l = new ProcLook();
            Func<string, bool, bool> flag = (k, d) => { object o; return m.TryGetValue(prefix + k, out o) && o is bool ? (bool)o : d; };
            l.Show = flag("show", true);
            for (int i = 0; i < ColumnKeys.Length; i++) l.Columns[i] = flag(ColumnKeys[i], i < 5);
            l.Group = flag("group", true);
            l.Locked = flag("locked", false);
            object iv;
            if (m.TryGetValue(prefix + "interval", out iv) && (iv is int || iv is long || iv is decimal || iv is double))
            {
                double v = Convert.ToDouble(iv, CultureInfo.InvariantCulture);
                if (v == 1 || v == 2 || v == 5) l.Interval = (int)v;
            }
            return l;
        }
    }

    // The columns: their key (as in the settings and the sort), title, width at 100 % (logical px) and how each value
    // reads. The name column takes the room the list is stretched by.
    enum Col { Name, Pid, Cpu, Memory, Io, Gpu, Threads, Handles, Commit }

    // One row of the list: a program (its processes added up) or one process.
    sealed class ProcRow
    {
        public string Key;                // the group's key, or "pid:created" for a process under its group
        public string Name;
        public Proc First;                // for the icon, the PID and the path
        public int Count = 1;             // processes in it
        public bool Child;                // a process shown under its program
        public double Cpu, Memory, Commit, Io, Gpu = double.NaN;
        public long Threads, Handles;
        public List<Proc> Procs = new List<Proc>();
    }

    // Lays out, draws and answers clicks on the list (at scale k; logical sizes below are px at 100 %).
    static class ProcTable
    {
        public const float Pad = 14, TitleH = 34, HeadH = 46, RowH = 24, MadeRows = 14, NameW = 230;
        static readonly float[] Widths = { 0, 64, 64, 100, 100, 58, 66, 74, 100 };   // per Col (Name: NameW + stretch)
        static readonly string[] Titles = { "Name", "PID", "CPU", "Memory", "I/O", "GPU", "Threads", "Handles", "Commit" };
        static readonly CultureInfo Inv = CultureInfo.InvariantCulture;
        static readonly Color Text = Color.FromArgb(236, 239, 243), Dim = Color.FromArgb(160, 168, 180), Back = Color.FromArgb(215, 16, 19, 25);
        static readonly Color Edge = Color.FromArgb(70, 255, 255, 255), Rule = Color.FromArgb(40, 255, 255, 255), Heat = Color.FromArgb(52, 120, 220), Sorted = Color.FromArgb(34, 255, 255, 255);

        public static List<Col> Shown(ProcLook look, bool gpuKnown)
        {
            var cols = new List<Col> { Col.Name };
            for (int i = 0; i < ProcLook.ColumnKeys.Length; i++)
                if (look.Columns[i] && (i + 1 != (int)Col.Gpu || gpuKnown)) cols.Add((Col)(i + 1));
            return cols;
        }

        public static float Width(Col c, double wider) { return c == Col.Name ? NameW + (float)wider : Widths[(int)c]; }

        public static Size Measure(List<Col> cols, float k, double wider, double taller)
        {
            float w = 2 * Pad + cols.Sum(c => Width(c, wider));
            float h = 2 * Pad + TitleH + HeadH + MadeRows * RowH + (float)taller;
            return new Size((int)Math.Ceiling(w * k), (int)Math.Ceiling(h * k));
        }

        // Rows that fit at this height.
        public static int RowsFitting(float k, int height) { return Math.Max(1, (int)Math.Floor((height / k - 2 * Pad - TitleH - HeadH) / RowH)); }

        // The rows in order: programs (or processes) sorted, each open program followed by its processes.
        public static List<ProcRow> Rows(ProcReading r, bool group, Col sort, bool ascending, ICollection<string> open)
        {
            var rows = new List<ProcRow>();
            if (r == null) return rows;
            if (group)
            {
                foreach (var g in r.All.GroupBy(p => p.Path != null ? p.Path.ToLowerInvariant() : "*" + p.Image.ToLowerInvariant()))
                {
                    var row = new ProcRow { Key = g.Key, Count = g.Count(), First = g.OrderBy(p => p.Created).First() };
                    row.Name = AppInfo.Name(row.First);
                    foreach (var p in g) Add(row, p);
                    rows.Add(row);
                }
            }
            else foreach (var p in r.All) { var row = One(p, false); rows.Add(row); }
            var sorted = Sort(rows, sort, ascending);
            var all = new List<ProcRow>();
            foreach (var row in sorted)
            {
                all.Add(row);
                if (row.Count > 1 && open.Contains(row.Key))
                    all.AddRange(Sort(row.Procs.Select(p => One(p, true)).ToList(), sort, ascending));
            }
            return all;
        }

        static ProcRow One(Proc p, bool child)
        {
            var row = new ProcRow { Key = child ? "pid:" + p.Pid.ToString(Inv) + ":" + p.Created.ToString(Inv) : "pid:" + p.Pid.ToString(Inv) + ":" + p.Created.ToString(Inv), First = p, Child = child };
            row.Name = AppInfo.Name(p);
            Add(row, p);
            return row;
        }

        static void Add(ProcRow row, Proc p)
        {
            row.Procs.Add(p);
            row.Cpu += p.Cpu; row.Memory += p.Memory; row.Commit += p.Commit; row.Io += p.Io;
            row.Threads += p.Threads; row.Handles += p.Handles;
            if (!double.IsNaN(p.Gpu)) row.Gpu = Math.Min(100, (double.IsNaN(row.Gpu) ? 0 : row.Gpu) + p.Gpu);
        }

        static List<ProcRow> Sort(List<ProcRow> rows, Col c, bool ascending)
        {
            Func<ProcRow, double> v = Value(c);
            IOrderedEnumerable<ProcRow> o;
            if (c == Col.Name) o = ascending ? rows.OrderBy(r => r.Name, StringComparer.CurrentCultureIgnoreCase) : rows.OrderByDescending(r => r.Name, StringComparer.CurrentCultureIgnoreCase);
            else o = ascending ? rows.OrderBy(v) : rows.OrderByDescending(v);
            return o.ThenBy(r => r.Name, StringComparer.CurrentCultureIgnoreCase).ThenBy(r => r.First.Pid).ToList();
        }

        static Func<ProcRow, double> Value(Col c)
        {
            switch (c)
            {
                case Col.Pid: return r => r.Count > 1 ? -1 : r.First.Pid;
                case Col.Cpu: return r => r.Cpu;
                case Col.Memory: return r => r.Memory;
                case Col.Io: return r => r.Io;
                case Col.Gpu: return r => double.IsNaN(r.Gpu) ? -1 : r.Gpu;
                case Col.Threads: return r => r.Threads;
                case Col.Handles: return r => r.Handles;
                case Col.Commit: return r => r.Commit;
                default: return r => 0;
            }
        }

        static string Cell(Col c, ProcRow r)
        {
            switch (c)
            {
                case Col.Pid: return r.Count > 1 ? "" : r.First.Pid.ToString(Inv);
                case Col.Cpu: return r.Cpu < 0.05 ? "0%" : r.Cpu.ToString(r.Cpu >= 99.95 ? "0" : "0.0", Inv) + "%";
                case Col.Memory: return Mb(r.Memory);
                case Col.Io: return r.Io < 0.05 * 1048576 ? "0 MB/s" : (r.Io / 1048576).ToString("#,0.0", Inv) + " MB/s";
                case Col.Gpu: return double.IsNaN(r.Gpu) ? "-" : r.Gpu < 0.05 ? "0%" : r.Gpu.ToString("0.0", Inv) + "%";
                case Col.Threads: return r.Threads.ToString("#,0", Inv);
                case Col.Handles: return r.Handles.ToString("#,0", Inv);
                case Col.Commit: return Mb(r.Commit);
                default: return "";
            }
        }
        static string Mb(double bytes) { return (bytes / 1048576).ToString("#,0.0", Inv) + " MB"; }

        // How strongly a value is shaded (0 to 1), as Task Manager does: CPU against 25 % of the PC, memory against a
        // tenth of the RAM, I/O against 50 MB/s, GPU against 50 %.
        static double HeatOf(Col c, ProcRow r, double ram)
        {
            switch (c)
            {
                case Col.Cpu: return r.Cpu / 25;
                case Col.Memory: return ram > 0 ? r.Memory / (ram * 0.1) : 0;
                case Col.Commit: return ram > 0 ? r.Commit / (ram * 0.1) : 0;
                case Col.Io: return r.Io / (50.0 * 1048576);
                case Col.Gpu: return double.IsNaN(r.Gpu) ? 0 : r.Gpu / 50;
                default: return 0;
            }
        }

        static string Total(Col c, ProcReading r)
        {
            if (r == null) return "";
            switch (c)
            {
                case Col.Cpu: return r.Cpu.ToString("0", Inv) + "%";
                case Col.Memory: return r.Memory.ToString("0", Inv) + "%";
                case Col.Io: return (r.Io / 1048576).ToString(r.Io >= 100 * 1048576 ? "#,0" : "0.0", Inv) + " MB/s";
                case Col.Gpu: return double.IsNaN(r.Gpu) ? "" : r.Gpu.ToString("0", Inv) + "%";
                case Col.Threads: return r.All.Sum(p => (long)p.Threads).ToString("#,0", Inv);
                case Col.Handles: return r.All.Sum(p => (long)p.Handles).ToString("#,0", Inv);
                case Col.Commit: return (r.All.Sum(p => p.Commit) / 1073741824).ToString("0.0", Inv) + " GB";
                default: return "";
            }
        }

        // Where things are on the bitmap last drawn, for clicks (bitmap px).
        public sealed class Hits
        {
            public readonly List<KeyValuePair<RectangleF, Col>> Titles = new List<KeyValuePair<RectangleF, Col>>();
            public readonly List<KeyValuePair<RectangleF, ProcRow>> Rows = new List<KeyValuePair<RectangleF, ProcRow>>();
            public int First, Visible, Total;   // the first row shown, how many fit, how many there are
        }

        public static Bitmap Render(ProcReading r, List<ProcRow> rows, List<Col> cols, Col sort, bool ascending, ICollection<string> open, int first,
            float k, double wider, double taller, int opacity, double ram, Hits hits)
        {
            Func<float, float> S = v => v * k;
            var size = Measure(cols, k, wider, taller);
            int w = size.Width, h = size.Height;
            int fit = RowsFitting(k, h);
            first = Math.Max(0, Math.Min(first, rows.Count - fit));
            hits.Titles.Clear(); hits.Rows.Clear();
            hits.First = first; hits.Visible = fit; hits.Total = rows.Count;
            var bmp = new Bitmap(w, h, PixelFormat.Format32bppPArgb);
            using (var g = Graphics.FromImage(bmp))
            using (var fTitle = new Font("Segoe UI", S(17), FontStyle.Bold, GraphicsUnit.Pixel))
            using (var fSmall = new Font("Segoe UI", S(12), GraphicsUnit.Pixel))
            using (var fTotal = new Font("Segoe UI", S(17), GraphicsUnit.Pixel))
            using (var fRow = new Font("Segoe UI", S(13), GraphicsUnit.Pixel))
            using (var text = new SolidBrush(Text))
            using (var dim = new SolidBrush(Dim))
            using (var rule = new Pen(Rule, 1))
            using (var right = new StringFormat { Alignment = StringAlignment.Far, LineAlignment = StringAlignment.Center, Trimming = StringTrimming.EllipsisCharacter, FormatFlags = StringFormatFlags.NoWrap })
            using (var left = new StringFormat { LineAlignment = StringAlignment.Center, Trimming = StringTrimming.EllipsisCharacter, FormatFlags = StringFormatFlags.NoWrap })
            {
                g.SmoothingMode = SmoothingMode.AntiAlias; g.PixelOffsetMode = PixelOffsetMode.HighQuality;
                g.TextRenderingHint = TextRenderingHint.AntiAliasGridFit;
                g.InterpolationMode = InterpolationMode.HighQualityBicubic;
                g.Clear(Color.Transparent);
                var back = Color.FromArgb((int)Math.Round(255 * Math.Max(40, Math.Min(100, opacity)) / 100.0), Back);
                using (var card = Round(0.5f, 0.5f, w - 1, h - 1, S(16)))
                using (var b = new SolidBrush(back)) using (var pen = new Pen(Edge, 1)) { g.FillPath(b, card); g.DrawPath(pen, card); }

                float x0 = S(Pad), y = S(Pad);
                // The title, and how many processes and when.
                g.DrawString("Processes", fTitle, text, new RectangleF(x0, y, w / 2f, S(TitleH)), left);
                if (r != null)
                    g.DrawString(r.All.Count.ToString("#,0", Inv) + " processes  \u00b7  updated " + r.At.ToString("HH:mm:ss", Inv), fSmall, dim,
                        new RectangleF(w / 2f, y, w / 2f - S(Pad), S(TitleH)), right);
                y += S(TitleH);

                // Column titles: the PC's total over each, the sorted one shaded with an arrow.
                float x = x0;
                foreach (var c in cols)
                {
                    float cw = S(Width(c, wider));
                    var cell = new RectangleF(x, y, cw, S(HeadH));
                    hits.Titles.Add(new KeyValuePair<RectangleF, Col>(cell, c));
                    if (c == sort) using (var sb = new SolidBrush(Sorted)) g.FillRectangle(sb, cell);
                    bool num = c != Col.Name;
                    var top = new RectangleF(x + S(6), y + S(2), cw - S(12), S(HeadH / 2));
                    var bottom = new RectangleF(x + S(6), y + S(HeadH / 2), cw - S(12), S(HeadH / 2 - 4));
                    if (num) g.DrawString(Total(c, r), fTotal, text, top, right);
                    g.DrawString(Titles[(int)c], fSmall, dim, bottom, num ? right : left);
                    if (c == sort) Arrow(g, dim.Color, num ? x + S(10) : x + cw - S(14), y + S(HeadH) - S(12), S(4), ascending);
                    x += cw;
                }
                y += S(HeadH);
                g.DrawLine(rule, x0, y, w - S(Pad), y);

                // The rows.
                for (int i = first; i < rows.Count && i < first + fit; i++)
                {
                    var row = rows[i];
                    var line = new RectangleF(x0, y, w - 2 * S(Pad), S(RowH));
                    hits.Rows.Add(new KeyValuePair<RectangleF, ProcRow>(line, row));
                    x = x0;
                    foreach (var c in cols)
                    {
                        float cw = S(Width(c, wider));
                        var cell = new RectangleF(x, y, cw, S(RowH));
                        double heat = Math.Min(1, HeatOf(c, row, ram));
                        if (heat > 0.02) using (var hb = new SolidBrush(Color.FromArgb((int)(40 + 150 * heat), Heat))) g.FillRectangle(hb, cell.X + S(1), cell.Y + S(1), cell.Width - S(2), cell.Height - S(2));
                        if (c == Col.Name)
                        {
                            // The arrow of a program with more than one process, then the icons in one column (a process
                            // under its program a step further in).
                            float ix = x + S(row.Child ? 40 : 18);
                            if (row.Count > 1) Chevron(g, dim.Color, x + S(4), y + S(RowH / 2), S(4), open.Contains(row.Key));
                            g.DrawImage(AppInfo.Icon(row.First), ix, y + (S(RowH) - S(16)) / 2, S(16), S(16));
                            string name = row.Name + (row.Count > 1 ? " (" + row.Count.ToString(Inv) + ")" : "");
                            g.DrawString(name, fRow, text, new RectangleF(ix + S(22), y, x + cw - ix - S(26), S(RowH)), left);
                        }
                        else g.DrawString(Cell(c, row), fRow, text, new RectangleF(x + S(4), y, cw - S(10), S(RowH)), right);
                        x += cw;
                    }
                    y += S(RowH);
                }
                // More rows than fit: where the ones shown are among them.
                if (rows.Count > fit)
                {
                    float top = S(Pad + TitleH + HeadH), span = fit * S(RowH);
                    float barH = Math.Max(S(16), span * fit / rows.Count), barY = top + (span - barH) * first / Math.Max(1, rows.Count - fit);
                    using (var bb = new SolidBrush(Color.FromArgb(70, 255, 255, 255))) g.FillRectangle(bb, w - S(Pad) + S(4), barY, S(3), barH);
                }
            }
            return bmp;
        }

        static void Arrow(Graphics g, Color c, float cx, float cy, float s, bool up)
        {
            using (var b = new SolidBrush(c))
                g.FillPolygon(b, up ? new[] { new PointF(cx - s, cy + s / 2), new PointF(cx + s, cy + s / 2), new PointF(cx, cy - s / 2) }
                                    : new[] { new PointF(cx - s, cy - s / 2), new PointF(cx + s, cy - s / 2), new PointF(cx, cy + s / 2) });
        }

        static void Chevron(Graphics g, Color c, float x, float cy, float s, bool open)
        {
            using (var pen = new Pen(c, Math.Max(1f, s / 3)) { StartCap = LineCap.Round, EndCap = LineCap.Round })
                g.DrawLines(pen, open ? new[] { new PointF(x, cy - s / 2), new PointF(x + s, cy + s / 2), new PointF(x + 2 * s, cy - s / 2) }
                                      : new[] { new PointF(x + s / 2, cy - s), new PointF(x + s * 1.5f, cy), new PointF(x + s / 2, cy + s) });
        }

        static GraphicsPath Round(float x, float y, float w, float h, float r)
        {
            var p = new GraphicsPath(); float d = 2 * r;
            p.AddArc(x, y, d, d, 180, 90); p.AddArc(x + w - d, y, d, d, 270, 90);
            p.AddArc(x + w - d, y + h - d, d, d, 0, 90); p.AddArc(x, y + h - d, d, d, 90, 90); p.CloseFigure(); return p;
        }
    }

    // The process list on the desktop, next to the card: shown or not, and what it shows, from the recorder's settings
    // (Settings page, Process list, carried by the card's answers); its place, size, opacity and sort its own
    // (processes-placement.txt). Click a column title to sort by it (again: the other way), a program to see its
    // processes; scroll over it for more rows; Shift + scroll for its opacity; right-click for its settings.
    sealed class ProcessWindow : DeskWindow
    {
        public static string PlacementFile = Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), @"Kymo\processes-placement.txt");
        public const int DefaultOpacity = 85;
        readonly WidgetWindow card;
        ProcLook look;
        readonly ProcSampler sampler = new ProcSampler();
        ProcReading reading;
        double lastRead = -1e9;
        readonly HashSet<string> open = new HashSet<string>();
        readonly ProcTable.Hits hits = new ProcTable.Hits();
        int first;                                    // the first row shown (scrolled)
        int scrollWheel, opacityWheel;
        double ram;
        readonly System.Windows.Forms.Timer opacitySave = new System.Windows.Forms.Timer { Interval = 1000 };

        public ProcessWindow(WidgetWindow card)
        {
            this.card = card;
            Text = "Kymo processes";
            look = card.CurrentLook.Procs;
            hiddenBySetting = !look.Show;
            var ms = new ProcNative.MEMORYSTATUSEX { Length = Marshal.SizeOf(typeof(ProcNative.MEMORYSTATUSEX)) };
            if (ProcNative.GlobalMemoryStatusEx(ref ms)) ram = ms.TotalPhys;
            opacitySave.Tick += delegate { opacitySave.Stop(); SaveSpot("opacity " + Opacity2 + " %"); };
            ReadSpot();
        }

        protected override string PlaceFile { get { return PlacementFile; } }
        protected override string WidgetName { get { return "process list"; } }
        protected override bool PlaceLocked { get { return look.Locked; } }
        protected override bool Ready { get { return reading != null; } }   // nothing drawn before a real reading
        int Opacity2 { get { return spot.Opacity >= 40 ? spot.Opacity : DefaultOpacity; } }
        // The saved column, if shown; else memory (or the name, memory hidden).
        Col Sort
        {
            get
            {
                Col c;
                var cols = Columns;
                if (Enum.TryParse(spot.Sort, true, out c) && Enum.IsDefined(typeof(Col), c) && cols.Contains(c)) return c;
                return cols.Contains(Col.Memory) ? Col.Memory : Col.Name;
            }
        }
        bool SortUp { get { Col c; return Enum.TryParse(spot.Sort, true, out c) && c == Sort ? spot.Ascending : Sort == Col.Name; } }
        List<Col> Columns { get { return ProcTable.Shown(look, sampler.GpuKnown); } }

        // Next to the card, on the side away from its corner, its top in line with the card's; where that does not fit
        // the screen, under the card (above it, for a card at the bottom), clear of all its issue lines. Followed while
        // the list has no placement file of its own (VisibleTick).
        protected override Spot DefaultSpot()
        {
            var c = card.CurrentSpot ?? new Spot();
            float z = (float)(c.Size / 100);
            var made = Card.Measure(card.CurrentLook, 0, z, c.Wider, c.Taller);
            var most = Card.Measure(card.CurrentLook, Math.Max(0, Math.Min(6, card.CurrentLook.IssueLines)), z, c.Wider, c.Taller);
            var list = ProcTable.Measure(Columns, 1, 0, 0);
            float k = card.DpiScale;
            double workW = card.Work.Width / (double)k, workH = card.Work.Height / (double)k;
            if (workW <= 0 || c.X + made.Width + 16 + list.Width <= workW || c.Y + most.Height + 16 + list.Height > workH)
                return new Spot { Monitor = c.Monitor, Right = c.Right, Bottom = c.Bottom, X = Math.Round(c.X + made.Width + 16, 1), Y = c.Y };
            return new Spot { Monitor = c.Monitor, Right = c.Right, Bottom = c.Bottom, X = c.X, Y = Math.Round(c.Y + most.Height + 16, 1) };
        }

        static bool Same(Spot a, Spot b) { return a.Monitor == b.Monitor && a.Right == b.Right && a.Bottom == b.Bottom && a.X == b.X && a.Y == b.Y; }

        protected override Size Measure(float k, double wider, double taller) { return ProcTable.Measure(Columns, k, wider, taller); }

        protected override Bitmap Render(float k, double wider, double taller)
        {
            var rows = ProcTable.Rows(reading, look.Group, Sort, SortUp, open);
            var bmp = ProcTable.Render(reading, rows, Columns, Sort, SortUp, open, first, k, wider, taller, Opacity2, ram, hits);
            first = hits.First;
            return bmp;
        }

        // The settings arrived with the card's answer: shown or hidden, columns, update speed.
        internal void Apply(Look l)
        {
            var was = look;
            look = l.Procs;
            if (!look.Show)
            {
                if (!hiddenBySetting) { hiddenBySetting = true; if (Visible) Hide(); Log("process list hidden (its setting)"); }
                return;
            }
            if (hiddenBySetting)
            {
                hiddenBySetting = false;
                Log("process list shown (its setting)");
                bool firstShowing = !IsHandleCreated;
                if (!Visible) Show();
                if (!firstShowing) ReadNow();   // the first showing reads from OnShown
                return;
            }
            if (was.ToText("") != look.ToText("") && IsHandleCreated) Draw();
        }

        // Shown (or shown again): read at once, then each `Interval` seconds while it can be seen.
        internal void Start() { if (!hiddenBySetting) Show(); }

        protected override void TestStart() { reading = sampler.Read(); Draw(); }

        protected override void VisibleTick()
        {
            FollowCard();
            if (Clock.Now - lastRead >= look.Interval - 0.05) ReadNow();
        }

        // Without a file of its own, it follows the card (moved, resized, another layout); its size, opacity and sort stay.
        internal void FollowCard()
        {
            if (File.Exists(PlaceFile) || Held) return;
            var d = DefaultSpot();
            if (Same(d, spot)) return;
            d.Size = spot.Size; d.Wider = spot.Wider; d.Taller = spot.Taller; d.Opacity = spot.Opacity; d.Sort = spot.Sort; d.Ascending = spot.Ascending;
            spot = d;
            Place();
            Draw();
        }

        protected override void Revealed() { ReadNow(); }

        // CPU, I/O and GPU are rates between two readings. A reading after a pause (covered, locked, hidden: longer than
        // twice the interval), or the very first, only primes them: the figures shown stay, and the next reading comes
        // a second later. None within half a second of the last (Show desktop right after a tick): drawn again only.
        void ReadNow()
        {
            if (!Test) Read();
        }

        bool primed;   // a reading taken: the next one (soon enough) has rates

        void Read()
        {
            double gap = Clock.Now - lastRead;
            if (primed && gap < 0.5) { Draw(); return; }
            bool priming = !primed || gap > 2 * look.Interval + 1;
            var r = sampler.Read();
            fetches++;
            if (r == null) { lastRead = Clock.Now; return; }   // not read just now: tried again at the next interval
            primed = true;
            lastRead = priming ? Clock.Now - look.Interval + 1 : Clock.Now;
            if (!priming) reading = r;
            Draw();
        }

        protected override void Clicked(Point at, bool second)
        {
            if (second) return;   // a double-click: one click (the second would undo the first)
            var o = Origin();
            var p = new PointF(at.X - o.X, at.Y - o.Y);
            foreach (var t in hits.Titles)
                if (t.Key.Contains(p))
                {
                    // Again on the same column: the other way. A new one: biggest first (names A to Z).
                    spot.Ascending = t.Value == Sort ? !SortUp : t.Value == Col.Name;
                    spot.Sort = t.Value.ToString().ToLowerInvariant();
                    first = 0;
                    SaveSpot("sorted by " + spot.Sort + (spot.Ascending ? ", ascending" : ""));
                    Draw();
                    return;
                }
            foreach (var row in hits.Rows)
                if (row.Key.Contains(p) && row.Value.Count > 1 && !row.Value.Child)
                {
                    if (!open.Remove(row.Value.Key)) open.Add(row.Value.Key);
                    Draw();
                    return;
                }
        }

        protected override void RightClicked(Point at) { Open("/#/settings?group=process-list"); }

        // Over the list: 3 rows a notch.
        protected override bool OnPlainWheel(int delta, Point at)
        {
            if (Math.Sign(delta) != Math.Sign(scrollWheel)) scrollWheel = 0;
            scrollWheel += delta;
            int steps = scrollWheel * 3 / 120;
            if (steps == 0) return true;
            scrollWheel -= steps * 120 / 3;
            int to = Math.Max(0, Math.Min(first - steps, hits.Total - hits.Visible));
            if (to != first) { first = to; Draw(); }
            return true;
        }

        // Shift + scroll: 5 % more or less opaque a notch, from 40 to 100 %, kept in its placement file a second after.
        protected override void OnShiftWheel(int delta)
        {
            if (Math.Sign(delta) != Math.Sign(opacityWheel)) opacityWheel = 0;
            opacityWheel += delta;
            int steps = opacityWheel / 120;
            if (steps == 0) return;
            opacityWheel -= steps * 120;
            int to = Math.Max(40, Math.Min(100, (int)Math.Round(Opacity2 / 5.0) * 5 + 5 * steps));
            if (to == Opacity2) return;
            spot.Opacity = to;
            Draw();
            opacitySave.Stop(); opacitySave.Start();
        }

        protected override void OnFormClosed(FormClosedEventArgs e)
        {
            if (opacitySave.Enabled) { opacitySave.Stop(); SaveSpot("opacity " + Opacity2 + " %"); }
            sampler.Dispose();
            base.OnFormClosed(e);
        }

        // For --selftest.
        internal void ShowReading(ProcReading r) { reading = r; Draw(); }
        internal void FlushOpacity() { if (opacitySave.Enabled) { opacitySave.Stop(); SaveSpot("opacity " + Opacity2 + " %"); } }
        internal ProcTable.Hits LastHits { get { return hits; } }
        internal int OpacityShown { get { return Opacity2; } }
        internal Col SortedBy { get { return Sort; } }
        internal bool SortedUp { get { return SortUp; } }
        internal int FirstRow { get { return first; } }
        internal bool Hidden { get { return hiddenBySetting; } }
        internal void ClickAt(Point at, bool second = false) { Clicked(at, second); }
        internal Spot CurrentPlace { get { return spot; } }
        internal void WheelOver(int delta) { Native.SendMessage(Handle, 0x020A, (IntPtr)(delta << 16), IntPtr.Zero); }
        internal ProcReading ReadOnce() { return sampler.Read(); }
        internal void ReadAsRunning() { Read(); }   // as the timer reads (ReadNow does nothing in --selftest)
        internal bool HasReading { get { return reading != null; } }
    }

    // KymoCard.exe --selftest: the process list, after the card's checks. A real reading of this PC, then made-up ones
    // for what is drawn and what a click does (the real mouse is not touched).
    static class ProcSelfTest
    {
        static int failed;
        static void Check(bool ok, string what) { DeskWindow.Log((ok ? "ok    " : "FAIL  ") + "process list: " + what); if (!ok) failed++; }

        static Proc Fake(int pid, string image, string path, double cpu, double mb)
        {
            return new Proc { Pid = pid, Created = 1000 + pid, Image = image, Path = path, Cpu = cpu, Memory = mb * 1048576, Commit = mb * 1048576, Gpu = 0, Threads = 10, Handles = 100 };
        }

        static Point Mid(RectangleF r, Point origin) { return new Point(origin.X + (int)(r.X + r.Width / 2), origin.Y + (int)(r.Y + r.Height / 2)); }

        public static int Run(WidgetWindow card)
        {
            failed = 0;
            card.CheckSpotFile();
            var w = new ProcessWindow(card);
            w.Show();
            Application.DoEvents();
            float k = card.DpiScale;
            Func<double, int> P = v => (int)Math.Round(v * k);

            // A real reading: this process is there, with its memory, threads and program; a busy moment shows as CPU.
            w.ReadOnce();
            var spin = Stopwatch.StartNew();
            while (spin.ElapsedMilliseconds < 400) { }
            var r = w.ReadOnce();
            int me = Process.GetCurrentProcess().Id;
            var mine = r == null ? null : r.All.FirstOrDefault(x => x.Pid == me);
            Check(mine != null && mine.Memory > 0 && mine.Threads > 0 && mine.Cpu > 0.3 && mine.Path != null && mine.Path.EndsWith(".exe", StringComparison.OrdinalIgnoreCase),
                "this process read: memory, threads, its program, CPU while busy (" + (mine == null ? "missing" : mine.Cpu.ToString("0.0", CultureInfo.InvariantCulture) + " %") + ")");
            Check(r != null && r.Cpu >= 0 && r.Cpu <= 100 && r.Memory > 0 && r.Memory < 100 && r.All.Count > 20 && !r.All.Any(x => x.Pid == 0),
                "the PC's CPU and memory in %, every process but the idle one (" + (r == null ? 0 : r.All.Count) + " processes)");
            // Read as it runs: the first reading only primes the rates (nothing drawn yet); the next, a second later, is
            // shown; one right after it (within half a second) is not taken.
            var fresh = new ProcessWindow(card);
            fresh.ReadAsRunning();
            bool none = !fresh.HasReading;
            System.Threading.Thread.Sleep(1100);
            fresh.ReadAsRunning();
            bool shown = fresh.HasReading;
            fresh.Dispose();
            Check(none && shown, "read as it runs: the first reading primes, the next one is shown (" + none + ", " + shown + ")");

            int gpid; string engine;
            Check(GpuEngines.Parse("pid_1234_luid_0x00000000_0x0000D1F2_phys_0_eng_3_engtype_VideoDecode", out gpid, out engine) && gpid == 1234 && engine == "0x00000000_0x0000D1F2|0|VideoDecode"
                && !GpuEngines.Parse("_Total", out gpid, out engine), "GPU engine counters read per process and engine type");

            // The settings, as /api/card sends them ("processes") and as card-look.txt keeps them (procs_).
            var sent = ProcLook.From(new Dictionary<string, object> { { "show", false }, { "threads", true }, { "gpu", false }, { "interval", 5 }, { "group", false } }, "");
            var back = Look.From(Look.Pairs(new Look { Procs = sent }.ToText().Split('\n').Select(x => x.TrimEnd('\r')))).Procs;
            Check(!sent.Show && sent.Columns[5] && !sent.Columns[4] && sent.Interval == 5 && !sent.Group && sent.Columns[0]
                && back.ToText("") == sent.ToText("") && ProcLook.From(new Dictionary<string, object> { { "interval", 3 } }, "").Interval == 2,
                "its settings read from the recorder and from card-look.txt; a wrong one left at its default");

            // Made-up processes: a program of three, a big one, a busy one, and many small ones.
            var fake = new ProcReading { Cpu = 15, Memory = 54, Io = 2 * 1048576, Gpu = 3 };
            fake.All.Add(Fake(100, "app.exe", @"C:\kymo-test\app.exe", 5, 300));
            fake.All.Add(Fake(101, "app.exe", @"C:\kymo-test\app.exe", 1, 200));
            fake.All.Add(Fake(102, "app.exe", @"C:\kymo-test\app.exe", 0, 100));
            fake.All.Add(Fake(200, "big.exe", @"C:\kymo-test\big.exe", 0.5, 900));
            fake.All.Add(Fake(300, "busy.exe", @"C:\kymo-test\busy.exe", 30, 50));
            for (int i = 0; i < 40; i++) fake.All.Add(Fake(1000 + i, "small" + i + ".exe", null, 0, i + 1));
            w.ShowReading(fake);
            var hits = w.LastHits;
            var o = w.Bounds2.Location;
            Func<int, ProcRow> row = i => i < hits.Rows.Count ? hits.Rows[i].Value : null;
            Check(w.SortedBy == Col.Memory && !w.SortedUp && row(0).Name == "big.exe" && row(1).Name == "app.exe" && row(1).Count == 3 && Math.Abs(row(1).Memory - 600 * 1048576.0) < 1,
                "sorted by memory, biggest first; a program's processes in one row, added up (" + row(0).Name + ", " + row(1).Name + " x" + row(1).Count + ")");
            Check(hits.Visible == (int)ProcTable.MadeRows && hits.Rows.Count == hits.Visible && hits.Total == 43, "as made: 14 rows shown of 43 (" + hits.Rows.Count + ")");

            var cpuTitle = hits.Titles.First(t => t.Value == Col.Cpu).Key;
            w.ClickAt(Mid(cpuTitle, o));
            hits = w.LastHits;
            Check(w.SortedBy == Col.Cpu && !w.SortedUp && row(0).Name == "busy.exe" && row(1).Name == "app.exe" && Spot.Load(ProcessWindow.PlacementFile).Sort == "cpu",
                "a click on CPU: sorted by CPU, busiest first, kept in its file (" + row(0).Name + ")");
            w.ClickAt(Mid(hits.Titles.First(t => t.Value == Col.Cpu).Key, o));
            hits = w.LastHits;
            Check(w.SortedUp && row(0).Cpu == 0, "again: the other way");
            w.ClickAt(Mid(hits.Titles.First(t => t.Value == Col.Memory).Key, o));
            hits = w.LastHits;
            w.ClickAt(Mid(hits.Rows[1].Key, o));
            hits = w.LastHits;
            Check(row(1).Name == "app.exe" && row(2).Child && row(3).Child && row(4).Child && !row(5).Child && row(2).First.Pid == 100,
                "a click on a program: its processes under it, biggest first (" + row(2).First.Pid + ", " + row(3).First.Pid + ", " + row(4).First.Pid + ")");
            w.ClickAt(Mid(hits.Rows[1].Key, o));
            hits = w.LastHits;
            Check(!row(2).Child, "and again: folded");
            w.ClickAt(Mid(hits.Rows[1].Key, o));
            w.ClickAt(Mid(hits.Rows[1].Key, o), true);
            hits = w.LastHits;
            bool openedOnce = row(2).Child;
            w.ClickAt(Mid(hits.Rows[1].Key, o));
            hits = w.LastHits;
            var memTitle = hits.Titles.First(t => t.Value == Col.Memory).Key;
            w.ClickAt(Mid(memTitle, o));
            w.ClickAt(Mid(memTitle, o), true);
            Check(openedOnce && w.SortedBy == Col.Memory && w.SortedUp && Spot.Load(ProcessWindow.PlacementFile).Ascending,
                "a double-click counts once (a program opened, a sort turned); memory smallest first is kept in its file");
            w.ClickAt(Mid(w.LastHits.Titles.First(t => t.Value == Col.Memory).Key, o));
            hits = w.LastHits;

            w.WheelOver(-120);
            hits = w.LastHits;
            Check(w.FirstRow == 3 && hits.First == 3, "scrolled down a notch: 3 rows (" + w.FirstRow + ")");
            for (int i = 0; i < 20; i++) w.WheelOver(-120);
            Check(w.FirstRow == 43 - hits.Visible, "never past the last row (" + w.FirstRow + ")");
            for (int i = 0; i < 20; i++) w.WheelOver(120);
            Check(w.FirstRow == 0, "and back to the top");

            // The settings: one row per process, more columns, hidden and shown again, locked.
            var l = new Look();
            l.Procs.Group = false;
            l.Procs.Columns[5] = true;   // threads
            int before = w.Bounds2.Width;
            w.Apply(l);
            hits = w.LastHits;
            Check(hits.Total == 45 && Math.Abs(w.Bounds2.Width - before - P(66)) <= 2, "one row per process (45), and a threads column (" + (w.Bounds2.Width - before) + " px wider)");
            w.ClickAt(Mid(hits.Titles.First(t => t.Value == Col.Threads).Key, w.Bounds2.Location));
            bool byThreads = w.SortedBy == Col.Threads;
            w.Apply(new Look());
            Check(byThreads && w.SortedBy == Col.Memory && !w.SortedUp, "sorted by threads, then the threads column turned off: by memory again");
            w.ClickAt(Mid(w.LastHits.Titles.First(t => t.Value == Col.Memory).Key, w.Bounds2.Location));
            w.ClickAt(Mid(w.LastHits.Titles.First(t => t.Value == Col.Memory).Key, w.Bounds2.Location));
            l = new Look();
            l.Procs.Show = false;
            w.Apply(l);
            bool hid = w.Hidden && !w.Visible;
            w.Apply(new Look());
            Check(hid && !w.Hidden && w.Visible, "hidden by its setting, and shown again");
            l = new Look();
            l.Procs.Locked = true;
            w.Apply(l);
            var bb = w.Bounds2;
            Check(w.ZoneAt(new Point(bb.Left + 2, bb.Bottom - 3)) == 0, "locked: no handles");
            w.Apply(new Look());
            w.ShowReading(fake);

            // Taller: more rows; Shift + scroll: its own opacity, kept in its file.
            bb = w.Bounds2;
            var edge = new Point((bb.Left + bb.Right) / 2, bb.Bottom - 3);
            w.Press(edge);
            for (int i = 1; i <= 10; i++) w.DragTo(new Point(edge.X, edge.Y + P(ProcTable.RowH * 3) * i / 10));
            w.Release();
            hits = w.LastHits;
            Check(hits.Visible == (int)ProcTable.MadeRows + 3, "pulled 3 rows taller by its bottom edge: 17 rows (" + hits.Visible + ")");
            w.ShiftWheel(120);
            int up = w.OpacityShown;
            w.FlushOpacity();
            var kept = Spot.Load(ProcessWindow.PlacementFile);
            Check(up == ProcessWindow.DefaultOpacity + 5 && kept.Opacity == up && kept.Taller > 0 && kept.Sort == "memory", "Shift + scroll: 5 % more opaque, kept with its place, size and sort (" + kept.Opacity + ")");

            // No file: next to the card, on the side away from its corner.
            File.Delete(ProcessWindow.PlacementFile);
            w.CheckSpotFile();
            var c = card.Bounds2;
            bb = w.Bounds2;
            Check(card.CurrentSpot.Right ? bb.Right <= c.Left - P(14) && bb.Right >= c.Left - P(18) : bb.Left >= c.Right + P(14), "no file: beside the card, 16 px apart (" + bb + ", card " + c + ")");
            // Still without a file, the card moves: the list follows it.
            File.WriteAllLines(WidgetWindow.PositionFile, new[] { "corner=top-right", "x=120", "y=60", "size=100" });
            File.SetLastWriteTimeUtc(WidgetWindow.PositionFile, DateTime.UtcNow.AddSeconds(-11));
            card.CheckSpotFile();
            w.FollowCard();
            c = card.Bounds2;
            bb = w.Bounds2;
            Check(bb.Top == c.Top && bb.Right <= c.Left - P(14) && bb.Right >= c.Left - P(18) && !File.Exists(ProcessWindow.PlacementFile), "the card moved: the list beside it again, still without a file (" + bb + ")");
            File.Delete(WidgetWindow.PositionFile);
            card.CheckSpotFile();
            w.Close();
            return failed;
        }
    }
}
