// ufp 调度台：原生 JS，无构建步骤，随二进制发布。
//
// 结构：共享状态（池子 / 健康）→ 机架与页签 → 每页一个 render 函数。
//
// 同步规则（这几条被违反过，后台就会「操作了，页面没变」）：
// - 切页只走 go()：它先刷新共享状态，再画机架、读数和当前页，三处用同一份数据；
// - 页面可见性只由 render() 决定，别处不许单独改 S.page；
// - 条目的健康只从 entryHealth() 算，机架的灯、机架的说明、渠道页都用它；
// - 写操作之后一律 reload()；点击处理一律包 guard()，失败要弹出来而不是静默。
'use strict';

const PAGES = [
  ['overview', '概览'],
  ['stats', '用量'],
  ['requests', '请求'],
  ['pool', '渠道与条目'],
  ['health', '健康'],
  ['downstream', '下游 key'],
  ['search', '搜索'],
  ['rules', '矫正规则'],
  ['settings', '设置'],
];

const S = {
  page: 'overview',
  pool: { channels: [], keys: [], entries: [] },
  health: { breakers: [], cooldowns: [] },
  days: 7,
  onlyErrors: false,
  probes: {}, // entry_id -> { text, cls, at }：最近一次连通性测试
  searchProbes: {}, // search_backend_id -> 同上
};

// ── 工具 ────────────────────────────────────────────────────────────────
const $ = (sel, root = document) => root.querySelector(sel);
const $$ = (sel, root = document) => [...root.querySelectorAll(sel)];
const esc = (v) => String(v ?? '').replace(/[&<>"']/g, (c) =>
  ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));
const int = (n) => (n ?? 0).toLocaleString('zh-CN');
const ms = (n) => (n == null ? '—' : n >= 1000 ? (n / 1000).toFixed(1) + 's' : n + 'ms');

function stamp(msv) {
  if (!msv) return '—';
  const d = new Date(msv), p = (n) => String(n).padStart(2, '0');
  return `${d.getMonth() + 1}/${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}`;
}

function remain(untilMs) {
  const left = untilMs - Date.now();
  if (left <= 0) return '已到期';
  if (left > 3600_000) return `${(left / 3600_000).toFixed(1)} 小时后`;
  return `${Math.ceil(left / 60000)} 分钟后`;
}

async function api(path, opts = {}) {
  const init = { credentials: 'same-origin', ...opts };
  // 带 body 就默认按 JSON 发。fetch 对字符串 body 默认发的 content-type 是 text/plain，
  // 服务端（axum 的 Json 提取器）会直接回 415 —— 所有写操作都会挂，包括登录。
  if (init.body != null) {
    const h = init.headers || {};
    const has = Object.keys(h).some((k) => k.toLowerCase() === 'content-type');
    if (!has) init.headers = { ...h, 'content-type': 'application/json' };
  }
  const res = await fetch(path, init);
  if (res.status === 401) { gate('登录已过期，重新输入密码。'); throw new Error('unauthorized'); }
  const text = await res.text();
  let data = null;
  try { data = text ? JSON.parse(text) : null; } catch { data = { raw: text }; }
  if (!res.ok) throw new Error((data && data.error && (data.error.message || data.error)) || res.statusText);
  return data;
}
const post = (path, body, method = 'POST') => api(path, { method, body: JSON.stringify(body ?? {}) });

function toast(msg, bad = false) {
  const el = document.createElement('div');
  el.className = bad ? 'bad' : '';
  el.textContent = msg;
  $('#toasts').append(el);
  setTimeout(() => el.remove(), 4200);
}

// 点击处理都包一层：失败要弹出来，而不是变成控制台里一条没人看的 unhandled rejection
const guard = (fn) => async (...args) => {
  try { await fn(...args); }
  catch (e) { if (e.message !== 'unauthorized') toast(e.message, true); }
};

function dlg(html) {
  const body = $('#dlg-body');
  body.onclick = null; // 上一个对话框挂的点击处理不能漏到这一个
  body.innerHTML = html;
  if (!$('#dlg').open) $('#dlg').showModal();
}
const closeDlg = () => $('#dlg').close();
const dlgButtons = (saveLabel = '保存') => `<div class="row"><button class="primary" id="f-save">${saveLabel}</button>
  <button class="ghost" type="button" onclick="document.getElementById('dlg').close()">取消</button></div>`;
// 对话框里的保存：成功才关，失败留着让人改（toast 说明原因）
const onSave = (fn) => { $('#f-save').onclick = guard(async () => { if ((await fn()) !== false) closeDlg(); }); };

// 加载期绑定：节点缺失只警告，不让整页脚本挂掉（历史上就因此白过一次）
const on = (sel, ev, fn) => {
  const el = $(sel);
  if (!el) { console.warn('缺少节点', sel); return; }
  el.addEventListener(ev, fn);
};

// ── 条目健康：机架的灯、机架的说明、渠道页都从这里取，彼此不会打架 ─────────
function entryHealth(e) {
  const ch = S.pool.channels.find((c) => c.id === e.channel_id);
  const keys = S.pool.keys.filter((k) => k.channel_id === e.channel_id && k.enabled);
  // 冷却按 key 记：只算「这个渠道的 key」的冷却，别的渠道有同名模型不相干
  const cooling = S.health.cooldowns.filter((c) => c.model === e.upstream_model && keys.some((k) => k.id === c.key_id));
  const breaker = S.health.breakers.find((b) => b.channel_id === e.channel_id && b.model === e.upstream_model);
  let st = 'live', line;
  if (breaker && breaker.state !== 'closed') {
    st = 'fail'; line = breaker.state === 'half_open' ? '熔断半开，等探测结果' : '熔断中，等半开探测';
  } else if (!e.enabled) { st = 'skip'; line = '条目已停用'; }
  else if (!ch) { st = 'skip'; line = '渠道已删'; }
  else if (!ch.enabled) { st = 'skip'; line = '渠道已停用'; }
  else if (!keys.length) { st = 'skip'; line = '没有可用的 key'; }
  else if (cooling.length) {
    const soonest = Math.min(...cooling.map((c) => c.until_ms));
    if (cooling.length >= keys.length) st = 'hold';
    line = `${cooling.length}/${keys.length} 把 key 冷却 · ${remain(soonest)}`;
  } else line = `${ch.name} · ${keys.length} 把 key`;
  return { st, line, ch, keys, cooling, breaker };
}

// ── 登录 ────────────────────────────────────────────────────────────────
function gate(msg) {
  // 除了切显示，顺手把渲染出来的数据抹掉：会话没了，屏幕上就不该再留着池子的内容。
  S.pool = { channels: [], keys: [], entries: [] };
  S.health = { breakers: [], cooldowns: [] };
  S.probes = {};
  S.searchProbes = {};
  for (const sel of ['#rail', '#readout', '#pages']) {
    const el = $(sel);
    if (el) el.innerHTML = '';
  }
  $$('section').forEach((s) => { s.innerHTML = ''; });
  $('.shell').hidden = true;
  $('#gate').hidden = false;
  $('#gate-msg').textContent = msg;
  $('#pw').focus();
}
async function boot() {
  try {
    await api('/admin/api/overview');
    $('#gate').hidden = true;
    $('.shell').hidden = false;
    await reload();
  } catch (e) { if (e.message !== 'unauthorized') gate(e.message); }
}
on('#do-login', 'click', async () => {
  try {
    await post('/admin/api/login', { password: $('#pw').value });
    $('#pw').value = '';
    await boot();
  } catch (e) { $('#gate-msg').textContent = e.message; }
});
on('#pw', 'keydown', (e) => { if (e.key === 'Enter') $('#do-login').click(); });
on('#logout', 'click', guard(async () => {
  await post('/admin/api/logout');
  gate('已退出。');
}));
on('#refresh', 'click', guard(() => reload()));

// ── 共享数据与外壳 ──────────────────────────────────────────────────────
async function refreshShared() {
  const [pool, health] = await Promise.all([api('/admin/api/channels'), api('/admin/api/health')]);
  S.pool = pool;
  S.health = health;
}

