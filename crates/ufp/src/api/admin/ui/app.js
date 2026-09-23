// ufp 管理后台前端：原生 JS，无构建步骤，随二进制一起发布。
'use strict';

const TABS = [
  ['overview', '概览'],
  ['stats', '统计'],
  ['requests', '请求日志'],
  ['pool', '渠道与条目'],
  ['health', '健康与冷却'],
  ['downstream', '下游 key'],
  ['search', '搜索后端'],
  ['rules', '矫正规则'],
  ['settings', '设置'],
];
let activeTab = 'overview';

// ---------- 基础工具 ----------

async function api(path, opts = {}) {
  const res = await fetch(path, {
    headers: { 'content-type': 'application/json' },
    credentials: 'same-origin',
    ...opts,
  });
  if (res.status === 401) {
    showLogin('登录已过期，请重新登录');
    throw new Error('unauthorized');
  }
  const text = await res.text();
  let data = null;
  try { data = text ? JSON.parse(text) : null; } catch { data = { raw: text }; }
  if (!res.ok) throw new Error((data && data.error && (data.error.message || data.error)) || res.statusText);
  return data;
}

const esc = (s) => String(s ?? '').replace(/[&<>"']/g, (c) =>
  ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));

const fmtInt = (n) => (n ?? 0).toLocaleString('zh-CN');
const fmtMs = (n) => (n == null ? '—' : n > 1000 ? (n / 1000).toFixed(1) + 's' : n + 'ms');

function fmtTime(ms) {
  if (!ms) return '—';
  const d = new Date(ms);
  const p = (n) => String(n).padStart(2, '0');
  return `${d.getMonth() + 1}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}`;
}

function fmtUntil(ms) {
  if (!ms) return '—';
  const left = ms - Date.now();
  if (left <= 0) return '已过期';
  if (left > 3600_000) return `${(left / 3600_000).toFixed(1)} 小时后`;
  return `${Math.ceil(left / 60000)} 分钟后`;
}

function toast(message, kind = 'ok') {
  const box = document.createElement('div');
  box.className = 'card ' + kind;
  box.style.cssText = 'position:fixed;right:18px;bottom:18px;z-index:50;margin:0;max-width:420px';
  box.textContent = message;
  document.body.appendChild(box);
  setTimeout(() => box.remove(), 4000);
}

function openDialog(html) {
  document.getElementById('dialog-body').innerHTML = html;
  document.getElementById('dialog').showModal();
}

// ---------- 登录 ----------

function showLogin(message = '') {
  document.getElementById('app').hidden = true;
  document.getElementById('login-card').hidden = false;
  document.getElementById('login-error').textContent = message;
  document.getElementById('password').focus();
}

async function checkAuth() {
  try {
    await api('/admin/api/overview');
    document.getElementById('login-card').hidden = true;
    document.getElementById('app').hidden = false;
    renderTabs();
    await refresh();
  } catch (e) {
    if (e.message !== 'unauthorized') showLogin(e.message);
  }
}

document.getElementById('do-login').onclick = async () => {
  const password = document.getElementById('password').value;
  try {
    await api('/admin/api/login', { method: 'POST', body: JSON.stringify({ password }) });
    document.getElementById('password').value = '';
    await checkAuth();
  } catch (e) {
    document.getElementById('login-error').textContent = e.message;
  }
};
document.getElementById('password').addEventListener('keydown', (e) => {
  if (e.key === 'Enter') document.getElementById('do-login').click();
});
document.getElementById('logout').onclick = async () => {
  await api('/admin/api/logout', { method: 'POST' });
  showLogin();
};
document.getElementById('refresh').onclick = () => refresh();

// ---------- 标签与渲染 ----------

function renderTabs() {
  const nav = document.getElementById('tabs');
  nav.innerHTML = TABS.map(([id, label]) =>
    `<button data-tab="${id}" class="${id === activeTab ? 'active' : ''}">${label}</button>`).join('');
  nav.onclick = (e) => {
    const btn = e.target.closest('button[data-tab]');
    if (!btn) return;
    activeTab = btn.dataset.tab;
    renderTabs();
    document.querySelectorAll('section').forEach((s) => s.classList.remove('active'));
    document.getElementById('tab-' + activeTab).classList.add('active');
    refresh();
  };
  document.querySelectorAll('section').forEach((s) =>
    s.classList.toggle('active', s.id === 'tab-' + activeTab));
}

async function refresh() {
  const target = document.getElementById('tab-' + activeTab);
  try {
    if (activeTab === 'overview') await renderOverview(target);
    if (activeTab === 'stats') await renderStats(target);
    if (activeTab === 'requests') await renderRequests(target);
    if (activeTab === 'pool') await renderPool(target);
    if (activeTab === 'health') await renderHealth(target);
    if (activeTab === 'downstream') await renderDownstream(target);
    if (activeTab === 'search') await renderSearch(target);
    if (activeTab === 'rules') await renderRules(target);
    if (activeTab === 'settings') await renderSettings(target);
  } catch (e) {
    if (e.message !== 'unauthorized') target.innerHTML = `<div class="card err">加载失败：${esc(e.message)}</div>`;
  }
}

