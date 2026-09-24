// ufp 调度台：原生 JS，无构建步骤，随二进制发布。
//
// 结构：共享状态（池子 / 健康）→ 机架与页签 → 每页一个 render 函数。
// 所有写操作之后都会重新拉取池子与健康，让左边的状态灯立刻反映现实。
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
  probes: {}, // entry_id -> 最近一次测试结果
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

function toast(msg, bad = false) {
  const el = document.createElement('div');
  el.className = bad ? 'bad' : '';
  el.textContent = msg;
  $('#toasts').append(el);
  setTimeout(() => el.remove(), 4200);
}

function dlg(html) { $('#dlg-body').innerHTML = html; $('#dlg').showModal(); }
const closeDlg = () => $('#dlg').close();
// 加载期绑定：节点缺失只警告，不让整页脚本挂掉（历史上就因此白过一次）
const on = (sel, ev, fn) => {
  const el = $(sel);
  if (!el) { console.warn('缺少节点', sel); return; }
  el.addEventListener(ev, fn);
};

// 状态灯：熔断 > 冷却 > 在线。skip 只用于尝试色带。
function stateOf(entryId) {
  const entry = S.pool.entries.find((e) => e.id === entryId);
  if (!entry) return 'skip';
  const ch = S.pool.channels.find((c) => c.id === entry.channel_id);
  const breaker = S.health.breakers.find((b) => b.channel_id === entry.channel_id && b.model === entry.upstream_model);
  if (breaker && breaker.state !== 'closed') return 'fail';
  const keys = S.pool.keys.filter((k) => k.channel_id === entry.channel_id && k.enabled);
  if (!ch || !ch.enabled || keys.length === 0) return 'skip';
  const cooling = S.health.cooldowns.filter((c) => c.model === entry.upstream_model && keys.some((k) => k.id === c.key_id));
  if (cooling.length >= keys.length) return 'hold';
  return 'live';
}

// ── 登录 ────────────────────────────────────────────────────────────────
function gate(msg) {
  // 除了切显示，顺手把渲染出来的数据抹掉：会话没了，屏幕上就不该再留着池子的内容。
  S.pool = { channels: [], keys: [], entries: [] };
  S.health = { breakers: [], cooldowns: [] };
  S.probes = {};
  for (const sel of ['#rail', '#readout', '#pages']) {
    const el = $(sel);
    if (el) el.innerHTML = '';
  }
  // 清内容但保留 .on 归属：重新登录后 render() 会往当前这一页写，页面才是可见的
  $$('section').forEach((s) => { s.innerHTML = ''; s.classList.toggle('on', s.id === 'p-' + S.page); });
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
    await api('/admin/api/login', { method: 'POST', body: JSON.stringify({ password: $('#pw').value }) });
    $('#pw').value = '';
    await boot();
  } catch (e) { $('#gate-msg').textContent = e.message; }
});
on('#pw', 'keydown', (e) => { if (e.key === 'Enter') $('#do-login').click(); });
on('#logout', 'click', async () => {
  await api('/admin/api/logout', { method: 'POST' });
  gate('已退出。');
});
on('#refresh', 'click', () => reload());

// ── 共享数据与外壳 ──────────────────────────────────────────────────────
async function reload() {
  const [pool, health] = await Promise.all([
    api('/admin/api/channels'),
    api('/admin/api/health'),
  ]);
  S.pool = pool;
  S.health = health;
  renderRail();
  renderReadout();
  await render();
}

function renderPages() {
  const nav = $('#pages');
  nav.innerHTML = PAGES.map(([id, label]) =>
    `<button data-page="${id}" ${id === S.page ? 'aria-current="page"' : ''}>${label}</button>`).join('');
  nav.onclick = (e) => {
    const b = e.target.closest('button[data-page]');
    if (!b) return;
    S.page = b.dataset.page;
    renderPages();
    $$('section').forEach((s) => s.classList.toggle('on', s.id === 'p-' + S.page));
    render();
  };
}

