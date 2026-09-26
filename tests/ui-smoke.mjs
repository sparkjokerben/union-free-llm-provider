// 后台界面冒烟测试：用无头 Chrome 真跑一遍管理后台。
//
// 为什么需要它：后台是唯一的人机界面，却曾经两次悄悄坏掉 —— 一次是旧 app.js 配新
// 页面（整页白屏），一次是写请求没带 content-type（fetch 默认发 text/plain，
// 服务端的 Json 提取器直接回 415，连登录都进不去）。纯 Rust 测试看不见这些，
// 因为坏的是浏览器里的那段代码。
//
// 用法：
//   cargo build -p ufp
//   UFP_DB=/tmp/smoke.db ./target/debug/ufp set-admin-password 'ci-smoke-password'
//   UFP_DB=/tmp/smoke.db UFP_LISTEN=127.0.0.1:8787 ./target/debug/ufp serve &
//   node tests/ui-smoke.mjs http://127.0.0.1:8787 ci-smoke-password
//
// 关于写操作：登录本身是 POST（会话），另外还会把设置原样存回、建一个渠道再删掉。
// 这些写只允许落在**本机的一次性实例**上 —— 这个脚本能被随手指向任何地址，不能让它
// 有机会改掉线上配置。指向非本机地址时自动只读（跳过写操作那几步），要对非本机实例
// 写就必须显式加 --write。
//   看判定结果（不连任何东西）：node tests/ui-smoke.mjs <地址> --selfcheck
//   强制只读（用来验证只读那条路径）：UFP_SMOKE_RO=1 node tests/ui-smoke.mjs <地址> <密码>
//
// 依赖：Node 22+（用到全局 WebSocket）、本机的 Chrome / Chromium / Edge。
// 退出码非 0 表示后台不可用，消息里会写明是哪一步坏、控制台报了什么。