// ---------- 概览 ----------

async function renderOverview(target) {
  const d = await api('/admin/api/overview');
  const t = d.stats.today, p = d.pool, r = d.runtime, h = d.health;
  document.getElementById('runtime').textContent =
    `v${r.version} · 运行 ${(r.uptime_ms / 3600000).toFixed(1)} 小时 · 在途 ${r.inflight}/${r.max_inflight}` +
    (r.dropped_writes ? ` · 丢弃写入 ${r.dropped_writes}` : '');
  const kpi = (label, value, cls = '') =>
    `<div class="kpi"><div class="v ${cls}">${value}</div><div class="l">${label}</div></div>`;
  target.innerHTML = `
    <div class="card"><h2>今天</h2><div class="grid">
      ${kpi('请求数', fmtInt(t.requests))}
      ${kpi('错误数', fmtInt(t.errors), t.errors > 0 ? 'err' : '')}
      ${kpi('输入 token', fmtInt(t.input_tokens))}
      ${kpi('输出 token', fmtInt(t.output_tokens))}
      ${kpi('缓存读取 token', fmtInt(t.cache_read_tokens))}
      ${kpi('网页搜索次数', fmtInt(t.search_requests))}
      ${kpi('平均首内容延迟', fmtMs(t.avg_first_content_ms))}
      ${kpi('近 1 小时错误', fmtInt(d.stats.recent_errors), d.stats.recent_errors ? 'err' : '')}
    </div></div>
    <div class="card"><h2>池与健康</h2><div class="grid">
      ${kpi('渠道', p.channels)}
      ${kpi('条目（渠道×模型）', p.entries)}
      ${kpi('上游 key', p.upstream_keys)}
      ${kpi('下游 key', p.downstream_keys)}
      ${kpi('搜索后端', p.search_backends)}
      ${kpi('熔断中', `${h.breakers_open}/${h.breakers}`, h.breakers_open ? 'warn' : '')}
      ${kpi('冷却中', h.cooldowns, h.cooldowns ? 'warn' : '')}
    </div></div>
    <div class="card"><h2>下一步</h2>
      <p class="muted">还没有条目？去「渠道与条目」添加上游渠道、key 与模型条目；
      需要 WebSearch 就去「搜索后端」加一个 Tavily/Exa key。
      然后在 Claude Code 里设置 <code>ANTHROPIC_BASE_URL</code> 与 <code>ANTHROPIC_AUTH_TOKEN</code>（下游 key）即可。</p>
    </div>`;
}

// ---------- 统计 ----------

async function renderStats(target) {
  const d = await api('/admin/api/stats?days=' + (target.dataset.days || 7));
  const rows = d.daily;
  const max = Math.max(1, ...rows.map((r) => r.requests));
  const bars = rows.map((r, i) => {
    const h = Math.round((r.requests / max) * 120);
    const x = i * 46 + 20;
    const errH = Math.round((r.errors / max) * 120);
    return `<rect x="${x}" y="${140 - h}" width="26" height="${h}" fill="#6ea8fe" rx="3"></rect>
            <rect x="${x}" y="${140 - errH}" width="26" height="${errH}" fill="#f87171" rx="3"></rect>
            <text x="${x + 13}" y="156" fill="#98a0b0" font-size="10" text-anchor="middle">${esc(r.date.slice(5))}</text>`;
  }).join('');
  const table = (title, list, nameKey) => `
    <div class="card"><h2>${title}</h2><table><thead><tr>
      <th>名称</th><th>请求</th><th>错误</th><th>输入 token</th><th>输出 token</th></tr></thead><tbody>
      ${list.map((r) => `<tr><td>${esc(r[nameKey] || r.name || r.model || '—')}</td>
        <td>${fmtInt(r.requests)}</td><td class="${r.errors ? 'err' : ''}">${fmtInt(r.errors)}</td>
        <td>${fmtInt(r.input_tokens)}</td><td>${fmtInt(r.output_tokens)}</td></tr>`).join('')
      || '<tr><td colspan="5" class="muted">没有数据</td></tr>'}
    </tbody></table></div>`;
  target.innerHTML = `
    <div class="card"><h2>最近 ${d.days} 天（蓝=请求，红=错误）</h2>
      <svg class="chart" viewBox="0 0 ${Math.max(340, rows.length * 46 + 40)} 165" preserveAspectRatio="xMinYMid meet">${bars}</svg>
    </div>
    ${table('按下游 key', d.by_key, 'name')}
    ${table('按渠道', d.by_channel, 'name')}
    ${table('按上游模型', d.by_model, 'model')}
    <div class="row"><button class="ghost" data-days="1">1 天</button>
      <button class="ghost" data-days="7">7 天</button>
      <button class="ghost" data-days="30">30 天</button></div>`;
  target.querySelectorAll('button[data-days]').forEach((b) => {
    b.onclick = () => { target.dataset.days = b.dataset.days; renderStats(target); };
  });
}