const PAGE_FN = {
  overview: pOverview, stats: pStats, requests: pRequests, pool: pPool, health: pHealth,
  downstream: pDownstream, search: pSearch, rules: pRules, settings: pSettings,
};

async function render() {
  renderPages();
  const host = $('#p-' + S.page);
  try {
    await PAGE_FN[S.page](host);
  } catch (e) {
    if (e.message !== 'unauthorized') {
      host.innerHTML = `<div class="sec"><p class="note">加载失败：${esc(e.message)}</p></div>`;
    }
  }
}

// 左侧机架：按层列出条目，每行一盏灯 + 冷却提示
function renderRail() {
  const rail = $('#rail');
  const tiers = [...new Set(S.pool.entries.map((e) => e.tier))].sort((a, b) => a - b);
  if (tiers.length === 0) {
    rail.innerHTML = `<h2>池子</h2><p class="note">还没有条目。先在「渠道与条目」加一个渠道与模型。</p>`;
    return;
  }
  rail.innerHTML = `<h2>池子 · ${S.pool.entries.length} 个条目</h2>` + tiers.map((tier) => {
    const list = S.pool.entries.filter((e) => e.tier === tier);
    return `<div class="tier"><div class="tier-head">第 ${tier} 层</div>` + list.map((e) => {
      const st = stateOf(e.id);
      const ch = S.pool.channels.find((c) => c.id === e.channel_id);
      const keys = S.pool.keys.filter((k) => k.channel_id === e.channel_id && k.enabled);
      const cooling = S.health.cooldowns.filter((c) => c.model === e.upstream_model);
      const probe = S.probes[e.id];
      const sub = st === 'fail' ? '熔断中，等半开探测'
        : cooling.length ? `${cooling.length}/${keys.length} 把 key 冷却 · ${remain(cooling[0].until_ms)}`
        : `${esc(ch ? ch.name : '渠道已删')} · ${keys.length} 把 key`;
      return `<div class="slot" title="${esc(e.upstream_model)}">
        <span class="lamp ${st}"></span>
        <span class="name">${esc(e.upstream_model)}</span>
        <button class="ghost tiny" data-probe="${e.id}" title="发一次最小请求测连通性">测</button>
        <span class="sub">${probe ? probe : sub}</span>
      </div>`;
    }).join('') + `</div>`;
  }).join('');
  rail.onclick = (e) => {
    const b = e.target.closest('button[data-probe]');
    if (b) { probeEntry(Number(b.dataset.probe)); return; }
    S.page = 'pool';
    $$('section').forEach((s) => s.classList.toggle('on', s.id === 'p-pool'));
    render();
  };
}

// 顶部读数带：一行数字，不是卡片
function renderReadout() {
  const cooling = S.health.cooldowns.length;
  const open = S.health.breakers.filter((b) => b.state !== 'closed').length;
  $('#readout').innerHTML = [
    `<span><b>${S.pool.entries.length}</b>条目</span>`,
    `<span><b>${S.pool.keys.filter((k) => k.enabled).length}</b>上游 key</span>`,
    `<span style="color:${cooling ? 'var(--hold)' : 'inherit'}"><b style="color:inherit">${cooling}</b>冷却</span>`,
    `<span style="color:${open ? 'var(--fail)' : 'inherit'}"><b style="color:inherit">${open}</b>熔断</span>`,
  ].join('');
}