// 刷新共享状态并重画一切：写操作之后、切页时都走这里
async function reload() {
  await refreshShared();
  renderRail();
  renderReadout();
  await render();
}

// 切页的唯一入口
const go = (page) => { S.page = page; return reload(); };

function renderPages() {
  const nav = $('#pages');
  nav.innerHTML = PAGES.map(([id, label]) =>
    `<button data-page="${id}" ${id === S.page ? 'aria-current="page"' : ''}>${label}</button>`).join('');
  nav.onclick = guard(async (e) => {
    const b = e.target.closest('button[data-page]');
    if (b) await go(b.dataset.page);
  });
}

const PAGE_FN = {
  overview: pOverview, stats: pStats, requests: pRequests, pool: pPool, health: pHealth,
  downstream: pDownstream, search: pSearch, rules: pRules, settings: pSettings,
};

async function render() {
  renderPages();
  // 可见性只在这里定：页签高亮和显示的内容永远是同一页
  $$('section').forEach((s) => s.classList.toggle('on', s.id === 'p-' + S.page));
  const host = $('#p-' + S.page);
  try {
    await PAGE_FN[S.page](host);
  } catch (e) {
    if (e.message !== 'unauthorized') {
      host.innerHTML = `<div class="sec"><p class="note">加载失败：${esc(e.message)}</p></div>`;
    }
  }
}

// 页面里任何带 data-go 的按钮都是「去别的页」
document.addEventListener('click', guard(async (e) => {
  const b = e.target.closest('button[data-go]');
  if (b && b.closest('section, .rail')) await go(b.dataset.go);
}));

// 左侧机架：按层列出条目，每行一盏灯 + 状态说明 + 最近一次测试
function renderRail() {
  const rail = $('#rail');
  const tiers = [...new Set(S.pool.entries.map((e) => e.tier))].sort((a, b) => a - b);
  if (tiers.length === 0) {
    rail.innerHTML = `<h2>池子</h2><p class="note">还没有条目。先在「渠道与条目」加一个渠道与模型。</p>`;
    rail.onclick = null;
    return;
  }
  rail.innerHTML = `<h2>池子 · ${S.pool.entries.length} 个条目</h2>` + tiers.map((tier) => {
    const list = S.pool.entries.filter((e) => e.tier === tier);
    return `<div class="tier"><div class="tier-head">第 ${tier} 层</div>` + list.map((e) => {
      const h = entryHealth(e);
      return `<div class="slot" title="${esc(e.upstream_model)}">
        <span class="lamp ${h.st}"></span>
        <span class="name">${esc(e.upstream_model)}</span>
        <button class="ghost tiny" data-probe="${e.id}" title="发一次最小请求测连通性">测</button>
        <span class="sub">${esc(h.line)}</span>
        <span class="sub" data-probe-out="${e.id}">${probeHtml(S.probes[e.id])}</span>
      </div>`;
    }).join('') + `</div>`;
  }).join('');
  rail.onclick = guard(async (e) => {
    const b = e.target.closest('button[data-probe]');
    if (b) return probeEntry(Number(b.dataset.probe));
    if (e.target.closest('.slot')) await go('pool');
  });
}

// 顶部读数带：一行数字，不是卡片
function renderReadout() {
  const cooling = S.health.cooldowns.length;
  const open = S.health.breakers.filter((b) => b.state !== 'closed').length;
  const live = S.pool.entries.filter((e) => entryHealth(e).st === 'live').length;
  $('#readout').innerHTML = [
    `<span><b>${live}</b>/ ${S.pool.entries.length} 条目可用</span>`,
    `<span><b>${S.pool.keys.filter((k) => k.enabled).length}</b>上游 key</span>`,
    `<span style="color:${cooling ? 'var(--hold)' : 'inherit'}"><b style="color:inherit">${cooling}</b>冷却</span>`,
    `<span style="color:${open ? 'var(--fail)' : 'inherit'}"><b style="color:inherit">${open}</b>熔断</span>`,
  ].join('');
}

// ── 连通性测试（条目与搜索后端共用一套显示） ─────────────────────────────
// 结果按 id 存着，页面上每个显示它的位置都标 data-probe-out / data-sprobe-out，
// 测完一次把所有位置一起刷新——机架、渠道页、搜索页不会各说各话。
function probeHtml(p) {
  if (!p) return '';
  const age = Date.now() - p.at;
  const when = p.cls === 'run' ? '' : age < 60_000 ? '刚刚' : `${Math.round(age / 60_000)} 分钟前`;
  return `<span class="probe ${p.cls}" title="${esc(p.full || p.text)}">${esc(p.text)}${when ? ` <span class="when">· ${when}测</span>` : ''}</span>`;
}
function paintProbes(attr, id, p) {
  $$(`[${attr}="${id}"]`).forEach((el) => { el.innerHTML = probeHtml(p); });
}

async function probeEntry(entryId) {
  const mark = (text, cls, full) => {
    S.probes[entryId] = { text, cls, full, at: Date.now() };
    paintProbes('data-probe-out', entryId, S.probes[entryId]);
  };
  mark('测试中…', 'run');
  try {
    const r = await post('/admin/api/test_connection', { entry_id: entryId });
    if (r.ok) mark(r.reply ? `${r.latency_ms}ms · ${r.reply.slice(0, 40)}` : `${r.latency_ms}ms · ${r.status} · 空回话`, 'ok', r.reply);
    else mark(`${r.status ?? r.error_type ?? '失败'} · ${(r.hint || r.error || '').slice(0, 90)}`, 'bad', r.error);
  } catch (e) {
    if (e.message !== 'unauthorized') mark('测试失败：' + e.message, 'bad');
  }
}

async function probeSearch(body, id) {
  const key = id ?? 'form';
  const mark = (text, cls, full) => {
    S.searchProbes[key] = { text, cls, full, at: Date.now() };
    paintProbes('data-sprobe-out', key, S.searchProbes[key]);
  };
  mark('搜索中…', 'run');
  try {
    const r = await post('/admin/api/search_backends/test', body);
    if (!r.ok) return mark(`${ms(r.latency_ms)} · ${r.hint || r.error}`.slice(0, 120), 'bad', r.error);
    const first = r.items[0] ? ` · 「${r.items[0].title}」` : '';
    const titles = r.items.map((i) => `${i.title}\n${i.url}`).join('\n\n');
    if (!r.count) return mark(`${ms(r.latency_ms)} · 通了，但搜「${r.query}」没拿到结果`, 'hold', titles);
    mark(`${ms(r.latency_ms)} · ${r.count} 条${first}${r.cooling_until_ms ? ' · 冷却仍在，到期前不会被用' : ''}`.slice(0, 120), 'ok', titles);
  } catch (e) {
    if (e.message !== 'unauthorized') mark('测试失败：' + e.message, 'bad');
  }
}