// ---------- 请求日志 ----------

async function renderRequests(target) {
  const onlyErrors = target.dataset.onlyErrors === '1';
  const rows = await api('/admin/api/requests?limit=100' + (onlyErrors ? '&only_errors=true' : ''));
  target.innerHTML = `
    <div class="card">
      <div class="row"><label><input type="checkbox" id="only-errors" ${onlyErrors ? 'checked' : ''}> 只看错误</label>
        <span class="muted">点任意一行看每次尝试的明细</span></div>
      <table><thead><tr>
        <th>时间</th><th>下游 key</th><th>请求模型</th><th>实际模型</th><th>渠道 / 上游 key</th>
        <th>状态</th><th>token（入/出/缓存读）</th><th>搜索</th><th>尝试</th><th>首内容</th><th>总耗时</th><th>错误</th>
      </tr></thead><tbody>
      ${rows.map((r) => `<tr data-req="${esc(r.request_id)}" style="cursor:pointer">
        <td>${fmtTime(r.created_ms)}</td><td>${esc(r.key_name)}</td>
        <td class="muted">${esc(r.requested_model)}</td><td>${esc(r.upstream_model)}</td>
        <td>${esc(r.channel)}<span class="muted"> / ${esc(r.upstream_key)}</span></td>
        <td class="${r.status >= 400 ? 'err' : 'ok'}">${r.status}${r.stop_reason ? ' · ' + esc(r.stop_reason) : ''}</td>
        <td>${fmtInt(r.input_tokens)} / ${fmtInt(r.output_tokens)} / ${fmtInt(r.cache_read_tokens)}</td>
        <td>${r.search_requests || ''}</td><td>${r.attempts}</td>
        <td>${fmtMs(r.first_content_ms)}</td><td>${fmtMs(r.total_ms)}</td>
        <td class="err">${r.error_type ? esc(r.error_type) : ''}</td></tr>`).join('')
      || '<tr><td colspan="12" class="muted">还没有请求</td></tr>'}
      </tbody></table>
    </div>`;
  const cb = target.querySelector('#only-errors');
  cb.onchange = () => { target.dataset.onlyErrors = cb.checked ? '1' : '0'; renderRequests(target); };
  target.querySelectorAll('tr[data-req]').forEach((tr) => {
    tr.onclick = async () => {
      const attempts = await api('/admin/api/attempts?request_id=' + encodeURIComponent(tr.dataset.req));
      openDialog(`<h2>请求 ${esc(tr.dataset.req)} 的尝试明细</h2>
        <table><thead><tr><th>#</th><th>渠道</th><th>key</th><th>模型</th><th>协议</th><th>状态</th>
        <th>耗时</th><th>提交</th><th>错误</th></tr></thead><tbody>
        ${attempts.map((a) => `<tr><td>${a.attempt_no}</td><td>${esc(a.channel)}</td><td>${esc(a.upstream_key)}</td>
          <td>${esc(a.upstream_model)}</td><td>${esc(a.protocol)}</td>
          <td class="${a.status >= 400 ? 'err' : ''}">${a.status ?? '—'}</td><td>${fmtMs(a.total_ms)}</td>
          <td>${a.committed ? '是' : ''}</td><td class="err">${esc(a.error_type || '')}
          <div class="muted">${esc(a.error_message || '')}</div></td></tr>`).join('')}
        </tbody></table>
        <div class="row"><button class="primary" onclick="document.getElementById('dialog').close()">关闭</button></div>`);
    };
  });
}

// ---------- 渠道与条目 ----------