// ── 测试上游连通性 ──────────────────────────────────────────────────────
async function probeEntry(entryId, host) {
  const mark = (text, cls) => {
    S.probes[entryId] = text;
    if (host) host.innerHTML = `<span class="probe ${cls}">${esc(text)}</span>`;
    const railSub = $(`#rail button[data-probe="${entryId}"]`);
    if (railSub) railSub.parentElement.querySelector('.sub').textContent = text;
  };
  mark('测试中…', 'run');
  try {
    const r = await api('/admin/api/test_connection', {
      method: 'POST', headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ entry_id: entryId }),
    });
    if (r.ok) mark(r.reply ? `${r.latency_ms}ms · ${r.reply.slice(0, 40)}` : `${r.latency_ms}ms · ${r.status} · 空回话`, 'ok');
    else mark(`${r.status ?? r.error_type ?? '失败'} · ${(r.error || '').slice(0, 80)}`, 'bad');
  } catch (e) { mark('测试失败：' + e.message, 'bad'); }
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
  host.onclick = (e) => {
    const b = e.target.closest('button[data-go]');
    if (b) { S.page = b.dataset.go; render(); }
  };
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
  host.onclick = (e) => {
    const b = e.target.closest('button[data-days]');
    if (b) { S.days = Number(b.dataset.days); pStats(host); }
  };
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
  $('#oe').onchange = (e) => { S.onlyErrors = e.target.checked; pRequests(host); };
  $$('tr[data-req]', host).forEach((tr) => {
    tr.onclick = async () => {
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
    };
  });
}

