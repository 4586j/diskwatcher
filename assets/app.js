// diskwatch Web 界面：无构建工具、无框架，纯原生 JS。
// hash 路由：file:// 预览与 http://127.0.0.1:8421 下行为一致；#mock 启用内置假数据。
// 渲染安全：所有数据一律走 textContent / setAttribute，禁止 innerHTML 插值。
'use strict';
(() => {

// ---------- 小工具 ----------
const $ = id => document.getElementById(id);
function el(tag, cls, text) {
  const n = document.createElement(tag);
  if (cls) n.className = cls;
  if (text !== undefined && text !== null) n.textContent = text;
  return n;
}
function humanBytes(n) {
  n = Number(n) || 0;
  const u = ['B', 'KB', 'MB', 'GB', 'TB'];
  let i = 0, v = n;
  while (v >= 1024 && i < u.length - 1) { v /= 1024; i++; }
  return i === 0 ? Math.round(n) + ' B' : v.toFixed(1) + ' ' + u[i];
}
const p2 = x => String(x).padStart(2, '0');
function fmtMdHM(ts) { const d = new Date(ts * 1000); return `${p2(d.getMonth() + 1)}-${p2(d.getDate())} ${p2(d.getHours())}:${p2(d.getMinutes())}`; }
function fmtHM(ts) { const d = new Date(ts * 1000); return `${p2(d.getHours())}:${p2(d.getMinutes())}`; }
function fmtFull(ts) { const d = new Date(ts * 1000); return `${d.getFullYear()}-${p2(d.getMonth() + 1)}-${p2(d.getDate())} ${p2(d.getHours())}:${p2(d.getMinutes())}`; }
function fmtAgo(ts, now) {
  const s = Math.max(0, (now || Math.floor(Date.now() / 1000)) - ts);
  if (s < 60) return s + ' 秒前';
  if (s < 3600) return Math.floor(s / 60) + ' 分钟前';
  if (s < 86400) return Math.floor(s / 3600) + ' 小时前';
  return Math.floor(s / 86400) + ' 天前';
}
const DUR = { m: 60, h: 3600, d: 86400 };
function parseDur(s) {
  const m = /^(\d+)([mhd])$/.exec(s);
  return m ? Number(m[1]) * DUR[m[2]] : 3600;
}
function durLabel(s) {
  const m = /^(\d+)([mhd])$/.exec(s);
  if (!m) return s;
  return m[1] + ' ' + { m: '分钟', h: '小时', d: '天' }[m[2]];
}
function animateCount(node, target, fmt) {
  const finish = () => { node.textContent = fmt(target); };
  // 后台标签页里 rAF 完全不跑，动画会让数字停在初始的 0。
  if (document.hidden) { finish(); return; }
  const t0 = performance.now(), dur = 900;
  function step(t) {
    // 动画跑到一半切走：rAF 被冻结，直接落终值，否则数字会停在半程。
    if (document.hidden) { finish(); return; }
    const p = Math.max(0, Math.min(1, (t - t0) / dur));
    const e = 1 - Math.pow(1 - p, 3);
    node.textContent = fmt(target * e);
    if (p < 1) requestAnimationFrame(step);
  }
  requestAnimationFrame(step);
}

// ---------- 视图与列定义 ----------
const GROUPS = {
  writes: ['file', 'dir', 'none'],
  usn: ['file', 'dir'],
  alerts: ['none'],
};
const VIEWS = {
  writes: { label: '实时写入', api: '/api/writes', defSort: 'bytes', defDir: 'desc' },
  usn: { label: '离线补录', api: '/api/usn', defSort: 'bytes', defDir: 'desc' },
  alerts: { label: '大写入告警', api: '/api/alerts', defSort: 'ts', defDir: 'asc' },
};
// 每列：label / sort（可空）/ cls / render(row) → 单元格节点
function colsFor(view, group) {
  const rank = { label: '#', cls: 'rank', render: r => el('td', 'rank', String(r._rank)) };
  const bytes = { label: '写入量', sort: 'bytes', cls: 'num', render: r => numCell(r.bytes, r._w) };
  const size = { label: '大小', sort: 'bytes', cls: 'num', render: r => numCell(r.bytes, r._w) };
  const ops = { label: '次数', sort: 'ops', cls: 'num', render: r => numCell(r.ops, r._w2, '') };
  const timeRange = { label: '时间', sort: 'time_end', cls: 'time', render: r =>
    el('td', 'time', r.t0 === r.t1 ? fmtMdHM(r.t0) : `${fmtHM(r.t0)}~${fmtHM(r.t1)}`) };
  const tsCol = { label: '时间', sort: 'ts', cls: 'time', render: r => el('td', 'time', fmtMdHM(r.ts)) };
  const proc = { label: '进程', sort: 'process', cls: 'proc', render: r => {
    const list = String(r.process || '').split(',').filter(Boolean);
    const t = list.length > 1 ? `${list[0]} +${list.length - 1}` : (list[0] || '—');
    const td = el('td', 'proc', t);
    td.title = list.join(', ');
    return td;
  } };
  const path = { label: '文件路径', sort: 'path', cls: 'path', render: r => {
    const td = el('td', 'path', r.path);
    td.title = r.path;
    if (view === 'writes' && group === 'file') td.classList.add('drill');
    return td;
  } };
  const dir = { label: '目录', sort: 'path', cls: 'path', render: r => {
    const td = el('td', 'path', r.path);
    td.title = r.path;
    td.classList.add('drill');
    return td;
  } };
  const notified = { label: '通知', cls: 'badge', render: r =>
    el('td', 'badge ' + (r.notified ? 'ok' : 'no'), r.notified ? '已推送' : '待推送') };
  if (view === 'writes') {
    if (group === 'none') return [rank, bytes, ops, tsCol, proc, path];
    return [rank, bytes, ops, timeRange, proc, group === 'dir' ? dir : path];
  }
  if (view === 'usn') return [rank, size, group === 'dir' ? dir : path, tsCol];
  return [rank, tsCol, bytes, proc, path, notified];
}

// ---------- 状态（hash 驱动，单一事实源） ----------
function parseHash() {
  const h = location.hash.replace(/^#/, '');
  const sp = new URLSearchParams(h);
  const st = {
    mock: h === 'mock' || sp.has('mock'),
    view: sp.get('view') || 'writes',
    group: sp.get('group') || 'file',
    last: sp.get('last') || '1h',
    from: sp.get('from'), to: sp.get('to'),
    q: sp.get('q') || '', process: sp.get('process') || '',
    sort: sp.get('sort') || '', order: sp.get('order') || '',
    limit: Number(sp.get('limit')) || 50, offset: Number(sp.get('offset')) || 0,
  };
  return normalize(st);
}
function normalize(st) {
  if (!VIEWS[st.view]) st.view = 'writes';
  // 告警是行级视图，没有聚合概念：为它改写 group 会泄漏回写入视图，
  // 用户从告警切回"实时写入"就会莫名停在分钟明细。
  if (GROUPS[st.view].length > 1 && !GROUPS[st.view].includes(st.group)) st.group = GROUPS[st.view][0];
  if (st.view === 'usn') st.process = '';
  const keys = colsFor(st.view, st.group).filter(c => c.sort).map(c => c.sort);
  if (!st.sort || !keys.includes(st.sort)) {
    st.sort = keys.includes(VIEWS[st.view].defSort) ? VIEWS[st.view].defSort : keys[0];
    st.order = VIEWS[st.view].defDir;
  }
  if (st.order !== 'asc' && st.order !== 'desc') st.order = 'desc';
  st.limit = Math.min(500, Math.max(10, st.limit));
  st.offset = Math.max(0, Math.floor(st.offset) || 0);
  if (st.from && st.to && Number(st.from) >= Number(st.to)) { st.from = st.to = null; st.last = '1h'; }
  return st;
}
function hashFor(st) {
  const sp = new URLSearchParams();
  if (st.mock) sp.set('mock', '1');
  if (st.view !== 'writes') sp.set('view', st.view);
  if (GROUPS[st.view].length > 1 && st.group !== GROUPS[st.view][0]) sp.set('group', st.group);
  if (st.from && st.to) { sp.set('from', st.from); sp.set('to', st.to); }
  else sp.set('last', st.last);
  if (st.q) sp.set('q', st.q);
  if (st.process) sp.set('process', st.process);
  sp.set('sort', st.sort); sp.set('order', st.order);
  if (st.limit !== 50) sp.set('limit', st.limit);
  if (st.offset) sp.set('offset', st.offset);
  const s = sp.toString();
  return s ? '#' + s : '#';
}
let S = parseHash();
function applyPatch(patch, push) {
  const st = normalize(Object.assign({}, S, patch));
  const same = hashFor(st) === hashFor(S);
  S = st;
  if (!same) {
    const href = location.pathname + hashFor(st);
    if (push) history.pushState(null, '', href);
    else history.replaceState(null, '', href);
  }
  syncControls();
  refreshAll();
}

// ---------- 数据层：真 fetch 与 mock 假 fetch 同接口 ----------
let inflight = null;
async function api(path, params, signal) {
  const qs = new URLSearchParams(params).toString();
  const url = path + (qs ? '?' + qs : '');
  if (S.mock) {
    await new Promise(r => setTimeout(r, 60 + Math.random() * 140));
    if (signal && signal.aborted) throw Object.assign(new Error('abort'), { name: 'AbortError' });
    return mockApi(url);
  }
  const res = await fetch(url, { signal });
  const j = await res.json().catch(() => null);
  if (!j || j.ok !== true) throw new Error(j && j.error ? j.error.message : '接口返回异常 (' + res.status + ')');
  return j;
}

// mock 数据（时间戳在加载时按当前时间生成，时间预设切换才有真实感）
const MOCK = (() => {
  const NOW = Math.floor(Date.now() / 1000);
  const m = minutesAgo => NOW - minutesAgo * 60;
  const MB = 1024 * 1024;
  const longPath = 'C:\\Users\\33833\\projects\\workspace\\2026\\季度报表\\归档目录\\segment-'
    + 'abcdefghij'.repeat(10) + '\\payload-一二三四五六七八九十.bin';
  const WRITES = [
    [2,  r('C:\\Users\\33833\\AppData\\Local\\Temp\\dw-gap-test.bin'), 'explorer.exe',      150 * MB, 6],
    [4,  r('C:\\Windows\\Temp\\report-2026-09-01.html'),            'diskwatch.exe',     4 * MB,   2],
    [9,  r('C:\\Users\\33833\\Documents\\财务归档\\2026年9月报表.xlsx'), 'EXCEL.EXE',       12.4 * MB, 5],
    [13, r('C:\\Users\\33833\\AppData\\Local\\Microsoft\\Edge\\User Data\\Default\\000003.log'), 'msedge.exe', 33 * MB, 40],
    [13, r('C:\\Users\\33833\\Downloads\\dw-crash-test.bin'),        'chrome.exe',        100 * MB, 10],
    [21, r('C:\\ProgramData\\Microsoft\\Windows\\Power Shell\\Module Analysis Cache\\module.dat'), 'powershell.exe', 1.6 * MB, 3],
    [27, r('D:\\pagefile.sys'),                                    'System',            512 * MB, 300],
    [33, r('C:\\$Recycle.Bin\\S-1-5-21-3846\\$I2W0ZC.tmp'),            'explorer.exe',      0.2 * MB, 1],
    [41, r('C:\\Users\\33833\\AppData\\Roaming\\Code\\User\\settings.json'), 'Code.exe',     24 * KB(), 2],
    [48, r(longPath),                                             'git.exe',           88 * MB, 20],
    [57, r('C:\\Users\\33833\\projects\\<img src=x onerror=alert(1)>\\qq\\dw-probe.bin'), 'node.exe', 7 * MB, 4],
    [64, r('C:\\Windows\\Logs\\DISM\\dism.log'),                      'TiWorker.exe',      2.3 * MB, 6],
    [72, r('C:\\Users\\33833\\AppData\\Local\\Packages\\Cortana\\~app.dat'), 'SearchIndexer.exe', 18 * MB, 25],
    [95, r('C:\\Users\\33833\\AppData\\Local\\npm-cache\\_cacache\\index-v5\\ab\\cd'), 'node.exe', 6.4 * MB, 9],
    [120, r('C:\\Intel\\logging\\ihpmTracker.log'),                  'ipf_helper.exe',    1.4 * MB, 140],
    [160, r('C:\\Users\\33833\\AppData\\Local\\Temp\\setup_20260901.log'), 'msiexec.exe',    3.9 * MB, 12],
    [230, r('C:\\Program Files\\Windows NT\\Accessories\\testdata.dat'), 'taskhostw.exe',  0.6 * MB, 2],
    [400, r('C:\\Users\\33833\\Documents\\笔记\\week41.md'),           'Notepad.exe',       18 * KB(), 1],
    [700, r('C:\\Users\\33833\\AppData\\Local\\CrashDumps\\idea.exe.1234.dmp'), 'WerFault.exe', 246 * MB, 33],
    [1500, r('C:\\PerfLogs\\admin2.data.dat'),                      'Perflis',           72 * MB, 90],
    [2600, r('C:\\Users\\33833\\Downloads\\ubuntu-2404-wsl.amz2.vhdx'), 'wslservice.exe',  1.8 * GB(), 60],
    [5000, r('C:\\Users\\33833\\AppData\\Local\\JetBrains\\IntelliJIdea2026.2\\caches\\content.dat'), 'idea64.exe', 640 * MB, 120],
  ].map(([mm, path, process, bytes, ops]) => ({ ts: m(mm), path, process, bytes: Math.round(bytes), ops }));
  const USN = [
    [180, r('C:\\Users\\33833\\Downloads\\归档.zip'),          66 * MB],
    [320, r('C:\\Users\\33833\\Downloads\\dw-backfill.bin'),   100 * MB],
    [760, r('C:\\Users\\33833\\Documents\\测试 文件 (1).txt'), 2 * KB()],
    [1400, r('C:\\temp\\large with spaces.bin'),             780 * MB],
    [3000, r('C:\\Users\\33833\\Downloads\\video-片段.mp4'),   1.2 * GB()],
  ].map(([mm, path, bytes]) => ({ ts: m(mm), path, bytes: Math.round(bytes) }));
  const ALERTS = [
    [2,   r('C:\\Users\\33833\\AppData\\Local\\Temp\\dw-gap-test.bin'),  'explorer.exe',       150 * MB, 1],
    [13,  r('C:\\Users\\33833\\Downloads\\dw-crash-test.bin'),         'chrome.exe',         100 * MB, 1],
    [27,  r('D:\\pagefile.sys'),                                     'System',             512 * MB, 0],
    [700, r('C:\\Users\\33833\\AppData\\Local\\CrashDumps\\idea.exe.1234.dmp'), 'WerFault.exe', 246 * MB, 1],
    [5000, r('C:\\Users\\33833\\AppData\\Local\\JetBrains\\IntelliJIdea2026.2\\caches\\content.dat'), 'idea64.exe', 640 * MB, 0],
  ].map(([mm, path, process, bytes, notified]) => ({ ts: m(mm), path, process, bytes, notified }));
  function r(s) { return s; }
  function KB() { return 1024; }
  function GB() { return 1024 * 1024 * 1024; }
  return { NOW: () => NOW, WRITES, USN, ALERTS };
})();

function resolveWindow(params) {
  if (params.from && params.to) {
    return { from: Number(params.from), to: Number(params.to), custom: true };
  }
  const last = params.last || '1h';
  const to = Math.floor(Date.now() / 1000);
  return { from: to - parseDur(last), to, last };
}
function mockDir(p) {
  const i = Math.max(p.lastIndexOf('\\'), p.lastIndexOf('/'));
  return i > 2 ? p.slice(0, i) : 'C:\\';
}
function mockGroupWrites(rows, group) {
  if (group === 'none') {
    return rows.map(w => ({
      path: w.path, bytes: w.bytes, ops: w.ops,
      t0: w.ts, t1: w.ts, ts: w.ts, process: w.process,
    }));
  }
  const map = new Map();
  for (const w of rows) {
    const key = group === 'dir' ? mockDir(w.path) : w.path;
    let g = map.get(key);
    if (!g) { g = { path: key, bytes: 0, ops: 0, t0: w.ts, t1: w.ts, procs: new Set() }; map.set(key, g); }
    g.bytes += w.bytes; g.ops += w.ops;
    g.t0 = Math.min(g.t0, w.ts); g.t1 = Math.max(g.t1, w.ts);
    g.procs.add(w.process);
  }
  return [...map.values()].map(g => ({ ...g, process: [...g.procs].join(',') }));
}
function mockFilter(rows, params, opts) {
  const w = resolveWindow(params);
  const q = (params.q || '').toLowerCase();
  const proc = params.process || '';
  let out = rows.filter(r0 => {
    const t = opts.overlap && r0.t0 !== undefined ? [r0.t0, r0.t1] : [r0.ts, r0.ts];
    if (t[0] >= w.to || t[1] < w.from) return false;
    if (q && !String(r0.path).toLowerCase().includes(q)) return false;
    if (proc) {
      if (opts.procExact) { if (r0.process !== proc) return false; }
      else if (!String(r0.process || '').includes(proc)) return false;
    }
    return true;
  });
  const sort = params.sort || opts.defSort;
  const dir = params.order === 'asc' ? 1 : -1;
  const val = r0 => {
    switch (sort) {
      case 'bytes': return r0.bytes;
      case 'ops': return r0.ops || 0;
      case 'ts': return r0.ts !== undefined ? r0.ts : r0.t1;
      case 'time_end': return r0.t1;
      case 'path': return null;
      case 'process': return String(r0.process || '');
      case 'notified': return r0.notified || 0;
      default: return r0.bytes;
    }
  };
  out.sort((a, b0) => {
    const va = val(a), vb = val(b0);
    if (va === null) return dir * String(a.path).localeCompare(String(b0.path), 'zh-CN');
    if (typeof va === 'string') return dir * va.localeCompare(vb);
    return dir * (va - vb);
  });
  return { rows: out, w };
}
function mockPage(all, params) {
  const limit = Math.max(1, Math.min(500, Number(params.limit) || 50));
  const offset = Math.max(0, Number(params.offset) || 0);
  const rows = all.slice(offset, offset + limit);
  rows.forEach((r0, i) => { r0._rank = offset + i + 1; });
  return {
    ok: true,
    query: { from: params._from, to: params._to, last: params.last || null, q: params.q || null, process: params.process || null, group: params.group || null },
    page: { total: all.length, limit, offset, has_more: offset + limit < all.length },
    rows,
  };
}
function mockOverviewWrites(params) {
  const w = resolveWindow(params);
  const q = (params.q || '').toLowerCase();
  const rows = mockGroupWrites(MOCK.WRITES, 'file')
    .filter(r0 => r0.t0 < w.to && r0.t1 >= w.from && (!q || r0.path.toLowerCase().includes(q)));
  const bytes = rows.reduce((s, r0) => s + r0.bytes, 0);
  const ops = rows.reduce((s, r0) => s + r0.ops, 0);
  const procs = new Set();
  MOCK.WRITES.forEach(x => { if (x.ts < w.to && x.ts >= w.from && (!q || x.path.toLowerCase().includes(q))) procs.add(x.process); });
  const maxRow = rows.reduce((m, r0) => (!m || r0.bytes > m.bytes ? r0 : m), null);
  return {
    bytes, ops, files: rows.length, procs: procs.size,
    max_file: maxRow ? maxRow.bytes : 0,
    max_file_path: maxRow ? maxRow.path : '',
  };
}
function mockApi(url) {
  const [path, qs] = url.split('?');
  const params = Object.fromEntries(new URLSearchParams(qs || ''));
  const envelope = data => ({ ok: true, ...data });
  if (path === '/api/overview') {
    const t = mockOverviewWrites(params);
    return envelope({ totals: t, hourly: mockHourly(params), procs: mockProcsTop(params, 10), dirs: mockDirsTop(params, 8) });
  }
  if (path === '/api/writes') {
    const group = params.group || 'file';
    const all = mockGroupWrites(MOCK.WRITES, group);
    const r0 = mockFilter(all, params, { overlap: true, procExact: false, defSort: 'bytes' });
    params._from = r0.w.from; params._to = r0.w.to;
    return envelope({ query: {}, page: {}, rows: [], ...mockPage(r0.rows, params) });
  }
  if (path === '/api/usn') {
    if (params.process) return { ok: false, error: { code: 'bad_request', message: 'usn 不支持进程过滤' } };
    let all = MOCK.USN;
    if (params.group === 'dir') {
      const map = new Map();
      for (const u of all) {
        const d = mockDir(u.path);
        let g = map.get(d);
        if (!g) { g = { path: d, bytes: 0, ts: 0 }; map.set(d, g); }
        g.bytes += u.bytes; g.ts = Math.max(g.ts, u.ts);
      }
      all = [...map.values()];
    } else {
      all = all.map(u => ({ ...u }));
    }
    const r0 = mockFilter(all, params, { overlap: false, procExact: false, defSort: 'bytes' });
    params._from = r0.w.from; params._to = r0.w.to;
    return envelope({ query: {}, page: {}, rows: [], ...mockPage(r0.rows, params) });
  }
  if (path === '/api/alerts') {
    const all = MOCK.ALERTS.map(a => ({ ...a, ts: a.ts }));
    const r0 = mockFilter(all, params, { overlap: false, procExact: false, defSort: 'ts' });
    params._from = r0.w.from; params._to = r0.w.to;
    return envelope({ query: {}, page: {}, rows: [], ...mockPage(r0.rows, params) });
  }
  if (path === '/api/processes') {
    const w = resolveWindow(params);
    const map = new Map();
    for (const x of MOCK.WRITES) {
      if (x.ts >= w.to || x.ts < w.from) continue;
      let g = map.get(x.process);
      if (!g) { g = { process: x.process, bytes: 0, ops: 0 }; map.set(x.process, g); }
      g.bytes += x.bytes; g.ops += x.ops;
    }
    const rows = [...map.values()].sort((a, b) => b.bytes - a.bytes).slice(0, 200);
    return envelope({ query: {}, page: { total: rows.length, limit: 200, offset: 0, has_more: false }, rows });
  }
  if (path === '/api/status') {
    const rows = MOCK.WRITES.length + MOCK.USN.length + MOCK.ALERTS.length;
    const bytes = MOCK.WRITES.reduce((s, x) => s + x.bytes, 0);
    return envelope({
      version: 'mock', port: 0, mock: true,
      collector: { last_flush: MOCK.NOW() - 12, stale: false, flush_secs: 30 },
      usn: { last_backfill: MOCK.NOW() - 46 * 60, journal_id: 0x1234 },
      db: { writes: rows, usn: MOCK.USN.length, alerts: MOCK.ALERTS.length, bytes, approx: false },
      config: { drive: 'C:', min_bytes: 1048576, flush_secs: 30, keep_days: 30, alert_bytes: 104857600 },
    });
  }
  return { ok: false, error: { code: 'not_found', message: '未知接口 ' + path } };
}
function mockHourly(params) {
  const w = resolveWindow(params);
  const start = Math.floor(w.from / 3600) * 3600;
  const end = Math.floor(w.to / 3600) * 3600;
  const n = Math.min(168, Math.floor((end - start) / 3600) + 1);
  const buckets = new Array(n).fill(0);
  for (const x of MOCK.WRITES) {
    if (x.ts >= w.to || x.ts < w.from) continue;
    const i = Math.floor((Math.floor(x.ts / 3600) * 3600 - start) / 3600);
    if (i >= 0 && i < n) buckets[i] += x.bytes;
  }
  return buckets.map((v, i) => [start + i * 3600, v]);
}
function mockProcsTop(params, n) {
  const w = resolveWindow(params);
  const map = new Map();
  for (const x of MOCK.WRITES) {
    if (x.ts >= w.to || x.ts < w.from) continue;
    let g = map.get(x.process);
    if (!g) { g = { process: x.process, bytes: 0, ops: 0, files: new Set() }; map.set(x.process, g); }
    g.bytes += x.bytes; g.ops += x.ops; g.files.add(x.path);
  }
  return [...map.values()].sort((a, b) => b.bytes - a.bytes).slice(0, n)
    .map(g => ({ process: g.process, bytes: g.bytes, ops: g.ops, files: g.files.size }));
}
function mockDirsTop(params, n) {
  const w = resolveWindow(params);
  const map = new Map();
  for (const x of MOCK.WRITES) {
    if (x.ts >= w.to || x.ts < w.from) continue;
    const d = mockDir(x.path);
    let g = map.get(d);
    if (!g) { g = { dir: d, bytes: 0, ops: 0 }; map.set(d, g); }
    g.bytes += x.bytes; g.ops += x.ops;
  }
  return [...map.values()].sort((a, b) => b.bytes - a.bytes).slice(0, n);
}

// ---------- 渲染 ----------
let loadGen = 0;
let loadingCount = 0;
function loading(delta) {
  loadingCount = Math.max(0, loadingCount + delta);
  $('loading').hidden = loadingCount === 0;
}
function setError(msg) {
  const e = $('err');
  if (msg) { e.textContent = '出错了：' + msg; e.hidden = false; }
  else { e.hidden = true; e.textContent = ''; }
}

function renderHeader() {
  const w = resolveWindow({ from: S.from, to: S.to, last: S.last });
  const range = `${fmtFull(w.from)} ~ ${fmtFull(w.to)}`;
  const label = w.custom ? range : `最近 ${durLabel(S.last)} · ${range}`;
  $('subtitle').textContent = (S.mock ? 'Mock 预览 · ' : '') + label + ' · 页面刷新于 ' + fmtFull(Math.floor(Date.now() / 1000));
}

function renderPills(st) {
  const pills = $('pills');
  pills.textContent = '';
  if (S.mock) pills.appendChild(pill('mock', 'MOCK 假数据'));
  if (st) {
    if (st.mock) pills.appendChild(pill('good', '采集器 运行中（mock）'));
    else if (st.collector && st.collector.last_flush) {
      const cls = st.collector.stale ? 'warn' : 'good';
      pills.appendChild(pill(cls, `采集器 ${st.collector.stale ? '疑似停滞' : '运行中'} · 心跳 ${fmtAgo(st.collector.last_flush)}`));
    } else {
      pills.appendChild(pill('bad', '采集器 无心跳'));
    }
    if (st.db) pills.appendChild(pill('', `归档 ${st.db.writes.toLocaleString()} 行 · ${humanBytes(st.db.bytes)}`));
    if (st.usn && st.usn.last_backfill) pills.appendChild(pill('', `上次补录 ${fmtAgo(st.usn.last_backfill)}`));
  }
}
function pill(cls, text) {
  const p = el('span', 'pill' + (cls ? ' ' + cls : ''));
  p.appendChild(el('i'));
  p.appendChild(el('span', null, text));
  return p;
}

function renderCards(t) {
  const cards = $('cards');
  cards.textContent = '';
  const max = t.max_file || 0;
  const mk = (delay, label, value, fmt, sub, subTitle) => {
    const c = el('div', 'card stat');
    c.style.animationDelay = delay + 'ms';
    c.appendChild(el('div', 'stat-label', label));
    const v = el('div', 'stat-value', '0');
    c.appendChild(v);
    if (sub !== undefined && sub !== null) {
      const s = el('div', 'stat-sub', sub);
      if (subTitle) s.title = subTitle;
      c.appendChild(s);
    }
    animateCount(v, value, fmt);
    cards.appendChild(c);
  };
  mk(0, '总写入量', t.bytes, humanBytes, `共 ${t.ops.toLocaleString()} 次写入操作`);
  mk(80, '写入文件数', t.files, x => Math.round(x).toLocaleString(), S.from ? '当前筛选窗口内' : '达到阈值的不同文件');
  mk(160, '活跃进程数', t.procs, x => Math.round(x).toLocaleString(), '产生写入的进程');
  mk(240, '最大单文件', max, humanBytes, t.max_file_path || '', t.max_file_path || '');
}

function renderChart(hourly) {
  const chart = $('chart');
  chart.textContent = '';
  if (!hourly || hourly.length === 0) {
    chart.appendChild(el('div', 'note', '窗口内没有数据'));
    return;
  }
  let step = 1;
  for (const m of [1, 6, 24]) {
    step = m;
    if (Math.ceil(hourly.length / m) <= 60) break;
  }
  const bucketSec = step * 3600;
  const buckets = [];
  for (let i = 0; i < hourly.length; i += step) {
    const slice = hourly.slice(i, i + step);
    buckets.push([slice[0][0], slice.reduce((s, x) => s + x[1], 0)]);
  }
  const max = buckets.reduce((m, b) => Math.max(m, b[1]), 0);
  const fromNum = S.from ? Number(S.from) : null;
  buckets.forEach(([h, v], i) => {
    const col = el('div', 'chart-col');
    const bar = el('span', 'chart-bar');
    bar.style.height = (max > 0 ? Math.max(v > 0 ? 4 : 0, (v / max) * 100) : 0) + '%';
    bar.style.animationDelay = (i * 30) + 'ms';
    bar.title = `${fmtFull(h)} 起 ${step} 小时 — ${humanBytes(v)}`;
    if (fromNum !== null && fromNum >= h && fromNum < h + bucketSec) bar.classList.add('active');
    col.appendChild(bar);
    col.appendChild(el('span', 'chart-h', step === 1 ? fmtHM(h) : fmtMdHM(h)));
    col.style.cursor = 'pointer';
    col.addEventListener('click', () => applyPatch({ from: String(h), to: String(h + bucketSec), offset: 0 }, true));
    chart.appendChild(col);
  });
}

function renderBarRow(container, label, bytes, max, sub, delay, onClick) {
  const row = el('div', 'bar-row');
  row.style.animationDelay = delay + 'ms';
  const lab = el('span', 'bar-label', label);
  lab.title = label;
  row.appendChild(lab);
  const track = el('span', 'bar-track');
  const fill = el('span', 'bar-fill');
  fill.style.width = (max > 0 ? Math.max(2, (bytes / max) * 100) : 0) + '%';
  fill.style.animationDelay = delay + 'ms';
  track.appendChild(fill);
  row.appendChild(track);
  const val = el('span', 'bar-value', humanBytes(bytes));
  if (sub) val.appendChild(el('em', null, sub));
  row.appendChild(val);
  row.addEventListener('click', onClick);
  container.appendChild(row);
}

function renderRankings(ov) {
  const procsBox = $('procs'), dirsBox = $('dirs');
  procsBox.textContent = '';
  dirsBox.textContent = '';
  const showProcs = S.view === 'writes';
  $('procs-card').hidden = !showProcs;
  const procs = showProcs ? (ov.procs || []) : [];
  if (procs.length === 0) procsBox.appendChild(el('div', 'note', '窗口内没有数据'));
  const maxP = procs.reduce((m, p) => Math.max(m, p.bytes), 0);
  procs.forEach((p, i) => renderBarRow(procsBox, p.process, p.bytes, maxP, `${p.ops.toLocaleString()} 次 / ${p.files} 个文件`, i * 50,
    () => applyPatch({ process: S.process === p.process ? '' : p.process, offset: 0 }, true)));
  const dirs = ov.dirs || [];
  if (dirs.length === 0) dirsBox.appendChild(el('div', 'note', '窗口内没有数据'));
  const maxD = dirs.reduce((m, d) => Math.max(m, d.bytes), 0);
  dirs.forEach((d, i) => renderBarRow(dirsBox, d.dir, d.bytes, maxD, `${d.ops.toLocaleString()} 次写入`, i * 50,
    () => applyPatch({ q: d.dir, offset: 0 }, true)));
}

function numCell(v, w, suffix) {
  const td = el('td', 'num');
  if (w > 0) {
    const b = el('span', 'cellbar');
    b.style.setProperty('--w', Math.max(2, w) + '%');
    td.appendChild(b);
  }
  td.appendChild(el('span', null, (typeof v === 'number' ? Math.round(v).toLocaleString() : v) + (suffix || '')));
  return td;
}

function renderTable(res) {
  const thead = $('thead-row');
  const tbody = $('rows');
  thead.textContent = '';
  tbody.textContent = '';
  const cols = colsFor(S.view, S.group);
  cols.forEach(c => {
    const th = el('th', c.label === '#' ? 'rank' : '', c.label);
    if (c.sort) {
      th.classList.add('sortable');
      if (S.sort === c.sort) {
        th.classList.add('sorted');
        th.appendChild(el('span', 'arrow', S.order === 'desc' ? '▼' : '▲'));
      }
      th.addEventListener('click', () => applyPatch({
        sort: c.sort, order: S.sort === c.sort ? (S.order === 'desc' ? 'asc' : 'desc') : 'desc', offset: 0,
      }, true));
    }
    thead.appendChild(th);
  });
  const rows = res.rows || [];
  if (rows.length === 0 && res.page.total > 0 && res.page.offset >= res.page.total) {
    applyPatch({ offset: 0 }, false);
    return;
  }
  if (rows.length === 0) {
    const tr = el('tr');
    const td = el('td', 'empty');
    td.appendChild(el('span', null, '没有匹配的数据 · 换个时间窗、关键字或清空过滤试试'));
    if (S.q || S.process) {
      const b = el('button', 'btn sm', '清空搜索与进程过滤');
      b.style.marginLeft = '10px';
      b.addEventListener('click', () => applyPatch({ q: '', process: '', offset: 0 }, true));
      td.appendChild(b);
    }
    td.colSpan = cols.length;
    tr.appendChild(td);
    tbody.appendChild(tr);
  }
  const maxBytes = rows.reduce((m, r) => Math.max(m, r.bytes || 0), 0);
  const maxOps = rows.reduce((m, r) => Math.max(m, r.ops || 0), 0);
  rows.forEach((r, i) => {
    r._w = maxBytes > 0 ? (r.bytes / maxBytes) * 100 : 0;
    r._w2 = maxOps > 0 ? (r.ops / maxOps) * 100 : 0;
    r._rank = res.page.offset + i + 1;
    const tr = el('tr');
    tr.style.animationDelay = (i * 25) + 'ms';
    cols.forEach(c => tr.appendChild(c.render(r)));
    tr.classList.add('clickable');
    tr.addEventListener('click', () => drill(r));
    tbody.appendChild(tr);
  });
  $('table-title').textContent = VIEWS[S.view].label +
    (S.view === 'writes' ? '（' + ({ file: '按文件', dir: '按目录', none: '分钟明细' })[S.group] + '）'
      : S.view === 'usn' ? '（' + ({ file: '按文件', dir: '按目录' })[S.group] + '）' : '');
  const total = res.page.total;
  const shown = rows.length ? `${res.page.offset + 1}–${res.page.offset + rows.length}` : '0';
  $('table-count').textContent = `共 ${total.toLocaleString()} 条 · 第 ${shown} 条`;
  renderViewNote();
  renderPager(res.page);
}

function renderViewNote() {
  const note = $('view-note');
  note.textContent = '';
  if (S.view === 'usn') {
    note.appendChild(el('div', 'note', '说明：补录数据来自离线期间的 NTFS USN 日志——大小为文件当前大小、路径为当前路径、写入进程未知，与实时采集数据分别统计、不合并求和。'));
  } else if (S.view === 'writes' && S.group === 'none') {
    note.appendChild(el('div', 'note', '明细口径：同一文件同一分钟若跨两次落盘，会出现多行；「按文件」聚合的求和始终正确。'));
  } else if (S.view === 'writes' && S.group === 'dir') {
    note.appendChild(el('div', 'note', '目录聚合：按文件路径最后一个反斜杠前的目录归并（与报告口径一致）。'));
  }
}

function renderPager(page) {
  const pager = $('pager');
  pager.textContent = '';
  const pages = Math.max(1, Math.ceil(page.total / page.limit));
  const cur = Math.floor(page.offset / page.limit) + 1;
  if (page.total === 0) return;
  const mkBtn = (label, target, active, disabled) => {
    const b = el('button', 'pg' + (active ? ' active' : ''), label);
    if (disabled) b.disabled = true;
    else b.addEventListener('click', () => applyPatch({ offset: target }, true));
    return b;
  };
  pager.appendChild(mkBtn('«', 0, false, cur === 1));
  const list = [];
  for (let p = 1; p <= pages; p++) {
    if (p <= 2 || p > pages - 1 || Math.abs(p - cur) <= 1) list.push(p);
    else if (list[list.length - 1] !== '…') list.push('…');
  }
  list.forEach(p => {
    if (p === '…') pager.appendChild(el('span', 'pg-info', '…'));
    else pager.appendChild(mkBtn(String(p), (p - 1) * page.limit, p === cur, false));
  });
  pager.appendChild(mkBtn('»', Math.min((pages - 1) * page.limit, 500000 - page.limit), false, cur === pages));
  const jump = el('input', 'pg jump');
  jump.type = 'text';
  jump.value = String(cur);
  jump.title = '输入页码后回车';
  jump.addEventListener('keydown', ev => {
    if (ev.key !== 'Enter') return;
    const p = Math.min(pages, Math.max(1, Number(jump.value) || 1));
    applyPatch({ offset: (p - 1) * page.limit }, true);
  });
  pager.appendChild(jump);
  pager.appendChild(el('span', 'pg-info', `第 ${cur} / ${pages} 页`));
}

function drill(r) {
  if (S.view === 'alerts') return;
  if (S.view === 'usn') { applyPatch({ q: r.path, offset: 0 }, true); return; }
  if (S.group === 'file') applyPatch({ q: r.path, group: 'none', sort: 'ts', order: 'desc', offset: 0 }, true);
  else if (S.group === 'dir') applyPatch({ q: r.path, offset: 0 }, true);
  else applyPatch({ q: r.path, offset: 0 }, true);
}

// ---------- 加载流程 ----------
function mainParams() {
  const p = { limit: S.limit, offset: S.offset, sort: S.sort, order: S.order };
  if (S.from && S.to) { p.from = S.from; p.to = S.to; } else p.last = S.last;
  if (S.q) p.q = S.q;
  if (S.process && S.view !== 'usn') p.process = S.process;
  if (S.view === 'writes' || S.view === 'usn') p.group = S.group;
  return p;
}
// 导出走的是全窗口，不是屏幕上这 50 行：继承筛选与聚合粒度，丢掉分页和排序。
function exportParams() {
  const p = mainParams();
  delete p.limit;
  delete p.offset;
  delete p.sort;
  delete p.order;
  return p;
}
async function loadMain() {
  const gen = loadGen;
  loading(1);
  try {
    if (inflight) inflight.abort();
    inflight = new AbortController();
    const res = await api(VIEWS[S.view].api, mainParams(), inflight.signal);
    if (gen !== loadGen) return;
    setError(null);
    renderTable(res);
  } catch (e) {
    if (e && e.name === 'AbortError') return;
    if (gen !== loadGen) return;
    setError(e.message || String(e));
  } finally {
    loading(-1);
  }
}
async function loadTop() {
  const gen = loadGen;
  loading(1);
  try {
    const w = {};
    if (S.from && S.to) { w.from = S.from; w.to = S.to; } else w.last = S.last;
    if (S.q) w.q = S.q;
    let ov = null, st = null;
    if (S.view === 'writes') {
      [ov, st] = await Promise.all([api('/api/overview', w), api('/api/status', {})]);
    } else if (S.view === 'usn') {
      const res = await api('/api/usn', { ...w, group: 'dir', limit: 8, sort: 'bytes', order: 'desc' });
      ov = { dirs: (res.rows || []).map(r => ({ dir: r.path, bytes: r.bytes, ops: 0 })) };
      st = await api('/api/status', {}).catch(() => null);
    } else {
      ov = { dirs: [] };
      st = await api('/api/status', {}).catch(() => null);
    }
    if (gen !== loadGen) return;
    renderHeader();
    renderPills(st);
    const isWrites = S.view === 'writes';
    $('cards').hidden = !isWrites;
    $('chart-card').hidden = !isWrites;
    $('procs-card').hidden = !isWrites;
    $('dirs-card').hidden = S.view === 'alerts';
    if (isWrites) {
      renderCards(ov.totals);
      renderChart(ov.hourly);
    }
    renderRankings(ov);
  } catch (e) {
    if (e && e.name === 'AbortError') return;
    setError(e.message || String(e));
  } finally {
    loading(-1);
  }
}
async function loadProcs() {
  try {
    const p = {};
    if (S.from && S.to) { p.from = S.from; p.to = S.to; } else p.last = S.last;
    const res = await api('/api/processes', p);
    const sel = $('proc');
    const keep = S.process;
    sel.textContent = '';
    sel.appendChild(el('option', null, '全部进程'));
    (res.rows || []).forEach(r => sel.appendChild(el('option', null, r.process)));
    sel.value = keep;
    if (sel.selectedIndex === -1) sel.selectedIndex = 0;
  } catch (e) { /* 下拉失败不阻断主流程 */ }
}
function refreshAll() {
  loadGen++;
  loadTop();
  loadMain();
  loadProcs();
}

// ---------- 控件同步 ----------
function syncControls() {
  document.querySelectorAll('#ranges .btn[data-last]').forEach(b =>
    b.classList.toggle('active', !S.from && S.last === b.dataset.last));
  $('btn-custom').classList.toggle('active', !!(S.from && S.to));
  $('custom-range').hidden = !(S.from && S.to);
  if (S.from && S.to) {
    $('fromTs').value = toLocalInput(Number(S.from));
    $('toTs').value = toLocalInput(Number(S.to));
  }
  document.querySelectorAll('#tabs .tab').forEach(b =>
    b.classList.toggle('active', b.dataset.view === S.view));
  document.querySelectorAll('#groups .btn').forEach(b => {
    b.classList.toggle('active', b.dataset.group === S.group);
    b.disabled = !GROUPS[S.view].includes(b.dataset.group);
  });
  $('groups').hidden = S.view === 'alerts';
  if ($('q').value !== S.q) $('q').value = S.q;
  $('proc').disabled = S.view === 'usn';
  $('btn-refresh-proc').disabled = S.view === 'usn';
  if ($('proc').value !== S.process) $('proc').value = S.process;
  $('size').value = String(S.limit);
  $('page-footer').textContent = S.mock
    ? 'diskwatch mock 预览 · 数据为演示样例 · 正式版仅监听 127.0.0.1:8421 · 数据保留 30 天'
    : 'diskwatch · 仅监听 127.0.0.1 · 数据来源：ETW 实时采集 + NTFS USN 离线回填 · 保留 30 天 · 同机任意进程可读取本界面';
}
function toLocalInput(ts) {
  const d = new Date(ts * 1000);
  const s = `${d.getFullYear()}-${p2(d.getMonth() + 1)}-${p2(d.getDate())}T${p2(d.getHours())}:${p2(d.getMinutes())}:${p2(d.getSeconds())}`;
  return s;
}
function fromLocalInput(v) {
  if (!v) return null;
  const t = new Date(v).getTime();
  return Number.isNaN(t) ? null : Math.floor(t / 1000);
}

// ---------- 事件绑定 ----------
function wire() {
  $('ranges').addEventListener('click', ev => {
    const b = ev.target.closest('.btn[data-last]');
    if (b) applyPatch({ last: b.dataset.last, from: null, to: null, offset: 0 }, true);
  });
  $('btn-custom').addEventListener('click', () => {
    if (S.from && S.to) { applyPatch({ from: null, to: null, last: '1h', offset: 0 }, true); return; }
    const now = Math.floor(Date.now() / 1000);
    applyPatch({ from: String(now - 3600), to: String(now), offset: 0 }, true);
  });
  $('btn-apply').addEventListener('click', () => {
    const f = fromLocalInput($('fromTs').value), t = fromLocalInput($('toTs').value);
    if (f === null || t === null || f >= t) { setError('自定义时间区间无效，请检查起止时间'); return; }
    applyPatch({ from: String(f), to: String(t), offset: 0 }, true);
  });
  let qTimer = null;
  $('q').addEventListener('input', () => {
    clearTimeout(qTimer);
    qTimer = setTimeout(() => {
      const v = $('q').value.trim();
      const min = /[\u3040-\u30ff\u3400-\u9fff\uf900-\ufaff\uac00-\ud7af]/.test(v) ? 1 : 3;
      if (v.length > 0 && v.length < min) return;
      if (v !== S.q) applyPatch({ q: v, offset: 0 }, true);
    }, 300);
  });
  $('proc').addEventListener('change', () => applyPatch({ process: $('proc').value, offset: 0 }, true));
  $('btn-refresh-proc').addEventListener('click', () => loadProcs());
  // 响应是 CSV 不是 JSON，不能走 api()；先验内容类型再落地，避免把错误页存成 .csv。
  $('btn-export').addEventListener('click', async () => {
    if (S.mock) { setError('mock 预览页没有真实数据，无法导出'); return; }
    const qs = new URLSearchParams(exportParams()).toString();
    const url = '/api/export/' + S.view + (qs ? '?' + qs : '');
    $('btn-export').disabled = true;
    try {
      const res = await fetch(url);
      const ctype = res.headers.get('Content-Type') || '';
      if (!res.ok || ctype.indexOf('text/csv') < 0) {
        const j = await res.json().catch(() => null);
        throw new Error(j && j.error ? j.error.message : '导出失败 (' + res.status + ')');
      }
      const m = (res.headers.get('Content-Disposition') || '').match(/filename="?([^";]+)"?/);
      const href = URL.createObjectURL(await res.blob());
      const a = document.createElement('a');
      a.href = href;
      a.download = m ? m[1] : 'diskwatch-' + S.view + '.csv';
      document.body.appendChild(a);
      a.click();
      a.remove();
      setTimeout(() => URL.revokeObjectURL(href), 60000);
      setError(null);
    } catch (e) {
      setError(e.message || String(e));
    } finally {
      $('btn-export').disabled = false;
    }
  });
  $('tabs').addEventListener('click', ev => {
    const b = ev.target.closest('.tab');
    if (b) applyPatch({ view: b.dataset.view, offset: 0 }, true);
  });
  $('groups').addEventListener('click', ev => {
    const b = ev.target.closest('.btn[data-group]');
    if (b && !b.disabled) applyPatch({ group: b.dataset.group, offset: 0 }, true);
  });
  $('size').addEventListener('change', () => applyPatch({ limit: Number($('size').value), offset: 0 }, true));
  window.addEventListener('hashchange', () => { S = parseHash(); syncControls(); refreshAll(); });
  window.addEventListener('popstate', () => { S = parseHash(); syncControls(); refreshAll(); });
  setInterval(() => {
    if (document.hidden) return;
    loadTop();
    if ($('autorefresh').checked && S.view === 'writes') loadMain();
  }, 30000);
}

wire();
syncControls();
refreshAll();

})();
