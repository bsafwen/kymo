// Kymo desktop card: what the process list's right-click menu does to a process (Processes.cs draws the list).
// End it or its tree, set its priority or efficiency mode, open its file's folder or properties, search for it online,
// open its history in the dashboards. A process is named by its PID and the moment it started, so a later process
// given the same PID is never touched. The card runs with the user's rights: what Windows refuses (a service, another
// account's process) can be done again as administrator, once the user agrees: the installer's copy of this program,
// in C:\Program Files\Kymo where only administrators can change it, started elevated with --act (Windows asks first).
// A process Windows cannot run without is never ended, slowed or sped up.
using System;
using System.Collections.Generic;
using System.ComponentModel;
using System.Diagnostics;
using System.Drawing;
using System.Globalization;
using System.IO;
using System.Linq;
using System.Runtime.InteropServices;
using System.Threading;
using System.Windows.Forms;

namespace Kymo
{
    // A process to act on: its PID and its start (FILETIME), as the list read them.
    struct Target
    {
        public int Pid;
        public long Created;
        public Target(Proc p) { Pid = p.Pid; Created = p.Created; }
        public override string ToString() { return Pid.ToString(CultureInfo.InvariantCulture) + ":" + Created.ToString(CultureInfo.InvariantCulture); }

        // "1234:133725000000000000", as ToString writes it.
        public static bool TryParse(string s, out Target t)
        {
            t = new Target();
            var parts = (s ?? "").Split(':');
            int pid; long created;
            if (parts.Length != 2 || !int.TryParse(parts[0], NumberStyles.None, CultureInfo.InvariantCulture, out pid) || pid <= 0
                || !long.TryParse(parts[1], NumberStyles.None, CultureInfo.InvariantCulture, out created) || created <= 0) return false;
            t.Pid = pid; t.Created = created;
            return true;
        }
    }

    // How an action went, over all its processes.
    sealed class Outcome
    {
        public int Done, Gone, Critical, Failed;
        public readonly List<Target> Denied = new List<Target>();   // refused by Windows: to try again as administrator
        public int LastError;                                       // Windows' error for the last failure

        // --act's exit code: 0 done (or gone already), else 1 refused, 2 failed, 4 one Windows cannot run without left.
        public int Code { get { return (Denied.Count > 0 ? 1 : 0) | (Failed > 0 ? 2 : 0) | (Critical > 0 ? 4 : 0); } }
    }

    static class ProcActions
    {
        const uint TERMINATE = 0x0001, SET_INFORMATION = 0x0200, QUERY_LIMITED = 0x1000, SYNCHRONIZE = 0x100000;
        const int ERROR_ACCESS_DENIED = 5, ERROR_INVALID_PARAMETER = 87, STILL_ACTIVE = 259, ProcessPowerThrottling = 4;
        const uint ExecutionSpeed = 1;   // PROCESS_POWER_THROTTLING_EXECUTION_SPEED: EcoQoS

        // The priorities offered, highest first, the class each sets and the base priority each gives (as the list reads
        // it). Realtime is left out: it can starve the mouse, the keyboard and Windows itself, and only an administrator
        // gets it.
        public static readonly string[] Priorities = { "High", "Above normal", "Normal", "Below normal", "Low" };
        static readonly uint[] Classes = { 0x80, 0x8000, 0x20, 0x4000, 0x40 };
        public static readonly int[] Bases = { 13, 10, 8, 6, 4 };