// ── 渠道与条目 ──────────────────────────────────────────────────────────
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
      return `<div class="sec">
        <h2>${esc(c.name)} <span class="tag">${esc(c.protocol)}</span>
          ${c.enabled ? '' : '<span class="tag fail">已停用</span>'}</h2>
        <p class="note">${esc(c.base_url)}${c.notes ? ' — ' + esc(c.notes) : ''}</p>
        <div class="row">
          <button class="tiny" data-newkey="${c.id}">加 key</button>
          <button class="tiny" data-newentry="${c.id}">加条目</button>
          <button class="tiny" data-editchan="${c.id}">改渠道</button>
          <button class="tiny" data-testchan="${c.id}">测试该渠道全部条目</button>
          <button class="tiny danger" data-delchan="${c.id}">删除渠道</button>
        </div>
        <table><thead><tr><th>key</th><th>密钥</th><th>状态</th><th>说明</th><th></th></tr></thead><tbody>
        ${keys.map((k) => `<tr>
          <td data-k="key">${esc(k.label) || '（未命名）'}</td>
          <td data-k="密钥" class="mono">${esc(k.api_key_masked)}</td>
          <td data-k="状态">${k.status === 'disabled'
            ? '<span class="tag fail">被上游拒绝</span>'
            : (k.enabled ? '<span class="tag live">启用</span>' : '<span class="tag">停用</span>')}</td>
          <td data-k="说明">${esc(k.status_reason || '')}</td>
          <td><button class="tiny ghost" data-editkey="${k.id}">改</button>
            ${k.status === 'disabled' ? `<button class="tiny" data-enablekey="${k.id}">恢复</button>` : ''}
            <button class="tiny danger" data-delkey="${k.id}">删</button></td></tr>`).join('')
          || '<tr><td class="empty" colspan="5">还没有 key，这个渠道不会被选中</td></tr>'}
        </tbody></table>
        <table style="margin-top:10px"><thead><tr><th>上游模型</th><th class="num">层级</th>
          <th class="num">上下文</th><th>图片/PDF</th><th>状态</th><th>连通性</th><th></th></tr></thead><tbody>
        ${entries.map((e) => `<tr>
          <td data-k="上游模型">${esc(e.upstream_model)}</td>
          <td class="num" data-k="层级">${e.tier}</td>
          <td class="num" data-k="上下文">${int(e.max_context)}</td>
          <td data-k="图片/PDF">${e.vision ? '图片' : '纯文本'}${e.pdf ? ' + PDF' : ''}</td>
          <td data-k="状态">${e.enabled ? '<span class="tag live">启用</span>' : '<span class="tag">停用</span>'}</td>
          <td data-k="连通性"><span class="probe ${S.probes[e.id] && !S.probes[e.id].includes('ms') ? 'bad' : ''}">${esc(S.probes[e.id] || '')}</span></td>
          <td><button class="tiny" data-probe="${e.id}">测</button>
            <button class="tiny danger" data-delentry="${e.id}">删</button></td></tr>`).join('')
          || '<tr><td class="empty" colspan="7">还没有条目，池子是空的</td></tr>'}
        </tbody></table>
      </div>`;
    }).join('') || '<div class="sec"><p class="note">还没有渠道。点上面的「新增渠道」开始。</p></div>'}`;

  host.onclick = async (e) => {
    const g = (attr) => e.target.closest(`button[data-${attr}]`);
    if (g('newch')) return channelForm(null);
    if (g('go')) { S.page = g('go').dataset.go; return render(); }
    const cid = (attr) => Number(g(attr).dataset[attr]);
    if (g('newkey')) return keyForm(cid('newkey'));
    if (g('newentry')) return entryForm(cid('newentry'));
    if (g('editchan')) return channelForm(chans.find((c) => c.id === cid('editchan')));
    if (g('testchan')) {
      for (const en of S.pool.entries.filter((x) => x.channel_id === cid('testchan'))) await probeEntry(en.id);
      return;
    }
    if (g('delchan')) {
      if (!confirm('删除渠道会连它的 key、条目与冷却记录一起删掉，确定？')) return;
      await api('/admin/api/channels/' + cid('delchan'), { method: 'DELETE' });
      toast('渠道已删除'); return reload();
    }
    if (g('probe')) return probeEntry(cid('probe'));
    if (g('delkey')) {
      if (!confirm('删除这把上游 key？')) return;
      await api('/admin/api/keys/' + cid('delkey'), { method: 'DELETE' });
      toast('key 已删除'); return reload();
    }
    if (g('enablekey')) {
      await api(`/admin/api/keys/${cid('enablekey')}/enable`, { method: 'POST' });
      toast('已恢复。如果它真的失效，下一轮 401 会再把它停掉'); return reload();
    }
    if (g('editkey')) {
      const k = S.pool.keys.find((x) => x.id === cid('editkey'));
      const label = prompt('key 的备注：', k.label);
      if (label === null) return;
      const enabled = confirm('这个 key 要启用吗？（取消 = 停用）');
      await api('/admin/api/keys/' + k.id, { method: 'PATCH', body: JSON.stringify({ label, enabled }) });
      toast('已保存'); return reload();
    }
    if (g('delentry')) {
      if (!confirm('删除这个条目？')) return;
      await api('/admin/api/entries/' + cid('delentry'), { method: 'DELETE' });
      toast('条目已删除'); return reload();
    }
  };
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
    <label class="f"><span>附加请求头（JSON，可留空）</span>
      <textarea id="f-headers" style="min-height:70px">${esc(JSON.stringify(cur.extra_headers || {}, null, 2))}</textarea></label>
    <label class="f"><span>备注</span><input id="f-notes" value="${esc(cur.notes || '')}"></label>
    <label class="f"><span><input type="checkbox" id="f-enabled" ${cur.enabled ? 'checked' : ''} style="width:auto"> 启用</span></label>
    <div class="row"><button class="primary" id="f-save">保存</button>
      <button class="ghost" onclick="document.getElementById('dlg').close()">取消</button></div>`);
  $('#f-save').onclick = async () => {
    let extra_headers = {};
    try { extra_headers = JSON.parse($('#f-headers').value || '{}'); }
    catch { return toast('附加请求头不是合法 JSON', true); }
    const payload = {
      name: $('#f-name').value.trim(), protocol: $('#f-proto').value,
      base_url: $('#f-url').value.trim(), extra_headers,
      enabled: $('#f-enabled').checked, notes: $('#f-notes').value.trim(),
    };
    if (!payload.name || !payload.base_url) return toast('名字和 base_url 必填', true);
    try {
      if (c) await api('/admin/api/channels/' + c.id, { method: 'PATCH', body: JSON.stringify(payload) });
      else await api('/admin/api/channels', { method: 'POST', body: JSON.stringify(payload) });
      closeDlg(); toast('已保存，立刻生效'); reload();
    } catch (err) { toast(err.message, true); }
  };
}