async function renderPool(target) {
  const d = await api('/admin/api/channels');
  const keyRow = (k) => `<tr>
    <td>${esc(k.label) || '—'}</td>
    <td class="muted">${esc(k.api_key_masked)}</td>
    <td>${k.enabled ? '<span class="tag ok">启用</span>' : '<span class="tag err">停用</span>'}</td>
    <td>${k.status === 'disabled' ? `<span class="err">${esc(k.status_reason || '被上游拒绝')}</span>` : ''}</td>
    <td>
      <button class="ghost" data-key-edit="${k.id}" data-label="${esc(k.label)}" data-enabled="${k.enabled}">改</button>
      ${k.status === 'disabled' ? `<button class="ghost" data-key-enable="${k.id}">恢复</button>` : ''}
      <button class="danger" data-key-del="${k.id}">删</button>
    </td></tr>`;
  const entryRow = (e) => `<tr>
    <td>${esc(e.upstream_model)}</td><td>${e.tier}</td>
    <td>${fmtInt(e.max_context)}</td>
    <td>${e.vision ? '是' : '否'}${e.pdf ? ' / PDF' : ''}</td>
    <td>${e.enabled ? '<span class="tag ok">启用</span>' : '<span class="tag">停用</span>'}</td>
    <td><button class="ghost" data-entry-del="${e.id}">删</button></td></tr>`;
  target.innerHTML = `
    <div class="card">
      <div class="row"><button class="primary" id="add-channel">新增渠道</button>
        <span class="muted">条目 = 渠道 × 上游模型；同一渠道的所有 key 共享这些条目</span></div>
    </div>
    ${d.channels.map((c) => {
      const keys = d.keys.filter((k) => k.channel_id === c.id);
      const entries = d.entries.filter((e) => e.channel_id === c.id);
      return `<div class="card">
        <h2>${esc(c.name)} <span class="tag">${esc(c.protocol)}</span>
          ${c.enabled ? '' : '<span class="tag err">已停用</span>'}</h2>
        <div class="muted">${esc(c.base_url)}${c.notes ? ' · ' + esc(c.notes) : ''}</div>
        <div class="row" style="margin-top:8px">
          <button class="ghost" data-add-key="${c.id}">加 key</button>
          <button class="ghost" data-add-entry="${c.id}">加条目</button>
          <button class="ghost" data-edit-channel="${c.id}">改渠道</button>
          <button class="danger" data-del-channel="${c.id}">删渠道</button>
        </div>
        <table><thead><tr><th>key 备注</th><th>密钥</th><th>状态</th><th>说明</th><th></th></tr></thead>
          <tbody>${keys.map(keyRow).join('') || '<tr><td colspan="5" class="muted">还没有 key</td></tr>'}</tbody></table>
        <table style="margin-top:8px"><thead><tr><th>条目</th><th>层级</th><th>上下文</th><th>多模态</th><th>状态</th><th></th></tr></thead>
          <tbody>${entries.map(entryRow).join('') || '<tr><td colspan="6" class="muted">还没有条目</td></tr>'}</tbody></table>
      </div>`;
    }).join('') || '<div class="card muted">还没有渠道</div>'}`;

  const byId = (id) => d.channels.find((c) => c.id === Number(id));
  target.querySelector('#add-channel').onclick = () => channelForm();
  target.querySelectorAll('[data-add-key]').forEach((b) => b.onclick = () => keyForm(Number(b.dataset.addKey)));
  target.querySelectorAll('[data-add-entry]').forEach((b) => b.onclick = () => entryForm(Number(b.dataset.addEntry)));
  target.querySelectorAll('[data-edit-channel]').forEach((b) => b.onclick = () => channelForm(byId(b.dataset.editChannel)));
  target.querySelectorAll('[data-del-channel]').forEach((b) => b.onclick = async () => {
    if (!confirm('删除渠道会同时删掉它的 key、条目与冷却记录，确定？')) return;
    await api('/admin/api/channels/' + b.dataset.delChannel, { method: 'DELETE' });
    toast('已删除'); refresh();
  });
  target.querySelectorAll('[data-key-edit]').forEach((b) => b.onclick = async () => {
    const label = prompt('key 备注：', b.dataset.label);
    if (label === null) return;
    const enabled = confirm('这个 key 现在要启用吗？（取消 = 停用）');
    await api('/admin/api/keys/' + b.dataset.keyEdit, {
      method: 'PATCH', body: JSON.stringify({ label, enabled }),
    });
    toast('已保存'); refresh();
  });
  target.querySelectorAll('[data-key-enable]').forEach((b) => b.onclick = async () => {
    await api(`/admin/api/keys/${b.dataset.keyEnable}/enable`, { method: 'POST' });
    toast('已恢复，请确认这把 key 确实可用'); refresh();
  });
  target.querySelectorAll('[data-key-del]').forEach((b) => b.onclick = async () => {
    if (!confirm('删除这把上游 key？')) return;
    await api('/admin/api/keys/' + b.dataset.keyDel, { method: 'DELETE' });
    toast('已删除'); refresh();
  });
  target.querySelectorAll('[data-entry-del]').forEach((b) => b.onclick = async () => {
    if (!confirm('删除这个条目？')) return;
    await api('/admin/api/entries/' + b.dataset.entryDel, { method: 'DELETE' });
    toast('已删除'); refresh();
  });
}