        [DllImport("kernel32.dll", SetLastError = true)] static extern IntPtr OpenProcess(uint access, bool inherit, int pid);
        [DllImport("kernel32.dll")] static extern bool CloseHandle(IntPtr h);
        [DllImport("kernel32.dll", SetLastError = true)] static extern bool TerminateProcess(IntPtr h, uint code);
        [DllImport("kernel32.dll", SetLastError = true)] static extern bool GetProcessTimes(IntPtr h, out long created, out long exited, out long kernel, out long user);
        [DllImport("kernel32.dll", SetLastError = true)] static extern bool GetExitCodeProcess(IntPtr h, out int code);
        [DllImport("kernel32.dll", SetLastError = true)] static extern bool SetPriorityClass(IntPtr h, uint cls);
        [DllImport("kernel32.dll", SetLastError = true)] static extern bool IsProcessCritical(IntPtr h, out bool critical);
        [DllImport("kernel32.dll")] static extern uint WaitForSingleObject(IntPtr h, uint ms);
        [StructLayout(LayoutKind.Sequential)] struct PowerThrottling { public uint Version, ControlMask, StateMask; }
        [DllImport("kernel32.dll", SetLastError = true)] static extern bool SetProcessInformation(IntPtr h, int cls, ref PowerThrottling info, int size);
        [DllImport("kernel32.dll", SetLastError = true)] static extern bool GetProcessInformation(IntPtr h, int cls, ref PowerThrottling info, int size);
        [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
        struct OsVersion { public int Size, Major, Minor, Build, Platform; [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 128)] public string Csd; }
        [DllImport("ntdll.dll")] static extern int RtlGetVersion(ref OsVersion v);
        [DllImport("user32.dll", CharSet = CharSet.Unicode)] static extern int MessageBoxW(IntPtr owner, string text, string caption, uint type);
        [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
        struct ShellExecuteInfo
        {
            public int Size; public uint Mask; public IntPtr Hwnd; public string Verb, File, Parameters, Directory; public int Show;
            public IntPtr InstApp, IdList; public string Class; public IntPtr KeyClass; public uint HotKey; public IntPtr Icon, Process;
        }
        [DllImport("shell32.dll", CharSet = CharSet.Unicode, SetLastError = true)] static extern bool ShellExecuteExW(ref ShellExecuteInfo info);

        // Windows' real build (Environment.OSVersion stops at Windows 8 without a manifest saying otherwise).
        public static readonly int Build = ReadBuild();
        static int ReadBuild() { try { var v = new OsVersion { Size = Marshal.SizeOf(typeof(OsVersion)) }; return RtlGetVersion(ref v) == 0 ? v.Build : 0; } catch { return 0; } }

        // Efficiency mode, as Windows 11's Task Manager sets it: power throttling (EcoQoS) at the lowest priority.
        public static readonly bool EfficiencyShown = Build >= 22000;

        // The process, opened for `access`, if it is still the one the list read and still running: else counted in `o`
        // (gone, refused or failed) and zero.
        static IntPtr Open(Target t, uint access, Outcome o)
        {
            IntPtr h = OpenProcess(access | QUERY_LIMITED, false, t.Pid);
            if (h == IntPtr.Zero)
            {
                int e = Marshal.GetLastWin32Error();
                if (e == ERROR_ACCESS_DENIED) o.Denied.Add(t);
                else if (e == ERROR_INVALID_PARAMETER) o.Gone++;   // no such PID any more
                else { o.Failed++; o.LastError = e; }
                return IntPtr.Zero;
            }
            long created, exited, kernel, user; int code;
            if (!GetProcessTimes(h, out created, out exited, out kernel, out user) || created != t.Created || !GetExitCodeProcess(h, out code) || code != STILL_ACTIVE)
            {
                CloseHandle(h); o.Gone++;
                return IntPtr.Zero;
            }
            return h;
        }

        // Windows cannot run without it (ending it stops Windows), or that cannot be read: left alone.
        static bool Critical(IntPtr h) { bool critical; return !IsProcessCritical(h, out critical) || critical; }

        static void Refused(Outcome o, Target t)
        {
            int e = Marshal.GetLastWin32Error();
            if (e == ERROR_ACCESS_DENIED) o.Denied.Add(t); else { o.Failed++; o.LastError = e; }
        }

        // Ends each process at once (TerminateProcess, as Task Manager's End task), then waits up to a second in all for
        // them to be gone, so that the next reading no longer lists them.
        public static Outcome End(IList<Target> ts)
        {
            var o = new Outcome();
            var ended = new List<IntPtr>();
            foreach (var t in ts)
            {
                IntPtr h = Open(t, TERMINATE | SYNCHRONIZE, o);
                if (h == IntPtr.Zero) continue;
                if (Critical(h)) { o.Critical++; CloseHandle(h); continue; }
                if (TerminateProcess(h, 1)) { o.Done++; ended.Add(h); }
                else { Refused(o, t); CloseHandle(h); }
            }
            var waited = Stopwatch.StartNew();
            foreach (var h in ended) { WaitForSingleObject(h, (uint)Math.Max(0, 1000 - waited.ElapsedMilliseconds)); CloseHandle(h); }
            return o;
        }

        // `level`: an index in Priorities.
        public static Outcome SetPriority(IList<Target> ts, int level)
        {
            var o = new Outcome();
            foreach (var t in ts)
            {
                IntPtr h = Open(t, SET_INFORMATION, o);
                if (h == IntPtr.Zero) continue;
                if (Critical(h)) o.Critical++;
                else if (SetPriorityClass(h, Classes[level])) o.Done++; else Refused(o, t);
                CloseHandle(h);
            }
            return o;
        }

        // On: throttled (EcoQoS) at the lowest priority. Off: Windows decides again, at normal priority. The program's
        // other throttling choices are kept (Chromium's apps ask Windows not to ignore their timer resolution).
        public static Outcome SetEfficiency(IList<Target> ts, bool on)
        {
            var o = new Outcome();
            foreach (var t in ts)
            {
                IntPtr h = Open(t, SET_INFORMATION, o);
                if (h == IntPtr.Zero) continue;
                if (Critical(h)) { o.Critical++; CloseHandle(h); continue; }
                var s = new PowerThrottling { Version = 1 };
                if (!GetProcessInformation(h, ProcessPowerThrottling, ref s, Marshal.SizeOf(s))) s = new PowerThrottling { Version = 1 };
                s.Version = 1;
                if (on) { s.ControlMask |= ExecutionSpeed; s.StateMask |= ExecutionSpeed; }
                else { s.ControlMask &= ~ExecutionSpeed; s.StateMask &= ~ExecutionSpeed; }
                if (SetProcessInformation(h, ProcessPowerThrottling, ref s, Marshal.SizeOf(s)) && SetPriorityClass(h, on ? 0x40u : 0x20u)) o.Done++;
                else Refused(o, t);
                CloseHandle(h);
            }
            return o;
        }

        // A process's throttling as Windows keeps it (for --selftest): read, or set as the program itself would.
        internal static bool Throttling(Target t, ref uint control, ref uint state, bool set)
        {
            var o = new Outcome();
            IntPtr h = Open(t, set ? SET_INFORMATION : 0, o);
            if (h == IntPtr.Zero) return false;
            var s = new PowerThrottling { Version = 1, ControlMask = control, StateMask = state };
            bool ok = set ? SetProcessInformation(h, ProcessPowerThrottling, ref s, Marshal.SizeOf(s)) : GetProcessInformation(h, ProcessPowerThrottling, ref s, Marshal.SizeOf(s));
            CloseHandle(h);
            control = s.ControlMask; state = s.StateMask;
            return ok;
        }

        // All of them in efficiency mode (false when one cannot be read).
        public static bool Efficient(IList<Target> ts)
        {
            if (!EfficiencyShown || ts.Count == 0) return false;
            var o = new Outcome();
            foreach (var t in ts)
            {
                IntPtr h = Open(t, 0, o);
                if (h == IntPtr.Zero) return false;
                var s = new PowerThrottling { Version = 1 };
                bool read = GetProcessInformation(h, ProcessPowerThrottling, ref s, Marshal.SizeOf(s));
                CloseHandle(h);
                if (!read || (s.ControlMask & ExecutionSpeed) == 0 || (s.StateMask & ExecutionSpeed) == 0) return false;
            }
            return true;
        }

        // The processes and every process they started, from one reading, the given ones first: a child is a process
        // whose parent PID is its parent's and that started after it (the PID of a parent gone may be another's since).
        public static List<Proc> Tree(IEnumerable<Proc> roots, IEnumerable<Proc> all)
        {
            var byParent = all.ToLookup(p => p.Parent);
            var tree = new List<Proc>();
            var seen = new HashSet<string>();   // PID and start: a row and a later reading hold different objects for one process
            var next = new Queue<Proc>(roots);
            while (next.Count > 0)
            {
                var p = next.Dequeue();
                if (!seen.Add(new Target(p).ToString())) continue;
                tree.Add(p);
                foreach (var c in byParent[p.Pid]) if (c.Pid != p.Pid && c.Created >= p.Created) next.Enqueue(c);
            }
            return tree;
        }

        // An action by its name, as --act takes it: "end", "priority:0" (High) to "priority:4" (Low), "efficiency:on",
        // "efficiency:off". Null for anything else.
        public static Outcome Do(string action, IList<Target> ts)
        {
            int level;
            bool check = action.StartsWith("check:", StringComparison.Ordinal);   // understood? nothing done
            if (check) action = action.Substring(6);
            if (action == "end" || action == "efficiency:on" || action == "efficiency:off"
                || (action.StartsWith("priority:", StringComparison.Ordinal) && action.Length == 10
                    && int.TryParse(action.Substring(9), NumberStyles.None, CultureInfo.InvariantCulture, out level) && level < Classes.Length))
            {
                if (check) return new Outcome();
            }
            else return null;
            if (action == "end") return End(ts);
            if (action.StartsWith("priority:", StringComparison.Ordinal) && action.Length == 10
                && int.TryParse(action.Substring(9), NumberStyles.None, CultureInfo.InvariantCulture, out level) && level < Classes.Length) return SetPriority(ts, level);
            if (action == "efficiency:on" || action == "efficiency:off") return SetEfficiency(ts, action == "efficiency:on");
            return null;
        }

        // --act's exit codes carry this mark, so that a program that does not know --act (an older copy, which would start
        // as a card and stop at once, exit code 0) is never taken for one that did it.
        internal const int ActMark = 0x4B00;

        // KymoCard.exe --act <action> <pid>:<start> ...: started elevated by AsAdministrator. The exit code is ActMark with
        // the outcome's, or with 2 for anything not understood.
        public static int Act(string[] args)
        {
            if (args.Length < 3) return ActMark | 2;
            var ts = new List<Target>();
            for (int i = 2; i < args.Length; i++) { Target t; if (!Target.TryParse(args[i], out t)) return ActMark | 2; ts.Add(t); }
            if (Do("check:" + args[1], ts) == null) return ActMark | 2;
            // As Task Manager: an administrator opens a service's or another account's process only with this privilege on.
            try { Process.EnterDebugMode(); } catch (Exception) { }
            return ActMark | Do(args[1], ts).Code;
        }

        // The copy of this program the installer puts where only administrators can change it or add files next to it (a
        // .config, a DLL): the one run as administrator. The card itself runs from a folder the user can write to.
        public static readonly string AdminCopy = Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.ProgramFiles), "Kymo", "KymoCard.exe");

