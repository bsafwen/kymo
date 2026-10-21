"""Export the previous stack's history (VictoriaMetrics, http://127.0.0.1:8428) for `kymo
import`: every figure the recorder keeps, per minute, from the first scrape up to the recorder's
first minute, plus the hourly rows made from them, for the system and for the apps.

    python -I import_vm.py --out FILE [--until UNIX_SECONDS]

Each minute is the window (t - 60 s, t], as the recorder's rows. Averages come from rate() and
avg_over_time(); the maxima of gauges from max_over_time() (for rates the maximum is the average:
VictoriaMetrics keeps 10-second scrapes, not the recorder's 10-second rates). Hourly rows weigh each
minute by its samples, as the recorder's own summaries do. The output format is in src/import.rs.
"""

import argparse
import json
import math
import re
import sys
import time
import urllib.parse
import urllib.request

VM = "http://127.0.0.1:8428"
REC = "http://127.0.0.1:21223"
OPENER = urllib.request.build_opener(urllib.request.ProxyHandler({}))  # loopback only, never a proxy

# Unit codes (src/units.rs, Unit::ALL order).
PCT, BYTES, BPS, PERSEC, SECS, MHZ, CELSIUS, WATTS, COUNT, BITS, TS, PERSEC_FINE, SECS_FINE = range(13)
W = '{job="windows"}'
N = '{job="nvidia_gpu"}'
APP_N = 9


def get_json(url, data=None):
    body = urllib.parse.urlencode(data).encode() if data else None
    with OPENER.open(urllib.request.Request(url, data=body), timeout=120) as r:
        return json.load(r)


def vm_range(expr, start, end):
    """{labels: {t: value}} for an expression at start, start+60, ..., end."""
    r = get_json(f"{VM}/api/v1/query_range", {"query": expr, "start": start, "end": end, "step": 60})
    out = {}
    for s in r["data"]["result"]:
        vals = {}
        for t, v in s["values"]:
            x = float(v)
            if math.isfinite(x):
                vals[int(round(float(t)))] = x
        out[tuple(sorted(s["metric"].items()))] = vals
    return out


def core_key(core):
    """windows_exporter's core label "0,7" -> "cpu.core.07" (as src/system.rs cpu_key)."""
    group, _, n = core.partition(",")
    if not n.isdigit():
        return None
    return f"cpu.core.{int(n):02d}" if group == "0" else f"cpu.core.{group}_{int(n):02d}"


