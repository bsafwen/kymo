'use strict';
// The dashboards of Kymo: three pages drawn on canvas, with data from this origin's API.
// No libraries. Charts share one crosshair; drag across any chart to zoom; the browser's Back undoes.
(() => {

// ---------------------------------------------------------------- helpers

function el(tag, props, ...kids) {
  const e = document.createElement(tag);
  for (const [k, v] of Object.entries(props || {})) {
    if (v == null) continue;
    if (k === 'class') e.className = v;
    else if (k === 'text') e.textContent = v;
    else if (k.startsWith('on')) e.addEventListener(k.slice(2), v);
    else e.setAttribute(k, v);
  }
  for (const k of kids) if (k != null) e.append(k);
  return e;
}

const css = name => getComputedStyle(document.documentElement).getPropertyValue(name).trim();
const PALETTE = ['#7EB26D', '#EAB839', '#6ED0E0', '#EF843C', '#E24D42', '#1F78C1', '#BA43A9', '#705DA0',
  '#508642', '#CCA300', '#447EBC', '#C15C17', '#890F02', '#0A437C', '#6D1F62', '#584477'];
const C = { green: '#73bf69', yellow: '#fade2a', orange: '#ff9830', red: '#f2495c', blue: '#5794f2', purple: '#b877d9' };

function fixed(v, d) {
  const s = v.toFixed(d);
  return s.includes('.') ? s.replace(/0+$/, '').replace(/\.$/, '') : s;
}
function auto(v) {
  const a = Math.abs(v);
  return fixed(v, a >= 100 ? 0 : a >= 10 ? 1 : a >= 1 ? 2 : 3);
}
function scaled(v, base, units, sep = ' ') {
  let i = 0;
  while (Math.abs(v) >= base && i < units.length - 1) { v /= base; i++; }
  return auto(v) + sep + units[i];
}
function duration(s) {
  s = Math.max(0, Math.round(s));
  const d = Math.floor(s / 86400), h = Math.floor(s % 86400 / 3600), m = Math.floor(s % 3600 / 60);
  return d ? `${d} d ${h} h` : h ? `${h} h ${m} min` : `${m} min`;
}
const UNITS = {
  percent: v => auto(v) + ' %',
  bytes: v => scaled(v, 1024, ['B', 'KB', 'MB', 'GB', 'TB']),
  Bps: v => scaled(v, 1024, ['B/s', 'KB/s', 'MB/s', 'GB/s']),
  bps: v => scaled(v, 1000, ['b/s', 'kb/s', 'Mb/s', 'Gb/s']),
  watt: v => auto(v) + ' W',
  celsius: v => auto(v) + ' °C',
  mhz: v => Math.abs(v) >= 1000 ? fixed(v / 1000, 2) + ' GHz' : fixed(v, 0) + ' MHz',
  ms: v => auto(v) + ' ms',
  short: v => scaled(v, 1000, ['', 'k', 'M', 'G'], ''),
  iops: v => scaled(v, 1000, ['', 'k', 'M'], '') + '/s',
  pps: v => auto(v) + '/s',
  count: v => fixed(v, Math.abs(v) < 10 ? 1 : 0),
  duration: v => duration(v),
};
const BINARY = new Set(['bytes', 'Bps']);
function format(v, unit) {
  if (v == null || !Number.isFinite(v)) return '–';
  return (UNITS[unit] || UNITS.short)(v);
}

// Sizes count in binary units, as Windows shows them (the settings' GB too).
const GIB = 1073741824;

const pad = n => String(n).padStart(2, '0');
const hm = d => `${pad(d.getHours())}:${pad(d.getMinutes())}`;
const md = d => `${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;
function when(ms, withSeconds) {
  const d = new Date(ms);
  return `${d.getFullYear()}-${md(d)} ${hm(d)}${withSeconds ? ':' + pad(d.getSeconds()) : ''}`;
}

/** Values of several arrays combined point by point; null where any input is null. */
function combine(fn, ...arrays) {
  const n = Math.max(0, ...arrays.map(a => (a ? a.length : 0)));
  const out = new Array(n);
  for (let i = 0; i < n; i++) {
    const vs = arrays.map(a => (a ? a[i] : null));
    out[i] = vs.some(v => v == null) ? null : fn(...vs);
  }
  return out;
}
const times = (a, k) => a && a.map(v => (v == null ? null : v * k));
function mean(a) { let s = 0, n = 0; for (const v of a || []) if (v != null) { s += v; n++; } return n ? s / n : null; }
function max(a) { let m = null; for (const v of a || []) if (v != null && (m == null || v > m)) m = v; return m; }
function last(a) { for (let i = (a || []).length - 1; i >= 0; i--) if (a[i] != null) return a[i]; return null; }

// ---------------------------------------------------------------- API

async function getJSON(path, params) {
  const u = new URL(path, location.origin);
  for (const [k, v] of Object.entries(params || {})) u.searchParams.set(k, v);
  const r = await fetch(u, { cache: 'no-store' });
  const j = await r.json().catch(() => ({}));
  if (!r.ok) throw new Error(j.error || `${path}: HTTP ${r.status}`);
  return j;
}

/** System series for `keys` (globs allowed) over range `r` (ms) at `step` (ms); 64 keys a request. */
async function query(keys, r, step) {
  const chunks = [];
  for (let i = 0; i < keys.length; i += 64) chunks.push(keys.slice(i, i + 64));
  const res = await Promise.all(chunks.map(c =>
    getJSON('/api/query', { keys: c.join(','), from: r.from / 1000, to: r.to / 1000, step: Math.round(step / 1000) })));
  const map = new Map();
  let grid = null;
  for (const j of res) {
    grid = grid || gridOf(j);
    for (const s of j.series) map.set(s.key, s);
  }
  return { grid, map, avg: k => (map.get(k) || {}).avg || null, keys: [...map.keys()] };
}
const gridOf = j => ({ start: j.start * 1000, step: j.step * 1000, n: j.points });

/** One field of the named apps (64 names a request; "all other apps" only for one request's worth). */
async function appSeries(names, field, r, step, others) {
  if (!names.length) return { grid: null, list: [] };
  const chunks = [];
  for (let i = 0; i < names.length; i += 64) chunks.push(names.slice(i, i + 64));
  const res = await Promise.all(chunks.map(c => {
    const params = { names: c.join(','), field, from: r.from / 1000, to: r.to / 1000, step: Math.round(step / 1000) };
    if (others && chunks.length === 1) params.others = 1;
    return getJSON('/api/app', params);
  }));
  return { grid: gridOf(res[0]), list: res.flatMap(j => j.series) };
}

/** Values of `data` (on grid `src`) at the buckets of grid `dst`. */
function resample(src, data, dst) {
  if (!src || !data) return new Array(dst.n).fill(null);
  return Array.from({ length: dst.n }, (_, i) => {
    const j = Math.floor((dst.start + (i + 1) * dst.step - 1 - src.start) / src.step);
    return j >= 0 && j < data.length ? data[j] : null;
  });
}

// ---------------------------------------------------------------- shared chart state

const live = new Set();       // components on the page (crosshair, resize)
let hoverT = null;            // time under the mouse, shared by every chart
let bootMarks = [];           // Windows start times in the range (ms)
let dragging = null;          // the chart a zoom selection is being dragged on
const tip = () => document.getElementById('tip');
window.addEventListener('mouseup', e => { if (dragging) { const c = dragging; dragging = null; c.up(e); } });

function setHover(t) {
  hoverT = t;
  for (const c of live) if (c.drawOver) c.drawOver();
}
function hideTip() { tip().hidden = true; }
function showTip(e, html) {
  const t = tip();
  t.replaceChildren(...html);
  t.hidden = false;
  const w = t.offsetWidth, h = t.offsetHeight;
  let x = e.clientX + 14, y = e.clientY + 14;
  if (x + w > innerWidth - 8) x = e.clientX - w - 14;
  if (y + h > innerHeight - 8) y = innerHeight - h - 8;
  t.style.left = `${Math.max(4, x)}px`;
  t.style.top = `${Math.max(4, y)}px`;
}

function sizeCanvas(c, w, h) {
  const dpr = window.devicePixelRatio || 1;
  c.width = Math.round(w * dpr);
  c.height = Math.round(h * dpr);
  c.style.width = `${w}px`;
  c.style.height = `${h}px`;
  const ctx = c.getContext('2d');
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  return ctx;
}

const resizer = new ResizeObserver(entries => {
  for (const e of entries) {
    try {
      if (e.target.component) e.target.component.draw();
    } catch (err) { console.error(err); } // one chart's problem never blanks the others
  }
});

/** Time labels along the bottom, leaving out those that would be cut at either edge. */
function timeLabels(ctx, t0, t1, x, w, h, plotW, grid) {
  const muted = css('--muted'), gridColor = css('--grid');
  ctx.textAlign = 'center';
  ctx.textBaseline = 'alphabetic';
  for (const [t, text] of timeTicks(t0, t1, Math.max(2, Math.floor(plotW / 85)))) {
    const xx = Math.round(x(t)) + 0.5;
    if (grid) {
      ctx.strokeStyle = gridColor;
      ctx.beginPath(); ctx.moveTo(xx, grid[0]); ctx.lineTo(xx, grid[1]); ctx.stroke();
    }
    const half = ctx.measureText(text).width / 2;
    if (xx - half < 0 || xx + half > w) continue;
    ctx.fillStyle = muted;
    ctx.fillText(text, xx, h - 4);
  }
}

/** Green to yellow to red for 0 to 1. */
function heatColor(f) {
  f = Math.max(0, Math.min(1, f));
  const [a, b, k] = f < 0.5 ? [[115, 191, 105], [250, 222, 42], f * 2] : [[250, 222, 42], [242, 73, 92], (f - 0.5) * 2];
  return `rgb(${a.map((c, i) => Math.round(c + (b[i] - c) * k)).join(',')})`;
}

function niceStep(span, count) {
  const raw = span / Math.max(1, count);
  const p = Math.pow(10, Math.floor(Math.log10(raw)));
  const f = raw / p;
  return (f <= 1 ? 1 : f <= 2 ? 2 : f <= 5 ? 5 : 10) * p;
}
function valueTicks(lo, hi, count, unit) {
  let scale = 1;
  if (BINARY.has(unit)) {
    const m = Math.max(Math.abs(lo), Math.abs(hi));
    while (m / scale >= 1024) scale *= 1024;
  }
  const st = niceStep((hi - lo) / scale, count) * scale;
  const out = [];
  for (let v = Math.ceil(lo / st - 1e-9) * st; v <= hi + st * 1e-6; v += st) out.push(Math.abs(v) < st * 1e-9 ? 0 : v);
  return out;
}

const TIME_STEPS = [10, 30, 60, 120, 300, 600, 900, 1800, 3600, 7200, 10800, 21600, 43200, 86400, 172800, 604800,
  1209600, 2592000, 7776000, 15552000, 31536000].map(s => s * 1000);
function timeTicks(t0, t1, count) {
  const span = t1 - t0;
  const st = TIME_STEPS.find(s => span / s <= count) || TIME_STEPS[TIME_STEPS.length - 1];
  const out = [];
  if (st < 86400000) {
    // On local clock times: hh:00, hh:30...
    const off = new Date(t0).getTimezoneOffset() * 60000;
    for (let t = Math.ceil((t0 - off) / st) * st + off; t <= t1; t += st) out.push(t);
  } else {
    const d = new Date(t0);
    d.setHours(0, 0, 0, 0);
    const days = st / 86400000;
    while (d.getTime() < t0) d.setDate(d.getDate() + 1);
    for (; d.getTime() <= t1; d.setDate(d.getDate() + days)) out.push(d.getTime());
  }
  const label = t => {
    const d = new Date(t);
    return st >= 86400000 || (d.getHours() === 0 && d.getMinutes() === 0) ? md(d) : hm(d);
  };
  return out.map(t => [t, label(t)]);
}

// ---------------------------------------------------------------- panels

function row(parent, title) {
  parent.append(el('h2', { class: 'row', text: title }));
  const g = el('div', { class: 'grid' });
  parent.append(g);
  return g;
}

function panel(parent, title, desc, span, extraClass) {
  const p = el('section', { class: `panel w${span}${extraClass ? ' ' + extraClass : ''}` });
  const head = el('div', { class: 'phead' }, el('h3', { text: title, title }), desc ? el('span', { class: 'info', title: desc, text: 'i' }) : null);
  const body = el('div', { class: 'pbody' });
  p.append(head, body);
  parent.append(p);
  return { el: p, body };
}

/** Time series chart: lines or stacked areas, an optional right axis, shared crosshair, drag to zoom. */
class Chart {
  constructor(parent, title, desc, span, cfg) {
    this.cfg = Object.assign({ height: 190, legend: 'list', fill: 0.1 }, cfg);
    this.p = panel(parent, title, desc, span);
    this.plot = el('div', { class: 'plot' });
    this.plot.style.height = `${this.cfg.height}px`;
    this.base = el('canvas');
    this.over = el('canvas', { class: 'over' });
    this.plot.append(this.base, this.over);
    if (this.cfg.legend === 'table') {
      this.table = el('table', { class: 'ltable' });
      this.p.body.append(el('div', { class: 'with-table' }, this.plot, this.table));
    } else {
      this.legend = el('div', { class: 'legend' });
      this.p.body.append(this.plot, this.legend);
    }
    this.series = [];
    this.hidden = new Set();
    this.grid = null;
    this.range = null;
    this.plot.component = this;
    resizer.observe(this.plot);
    live.add(this);
    this.over.addEventListener('mousemove', e => this.move(e));
    this.over.addEventListener('mouseleave', () => { if (!this.drag) { setHover(null); hideTip(); } });
    this.over.addEventListener('mousedown', e => {
      this.drag = { x0: this.px(e), x1: this.px(e) };
      dragging = this;
      e.preventDefault();
    });
  }

  set(grid, range, series) {
    this.grid = grid;
    this.range = range;
    this.series = series.filter(s => s && s.data);
    this.renderLegend();
    this.draw();
  }

  px(e) { return e.clientX - this.over.getBoundingClientRect().left; }

  timeAt(px) {
    const L = this.layout;
    return L ? L.t0 + (px - L.pl) / L.w * (L.t1 - L.t0) : null;
  }

  move(e) {
    const L = this.layout;
    if (!L) return;
    const px = this.px(e);
    if (this.drag) { this.drag.x1 = px; }
    if (px < L.pl || px > L.pl + L.w) { setHover(null); hideTip(); return; }
    const t = this.timeAt(px);
    setHover(t);
    this.tooltip(e, t);
  }

  up(e) {
    if (!this.drag) return;
    const d = this.drag;
    this.drag = null;
    this.drawOver();
    if (Math.abs(d.x1 - d.x0) > 6 && this.layout) {
      const a = this.timeAt(Math.min(d.x0, d.x1)), b = this.timeAt(Math.max(d.x0, d.x1));
      app.zoom(Math.max(a, this.layout.t0), Math.min(b, this.layout.t1));
    }
  }

  index(t) {
    const g = this.grid;
    if (!g) return -1;
    return Math.min(g.n - 1, Math.max(0, Math.round((t - g.start) / g.step - 1)));
  }

  tooltip(e, t) {
    const g = this.grid;
    if (!g) return;
    const i = this.index(t);
    const end = g.start + (i + 1) * g.step;
    const lines = [];
    for (const s of this.series) {
      if (this.hidden.has(s.name)) continue;
      const v = s.data[i];
      if (v == null) continue;
      lines.push([s, v]);
    }
    lines.sort((a, b) => b[1] - a[1]);
    const head = g.step > 60000 ? `${when(end - g.step)} – ${hm(new Date(end))}` : when(end, true);
    const html = [el('div', { class: 'when', text: head })];
    for (const [s, v] of lines.slice(0, 16)) {
      const sw = el('span', { class: 'swatch' });
      sw.style.background = s.color;
      html.push(el('div', { class: 'line' }, sw, el('span', { class: 'n', text: s.name }),
        el('span', { class: 'v', text: format(v, s.right ? this.cfg.rightUnit : this.cfg.unit) })));
    }
    if (lines.length > 16) html.push(el('div', { class: 'when', text: `and ${lines.length - 16} more` }));
    if (!lines.length) html.push(el('div', { class: 'when', text: 'no data' }));
    showTip(e, html);
  }

  renderLegend() {
    const seen = new Set();
    // Series without a single value in the range stay out of the legend.
    const list = this.series.filter(s => !seen.has(s.name) && seen.add(s.name) && s.data.some(v => v != null));
    if (this.table) {
      const calcs = this.cfg.calcs || ['mean', 'max'];
      const fn = { mean, max, last };
      const rows = list.map(s => [s, calcs.map(c => fn[c](s.data))]);
      rows.sort((a, b) => (b[1][0] ?? -Infinity) - (a[1][0] ?? -Infinity));
      const head = el('tr', {}, el('th', { text: 'Name' }), ...calcs.map(c => el('th', { text: c })));
      const body = rows.map(([s, vals]) => {
        const sw = el('span', { class: 'swatch' });
        sw.style.background = s.color;
        const name = el('td', { class: 'name', title: s.name }, el('span', { class: 'item' }, sw, el('span', { text: s.name })));
        name.addEventListener('click', () => (this.cfg.onName && s.link !== false ? this.cfg.onName(s.name) : this.toggle(s.name)));
        const tr = el('tr', { class: this.hidden.has(s.name) ? 'off' : null }, name, ...vals.map(v => el('td', { text: format(v, this.cfg.unit) })));
        return tr;
      });
      this.table.replaceChildren(el('thead', {}, head), el('tbody', {}, ...body));
      return;
    }
    if (this.cfg.legend === 'none') { this.legend.replaceChildren(); return; }
    this.legend.replaceChildren(...list.map(s => {
      const sw = el('span', { class: 'swatch' + (s.dash ? ' dash' : '') });
      if (s.dash) sw.style.borderTopColor = s.color; else sw.style.background = s.color;
      return el('span', { class: 'item' + (this.hidden.has(s.name) ? ' off' : ''), onclick: () => this.toggle(s.name) }, sw, el('span', { text: s.name }));
    }));
  }

  toggle(name) {
    if (this.hidden.has(name)) this.hidden.delete(name); else this.hidden.add(name);
    this.renderLegend();
    this.draw();
  }

  draw() {
    const w = this.plot.clientWidth, h = this.cfg.height;
    if (!w) return;
    const ctx = sizeCanvas(this.base, w, h);
    sizeCanvas(this.over, w, h);
    ctx.font = '11px "Segoe UI", system-ui, sans-serif';
    const muted = css('--muted'), gridColor = css('--grid');
    const g = this.grid, r = this.range;
    const vis = this.series.filter(s => !this.hidden.has(s.name));
    if (!g || !r || !vis.length || vis.every(s => s.data.every(v => v == null))) {
      this.layout = null;
      ctx.fillStyle = muted;
      ctx.fillText('No data in this range', 8, 18);
      return;
    }
    // Values as drawn: negative-Y series flipped, stacked series on top of each other.
    const n = g.n;
    const stack = { left: new Array(n).fill(0) };
    const lines = vis.map(s => {
      const sign = s.neg ? -1 : 1;
      let top = s.data.map(v => (v == null ? null : v * sign));
      let bottom = null;
      if (this.cfg.stacked && !s.right && s.stack !== false) {
        const base = stack.left;
        bottom = base.slice();
        top = top.map((v, i) => (v == null ? null : base[i] + v));
        top.forEach((v, i) => { if (v != null) base[i] = v; });
      }
      return { s, top, bottom };
    });
    const extent = right => {
      let lo = Infinity, hi = -Infinity;
      for (const l of lines) {
        if (!!l.s.right !== right) continue;
        for (const v of l.top) if (v != null) { if (v < lo) lo = v; if (v > hi) hi = v; }
      }
      if (lo === Infinity) return null;
      const c = right ? this.cfg.rightRange || {} : this.cfg;
      lo = c.min != null ? c.min : Math.min(0, lo);
      hi = c.max != null ? c.max : Math.max(hi, c.softMax != null ? c.softMax : -Infinity);
      if (!(hi > lo)) hi = lo + (Math.abs(lo) || 1);
      return [lo, hi];
    };
    const L = extent(false) || [0, 1], R = extent(true);
    const count = Math.max(2, Math.floor(h / 45));
    const ticksL = valueTicks(L[0], L[1], count, this.cfg.unit);
    const ticksR = R ? valueTicks(R[0], R[1], count, this.cfg.rightUnit) : [];
    const abs = this.series.some(s => s.neg);
    const lab = (v, unit) => format(abs ? Math.abs(v) : v, unit);
    const wl = Math.max(...ticksL.map(v => ctx.measureText(lab(v, this.cfg.unit)).width), 10);
    const wr = R ? Math.max(...ticksR.map(v => ctx.measureText(lab(v, this.cfg.rightUnit)).width), 10) : 0;
    const pl = Math.ceil(wl) + 8, pr = R ? Math.ceil(wr) + 8 : 6, pt = 6, pb = 18;
    const pw = Math.max(10, w - pl - pr), ph = Math.max(10, h - pt - pb);
    const t0 = r.from, t1 = r.to;
    const x = t => pl + (t - t0) / (t1 - t0) * pw;
    const yOf = ([lo, hi]) => v => pt + (hi - v) / (hi - lo) * ph;
    const yL = yOf(L), yR = R ? yOf(R) : null;
    this.layout = { pl, w: pw, t0, t1, pt, ph };

    // Grid and axes.
    ctx.lineWidth = 1;
    ctx.fillStyle = muted;
    ctx.textBaseline = 'middle';
    for (const v of ticksL) {
      if (v < L[0] - 1e-9 || v > L[1] + 1e-9) continue;
      const y = Math.round(yL(v)) + 0.5;
      ctx.strokeStyle = v === 0 && L[0] < 0 ? muted : gridColor;
      ctx.beginPath(); ctx.moveTo(pl, y); ctx.lineTo(pl + pw, y); ctx.stroke();
      ctx.textAlign = 'right';
      ctx.fillText(lab(v, this.cfg.unit), pl - 5, y);
    }
    if (R) {
      ctx.textAlign = 'left';
      for (const v of ticksR) {
        if (v < R[0] - 1e-9 || v > R[1] + 1e-9) continue;
        ctx.fillText(lab(v, this.cfg.rightUnit), pl + pw + 5, yR(v));
      }
    }
    timeLabels(ctx, t0, t1, x, w, h, pw, [pt, pt + ph]);
    for (const b of bootMarks) {
      if (b < t0 || b > t1) continue;
      const xx = Math.round(x(b)) + 0.5;
      ctx.strokeStyle = C.purple;
      ctx.setLineDash([3, 3]);
      ctx.beginPath(); ctx.moveTo(xx, pt); ctx.lineTo(xx, pt + ph); ctx.stroke();
      ctx.setLineDash([]);
    }

    // Series: areas first, then lines.
    ctx.save();
    ctx.beginPath(); ctx.rect(pl, pt, pw, ph); ctx.clip();
    const tx = i => x(g.start + (i + 1) * g.step);
    const segments = top => {
      const out = [];
      let cur = null;
      top.forEach((v, i) => {
        if (v == null) { cur = null; return; }
        if (!cur) { cur = []; out.push(cur); }
        cur.push(i);
      });
      return out;
    };
    for (const { s, top, bottom } of lines) {
      const fill = s.fill != null ? s.fill : this.cfg.stacked && !s.right ? 0.45 : this.cfg.fill;
      if (!fill) continue;
      const y = s.right ? yR : yL;
      ctx.fillStyle = s.color;
      ctx.globalAlpha = fill;
      for (const seg of segments(top)) {
        ctx.beginPath();
        seg.forEach((i, k) => (k ? ctx.lineTo(tx(i), y(top[i])) : ctx.moveTo(tx(i), y(top[i]))));
        for (let k = seg.length - 1; k >= 0; k--) {
          const i = seg[k];
          ctx.lineTo(tx(i), y(bottom ? bottom[i] : 0));
        }
        ctx.closePath();
        ctx.fill();
      }
    }
    ctx.globalAlpha = 1;
    ctx.lineWidth = 1.4;
    for (const { s, top } of lines) {
      const y = s.right ? yR : yL;
      ctx.strokeStyle = s.color;
      ctx.setLineDash(s.dash ? [6, 4] : []);
      for (const seg of segments(top)) {
        ctx.beginPath();
        if (seg.length === 1) {
          ctx.arc(tx(seg[0]), y(top[seg[0]]), 1.5, 0, 2 * Math.PI);
          ctx.fillStyle = s.color;
          ctx.fill();
          continue;
        }
        seg.forEach((i, k) => (k ? ctx.lineTo(tx(i), y(top[i])) : ctx.moveTo(tx(i), y(top[i]))));
        ctx.stroke();
      }
    }
    ctx.setLineDash([]);
    ctx.restore();
    this.drawOver();
  }

  drawOver() {
    const L = this.layout;
    const ctx = this.over.getContext('2d');
    const dpr = window.devicePixelRatio || 1;
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx.clearRect(0, 0, this.over.width, this.over.height);
    if (!L) return;
    if (hoverT != null && hoverT >= L.t0 && hoverT <= L.t1) {
      const xx = Math.round(L.pl + (hoverT - L.t0) / (L.t1 - L.t0) * L.w) + 0.5;
      ctx.strokeStyle = css('--muted');
      ctx.lineWidth = 1;
      ctx.beginPath(); ctx.moveTo(xx, L.pt); ctx.lineTo(xx, L.pt + L.ph); ctx.stroke();
    }
    if (this.drag) {
      const a = Math.max(L.pl, Math.min(this.drag.x0, this.drag.x1)), b = Math.min(L.pl + L.w, Math.max(this.drag.x0, this.drag.x1));
      ctx.fillStyle = 'rgba(87, 148, 242, 0.18)';
      ctx.fillRect(a, L.pt, b - a, L.ph);
    }
  }
}

/** Rows of cells coloured by value (0-100), for example one row per logical processor. */
class Heat {
  constructor(parent, title, desc, span, cfg) {
    this.cfg = Object.assign({ rowHeight: 11, unit: 'percent' }, cfg);
    this.p = panel(parent, title, desc, span);
    this.plot = el('div', { class: 'plot' });
    this.base = el('canvas');
    this.over = el('canvas', { class: 'over' });
    this.plot.append(this.base, this.over);
    this.p.body.append(this.plot);
    this.rows = [];
    this.plot.component = this;
    resizer.observe(this.plot);
    live.add(this);
    this.over.addEventListener('mousemove', e => this.move(e));
    this.over.addEventListener('mouseleave', () => { if (!this.drag) { setHover(null); hideTip(); } });
    this.over.addEventListener('mousedown', e => {
      this.drag = { x0: this.px(e), x1: this.px(e) };
      dragging = this;
      e.preventDefault();
    });
  }

  set(grid, range, rows) {
    this.grid = grid;
    this.range = range;
    // A row without data (the source was not recording then) draws as empty.
    this.rows = rows.map(r => Object.assign({}, r, { data: r.data || new Array(grid ? grid.n : 0).fill(null) }));
    this.plot.style.height = `${rows.length * this.cfg.rowHeight + 22}px`;
    this.draw();
  }

  color(v) { return heatColor(v / 100); }

  px(e) { return Chart.prototype.px.call(this, e); }
  timeAt(px) { return Chart.prototype.timeAt.call(this, px); }
  up(e) { Chart.prototype.up.call(this, e); }

  draw() {
    const w = this.plot.clientWidth;
    const h = this.rows.length * this.cfg.rowHeight + 22;
    if (!w || !this.grid) return;
    const ctx = sizeCanvas(this.base, w, h);
    sizeCanvas(this.over, w, h);
    ctx.font = '11px "Segoe UI", system-ui, sans-serif';
    const muted = css('--muted');
    const pl = 52, pt = 2, rh = this.cfg.rowHeight, pw = w - pl - 6;
    const { start, step, n } = this.grid, t0 = this.range.from, t1 = this.range.to;
    const x = t => pl + (t - t0) / (t1 - t0) * pw;
    this.layout = { pl, w: pw, t0, t1, pt, ph: this.rows.length * rh };
    ctx.textBaseline = 'middle';
    this.rows.forEach((r, j) => {
      const y = pt + j * rh;
      ctx.fillStyle = muted;
      ctx.textAlign = 'right';
      ctx.fillText(r.name, pl - 6, y + rh / 2);
      for (let i = 0; i < n; i++) {
        const v = r.data[i];
        if (v == null) continue;
        const xa = x(start + i * step), xb = x(start + (i + 1) * step);
        if (xb < pl || xa > pl + pw) continue;
        ctx.fillStyle = this.color(v);
        ctx.fillRect(Math.max(pl, xa), y, Math.min(pl + pw, xb) - Math.max(pl, xa) + 0.5, rh - 1);
      }
    });
    timeLabels(ctx, t0, t1, x, w, h, pw, null);
    this.drawOver();
  }

  drawOver() { Chart.prototype.drawOver.call(this); }

  move(e) {
    const L = this.layout;
    if (!L) return;
    const r = this.over.getBoundingClientRect();
    const px = e.clientX - r.left, py = e.clientY - r.top;
    if (this.drag) this.drag.x1 = px;
    const t = L.t0 + (px - L.pl) / L.w * (L.t1 - L.t0);
    if (px < L.pl) { setHover(null); hideTip(); return; }
    setHover(t);
    const j = Math.floor((py - L.pt) / this.cfg.rowHeight);
    const rowData = this.rows[j];
    if (!rowData) { hideTip(); return; }
    // The cell under the mouse: cell i covers (start + i*step, start + (i+1)*step].
    const g = this.grid;
    const i = Math.min(g.n - 1, Math.max(0, Math.floor((t - g.start) / g.step)));
    const end = g.start + (i + 1) * g.step;
    showTip(e, [el('div', { class: 'when', text: when(end, this.grid.step < 60000) }),
      el('div', { class: 'line' }, el('span', { class: 'n', text: rowData.name }), el('span', { class: 'v', text: format(rowData.data[i], this.cfg.unit) }))]);
  }
}

/** Named rows that light up while a condition holds (the value is the % of the time it held). */
class Timeline extends Heat {
  constructor(parent, title, desc, span) {
    super(parent, title, desc, span, { rowHeight: 18, unit: 'percent' });
  }
  draw() {
    const w = this.plot.clientWidth;
    const h = this.rows.length * this.cfg.rowHeight + 22;
    if (!w || !this.grid) return;
    const ctx = sizeCanvas(this.base, w, h);
    sizeCanvas(this.over, w, h);
    ctx.font = '11px "Segoe UI", system-ui, sans-serif';
    const muted = css('--muted');
    const pl = 110, pt = 2, rh = this.cfg.rowHeight, pw = w - pl - 6;
    const { start, step, n } = this.grid, t0 = this.range.from, t1 = this.range.to;
    const x = t => pl + (t - t0) / (t1 - t0) * pw;
    this.layout = { pl, w: pw, t0, t1, pt, ph: this.rows.length * rh };
    ctx.textBaseline = 'middle';
    this.rows.forEach((r, j) => {
      const y = pt + j * rh;
      ctx.fillStyle = muted;
      ctx.textAlign = 'right';
      ctx.fillText(r.name, pl - 6, y + rh / 2);
      ctx.fillStyle = r.color;
      for (let i = 0; i < n; i++) {
        const v = r.data[i];
        if (v == null || v <= 0) continue;
        ctx.globalAlpha = 0.25 + 0.75 * Math.min(1, v / 100);
        const xa = x(start + i * step), xb = x(start + (i + 1) * step);
        ctx.fillRect(Math.max(pl, xa), y + 2, Math.min(pl + pw, xb) - Math.max(pl, xa) + 0.5, rh - 4);
      }
      ctx.globalAlpha = 1;
    });
    timeLabels(ctx, t0, t1, x, w, h, pw, null);
    this.drawOver();
  }
}

/** A single figure with a small history behind it. */
class Tile {
  constructor(parent, title, desc, unit, levels) {
    this.unit = unit;
    this.levels = levels; // [[color, from], ...] in increasing order; the first applies from -inf
    this.p = panel(parent, title, desc, 3, 'tile');
    this.value = el('div', { class: 'value', text: '–' });
    this.canvas = el('canvas');
    this.p.body.append(this.value, this.canvas);
    this.p.el.component = this;
    resizer.observe(this.p.el);
  }
  set(v, spark, text) {
    this.v = v;
    this.spark = spark || [];
    this.value.textContent = text != null ? text : format(v, this.unit);
    this.value.style.color = this.color(v);
    this.draw();
  }
  color(v) {
    if (v == null || !this.levels) return '';
    let c = this.levels[0][0];
    for (const [col, from] of this.levels) if (from == null || v >= from) c = col;
    return c === 'text' ? '' : C[c] || c;
  }
  draw() {
    const w = this.p.el.clientWidth, h = 40;
    if (!w) return;
    const ctx = sizeCanvas(this.canvas, w, h);
    const pts = (this.spark || []).map((v, i) => [i, v]).filter(p => p[1] != null);
    if (pts.length < 2) return;
    const vals = pts.map(p => p[1]);
    const lo = Math.min(0, ...vals), hi = Math.max(...vals, lo + 1e-9);
    const n = this.spark.length;
    const x = i => i / (n - 1) * w, y = v => h - 2 - (v - lo) / (hi - lo || 1) * (h - 6);
    const col = this.color(this.v) || css('--accent');
    ctx.beginPath();
    pts.forEach(([i, v], k) => (k ? ctx.lineTo(x(i), y(v)) : ctx.moveTo(x(i), y(v))));
    ctx.lineTo(x(pts[pts.length - 1][0]), h);
    ctx.lineTo(x(pts[0][0]), h);
    ctx.closePath();
    ctx.globalAlpha = 0.18;
    ctx.fillStyle = col;
    ctx.fill();
    ctx.globalAlpha = 1;
  }
}

/** A sortable, filterable table of apps. */
class AppTable {
  constructor(parent, title, desc, span, cfg) {
    this.cfg = cfg; // { columns: [{key, label, unit, heat}], onName, picked: Set }
    this.p = panel(parent, title, desc, span);
    this.filter = el('input', { type: 'search', placeholder: 'Filter apps' });
    this.filter.addEventListener('input', () => this.render());
    this.count = el('span', { class: 'range-label' });
    this.box = el('div', { class: 'tablebox' });
    this.p.body.append(el('div', { class: 'tablebar' }, this.filter, this.count), this.box);
    this.sort = cfg.columns[1].key;
    this.desc = true;
    this.rows = [];
  }
  set(rows) { this.rows = rows; this.render(); }
  render() {
    const f = this.filter.value.trim().toLowerCase();
    const rows = this.rows.filter(r => !f || r.name.toLowerCase().includes(f));
    const k = this.sort, d = this.desc ? -1 : 1;
    rows.sort((a, b) => (k === 'name' ? a.name.localeCompare(b.name) * d : ((a[k] ?? -Infinity) - (b[k] ?? -Infinity)) * d));
    this.count.textContent = `${rows.length} apps`;
    const cols = this.cfg.columns;
    const head = el('tr', {}, ...cols.map(c => el('th', {
      class: c.key === this.sort ? 'sorted' : null,
      text: c.label + (c.key === this.sort ? (this.desc ? ' ↓' : ' ↑') : ''),
      onclick: () => { if (this.sort === c.key) this.desc = !this.desc; else { this.sort = c.key; this.desc = c.key !== 'name'; } this.render(); },
    })));
    // Shaded on a fixed scale, as the Grafana table: 0 % green to 25 % or more red.
    const body = rows.slice(0, 500).map(r => el('tr', { class: this.cfg.picked && this.cfg.picked.has(r.name) ? 'picked' : null },
      ...cols.map(c => {
        if (c.key === 'name') return el('td', { class: 'link', text: r.name, title: 'Open in Process explorer', onclick: () => this.cfg.onName(r.name) });
        const td = el('td', { text: format(r[c.key], c.unit) });
        if (c.heat && r[c.key] != null) {
          td.style.background = heatColor(r[c.key] / 25);
          td.style.color = '#111217';
        }
        return td;
      })));
    this.box.replaceChildren(el('table', { class: 'data' }, el('thead', {}, head), el('tbody', {}, ...body)));
  }
}

// ---------------------------------------------------------------- state and navigation

const RANGES = [['15m', 900], ['1h', 3600], ['3h', 10800], ['6h', 21600], ['12h', 43200], ['24h', 86400], ['2d', 172800],
  ['7d', 604800], ['30d', 2592000], ['90d', 7776000], ['1y', 31536000], ['2y', 63072000]];
const PAGES = { pc: ['This PC', '3h'], apps: ['Apps', '24h'], explorer: ['Process explorer', '24h'], settings: ['Settings', '3h'] };
const state = { page: 'pc', rel: null, from: null, to: null, apps: [], group: null };
let cores = navigator.hardwareConcurrency || 1;

// The recorder's settings in force (GET /api/settings), read at start and after each save.
let cfg = {};
let systemDrive = 'C:';
const setting = (k, d) => (k in cfg ? cfg[k] : d);
const cardDrive = () => setting('card_drive', '') || systemDrive;
const intervalMs = () => setting('interval', 10) * 1000;
async function loadSettings() {
  try {
    const s = await getJSON('/api/settings');
    cfg = s.values || {};
    systemDrive = s.system_drive || systemDrive;
    return s;
  } catch (e) { return null; }
}

function readHash() {
  const [path, qs] = (location.hash.slice(1) || '/pc').split('?');
  const p = new URLSearchParams(qs || '');
  state.page = PAGES[path.replace('/', '')] ? path.replace('/', '') : 'pc';
  state.from = p.has('from') ? Number(p.get('from')) * 1000 : null;
  state.to = p.has('to') ? Number(p.get('to')) * 1000 : null;
  state.rel = p.get('r') || (state.from == null ? PAGES[state.page][1] : null);
  if (!RANGES.some(r => r[0] === state.rel)) state.rel = state.from == null ? PAGES[state.page][1] : null;
  state.apps = [...new Set((p.get('apps') || '').split(',').filter(Boolean))].slice(0, 64);
  // Settings: a group to show first (the desktop card's right-click opens ?group=desktop-card).
  state.group = state.page === 'settings' ? p.get('group') : null;
}

const groupId = name => `group-${name.toLowerCase().replace(/[^a-z0-9]+/g, '-')}`;

function hashFor(page, over) {
  const s = Object.assign({}, state, { page }, over || {});
  const p = new URLSearchParams();
  if (s.from != null) { p.set('from', Math.round(s.from / 1000)); p.set('to', Math.round(s.to / 1000)); } else if (s.rel) p.set('r', s.rel);
  if (page === 'explorer' && s.apps.length) p.set('apps', s.apps.join(','));
  return `#/${page}?${p}`;
}

function currentRange() {
  if (state.from != null && state.to > state.from) return { from: state.from, to: state.to, live: false };
  const secs = (RANGES.find(r => r[0] === state.rel) || RANGES[2])[1];
  const to = Date.now();
  return { from: to - secs * 1000, to, live: true };
}

/** Step for about one point every 3 px of the page width, never below the sample interval (the API
 * rounds it to whole rows). */
function stepFor(r, points) {
  const target = points || Math.max(150, Math.min(600, Math.floor(document.getElementById('main').clientWidth / 3)));
  return Math.max(intervalMs(), Math.ceil((r.to - r.from) / target));
}

const app = {
  gen: 0,
  timer: null,
  waiting: false,
  page: null,

  zoom(from, to) {
    if (!(to - from >= 30000)) return;
    location.hash = hashFor(state.page, { from, to, rel: null });
  },

  render() {
    readHash();
    const main = document.getElementById('main');
    window.onbeforeunload = null;
    live.clear();
    resizer.disconnect();
    hideTip();
    bootMarks = [];
    main.replaceChildren();
    this.header();
    this.page = { pc: pagePC, apps: pageApps, explorer: pageExplorer, settings: pageSettings }[state.page]();
    this.page.build(main);
    main.append(this.foot = el('footer', { class: 'foot' }));
    this.refresh();
  },

  header() {
    const top = document.getElementById('top');
    const nav = el('nav', { class: 'nav' }, ...Object.entries(PAGES).map(([k, [label]]) =>
      el('a', { href: hashFor(k), class: k === state.page ? 'on' : null, text: label })));
    const sel = el('select', { title: 'Time range' });
    for (const [k] of RANGES) sel.append(el('option', { value: k, text: `Last ${k}` }));
    if (state.from != null) sel.prepend(el('option', { value: '', text: 'Zoomed range' }));
    sel.value = state.from != null ? '' : state.rel;
    sel.addEventListener('change', () => { if (sel.value) location.hash = hashFor(state.page, { rel: sel.value, from: null, to: null }); });
    const out = el('button', { text: 'Zoom out', title: 'Show a range twice as long' });
    out.addEventListener('click', () => {
      const r = currentRange(), span = r.to - r.from, mid = (r.from + r.to) / 2;
      const to = Math.min(Date.now(), mid + span), from = to - 2 * span;
      if (r.live) {
        const next = RANGES.find(x => x[1] * 1000 >= 2 * span * 0.95);
        location.hash = hashFor(state.page, { rel: next ? next[0] : '2y', from: null, to: null });
      } else location.hash = hashFor(state.page, { from, to, rel: null });
    });
    const refresh = el('button', { text: 'Refresh', title: 'Read the latest data now' });
    refresh.addEventListener('click', () => this.refresh());
    this.rangeLabel = el('span', { class: 'range-label' });
    this.dot = el('span', { class: 'dot' });
    this.statusText = el('span', { text: '…' });
    top.replaceChildren(
      el('div', { class: 'brand' }, el('img', { src: '/favicon.svg', alt: '' }), el('span', { text: 'Kymo' })),
      nav, el('div', { class: 'spacer' }),
      // The Settings page has no time range.
      ...(state.page === 'settings' ? [] : [el('div', { class: 'tools' }, this.rangeLabel, sel, out, refresh)]),
      el('div', { class: 'status', title: 'Is the recorder taking samples?' }, this.dot, this.statusText));
  },

  async refresh() {
    clearTimeout(this.timer);
    const gen = ++this.gen;
    const r = currentRange();
    this.rangeLabel.textContent = `${when(r.from)} – ${r.live ? 'now' : when(r.to)}`;
    const alive = () => gen === this.gen;
    // 503 with status "stale" means the recorder answers but took no sample for a minute.
    fetch('/api/health', { cache: 'no-store' }).then(x => x.json()).then(h => {
      this.health = h;
      this.dot.className = 'dot ' + (h.status === 'ok' ? 'ok' : 'bad');
      this.statusText.textContent = h.status === 'ok' ? 'Recording' : 'Not recording';
    }).catch(() => { this.dot.className = 'dot bad'; this.statusText.textContent = 'Recorder not answering'; });
    try {
      await this.page.load(r, alive);
      if (alive()) this.footer();
    } catch (e) {
      if (alive()) this.foot.replaceChildren(el('div', { class: 'error', text: `Could not read the history: ${e.message}` }));
    }
    // Automatic (by range), a fixed pace or never, as the settings say; the Settings page never
    // refreshes by itself (it would undo edits in progress).
    const pace = setting('refresh', 0);
    if (alive() && r.live && state.page !== 'settings' && pace !== -1) {
      const span = r.to - r.from;
      const auto = span <= 3 * 3600e3 ? 10e3 : span <= 86400e3 ? 30e3 : span <= 7 * 86400e3 ? 120e3 : 600e3;
      const every = pace > 0 ? Math.max(pace * 1000, intervalMs()) : Math.max(auto, intervalMs());
      this.timer = setTimeout(() => { if (document.hidden) this.waiting = true; else this.refresh(); }, every);
    }
  },

  async footer() {
    try {
      const s = await getJSON('/api/stats');
      this.foot.textContent = `Kymo ${s.version} · its own cost: ${format(s.cpu_percent_of_one_core.last_10s, 'percent')} of one core, `
        + `${format(s.memory_bytes.private, 'bytes')} of memory · history ${format(s.data_bytes.total, 'bytes')} on disk`
        + (s.errors ? ` · ${s.errors} write errors (last: ${s.last_error})` : '');
    } catch (e) { /* the footer is optional */ }
  },
};

document.addEventListener('visibilitychange', () => {
  if (!document.hidden && app.waiting) { app.waiting = false; app.refresh(); }
});
// Leaving the Settings page with unsaved edits asks first. Each history entry carries its place in
// the list, so staying goes back to the entry shown (a link added one after it; Back or Forward
// moved within the list) without rewriting any other entry.
let shownIndex = history.state && typeof history.state.i === 'number' ? history.state.i : 0;
if (!history.state || typeof history.state.i !== 'number') history.replaceState({ i: 0 }, '');
let returning = false;
window.addEventListener('hashchange', () => {
  if (returning) { returning = false; return; }
  const st = history.state;
  const moved = st && typeof st.i === 'number';
  if (app.page && app.page.leaving && !app.page.leaving()) {
    // The entry left behind (a link) gets its true place, for when it is reached again.
    if (!moved) history.replaceState({ i: shownIndex + 1 }, '');
    const delta = moved ? shownIndex - st.i : -1;
    if (delta !== 0) { returning = true; history.go(delta); }
    return;
  }
  if (!moved) history.replaceState({ i: shownIndex + 1 }, '');
  shownIndex = history.state.i;
  app.render();
});

const openApp = name => { location.hash = hashFor('explorer', { apps: [name] }); };

/** Per-bucket latest boot time (seconds): the maximum, since an average across a restart is no time at all. */
const bootSeries = d => (d.map.get('sys.boot') || {}).max || null;

/** Windows start times inside the range: where the boot time changes, and a boot inside the first bucket. */
function boots(d, r) {
  const out = [];
  let prev = null;
  for (const v of bootSeries(d) || []) {
    if (v == null) continue;
    if (v !== prev && v * 1000 >= r.from && v * 1000 <= r.to) out.push(v * 1000);
    prev = v;
  }
  return out;
}

/** Boot marks for pages that do not read sys.boot with their own data. */
async function loadBoots(r, step) {
  const d = await query(['sys.boot'], r, step);
  return boots(d, r);
}

// ---------------------------------------------------------------- This PC

const LEVELS = [['green', null], ['orange', 70], ['red', 90]];

function pagePC() {
  const c = {};
  return {
    build(main) {
      // The colours follow the alert settings (the same ones the desktop card uses).
      const lv = (warn, crit) => [['green', null], ['orange', setting(warn, 70)], ['red', setting(crit, 90)]];
      const d = cardDrive();
      let g = row(main, `Right now (last ${Math.max(30, intervalMs() / 1000)} s)`);
      const tiles = [
        ['cpu', 'CPU', 'Share of time the processors were busy, the way Task Manager counts it since Windows 11 24H2.', 'percent', lv('cpu_warn', 'cpu_crit')],
        ['mem', 'Memory in use', 'RAM in use = total minus available (standby + free), like Task Manager.', 'percent', lv('mem_warn', 'mem_crit')],
        ['gpu', 'GPU', 'Graphics engine busy %.', 'percent', LEVELS],
        ['vram', 'GPU memory', 'Dedicated video memory used.', 'percent', [['green', null], ['orange', setting('vram_full', 95)]]],
        ['disk', `Disk ${d} active`, `Share of time ${d} was busy, like Task Manager's Active time.`, 'percent', [['green', null], ['orange', setting('disk_busy', 90)]]],
        ['net', 'Network', 'All network adapters, in + out.', 'bps', null],
        ['cpuw', 'CPU power', 'CPU package power from the RAPL energy counter.', 'watt', [['text', null], ['orange', 100], ['red', 150]]],
        ['gput', 'GPU temperature', 'GPU core temperature.', 'celsius', lv('gpu_temp_warn', 'gpu_temp_crit')],
        // Against the commit limit of now, not the most it can reach (the card's alert), so its own levels.
        ['commit', 'Commit', 'Committed memory / commit limit (RAM + page file). Near 100 % programs fail to allocate.', 'percent', LEVELS],
        ['clock', 'CPU clock', 'Effective clock while running code: above the base clock while the CPU boosts.', 'mhz', null],
        ['limit', 'CPU perf. limit', '% Performance Limit: 100 = not limited; below, power, thermal or platform limits hold the CPU back.',
          'percent', [['orange', null], ['green', setting('perf_limit', 90)]]],
        ['gpuw', 'GPU power', 'Board power draw.', 'watt', [['text', null], ['orange', 170], ['red', 195]]],
        ['procs', 'Processes', 'Running processes.', 'count', null],
        ['cfree', `${d} free`, `Free space on ${d}.`, 'bytes', [['red', null], ['orange', setting('free_crit', 10) * GIB], ['green', setting('free_warn', 25) * GIB]]],
        ['uptime', 'Uptime', 'Time since Windows started.', 'duration', null],
        ['rec', 'Recording', 'Whether the recorder took a sample in the last minute.', 'count', null],
      ];
      c.tiles = {};
      for (const [k, title, desc, unit, levels] of tiles) c.tiles[k] = new Tile(g, title, desc, unit, levels);

      g = row(main, 'CPU');
      c.usage = new Chart(g, 'CPU usage', 'CPU (Task Manager) = share of time not idle. Processor utility also counts turbo clocks, so it reads higher under boost (capped at 100 % here).', 12, { unit: 'percent', min: 0, max: 100 });
      c.kind = new Chart(g, 'CPU time by kind', "user = apps' own code; kernel = Windows and drivers working for them; dpc/interrupt = driver interrupt work (high values cause stutter and audio drop-outs). % of the whole CPU.", 12, { unit: 'percent', min: 0, stacked: true });
      c.cores = new Heat(g, 'Each logical processor', 'Busy % per logical processor. A row that stays red while the others are idle points at a single-threaded bottleneck.', 24);
      c.clock = new Chart(g, 'Effective clock', 'Clock while executing code. A low maximum under load means power or thermal limits.', 8, { unit: 'mhz' });
      c.power = new Chart(g, 'CPU power and limit', 'RAPL power of the whole package and of the cores. Right axis: % Performance Limit (100 = no limit).', 8, { unit: 'watt', rightUnit: 'percent', rightRange: { min: 0, max: 100 } });
      c.queue = new Chart(g, 'Waiting threads and context switches', 'Processor queue length (sustained > 2 per core = CPU-bound). Right axis: context switches per second.', 8, { unit: 'short', rightUnit: 'short' });

      g = row(main, 'Memory');
      c.ram = new Chart(g, 'RAM composition', "Same split as Task Manager's memory composition. Standby is cache Windows gives back instantly; it is not a problem.", 12, { unit: 'bytes', stacked: true });
      c.commit = new Chart(g, 'Commit charge', 'Memory promised to programs. When committed reaches the limit, Windows grows the page file or programs fail.', 12, { unit: 'bytes' });
      c.paging = new Chart(g, 'Paging (hard faults)', 'Pages read back from the page file or mapped files (up) and written (down). Thousands per second while RAM is full = short of memory.', 8, { unit: 'short' });
      c.pools = new Chart(g, 'Kernel memory pools', 'Driver and kernel allocations. A line that only ever grows points at a driver leak.', 8, { unit: 'bytes' });
      c.pagefile = new Chart(g, 'Page file', 'Page file use and size.', 8, { unit: 'bytes' });

      g = row(main, 'Disks');
      c.tput = new Chart(g, 'Throughput', 'Bytes read (up) and written (down) per volume.', 8, { unit: 'Bps' });
      c.ops = new Chart(g, 'Operations per second', 'Read (up) and write (down) operations per second.', 8, { unit: 'iops' });
      c.lat = new Chart(g, 'Average latency', 'Average time per operation. NVMe is normally well under 1 ms; tens of ms feel slow.', 8, { unit: 'ms' });
      c.active = new Chart(g, 'Active time', "Share of time each volume was busy, like Task Manager's Active time.", 8, { unit: 'percent', min: 0, max: 100 });
      c.dqueue = new Chart(g, 'Queue length', 'Requests waiting.', 8, { unit: 'short' });
      c.free = new Chart(g, 'Free space', 'Free space and size per volume.', 8, { unit: 'bytes' });

      g = row(main, 'Network');
      c.traffic = new Chart(g, 'Traffic', 'Bits per second received (up) and sent (down) per adapter; disconnected adapters are hidden.', 16, { unit: 'bps' });
      c.neterr = new Chart(g, 'Errors and drops', 'Should stay at zero; anything else points at a cable, driver or Wi-Fi problem.', 8, { unit: 'pps' });

      g = row(main, 'GPU');
      c.engines = new Chart(g, 'GPU engines', 'Busy % of each GPU engine.', 8, { unit: 'percent', min: 0, max: 100 });
      c.vram = new Chart(g, 'Video memory', 'Dedicated VRAM. When it is full, textures spill into system RAM and frame times jump.', 8, { unit: 'bytes' });
      c.temp = new Chart(g, 'Temperature and fan', 'Fan at 0 % is the zero-RPM idle mode.', 8, { unit: 'celsius', rightUnit: 'percent', rightRange: { min: 0, max: 100 } });
      c.gpow = new Chart(g, 'Power', 'Board power.', 8, { unit: 'watt' });
      c.gclk = new Chart(g, 'Clocks', 'Current clocks. The graphics clock drops while power- or thermal-limited.', 8, { unit: 'mhz' });
      c.why = new Timeline(g, 'Why the GPU is slowing down', 'Clock event reasons. Idle is normal; power limit under load is normal for this card; thermal or hardware slowdown is not.', 8);

      g = row(main, 'System and recording');
      c.procs = new Chart(g, 'Processes and threads', '', 8, { unit: 'count', rightUnit: 'count' });
      c.coverage = new Timeline(g, 'Recording coverage', 'Lit = recorded. Empty = the PC was off or asleep, or a source did not answer.', 8);
      c.size = new Chart(g, 'History size on disk', 'Size of the recorded history (2-year retention). Right axis: free space on C:.', 8, { unit: 'bytes', rightUnit: 'bytes' });
      c.cost = new Chart(g, 'Cost of recording', "This recorder's own CPU (% of one core) and memory (right axis).", 8, { unit: 'percent', rightUnit: 'bytes' });
      c.collect = new Chart(g, 'Collection time per sample', 'How long reading the counters, every process and the GPU takes, every 10 s. The cost of monitoring itself.', 8, { unit: 'ms' });
      c.uptime = new Chart(g, 'Uptime (drops to zero at each boot)', 'Each drop is a boot.', 8, { unit: 'duration', fill: 0.2 });
    },

    async load(r, alive) {
      const step = stepFor(r);
      // At most 64 patterns: one request, one read of the history per refresh.
      const keys = ['cpu.busy', 'cpu.utility', 'cpu.user', 'cpu.privileged', 'cpu.interrupt', 'cpu.dpc', 'cpu.clock', 'cpu.core.*.clock',
        'cpu.limit', 'power.package', 'power.cores', 'sys.queue', 'sys.switches', 'sys.processes', 'sys.threads', 'sys.boot',
        'mem.total', 'mem.available', 'mem.modified', 'mem.standby_*', 'mem.free',
        'mem.committed', 'mem.commit_limit', 'mem.pages_in', 'mem.pages_out', 'mem.pool_*', 'pagefile.*',
        'disk.*.read_bps', 'disk.*.write_bps', 'disk.*.reads', 'disk.*.writes', 'disk.*.read_latency', 'disk.*.write_latency',
        'disk.*.active', 'disk.*.queue', 'disk.*.free', 'disk.*.size',
        'net.*.rx_bps', 'net.*.tx_bps', 'net.*.bandwidth', 'net.*.*_errors', 'net.*.*_discards',
        'gpu.*util', 'gpu.mem_used', 'gpu.mem_total', 'gpu.temp', 'gpu.fan', 'gpu.power', 'gpu.power_limit',
        'gpu.clock_graphics', 'gpu.clock_mem', 'gpu.slow_*',
        'self.cpu', 'self.private', 'self.data', 'self.collect'];
      const now = Date.now();
      const tileRange = { from: now - 300000, to: now };
      const [d, heat, t, appRows] = await Promise.all([
        query(keys, r, step),
        query(['cpu.core.*.busy'], r, stepFor(r, 240)),
        query(['cpu.busy', 'mem.available', 'mem.total', 'gpu.util', 'gpu.mem_used', 'gpu.mem_total', `disk.${cardDrive()}.active`, 'net.rx_bps',
          'net.tx_bps', 'power.package', 'gpu.temp', 'mem.committed', 'mem.commit_limit', 'cpu.clock', 'cpu.limit', 'gpu.power',
          'sys.processes', `disk.${cardDrive()}.free`, 'sys.boot'], tileRange, intervalMs()),
        // "System" exists whenever per-app rows were recorded: its rows show the per-app coverage.
        appSeries(['System'], 'instances', r, Math.max(step, 60000), false),
      ]);
      if (!alive()) return;
      bootMarks = boots(d, r);
      this.tiles(t);
      const G = d.grid, A = k => d.avg(k);
      const S = (name, data, color, extra) => Object.assign({ name, data, color }, extra || {});

      c.usage.set(G, r, [S('CPU (Task Manager)', A('cpu.busy'), C.green),
        S('processor utility (frequency-aware)', A('cpu.utility') && A('cpu.utility').map(v => (v == null ? null : Math.min(100, v))), C.yellow, { dash: true, fill: 0 })]);
      const kernel = combine((p, i, dp) => Math.max(0, p - i - dp), A('cpu.privileged'), A('cpu.interrupt'), A('cpu.dpc'));
      c.kind.set(G, r, [S('user (apps)', A('cpu.user'), C.green), S('kernel', kernel, C.yellow), S('dpc', A('cpu.dpc'), C.orange), S('interrupt', A('cpu.interrupt'), C.red)]);
      const coreRows = heat.keys.filter(k => /^cpu\.core\./.test(k)).sort()
        .map(k => ({ name: 'CPU ' + k.split('.')[2], data: heat.avg(k) }));
      c.cores.set(heat.grid, r, coreRows);
      const coreClocks = d.keys.filter(k => /^cpu\.core\..*\.clock$/.test(k)).map(k => A(k));
      const fastest = coreClocks.length ? coreClocks[0].map((_, i) => max(coreClocks.map(a => a[i]))) : null;
      c.clock.set(G, r, [S('average', A('cpu.clock'), C.blue), S('fastest processor', fastest, C.orange, { dash: true, fill: 0 })]);
      c.power.set(G, r, [S('package W', A('power.package'), C.orange), S('cores W', A('power.cores'), C.yellow),
        S('performance limit %', A('cpu.limit'), C.red, { right: true, dash: true, fill: 0 })]);
      c.queue.set(G, r, [S('threads waiting for a CPU', A('sys.queue'), C.orange), S('context switches/s', A('sys.switches'), C.blue, { right: true, fill: 0 })]);

      const standby = combine((a, b, x) => a + b + x, A('mem.standby_core'), A('mem.standby_normal'), A('mem.standby_reserve'));
      const inUse = combine((t, a, m) => t - a - m, A('mem.total'), A('mem.available'), A('mem.modified'));
      c.ram.set(G, r, [S('in use', inUse, C.orange), S('modified (to be written)', A('mem.modified'), C.yellow),
        S('standby (cache)', standby, C.blue), S('free', A('mem.free'), C.green)]);
      c.commit.set(G, r, [S('committed', A('mem.committed'), C.blue), S('limit (RAM + page file)', A('mem.commit_limit'), C.red, { dash: true, fill: 0 })]);
      c.paging.set(G, r, [S('pages read from disk/s', A('mem.pages_in'), C.orange), S('pages written to disk/s', A('mem.pages_out'), C.blue, { neg: true })]);
      c.pools.set(G, r, [S('non-paged pool', A('mem.pool_nonpaged'), C.purple), S('paged pool', A('mem.pool_paged'), C.blue)]);
      c.pagefile.set(G, r, [S('used', A('pagefile.used'), C.orange), S('size', A('pagefile.size'), C.blue, { dash: true, fill: 0 })]);

      const vols = [...new Set(d.keys.filter(k => k.startsWith('disk.')).map(k => k.split('.')[1]))].sort();
      const vs = (field, fn) => vols.map((v, i) => fn(v, A(`disk.${v}.${field}`), PALETTE[i % PALETTE.length]));
      c.tput.set(G, r, [...vs('read_bps', (v, a, col) => S(`${v} read`, a, col)), ...vs('write_bps', (v, a, col) => S(`${v} write`, a, col, { neg: true }))]);
      c.ops.set(G, r, [...vs('reads', (v, a, col) => S(`${v} reads`, a, col)), ...vs('writes', (v, a, col) => S(`${v} writes`, a, col, { neg: true }))]);
      c.lat.set(G, r, [...vs('read_latency', (v, a, col) => S(`${v} read`, times(a, 1000), col)),
        ...vs('write_latency', (v, a, col) => S(`${v} write`, times(a, 1000), col, { dash: true }))]);
      c.active.set(G, r, vs('active', (v, a, col) => S(v, a, col)));
      c.dqueue.set(G, r, vs('queue', (v, a, col) => S(`${v} waiting`, a, col)));
      c.free.set(G, r, [...vs('free', (v, a, col) => S(`${v} free`, a, col)), ...vs('size', (v, a, col) => S(`${v} size`, a, col, { dash: true, fill: 0 }))]);

      const nics = [...new Set(d.keys.filter(k => /^net\..+\.rx_bps$/.test(k)).map(k => k.slice(4, -7)))]
        .filter(n => (max(A(`net.${n}.bandwidth`)) || 0) > 0).sort();
      c.traffic.set(G, r, nics.flatMap((n, i) => [S(`${n} in`, times(A(`net.${n}.rx_bps`), 8), PALETTE[i]),
        S(`${n} out`, times(A(`net.${n}.tx_bps`), 8), PALETTE[i], { neg: true })]));
      const sum2 = (a, b) => combine((x, y) => x + y, a, b);
      c.neterr.set(G, r, nics.flatMap((n, i) => [S(`${n} errors`, sum2(A(`net.${n}.rx_errors`), A(`net.${n}.tx_errors`)), PALETTE[i]),
        S(`${n} discarded`, sum2(A(`net.${n}.rx_discards`), A(`net.${n}.tx_discards`)), PALETTE[i], { dash: true })]));

      c.engines.set(G, r, [S('graphics/compute', A('gpu.util'), C.green), S('memory controller', A('gpu.mem_util'), C.yellow),
        S('video encoder', A('gpu.enc_util'), C.blue), S('video decoder', A('gpu.dec_util'), C.purple)]);
      c.vram.set(G, r, [S('used', A('gpu.mem_used'), C.blue), S('total', A('gpu.mem_total'), C.red, { dash: true, fill: 0 })]);
      c.temp.set(G, r, [S('GPU °C', A('gpu.temp'), C.orange), S('fan %', A('gpu.fan'), C.blue, { right: true, fill: 0 })]);
      c.gpow.set(G, r, [S('draw', A('gpu.power'), C.orange), S('limit', A('gpu.power_limit'), C.red, { dash: true, fill: 0 })]);
      c.gclk.set(G, r, [S('graphics', A('gpu.clock_graphics'), C.green), S('memory', A('gpu.clock_mem'), C.blue)]);
      const either = (a, b) => combine((x, y) => Math.max(x, y), a, b);
      c.why.set(G, r, [ // missing rows (no GPU readings in the range) draw empty
        { name: 'idle', data: A('gpu.slow_idle'), color: css('--muted') },
        { name: 'power limit', data: A('gpu.slow_sw_power'), color: C.orange },
        { name: 'thermal', data: either(A('gpu.slow_sw_thermal'), A('gpu.slow_hw_thermal')), color: C.red },
        { name: 'hardware slowdown', data: either(A('gpu.slow_hw'), A('gpu.slow_hw_power_brake')), color: C.red },
      ]);

      c.procs.set(G, r, [S('processes', A('sys.processes'), C.blue), S('threads', A('sys.threads'), C.purple, { right: true, fill: 0 })]);
      const present = a => (a ? a.map(v => (v == null ? null : 100)) : new Array(G.n).fill(null));
      const apps = appRows.list[0] ? resample(appRows.grid, appRows.list[0].avg, G) : null;
      c.coverage.set(G, r, [
        { name: 'system (10 s)', data: present(A('cpu.busy')), color: C.green },
        { name: 'apps (1 min)', data: present(apps), color: C.green },
        { name: 'GPU (10 s)', data: present(A('gpu.util')), color: C.green },
        { name: 'CPU power', data: present(A('power.package')), color: C.green },
      ]);
      c.size.set(G, r, [S('history', A('self.data'), C.blue), S('free on C:', A('disk.C:.free'), C.green, { right: true, dash: true, fill: 0 })]);
      c.cost.set(G, r, [S('CPU, % of one core', A('self.cpu'), C.orange), S('memory', A('self.private'), C.blue, { right: true, fill: 0 })]);
      c.collect.set(G, r, [S('collection', times(A('self.collect'), 1000), C.purple)]);
      const boot = bootSeries(d);
      const uptime = boot && boot.map((b, i) => (b == null ? null : Math.max(0, (G.start + (i + 1) * G.step) / 1000 - b)));
      c.uptime.set(G, r, [S('uptime', uptime, C.blue)]);
    },

    tiles(t) {
      // Each value is the last 30 s (or the last sample, at longer intervals); the curve behind it is
      // the last 5 minutes.
      const A = k => t.avg(k) || [];
      const n = Math.max(1, Math.round(30000 / intervalMs()));
      const recent = a => mean(a.filter(v => v != null).slice(-n));
      const T = c.tiles;
      const set = (k, a) => T[k].set(recent(a), a);
      set('cpu', A('cpu.busy'));
      set('mem', combine((av, tot) => 100 * (1 - av / tot), A('mem.available'), A('mem.total')));
      set('gpu', A('gpu.util'));
      set('vram', combine((u, tot) => 100 * u / tot, A('gpu.mem_used'), A('gpu.mem_total')));
      set('disk', A(`disk.${cardDrive()}.active`));
      set('net', combine((rx, tx) => 8 * (rx + tx), A('net.rx_bps'), A('net.tx_bps')));
      set('cpuw', A('power.package'));
      set('gput', A('gpu.temp'));
      set('commit', combine((u, l) => 100 * u / l, A('mem.committed'), A('mem.commit_limit')));
      set('clock', A('cpu.clock'));
      set('limit', A('cpu.limit'));
      set('gpuw', A('gpu.power'));
      T.procs.set(last(A('sys.processes')), A('sys.processes'));
      T.cfree.set(last(A(`disk.${cardDrive()}.free`)), null);
      const boot = last(A('sys.boot'));
      T.uptime.set(boot == null ? null : Date.now() / 1000 - boot, null);
      const ok = app.health ? app.health.status === 'ok' : last(A('cpu.busy')) != null;
      T.rec.set(ok ? 1 : 0, null, ok ? 'OK' : 'GAP');
      T.rec.value.style.color = ok ? C.green : C.red;
    },
  };
}

// ---------------------------------------------------------------- Apps

const APP_NOTE = 'Apps are summed per name (all chrome.exe processes together), every minute.';
const TABLE_COLUMNS = [
  { key: 'name', label: 'App' },
  { key: 'cpu_avg', label: 'Avg CPU %', unit: 'percent', heat: true },
  { key: 'cpu_max', label: 'Peak CPU % (1 min)', unit: 'percent' },
  { key: 'mem_max', label: 'Peak memory', unit: 'bytes' },
  { key: 'commit_max', label: 'Peak commit', unit: 'bytes' },
  { key: 'read_bytes', label: 'Read', unit: 'bytes' },
  { key: 'write_bytes', label: 'Written', unit: 'bytes' },
  { key: 'instances_max', label: 'Max instances', unit: 'count' },
];

/** /api/apps with CPU as % of the whole PC (all logical processors), like Task Manager. */
async function appList(r) {
  const j = await getJSON('/api/apps', { from: r.from / 1000, to: r.to / 1000, sort: 'cpu', limit: 1000 });
  return j.apps.map(a => Object.assign({}, a, { cpu_avg: a.cpu_avg / cores, cpu_max: Math.min(100, a.cpu_max / cores) }));
}

function pageApps() {
  const c = {};
  return {
    build(main) {
      const g = row(main, 'Apps (per process name)');
      c.cpu = new Chart(g, 'Top apps by CPU', `Share of the whole CPU used by each app, like Task Manager's CPU column. ${APP_NOTE}`, 12,
        { unit: 'percent', stacked: true, legend: 'table', calcs: ['mean', 'max'], onName: openApp, height: 260 });
      c.mem = new Chart(g, 'Top apps by memory', `Private working set (Task Manager's Memory column) summed over each app's processes. ${APP_NOTE}`, 12,
        { unit: 'bytes', legend: 'table', calcs: ['last', 'max'], onName: openApp, height: 260 });
      c.io = new Chart(g, 'Top apps by I/O', `Read + write bytes per second per app. Counts all I/O (files, network, devices), not only the disk. ${APP_NOTE}`, 12,
        { unit: 'Bps', legend: 'table', calcs: ['mean', 'max'], onName: openApp, height: 260 });
      c.commit = new Chart(g, 'Top apps by commit', `Private bytes: memory each app has reserved, which counts against the commit limit (RAM + page file). ${APP_NOTE}`, 12,
        { unit: 'bytes', legend: 'table', calcs: ['last', 'max'], onName: openApp, height: 260 });
      const g2 = row(main, 'Every app');
      c.table = new AppTable(g2, 'Apps over the selected time range', 'Every app seen in the range. Average CPU is over the whole range (time without the app counts as 0); peak CPU is the busiest minute. Click an app to open it in the Process explorer.', 24,
        { columns: TABLE_COLUMNS, onName: openApp });
    },
    async load(r, alive) {
      const step = stepFor(r, 300);
      const apps = await appList(r);
      if (!alive()) return;
      c.table.set(apps);
      const top = (k, n = 10) => [...apps].sort((a, b) => (b[k] || 0) - (a[k] || 0)).slice(0, n).map(a => a.name);
      const io = [...apps].sort((a, b) => (b.read_bytes + b.write_bytes) - (a.read_bytes + a.write_bytes)).slice(0, 10).map(a => a.name);
      const [cpu, mem, rd, wr, com, marks] = await Promise.all([
        appSeries(top('cpu_avg'), 'cpu', r, step, true),
        appSeries(top('mem_max'), 'mem', r, step, false),
        appSeries(io, 'read', r, step, false),
        appSeries(io, 'write', r, step, false),
        appSeries(top('commit_max'), 'commit', r, step, false),
        loadBoots(r, step),
      ]);
      if (!alive()) return;
      bootMarks = marks;
      c.commit.set(com.grid, r, com.list.map((s, i) => ({ name: s.key, data: s.avg, color: PALETTE[i % PALETTE.length], fill: 0 })));
      const color = i => PALETTE[i % PALETTE.length];
      c.cpu.set(cpu.grid, r, cpu.list.map((s, i) => ({
        name: s.key, data: times(s.avg, 1 / cores),
        color: s.key === 'all other apps' ? css('--muted') : color(i), link: s.key !== 'all other apps',
      })));
      c.mem.set(mem.grid, r, mem.list.map((s, i) => ({ name: s.key, data: s.avg, color: color(i), fill: 0 })));
      c.io.set(rd.grid, r, rd.list.map((s, i) => ({ name: s.key, data: combine((a, b) => a + b, s.avg, wr.list[i].avg), color: color(i), fill: 0 })));
    },
  };
}

// ---------------------------------------------------------------- Process explorer

function pageExplorer() {
  const c = {};
  const picked = () => new Set(state.apps);
  const setApps = apps => { location.hash = hashFor('explorer', { apps }); };
  return {
    build(main) {
      if (!state.apps.length) state.apps = ['explorer'];
      const g0 = row(main, 'Selected apps');
      const box = el('div', { class: 'picker w24' });
      for (const a of state.apps) {
        box.append(el('span', { class: 'chip' }, el('span', { text: a }),
          el('button', { text: '×', title: `Remove ${a}`, onclick: () => setApps(state.apps.filter(x => x !== a)) })));
      }
      this.input = el('input', { list: 'app-names', placeholder: 'Add an app (type its name)' });
      this.names = el('datalist', { id: 'app-names' });
      this.msg = el('span', { class: 'range-label' });
      this.known = [];
      this.input.addEventListener('change', () => {
        // Names are stored without ".exe" and with their own case: match what was typed to one of them.
        const typed = this.input.value.trim().replace(/\.exe$/i, '');
        if (!typed) return;
        const name = this.known.find(n => n === typed) || this.known.find(n => n.toLowerCase() === typed.toLowerCase());
        if (!name) { this.msg.textContent = `No app named "${typed}" in this range.`; return; }
        if (state.apps.length >= 64) { this.msg.textContent = 'At most 64 apps at a time.'; return; }
        if (!state.apps.includes(name)) setApps([...state.apps, name]);
      });
      box.append(this.input, this.names, this.msg);
      g0.append(box);
      c.cpu = new Chart(g0, 'CPU', `Share of the whole CPU, summed over the app's processes. ${APP_NOTE}`, 12,
        { unit: 'percent', legend: 'table', calcs: ['mean', 'max'], fill: 0 });
      c.split = new Chart(g0, 'CPU time split', "user = the app's own code; kernel = work Windows does for it (files, network, drivers).", 12,
        { unit: 'percent', stacked: true });
      c.mem = new Chart(g0, 'Memory (private working set)', "Task Manager's Memory column, summed over the app's processes.", 12,
        { unit: 'bytes', legend: 'table', calcs: ['last', 'max'], fill: 0 });
      c.commit = new Chart(g0, 'Commit (private bytes)', 'Memory the app has reserved. Steady growth that never comes back = a leak.', 12,
        { unit: 'bytes', legend: 'table', calcs: ['last', 'max'], fill: 0 });
      c.io = new Chart(g0, 'I/O', 'Read (up) and write (down) bytes per second; all I/O, not only the disk.', 12, { unit: 'Bps' });
      c.inst = new Chart(g0, 'Running instances', 'How many processes with this name were running.', 12, { unit: 'count', fill: 0 });
      const g1 = row(main, 'All apps');
      c.table = new AppTable(g1, 'All apps over the selected time range', 'Click an app to show it alone above.', 24,
        { columns: TABLE_COLUMNS, onName: openApp, picked: picked() });
    },
    async load(r, alive) {
      const step = stepFor(r, 300);
      const names = state.apps;
      const fields = ['cpu', 'user', 'kernel', 'mem', 'commit', 'read', 'write', 'instances'];
      // The table first: it stays usable even if the series fail.
      const apps = await appList(r);
      if (!alive()) return;
      this.known = apps.map(a => a.name);
      this.names.replaceChildren(...apps.map(a => el('option', { value: a.name })));
      c.table.set(apps);
      const [marks, ...res] = await Promise.all([loadBoots(r, step), ...fields.map(f => appSeries(names, f, r, step, false))]);
      if (!alive()) return;
      bootMarks = marks;
      const by = Object.fromEntries(fields.map((f, i) => [f, res[i]]));
      const color = i => PALETTE[i % PALETTE.length];
      const G = by.cpu.grid;
      const pct = a => times(a, 1 / cores);
      c.cpu.set(G, r, by.cpu.list.map((s, i) => ({ name: s.key, data: pct(s.avg), color: color(i) })));
      c.split.set(G, r, names.flatMap((n, i) => [
        { name: `${n} user`, data: pct(by.user.list[i].avg), color: color(i) },
        { name: `${n} kernel`, data: pct(by.kernel.list[i].avg), color: color(i + 8) },
      ]));
      c.mem.set(G, r, by.mem.list.map((s, i) => ({ name: s.key, data: s.avg, color: color(i) })));
      c.commit.set(G, r, by.commit.list.map((s, i) => ({ name: s.key, data: s.avg, color: color(i) })));
      c.io.set(G, r, names.flatMap((n, i) => [
        { name: `${n} read`, data: by.read.list[i].avg, color: color(i) },
        { name: `${n} write`, data: by.write.list[i].avg, color: color(i), neg: true },
      ]));
      c.inst.set(G, r, by.instances.list.map((s, i) => ({ name: s.key, data: s.avg, color: color(i) })));
    },
  };
}

// ---------------------------------------------------------------- Settings

/** A change to the recorder: a form POST with the header the API asks of every write. */
async function postForm(path, values) {
  const r = await fetch(path, {
    method: 'POST', cache: 'no-store', body: new URLSearchParams(values).toString(),
    headers: { 'Content-Type': 'application/x-www-form-urlencoded', 'X-Kymo': '1' },
  });
  const j = await r.json().catch(() => ({}));
  if (!r.ok) throw Object.assign(new Error(j.error || `HTTP ${r.status}`), { field: j.field });
  return j;
}

const keptFor = n => (n % 365 === 0 ? `${n / 365} year${n === 365 ? '' : 's'}` : n % 7 === 0 && n < 60 ? `${n / 7} week${n === 7 ? '' : 's'}` : `${n} day${n === 1 ? '' : 's'}`);
const DELETE_AGES = [[7, 'older than 1 week'], [30, 'older than 30 days'], [90, 'older than 90 days'], [180, 'older than 6 months'],
  [365, 'older than 1 year'], [730, 'older than 2 years'], [0, 'all of it, today included']];
const DELETE_SCOPES = [['all', 'Everything (system and apps)'], ['system', 'System only (samples and summaries)'], ['apps', 'Apps only']];

/** Every value a field can take, in order (null for a switch). */
function fieldValues(f) {
  if (f.kind === 'choice') return f.choices.map(c => c.value);
  if (f.kind === 'range') { const out = []; for (let v = f.min; v <= f.max; v += f.step) out.push(v); return out; }
  return null;
}
const holds = (rule, v) => (rule.rel === 'le' ? v[rule.a] <= v[rule.b] : rule.rel === 'lt' ? v[rule.a] < v[rule.b] : v[rule.a] > v[rule.b]);
function valueLabel(f, v) {
  if (f.kind === 'bool') return v ? 'on' : 'off';
  if (f.kind === 'range') return `${v} ${f.unit}`;
  const c = f.choices.find(x => x.value === v);
  return c ? c.label : String(v);
}

/**
 * `vals` (which keep every rule) with `key` set to `v` and every rule between fields still kept: a
 * field a rule ties to it moves to its nearest value that keeps the rule, and so on along the chain.
 * Returns the moves, or null when no value of the other fields fits.
 */
function settle(schema, vals, key, v) {
  const byKey = Object.fromEntries(schema.groups.flatMap(g => g.fields).map(f => [f.key, f]));
  const next = Object.assign({}, vals, { [key]: v });
  const pinned = new Set([key]);
  const moved = [];
  for (let round = 0; round < 20; round++) {
    const rule = schema.rules.find(r => !holds(r, next));
    if (!rule) return { vals: next, moved };
    const other = pinned.has(rule.a) ? rule.b : rule.a;
    if (pinned.has(other)) return null;
    const cands = fieldValues(byKey[other]).filter(x => holds(rule, Object.assign({}, next, { [other]: x })));
    if (!cands.length) return null;
    const best = cands.reduce((a, b) => (Math.abs(b - next[other]) < Math.abs(a - next[other]) ? b : a));
    next[other] = best;
    pinned.add(other);
    moved.push([other, rule.text]);
  }
  return null;
}

function pageSettings() {
  let schema = null, hist = null, health = null, stats = null;
  let vals = {}, saved = {};
  const notes = {};
  // The edits made by hand, oldest first: [key, value], each key once, at the place of its last
  // edit. The values shown are the saved ones with these edits made again in order, each through the
  // rules from values that keep them all, so the latest edit always wins and taking an edit back
  // takes back what it moved.
  let hand = [];
  let saving = false;
  let msg = '', msgClass = '';
  let del = { scope: 'all', age: 90, armed: false, busy: false, result: '' };
  let boxes = {};
  const fields = () => (schema ? schema.groups.flatMap(g => g.fields) : []);
  const byKey = k => fields().find(f => f.key === k);
  const dirty = () => fields().filter(f => vals[f.key] !== saved[f.key]);
  const readOnly = () => !hist || hist.read_only;
  // No edits while a save is on its way: its answer replaces the values shown.
  const locked = () => readOnly() || saving;
  const clearNotes = () => { for (const k of Object.keys(notes)) delete notes[k]; };

  function take(s) {
    schema = s;
    saved = Object.assign({}, s.values);
    vals = Object.assign({}, s.values);
    cfg = Object.assign({}, s.values);
    systemDrive = s.system_drive || systemDrive;
    clearNotes();
    hand = [];
  }

  /** The values shown: the saved ones with the hand edits made again, oldest first. */
  function replay() {
    let v = Object.assign({}, saved);
    const why = {}, closest = {};
    for (const [k, x] of hand) {
      const f = byKey(k);
      let res = settle(schema, v, k, x);
      if (!res && f.kind !== 'bool') {
        // Not possible as asked: the nearest value of this field that is.
        for (const y of fieldValues(f).slice().sort((a, b) => Math.abs(a - x) - Math.abs(b - x))) {
          res = settle(schema, v, k, y);
          if (res) {
            const rule = schema.rules.find(r => r.a === k || r.b === k);
            closest[k] = `${valueLabel(f, y)} is the closest possible: ${rule ? rule.text : ''}`;
            break;
          }
        }
      }
      if (!res) continue;
      v = res.vals;
      for (const [m, text] of res.moved) why[m] = text;
    }
    vals = v;
    clearNotes();
    const meant = Object.fromEntries(hand);
    for (const f of fields()) {
      const k = f.key;
      if (closest[k] && vals[k] !== meant[k]) notes[k] = closest[k];
      else if (why[k] && vals[k] !== (k in meant ? meant[k] : saved[k])) notes[k] = `Moved to ${valueLabel(f, vals[k])}: ${why[k]}`;
    }
  }

  function change(key, v) {
    if (locked()) return;
    hand = hand.filter(([k]) => k !== key);
    if (v !== saved[key]) hand.push([key, v]);
    replay();
    msg = '';
    draw();
  }

  async function save() {
    const changed = dirty();
    if (!changed.length || locked()) return;
    saving = true;
    msg = 'Saving…'; msgClass = ''; draw();
    try {
      const answer = await postForm('/api/settings', Object.fromEntries(changed.map(f => [f.key, String(vals[f.key])])));
      saving = false;
      take(answer);
      msg = `Saved ${changed.length} change${changed.length === 1 ? '' : 's'}: the recorder uses ${changed.length === 1 ? 'it' : 'them'} now.`;
      msgClass = 'ok';
      await loadHistory().catch(() => {});
    } catch (e) {
      saving = false;
      msg = `Not saved: ${e.message}`; msgClass = 'bad';
      if (e.field) {
        // Refused against the settings in force: if they changed elsewhere (another tab), show
        // them, with this page's own edits put back on top through the rules.
        const fresh = await getJSON('/api/settings').catch(() => null);
        if (fresh && fields().some(f => fresh.values[f.key] !== saved[f.key])) {
          const mine = hand;
          take(fresh);
          hand = mine.filter(([k, v]) => v !== saved[k]);
          replay();
          msg = `Not saved: the settings were changed elsewhere meanwhile. They are shown now, with your changes on top: check them and save again.`;
          msgClass = 'bad';
        } else notes[e.field] = e.message;
      }
    }
    draw();
  }

  async function loadHistory() {
    [hist, health, stats] = await Promise.all([
      getJSON('/api/history'),
      fetch('/api/health', { cache: 'no-store' }).then(x => x.json()).catch(() => null),
      getJSON('/api/stats').catch(() => null),
    ]);
  }

  // ---- what the history weighs, and what a change would do

  const tierOf = keep => hist && hist.tiers.find(t => t.keep === keep);
  /** Bytes a day the tier grows by: its last complete files, else the current one so far. */
  function perDay(t) {
    const now = Date.now() / 1000;
    const done = t.files.filter(f => f[1] <= now).slice(-7);
    if (done.length) return done.reduce((s, f) => s + f[2], 0) / done.reduce((s, f) => s + (f[1] - f[0]), 0) * 86400;
    const cur = t.files[t.files.length - 1];
    return cur && now > cur[0] + 3600 ? cur[2] / (now - cur[0]) * 86400 : null;
  }
  /** Estimated growth a day of each tier with the edited values. */
  function growth() {
    const out = {};
    for (const t of hist.tiers) {
      let d = perDay(t);
      if (d == null) continue;
      // Samples scale with the interval; switched-off sources weigh nothing.
      if (t.tier === 'raw') d *= saved.interval / vals.interval;
      if (t.scope === 'apps' && !vals.apps) d = 0;
      out[t.keep] = d;
    }
    return out;
  }
  /** A tier's files and the files set aside after damage, by period: [start, end, bytes]. */
  const allFiles = t => [...t.files, ...(t.aside || [])].sort((a, b) => a[0] - b[0]);
  /** What the edited retention of a tier leaves out (as the recorder: whole periods ended before it). */
  const expired = t => { const cutoff = Date.now() / 1000 - vals[t.keep] * 86400; return allFiles(t).filter(f => f[1] <= cutoff); };
  /** Files a shorter retention would delete on saving. */
  function cut(keep) {
    const t = tierOf(keep);
    if (!t || vals[keep] >= saved[keep]) return null;
    const gone = expired(t);
    return gone.length ? { n: gone.length, bytes: gone.reduce((s, f) => s + f[2], 0), from: gone[0][0] * 1000, to: gone[gone.length - 1][1] * 1000 } : null;
  }
  /**
   * Files the ceiling deletes on saving, after the retention: as the recorder, set-aside files
   * first, then the oldest file of the first tier in this order that has more than one.
   */
  function budgetCut() {
    const budget = vals.budget_gb * GIB;
    const retention = ['budget_gb', ...hist.tiers.map(t => t.keep)];
    if (!budget || !retention.some(k => vals[k] !== saved[k])) return null;
    const left = {};
    let total = hist.total;
    for (const t of hist.tiers) {
      const gone = new Set(expired(t));
      total -= [...gone].reduce((s, f) => s + f[2], 0);
      left[t.tier] = { files: t.files.filter(f => !gone.has(f)), aside: (t.aside || []).filter(f => !gone.has(f)) };
    }
    let n = 0, bytes = 0;
    while (total > budget) {
      const asides = Object.values(left).flatMap(l => l.aside.map(f => ({ l, f }))).sort((a, b) => a.f[0] - b.f[0]);
      let f = null;
      if (asides.length) { f = asides[0].f; asides[0].l.aside.splice(asides[0].l.aside.indexOf(f), 1); }
      else {
        const t = ['raw', 'apps', '1m', 'apps-1h', '1h'].find(k => left[k] && left[k].files.length > 1);
        if (!t) break;
        f = left[t].files.shift();
      }
      n++; bytes += f[2]; total -= f[2];
    }
    return n ? { n, bytes } : null;
  }

  function fieldRow(f, group) {
    const v = vals[f.key];
    const off = locked();
    let input;
    if (f.kind === 'bool') {
      input = el('label', { class: 'switch' }, el('input', { type: 'checkbox', disabled: off ? '' : null, onchange: e => change(f.key, e.target.checked) }),
        el('span', { text: v ? 'on' : 'off' }));
      input.querySelector('input').checked = v;
    } else if (f.kind === 'range') {
      const out = el('span', { class: 'rval', text: valueLabel(f, v) });
      const r = el('input', { type: 'range', min: f.min, max: f.max, step: f.step, value: v, disabled: off ? '' : null });
      r.addEventListener('input', () => { out.textContent = valueLabel(f, Number(r.value)); });
      r.addEventListener('change', () => change(f.key, Number(r.value)));
      input = el('span', { class: 'range' }, r, out);
    } else {
      input = el('select', { disabled: off ? '' : null });
      for (const c of f.choices) input.append(el('option', { value: String(c.value), text: c.label }));
      input.value = String(v);
      input.addEventListener('change', () => change(f.key, f.choices.find(c => String(c.value) === input.value).value));
    }
    const isDefault = v === f.default;
    const extra = [];
    if (notes[f.key]) extra.push(el('div', { class: 'fnote warn', text: notes[f.key] }));
    const c = f.key.startsWith('keep_') ? cut(f.key) : null;
    if (c) extra.push(el('div', { class: 'fnote bad', text: `Saving deletes ${c.n} file${c.n === 1 ? '' : 's'} (${format(c.bytes, 'bytes')}) from ${when(c.from)} to ${when(c.to)}.` }));
    const b = f.key === 'budget_gb' && hist ? budgetCut() : null;
    if (b) extra.push(el('div', { class: 'fnote bad', text: `Saving deletes ${b.n} more file${b.n === 1 ? '' : 's'} (${format(b.bytes, 'bytes')}), the oldest first, to stay under the ceiling.` }));
    if (group === 'Recording' && f.kind === 'bool' && saved[f.key] && !v) extra.push(el('div', { class: 'fnote', text: 'What is already recorded stays; new samples leave it out.' }));
    return el('div', { class: `field${v !== saved[f.key] ? ' edited' : ''}` },
      el('div', { class: 'flabel' }, el('span', { text: f.label }),
        f.help ? el('div', { class: 'fhelp', text: f.help }) : null, ...extra),
      el('div', { class: 'finput' }, input,
        isDefault ? el('span', { class: 'fdefault', text: 'default' })
          : el('button', { class: 'link', text: `default: ${valueLabel(f, f.default)}`, title: 'Use the default', disabled: off ? '' : null, onclick: () => change(f.key, f.default) })));
  }

  function drawSettings() {
    const box = boxes.settings;
    const parts = [];
    if (readOnly()) parts.push(el('div', { class: 'banner', text: 'This is a read-only copy (kymo serve): change the settings on the recorder\'s own page.' }));
    for (const g of schema.groups) {
      const wide = g.fields.length > 8 || g.fields.length < 3;
      const p = el('section', { class: `panel ${wide ? 'w24 cols2' : 'w12'} settings`, id: groupId(g.name) }, el('div', { class: 'phead' }, el('h3', { text: g.name })));
      if (g.name === 'Desktop card') {
        p.append(el('div', { class: 'fnote', text: `The card follows these with its next sample (every ${valueLabel(byKey('interval'), saved.interval)}). Its place and size are set on the desktop: drag the card to move it; drag an edge to make it wider or taller, or a corner (or hold Ctrl and scroll over it) to make all of it bigger or smaller; hold Shift and scroll over it to change its background opacity. Right-click the card to come back here.` }));
      }
      if (g.name === 'Recording' && hist) {
        const raw = growth().keep_raw;
        if (raw != null) p.append(el('div', { class: 'fnote', text: `At ${valueLabel(byKey('interval'), vals.interval)} the samples take about ${format(raw, 'bytes')} a day.` }));
      }
      if (g.name === 'Retention' && hist) {
        // Saving any retention or ceiling change makes the recorder delete at once everything the
        // retention no longer keeps, in every tier (the hourly run does one file at a time).
        if (['budget_gb', ...hist.tiers.map(t => t.keep)].some(k => vals[k] !== saved[k])) {
          const gone = hist.tiers.flatMap(t => expired(t));
          if (gone.length) p.append(el('div', { class: 'fnote bad', text: `Saving deletes ${gone.length} file${gone.length === 1 ? '' : 's'} (${format(gone.reduce((x, f) => x + f[2], 0), 'bytes')}) that the retention no longer keeps, in all tiers.` }));
        }
        const gr = growth();
        let total = 0, known = true;
        for (const t of hist.tiers) {
          if (gr[t.keep] == null) { known = false; continue; }
          total += gr[t.keep] * vals[t.keep];
        }
        const budget = vals.budget_gb * GIB;
        const line = `Kept this long, the history would settle at about ${format(total, 'bytes')}${known ? '' : ' (some tiers have no data yet to go by)'}.`;
        p.append(el('div', { class: `fnote${budget && total > budget ? ' warn' : ''}` }, line + (budget && total > budget
          ? ` That is more than the ${format(budget, 'bytes')} ceiling: the oldest files will go before their time.` : '')));
      }
      for (const f of g.fields) p.append(fieldRow(f, g.name));
      parts.push(p);
    }
    const n = dirty().length;
    const off = locked();
    const bar = el('div', { class: 'savebar w24' },
      el('button', { class: 'primary', text: saving ? 'Saving…' : n ? `Save ${n} change${n === 1 ? '' : 's'}` : 'Saved', disabled: n && !off ? null : '', onclick: save }),
      el('button', { text: 'Undo changes', disabled: n && !off ? null : '', onclick: () => { hand = []; replay(); msg = ''; draw(); } }),
      el('button', {
        text: 'Set all to defaults', title: 'Puts every field at its default; nothing changes until you save', disabled: off ? '' : null,
        onclick: () => {
          hand = fields().filter(f => f.default !== saved[f.key]).map(f => [f.key, f.default]);
          replay(); msg = 'Every field is at its default: save to apply.'; msgClass = ''; draw();
        },
      }),
      el('span', { class: `savemsg ${msgClass}`, text: msg }));
    box.replaceChildren(bar, ...parts);
  }

  function drawStorage() {
    const box = boxes.storage;
    if (!hist) return box.replaceChildren();
    const now = Date.now() / 1000;
    const rows = hist.tiers.map(t => {
      const f = t.files, aside = t.aside || [];
      const asideBytes = aside.reduce((s, x) => s + x[2], 0);
      return el('tr', {}, el('td', { text: t.label }),
        el('td', { text: aside.length ? `${f.length} + ${aside.length} set aside`: String(f.length), title: aside.length ? 'Set aside: parts of files found damaged, kept for their period' : null }),
        el('td', { text: format(t.bytes + asideBytes, 'bytes') }),
        el('td', { text: f.length ? when(f[0][0] * 1000) : '–' }), el('td', { text: f.length ? (f[f.length - 1][1] > now ? 'now' : when(f[f.length - 1][1] * 1000)) : '–' }),
        el('td', { text: keptFor(t.keep_days) }));
    });
    const p = el('section', { class: 'panel w16' }, el('div', { class: 'phead' }, el('h3', { text: 'History on disk' })),
      el('table', { class: 'data' }, el('thead', {}, el('tr', {}, ...['Tier', 'Files', 'Size', 'Files cover from', 'to', 'Kept for'].map(h => el('th', { text: h })))), el('tbody', {}, ...rows)),
      el('div', { class: 'fnote' }, `${format(hist.total, 'bytes')} in all${hist.budget ? `, of a ${format(hist.budget, 'bytes')} ceiling` : ''}`
        + `${Number.isFinite(hist.free) ? ` · ${format(hist.free, 'bytes')} free on its disk` : ''} · folder ${hist.folder}`));
    const info = el('section', { class: 'panel w8' }, el('div', { class: 'phead' }, el('h3', { text: 'Recorder' })),
      el('table', { class: 'data kv' }, el('tbody', {},
        ...[
          ['Version', stats ? stats.version : '–'],
          ['Sampling', `every ${valueLabel(byKey('interval'), saved.interval)}`],
          ['Last sample', health && health.last_sample ? `${when(health.last_sample * 1000, true)} (${health.status})` : '–'],
          ['Its own cost', stats ? `${format(stats.cpu_percent_of_one_core.last_10s, 'percent')} of one core, ${format(stats.memory_bytes.private, 'bytes')}` : '–'],
          ['Write errors', stats ? String(stats.errors || 0) : '–'],
          ['Settings file', `${hist.folder}\\settings.conf`],
        ].map(([k, v]) => el('tr', {}, el('td', { text: k }), el('td', { text: v }))))));
    box.replaceChildren(p, info);
  }

  /** The files a deletion would take, from the history list (whole files, as the recorder deletes). */
  function deletion() {
    const scope = hist.tiers.filter(t => del.scope === 'all' || t.scope === del.scope);
    const cutoff = Date.now() / 1000 - del.age * 86400;
    // The files set aside after damage go with their period too (and all of them with "all").
    const files = scope.flatMap(t => (del.age ? allFiles(t).filter(f => f[1] <= cutoff) : allFiles(t)));
    return { n: files.length, bytes: files.reduce((s, f) => s + f[2], 0), before: del.age ? Math.floor(cutoff) : 'all' };
  }

  function drawDelete() {
    const box = boxes.del;
    if (!hist) return box.replaceChildren();
    const off = readOnly() || del.busy;
    const scope = el('select', { disabled: off ? '' : null }, ...DELETE_SCOPES.map(([v, t]) => el('option', { value: v, text: t })));
    scope.value = del.scope;
    scope.addEventListener('change', () => { del.scope = scope.value; del.armed = false; del.result = ''; drawDelete(); });
    const age = el('select', { disabled: off ? '' : null }, ...DELETE_AGES.map(([v, t]) => el('option', { value: String(v), text: t })));
    age.value = String(del.age);
    age.addEventListener('change', () => { del.age = Number(age.value); del.armed = false; del.result = ''; drawDelete(); });
    const d = deletion();
    const what = d.n ? `${d.n} file${d.n === 1 ? '' : 's'}, ${format(d.bytes, 'bytes')}` : 'nothing';
    const go = async () => {
      if (!del.armed) { del.armed = true; return drawDelete(); }
      del.busy = true; del.armed = false; drawDelete();
      try {
        const r = await postForm('/api/history/delete', { scope: del.scope, before: String(d.before) });
        del.result = `Deleted ${r.files} file${r.files === 1 ? '' : 's'} (${format(r.bytes, 'bytes')}).`;
      } catch (e) { del.result = `Not deleted: ${e.message}`; }
      del.busy = false;
      await loadHistory().catch(() => {});
      if (saving) { drawStorage(); drawDelete(); } else draw();
    };
    const buttons = del.armed
      ? [el('button', { class: 'danger', text: `Delete ${what} for good`, onclick: go }), el('button', { text: 'Cancel', onclick: () => { del.armed = false; drawDelete(); } })]
      : [el('button', { text: 'Delete…', disabled: off || !d.n ? '' : null, onclick: go })];
    box.replaceChildren(el('section', { class: 'panel w24' }, el('div', { class: 'phead' }, el('h3', { text: 'Clear history' })),
      el('div', { class: 'fhelp' }, 'Deletes whole files: days of samples and apps, months of hourly summaries. '
        + (del.age ? 'A file goes only when all of it is older than the age picked.' : 'Recording goes on right away into new files.')),
      el('div', { class: 'delrow' }, scope, age, el('span', { class: 'range-label', text: `That is ${what}.` }), ...buttons,
        el('span', { class: 'savemsg', text: del.result }))));
  }

  function draw() {
    if (!schema) return;
    drawSettings();
    drawStorage();
    drawDelete();
  }

  return {
    build(main) {
      const g = row(main, 'Settings');
      boxes.settings = el('div', { class: 'grid w24 inner' });
      g.append(boxes.settings);
      const g2 = row(main, 'History');
      boxes.storage = el('div', { class: 'grid w24 inner' });
      boxes.del = el('div', { class: 'grid w24 inner' });
      g2.append(boxes.storage, boxes.del);
      window.onbeforeunload = () => (dirty().length ? true : undefined);
    },
    leaving: () => !dirty().length || confirm('Leave the Settings page without saving your changes?'),
    async load(r, alive) {
      // Edits in progress are never redrawn away.
      if (schema && dirty().length) return;
      const s = await getJSON('/api/settings');
      await loadHistory();
      if (!alive()) return;
      take(s);
      draw();
      // Asked for a group (the card's right-click): show it once, on the first draw, with its heading below the
      // sticky bars (the top bar, and the save bar under it on wide windows).
      const target = state.group && document.getElementById(groupId(state.group));
      state.group = null;
      if (target) {
        // Measured as they stick: the top bar at 0, the save bar at its own sticky offset (0 on narrow windows).
        const top = document.getElementById('top'), bar = boxes.settings.querySelector('.savebar');
        const covered = Math.max(top ? top.offsetHeight : 0, bar ? (parseFloat(getComputedStyle(bar).top) || 0) + bar.offsetHeight : 0);
        window.scrollTo(0, Math.max(0, window.scrollY + target.getBoundingClientRect().top - covered - 8));
      }
    },
  };
}

// ---------------------------------------------------------------- start

async function start() {
  try {
    const s = await getJSON('/api/series');
    const n = s.series.filter(x => /^cpu\.core\..+\.busy$/.test(x.key)).length;
    if (n) cores = n;
  } catch (e) { /* keep the browser's count */ }
  await loadSettings();
  app.render();
}
start();
})();
