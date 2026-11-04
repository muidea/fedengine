#!/usr/bin/env node
// Real Chromium + isolated FedEngine. Node >= 22, no npm dependencies.
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { once } from 'node:events';
import { mkdtemp, readFile, writeFile, rm, readdir } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { dirname, resolve, join } from 'node:path';
import { parseArgs } from 'node:util';

const { values } = parseArgs({ options: {
  binary: { type: 'string', default: 'target/debug/fedengine' },
  chrome: { type: 'string', default: '/opt/google/chrome/chrome' },
  screenshot: { type: 'string' },
} });
const temporary = await mkdtemp(join(tmpdir(), 'fedengine-web-'));
const processes = [];
const sleep = milliseconds => new Promise(resolve => setTimeout(resolve, milliseconds));
async function until(fn, message, timeout = 15000) {
  const deadline = Date.now() + timeout;
  while (Date.now() < deadline) {
    const value = await fn();
    if (value) return value;
    await sleep(80);
  }
  throw new Error(`Timed out: ${message}`);
}
function launch(command, args, options) {
  const child = spawn(command, args, { stdio: ['ignore', 'pipe', 'pipe'], ...options });
  child.output = ''; child.failure = null;
  child.on('error', error => { child.failure = error; });
  for (const stream of [child.stdout, child.stderr]) stream.on('data', data => { child.output = (child.output + data).slice(-100000); });
  processes.push(child); return child;
}
function running(child) {
  if (child.failure) throw child.failure;
  if (child.exitCode !== null) throw new Error(`Process exited: ${child.output}`);
}
class DevTools {
  constructor(socket) {
    this.socket = socket; this.nextId = 0; this.pending = new Map(); this.listeners = new Map();
    socket.addEventListener('message', event => {
      const message = JSON.parse(event.data);
      if (message.id) {
        const task = this.pending.get(message.id);
        if (task) { clearTimeout(task.timer); this.pending.delete(message.id); message.error ? task.reject(new Error(JSON.stringify(message.error))) : task.resolve(message.result); }
      } else for (const fn of this.listeners.get(message.method) ?? []) fn(message.params);
    });
  }
  send(method, params = {}) {
    return new Promise((resolve, reject) => {
      const id = ++this.nextId;
      const timer = setTimeout(() => { this.pending.delete(id); reject(new Error(`CDP timeout: ${method}`)); }, 15000);
      this.pending.set(id, { resolve, reject, timer }); this.socket.send(JSON.stringify({ id, method, params }));
    });
  }
  on(method, callback) { this.listeners.set(method, [...this.listeners.get(method) ?? [], callback]); }
  async evaluate(expression) {
    const result = await this.send('Runtime.evaluate', { expression, awaitPromise: true, returnByValue: true, userGesture: true });
    if (result.exceptionDetails) throw new Error(result.exceptionDetails.exception?.description ?? result.exceptionDetails.text);
    return result.result.value;
  }
}