def figures():
    """(key, or a function of the labels giving the key; unit; average expression; maximum
    expression or None) with {w} for the window."""
    f = []
    cpu = 'windows_cpu_time_total{job="windows",mode="%s"}'
    f.append(("cpu.busy", PCT, f'clamp(100 * (1 - avg(rate({cpu % "idle"}[{{w}}]))), 0, 100)', None))
    for mode in ("user", "privileged", "interrupt", "dpc"):
        f.append((f"cpu.{mode}", PCT, f"100 * avg(rate({cpu % mode}[{{w}}]))", None))
    # windows_exporter restarts rtc and mperf but not utility and performance: a minute with such a
    # restart (on any core: they all restart together) has no utility or clock rather than a wrong
    # one, as the old dashboards did. The guard looks a little wider than the minute, so the sample
    # before it counts too.
    reset = f" unless on () ((sum(resets(windows_cpu_processor_rtc_total{W}[90s])) + sum(resets(windows_cpu_processor_mperf_total{W}[90s]))) > 0)"
    f.append(("cpu.utility", PCT, f"sum(rate(windows_cpu_processor_utility_total{W}[{{w}}])) / sum(rate(windows_cpu_processor_rtc_total{W}[{{w}}]))" + reset, None))
    f.append(("cpu.clock", MHZ, f"1e-2 * avg(windows_cpu_core_frequency_mhz{W}) * sum(rate(windows_cpu_processor_performance_total{W}[{{w}}])) / sum(rate(windows_cpu_processor_mperf_total{W}[{{w}}]))" + reset, None))
    f.append(("cpu.frequency", MHZ, f"avg(avg_over_time(windows_cpu_core_frequency_mhz{W}[{{w}}]))", None))
    f.append(("cpu.limit", PCT, f"avg_over_time(windows_perf_cpu_performance_limit_percent{W}[{{w}}])", f"max_over_time(windows_perf_cpu_performance_limit_percent{W}[{{w}}])"))
    by_core = lambda m: core_key(dict(m).get("core", ""))  # noqa: E731
    f.append((lambda m: by_core(m) and by_core(m) + ".busy", PCT, f'clamp(100 * (1 - rate({cpu % "idle"}[{{w}}])), 0, 100)', None))
    f.append((lambda m: by_core(m) and by_core(m) + ".utility", PCT, f"rate(windows_cpu_processor_utility_total{W}[{{w}}]) / rate(windows_cpu_processor_rtc_total{W}[{{w}}])" + reset, None))
    f.append((lambda m: by_core(m) and by_core(m) + ".clock", MHZ, f"1e-2 * windows_cpu_core_frequency_mhz{W} * rate(windows_cpu_processor_performance_total{W}[{{w}}]) / rate(windows_cpu_processor_mperf_total{W}[{{w}}])" + reset, None))

    f.append(("sys.switches", PERSEC, f"rate(windows_system_context_switches_total{W}[{{w}}])", None))
    for key, metric in (("sys.queue", "windows_system_processor_queue_length"), ("sys.processes", "windows_system_processes"), ("sys.threads", "windows_system_threads")):
        f.append((key, COUNT, f"avg_over_time({metric}{W}[{{w}}])", f"max_over_time({metric}{W}[{{w}}])"))
    f.append(("sys.boot", TS, f"max_over_time(windows_system_boot_time_timestamp{W}[{{w}}])", None))
    f.append(("power.package", WATTS, f"rate(windows_perf_rapl_package_energy_picowatt_hours_total{W}[{{w}}]) * 3.6e-9", None))
    f.append(("power.cores", WATTS, f"rate(windows_perf_rapl_cores_energy_picowatt_hours_total{W}[{{w}}]) * 3.6e-9", None))

    for key, metric in (("mem.total", "windows_memory_physical_total_bytes"), ("mem.available", "windows_memory_available_bytes"),
                        ("mem.committed", "windows_memory_committed_bytes"), ("mem.commit_limit", "windows_memory_commit_limit"),
                        ("mem.free", "windows_memory_free_and_zero_page_list_bytes"), ("mem.modified", "windows_memory_modified_page_list_bytes"),
                        ("mem.standby_core", "windows_memory_standby_cache_core_bytes"),
                        ("mem.standby_normal", "windows_memory_standby_cache_normal_priority_bytes"),
                        ("mem.standby_reserve", "windows_memory_standby_cache_reserve_bytes"),
                        ("mem.pool_paged", "windows_memory_pool_paged_bytes"), ("mem.pool_nonpaged", "windows_memory_pool_nonpaged_bytes"),
                        ("mem.cache", "windows_memory_cache_bytes")):
        f.append((key, BYTES, f"avg_over_time({metric}{W}[{{w}}])", f"max_over_time({metric}{W}[{{w}}])"))
    f.append(("mem.pages_in", PERSEC, f"rate(windows_memory_swap_pages_read_total{W}[{{w}}])", None))
    f.append(("mem.pages_out", PERSEC, f"rate(windows_memory_swap_pages_written_total{W}[{{w}}])", None))
    f.append(("mem.faults", PERSEC, f"rate(windows_memory_page_faults_total{W}[{{w}}])", None))
    f.append(("pagefile.size", BYTES, f"sum(avg_over_time(windows_pagefile_limit_bytes{W}[{{w}}]))", None))
    f.append(("pagefile.used", BYTES, f"sum(avg_over_time(windows_pagefile_limit_bytes{W}[{{w}}])) - sum(avg_over_time(windows_pagefile_free_bytes{W}[{{w}}]))", None))

    vol = lambda field: lambda m: (lambda v: f"disk.{v}.{field}" if re.fullmatch(r"[A-Z]:", v) else None)(dict(m).get("volume", ""))  # noqa: E731
    d = "windows_logical_disk_%s" + W
    f.append((vol("active"), PCT, f"clamp(100 * (1 - rate({d % 'idle_seconds_total'}[{{w}}])), 0, 100)", None))
    f.append((vol("read_bps"), BPS, f"rate({d % 'read_bytes_total'}[{{w}}])", None))
    f.append((vol("write_bps"), BPS, f"rate({d % 'write_bytes_total'}[{{w}}])", None))
    f.append((vol("reads"), PERSEC_FINE, f"rate({d % 'reads_total'}[{{w}}])", None))
    f.append((vol("writes"), PERSEC_FINE, f"rate({d % 'writes_total'}[{{w}}])", None))
    f.append((vol("read_time"), SECS_FINE, f"rate({d % 'read_seconds_total'}[{{w}}])", None))
    f.append((vol("write_time"), SECS_FINE, f"rate({d % 'write_seconds_total'}[{{w}}])", None))
    f.append((vol("read_latency"), SECS, f"rate({d % 'read_seconds_total'}[{{w}}]) / (rate({d % 'reads_total'}[{{w}}]) > 0)", None))
    f.append((vol("write_latency"), SECS, f"rate({d % 'write_seconds_total'}[{{w}}]) / (rate({d % 'writes_total'}[{{w}}]) > 0)", None))
    f.append((vol("queue"), COUNT, f"avg_over_time({d % 'requests_queued'}[{{w}}])", f"max_over_time({d % 'requests_queued'}[{{w}}])"))
    f.append((vol("size"), BYTES, f"avg_over_time({d % 'size_bytes'}[{{w}}])", None))
    f.append((vol("free"), BYTES, f"avg_over_time({d % 'free_bytes'}[{{w}}])", None))

    f.append(("net.rx_bps", BPS, f"sum(rate(windows_net_bytes_received_total{W}[{{w}}]))", None))
    f.append(("net.tx_bps", BPS, f"sum(rate(windows_net_bytes_sent_total{W}[{{w}}]))", None))
    for field, metric, unit, scale in (("rx_bps", "bytes_received_total", BPS, ""), ("tx_bps", "bytes_sent_total", BPS, ""),
                                       ("rx_errors", "packets_received_errors_total", PERSEC, ""), ("tx_errors", "packets_outbound_errors_total", PERSEC, ""),
                                       ("rx_discards", "packets_received_discarded_total", PERSEC, ""), ("tx_discards", "packets_outbound_discarded_total", PERSEC, "")):
        f.append((("nic", field), unit, f"rate(windows_net_{metric}{W}[{{w}}]){scale}", None))
    f.append((("nic", "bandwidth"), BITS, f"8 * avg_over_time(windows_net_current_bandwidth_bytes{W}[{{w}}])", None))

    g = lambda m: f"avg(avg_over_time({m}{N}[{{w}}]))"  # noqa: E731
    gm = lambda m: f"avg(max_over_time({m}{N}[{{w}}]))"  # noqa: E731
    f.append(("gpu.util", PCT, "100 * " + g("nvidia_smi_utilization_gpu_ratio"), "100 * " + gm("nvidia_smi_utilization_gpu_ratio")))
    f.append(("gpu.mem_util", PCT, "100 * " + g("nvidia_smi_utilization_memory_ratio"), "100 * " + gm("nvidia_smi_utilization_memory_ratio")))
    f.append(("gpu.enc_util", PCT, "100 * " + g("nvidia_smi_utilization_encoder_ratio"), None))
    f.append(("gpu.dec_util", PCT, "100 * " + g("nvidia_smi_utilization_decoder_ratio"), None))
    f.append(("gpu.mem_used", BYTES, g("nvidia_smi_memory_used_bytes"), gm("nvidia_smi_memory_used_bytes")))
    f.append(("gpu.mem_total", BYTES, g("nvidia_smi_memory_total_bytes"), None))
    f.append(("gpu.temp", CELSIUS, g("nvidia_smi_temperature_gpu"), gm("nvidia_smi_temperature_gpu")))
    f.append(("gpu.power", WATTS, g("nvidia_smi_power_draw_watts"), gm("nvidia_smi_power_draw_watts")))
    f.append(("gpu.power_limit", WATTS, g("nvidia_smi_enforced_power_limit_watts"), None))
    for key, clock in (("clock_graphics", "graphics"), ("clock_mem", "memory"), ("clock_sm", "sm"), ("clock_video", "video")):
        f.append((f"gpu.{key}", MHZ, g(f"nvidia_smi_clocks_current_{clock}_clock_hz") + " / 1e6", gm(f"nvidia_smi_clocks_current_{clock}_clock_hz") + " / 1e6"))
    f.append(("gpu.fan", PCT, "100 * " + g("nvidia_smi_fan_speed_ratio"), None))
    f.append(("gpu.pstate", COUNT, g("nvidia_smi_pstate"), None))
    for key, reason in (("slow_idle", "gpu_idle"), ("slow_sw_power", "sw_power_cap"), ("slow_hw", "hw_slowdown"),
                        ("slow_sw_thermal", "sw_thermal_slowdown"), ("slow_hw_thermal", "hw_thermal_slowdown"),
                        ("slow_hw_power_brake", "hw_power_brake_slowdown")):
        f.append((f"gpu.{key}", PCT, "100 * " + g(f"nvidia_smi_clocks_event_reasons_{reason}"), None))
    return f


