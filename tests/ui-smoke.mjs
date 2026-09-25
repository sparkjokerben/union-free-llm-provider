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

await step('外壳渲染：机架 + 读数带 + 九个页签',
  `document.querySelector('#rail').innerHTML.length > 20 &&
   document.querySelector('#readout').textContent.trim().length > 0 &&
   document.querySelectorAll('#pages button').length === 9`, 200);

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

console.log(`\n✓ 后台冒烟测试通过：登录、九个页面${WRITES ? '、写操作、对话框' : '（只读，未做写操作）'}都正常，控制台无异常。`);
ws.close();
child.kill('SIGKILL');
process.exit(0);