// ── 概览 ────────────────────────────────────────────────────────────────
async function pOverview(host) {
  const [ov, pulse] = await Promise.all([api('/admin/api/overview'), api('/admin/api/pulse?limit=80')]);
  const t = ov.stats.today;
  const bars = pulse.slice().reverse().map((a) => {
    const cls = a.committed ? 'live'
      : a.error_type === 'rate_limit' || a.error_type === 'quota' || a.status === 429 ? 'hold'
      : a.error_type === 'context_too_long' || a.error_type === 'rectified' ? 'skip' : 'fail';
    const h = Math.max(14, Math.min(46, 14 + Math.log2(Math.max(2, a.ms)) * 4));
    return `<i class="${cls}" style="height:${h}px"
      title="${esc(stamp(a.at))} ${esc(a.channel)}/${esc(a.key)} ${esc(a.model)} · ${a.status ?? '—'} · ${ms(a.ms)}${a.error_type ? ' · ' + esc(a.error_type) : ''}"></i>`;
  }).join('');
  host.innerHTML = `
    <div class="sec">
      <h2>池子心跳</h2>
      <p class="note">最近 80 次上游尝试。高度是耗时，颜色是结果 —— 一整排琥珀色通常意味着一层 key 的额度同时见底。</p>
      <div class="pulse">${bars || '<span class="empty">还没有请求</span>'}</div>
      <div class="pulse-legend">
        <span class="k live">成功</span><span class="k hold">限流/额度</span>
        <span class="k fail">失败</span><span class="k skip">跳过/已矫正</span>
      </div>
    </div>
    <div class="sec">
      <h2>今天</h2>
      <p class="note">${int(t.requests)} 次请求${t.errors ? `（${int(t.errors)} 次以错误收场）` : ''}，
        烧掉 ${int(t.input_tokens)} 输入 / ${int(t.output_tokens)} 输出 token，
        网页搜索 ${int(t.search_requests)} 次，首内容平均 ${ms(t.avg_first_content_ms)}。</p>
      <div class="row">
        <button data-go="requests">看请求明细</button>
        <button data-go="stats">看用量趋势</button>
        <button data-go="health">看冷却与熔断</button>
      </div>
    </div>
    <div class="sec">
      <h2>运行状态</h2>
      <table><tbody>
        <tr><td data-k="版本">v${esc(ov.runtime.version)}</td><td class="num" data-k="已运行">${(ov.runtime.uptime_ms / 3600000).toFixed(1)} 小时</td></tr>
        <tr><td data-k="在途请求">${ov.runtime.inflight} / ${ov.runtime.max_inflight}</td>
            <td class="num" data-k="丢弃的用量记录">${int(ov.runtime.dropped_writes)}</td></tr>
        <tr><td data-k="下游 key">${ov.pool.downstream_keys}</td>
            <td class="num" data-k="搜索后端">${ov.pool.search_backends}</td></tr>
        <tr><td data-k="部署令牌">${ov.deploy.token_set ? '已配置' : '未配置（CI 无法部署）'}</td>
            <td class="num" data-k="部署脚本">${esc(ov.deploy.apply_script)}</td></tr>
      </tbody></table>
    </div>`;
}

// ── 用量 ────────────────────────────────────────────────────────────────
async function pStats(host) {
  const d = await api('/admin/api/stats?days=' + S.days);
  const max = Math.max(1, ...d.daily.map((r) => r.requests));
  const trend = d.daily.map((r) => {
    const ok = r.requests - r.errors;
    return `<div class="d" title="${esc(r.date)} · ${int(r.requests)} 次 · ${int(r.errors)} 错">
      ${r.errors ? `<i class="err" style="height:${Math.round((r.errors / max) * 100)}%"></i>` : ''}
      <i style="height:${Math.round((ok / max) * 100)}%"></i></div>`;
  }).join('');
  const x = d.daily.map((r) => `<span>${esc(r.date.slice(5))}</span>`).join('');
  const table = (title, rows, nameKey) => `<div class="sec"><h2>${title}</h2>
    <table><thead><tr><th>名称</th><th class="num">请求</th><th class="num">错误</th>
      <th class="num">输入</th><th class="num">输出</th></tr></thead><tbody>
      ${rows.map((r) => `<tr><td data-k="${title}">${esc(r[nameKey] || r.name || r.model || '—')}</td>
        <td class="num" data-k="请求">${int(r.requests)}</td>
        <td class="num" data-k="错误">${r.errors ? `<span class="tag fail">${int(r.errors)}</span>` : '0'}</td>
        <td class="num" data-k="输入 token">${int(r.input_tokens)}</td>
        <td class="num" data-k="输出 token">${int(r.output_tokens)}</td></tr>`).join('')
      || '<tr><td class="empty" colspan="5">这段时间没有数据</td></tr>'}
    </tbody></table></div>`;
  host.innerHTML = `
    <div class="sec">
      <h2>最近 ${d.days} 天</h2>
      <p class="note">每根竖条是一天：下半段是正常收场的请求，上半段（红）是出错的。</p>
      <div class="trend">${trend || '<span class="empty">没有数据</span>'}</div>
      <div class="trend-x">${x}</div>
      <div class="row" style="margin-top:10px">
        ${[1, 7, 30].map((n) => `<button class="tiny ${n === S.days ? 'primary' : ''}" data-days="${n}">${n} 天</button>`).join('')}
      </div>
    </div>
    ${table('按下游 key', d.by_key, 'name')}
    ${table('按渠道', d.by_channel, 'name')}
    ${table('按上游模型', d.by_model, 'model')}`;
  host.onclick = guard(async (e) => {
    const b = e.target.closest('button[data-days]');
    if (b) { S.days = Number(b.dataset.days); await pStats(host); }
  });
}

// ── 请求 ────────────────────────────────────────────────────────────────
async function pRequests(host) {
  const rows = await api('/admin/api/requests?limit=100' + (S.onlyErrors ? '&only_errors=true' : ''));
  host.innerHTML = `
    <div class="sec">
      <h2>最近请求</h2>
      <p class="note">点任意一行看这个请求换过哪些条目、每次是什么结果。</p>
      <div class="row"><label><input type="checkbox" id="oe" ${S.onlyErrors ? 'checked' : ''} style="width:auto"> 只看出错的</label></div>
      <table><thead><tr>
        <th>时间</th><th>下游 key</th><th>请求的模型</th><th>实际用的</th>
        <th>渠道 / key</th><th>结果</th><th class="num">token 入/出</th>
        <th class="num">搜索</th><th class="num">尝试</th><th class="num">首内容</th><th class="num">总耗时</th>
      </tr></thead><tbody>
      ${rows.map((r) => `<tr data-req="${esc(r.request_id)}" style="cursor:pointer">
        <td data-k="时间">${stamp(r.created_ms)}</td>
        <td data-k="下游 key">${esc(r.key_name)}</td>
        <td data-k="请求的模型" class="note" style="margin:0">${esc(r.requested_model)}</td>
        <td data-k="实际用的">${esc(r.upstream_model)}</td>
        <td data-k="渠道 / key">${esc(r.channel)}<span class="note"> / ${esc(r.upstream_key)}</span></td>
        <td data-k="结果">${r.status >= 400
          ? `<span class="tag fail">${r.status}</span>`
          : `<span class="tag live">${r.status}</span>`}${r.stop_reason ? ` ${esc(r.stop_reason)}` : ''}</td>
        <td class="num" data-k="token 入/出">${int(r.input_tokens)} / ${int(r.output_tokens)}</td>
        <td class="num" data-k="搜索">${r.search_requests || ''}</td>
        <td class="num" data-k="尝试">${r.attempts}</td>
        <td class="num" data-k="首内容">${ms(r.first_content_ms)}</td>
        <td class="num" data-k="总耗时">${ms(r.total_ms)}${r.error_type ? ` <span class="tag fail">${esc(r.error_type)}</span>` : ''}</td>
      </tr>`).join('') || '<tr><td class="empty" colspan="11">没有请求</td></tr>'}
      </tbody></table>
    </div>`;
  $('#oe').onchange = guard(async (e) => { S.onlyErrors = e.target.checked; await pRequests(host); });
  host.onclick = guard(async (e) => {
    const tr = e.target.closest('tr[data-req]');
    if (!tr) return;
    const list = await api('/admin/api/attempts?request_id=' + encodeURIComponent(tr.dataset.req));
    dlg(`<h2>这个请求换过的条目</h2>
      <table><thead><tr><th class="num">#</th><th>渠道</th><th>key</th><th>模型</th><th>协议</th>
      <th class="num">状态</th><th class="num">耗时</th><th>提交给了客户端</th><th>问题</th></tr></thead><tbody>
      ${list.map((a) => `<tr><td class="num" data-k="第几次">${a.attempt_no}</td><td data-k="渠道">${esc(a.channel)}</td>
        <td data-k="key">${esc(a.upstream_key)}</td><td data-k="模型">${esc(a.upstream_model)}</td><td data-k="协议">${esc(a.protocol)}</td>
        <td class="num" data-k="状态">${a.status >= 400 ? `<span class="tag fail">${a.status}</span>` : (a.status ?? '—')}</td>
        <td class="num" data-k="耗时">${ms(a.total_ms)}</td><td data-k="提交给了客户端">${a.committed ? '是' : ''}</td>
        <td data-k="问题">${a.error_type ? `<span class="tag fail">${esc(a.error_type)}</span>` : ''}
          <div class="note" style="margin:0">${esc(a.error_message || '')}</div></td></tr>`).join('')}
      </tbody></table>
      <div class="row" style="margin-top:14px"><button class="primary" onclick="document.getElementById('dlg').close()">关闭</button></div>`);
  });
}