def adapter_names():
    """windows_exporter's nic label (non-alphanumerics as "_") -> the recorder's adapter name, from
    every adapter the recorder has stored (not only those up now)."""
    now = int(time.time())
    q = urllib.parse.urlencode({"keys": "net.*.rx_bps", "from": now - 730 * 86400, "to": now, "step": 86400})
    keys = [s["key"] for s in get_json(f"{REC}/api/query?{q}")["series"]]
    keys += [s["key"] for s in get_json(f"{REC}/api/series")["series"]]
    names = {k[4:-7] for k in keys if k.startswith("net.") and k.endswith(".rx_bps") and k.count(".") >= 2}
    # Both name adapters as the performance counters do ("Intel[R] Ethernet Controller [3] I225-V");
    # older windows_exporter versions turned other characters into "_".
    out = {re.sub(r"[^A-Za-z0-9]", "_", n): n for n in names}
    out.update({n: n for n in names})
    return out


def first_minute():
    """End of the recorder's first minute row: the import stops there."""
    now = int(time.time()) // 60 * 60
    for days in (3, 14, 60):
        q = urllib.parse.urlencode({"keys": "cpu.busy", "from": now - days * 86400, "to": now, "step": 60 if days == 3 else 3600})
        r = get_json(f"{REC}/api/query?{q}")
        if r["series"]:
            avg = r["series"][0]["avg"]
            for i, v in enumerate(avg):
                if v is not None:
                    return int(r["start"] + (i + 1) * r["step"]) // 60 * 60 - (0 if days == 3 else 3600)
    return now


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--out", required=True)
    p.add_argument("--until", type=int, default=None, help="last minute end to export (default: before the recorder's first minute)")
    p.add_argument("--since", type=int, default=None, help="first minute end to export (default: the first scrape; for checks)")
    a = p.parse_args()
    until = (a.until // 60 * 60) if a.until else first_minute() - 60
    up = vm_range('count_over_time(up{job="windows"}[90d])', until, until)
    first_scrape = vm_range('tfirst_over_time(up{job="windows"}[90d])', until, until)
    t0 = int(next(iter(next(iter(first_scrape.values())).values()))) if first_scrape else until
    start = (max(t0, a.since - 60) // 60 + 1) * 60 if a.since else (t0 // 60 + 1) * 60
    print(f"exporting {time.strftime('%Y-%m-%d %H:%M', time.localtime(start))} - {time.strftime('%Y-%m-%d %H:%M', time.localtime(until))}"
          f" ({(until - start) / 3600:.1f} h; {int(next(iter(next(iter(up.values())).values()))) if up else 0} scrapes in the history)")
    if until <= start:
        print("nothing to export")
        return

    # Samples per minute (10-second scrapes): the weight of each minute row.
    counts = {}
    for chunk in range(start, until + 1, 86400):
        c_end = min(until, chunk + 86400 - 60)
        for s in vm_range('count_over_time(up{job="windows"}[60s])', chunk, c_end).values():
            for t, v in s.items():
                counts[t] = counts.get(t, 0) + int(v)
    nics = adapter_names()
    unknown_nics = []
    rows = {}  # (t, key) -> [unit, avg, max]
    missing = []
    for key, unit, avg_expr, max_expr in figures():
        for label, expr in (("avg", avg_expr), ("max", max_expr)):
            if expr is None:
                continue
            got = {}
            for chunk in range(start, until + 1, 86400):
                c_end = min(until, chunk + 86400 - 60)
                got.update({lab: {**got.get(lab, {}), **vals} for lab, vals in vm_range(expr.replace("{w}", "60s"), chunk, c_end).items()})
            if label == "avg" and not got:
                missing.append(key if isinstance(key, str) else str(key))
            for labels, vals in got.items():
                if isinstance(key, str):
                    k = key
                elif isinstance(key, tuple):
                    # An adapter the recorder never saw is left out (its key could not be told).
                    nic = dict(labels).get("nic", "")
                    k = f"net.{nics[nic]}.{key[1]}" if nic in nics else None
                    if nic and k is None and nic not in unknown_nics:
                        unknown_nics.append(nic)
                else:
                    k = key(labels)
                if not k:
                    continue
                for t, v in vals.items():
                    if counts.get(t):
                        r = rows.setdefault((t, k), [unit, v, v])
                        if label == "avg":
                            r[1] = v
                            r[2] = max(r[2], v)
                        else:
                            r[2] = max(v, r[1])
    if missing:
        print(f"no data in the old history for: {', '.join(missing)}")
    if unknown_nics:
        print(f"adapters the recorder never saw, left out: {', '.join(unknown_nics)}")

    out = open(a.out, "w", encoding="utf-8", newline="\n")
    out.write("# kymo import from VictoriaMetrics\n")
    fmt = lambda v: repr(float(v)) if v is not None and math.isfinite(v) else "nan"  # noqa: E731
    minutes = sorted({t for t, _ in rows})
    for (t, k), (unit, avg, mx) in sorted(rows.items()):
        out.write(f"S\t1m\t{t * 1000}\t{counts[t]}\t{k}\t{unit}\t{fmt(avg)}\t{fmt(mx)}\n")
    # Hourly rows: averages weighted by the samples of each minute, maxima, samples added up.
    hours = {}
    for (t, k), (unit, avg, mx) in rows.items():
        h = (t + 3599) // 3600 * 3600
        e = hours.setdefault((h, k), [unit, 0.0, 0, -math.inf, 0])
        e[1] += avg * counts[t]
        e[2] += counts[t]
        e[3] = max(e[3], mx)
    hour_samples = {}
    for t in minutes:
        h = (t + 3599) // 3600 * 3600
        hour_samples[h] = hour_samples.get(h, 0) + counts[t]
    for (h, k), (unit, total, n, mx, _) in sorted(hours.items()):
        out.write(f"S\t1h\t{h * 1000}\t{hour_samples[h]}\t{k}\t{unit}\t{fmt(total / n)}\t{fmt(mx)}\n")
    print(f"system: {len(minutes)} minutes, {len({k for _, k in rows})} series, {len({h for h, _ in hours})} hours")

    # Apps per minute: cpu, user, kernel (% of one core), read, write (bytes/s), private working set,
    # private bytes, instances, seconds covered (src/store.rs APP_FIELDS). The per-app counters are
    # flushed on each minute mark, so a 60 s window holds exactly the flush ending the minute, and
    # rate() takes the one before it as its base: (t - 60, t] exactly. (A wider window would span
    # two flushes and average two minutes.)
    agg = "windows_process_%s:1m_without_%s"
    app_exprs = [
        f"100 * rate({agg % ('cpu_time_total', 'mode_process_id_total')}[60s])",
        f'100 * rate({agg % ("cpu_time_total", "process_id_total")}{{mode="user"}}[60s])',
        f'100 * rate({agg % ("cpu_time_total", "process_id_total")}{{mode="privileged"}}[60s])',
        f'rate({agg % ("io_bytes_total", "process_id_total")}{{mode="read"}}[60s])',
        f'rate({agg % ("io_bytes_total", "process_id_total")}{{mode="write"}}[60s])',
        f"last_over_time({agg % ('working_set_private_bytes', 'process_id_sum_samples')}[60s])",
        f"last_over_time({agg % ('private_bytes', 'process_id_sum_samples')}[60s])",
        f"last_over_time({agg % ('working_set_private_bytes', 'process_id_count_series')}[60s])",
    ]
    apps = {}  # (t, name) -> values
    for i, expr in enumerate(app_exprs):
        for chunk in range(start, until + 1, 86400):
            c_end = min(until, chunk + 86400 - 60)
            for labels, vals in vm_range(f"sum by (process) ({expr})", chunk, c_end).items():
                name = dict(labels).get("process", "")
                if not name or name in ("Idle", "_Total"):
                    continue
                for t, v in vals.items():
                    apps.setdefault((t, name), [math.nan] * APP_N)[i] = v
    app_minutes = sorted({t for t, _ in apps})
    for (t, name), v in sorted(apps.items()):
        v[APP_N - 1] = 60.0
        out.write(f"A\tapps\t{t * 1000}\t{name}\t" + "\t".join(fmt(x) for x in v) + "\n")
    # Hourly app rows as the recorder's AppRollup: averages over the hour's covered seconds (a
    # minute without the app counts as 0), maxima, seconds covered.
    hour_secs = {}
    for t in app_minutes:
        h = (t + 3599) // 3600 * 3600
        hour_secs[h] = hour_secs.get(h, 0) + 60.0
    happs = {}
    for (t, name), v in apps.items():
        h = (t + 3599) // 3600 * 3600
        e = happs.setdefault((h, name), ([0.0] * APP_N, [-math.inf] * APP_N))
        for i in range(APP_N - 1):
            if math.isfinite(v[i]):
                e[0][i] += v[i] * 60.0
                e[1][i] = max(e[1][i], v[i])
    for (h, name), (sums, maxes) in sorted(happs.items()):
        secs = hour_secs[h]
        avg = [s / secs for s in sums[:APP_N - 1]] + [secs]
        mx = [m if math.isfinite(m) else math.nan for m in maxes[:APP_N - 1]] + [math.nan]
        out.write(f"A\tapps-1h\t{h * 1000}\t{name}\t" + "\t".join(fmt(x) for x in avg + mx) + "\n")
    out.close()
    print(f"apps: {len(app_minutes)} minutes, {len({n for _, n in apps})} apps, {len(hour_secs)} hours -> {a.out}")


if __name__ == "__main__":
    sys.exit(main())