        // The action again as administrator: AdminCopy elevated (Windows asks first), with --act. `done` gets the outcome's
        // code (0 to 7) on `ui`'s thread; or -1 when Windows' prompt was declined or the program could not start, -2 when
        // it did not finish within a minute, -3 when there is no AdminCopy, -4 when AdminCopy does not know --act.
        public static void AsAdministrator(string action, IList<Target> ts, Control ui, Action<int> done)
        {
            if (!File.Exists(AdminCopy)) { done(-3); return; }
            Process p;
            try
            {
                p = Process.Start(new ProcessStartInfo(AdminCopy, "--act " + action + " " + string.Join(" ", ts.Select(t => t.ToString())))
                    { UseShellExecute = true, Verb = "runas", WindowStyle = ProcessWindowStyle.Hidden });
            }
            catch (Exception e)
            {
                if (!(e is Win32Exception && ((Win32Exception)e).NativeErrorCode == 1223)) DeskWindow.Log("as administrator: " + e.Message);   // 1223: declined
                done(-1);
                return;
            }
            if (p == null) { done(-1); return; }
            ThreadPool.QueueUserWorkItem(delegate
            {
                int code = -2;
                try { if (p.WaitForExit(60000)) code = (p.ExitCode & ~7) == ActMark ? p.ExitCode & 7 : -4; } catch (Exception) { }
                finally { p.Dispose(); }
                try { ui.BeginInvoke(done, code); } catch (InvalidOperationException) { }   // the window closed meanwhile
            });
        }