function keyForm(channelId) {
  dlg(`<h2>加一把上游 key</h2>
    <label class="f"><span>备注</span><input id="f-label" placeholder="比如 免费号 1"></label>
    <label class="f"><span>api key</span><input id="f-key" placeholder="粘贴 key"></label>
    <p class="note">同一渠道可以放多把 key：额度按每把各算一份，被限流时只冷却那一把，不影响同渠道的其他 key。</p>
    <div class="row"><button class="primary" id="f-save">保存</button>
      <button class="ghost" onclick="document.getElementById('dlg').close()">取消</button></div>`);
  $('#f-save').onclick = async () => {
    try {
      await api(`/admin/api/channels/${channelId}/keys`, {
        method: 'POST',
        body: JSON.stringify({ label: $('#f-label').value.trim(), api_key: $('#f-key').value.trim(), enabled: true }),
      });
      closeDlg(); toast('已添加'); reload();
    } catch (e) { toast(e.message, true); }
  };
}

function entryForm(channelId) {
  dlg(`<h2>加一个条目</h2>
    <label class="f"><span>上游模型名</span><input id="f-model" placeholder="比如 gemini-2.5-flash"></label>
    <div class="grid2">
      <label class="f"><span>层级（越小越优先）</span><input id="f-tier" type="number" value="1"></label>
      <label class="f"><span>上下文窗口（token）</span><input id="f-ctx" type="number" value="200000"></label>
    </div>
    <p class="note">上下文窗口和模态会参与路由：放不下或没有对应能力的请求会自动跳过这个条目。</p>
    <label class="f"><span><input type="checkbox" id="f-vision" checked style="width:auto"> 支持图片</span></label>
    <label class="f"><span><input type="checkbox" id="f-pdf" style="width:auto"> 支持 PDF</span></label>
    <label class="f"><span>备注</span><input id="f-notes"></label>
    <div class="row"><button class="primary" id="f-save">保存</button>
      <button class="ghost" onclick="document.getElementById('dlg').close()">取消</button></div>`);
  $('#f-save').onclick = async () => {
    const payload = {
      upstream_model: $('#f-model').value.trim(),
      tier: Number($('#f-tier').value || 1),
      max_context: Number($('#f-ctx').value || 200000),
      vision: $('#f-vision').checked, pdf: $('#f-pdf').checked,
      enabled: true, notes: $('#f-notes').value.trim(),
    };
    if (!payload.upstream_model) return toast('模型名必填', true);
    try {
      await api(`/admin/api/channels/${channelId}/entries`, { method: 'POST', body: JSON.stringify(payload) });
      closeDlg(); toast('已添加'); reload();
    } catch (e) { toast(e.message, true); }
  };
}