function channelForm(channel) {
  const c = channel || { name: '', protocol: 'openai_chat', base_url: '', extra_headers: {}, enabled: true, notes: '' };
  openDialog(`<h2>${channel ? '修改' : '新增'}渠道</h2>
    <div class="row"><input id="f-name" placeholder="渠道名（如 gemini-free）" value="${esc(c.name)}" style="flex:1"></div>
    <div class="row"><select id="f-proto">
      ${['openai_chat', 'openai_responses', 'gemini', 'anthropic'].map((p) =>
        `<option value="${p}" ${p === c.protocol ? 'selected' : ''}>${p}</option>`).join('')}
    </select></div>
    <div class="row"><input id="f-url" placeholder="base_url（粘到 /v1 或完整端点都行）" value="${esc(c.base_url)}" style="flex:1"></div>
    <div class="row"><input id="f-headers" placeholder='附加请求头 JSON，如 {"x-foo":"bar"}' value="${esc(JSON.stringify(c.extra_headers || {}))}" style="flex:1"></div>
    <div class="row"><input id="f-notes" placeholder="备注" value="${esc(c.notes)}" style="flex:1"></div>
    <div class="row"><label><input type="checkbox" id="f-enabled" ${c.enabled ? 'checked' : ''}> 启用</label></div>
    <div class="row"><button class="primary" id="f-save">保存</button>
      <button class="ghost" onclick="document.getElementById('dialog').close()">取消</button></div>`);
  document.getElementById('f-save').onclick = async () => {
    let extra_headers = {};
    try { extra_headers = JSON.parse(document.getElementById('f-headers').value || '{}'); } catch { return toast('附加头不是合法 JSON', 'err'); }
    const payload = {
      name: document.getElementById('f-name').value.trim(),
      protocol: document.getElementById('f-proto').value,
      base_url: document.getElementById('f-url').value.trim(),
      extra_headers,
      enabled: document.getElementById('f-enabled').checked,
      notes: document.getElementById('f-notes').value.trim(),
    };
    if (!payload.name || !payload.base_url) return toast('名字和 base_url 必填', 'err');
    try {
      if (channel) await api('/admin/api/channels/' + channel.id, { method: 'PATCH', body: JSON.stringify(payload) });
      else await api('/admin/api/channels', { method: 'POST', body: JSON.stringify(payload) });
      document.getElementById('dialog').close();
      toast('已保存，立即生效'); refresh();
    } catch (e) { toast(e.message, 'err'); }
  };
}

function keyForm(channelId) {
  openDialog(`<h2>新增上游 key</h2>
    <div class="row"><input id="f-label" placeholder="备注（如 免费号 1）" style="flex:1"></div>
    <div class="row"><input id="f-key" placeholder="api key" style="flex:1"></div>
    <p class="muted">同一渠道可以放多把 key：额度是每把 key 各算一份，429 冷却也只针对单把 key。</p>
    <div class="row"><button class="primary" id="f-save">保存</button>
      <button class="ghost" onclick="document.getElementById('dialog').close()">取消</button></div>`);
  document.getElementById('f-save').onclick = async () => {
    const payload = {
      label: document.getElementById('f-label').value.trim(),
      api_key: document.getElementById('f-key').value.trim(),
      enabled: true,
    };
    try {
      await api(`/admin/api/channels/${channelId}/keys`, { method: 'POST', body: JSON.stringify(payload) });
      document.getElementById('dialog').close();
      toast('已添加'); refresh();
    } catch (e) { toast(e.message, 'err'); }
  };
}

function entryForm(channelId) {
  openDialog(`<h2>新增条目（渠道 × 上游模型）</h2>
    <div class="row"><input id="f-model" placeholder="上游模型名（如 gemini-2.5-flash）" style="flex:1"></div>
    <div class="row"><label>层级 <input id="f-tier" type="number" value="1" style="width:80px"></label>
      <span class="muted">数字越小越优先；整层不可用才会降级</span></div>
    <div class="row"><label>上下文窗口 <input id="f-ctx" type="number" value="200000" style="width:120px"></label>
      <span class="muted">按 token 估算，放不下的请求会跳过这个条目</span></div>
    <div class="row"><label><input type="checkbox" id="f-vision" checked> 支持图片</label>
      <label><input type="checkbox" id="f-pdf"> 支持 PDF</label></div>
    <div class="row"><input id="f-notes" placeholder="备注" style="flex:1"></div>
    <div class="row"><button class="primary" id="f-save">保存</button>
      <button class="ghost" onclick="document.getElementById('dialog').close()">取消</button></div>`);
  document.getElementById('f-save').onclick = async () => {
    const payload = {
      upstream_model: document.getElementById('f-model').value.trim(),
      tier: Number(document.getElementById('f-tier').value || 1),
      max_context: Number(document.getElementById('f-ctx').value || 200000),
      vision: document.getElementById('f-vision').checked,
      pdf: document.getElementById('f-pdf').checked,
      enabled: true,
      notes: document.getElementById('f-notes').value.trim(),
    };
    if (!payload.upstream_model) return toast('模型名必填', 'err');
    try {
      await api(`/admin/api/channels/${channelId}/entries`, { method: 'POST', body: JSON.stringify(payload) });
      document.getElementById('dialog').close();
      toast('已添加'); refresh();
    } catch (e) { toast(e.message, 'err'); }
  };
}

// ---------- 健康 ----------