// ── 渠道与条目 ──────────────────────────────────────────────────────────
function keyStatus(k) {
  // status=disabled 专指「上游拒绝过这把 key」（401/403 自动停用），和手动停用是两回事
  if (k.status === 'disabled') return `<span class="tag fail" title="${esc(k.status_reason)}">被上游拒绝</span>`;
  return k.enabled ? '<span class="tag live">启用</span>' : '<span class="tag">停用</span>';
}

async function pPool(host) {
  const chans = S.pool.channels;
  host.innerHTML = `
    <div class="sec">
      <h2>渠道与条目</h2>
      <p class="note">条目 = 渠道 × 上游模型。同一渠道下的所有 key 共享这些条目；层级数字越小越优先，
        只有整层不可用才会降级。每行右侧的「测」会拿真实配置发一次最小请求。</p>
      <div class="row"><button class="primary" data-newch>新增渠道</button>
        <button data-go="search">搜索后端</button></div>
    </div>
    ${chans.map((c) => {
      const keys = S.pool.keys.filter((k) => k.channel_id === c.id);
      const entries = S.pool.entries.filter((e) => e.channel_id === c.id);
      const hdrs = Object.keys(c.extra_headers || {});
      return `<div class="sec">
        <h2>${esc(c.name)} <span class="tag">${esc(c.protocol)}</span>
          ${c.enabled ? '' : '<span class="tag fail">已停用</span>'}</h2>
        <p class="note">${esc(c.base_url)}${c.notes ? ' — ' + esc(c.notes) : ''}${hdrs.length
          ? `<br>附加请求头：<span class="mono">${hdrs.map(esc).join('、')}</span>` : ''}</p>
        <div class="row">
          <button class="tiny" data-newkey="${c.id}">加 key</button>
          <button class="tiny" data-newentry="${c.id}">加条目</button>
          <button class="tiny" data-editchan="${c.id}">改渠道</button>
          <button class="tiny" data-togchan="${c.id}">${c.enabled ? '停用渠道' : '启用渠道'}</button>
          <button class="tiny" data-testchan="${c.id}">测试该渠道全部条目</button>
          <button class="tiny danger" data-delchan="${c.id}">删除渠道</button>
        </div>
        <table><thead><tr><th>key</th><th>密钥</th><th>状态</th><th>说明</th><th></th></tr></thead><tbody>
        ${keys.map((k) => `<tr>
          <td data-k="key">${esc(k.label) || '（未命名）'}</td>
          <td data-k="密钥" class="mono">${esc(k.api_key_masked)}</td>
          <td data-k="状态">${keyStatus(k)}</td>
          <td data-k="说明">${esc(k.status_reason || '')}${k.disabled_ms ? ` <span class="note">（${stamp(k.disabled_ms)}）</span>` : ''}</td>
          <td><button class="tiny ghost" data-editkey="${k.id}">改</button>
            ${k.status === 'disabled'
              ? `<button class="tiny" data-enablekey="${k.id}">恢复</button>`
              : `<button class="tiny" data-togkey="${k.id}">${k.enabled ? '停用' : '启用'}</button>`}
            <button class="tiny danger" data-delkey="${k.id}">删</button></td></tr>`).join('')
          || '<tr><td class="empty" colspan="5">还没有 key，这个渠道不会被选中</td></tr>'}
        </tbody></table>
        <table style="margin-top:10px"><thead><tr><th>上游模型</th><th class="num">层级</th>
          <th class="num">上下文</th><th>图片/PDF</th><th>状态</th><th>连通性</th><th></th></tr></thead><tbody>
        ${entries.map((e) => {
          const h = entryHealth(e);
          return `<tr>
          <td data-k="上游模型"><span class="lamp ${h.st}" style="display:inline-block;margin-right:7px"></span>${esc(e.upstream_model)}</td>
          <td class="num" data-k="层级">${e.tier}</td>
          <td class="num" data-k="上下文">${int(e.max_context)}</td>
          <td data-k="图片/PDF">${e.vision ? '图片' : '纯文本'}${e.pdf ? ' + PDF' : ''}</td>
          <td data-k="状态">${e.enabled ? `<span class="note" style="margin:0">${esc(h.line)}</span>` : '<span class="tag">停用</span>'}</td>
          <td data-k="连通性" data-probe-out="${e.id}">${probeHtml(S.probes[e.id])}</td>
          <td><button class="tiny" data-probe="${e.id}">测</button>
            <button class="tiny ghost" data-editentry="${e.id}">改</button>
            <button class="tiny" data-togentry="${e.id}">${e.enabled ? '停用' : '启用'}</button>
            <button class="tiny danger" data-delentry="${e.id}">删</button></td></tr>`;
        }).join('')
          || '<tr><td class="empty" colspan="7">还没有条目，池子是空的</td></tr>'}
        </tbody></table>
      </div>`;
    }).join('') || '<div class="sec"><p class="note">还没有渠道。点上面的「新增渠道」开始。</p></div>'}`;

  host.onclick = guard(async (e) => {
    const g = (attr) => e.target.closest(`button[data-${attr}]`);
    const cid = (attr) => Number(g(attr).dataset[attr]);
    const chan = (attr) => chans.find((c) => c.id === cid(attr));
    const key = (attr) => S.pool.keys.find((k) => k.id === cid(attr));
    const entry = (attr) => S.pool.entries.find((x) => x.id === cid(attr));
    if (g('newch')) return newChannel();
    if (g('newkey')) return keyForm(cid('newkey'), null);
    if (g('newentry')) return entryForm(cid('newentry'), null);
    if (g('editchan')) return channelForm(chan('editchan'));
    if (g('editkey')) return keyForm(key('editkey').channel_id, key('editkey'));
    if (g('editentry')) return entryForm(entry('editentry').channel_id, entry('editentry'));
    if (g('probe')) return probeEntry(cid('probe'));
    if (g('testchan')) {
      for (const en of S.pool.entries.filter((x) => x.channel_id === cid('testchan'))) await probeEntry(en.id);
      return;
    }
    if (g('togchan')) {
      const c = chan('togchan');
      await post('/admin/api/channels/' + c.id, { ...c, enabled: !c.enabled }, 'PATCH');
      toast(c.enabled ? '渠道已停用，它的条目不再被选中' : '渠道已启用');
    } else if (g('togkey')) {
      const k = key('togkey');
      await post('/admin/api/keys/' + k.id, { enabled: !k.enabled }, 'PATCH');
      toast(k.enabled ? 'key 已停用' : 'key 已启用');
    } else if (g('togentry')) {
      const en = entry('togentry');
      await post('/admin/api/entries/' + en.id, { ...en, enabled: !en.enabled }, 'PATCH');
      toast(en.enabled ? '条目已停用' : '条目已启用');
    } else if (g('enablekey')) {
      await post(`/admin/api/keys/${cid('enablekey')}/enable`);
      toast('已恢复。如果它真的失效，下一轮 401 会再把它停掉');
    } else if (g('delchan')) {
      if (!confirm('删除渠道会连它的 key、条目与冷却记录一起删掉，确定？')) return;
      await api('/admin/api/channels/' + cid('delchan'), { method: 'DELETE' });
      toast('渠道已删除');
    } else if (g('delkey')) {
      if (!confirm('删除这把上游 key？')) return;
      await api('/admin/api/keys/' + cid('delkey'), { method: 'DELETE' });
      toast('key 已删除');
    } else if (g('delentry')) {
      if (!confirm('删除这个条目？')) return;
      await api('/admin/api/entries/' + cid('delentry'), { method: 'DELETE' });
      toast('条目已删除');
    } else return;
    await reload();
  });
}