let cdp;
try {
  const binary = resolve(values.binary);
  let config = await readFile('config/application.toml', 'utf8');
  config = config.replaceAll('listen = "127.0.0.1:15432"', 'listen = "127.0.0.1:0"')
    .replaceAll('listen = "127.0.0.1:15433"', 'listen = "127.0.0.1:0"')
    .replace('database_path = "./data/fedengine.duckdb"', `database_path = "${temporary}/management.duckdb"`)
    .replace('temp_directory = "./data/tmp"', `temp_directory = "${temporary}/tmp"`)
    .replace('credentials_directory = "./secrets"', `credentials_directory = "${temporary}"`);
  if (process.env.FEDENGINE_WEB_POSTGRES_DSN) config = config.replace('[duckdb]', `[duckdb]\nextension_directory = "${resolve('data/extensions')}"`);
  await writeFile(join(temporary, 'application.toml'), config);
  const env = { ...process.env, FEDENGINE_PASSWORD: 'disposable-web-smoke', RUST_LOG: 'fedengine=info', NO_COLOR: '1' };
  const libraries = resolve(dirname(binary), '../duckdb-download');
  for (const triple of await readdir(libraries)) {
    env.LD_LIBRARY_PATH = `${join(libraries, triple, 'v2.0.0')}:${process.env.LD_LIBRARY_PATH ?? ''}`;
    break;
  }
  const engine = launch(binary, ['--config', join(temporary, 'application.toml')], { env });
  const address = await until(() => {
    running(engine); return /management_address=Some\((127\.0\.0\.1:\d+)\)/u.exec(engine.output)?.[1];
  }, 'FedEngine ready');
  const origin = `http://${address}`;
  async function api(path, method = 'GET', body = null) {
    const response = await fetch(`${origin}/api/v1/${path}`, {
      method, headers: { 'Content-Type': 'application/json', 'Idempotency-Key': `smoke-${crypto.randomUUID()}` },
      ...(body ? { body: JSON.stringify(body) } : {}),
    });
    const value = await response.json(); assert(response.ok, JSON.stringify(value)); return value;
  }
  async function finished(operation) {
    return until(async () => {
      const current = await api(`operations/${operation.id}`);
      return ['Succeeded', 'Failed', 'Interrupted'].includes(current.status) && current;
    }, 'operation completion');
  }
  const browser = launch(values.chrome, ['--headless=new', '--no-sandbox', '--disable-gpu', '--no-first-run', '--no-default-browser-check',
    '--remote-debugging-address=127.0.0.1', '--remote-debugging-port=0', `--user-data-dir=${temporary}/chrome`, 'about:blank']);
  const endpoint = await until(() => { running(browser); return /DevTools listening on (ws:\/\/[^\s]+)/u.exec(browser.output)?.[1]; }, 'Chromium ready');
  const browserOrigin = new URL(endpoint).origin.replace('ws:', 'http:');
  const targets = await (await fetch(`${browserOrigin}/json/list`)).json();
  const socket = new WebSocket(targets.find(target => target.type === 'page').webSocketDebuggerUrl);
  await once(socket, 'open'); cdp = new DevTools(socket);
  const exceptions = [], writes = [], blocked = [];
  cdp.on('Runtime.exceptionThrown', event => exceptions.push(event.exceptionDetails.exception?.description ?? event.exceptionDetails.text));
  cdp.on('Network.requestWillBeSent', event => {
    if (event.request.url.startsWith(origin) && event.request.method !== 'GET') writes.push(event.request);
  });
  cdp.on('Log.entryAdded', event => { if (event.entry.text.includes('Content Security Policy')) blocked.push(event.entry.text); });
  await cdp.send('Page.enable'); await cdp.send('Runtime.enable'); await cdp.send('Network.enable'); await cdp.send('Log.enable');
  await cdp.send('Emulation.setDeviceMetricsOverride', { width: 1440, height: 1000, deviceScaleFactor: 1, mobile: false });
  await cdp.send('Page.navigate', { url: `${origin}/manage/` });
  const evaluate = expression => cdp.evaluate(expression);
  async function waitText(text) { await until(() => evaluate(`document.body.innerText.includes(${JSON.stringify(text)})`), text); }
  async function click(text, scope = 'document') {
    await until(() => evaluate(`[...${scope}.querySelectorAll('button')].some(node => node.textContent === ${JSON.stringify(text)} && !node.disabled && !node.hidden)`), `button enabled: ${text}`);
    await evaluate(`(() => { const node = [...${scope}.querySelectorAll('button')].find(node => node.textContent === ${JSON.stringify(text)} && !node.disabled && !node.hidden); if (!node) throw new Error('Button unavailable: ' + ${JSON.stringify(text)}); node.click(); })()`);
  }
  async function fill(name, value) {
    await evaluate(`(() => { const node = document.querySelector(${JSON.stringify(name)}); if (!node) throw new Error('Missing input'); node.value = ${JSON.stringify(value)}; node.dispatchEvent(new Event('input', {bubbles:true})); node.dispatchEvent(new Event('change', {bubbles:true})); })()`);
  }
  async function submit() { await evaluate(`document.querySelector('#editor').requestSubmit()`); }
  async function closed() { await until(() => evaluate(`!document.querySelector('#dialog').open`), 'dialog closed'); }
  async function sourceState(predicate) { return until(async () => { const source = await api('sources/web_pg'); return predicate(source) && source; }, 'source state'); }
  async function settleSource() {
    await until(() => evaluate(`document.querySelector('#pending').hidden`), 'request accepted');
    await sourceState(source => !source.current_operation);
    await click('↻ 刷新状态'); await sleep(200);
  }
  await waitText('管理服务已连接');
  await waitText('尚未登记数据源');
  await click('＋ 登记数据源');
  await fill('[name=id]', 'web_pg'); await fill('[name=catalog_alias]', 'web_pg');
  await fill('[name=secret_ref]', 'postgresql://invalid:placeholder@localhost/example'); await submit();
  await waitText('不能填写连接串或密码');
  assert.equal(writes.length, 0);
  assert.equal(await evaluate('sessionStorage.getItem("fedengine.pending")'), null);
  await fill('[name=secret_ref]', 'file:missing.dsn');
  await submit(); await closed(); await waitText('web_pg');
  await sourceState(source => source.status === 'Registered' && !source.current_operation);
  await click('管理 →'); await waitText('凭据引用');
  await click('探测连接'); await settleSource();
  const probe = await api('sources/web_pg');
  assert.equal(probe.applied_revision, null);
  const probeId = await evaluate('JSON.parse(sessionStorage.getItem("fedengine.operations"))[0]');
  assert.equal((await api(`operations/${probeId}`)).status, 'Failed');
  console.log('Passed: registration, asynchronous tracking and failed credential probe');

  // Lose the PATCH response after the server received it, then reload and retry.
  let dropPatch = true;
  let mockCatalog = false;
  let catalogMode = 'first';
  const catalogRequests = [];
  const interceptErrors = [];
  cdp.on('Fetch.requestPaused', event => {
    (async () => {
      if (event.request.method === 'PATCH' && dropPatch && event.responseStatusCode) {
        dropPatch = false; await cdp.send('Fetch.failRequest', { requestId: event.requestId, errorReason: 'ConnectionReset' }); return;
      }
      if (mockCatalog && event.request.url.includes('/catalog')) {
        catalogRequests.push(new URL(event.request.url));
        const page = {
          snapshot_id: 'test-snapshot', snapshot_version: 'test-snapshot', revision: 1, generation: 1,
          refreshed_at: '2026-01-01T00:00:00Z', stale: false, total: 51, offset: catalogMode === 'first' ? 0 : 50,
          objects: [{ schema: 'public', name: '<img src=x onerror=alert(1)>', object_type: 'table', columns: [{ name: 'id', remote_type: 'integer' }], metadata: {} }],
          next_page_token: catalogMode === 'first' ? 'opaque-test-token' : null,
        };
        const conflict = catalogMode === 'conflict';
        await cdp.send('Fetch.fulfillRequest', { requestId: event.requestId, responseCode: conflict ? 409 : 200,
          responseHeaders: [{ name: 'Content-Type', value: 'application/json' }],
          body: Buffer.from(JSON.stringify(conflict ? { code: '40001', error: 'snapshot changed' } : page)).toString('base64') }); return;
      }
      await cdp.send('Fetch.continueRequest', { requestId: event.requestId });
    })().catch(error => interceptErrors.push(error.message));
  });
  await cdp.send('Fetch.enable', { patterns: [
    { urlPattern: `${origin}/api/v1/sources/web_pg`, requestStage: 'Response' },
    { urlPattern: `${origin}/api/v1/sources/web_pg/catalog*`, requestStage: 'Request' },
  ] });
  await click('凭据引用'); await fill('[name=secret_ref]', 'file:rotated.dsn'); await submit();
  await waitText('有一项请求需要确认结果');
  await sourceState(source => source.desired_revision === 2 && !source.current_operation);
  await cdp.send('Page.reload'); await waitText('有一项请求需要确认结果'); await waitText('管理服务已连接');
  await click('使用原幂等键重试');
  await until(() => evaluate(`document.querySelector('#pending').hidden`), 'pending resolved');
  const patched = await sourceState(source => !source.current_operation);
  assert.equal(patched.desired_revision, 2);
  const patches = writes.filter(request => request.method === 'PATCH');
  assert.equal(patches.length, 2);
  const key = request => Object.entries(request.headers).find(([name]) => name.toLowerCase() === 'idempotency-key')?.[1];
  assert.equal(key(patches[0]), key(patches[1])); assert.equal(patches[0].postData, patches[1].postData);
  console.log('Passed: lost response, reload recovery and identical idempotent retry');

  await click('管理 →'); await click('纳管范围'); await fill('[name=default_schema]', '');
  await submit(); await closed(); await settleSource();
  const schemas = await api('sources/web_pg');
  assert.equal(schemas.desired.definition.default_schema, '');
  assert.equal(schemas.desired_revision, 3);
  // An external change invalidates the open form; the UI must not silently rebase it.
  await click('凭据引用');
  const external = await api('sources/web_pg', 'PATCH', {
    target: { incarnation: schemas.incarnation, expected_revision: schemas.desired_revision }, secret_ref: 'file:external.dsn',
  });
  assert.equal((await finished(external)).status, 'Succeeded');
  await fill('[name=secret_ref]', 'file:stale-form.dsn'); await submit(); await waitText('页面版本已过期');
  assert.equal((await api('sources/web_pg')).desired.definition.secret_ref, 'file:external.dsn');
  await click('取消', 'document.querySelector("#dialog")');
  console.log('Passed: schema configuration, explicit default clearing and stale-form conflict');

  await click('↻ 刷新状态'); await sleep(200);
  await click('查询目录'); await waitText('目录读取失败');
  mockCatalog = true; await click('查询目录'); await waitText('共 51 项');
  assert.equal(await evaluate('document.querySelectorAll("#catalog img").length'), 0);
  await click('查看事实'); await waitText('remote_type'); await click('取消', 'document.querySelector("#dialog")');
  catalogMode = 'second'; await click('下一页'); await waitText('第 2 页');
  assert.equal(catalogRequests.at(-1).searchParams.get('page_token'), 'opaque-test-token');
  catalogMode = 'conflict'; await click('上一页'); await waitText('目录快照已变化');
  catalogMode = 'first'; await click('从第一页重新读取'); await waitText('第 1 页');
  assert.equal(catalogRequests.at(-1).searchParams.has('page_token'), false);
  mockCatalog = false; await cdp.send('Fetch.disable');
  console.log('Passed: catalog error, pagination, snapshot conflict and untrusted text rendering (synthetic HTTP facts)');

  await evaluate('document.querySelector("[data-page=databases]").click()');
  await click('＋ 创建业务库'); await fill('[name=name]', 'web_account'); await fill('[name=owner]', 'fedengine');
  await click('＋ 添加映射'); await fill('.mapping-name', 'business'); await fill('.mapping-source', 'web_pg'); await fill('.mapping-remote', 'public');
  await submit(); await closed(); await waitText('web_account');
  await click('编辑定义与映射'); await click('＋ 添加映射');
  await evaluate(`(() => { const row = document.querySelectorAll('.mapping-row')[1]; row.querySelector('.mapping-name').value='second'; row.querySelector('.mapping-source').value='web_pg'; row.querySelector('.mapping-remote').value='public'; })()`);
  await submit(); await closed();
  const database = await api('virtual-databases/web_account');
  assert.deepEqual(database.config.schemas.map(schema => schema.name), ['business', 'second']);
  assert.equal(database.revision, 2);
  await click('删除'); await click('确认删除业务库', 'document.querySelector("#dialog")'); await closed(); await waitText('尚未创建业务虚拟库');
  console.log('Passed: virtual database creation, full mapping preservation and deletion');

  await evaluate('document.querySelector("[data-page=sources]").click()'); await click('管理 →');
  await click('停用'); await click('确认停用', 'document.querySelector("#dialog")'); await closed(); await settleSource();
  await sourceState(source => source.status === 'Disabled');
  await click('删除登记'); await click('确认删除登记', 'document.querySelector("#dialog")'); await closed();
  await until(async () => (await api('sources')).length === 0, 'source deleted');
  await evaluate('document.querySelector("[data-page=operations]").click()'); await waitText('操作记录');
  assert.equal(await evaluate('document.querySelectorAll("#content tbody tr").length > 0'), true);
  await fill('[aria-label="操作编号"]', 'partial-operation-id'); await sleep(2800);
  assert.equal(await evaluate('document.querySelector("[aria-label=操作编号]").value'), 'partial-operation-id');
  assert.deepEqual(exceptions, []); assert.deepEqual(blocked, []); assert.deepEqual(interceptErrors, []);
  await cdp.send('Emulation.setDeviceMetricsOverride', { width: 390, height: 844, deviceScaleFactor: 1, mobile: true });
  assert.equal(await evaluate('document.documentElement.scrollWidth <= window.innerWidth'), true);
  console.log('Passed: source disable/delete, operation history, responsive layout and browser runtime/CSP');

  // Optional read-only lifecycle against an explicitly supplied PostgreSQL source.
  if (process.env.FEDENGINE_WEB_POSTGRES_DSN) {
    const schema = process.env.FEDENGINE_WEB_POSTGRES_SCHEMA ?? 'public';
    await cdp.send('Emulation.setDeviceMetricsOverride', { width: 1440, height: 1000, deviceScaleFactor: 1, mobile: false });
    await evaluate('document.querySelector("[data-page=sources]").click()');
    await click('＋ 登记数据源');
    await fill('[name=id]', 'web_live'); await fill('[name=catalog_alias]', 'web_live');
    await fill('[name=secret_ref]', 'FEDENGINE_WEB_POSTGRES_DSN');
    await fill('[name=schemas]', schema); await fill('[name=default_schema]', schema);
    await submit(); await closed(); await waitText('web_live'); await click('管理 →');
    async function completeLive(action) {
      await click(action);
      await until(() => evaluate('document.querySelector("#pending").hidden'), 'live source accepted');
      const source = await until(async () => { const source = await api('sources/web_live'); return !source.current_operation && source; }, 'live source completion');
      const id = await evaluate('JSON.parse(sessionStorage.getItem("fedengine.operations"))[0]');
      const operation = await api(`operations/${id}`);
      assert.equal(operation.status, 'Succeeded', operation.error ?? 'live operation');
      await click('↻ 刷新状态'); return source;
    }
    await completeLive('探测连接'); await completeLive('发现 Schema');
    assert.equal((await completeLive('激活 / 应用')).status, 'Active');
    await completeLive('刷新元数据');
    await click('查询目录'); await waitText('共 ');
    const catalog = await api('sources/web_live/catalog?limit=50');
    assert(catalog.total > 0, 'optional source schema must contain at least one table or view');
    await waitText(catalog.objects[0].name);
    if (values.screenshot) {
      const screenshot = await cdp.send('Page.captureScreenshot', { format: 'png' });
      await writeFile(values.screenshot, Buffer.from(screenshot.data, 'base64'));
    }
    await click('停用'); await click('确认停用', 'document.querySelector("#dialog")'); await closed();
    await until(async () => (await api('sources/web_live')).status === 'Disabled', 'live source disabled');
    await click('↻ 刷新状态'); await click('删除登记'); await click('确认删除登记', 'document.querySelector("#dialog")'); await closed();
    await until(async () => (await api('sources')).length === 0, 'live source deleted');
    console.log('Passed: real PostgreSQL probe, discover, activation, refresh, metadata and disable/delete (read-only)');
  }
  if (values.screenshot && !process.env.FEDENGINE_WEB_POSTGRES_DSN) {
    await cdp.send('Emulation.setDeviceMetricsOverride', { width: 1440, height: 1000, deviceScaleFactor: 1, mobile: false });
    const screenshot = await cdp.send('Page.captureScreenshot', { format: 'png' });
    await writeFile(values.screenshot, Buffer.from(screenshot.data, 'base64'));
  }
  engine.kill('SIGTERM'); await once(engine, 'exit');
  assert.equal(engine.exitCode, 0); assert.match(engine.output, /GracefullyStopped/u);
  console.log('Passed: isolated process checked drain');
} finally {
  try { await cdp?.send('Browser.close'); } catch { /* Browser may already have exited. */ }
  cdp?.socket.close();
  for (const child of processes.reverse()) {
    if (child.exitCode === null && child.signalCode === null) {
      child.kill('SIGTERM');
      await Promise.race([once(child, 'exit'), sleep(5000)]);
      if (child.exitCode === null && child.signalCode === null) { child.kill('SIGKILL'); await once(child, 'exit'); }
    }
  }
  await rm(temporary, { recursive: true, force: true, maxRetries: 10, retryDelay: 200 });
}