async function renderHealth(target) {
  const d = await api('/admin/api/health');
  target.innerHTML = `
    <div class="card"><h2>熔断（渠道 × 模型）</h2>
      <table><thead><tr><th>渠道</th><th>模型</th><th>状态</th><th>连续失败</th><th>样本</th><th>打开时间</th><th></th></tr></thead><tbody>
      ${d.breakers.map((b) => `<tr>
        <td>${esc(b.channel)}</td><td>${esc(b.model)}</td>
        <td class="${b.state === 'closed' ? 'ok' : 'warn'}">${esc(b.state)}</td>
        <td>${b.consecutive_failures}</td><td>${b.failed}/${b.total}</td>
        <td>${b.opened_ms ? fmtTime(b.opened_ms) : '—'}</td>
        <td><button class="ghost" data-reset-ch="${b.channel_id}" data-model="${esc(b.model)}">重置</button></td></tr>`).join('')
      || '<tr><td colspan="7" class="muted">没有熔断记录</td></tr>'}
      </tbody></table></div>
    <div class="card"><h2>冷却（key × 模型）</h2>
      <table><thead><tr><th>key id</th><th>模型</th><th>原因</th><th>剩余</th><th></th></tr></thead><tbody>
      ${d.cooldowns.map((c) => `<tr><td>${c.key_id}</td><td>${esc(c.model)}</td>
        <td>${esc(c.reason)}</td><td>${fmtUntil(c.until_ms)}</td>
        <td><button class="ghost" data-reset-key="${c.key_id}" data-model="${esc(c.model)}">清除</button></td></tr>`).join('')
      || '<tr><td colspan="5" class="muted">没有冷却中的 key</td></tr>'}
      </tbody></table>
      <div class="row" style="margin-top:10px"><button class="primary" id="reset-all">全部重置（熔断 + 冷却）</button></div>
    </div>`;
  target.querySelectorAll('[data-reset-ch]').forEach((b) => b.onclick = async () => {
    await api('/admin/api/health/reset', {
      method: 'POST',
      body: JSON.stringify({ channel_id: Number(b.dataset.resetCh), model: b.dataset.model, clear_cooldowns: true }),
    });
    toast('已重置'); renderHealth(target);
  });
  target.querySelectorAll('[data-reset-key]').forEach((b) => b.onclick = async () => {
    await api('/admin/api/health/reset', {
      method: 'POST',
      body: JSON.stringify({ key_id: Number(b.dataset.resetKey), model: b.dataset.model }),
    });
    toast('已清除'); renderHealth(target);
  });
  target.querySelector('#reset-all').onclick = async () => {
    await api('/admin/api/health/reset', { method: 'POST', body: JSON.stringify({ clear_cooldowns: true }) });
    toast('已全部重置'); renderHealth(target);
  };
}

// ---------- 下游 key ----------

async function renderDownstream(target) {
  const rows = await api('/admin/api/downstream_keys');
  target.innerHTML = `
    <div class="card">
      <div class="row"><button class="primary" id="add-down">新建下游 key</button>
        <span class="muted">明文只在创建时显示一次，库里只存 sha256</span></div>
      <table><thead><tr><th>名字</th><th>前缀</th><th>状态</th><th>创建</th><th>最近使用</th><th></th></tr></thead><tbody>
      ${rows.map((r) => `<tr><td>${esc(r.name)}</td><td class="muted">${esc(r.key_prefix)}…</td>
        <td>${r.enabled ? '<span class="tag ok">启用</span>' : '<span class="tag err">停用</span>'}</td>
        <td>${fmtTime(r.created_ms)}</td><td>${fmtTime(r.last_used_ms)}</td>
        <td><button class="ghost" data-toggle="${r.id}" data-name="${esc(r.name)}" data-enabled="${r.enabled}">改</button>
        <button class="danger" data-del="${r.id}">删</button></td></tr>`).join('')
      || '<tr><td colspan="6" class="muted">还没有下游 key</td></tr>'}
      </tbody></table></div>`;
  target.querySelector('#add-down').onclick = async () => {
    const name = prompt('给这把下游 key 起个名字（比如「我的笔记本」）：');
    if (!name) return;
    const out = await api('/admin/api/downstream_keys', { method: 'POST', body: JSON.stringify({ name, enabled: true }) });
    openDialog(`<h2>下游 key 已创建</h2>
      <p>请立刻复制保存，之后不会再显示：</p>
      <pre>${esc(out.key)}</pre>
      <p class="muted">在 Claude Code 里这样用：<br>
      <code>export ANTHROPIC_BASE_URL=https://你的域名</code><br>
      <code>export ANTHROPIC_AUTH_TOKEN=${esc(out.key)}</code></p>
      <button class="primary" onclick="document.getElementById('dialog').close()">我已保存</button>`);
    renderDownstream(target);
  };
  target.querySelectorAll('[data-toggle]').forEach((b) => b.onclick = async () => {
    const name = prompt('名字：', b.dataset.name);
    if (name === null) return;
    const enabled = confirm('启用吗？（取消 = 停用）');
    await api('/admin/api/downstream_keys/' + b.dataset.toggle, { method: 'PATCH', body: JSON.stringify({ name, enabled }) });
    toast('已保存'); renderDownstream(target);
  });
  target.querySelectorAll('[data-del]').forEach((b) => b.onclick = async () => {
    if (!confirm('删除后这把 key 立刻失效，确定？')) return;
    await api('/admin/api/downstream_keys/' + b.dataset.del, { method: 'DELETE' });
    toast('已删除'); renderDownstream(target);
  });
}