// ── 新增渠道：先选预设 ──────────────────────────────────────────────────
async function newChannel() {
  const presets = await api('/admin/api/presets');
  dlg(`<h2>新增渠道</h2>
    <p class="note">选预设就不用自己填地址、协议和请求头；拉下来的模型列表会标出免费的，以及 Claude Code 用不了的。</p>
    <div class="presets">
      ${presets.map((p) => `<button type="button" class="preset" data-preset="${esc(p.id)}">
        <b>${esc(p.name)}</b><span>${esc(p.summary)}</span></button>`).join('')}
      <button type="button" class="preset" data-preset=""><b>自定义</b><span>手动填协议、地址和请求头。</span></button>
    </div>`);
  $('#dlg-body').onclick = (e) => {
    const b = e.target.closest('button[data-preset]');
    if (!b) return;
    if (!b.dataset.preset) channelForm(null);
    else presetForm(presets.find((p) => p.id === b.dataset.preset));
  };
}

function presetForm(p) {
  const st = { models: [], picked: new Map(), freeOnly: false, filter: '' };
  dlg(`<h2>${esc(p.name)}</h2>
    <p class="note">${esc(p.summary)}<br>${p.client
      ? `请求头模仿 <b>${esc(p.client)}</b>（${p.headers.map(esc).join('、')}），建好后在「改渠道」里可以看、可以改。`
      : '不模仿任何客户端，用网关自己的请求头。'}${p.notes.map((n) => '<br>' + esc(n)).join('')}</p>
    <div class="grid2">
      <label class="f"><span>api key（<a href="${esc(p.key_url)}" target="_blank" rel="noopener">去申请</a>）</span>
        <input id="f-key" placeholder="已经接入过、只想补模型可以留空"></label>
      <label class="f"><span>key 备注</span><input id="f-label" placeholder="比如 主号"></label>
    </div>
    <div class="row">
      <button type="button" id="p-list">拉取模型列表</button>
      <label>层级 <input id="f-tier" class="narrow" type="number" value="1"></label>
      <span class="note" id="p-status" style="margin:0"></span>
    </div>
    <div id="p-tools" hidden>
      <div class="row">
        <input id="p-filter" placeholder="按名字筛">
        <label id="p-free-wrap" hidden><input type="checkbox" id="p-free" style="width:auto"> 只看免费</label>
      </div>
      <div class="models" id="p-models"></div>
    </div>
    ${dlgButtons('应用')}`);

  const badges = (m) => [
    m.free === true ? '<span class="tag live">免费</span>' : '',
    m.vision ? '<span class="tag">图片</span>' : '',
    m.pdf ? '<span class="tag">PDF</span>' : '',
  ].join(' ');
  const paint = () => {
    const f = st.filter.toLowerCase();
    const shown = st.models.filter((m) => (!st.freeOnly || m.free === true)
      && (!f || m.id.toLowerCase().includes(f) || m.name.toLowerCase().includes(f)));
    $('#p-models').innerHTML = shown.map((m) => `<label class="${m.selectable ? '' : 'off'}">
        <input type="checkbox" data-mid="${esc(m.id)}" ${st.picked.has(m.id) ? 'checked' : ''} ${m.selectable ? '' : 'disabled'}>
        <span class="mid">${esc(m.id)}</span>
        <span class="ctx">${m.context ? int(m.context) : '—'} ${badges(m)}</span>
        <span class="meta">${esc(m.blocked || m.note || (m.name !== m.id ? m.name : ''))}</span>
      </label>`).join('') || '<p class="empty">没有符合条件的模型</p>';
    st.shown = shown.length;
    count();
  };
  const count = () => {
    const usable = st.models.filter((m) => m.selectable).length;
    $('#p-status').textContent = `显示 ${st.shown} 个（共 ${usable} 个可用）· 已勾 ${st.picked.size} 个`;
  };

  $('#p-list').onclick = guard(async () => {
    $('#p-status').textContent = '拉取中…';
    const r = await post(`/admin/api/presets/${p.id}/models`, { api_key: $('#f-key').value.trim() });
    if (!r.ok) { $('#p-status').textContent = ''; return toast(r.error, true); }
    st.models = r.models;
    const anyFree = r.models.some((m) => m.free === true && m.selectable);
    $('#p-free-wrap').hidden = !anyFree;
    st.freeOnly = anyFree; // 有免费模型可选时，默认只看免费的——这个网关就是冲着免费额度来的
    $('#p-free').checked = anyFree;
    $('#p-tools').hidden = false;
    paint();
  });
  $('#p-filter').oninput = (e) => { st.filter = e.target.value.trim(); paint(); };
  $('#p-free').onchange = (e) => { st.freeOnly = e.target.checked; paint(); };
  $('#p-models').onchange = (e) => {
    const m = st.models.find((x) => x.id === e.target.dataset.mid);
    if (!m) return;
    if (e.target.checked) st.picked.set(m.id, m); else st.picked.delete(m.id);
    count(); // 只更新计数，不重画列表：重画会丢焦点、吞掉紧跟着的下一次点击
  };

  onSave(async () => {
    if (!st.picked.size) { toast('先拉取模型列表，勾选要接入的模型', true); return false; }
    const out = await post(`/admin/api/presets/${p.id}/apply`, {
      api_key: $('#f-key').value.trim(),
      key_label: $('#f-label').value.trim(),
      tier: Number($('#f-tier').value || 1),
      models: [...st.picked.values()].map((m) => ({ id: m.id, protocol: m.protocol, context: m.context, vision: m.vision, pdf: m.pdf })),
    });
    const created = out.channels.filter((c) => c.created).length;
    toast(`${p.name}：${created ? `新建 ${created} 个渠道，` : ''}加了 ${out.entries_added} 个条目`
      + `${out.keys_added ? `、${out.keys_added} 把 key` : ''}${out.entries_existing ? `（${out.entries_existing} 个已存在，跳过）` : ''}`
      + `${out.headers_added ? `；给已有渠道补了 ${out.headers_added} 个客户端请求头` : ''}`);
    await go('pool');
  });
}

function channelForm(c) {
  const cur = c || { name: '', protocol: 'openai_chat', base_url: '', extra_headers: {}, enabled: true, notes: '' };
  dlg(`<h2>${c ? '修改渠道' : '新增渠道'}</h2>
    <label class="f"><span>名字</span><input id="f-name" value="${esc(cur.name)}" placeholder="比如 gemini-free"></label>
    <div class="grid2">
      <label class="f"><span>协议</span><select id="f-proto">
        ${['openai_chat', 'openai_responses', 'gemini', 'anthropic'].map((p) =>
          `<option value="${p}" ${p === cur.protocol ? 'selected' : ''}>${p}</option>`).join('')}</select></label>
      <label class="f"><span>base_url（粘到 /v1 或完整端点都行）</span>
        <input id="f-url" value="${esc(cur.base_url)}" placeholder="https://api.example.com/v1"></label>
    </div>
    <label class="f"><span>附加请求头（JSON，可留空；值里的 {model} 发请求时换成条目的模型名）</span>
      <textarea id="f-headers" style="min-height:70px">${esc(JSON.stringify(cur.extra_headers || {}, null, 2))}</textarea></label>
    <label class="f"><span>备注</span><input id="f-notes" value="${esc(cur.notes || '')}"></label>
    <label class="f"><span><input type="checkbox" id="f-enabled" ${cur.enabled ? 'checked' : ''} style="width:auto"> 启用</span></label>
    ${dlgButtons()}`);
  onSave(async () => {
    let extra_headers = {};
    try { extra_headers = JSON.parse($('#f-headers').value || '{}'); }
    catch { toast('附加请求头不是合法 JSON', true); return false; }
    const payload = {
      name: $('#f-name').value.trim(), protocol: $('#f-proto').value,
      base_url: $('#f-url').value.trim(), extra_headers,
      enabled: $('#f-enabled').checked, notes: $('#f-notes').value.trim(),
    };
    if (!payload.name || !payload.base_url) { toast('名字和 base_url 必填', true); return false; }
    if (c) await post('/admin/api/channels/' + c.id, payload, 'PATCH');
    else await post('/admin/api/channels', payload);
    toast('已保存，立刻生效');
    await reload();
  });
}