import { spawn } from 'node:child_process';
import { existsSync, mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

const BASE = process.argv[2] || 'http://127.0.0.1:8787';
const PW = process.argv[3] || 'ci-smoke-password';
const PORT = 9500 + Math.floor(Math.random() * 400);

// 目标是不是「本机的一次性实例」——只看地址，写操作的全部许可都系在这上面
const host = (() => { try { return new URL(BASE).hostname; } catch { return ''; } })();
const isLoopback = host === 'localhost' || host === '::1' || host === '[::1]' || /^127\./.test(host);
const forceRo = ['1', 'true', 'yes'].includes(String(process.env.UFP_SMOKE_RO || '').toLowerCase());
const allowWrite = process.argv.includes('--write');
const WRITES = !forceRo && (isLoopback || allowWrite);

// 后台的页签数。加了页面记得改这里——下面那处断言和结尾的汇总都用它，
// 免得只改了其中一半，测试自己撒谎。
const PAGES_COUNT = 10;

if (process.argv.includes('--selfcheck')) {
  console.log(JSON.stringify({
    地址: BASE, 主机: host, 本机: isLoopback,
    强制只读: forceRo, 显式允许写: allowWrite, 会做写操作: WRITES,
  }, null, 1));
  process.exit(0);
}

const CANDIDATES = [
  process.env.CHROME,
  'google-chrome',
  'google-chrome-stable',
  'chromium',
  'chromium-browser',
  '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome',
  '/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge',
].filter(Boolean);

function findBrowser() {
  for (const c of CANDIDATES) {
    if (c.includes('/')) { if (existsSync(c)) return c; }
    else {
      const p = process.env.PATH.split(':').find((dir) => existsSync(join(dir, c)));
      if (p) return join(p, c);
    }
  }
  return null;
}

const browser = findBrowser();
if (!browser) fail(`找不到浏览器（试过：${CANDIDATES.join(', ')}）`);

function fail(msg) {
  console.error(`✗ 后台冒烟测试失败：${msg}`);
  process.exit(1);
}

// ── 启动浏览器 ─────────────────────────────────────────────────────────
// CI 上偶发起不来（DevTools 端口迟迟不监听），所以：等满 60 秒、进程提前退出
// 就立刻报错、整轮失败再重试一次。
let child = null;
process.on('exit', () => { try { child && child.kill('SIGKILL'); } catch {} });

async function tryLaunch(port) {
  const profile = mkdtempSync(join(tmpdir(), 'ufp-smoke-'));
  const proc = spawn(browser, [
    '--headless=new',
    `--remote-debugging-port=${port}`,
    `--user-data-dir=${profile}`,
    '--no-first-run',
    '--no-default-browser-check',
    '--disable-gpu',
    '--disable-dev-shm-usage',
    '--disable-background-networking',
    '--no-sandbox', // CI runner 里内核可能不给用户命名空间，不加这个 Chrome 直接起不来
    'about:blank',
  ], { stdio: ['ignore', 'ignore', 'pipe'] });

  let log = '';
  let exited = false;
  proc.stderr.on('data', (d) => { log += d.toString(); });
  proc.on('exit', (code) => { exited = true; log += `\n[浏览器进程退出，code=${code}]`; });

  const started = Date.now();
  while (Date.now() - started < 60_000) {
    try {
      const list = await (await fetch(`http://127.0.0.1:${port}/json/list`)).json();
      const page = list.find((t) => t.type === 'page' && t.webSocketDebuggerUrl);
      if (page) return { proc, page };
    } catch {}
    if (exited) break;
    await new Promise((r) => setTimeout(r, 500));
  }
  try { proc.kill('SIGKILL'); } catch {}
  throw new Error(`等了 ${Math.round((Date.now() - started) / 1000)} 秒还没有可调试的页面。\n${log.slice(-1200)}`);
}

let target = null;
for (let attempt = 1; attempt <= 2; attempt++) {
  const port = PORT + attempt - 1;
  try {
    const r = await tryLaunch(port);
    child = r.proc;
    target = r.page;
    break;
  } catch (e) {
    if (attempt === 2) fail(e.message || String(e));
    console.error(`· 第 ${attempt} 次启动浏览器没成功，重试一次…`);
  }
}
const ws = new WebSocket(target.webSocketDebuggerUrl);
const pending = new Map();
const problems = [];
let collecting = false;
let msgId = 0;

ws.addEventListener('message', (ev) => {
  const msg = JSON.parse(ev.data);
  if (msg.id && pending.has(msg.id)) {
    const { resolve, reject } = pending.get(msg.id);
    pending.delete(msg.id);
    msg.error ? reject(new Error(JSON.stringify(msg.error))) : resolve(msg.result);
    return;
  }
  if (!collecting) return;
  if (msg.method === 'Runtime.exceptionThrown') {
    const d = msg.params.exceptionDetails;
    const where = d.url ? ` @ ${d.url}:${d.lineNumber + 1}` : '';
    problems.push(`未捕获异常：${d.text}${where} ${String(d.exception?.description || '').split('\n')[0]}`);
  } else if (msg.method === 'Runtime.consoleAPICalled' && msg.params.type === 'error') {
    const text = msg.params.args.map((a) => a.value ?? a.description ?? a.type).join(' ');
    // 登录前的未授权探测是预期噪音，不算故障
    if (!/401|Unauthorized/.test(text)) problems.push(`console.error：${text}`);
  } else if (msg.method === 'Page.javascriptDialogOpening') {
    send('Page.handleJavaScriptDialog', { accept: true, promptText: '__smoke__' });
  }
});
await new Promise((res, rej) => {
  ws.addEventListener('open', res, { once: true });
  ws.addEventListener('error', rej, { once: true });
});

const send = (method, params = {}) =>
  new Promise((resolve, reject) => {
    const id = ++msgId;
    pending.set(id, { resolve, reject });
    ws.send(JSON.stringify({ id, method, params }));
  });
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function evaluate(expression) {
  const r = await send('Runtime.evaluate', { expression, awaitPromise: true, returnByValue: true });
  if (r.exceptionDetails) {
    fail(`页面里这段脚本抛了异常：${r.exceptionDetails.exception?.description || r.exceptionDetails.text}`);
  }
  return r.result.value;
}

async function step(label, expression, ms = 1200) {
  const ok = await evaluate(expression);
  if (ok !== true) fail(`「${label}」没通过：${JSON.stringify(ok)}`);
  await sleep(ms);
  console.log(`  ✓ ${label}`);
}

await send('Runtime.enable');
await send('Page.enable');
await send('Network.enable');
await send('Network.setCacheDisabled', { cacheDisabled: true });
// 固定成桌面尺寸：默认窗口是 800 宽，会落到窄屏那套布局上，测不到桌面版
await send('Emulation.setDeviceMetricsOverride', { width: 1280, height: 860, deviceScaleFactor: 1, mobile: false });

console.log(`· 浏览器：${browser}`);
console.log(`· 目标：${BASE}/admin`);
console.log(WRITES
  ? '· 写操作：开（只写这个实例自己的库）'
  : forceRo
    ? '· 写操作：关 —— 强制只读（UFP_SMOKE_RO）'
    : `· 写操作：关 —— 目标 ${host} 不是本机实例；确实要写就显式加 --write`);
await send('Page.navigate', { url: `${BASE}/admin` });
await sleep(2000);

collecting = true;

await step('页面加载出脚本（没有白屏）',
  `!!document.querySelector('#rail') && !!document.querySelector('#pages') && !!document.querySelector('#do-login')`, 300);

// 注意：这里看的是**算出来的** display，不是 hidden 属性 —— 曾经 CSS 里的
// .shell{display:grid} 盖掉了 [hidden]，属性是对的、屏幕上一片照旧。
await step('未登录时控制台不可见（hidden 真的生效）',
  `getComputedStyle(document.querySelector('.shell')).display === 'none' &&
   getComputedStyle(document.querySelector('#gate')).display !== 'none' &&
   getComputedStyle(document.querySelector('#bootfail')).display === 'none'`, 200);

await step('登录（这一步会发 POST，缺 content-type 就会 415）',
  `(async () => {
     document.querySelector('#pw').value = ${JSON.stringify(PW)};
     document.querySelector('#do-login').click();
     await new Promise(r => setTimeout(r, 2000));
     return document.querySelector('#gate').hidden === true;
   })()`, 300);

await step('登录后：控制台可见、登录框收起来',
  `getComputedStyle(document.querySelector('.shell')).display !== 'none' &&
   getComputedStyle(document.querySelector('#gate')).display === 'none'`, 200);

await step(`外壳渲染：机架 + 读数带 + ${PAGES_COUNT} 个页签`,
  `document.querySelector('#rail').innerHTML.length > 20 &&
   document.querySelector('#readout').textContent.trim().length > 0 &&
   document.querySelectorAll('#pages button').length === ${PAGES_COUNT}`, 200);

// 网格默认 align-content:stretch 会把 100vh 的余量摊给各行，顶部读数带那一行
// 于是被撑高，内容少的时候（池子空着、页面稀疏）标题下面就多出一大块空白。
// 直接量标题行的高度：它应该贴着内容（约 45px），被撑高就是这个问题回来了。
await step('版面没有异常空白（标题贴着内容，没有大缝）',
  `(() => {
     const top = document.querySelector('.top').getBoundingClientRect();
     if (top.height > 80) return '顶部读数带被撑高到 ' + Math.round(top.height) + 'px';
     const work = document.querySelector('.work').getBoundingClientRect();
     const gap = work.top - top.bottom;
     return gap < 80 ? true : '标题与内容之间空出了 ' + Math.round(gap) + 'px';
   })()`, 200);

const pages = await evaluate(`JSON.stringify([...document.querySelectorAll('#pages button')].map(b => b.dataset.page))`);
for (const page of JSON.parse(pages)) {
  await step(`页面「${page}」渲染`,
    `(async () => {
       document.querySelector('#pages button[data-page="${page}"]').click();
       await new Promise(r => setTimeout(r, 1000));
       const host = document.querySelector('#p-${page}');
       if (!host.classList.contains('on')) return '这一页没有被激活';
       const t = host.textContent.trim();
       if (t.length < 20) return '内容几乎为空：' + t.slice(0, 60);
       if (/加载失败/.test(t)) return '页面报错：' + t.slice(0, 120);
       return true;
     })()`, 150);
}

// 页面里的跳转按钮：页签高亮和显示的内容必须是同一页（曾经只换了高亮，内容还停在原页）
await step('页内跳转：概览的「看请求明细」真的切到请求页',
  `(async () => {
     document.querySelector('#pages button[data-page="overview"]').click();
     await new Promise(r => setTimeout(r, 900));
     document.querySelector('#p-overview button[data-go="requests"]').click();
     await new Promise(r => setTimeout(r, 900));
     const tab = document.querySelector('#pages [aria-current]').dataset.page;
     const shown = [...document.querySelectorAll('section')].filter(s => getComputedStyle(s).display !== 'none').map(s => s.id);
     return tab === 'requests' && shown.join() === 'p-requests' ? true : '页签=' + tab + ' 可见=' + shown.join();
   })()`, 200);

await step('设置编辑框里没有部署令牌',
  `(async () => {
     document.querySelector('#pages button[data-page="settings"]').click();
     await new Promise(r => setTimeout(r, 900));
     return /deployToken/.test(document.querySelector('#set').value) ? '编辑框里出现了 deployToken' : true;
   })()`, 200);

await step('定时刷新不冲掉设置页里没保存的编辑',
  `(async () => {
     const box = document.querySelector('#set');
     box.value = '{"__half_edited__": true}';
     await tick();
     return document.querySelector('#set').value.includes('__half_edited__') ? true : '编辑内容被刷新冲掉了';
   })()`, 200);

await step('新增渠道先给出预设（OpenRouter / Google AI Studio / OpenCode Zen / 自定义）',
  `(async () => {
     document.querySelector('#pages button[data-page="pool"]').click();
     await new Promise(r => setTimeout(r, 900));
     document.querySelector('button[data-newch]').click();
     await new Promise(r => setTimeout(r, 600));
     const names = [...document.querySelectorAll('#dlg .preset b')].map(b => b.textContent);
     document.querySelector('#dlg').close();
     const want = ['OpenRouter', 'Google AI Studio', 'OpenCode Zen', '自定义'];
     return want.every(w => names.includes(w)) ? true : '预设不全：' + names.join('、');
   })()`, 200);

if (WRITES) {
  await step('写操作：设置项原样存回（同样走 POST/PUT）',
    `(async () => {
       const s = await api('/admin/api/settings');
       await api('/admin/api/settings', { method: 'PUT', body: JSON.stringify(s) });
       return true;
     })()`, 300);

  await step('对话框：新建渠道 → 出现在池子里',
    `(async () => {
       document.querySelector('#pages button[data-page="pool"]').click();
       await new Promise(r => setTimeout(r, 800));
       document.querySelector('button[data-newch]').click();
       await new Promise(r => setTimeout(r, 600));
       [...document.querySelectorAll('#dlg .preset')].find(b => b.textContent.includes('自定义')).click();
       await new Promise(r => setTimeout(r, 300));
       document.querySelector('#f-name').value = '__smoke_chan__';
       document.querySelector('#f-proto').value = 'openai_chat';
       document.querySelector('#f-url').value = 'http://127.0.0.1:9/v1';
       document.querySelector('#f-save').click();
       await new Promise(r => setTimeout(r, 1500));
       if (!S.pool.channels.some(c => c.name === '__smoke_chan__')) return '新渠道没进池子';
       return true;
     })()`, 300);

  await step('加 key → 停用：行内显示「停用」，不是「被上游拒绝」',
    `(async () => {
       const sec = () => [...document.querySelectorAll('#p-pool div.sec')].find(x => x.textContent.includes('__smoke_chan__'));
       sec().querySelector('button[data-newkey]').click();
       await new Promise(r => setTimeout(r, 300));
       document.querySelector('#f-label').value = 'smoke-key';
       document.querySelector('#f-key').value = 'sk-smoke-0000000000';
       document.querySelector('#f-save').click();
       await new Promise(r => setTimeout(r, 1200));
       const tog = sec().querySelector('button[data-togkey]');
       if (!tog) return '找不到停用按钮';
       tog.click();
       await new Promise(r => setTimeout(r, 1200));
       const row = [...sec().querySelectorAll('tr')].find(tr => tr.textContent.includes('smoke-key'));
       const txt = row ? row.textContent.replace(/\s+/g, ' ') : '';
       if (/被上游拒绝/.test(txt)) return '手动停用被显示成被上游拒绝：' + txt;
       return /停用/.test(txt) && /启用/.test(row.querySelector('button[data-togkey]').textContent) ? true : '状态没同步：' + txt;
     })()`, 300);

  await step('渠道对话框：勾上「思考开到最大」真的存下来、页面上也标出来',
    `(async () => {
       const sec = () => [...document.querySelectorAll('#p-pool div.sec')].find(x => x.textContent.includes('__smoke_chan__'));
       sec().querySelector('button[data-editchan]').click();
       await new Promise(r => setTimeout(r, 400));
       const box = document.querySelector('#f-maxthinking');
       if (!box) return '渠道对话框里没有「思考开到最大」';
       if (box.checked) return '新建的渠道不该默认开';
       box.checked = true;
       document.querySelector('#f-save').click();
       await new Promise(r => setTimeout(r, 1200));
       const ch = S.pool.channels.find(c => c.name === '__smoke_chan__');
       if (!ch || !ch.max_thinking) return '勾上之后没存进渠道配置';
       if (!/思考开到最大/.test(sec().textContent)) return '渠道卡片上没有标出「思考开到最大」';
       return true;
     })()`, 300);

  await step('下游 key：一键导入 cc-switch 的链接、可用模型都真的出来',
    `(async () => {
       // 临时造一条可用的模型（渠道 + key + 条目），好让「可用模型」有内容
       const ch = await post('/admin/api/channels', { name: '__smoke_model_chan__', protocol: 'openai_chat',
         base_url: 'http://127.0.0.1:9/v1', enabled: true });
       await post('/admin/api/channels/' + ch.id + '/keys', { label: 'k', api_key: 'sk-smoke-model-key' });
       await post('/admin/api/channels/' + ch.id + '/entries', { upstream_model: '__smoke_model__',
         tier: 1, max_context: 200000, vision: true, pdf: false, enabled: true });
       const dk = await post('/admin/api/downstream_keys', { name: '__smoke_dkey__', enabled: true });

       // 对外模型列表：自动路由的名字排第一，池子里能用的模型都在
       const models = await (await fetch('/v1/models', { headers: { 'x-api-key': dk.key } })).json();
       const ids = models.data.map(m => m.id);
       if (ids[0] !== 'ufp') return '第一个应该是对外那个自动路由的名字：' + ids.join();
       if (!ids.includes('__smoke_model__')) return '池子里的模型没出现在列表里：' + ids.join();

       await go('downstream');
       await new Promise(r => setTimeout(r, 500));
       const page = document.querySelector('#p-downstream');
       if (!page.textContent.includes('__smoke_model__')) return '「可用模型」一段里没有这个模型';
       const row = [...page.querySelectorAll('tr')].find(tr => tr.textContent.includes('__smoke_dkey__'));
       if (!row) return '新建的下游 key 没出现在表里';
       const btn = row.querySelector('button[data-import]');
       if (!btn) return '这把 key 上没有「导入 cc-switch」按钮';
       btn.click();
       await new Promise(r => setTimeout(r, 900));
       const a = document.querySelector('#dlg a.btn');
       const href = a ? a.getAttribute('href') : '';
       const text = document.querySelector('#dlg').textContent;
       if (!href.startsWith('ccswitch://v1/import?resource=provider&app=claude&')) return '链接不对：' + href;
       if (!href.includes('apiKey=' + dk.key)) return '链接里没有这把 key：' + href;
       if (!/ANTHROPIC_BASE_URL/.test(text)) return '对话框里没有可手动填的那两行';
       document.querySelector('#dlg').close();

       // 自己造的这些临时东西自己收拾掉
       await api('/admin/api/downstream_keys/' + dk.id, { method: 'DELETE' });
       await api('/admin/api/channels/' + ch.id, { method: 'DELETE' });
       return true;
     })()`, 400);

  await step('条目总表：所有渠道的条目铺在一张表里，层内不排序',
    `(async () => {
       const ch = await post('/admin/api/channels', { name: '__smoke_ord_chan__', protocol: 'openai_chat',
         base_url: 'http://127.0.0.1:9/v1', enabled: true });
       await post('/admin/api/channels/' + ch.id + '/keys', { label: 'k', api_key: 'sk-smoke-ord-key' });
       for (const [m, t] of [['__smoke_ord_a__', 1], ['__smoke_ord_b__', 1], ['__smoke_ord_c__', 5]]) {
         await post('/admin/api/channels/' + ch.id + '/entries', { upstream_model: m, tier: t,
           max_context: 200000, vision: true, pdf: false, enabled: true });
       }
       await go('order');
       await new Promise(r => setTimeout(r, 600));
       const page = document.querySelector('#p-order');
       const bands = [...page.querySelectorAll('tr.band')];
       if (bands.length < 2) return '只有 ' + bands.length + ' 条层分隔带，tier 1 与 tier 5 该分成两层';
       if (!/第 1 层/.test(bands[0].textContent)) return '第一条分隔带不是第 1 层：' + bands[0].textContent;
       if (!/层内按会话哈希分摊/.test(bands[0].textContent)) return '分隔带上没写清层内不排序';
       const rows = [...page.querySelectorAll('tr[data-id]')].filter(tr => /__smoke_ord_/.test(tr.textContent));
       if (rows.length !== 3) return '表里只有 ' + rows.length + ' 行我们造的条目';
       if (!rows[0].querySelector('td.grip[draggable]')) return '行首没有可拖的手柄';
       const all = [...page.querySelectorAll('tr[data-id]')];
       if (!all[0].querySelector('button[data-up]').disabled) return '第一行的「上移」不该是可点的';
       if (!all[all.length - 1].querySelector('button[data-down]').disabled) return '最后一行的「下移」不该是可点的';
       if (rows.find(tr => tr.textContent.includes('__smoke_ord_b__')).querySelector('td.num').textContent.trim() !== '1') {
         return '层级列没显示原始 tier';
       }
       return true;
     })()`, 300);

  await step('条目总表：搜索过滤，并且过滤时不给改顺序',
    `(async () => {
       const inp = document.querySelector('#order-q');
       if (!inp) return '总表页没有搜索框';
       inp.focus();  // 直接赋 .value 不会聚焦，那样测不到「边打边筛焦点不丢」
       inp.value = '__smoke_ord_b__';
       inp.dispatchEvent(new Event('input'));
       await new Promise(r => setTimeout(r, 300));
       const page = document.querySelector('#p-order');
       const rows = [...page.querySelectorAll('tr[data-id]')];
       if (rows.length !== 1) return '筛出 ' + rows.length + ' 行，应该只有 1 行';
       if (!/__smoke_ord_b__/.test(rows[0].textContent)) return '筛出来的不是那一条';
       if (rows[0].querySelector('td.grip[draggable]')) return '过滤状态下还能拖';
       if (!rows[0].querySelector('button[data-up]').disabled) return '过滤状态下还能上移';
       if (!/不能调整顺序/.test(page.textContent)) return '没提示为什么这时候不能排';
       if (document.activeElement !== inp) return '输入框被重画掉了，焦点丢了';
       inp.value = '';
       inp.dispatchEvent(new Event('input'));
       await new Promise(r => setTimeout(r, 300));
       return true;
     })()`, 300);

  await step('条目总表：上移一层，层级真的改了、空层自动消失',
    `(async () => {
       const rowOf = (m) => [...document.querySelectorAll('#p-order tr[data-id]')]
         .find(tr => tr.textContent.includes(m));
       const c = rowOf('__smoke_ord_c__');
       if (!c) return '表里找不到这一条';
       c.querySelector('button[data-up]').click();
       await new Promise(r => setTimeout(r, 1600));
       const after = rowOf('__smoke_ord_c__');
       if (!after) return '挪完这一行不见了';
       // 它搬进了唯一的那一层，于是三层并成一层，层级一起变成 10
       if (after.querySelector('td.num').textContent.trim() !== '10') return '上移一层后层级不是 10';
       const bands = [...document.querySelectorAll('#p-order tr.band')];
       if (bands.length !== 1) return '搬空的那层没消失，还剩 ' + bands.length + ' 层';
       if (!/3 条/.test(bands[0].textContent)) return '第 1 层没收成 3 条：' + bands[0].textContent;
       if (S.pool.entries.find(e => e.upstream_model === '__smoke_ord_c__').tier !== 10) return '共享状态没刷新';
       return true;
     })()`, 300);

  await step('条目总表：插入新层——勾哪几个就成哪一层',
    `(async () => {
       await go('order');
       await new Promise(r => setTimeout(r, 500));
       document.querySelector('#p-order tr.band button[data-addlayer]').click();
       await new Promise(r => setTimeout(r, 500));
       const dlg = document.querySelector('#dlg');
       if (!dlg.open) return '「插入新层」没打开对话框';
       if (!/插入新层/.test(dlg.querySelector('h2').textContent)) return '对话框标题不对';
       const label = [...dlg.querySelectorAll('.models label')].find(l => l.textContent.includes('__smoke_ord_b__'));
       if (!label) return '勾选列表里没有这一条';
       label.querySelector('input').checked = true;
       document.querySelector('#f-save').click();
       await new Promise(r => setTimeout(r, 1800));
       if (document.querySelector('#dlg').open) return '保存没成功，对话框还开着';
       const bands = [...document.querySelectorAll('#p-order tr.band')];
       if (bands.length !== 2) return '插入后有 ' + bands.length + ' 层，应该是 2 层';
       if (!/2 条/.test(bands[0].textContent)) return '第 1 层不是 2 条：' + bands[0].textContent;
       if (!/1 条/.test(bands[1].textContent)) return '第 2 层不是 1 条：' + bands[1].textContent;
       if (S.pool.entries.find(e => e.upstream_model === '__smoke_ord_b__').tier !== 20) return '勾的那条没换层';
       return true;
     })()`, 300);

  // 窄屏（<860px）表格会退化成键值行：每个 td 靠 data-k 显示列名。分隔带与手柄
  // 是新加的，得确认它们在这套布局下不塌——手柄是鼠标手势，触屏上本来就使不上。
  await send('Emulation.setDeviceMetricsOverride', { width: 420, height: 860, deviceScaleFactor: 1, mobile: false });
  await step('窄屏：总表退化成键值行，分隔带还在、拖拽手柄收起来',
    `(async () => {
       await go('order');
       await new Promise(r => setTimeout(r, 700));
       const band = document.querySelector('#p-order tr.band');
       if (!band) return '分隔带没了';
       if (getComputedStyle(band).display !== 'block') return '分隔带在窄屏下没退成块';
       const row = [...document.querySelectorAll('#p-order tr[data-id]')]
         .find(tr => tr.textContent.includes('__smoke_ord_b__'));
       if (!row) return '表里找不到这一条';
       if (getComputedStyle(row.querySelector('td.grip')).display !== 'none') return '窄屏下还露着拖拽手柄';
       const tier = row.querySelector('td.num');
       if (!/层级/.test(getComputedStyle(tier, '::before').content)) return '层级格没带列名，会认不出是数字';
       if (row.getBoundingClientRect().width > 420) return '行宽超出视口，窄屏上会横向滚动';
       return true;
     })()`, 300);
  await send('Emulation.setDeviceMetricsOverride', { width: 1280, height: 860, deviceScaleFactor: 1, mobile: false });

  await step('条目总表：临时渠道自己清理干净',
    `(async () => {
       const ch = S.pool.channels.find(c => c.name === '__smoke_ord_chan__');
       if (!ch) return '临时渠道不见了（可能已经被别的步骤删了）';
       await api('/admin/api/channels/' + ch.id, { method: 'DELETE' });
       await go('order');
       await new Promise(r => setTimeout(r, 500));
       if (/__smoke_ord_/.test(document.querySelector('#p-order').textContent)) return '清理后表里还有残留';
       return true;
     })()`, 300);

  await step('搜索页：顺序就是尝试顺序，↑↓ 改完就存进库里',
    `(async () => {
       for (const n of ['__smoke_s甲__', '__smoke_s乙__', '__smoke_s丙__']) {
         await post('/admin/api/search_backends', { name: n, kind: 'jina', api_key: '',
           base_url: '', enabled: true, notes: '' });
       }
       await go('search');
       await new Promise(r => setTimeout(r, 700));
       const page = document.querySelector('#p-search');
       const rows = () => [...page.querySelectorAll('tr[data-id]')].filter(tr => /__smoke_s/.test(tr.textContent));
       const names = () => rows().map(tr => tr.querySelector('td[data-k="名字"]').textContent);
       if (rows().length !== 3) return '表里只有 ' + rows().length + ' 行我们造的后端';
       if (names().join() !== '__smoke_s甲__,__smoke_s乙__,__smoke_s丙__') return '初始顺序不对：' + names().join();
       if (!/从上到下就是尝试顺序/.test(page.textContent)) return '页面上没写清这个顺序是尝试顺序';
       if (!rows()[0].querySelector('td.grip[draggable]')) return '行首没有可拖的手柄';
       if (!rows()[0].querySelector('button[data-up]').disabled) return '第一行的「上移」不该可点';
       if (!rows()[2].querySelector('button[data-down]').disabled) return '最后一行的「下移」不该可点';

       rows()[0].querySelector('button[data-down]').click();
       await new Promise(r => setTimeout(r, 1600));
       if (names().join() !== '__smoke_s乙__,__smoke_s甲__,__smoke_s丙__') return '下移之后顺序不对：' + names().join();
       const saved = (await api('/admin/api/search_backends')).filter(r => /__smoke_s/.test(r.name)).map(r => r.name);
       if (saved.join() !== '__smoke_s乙__,__smoke_s甲__,__smoke_s丙__') return '没有存进库里：' + saved.join();

       for (const r of (await api('/admin/api/search_backends')).filter(r => /__smoke_s/.test(r.name))) {
         await api('/admin/api/search_backends/' + r.id, { method: 'DELETE' });
       }
       return true;
     })()`, 300);

  await step('矫正规则页：下拉框选分析条目真的存进设置、停用渠道会标红',
    `(async () => {
       const ch = await post('/admin/api/channels', { name: '__smoke_ana_chan__', protocol: 'openai_chat',
         base_url: 'http://127.0.0.1:9/v1', enabled: true });
       await post('/admin/api/channels/' + ch.id + '/keys', { label: 'k', api_key: 'sk-smoke-ana-key' });
       const en = await post('/admin/api/channels/' + ch.id + '/entries', { upstream_model: '__smoke_ana__',
         tier: 9, max_context: 200000, vision: true, pdf: false, enabled: true });
       const pick = async (value) => {
         await go('rules');
         await new Promise(r => setTimeout(r, 400));
         const sel = document.querySelector('#ana');
         if (!sel) return '矫正规则页没有分析条目下拉框';
         sel.value = value;
         sel.dispatchEvent(new Event('change'));
         await new Promise(r => setTimeout(r, 1200));
         return '';
       };
       let err = await pick(String(en.id));
       if (err) return err;
       if ((await api('/admin/api/settings')).analysisEntryId !== en.id) return '选了没存进设置';
       if (document.querySelector('#p-rules .tag.fail')) return '可用的条目不该标红';
       await post('/admin/api/channels/' + ch.id, { ...S.pool.channels.find(c => c.id === ch.id), enabled: false }, 'PATCH');
       await go('rules');
       await new Promise(r => setTimeout(r, 400));
       if (!/渠道已停用/.test(document.querySelector('#p-rules .tag.fail')?.textContent || '')) return '渠道停用后没有提示';
       err = await pick('');
       if (err) return err;
       if ((await api('/admin/api/settings')).analysisEntryId !== null) return '选「不启用」没清掉设置';
       await api('/admin/api/channels/' + ch.id, { method: 'DELETE' });
       return true;
     })()`, 300);

  await step('对话框：删除渠道（自己清理干净）', 
    `(async () => {
       const b = [...document.querySelectorAll('button[data-delchan]')]
         .find(x => x.closest('div.sec').textContent.includes('__smoke_chan__'));
       if (!b) return '找不到删除按钮';
       b.click();
       await new Promise(r => setTimeout(r, 1500));
       if (S.pool.channels.some(c => c.name === '__smoke_chan__')) return '渠道没删掉';
       return true;
     })()`, 300);

} else {
  console.log('  · 跳过写操作（设置回存、新建渠道 / 加 key / 停用 / 删除）—— 只读模式');
}

await step('退出登录：控制台真的消失，数据不留在屏幕上',
  `(async () => {
     document.querySelector('#logout').click();
     await new Promise(r => setTimeout(r, 1200));
     const shell = document.querySelector('.shell'), gate = document.querySelector('#gate');
     if (getComputedStyle(shell).display !== 'none') return '控制台还看得见';
     if (getComputedStyle(gate).display === 'none') return '登录框没出来';
     if (document.querySelector('#rail').textContent.trim()) return '池子的内容还留在屏幕上';
     return true;
   })()`, 300);

await step('重新登录后照旧可用',
  `(async () => {
     document.querySelector('#pw').value = ${JSON.stringify(PW)};
     document.querySelector('#do-login').click();
     await new Promise(r => setTimeout(r, 1800));
     if (getComputedStyle(document.querySelector('.shell')).display === 'none') return '控制台没回来';
     if (!document.querySelector('#rail').textContent.trim()) return '机架是空的';
     return true;
   })()`, 300);

if (problems.length) {
  fail('页面控制台有报错：\n  ' + problems.join('\n  '));
}

console.log(`\n✓ 后台冒烟测试通过：登录、${PAGES_COUNT} 个页面${WRITES ? '、写操作、对话框' : '（只读，未做写操作）'}都正常，控制台无异常。`);
ws.close();
child.kill('SIGKILL');
process.exit(0);