// ---------- 搜索后端 ----------

async function renderSearch(target) {
  const rows = await api('/admin/api/search_backends');
  const kinds = ['tavily', 'exa', 'firecrawl', 'parallel', 'jina'];
  target.innerHTML = `
    <div class="card">
      <div class="row"><button class="primary" id="add-search">新增搜索后端</button>
        <span class="muted">WebSearch 由网关自己执行：模型调用搜索 → 网关查后端 → 结果以标准块回给 Claude Code</span></div>
      <table><thead><tr><th>名字</th><th>类型</th><th>密钥</th><th>base_url</th><th>状态</th><th>冷却</th><th></th></tr></thead><tbody>
      ${rows.map((r) => `<tr><td>${esc(r.name)}</td><td>${esc(r.kind)}</td>
        <td class="muted">${esc(r.api_key_masked)}</td><td class="muted">${esc(r.base_url || '默认')}</td>
        <td>${r.enabled ? '<span class="tag ok">启用</span>' : '<span class="tag">停用</span>'}</td>
        <td>${r.cooldown_until_ms ? fmtUntil(r.cooldown_until_ms) : ''}</td>
        <td><button class="ghost" data-edit="${r.id}">改</button>
        <button class="danger" data-del="${r.id}">删</button></td></tr>`).join('')
      || '<tr><td colspan="7" class="muted">还没有搜索后端（WebSearch 会用不了，其它功能不受影响）</td></tr>'}
      </tbody></table></div>`;
  const form = (row) => {
    const r = row || { name: '', kind: 'tavily', base_url: '', enabled: true, notes: '' };
    openDialog(`<h2>${row ? '修改' : '新增'}搜索后端</h2>
      <div class="row"><input id="s-name" placeholder="名字（如 tavily-1）" value="${esc(r.name)}" style="flex:1"></div>
      <div class="row"><select id="s-kind">${kinds.map((k) =>
        `<option ${k === r.kind ? 'selected' : ''}>${k}</option>`).join('')}</select></div>
      <div class="row"><input id="s-key" placeholder="${row ? '留空表示不修改' : 'api key'}" style="flex:1"></div>
      <div class="row"><input id="s-url" placeholder="base_url（留空用默认）" value="${esc(r.base_url)}" style="flex:1"></div>
      <div class="row"><input id="s-notes" placeholder="备注" value="${esc(r.notes || '')}" style="flex:1"></div>
      <div class="row"><label><input type="checkbox" id="s-enabled" ${r.enabled ? 'checked' : ''}> 启用</label></div>
      <div class="row"><button class="primary" id="s-save">保存</button>
        <button class="ghost" onclick="document.getElementById('dialog').close()">取消</button></div>`);
    document.getElementById('s-save').onclick = async () => {
      const payload = {
        name: document.getElementById('s-name').value.trim(),
        kind: document.getElementById('s-kind').value,
        api_key: document.getElementById('s-key').value.trim(),
        base_url: document.getElementById('s-url').value.trim(),
        enabled: document.getElementById('s-enabled').checked,
        notes: document.getElementById('s-notes').value.trim(),
      };
      if (!payload.name) return toast('名字必填', 'err');
      try {
        if (row) await api('/admin/api/search_backends/' + row.id, { method: 'PATCH', body: JSON.stringify(payload) });
        else await api('/admin/api/search_backends', { method: 'POST', body: JSON.stringify(payload) });
        document.getElementById('dialog').close();
        toast('已保存'); renderSearch(target);
      } catch (e) { toast(e.message, 'err'); }
    };
  };
  target.querySelector('#add-search').onclick = () => form(null);
  target.querySelectorAll('[data-edit]').forEach((b) => b.onclick = () => form(rows.find((r) => r.id === Number(b.dataset.edit))));
  target.querySelectorAll('[data-del]').forEach((b) => b.onclick = async () => {
    if (!confirm('删除这个搜索后端？')) return;
    await api('/admin/api/search_backends/' + b.dataset.del, { method: 'DELETE' });
    toast('已删除'); renderSearch(target);
  });
}

// ---------- 矫正规则 ----------