function keyForm(channelId, k) {
  dlg(`<h2>${k ? '修改上游 key' : '加一把上游 key'}</h2>
    <label class="f"><span>备注</span><input id="f-label" value="${esc(k ? k.label : '')}" placeholder="比如 免费号 1"></label>
    <label class="f"><span>api key${k ? '（留空表示不改）' : ''}</span><input id="f-key" placeholder="${k ? esc(k.api_key_masked) : '粘贴 key'}"></label>
    <p class="note">同一渠道可以放多把 key：额度按每把各算一份，被限流时只冷却那一把，不影响同渠道的其他 key。</p>
    ${dlgButtons()}`);
  onSave(async () => {
    const label = $('#f-label').value.trim(), api_key = $('#f-key').value.trim();
    if (k) {
      await post('/admin/api/keys/' + k.id, { label, ...(api_key ? { api_key } : {}) }, 'PATCH');
    } else {
      if (!api_key) { toast('api key 必填', true); return false; }
      await post(`/admin/api/channels/${channelId}/keys`, { label, api_key, enabled: true });
    }
    toast(k ? '已保存' : '已添加');
    await reload();
  });
}

function entryForm(channelId, en) {
  const cur = en || { upstream_model: '', tier: 1, max_context: 200000, vision: true, pdf: false, enabled: true, notes: '' };
  dlg(`<h2>${en ? '修改条目' : '加一个条目'}</h2>
    <label class="f"><span>上游模型名</span><input id="f-model" value="${esc(cur.upstream_model)}" placeholder="比如 gemini-2.5-flash"></label>
    <div class="grid2">
      <label class="f"><span>层级（越小越优先）</span><input id="f-tier" type="number" value="${cur.tier}"></label>
      <label class="f"><span>上下文窗口（token）</span><input id="f-ctx" type="number" value="${cur.max_context}"></label>
    </div>
    <p class="note">上下文窗口和模态会参与路由：放不下或没有对应能力的请求会自动跳过这个条目。</p>
    <label class="f"><span><input type="checkbox" id="f-vision" ${cur.vision ? 'checked' : ''} style="width:auto"> 支持图片</span></label>
    <label class="f"><span><input type="checkbox" id="f-pdf" ${cur.pdf ? 'checked' : ''} style="width:auto"> 支持 PDF</span></label>
    <label class="f"><span><input type="checkbox" id="f-enabled" ${cur.enabled ? 'checked' : ''} style="width:auto"> 启用</span></label>
    <label class="f"><span>备注</span><input id="f-notes" value="${esc(cur.notes || '')}"></label>
    ${dlgButtons()}`);
  onSave(async () => {
    const payload = {
      upstream_model: $('#f-model').value.trim(),
      tier: Number($('#f-tier').value || 1),
      max_context: Number($('#f-ctx').value || 200000),
      vision: $('#f-vision').checked, pdf: $('#f-pdf').checked,
      enabled: $('#f-enabled').checked, notes: $('#f-notes').value.trim(),
    };
    if (!payload.upstream_model) { toast('模型名必填', true); return false; }
    if (en) await post('/admin/api/entries/' + en.id, payload, 'PATCH');
    else await post(`/admin/api/channels/${channelId}/entries`, payload);
    toast(en ? '已保存' : '已添加');
    await reload();
  });
}

// ── 健康 ────────────────────────────────────────────────────────────────
async function pHealth(host) {
  const { breakers, cooldowns } = S.health;
  const keyName = (id) => {
    const k = S.pool.keys.find((x) => x.id === id);
    if (!k) return `#${id}（已删）`;
    const c = S.pool.channels.find((x) => x.id === k.channel_id);
    return `${c ? c.name : '?'} / ${k.label || '#' + id}`;
  };
  host.innerHTML = `
    <div class="sec">
      <h2>熔断（渠道 × 模型）</h2>
      <p class="note">连续失败到阈值就打开，过一段时间放一次半开探测；探测成功才恢复。只有真故障会计入，
        400 类错误与上下文超长不算。</p>
      <table><thead><tr><th>渠道</th><th>模型</th><th>状态</th><th class="num">连续失败</th>
        <th class="num">样本</th><th>打开于</th><th></th></tr></thead><tbody>
      ${breakers.map((b) => `<tr>
        <td data-k="渠道">${esc(b.channel)}</td><td data-k="模型">${esc(b.model)}</td>
        <td data-k="状态">${b.state === 'closed' ? '<span class="tag live">正常</span>'
          : b.state === 'half_open' ? '<span class="tag hold">半开探测</span>' : '<span class="tag fail">已打开</span>'}</td>
        <td class="num" data-k="连续失败">${b.consecutive_failures}</td>
        <td class="num" data-k="样本">${b.failed}/${b.total}</td>
        <td data-k="打开于">${b.opened_ms ? stamp(b.opened_ms) : '—'}</td>
        <td><button class="tiny" data-resetch="${b.channel_id}" data-model="${esc(b.model)}">重置</button></td></tr>`).join('')
      || '<tr><td class="empty" colspan="7">没有失败记录，池子很健康</td></tr>'}
      </tbody></table>
    </div>
    <div class="sec">
      <h2>冷却（key × 模型）</h2>
      <p class="note">额度类错误（429、余额不足、日配额用尽）只冷却那一把 key 的这个模型，
        时长来自上游给的重试信号；日配额会一直冷到配额重置。</p>
      <table><thead><tr><th>渠道 / key</th><th>模型</th><th>原因</th><th>还剩</th><th></th></tr></thead><tbody>
      ${cooldowns.map((c) => `<tr>
        <td data-k="渠道 / key">${esc(keyName(c.key_id))}</td><td data-k="模型">${esc(c.model)}</td>
        <td data-k="原因">${esc(c.reason)}</td><td data-k="还剩">${remain(c.until_ms)}</td>
        <td><button class="tiny" data-resetkey="${c.key_id}" data-model="${esc(c.model)}">清除</button></td></tr>`).join('')
      || '<tr><td class="empty" colspan="5">没有 key 在冷却</td></tr>'}
      </tbody></table>
      <div class="row" style="margin-top:12px"><button data-resetall>全部重置</button></div>
    </div>`;
  host.onclick = guard(async (e) => {
    const b = e.target.closest('button');
    if (!b) return;
    if (b.dataset.resetch) {
      await post('/admin/api/health/reset', { channel_id: Number(b.dataset.resetch), model: b.dataset.model, clear_cooldowns: true });
    } else if (b.dataset.resetkey) {
      await post('/admin/api/health/reset', { key_id: Number(b.dataset.resetkey), model: b.dataset.model });
    } else if (b.hasAttribute('data-resetall')) {
      await post('/admin/api/health/reset', { clear_cooldowns: true });
    } else return;
    toast('已重置');
    await reload();
  });
}