// ── 健康 ────────────────────────────────────────────────────────────────
async function pHealth(host) {
  const { breakers, cooldowns } = S.health;
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
      <table><thead><tr><th class="num">key id</th><th>模型</th><th>原因</th><th>还剩</th><th></th></tr></thead><tbody>
      ${cooldowns.map((c) => `<tr>
        <td class="num" data-k="key id">${c.key_id}</td><td data-k="模型">${esc(c.model)}</td>
        <td data-k="原因">${esc(c.reason)}</td><td data-k="还剩">${remain(c.until_ms)}</td>
        <td><button class="tiny" data-resetkey="${c.key_id}" data-model="${esc(c.model)}">清除</button></td></tr>`).join('')
      || '<tr><td class="empty" colspan="5">没有 key 在冷却</td></tr>'}
      </tbody></table>
      <div class="row" style="margin-top:12px"><button data-resetall>全部重置</button></div>
    </div>`;
  host.onclick = async (e) => {
    const b = e.target.closest('button');
    if (!b) return;
    try {
      if (b.dataset.resetch) {
        await api('/admin/api/health/reset', { method: 'POST', body: JSON.stringify({ channel_id: Number(b.dataset.resetch), model: b.dataset.model, clear_cooldowns: true }) });
      } else if (b.dataset.resetkey) {
        await api('/admin/api/health/reset', { method: 'POST', body: JSON.stringify({ key_id: Number(b.dataset.resetkey), model: b.dataset.model }) });
      } else if (b.hasAttribute('data-resetall')) {
        await api('/admin/api/health/reset', { method: 'POST', body: JSON.stringify({ clear_cooldowns: true }) });
      } else return;
      toast('已重置'); reload();
    } catch (err) { toast(err.message, true); }
  };
}

// ── 下游 key ────────────────────────────────────────────────────────────
async function pDownstream(host) {
  const rows = await api('/admin/api/downstream_keys');
  host.innerHTML = `
    <div class="sec">
      <h2>下游 key</h2>
      <p class="note">客户端用它连网关。明文只在创建时显示一次，库里只存 sha256。</p>
      <div class="row"><button class="primary" id="newdown">新建下游 key</button></div>
      <table><thead><tr><th>名字</th><th>前缀</th><th>状态</th><th>创建</th><th>最近使用</th><th></th></tr></thead><tbody>
      ${rows.map((r) => `<tr>
        <td data-k="名字">${esc(r.name)}</td><td data-k="前缀" class="mono">${esc(r.key_prefix)}…</td>
        <td data-k="状态">${r.enabled ? '<span class="tag live">启用</span>' : '<span class="tag">停用</span>'}</td>
        <td data-k="创建">${stamp(r.created_ms)}</td><td data-k="最近使用">${stamp(r.last_used_ms)}</td>
        <td><button class="tiny" data-toggle="${r.id}" data-name="${esc(r.name)}" data-on="${r.enabled}">改</button>
          <button class="tiny danger" data-del="${r.id}">删</button></td></tr>`).join('')
      || '<tr><td class="empty" colspan="6">还没有下游 key，Claude Code 现在连不上</td></tr>'}
      </tbody></table>
    </div>`;
  $('#newdown').onclick = async () => {
    const name = prompt('给这把 key 起个名字（比如「我的笔记本」）：');
    if (!name) return;
    const out = await api('/admin/api/downstream_keys', { method: 'POST', body: JSON.stringify({ name, enabled: true }) });
    dlg(`<h2>已创建，请立刻复制</h2>
      <p class="note">只显示这一次。填进 Claude Code 的两个环境变量：</p>
      <label class="f"><span>key</span><input class="mono" value="${esc(out.key)}" readonly onclick="this.select()"></label>
      <label class="f"><span>环境变量</span><textarea class="mono" style="min-height:70px" readonly onclick="this.select()">export ANTHROPIC_BASE_URL=https://你的域名\nexport ANTHROPIC_AUTH_TOKEN=${esc(out.key)}</textarea></label>
      <button class="primary" onclick="document.getElementById('dlg').close()">我已保存</button>`);
    pDownstream(host);
  };
  host.onclick = async (e) => {
    const b = e.target.closest('button');
    if (!b) return;
    if (b.dataset.toggle) {
      const name = prompt('名字：', b.dataset.name);
      if (name === null) return;
      const enabled = confirm('启用吗？（取消 = 停用）');
      await api('/admin/api/downstream_keys/' + b.dataset.toggle, { method: 'PATCH', body: JSON.stringify({ name, enabled }) });
      toast('已保存');
    } else if (b.dataset.del) {
      if (!confirm('删除后这把 key 立刻失效，确定？')) return;
      await api('/admin/api/downstream_keys/' + b.dataset.del, { method: 'DELETE' });
      toast('已删除');
    } else return;
    pDownstream(host);
  };
}

// ── 搜索后端 ────────────────────────────────────────────────────────────
async function pSearch(host) {
  const rows = await api('/admin/api/search_backends');
  const kinds = ['tavily', 'exa', 'firecrawl', 'parallel', 'jina'];
  host.innerHTML = `
    <div class="sec">
      <h2>搜索后端</h2>
      <p class="note">网关自己执行网页搜索：上游模型调用搜索 → 网关查这些后端 → 结果以标准块回给 Claude Code。
        多个后端会依次尝试，被限流的那一个进入冷却。</p>
      <div class="row"><button class="primary" id="add">新增搜索后端</button></div>
      <table><thead><tr><th>名字</th><th>类型</th><th>密钥</th><th>base_url</th><th>状态</th><th>冷却</th><th></th></tr></thead><tbody>
      ${rows.map((r) => `<tr>
        <td data-k="名字">${esc(r.name)}</td><td data-k="类型">${esc(r.kind)}</td>
        <td data-k="密钥" class="mono">${esc(r.api_key_masked || '（无需密钥）')}</td>
        <td data-k="base_url" class="note" style="margin:0">${esc(r.base_url || '默认')}</td>
        <td data-k="状态">${r.enabled ? '<span class="tag live">启用</span>' : '<span class="tag">停用</span>'}</td>
        <td data-k="冷却">${r.cooldown_until_ms ? `<span class="tag hold">${remain(r.cooldown_until_ms)}</span>` : ''}</td>
        <td><button class="tiny" data-edit="${r.id}">改</button>
          <button class="tiny danger" data-del="${r.id}">删</button></td></tr>`).join('')
      || '<tr><td class="empty" colspan="7">还没有搜索后端 —— 不配的话 WebSearch 会用不了，其他功能不受影响</td></tr>'}
      </tbody></table>
    </div>`;
  const form = (row) => {
    const r = row || { name: '', kind: 'tavily', base_url: '', enabled: true, notes: '' };
    dlg(`<h2>${row ? '修改' : '新增'}搜索后端</h2>
      <div class="grid2">
        <label class="f"><span>名字</span><input id="s-name" value="${esc(r.name)}" placeholder="tavily-1"></label>
        <label class="f"><span>类型</span><select id="s-kind">${kinds.map((k) =>
          `<option ${k === r.kind ? 'selected' : ''}>${k}</option>`).join('')}</select></label>
      </div>
      <label class="f"><span>api key</span><input id="s-key" placeholder="${row ? '留空表示不改' : '粘贴 key'}"></label>
      <label class="f"><span>base_url（留空用默认）</span><input id="s-url" value="${esc(r.base_url || '')}"></label>
      <label class="f"><span>备注</span><input id="s-notes" value="${esc(r.notes || '')}"></label>
      <label class="f"><span><input type="checkbox" id="s-on" ${r.enabled ? 'checked' : ''} style="width:auto"> 启用</span></label>
      <div class="row"><button class="primary" id="s-save">保存</button>
        <button class="ghost" onclick="document.getElementById('dlg').close()">取消</button></div>`);
    $('#s-save').onclick = async () => {
      const payload = {
        name: $('#s-name').value.trim(), kind: $('#s-kind').value,
        api_key: $('#s-key').value.trim(), base_url: $('#s-url').value.trim(),
        enabled: $('#s-on').checked, notes: $('#s-notes').value.trim(),
      };
      if (!payload.name) return toast('名字必填', true);
      try {
        if (row) await api('/admin/api/search_backends/' + row.id, { method: 'PATCH', body: JSON.stringify(payload) });
        else await api('/admin/api/search_backends', { method: 'POST', body: JSON.stringify(payload) });
        closeDlg(); toast('已保存'); pSearch(host);
      } catch (e) { toast(e.message, true); }
    };
  };
  $('#add').onclick = () => form(null);
  host.onclick = async (e) => {
    const b = e.target.closest('button');
    if (!b) return;
    if (b.dataset.edit) form(rows.find((r) => r.id === Number(b.dataset.edit)));
    else if (b.dataset.del) {
      if (!confirm('删除这个搜索后端？')) return;
      await api('/admin/api/search_backends/' + b.dataset.del, { method: 'DELETE' });
      toast('已删除'); pSearch(host);
    }
  };
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
  host.onclick = async (e) => {
    const b = e.target.closest('button');
    if (!b) return;
    if (b.dataset.tog) {
      await api('/admin/api/rules/' + b.dataset.tog, { method: 'PATCH', body: JSON.stringify({ enabled: b.dataset.on !== 'true' }) });
    } else if (b.dataset.del) {
      if (!confirm('删除这条规则？')) return;
      await api('/admin/api/rules/' + b.dataset.del, { method: 'DELETE' });
    } else return;
    pRules(host);
  };
}

// ── 设置 ────────────────────────────────────────────────────────────────
async function pSettings(host) {
  const settings = await api('/admin/api/settings');
  host.innerHTML = `
    <div class="sec">
      <h2>运行期设置</h2>
      <p class="note">保存后立刻生效。常用项：publicModelId（对外模型名）、modelEcho（响应里回显哪个模型名）、
        maxAttempts（一次请求最多试几个条目）、firstContentTimeoutMs、breaker.*、search.*、analysisEntryId
        （「让另一个上游分析报错」用的条目）、alerts.smtp（邮件告警）。</p>
      <textarea id="set" spellcheck="false">${esc(JSON.stringify(settings, null, 2))}</textarea>
      <div class="row" style="margin-top:10px"><button class="primary" id="save">保存</button></div>
    </div>
    <div class="sec">
      <h2>在线部署令牌</h2>
      <p class="note">CI 往 <code>/admin/api/deploy</code> 推新版本时用，和登录密码是两套。
        ${settings.deployToken ? '当前已配置。' : '当前未配置，部署接口是关闭的。'}</p>
      <div class="row"><button id="rotate">生成 / 轮换令牌</button>
        <span class="note" style="margin:0">轮换后旧令牌立刻失效</span></div>
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
  $('#save').onclick = async () => {
    try {
      await api('/admin/api/settings', { method: 'PUT', body: $('#set').value });
      toast('已保存并生效'); reload();
    } catch (e) { toast('保存失败：' + e.message, true); }
  };
  $('#rotate').onclick = async () => {
    if (!confirm('生成新令牌会让旧的立刻失效，确定？')) return;
    const out = await api('/admin/api/deploy-token', { method: 'POST' });
    dlg(`<h2>新的部署令牌</h2>
      <p class="note">只显示这一次。填到仓库 Secret <code>DEPLOY_TOKEN</code>；也可以先复制到文件再
        <code>gh secret set DEPLOY_TOKEN &lt; 文件</code>，避免经过剪贴板和聊天记录。</p>
      <label class="f"><span>令牌</span><input class="mono" value="${esc(out.token)}" readonly onclick="this.select()"></label>
      <button class="primary" onclick="document.getElementById('dlg').close()">我已保存</button>`);
    pSettings(host);
  };
  $('#export').onclick = async () => {
    const data = await api('/admin/api/export');
    const a = document.createElement('a');
    a.href = URL.createObjectURL(new Blob([JSON.stringify(data, null, 2)], { type: 'application/json' }));
    a.download = 'ufp-config.json'; a.click();
  };
  $('#import').onclick = () => {
    const input = document.createElement('input');
    input.type = 'file'; input.accept = '.json';
    input.onchange = async () => {
      try {
        const out = await api('/admin/api/import', { method: 'POST', body: await input.files[0].text() });
        toast('导入完成：' + JSON.stringify(out.imported)); reload();
      } catch (e) { toast('导入失败：' + e.message, true); }
    };
    input.click();
  };
  $('#savepw').onclick = async () => {
    try {
      await api('/admin/api/password', { method: 'POST', body: JSON.stringify({ old_password: $('#old').value, new_password: $('#new').value }) });
      toast('密码已更新');
    } catch (e) { toast('修改失败：' + e.message, true); }
  };
}

// ── 启动 ────────────────────────────────────────────────────────────────
// 页面里的兜底脚本靠这个标记判断「我到底跑起来没有」
window.__ufpBooted = true;
boot();
setInterval(() => { if (!$('.shell').hidden) reload(); }, 30000);