async function renderRules(target) {
  const rows = await api('/admin/api/rules');
  target.innerHTML = `
    <div class="card">
      <p class="muted">这些是网关在遇到 400/413/422 时用来改写请求的补丁。内置矫正器（思考签名、思考预算、
      max_tokens 下夹、图片降级）不走这里；这张表是把「上游报错 → 请求骨架」交给指定分析条目分析后
      沉淀下来的规则，命中会自动应用。</p>
      <table><thead><tr><th>作用范围</th><th>错误特征</th><th>来源</th><th>补丁</th><th>命中</th><th>状态</th><th></th></tr></thead><tbody>
      ${rows.map((r) => `<tr><td>${esc(r.scope) || '全局'}</td>
        <td class="muted">${esc(r.error_fingerprint)}<div class="muted">${esc(r.error_sample || '')}</div></td>
        <td>${esc(r.source)}</td>
        <td><pre style="margin:0">${esc(r.patch_json)}</pre></td>
        <td>${r.hits}</td>
        <td>${r.enabled ? '<span class="tag ok">启用</span>' : '<span class="tag">停用</span>'}</td>
        <td><button class="ghost" data-toggle="${r.id}" data-enabled="${r.enabled}">${r.enabled ? '停用' : '启用'}</button>
        <button class="danger" data-del="${r.id}">删</button></td></tr>`).join('')
      || '<tr><td colspan="7" class="muted">还没有规则（遇到未知 400 时会自动分析并生成）</td></tr>'}
      </tbody></table></div>`;
  target.querySelectorAll('[data-toggle]').forEach((b) => b.onclick = async () => {
    await api('/admin/api/rules/' + b.dataset.toggle, {
      method: 'PATCH', body: JSON.stringify({ enabled: b.dataset.enabled !== 'true' }),
    });
    renderRules(target);
  });
  target.querySelectorAll('[data-del]').forEach((b) => b.onclick = async () => {
    if (!confirm('删除这条规则？')) return;
    await api('/admin/api/rules/' + b.dataset.del, { method: 'DELETE' });
    renderRules(target);
  });
}

// ---------- 设置 ----------

async function renderSettings(target) {
  const settings = await api('/admin/api/settings');
  target.innerHTML = `
    <div class="card"><h2>运行期设置（保存后立即生效）</h2>
      <textarea id="settings-json" spellcheck="false">${esc(JSON.stringify(settings, null, 2))}</textarea>
      <div class="row" style="margin-top:8px">
        <button class="primary" id="save-settings">保存</button>
        <span class="muted">常用项：publicModelId、modelEcho（upstream/request/fixed）、maxAttempts、
          firstContentTimeoutMs、breaker.*、search.*、analysisEntryId、alerts.smtp</span>
      </div></div>
    <div class="card"><h2>备份与迁移</h2>
      <div class="row"><button class="ghost" id="do-export">导出配置 JSON</button>
        <button class="ghost" id="do-import">导入配置 JSON</button></div>
      <p class="muted">导出包含渠道、上游 key、条目、搜索后端、规则与设置（不含下游 key 明文，
      只保留哈希，导入后原密钥继续有效）。</p></div>
    <div class="card"><h2>改后台密码</h2>
      <div class="row"><input id="old-pw" type="password" placeholder="当前密码">
        <input id="new-pw" type="password" placeholder="新密码（至少 8 位）">
        <button class="primary" id="save-pw">修改</button></div></div>`;

  target.querySelector('#save-settings').onclick = async () => {
    try {
      const parsed = JSON.parse(target.querySelector('#settings-json').value);
      await api('/admin/api/settings', { method: 'PUT', body: JSON.stringify(parsed) });
      toast('已保存并生效');
    } catch (e) { toast('保存失败：' + e.message, 'err'); }
  };
  target.querySelector('#do-export').onclick = async () => {
    const data = await api('/admin/api/export');
    const blob = new Blob([JSON.stringify(data, null, 2)], { type: 'application/json' });
    const a = document.createElement('a');
    a.href = URL.createObjectURL(blob);
    a.download = 'ufp-config.json';
    a.click();
  };
  target.querySelector('#do-import').onclick = () => {
    const input = document.createElement('input');
    input.type = 'file';
    input.accept = '.json';
    input.onchange = async () => {
      const text = await input.files[0].text();
      try {
        const out = await api('/admin/api/import', { method: 'POST', body: text });
        toast('导入完成：' + JSON.stringify(out.imported));
        refresh();
      } catch (e) { toast('导入失败：' + e.message, 'err'); }
    };
    input.click();
  };
  target.querySelector('#save-pw').onclick = async () => {
    try {
      await api('/admin/api/password', {
        method: 'POST',
        body: JSON.stringify({
          old_password: target.querySelector('#old-pw').value,
          new_password: target.querySelector('#new-pw').value,
        }),
      });
      toast('密码已更新');
    } catch (e) { toast('修改失败：' + e.message, 'err'); }
  };
}

// ---------- 启动 ----------

checkAuth();
setInterval(() => { if (!document.getElementById('app').hidden) refresh(); }, 30000);