        // A question in front of everything: true for Yes (No is the default button).
        public static bool Ask(IntPtr owner, string text) { return MessageBoxW(owner, text, "Kymo", 0x4 | 0x30 | 0x100 | 0x10000 | 0x40000) == 6; }   // YESNO, WARNING, DEFBUTTON2, SETFOREGROUND, TOPMOST; IDYES
        public static void Tell(IntPtr owner, string text) { MessageBoxW(owner, text, "Kymo", 0x30 | 0x10000 | 0x40000); }

        // The file's Properties, as Explorer shows them.
        public static void Properties(string path)
        {
            var i = new ShellExecuteInfo { Size = Marshal.SizeOf(typeof(ShellExecuteInfo)), Mask = 0xC, Verb = "properties", File = path, Show = 5 };   // SEE_MASK_INVOKEIDLIST, SW_SHOW
            if (!ShellExecuteExW(ref i)) DeskWindow.Log("properties of " + path + ": " + new Win32Exception(Marshal.GetLastWin32Error()).Message);
        }

        // The app's name in the recorder's history: its file's name without ".exe".
        public static string AppName(string image)
        {
            return image.EndsWith(".exe", StringComparison.OrdinalIgnoreCase) && image.Length > 4 ? image.Substring(0, image.Length - 4) : image;
        }
    }

    // The right-click menu of a row: Windows' own popup menu (dark, like the list), at the mouse.
    static class ProcMenu
    {
        public const int EndTask = 1, EndTree = 2, Efficiency = 3, Location = 4, Properties = 5, Search = 6, History = 7, Settings = 8;
        public const int Priority = 10;   // + the index in ProcActions.Priorities

        // An entry: a command, a submenu (Sub) or a separator (Text null).
        public sealed class Item
        {
            public string Text;
            public int Id;
            public bool Checked, Radio, Grayed;
            public List<Item> Sub;
        }

        // What a row is, as the menu and the questions after it name it: "Code (18 processes)", "Code (PID 1234)".
        public static string Title(ProcRow row)
        {
            int n = row.Procs.Count;
            return row.Name + (n > 1 ? " (" + n.ToString(CultureInfo.InvariantCulture) + " processes)" : " (PID " + row.First.Pid.ToString(CultureInfo.InvariantCulture) + ")");
        }

        // What an entry does to processes: the action's name as ProcActions.Do takes it, and whether it is for the whole
        // tree; null for the entries that open something instead (and for none chosen).
        public static string Action(int id, bool efficient, out bool tree)
        {
            tree = id == EndTree;
            if (id == EndTask || id == EndTree) return "end";
            if (id == Efficiency) return efficient ? "efficiency:off" : "efficiency:on";
            int level = id - Priority;
            if (level >= 0 && level < ProcActions.Priorities.Length) return "priority:" + level.ToString(CultureInfo.InvariantCulture);
            return null;
        }

        // The entries for a row: what it is (grayed: the list may move under the menu), then what can be done to its
        // processes, `tree` (them and every process they started), whether all of them are in efficiency mode.
        public static List<Item> For(ProcRow row, IList<Proc> tree, bool efficient)
        {
            int n = row.Procs.Count;
            var items = new List<Item> { new Item { Text = Title(row).Replace("&", "&&"), Grayed = true }, new Item() };
            items.Add(new Item { Text = "End task" + (n > 1 ? " (" + n.ToString(CultureInfo.InvariantCulture) + " processes)" : ""), Id = EndTask });
            if (tree.Count > n) items.Add(new Item { Text = "End process tree (" + tree.Count.ToString(CultureInfo.InvariantCulture) + " processes)", Id = EndTree });
            // The priority its processes share, if one of those offered: the radio dot on it.
            var bases = row.Procs.Select(p => p.BasePriority).Distinct().ToList();
            int current = bases.Count == 1 ? Array.IndexOf(ProcActions.Bases, bases[0]) : -1;
            items.Add(new Item { Text = "Set priority", Sub = ProcActions.Priorities.Select((name, i) => new Item { Text = name, Id = Priority + i, Radio = true, Checked = i == current }).ToList() });
            if (ProcActions.EfficiencyShown) items.Add(new Item { Text = "Efficiency mode", Id = Efficiency, Checked = efficient });
            items.Add(new Item());
            bool file = row.First.Path != null;
            items.Add(new Item { Text = "Open file location", Id = Location, Grayed = !file });
            items.Add(new Item { Text = "Properties", Id = Properties, Grayed = !file });
            items.Add(new Item { Text = "Search online", Id = Search });
            items.Add(new Item { Text = "History in Process explorer", Id = History });
            items.Add(new Item());
            items.Add(new Item { Text = "Process list settings", Id = Settings });
            return items;
        }

        [DllImport("user32.dll")] static extern IntPtr CreatePopupMenu();
        [DllImport("user32.dll", CharSet = CharSet.Unicode)] static extern bool AppendMenuW(IntPtr menu, uint flags, IntPtr id, string text);
        [DllImport("user32.dll")] static extern bool CheckMenuRadioItem(IntPtr menu, uint first, uint last, uint check, uint flags);
        [DllImport("user32.dll")] static extern bool DestroyMenu(IntPtr menu);
        [DllImport("user32.dll")] static extern int TrackPopupMenuEx(IntPtr menu, uint flags, int x, int y, IntPtr owner, IntPtr tpm);
        [DllImport("user32.dll")] static extern bool SetForegroundWindow(IntPtr h);
        [DllImport("user32.dll")] static extern bool PostMessage(IntPtr h, int msg, IntPtr w, IntPtr l);
        [DllImport("user32.dll")] public static extern bool EndMenu();
        [DllImport("user32.dll")] static extern IntPtr GetForegroundWindow();
        [DllImport("user32.dll")] static extern bool IsWindow(IntPtr h);
        [DllImport("user32.dll")] static extern bool SetWindowPos(IntPtr h, IntPtr after, int x, int y, int cx, int cy, uint flags);

        public static IntPtr Foreground() { return GetForegroundWindow(); }

        // Back to the window the user was in, if the menu's owner still has the foreground (nothing else took it).
        public static void GiveBack(IntPtr owner, IntPtr before)
        {
            IntPtr now = GetForegroundWindow();
            if (before != IntPtr.Zero && before != owner && IsWindow(before) && (now == owner || now == IntPtr.Zero)) SetForegroundWindow(before);
        }

        // The owner at the click: the menu and the questions after it take that monitor's scaling, and the questions
        // open there.
        public static void MoveOwner(IntPtr owner, Point at) { SetWindowPos(owner, IntPtr.Zero, at.X, at.Y, 0, 0, 0x0001 | 0x0004 | 0x0010); }   // NOSIZE, NOZORDER, NOACTIVATE
        [DllImport("kernel32.dll", CharSet = CharSet.Unicode)] static extern IntPtr LoadLibraryW(string name);
        [DllImport("kernel32.dll", EntryPoint = "GetProcAddress")] static extern IntPtr GetProcAddressOrdinal(IntPtr module, IntPtr ordinal);
        [UnmanagedFunctionPointer(CallingConvention.Winapi)] delegate int SetPreferredAppModeFn(int mode);
        [UnmanagedFunctionPointer(CallingConvention.Winapi)] [return: MarshalAs(UnmanagedType.U1)] delegate bool AllowDarkModeForWindowFn(IntPtr h, [MarshalAs(UnmanagedType.U1)] bool allow);
        [UnmanagedFunctionPointer(CallingConvention.Winapi)] delegate void FlushMenuThemesFn();

        static bool darkTried;

        // Dark menus, as the desktop's own in dark mode: uxtheme's SetPreferredAppMode(ForceDark) and FlushMenuThemes
        // (ordinals 135 and 136 since Windows 10 1903). They are not documented: without them the menu is the light one.
        static void Dark(IntPtr owner)
        {
            if (darkTried) return;
            darkTried = true;
            try
            {
                if (ProcActions.Build < 18362) return;
                IntPtr ux = LoadLibraryW("uxtheme.dll");
                if (ux == IntPtr.Zero) return;
                IntPtr mode = GetProcAddressOrdinal(ux, (IntPtr)135), allow = GetProcAddressOrdinal(ux, (IntPtr)133), flush = GetProcAddressOrdinal(ux, (IntPtr)136);
                if (mode == IntPtr.Zero || flush == IntPtr.Zero) return;
                ((SetPreferredAppModeFn)Marshal.GetDelegateForFunctionPointer(mode, typeof(SetPreferredAppModeFn)))(2);   // ForceDark
                if (allow != IntPtr.Zero) ((AllowDarkModeForWindowFn)Marshal.GetDelegateForFunctionPointer(allow, typeof(AllowDarkModeForWindowFn)))(owner, true);
                ((FlushMenuThemesFn)Marshal.GetDelegateForFunctionPointer(flush, typeof(FlushMenuThemesFn)))();
            }
            catch (Exception e) { DeskWindow.Log("dark menu: " + e.Message); }
        }

        static IntPtr Build(List<Item> items)
        {
            IntPtr m = CreatePopupMenu();
            foreach (var it in items)
            {
                if (it.Text == null) { AppendMenuW(m, 0x800, IntPtr.Zero, null); continue; }   // MF_SEPARATOR
                if (it.Sub != null)
                {
                    IntPtr sub = Build(it.Sub);
                    AppendMenuW(m, 0x10, sub, it.Text);   // MF_POPUP
                    var radio = it.Sub.Where(s => s.Radio).ToList();
                    var on = radio.FirstOrDefault(s => s.Checked);
                    if (on != null) CheckMenuRadioItem(sub, (uint)radio.First().Id, (uint)radio.Last().Id, (uint)on.Id, 0);   // MF_BYCOMMAND
                    continue;
                }
                AppendMenuW(m, (it.Checked && !it.Radio ? 0x8u : 0u) | (it.Grayed ? 0x1u : 0u), (IntPtr)it.Id, it.Text);   // MF_CHECKED, MF_GRAYED
            }
            return m;
        }

        // Shown at `at` (screen px) until an entry is chosen or the menu closed: the entry's Id, or 0.
        public static int Show(IntPtr owner, Point at, List<Item> items)
        {
            Dark(owner);
            IntPtr m = Build(items);
            try
            {
                // A popup menu closes when the user clicks elsewhere only if its owner is the foreground window.
                SetForegroundWindow(owner);
                int id = TrackPopupMenuEx(m, 0x0100 | 0x0002 | 0x0080, at.X, at.Y, owner, IntPtr.Zero);   // TPM_RETURNCMD, TPM_RIGHTBUTTON, TPM_NONOTIFY
                PostMessage(owner, 0, IntPtr.Zero, IntPtr.Zero);   // WM_NULL, as Windows' notes on TrackPopupMenu ask
                return id;
            }
            finally { DestroyMenu(m); }   // its submenus with it
        }
    }

    // KymoCard.exe --selftest: the actions on a throwaway process tree of its own (cmd.exe running ping.exe, with the
    // console host Windows gives cmd), never on another process; the menu's entries; --act's arguments; the menu shown
    // and closed.
    static class ActionsSelfTest
    {
        static int failed;
        static void Check(bool ok, string what) { DeskWindow.Log((ok ? "ok    " : "FAIL  ") + "process actions: " + what); if (!ok) failed++; }

        [DllImport("user32.dll")] static extern IntPtr GetForegroundWindow();
        [DllImport("user32.dll")] static extern bool SetForegroundWindow(IntPtr h);

        static Proc Find(ProcSampler s, Proc p)
        {
            var r = s.Read();
            return r == null ? null : r.All.FirstOrDefault(x => x.Pid == p.Pid && x.Created == p.Created);
        }

        public static int Run()
        {
            failed = 0;
            Process cmd = null;
            using (var sampler = new ProcSampler())
                try
                {
                    string sys = Environment.SystemDirectory;
                    cmd = Process.Start(new ProcessStartInfo(Path.Combine(sys, "cmd.exe"), "/d /c \"\"" + Path.Combine(sys, "PING.EXE") + "\" -n 120 127.0.0.1 >nul\"")
                        { UseShellExecute = false, CreateNoWindow = true });
                    ProcReading r = null; Proc shell = null, child = null;
                    var wait = Stopwatch.StartNew();
                    while (child == null && wait.ElapsedMilliseconds < 5000)
                    {
                        Thread.Sleep(100);
                        r = sampler.Read();
                        shell = r == null ? null : r.All.FirstOrDefault(p => p.Pid == cmd.Id);
                        child = shell == null ? null : r.All.FirstOrDefault(p => p.Parent == shell.Pid && p.Created >= shell.Created && p.Image.Equals("PING.EXE", StringComparison.OrdinalIgnoreCase));
                    }
                    Check(shell != null && child != null, "a test process and the one it started read, with its parent");
                    if (child == null) return failed;
                    var me = new Target(shell);
                    var tree = ProcActions.Tree(new[] { shell }, r.All);
                    Check(tree[0] == shell && tree.Contains(child) && tree.Skip(1).All(p => p.Parent == shell.Pid) && tree.Count <= 3,
                        "its tree: the process first, then those it started (" + string.Join(", ", tree.Select(p => p.Image)) + ")");
                    Check(shell.BasePriority == 8, "its priority read as the list reads it: normal (" + shell.BasePriority + ")");

                    // Each priority offered: set, read back as the list reads it (the base its menu entry is dotted by); then normal.
                    var bases = new List<int>();
                    for (int i = 0; i < ProcActions.Priorities.Length; i++)
                    {
                        var set = ProcActions.SetPriority(new[] { me }, i);
                        var back = Find(sampler, shell);
                        bases.Add(set.Done == 1 && back != null ? back.BasePriority : -1);
                    }
                    ProcActions.SetPriority(new[] { me }, 2);
                    var again = Find(sampler, shell);
                    Check(bases.SequenceEqual(ProcActions.Bases) && again != null && again.BasePriority == 8,
                        "each priority set and read back as its menu entry expects (" + string.Join(", ", bases) + "), then normal again");

                    // Efficiency mode (Windows 11): on and read back, then off again at normal priority.
                    if (ProcActions.EfficiencyShown)
                    {
                        var on = ProcActions.SetEfficiency(new[] { me }, true);
                        bool isOn = ProcActions.Efficient(new[] { me });
                        var low = Find(sampler, shell);
                        var off = ProcActions.SetEfficiency(new[] { me }, false);
                        bool isOff = !ProcActions.Efficient(new[] { me });
                        var after = Find(sampler, shell);
                        Check(on.Done == 1 && isOn && low != null && low.BasePriority == 4 && off.Done == 1 && isOff && after != null && after.BasePriority == 8,
                            "efficiency mode on (lowest priority), read back, then off at normal priority");
                        // The program's own throttling choice (as Chromium's apps make it) kept through on and off.
                        uint control = 4, state = 0;
                        bool chose = ProcActions.Throttling(me, ref control, ref state, true);
                        ProcActions.SetEfficiency(new[] { me }, true);
                        uint c1 = 0, s1 = 0; ProcActions.Throttling(me, ref c1, ref s1, false);
                        ProcActions.SetEfficiency(new[] { me }, false);
                        uint c2 = 0, s2 = 0; ProcActions.Throttling(me, ref c2, ref s2, false);
                        Check(chose && c1 == 5 && s1 == 1 && c2 == 4 && s2 == 0, "a program's own throttling choice kept through efficiency mode on and off (" + c1 + "/" + s1 + ", " + c2 + "/" + s2 + ")");
                    }

                    // Named with a start it does not have (its PID since given to another): not touched.
                    var stale = ProcActions.End(new[] { new Target { Pid = shell.Pid, Created = shell.Created + 1 } });
                    Check(stale.Gone == 1 && stale.Done == 0 && !cmd.HasExited, "a PID with another start time: left alone");

                    // The menu's entries: for this process (normal priority, a child), and for a program of three with
                    // two priorities and no file to open.
                    var row = new ProcRow { Key = "pid:test", Name = "Windows Command Processor", First = shell };
                    row.Procs.Add(shell);
                    var items = ProcMenu.For(row, tree, false);
                    var prio = items.First(i => i.Sub != null).Sub;
                    var commands = items.Where(i => i.Text != null && !i.Grayed).Select(i => i.Id).ToList();
                    Check(items[0].Grayed && items[0].Id == 0 && items[0].Text.Contains("PID " + shell.Pid) && items[1].Text == null
                        && items[2].Id == ProcMenu.EndTask && items[3].Id == ProcMenu.EndTree && items[3].Text.Contains("(" + tree.Count + " processes)")
                        && prio.Count == 5 && prio.Single(i => i.Checked).Id == ProcMenu.Priority + 2
                        && new[] { ProcMenu.Location, ProcMenu.Properties, ProcMenu.Search, ProcMenu.History }.All(commands.Contains)
                        && items.Last().Id == ProcMenu.Settings && items.Any(i => i.Id == ProcMenu.Efficiency) == ProcActions.EfficiencyShown,
                        "the menu of a process: named first (grayed), End task, its tree, Normal priority dotted, its file, the settings last");
                    Check(!ProcActions.EfficiencyShown || ProcMenu.For(row, tree, true).Single(i => i.Id == ProcMenu.Efficiency).Checked,
                        "efficiency mode ticked when the processes are in it");
                    // Each entry's action: what it does, and to which processes.
                    bool whole, w2, w3, w4, w5, w6, w7;
                    var acts = new[] { ProcMenu.Action(ProcMenu.EndTask, false, out whole), ProcMenu.Action(ProcMenu.EndTree, false, out w2),
                        ProcMenu.Action(ProcMenu.Efficiency, false, out w3), ProcMenu.Action(ProcMenu.Efficiency, true, out w4),
                        ProcMenu.Action(ProcMenu.Priority, false, out w5), ProcMenu.Action(ProcMenu.Priority + 4, false, out w6), ProcMenu.Action(ProcMenu.Priority + 5, false, out w7) };
                    Check(string.Join(",", acts.Select(a => a ?? "-")) == "end,end,efficiency:on,efficiency:off,priority:0,priority:4,-" && !whole && w2 && !w3 && !w6
                        && new[] { 0, ProcMenu.Location, ProcMenu.Properties, ProcMenu.Search, ProcMenu.History, ProcMenu.Settings }.All(id => ProcMenu.Action(id, false, out w7) == null),
                        "each entry's action: End task its processes, End process tree the tree, efficiency toggled, the five priorities, the others open something");
                    var three = new ProcRow { Key = "x", Name = "Three", First = new Proc { Pid = 11, Image = "three.exe", BasePriority = 8 } };
                    three.Procs.Add(three.First);
                    three.Procs.Add(new Proc { Pid = 12, Image = "three.exe", BasePriority = 8 });
                    three.Procs.Add(new Proc { Pid = 13, Image = "three.exe", BasePriority = 6 });
                    var items3 = ProcMenu.For(three, three.Procs, false);
                    Check(items3[0].Text == "Three (3 processes)" && items3[2].Text == "End task (3 processes)" && !items3.Any(i => i.Id == ProcMenu.EndTree) && !items3.First(i => i.Sub != null).Sub.Any(i => i.Checked)
                        && items3.First(i => i.Id == ProcMenu.Location).Grayed && items3.First(i => i.Id == ProcMenu.Properties).Grayed,
                        "the menu of a program of three: End task for all, no tree, no priority dotted (two differ), no file");

                    // Shown at the mouse and closed again, without blocking (the test closes it).
                    var owner = new NativeWindow();
                    owner.CreateHandle(new CreateParams { Style = unchecked((int)0x80000000), ExStyle = 0x80 });   // WS_POPUP, WS_EX_TOOLWINDOW: never shown
                    IntPtr before = GetForegroundWindow();
                    var close = new System.Windows.Forms.Timer { Interval = 300 };
                    close.Tick += delegate { close.Stop(); ProcMenu.EndMenu(); };
                    close.Start();
                    var shown = Stopwatch.StartNew();
                    int chosen = ProcMenu.Show(owner.Handle, Cursor.Position, items);
                    close.Dispose();
                    owner.DestroyHandle();
                    if (before != IntPtr.Zero) SetForegroundWindow(before);
                    Check(chosen == 0 && shown.ElapsedMilliseconds < 3000, "the menu shown and closed without a choice (" + shown.ElapsedMilliseconds + " ms)");

                    // --act: what it does not understand, refused; a process gone already, not an error.
                    int refused = ProcActions.ActMark | 2;
                    Check(ProcActions.Act(new[] { "--act", "nonsense", me.ToString() }) == refused && ProcActions.Act(new[] { "--act", "end" }) == refused
                        && ProcActions.Act(new[] { "--act", "priority:9", me.ToString() }) == refused && ProcActions.Act(new[] { "--act", "end", me.Pid + ":x" }) == refused
                        && ProcActions.Act(new[] { "--act", "end", "0:5" }) == refused && ProcActions.Act(new[] { "--act", "check:end", me.ToString() }) == refused
                        && ProcActions.Do("check:end", new[] { me }).Done == 0 && !cmd.HasExited,
                        "--act refuses what it does not understand, and touched nothing");

                    // End process tree: all gone, as the next reading shows (the console host may end with cmd on its own).
                    var end = ProcActions.End(tree.Select(p => new Target(p)).ToList());
                    bool exited = cmd.WaitForExit(2000);
                    Check(end.Done >= 2 && end.Done + end.Gone == tree.Count && end.Code == 0 && exited && Find(sampler, child) == null && tree.All(p => Find(sampler, p) == null),
                        "the tree ended: the process and those it started gone (" + end.Done + " ended, " + end.Gone + " gone already)");
                    Check(ProcActions.Act(new[] { "--act", "end", me.ToString() }) == ProcActions.ActMark, "--act on a process gone already: nothing to do, no error");
                    // A row of a process and its child from one reading, the tree from the next (the same processes in new
                    // objects): each process once.
                    var copies = new[] { shell, child }.Select(p => new Proc { Pid = p.Pid, Created = p.Created, Parent = p.Parent, Image = p.Image }).ToList();
                    Check(ProcActions.Tree(new[] { shell, child }, copies).Count == 2, "a row from one reading, its tree from the next: each process once");
                    Check(ProcActions.AppName("Code.EXE") == "Code" && ProcActions.AppName("System") == "System", "the app's name in the history: its file without .exe");
                }
                finally
                {
                    // Whatever happened above: the test's own processes do not outlive it.
                    if (cmd != null)
                    {
                        try { if (!cmd.HasExited) { var r = sampler.Read(); var s = r == null ? null : r.All.FirstOrDefault(p => p.Pid == cmd.Id); if (s != null) ProcActions.End(ProcActions.Tree(new[] { s }, r.All).Select(p => new Target(p)).ToList()); } }
                        catch (Exception) { }
                        cmd.Dispose();
                    }
                }
            return failed;
        }
    }
}