// ── 下游 key ────────────────────────────────────────────────────────────
async function pDownstream(host) {
  const rows = await api('/admin/api/downstream_keys');
  host.innerHTML = `
    <div class="sec">
      <h2>下游 key</h2>
      <p class="note">客户端用它连网关。明文只在创建时显示一次，库里只存 sha256。</p>
      <div class="row"><button class="primary" data-new>新建下游 key</button></div>
      <table><thead><tr><th>名字</th><th>前缀</th><th>状态</th><th>创建</th><th>最近使用</th><th></th></tr></thead><tbody>
      ${rows.map((r) => `<tr>
        <td data-k="名字">${esc(r.name)}</td><td data-k="前缀" class="mono">${esc(r.key_prefix)}…</td>
        <td data-k="状态">${r.enabled ? '<span class="tag live">启用</span>' : '<span class="tag">停用</span>'}</td>
        <td data-k="创建">${stamp(r.created_ms)}</td><td data-k="最近使用">${stamp(r.last_used_ms)}</td>
        <td><button class="tiny ghost" data-rename="${r.id}">改名</button>
          <button class="tiny" data-tog="${r.id}">${r.enabled ? '停用' : '启用'}</button>
          <button class="tiny danger" data-del="${r.id}">删</button></td></tr>`).join('')
      || '<tr><td class="empty" colspan="6">还没有下游 key，Claude Code 现在连不上</td></tr>'}
      </tbody></table>
    </div>`;
  const row = (b, attr) => rows.find((r) => r.id === Number(b.dataset[attr]));
  host.onclick = guard(async (e) => {
    const b = e.target.closest('button');
    if (!b) return;
    if (b.hasAttribute('data-new')) return newDownstreamKey();
    if (b.dataset.rename) {
      const r = row(b, 'rename');
      dlg(`<h2>改名</h2><label class="f"><span>名字</span><input id="f-name" value="${esc(r.name)}"></label>${dlgButtons()}`);
      return onSave(async () => {
        const name = $('#f-name').value.trim();
        if (!name) { toast('名字不能为空', true); return false; }
        await post('/admin/api/downstream_keys/' + r.id, { name, enabled: r.enabled }, 'PATCH');
        toast('已保存');
        await reload();
      });
    }
    if (b.dataset.tog) {
      const r = row(b, 'tog');
      await post('/admin/api/downstream_keys/' + r.id, { name: r.name, enabled: !r.enabled }, 'PATCH');
      toast(r.enabled ? '已停用，用它的客户端会立刻被拒' : '已启用');
    } else if (b.dataset.del) {
      if (!confirm('删除后这把 key 立刻失效，确定？')) return;
      await api('/admin/api/downstream_keys/' + b.dataset.del, { method: 'DELETE' });
      toast('已删除');
    } else return;
    await reload();
  });
}

function newDownstreamKey() {
  dlg(`<h2>新建下游 key</h2>
    <label class="f"><span>名字</span><input id="f-name" placeholder="比如「我的笔记本」"></label>
    ${dlgButtons('创建')}`);
  $('#f-name').focus();
  // 这里成功后不关对话框：换成「只显示一次」的明文
  $('#f-save').onclick = guard(async () => {
    const name = $('#f-name').value.trim();
    if (!name) return toast('名字不能为空', true);
    const out = await post('/admin/api/downstream_keys', { name, enabled: true });
    dlg(`<h2>已创建，请立刻复制</h2>
      <p class="note">只显示这一次。填进 Claude Code 的两个环境变量：</p>
      <label class="f"><span>key</span><input class="mono" value="${esc(out.key)}" readonly onclick="this.select()"></label>
      <label class="f"><span>环境变量</span><textarea class="mono" style="min-height:70px" readonly onclick="this.select()">export ANTHROPIC_BASE_URL=${esc(location.origin)}\nexport ANTHROPIC_AUTH_TOKEN=${esc(out.key)}</textarea></label>
      <button class="primary" onclick="document.getElementById('dlg').close()">我已保存</button>`);
    await reload();
  });
}

// ── 搜索后端 ────────────────────────────────────────────────────────────
const SEARCH_KINDS = ['tavily', 'exa', 'firecrawl', 'parallel', 'jina'];

async function pSearch(host) {
  const rows = await api('/admin/api/search_backends');
  host.innerHTML = `
    <div class="sec">
      <h2>搜索后端</h2>
      <p class="note">网关自己执行网页搜索：上游模型调用搜索 → 网关查这些后端 → 结果以标准块回给 Claude Code。
        多个后端会依次尝试，被限流的那一个进入冷却。「测」会拿真实配置搜一次，不影响冷却。</p>
      <div class="row"><button class="primary" data-add>新增搜索后端</button>
        ${rows.length > 1 ? '<button data-testall>全部测一遍</button>' : ''}</div>
      <table><thead><tr><th>名字</th><th>类型</th><th>密钥</th><th>base_url</th><th>状态</th><th>冷却</th><th>连通性</th><th></th></tr></thead><tbody>
      ${rows.map((r) => `<tr>
        <td data-k="名字">${esc(r.name)}</td><td data-k="类型">${esc(r.kind)}</td>
        <td data-k="密钥" class="mono">${esc(r.api_key_masked || '（无需密钥）')}</td>
        <td data-k="base_url" class="note" style="margin:0">${esc(r.base_url || '默认')}</td>
        <td data-k="状态">${r.enabled ? '<span class="tag live">启用</span>' : '<span class="tag">停用</span>'}</td>
        <td data-k="冷却">${r.cooldown_until_ms && r.cooldown_until_ms > Date.now() ? `<span class="tag hold">${remain(r.cooldown_until_ms)}</span>` : ''}</td>
        <td data-k="连通性" data-sprobe-out="${r.id}">${probeHtml(S.searchProbes[r.id])}</td>
        <td><button class="tiny" data-test="${r.id}">测</button>
          <button class="tiny ghost" data-edit="${r.id}">改</button>
          <button class="tiny" data-tog="${r.id}">${r.enabled ? '停用' : '启用'}</button>
          <button class="tiny danger" data-del="${r.id}">删</button></td></tr>`).join('')
      || '<tr><td class="empty" colspan="8">还没有搜索后端 —— 不配的话 WebSearch 会用不了，其他功能不受影响</td></tr>'}
      </tbody></table>
    </div>`;
  const row = (b, attr) => rows.find((r) => r.id === Number(b.dataset[attr]));
  host.onclick = guard(async (e) => {
    const b = e.target.closest('button');
    if (!b) return;
    if (b.hasAttribute('data-add')) return searchForm(null);
    if (b.dataset.edit) return searchForm(row(b, 'edit'));
    if (b.dataset.test) return probeSearch({ id: Number(b.dataset.test) }, Number(b.dataset.test));
    if (b.hasAttribute('data-testall')) {
      await Promise.all(rows.map((r) => probeSearch({ id: r.id }, r.id)));
      return;
    }
    if (b.dataset.tog) {
      const r = row(b, 'tog');
      await post('/admin/api/search_backends/' + r.id,
        { name: r.name, kind: r.kind, base_url: r.base_url || '', enabled: !r.enabled, notes: r.notes || '' }, 'PATCH');
      toast(r.enabled ? '已停用' : '已启用');
    } else if (b.dataset.del) {
      if (!confirm('删除这个搜索后端？')) return;
      await api('/admin/api/search_backends/' + b.dataset.del, { method: 'DELETE' });
      toast('已删除');
    } else return;
    await reload();
  });
}

function searchForm(row) {
  const r = row || { name: '', kind: 'tavily', base_url: '', enabled: true, notes: '' };
  dlg(`<h2>${row ? '修改' : '新增'}搜索后端</h2>
    <div class="grid2">
      <label class="f"><span>名字</span><input id="s-name" value="${esc(r.name)}" placeholder="tavily-1"></label>
      <label class="f"><span>类型</span><select id="s-kind">${SEARCH_KINDS.map((k) =>
        `<option ${k === r.kind ? 'selected' : ''}>${k}</option>`).join('')}</select></label>
    </div>
    <label class="f"><span>api key${row ? '（留空表示不改）' : ''}</span><input id="s-key" placeholder="${row ? esc(r.api_key_masked || '') : '粘贴 key'}"></label>
    <label class="f"><span>base_url（留空用默认）</span><input id="s-url" value="${esc(r.base_url || '')}"></label>
    <label class="f"><span>备注</span><input id="s-notes" value="${esc(r.notes || '')}"></label>
    <label class="f"><span><input type="checkbox" id="s-on" ${r.enabled ? 'checked' : ''} style="width:auto"> 启用</span></label>
    <div class="row"><button type="button" id="s-test">先测一下</button>
      <span data-sprobe-out="form">${probeHtml(S.searchProbes.form)}</span></div>
    ${dlgButtons()}`);
  const form = () => ({
    name: $('#s-name').value.trim(), kind: $('#s-kind').value,
    api_key: $('#s-key').value.trim(), base_url: $('#s-url').value.trim(),
    enabled: $('#s-on').checked, notes: $('#s-notes').value.trim(),
  });
  delete S.searchProbes.form;
  $('#s-test').onclick = guard(() => {
    const f = form();
    return probeSearch({ id: row ? row.id : undefined, kind: f.kind, api_key: f.api_key, base_url: f.base_url }, 'form');
  });
  onSave(async () => {
    const payload = form();
    if (!payload.name) { toast('名字必填', true); return false; }
    if (row) await post('/admin/api/search_backends/' + row.id, payload, 'PATCH');
    else await post('/admin/api/search_backends', payload);
    toast('已保存');
    await reload();
  });
}

// ── 矫正规则 ────────────────────────────────────────────────────────────
async function pRules(host) {
  const rows = await api('/admin/api/rules');
  host.innerHTML = `
    <div class="sec">
      <h2>矫正规则</h2>
      <p class="note">上游用 400 拒绝请求时，网关先试内置矫正器（思考签名、预算、max_tokens 下夹、图片降级）；
        仍不认识这个错误就把「错误 + 请求骨架（不含正文）」交给分析条目，只接受白名单内的改写补丁。
        这里是被采纳过的补丁，命中会自动套用。</p>
      <table><thead><tr><th>适用协议</th><th>错误特征</th><th class="num">命中</th><th>补丁</th><th>状态</th><th></th></tr></thead><tbody>
      ${rows.map((r) => `<tr>
        <td data-k="适用协议">${esc(r.scope) || '全局'}</td>
        <td data-k="错误特征"><span class="mono">${esc(r.error_fingerprint)}</span>
          <div class="note" style="margin:0">${esc(r.error_sample || '')}</div></td>
        <td class="num" data-k="命中">${r.hits}</td>
        <td data-k="补丁"><code>${esc(r.patch_json)}</code></td>
        <td data-k="状态">${r.enabled ? '<span class="tag live">启用</span>' : '<span class="tag">停用</span>'}</td>
        <td><button class="tiny" data-tog="${r.id}" data-on="${r.enabled}">${r.enabled ? '停用' : '启用'}</button>
          <button class="tiny danger" data-del="${r.id}">删</button></td></tr>`).join('')
      || '<tr><td class="empty" colspan="6">还没有规则。遇到没见过的 400 时会自动分析并生成。</td></tr>'}
      </tbody></table>
    </div>`;
  host.onclick = guard(async (e) => {
    const b = e.target.closest('button');
    if (!b) return;
    if (b.dataset.tog) {
      await post('/admin/api/rules/' + b.dataset.tog, { enabled: b.dataset.on !== 'true' }, 'PATCH');
    } else if (b.dataset.del) {
      if (!confirm('删除这条规则？')) return;
      await api('/admin/api/rules/' + b.dataset.del, { method: 'DELETE' });
    } else return;
    await reload();
  });
}

// ── 设置 ────────────────────────────────────────────────────────────────
async function pSettings(host) {
  const [settings, ov] = await Promise.all([api('/admin/api/settings'), api('/admin/api/overview')]);
  host.innerHTML = `
    <div class="sec">
      <h2>运行期设置</h2>
      <p class="note">保存后立刻生效。常用项：publicModelId（对外模型名）、modelEcho（响应里回显哪个模型名）、
        maxAttempts（一次请求最多试几个条目）、firstContentTimeoutMs、breaker.*、search.*、analysisEntryId
        （「让另一个上游分析报错」用的条目）、alerts.smtp（邮件告警）。部署令牌不在这里，见下一节。</p>
      <textarea id="set" spellcheck="false">${esc(JSON.stringify(settings, null, 2))}</textarea>
      <div class="row" style="margin-top:10px"><button class="primary" id="save">保存</button>
        <span class="note" id="set-dirty" style="margin:0"></span></div>
    </div>
    <div class="sec">
      <h2>在线部署令牌</h2>
      <p class="note">CI 往 <code>/admin/api/deploy</code> 推新版本时用，和登录密码是两套。
        ${ov.deploy.token_set ? '当前已配置。' : '当前未配置，部署接口是关闭的。'}</p>
      <div class="row"><button id="rotate">${ov.deploy.token_set ? '轮换令牌' : '生成令牌'}</button>
        <span class="note" style="margin:0">${ov.deploy.token_set ? '轮换后旧令牌立刻失效，记得同步更新仓库 Secret' : ''}</span></div>
    </div>
    <div class="sec">
      <h2>备份与迁移</h2>
      <p class="note">导出包含渠道、上游 key、条目、搜索后端、规则与设置；不含下游 key 明文（只保留哈希，导入后原密钥继续有效）。</p>
      <div class="row"><button id="export">导出配置</button><button id="import">导入配置</button></div>
    </div>
    <div class="sec">
      <h2>后台密码</h2>
      <div class="row">
        <input id="old" type="password" placeholder="当前密码">
        <input id="new" type="password" placeholder="新密码（至少 8 位）">
        <button class="primary" id="savepw">修改</button>
      </div>
    </div>`;
  const original = $('#set').value;
  $('#set').oninput = () => {
    $('#set-dirty').textContent = $('#set').value === original ? '' : '有未保存的改动';
  };
  $('#save').onclick = guard(async () => {
    let body;
    try { body = JSON.parse($('#set').value); } catch (e) { return toast('不是合法 JSON：' + e.message, true); }
    await post('/admin/api/settings', body, 'PUT');
    toast('已保存并生效');
    await reload();
  });
  $('#rotate').onclick = guard(async () => {
    if (ov.deploy.token_set && !confirm('生成新令牌会让旧的立刻失效，确定？')) return;
    const out = await post('/admin/api/deploy-token');
    await reload();
    dlg(`<h2>新的部署令牌</h2>
      <p class="note">只显示这一次。填到仓库 Secret <code>DEPLOY_TOKEN</code>；也可以先复制到文件再
        <code>gh secret set DEPLOY_TOKEN &lt; 文件</code>，避免经过剪贴板和聊天记录。</p>
      <label class="f"><span>令牌</span><input class="mono" value="${esc(out.token)}" readonly onclick="this.select()"></label>
      <button class="primary" onclick="document.getElementById('dlg').close()">我已保存</button>`);
  });
  $('#export').onclick = guard(async () => {
    const data = await api('/admin/api/export');
    const a = document.createElement('a');
    a.href = URL.createObjectURL(new Blob([JSON.stringify(data, null, 2)], { type: 'application/json' }));
    a.download = 'ufp-config.json'; a.click();
  });
  $('#import').onclick = () => {
    const input = document.createElement('input');
    input.type = 'file'; input.accept = '.json';
    input.onchange = guard(async () => {
      const out = await api('/admin/api/import', { method: 'POST', body: await input.files[0].text() });
      toast('导入完成：' + JSON.stringify(out.imported));
      await reload();
    });
    input.click();
  };
  $('#savepw').onclick = guard(async () => {
    await post('/admin/api/password', { old_password: $('#old').value, new_password: $('#new').value });
    $('#old').value = ''; $('#new').value = '';
    toast('密码已更新');
  });
}

// ── 启动 ────────────────────────────────────────────────────────────────
// 页面里的兜底脚本靠这个标记判断「我到底跑起来没有」
window.__ufpBooted = true;
boot();

// 定时刷新：共享状态（机架、读数）每次都更新；当前页只在「没人在操作」时重画——
// 打开的对话框、正在输入的框、设置页的编辑器都不能被冲掉。
function busy() {
  if ($('#dlg').open || S.page === 'settings') return true;
  const a = document.activeElement;
  return !!a && /^(INPUT|TEXTAREA|SELECT)$/.test(a.tagName) && !!a.closest('section.on');
}
async function tick() {
  if ($('.shell').hidden) return;
  try {
    await refreshShared();
    renderRail();
    renderReadout();
    if (!busy()) await render();
  } catch { /* 网络抖一下没关系，下一轮再来 */ }
}
setInterval(tick, 30000);
