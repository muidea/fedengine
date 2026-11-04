const $ = (selector, root = document) => root.querySelector(selector);
const apiBase = '/api/v1';
const terminal = new Set(['Succeeded', 'Failed', 'Interrupted']);
const labels = {
  Registered: '已登记', Activating: '激活中', Active: '运行中', Draining: '排空中',
  Disabled: '已停用', MetadataStale: '元数据过期', Degraded: '异常',
  Pending: '排队中', Running: '执行中', WaitingForDrain: '等待排空',
  Succeeded: '成功', Failed: '失败', Interrupted: '已中断',
};
const actionLabels = {
  register: '登记', probe: '探测连接', discover: '发现 Schema', schemas: '保存纳管范围',
  apply: '激活 / 应用', refresh: '刷新元数据', disable: '停用', delete: '删除登记', patch: '更新凭据引用',
};
function readSession(key, fallback) {
  try { return JSON.parse(sessionStorage.getItem(key)) ?? fallback; } catch { return fallback; }
}
function saveSession(key, value) {
  try { sessionStorage.setItem(key, JSON.stringify(value)); } catch { /* In-memory retry remains available. */ }
}
const state = {
  sources: [], databases: [], operations: new Map(), page: 'sources', selected: null, search: '',
  operationIds: readSession('fedengine.operations', []).slice(0, 40),
  pending: readSession('fedengine.pending', null), writing: false, loading: false,
  initialized: false, sourceSignature: '', databaseSignature: '',
};

// All remote strings are rendered as text nodes, including errors and metadata.
function el(tag, attrs = {}, ...children) {
  const node = document.createElement(tag);
  for (const [key, value] of Object.entries(attrs)) {
    if (key.startsWith('on')) node.addEventListener(key.slice(2).toLowerCase(), value);
    else if (key === 'class') node.className = value;
    else if (key === 'checked' || key === 'disabled' || key === 'hidden') node[key] = Boolean(value);
    else if (value !== null && value !== undefined) node.setAttribute(key, value);
  }
  for (const child of children.flat(Infinity)) {
    if (child !== null && child !== undefined) node.append(child instanceof Node ? child : document.createTextNode(String(child)));
  }
  return node;
}
function replace(node, ...children) { node.replaceChildren(...children); }
function guard(fn) {
  return (...args) => {
    try { return Promise.resolve(fn(...args)).catch(error => toast(error.message, true)); }
    catch (error) { toast(error.message, true); }
  };
}
function button(text, callback, cls = '', writing = false, disabled = false) {
  return el('button', {
    type: 'button', class: cls, onclick: guard(callback),
    'data-write': writing ? 'true' : null,
    disabled: disabled || (writing && (state.writing || Boolean(state.pending))),
  }, text);
}
function badge(status) {
  const cls = ['Active', 'Succeeded'].includes(status) ? 'good'
    : ['Failed', 'Interrupted', 'Degraded', 'MetadataStale'].includes(status) ? 'bad'
    : ['Pending', 'Running', 'Activating', 'Draining', 'WaitingForDrain'].includes(status) ? 'warn' : '';
  return el('span', { class: `badge ${cls}`, title: status }, labels[status] ?? status);
}
function empty(title, description = '') { return el('div', { class: 'empty' }, el('strong', {}, title), description); }
function panel(title, subtitle, ...children) {
  return el('section', { class: 'panel' }, el('div', { class: 'panel-heading' },
    el('div', {}, el('h3', {}, title), subtitle ? el('p', {}, subtitle) : null)), ...children);
}
function table(headers, rows) {
  return el('div', { class: 'table-scroll' }, el('table', {},
    el('thead', {}, el('tr', {}, headers.map(text => el('th', { scope: 'col' }, text)))), el('tbody', {}, rows)));
}
function time(value) { return value ? new Date(value).toLocaleString('zh-CN', { hour12: false }) : '—'; }
function toast(message, error = false) {
  const node = $('#toast');
  node.textContent = message; node.className = error ? 'error' : ''; node.hidden = false;
  clearTimeout(toast.timer); toast.timer = setTimeout(() => { node.hidden = true; }, 6500);
}
async function request(path, options = {}) {
  let response;
  try {
    response = await fetch(`${apiBase}/${path}`, {
      cache: 'no-store', mode: 'same-origin', credentials: 'same-origin',
      signal: AbortSignal.timeout(15000), ...options,
    });
  } catch {
    throw Object.assign(new Error('网络中断或请求超时，操作结果尚未确认。'), { uncertain: true });
  }
  let body;
  try { body = await response.json(); } catch {
    throw Object.assign(new Error(`HTTP ${response.status}：无法读取管理服务响应。`), { uncertain: true });
  }
  if (!response.ok) {
    const uncertain = (response.status >= 500 && !body.code) || body.code === '55P03';
    throw Object.assign(new Error(`${body.error ?? '请求失败'}${body.code ? ` (${body.code})` : ''}`), {
      status: response.status, code: body.code, uncertain,
    });
  }
  return { body, status: response.status };
}
async function get(path) { return (await request(path)).body; }
function sourcePath(id) { return `sources/${encodeURIComponent(id)}`; }
function sourceTarget(source) { return { incarnation: source.incarnation, expected_revision: source.desired_revision }; }
function rememberOperation(id) {
  state.operationIds = [id, ...state.operationIds.filter(other => other !== id)].slice(0, 40);
  saveSession('fedengine.operations', state.operationIds);
}
function syncWriteButtons() {
  document.querySelectorAll('[data-write="true"]').forEach(node => {
    node.disabled = state.writing || Boolean(state.pending) || node.dataset.unavailable === 'true';
  });
}
function renderPending() {
  const node = $('#pending'); node.hidden = !state.pending;
  if (!state.pending) return;
  replace(node, el('strong', {}, '有一项请求需要确认结果'),
    el('div', {}, state.pending.label, ' · 幂等键：', state.pending.key),
    el('div', {}, '请求可能已被服务受理。请使用原请求重试以找回结果；等待排空的操作需先结束相关查询或关闭 Portal。'),
    el('div', { class: 'actions' }, button('使用原幂等键重试', () => sendWrite(state.pending), 'small', false, state.writing)));
}
async function write(method, path, body, label) {
  if (state.writing || state.pending) throw new Error('请先确认上一项请求的结果。');
  const bytes = crypto.getRandomValues(new Uint32Array(4));
  const key = `web-${Array.from(bytes, value => value.toString(16)).join('-')}`;
  const pending = { method, path, body, label, key };
  state.pending = pending; saveSession('fedengine.pending', pending);
  return sendWrite(pending);
}
async function sendWrite(pending) {
  if (state.writing) return;
  state.writing = true; renderPending(); syncWriteButtons();
  try {
    const { body, status } = await request(pending.path, {
      method: pending.method,
      headers: { 'Content-Type': 'application/json', 'Idempotency-Key': pending.key },
      body: JSON.stringify(pending.body),
    });
    if (status === 202) {
      rememberOperation(body.id); state.operations.set(body.id, body);
      toast(`${pending.label}已受理，正在跟踪执行结果。`);
    } else toast(`${pending.label}成功。`);
    state.pending = null; saveSession('fedengine.pending', null);
    if ($('#dialog').open) $('#dialog').close();
    await reload().catch(() => toast('请求结果已确认，但状态刷新失败，请稍后刷新状态。', true));
    return body;
  } catch (error) {
    if (!error.uncertain) {
      state.pending = null; saveSession('fedengine.pending', null);
      if (error.code === '40001') error.message += '；页面版本已过期，请刷新后重新核对并提交。';
    }
    throw error;
  } finally {
    state.writing = false; renderPending(); syncWriteButtons();
  }
}

async function reload() {
  if (state.loading) return;
  state.loading = true;
  try {
    const [sources, databases] = await Promise.all([get('sources'), get('virtual-databases')]);
    state.sources = sources; state.databases = databases;
    const currentIds = sources.map(source => source.current_operation).filter(Boolean);
    for (const id of currentIds) if (!state.operations.has(id)) rememberOperation(id);
    const operationIds = [...new Set([...state.operationIds, ...currentIds])];
    const retained = new Set(operationIds);
    for (const id of state.operations.keys()) if (!retained.has(id)) state.operations.delete(id);
    const results = await Promise.allSettled(operationIds.map(async id => {
      const previous = state.operations.get(id);
      if (previous && terminal.has(previous.status)) return previous;
      return get(`operations/${encodeURIComponent(id)}`);
    }));
    results.forEach((result, index) => {
      if (result.status !== 'fulfilled') return;
      const operation = result.value;
      const previous = state.operations.get(operationIds[index]);
      if (previous && !terminal.has(previous.status) && terminal.has(operation.status)) {
        toast(`${actionLabels[operation.action.action] ?? operation.action.action}：${labels[operation.status]}${operation.error ? ` · ${operation.error}` : ''}`, operation.status !== 'Succeeded');
      }
      state.operations.set(operation.id, operation);
    });
    $('#connection').textContent = '● 管理服务已连接'; $('#connection').className = '';
    const sourceSignature = JSON.stringify(sources), databaseSignature = JSON.stringify(databases);
    const sourcesChanged = state.sourceSignature !== sourceSignature;
    const databasesChanged = state.databaseSignature !== databaseSignature;
    state.sourceSignature = sourceSignature; state.databaseSignature = databaseSignature;
    renderStats();
    if (!state.initialized) { state.initialized = true; renderPage(); }
    else if (state.page === 'sources') {
      if (state.selected && (sourcesChanged || sources.some(source => source.id === state.selected && source.current_operation))) renderSourceOverview();
      else if (!state.selected && sourcesChanged) renderSourceRows();
    } else if (state.page === 'databases' && databasesChanged) renderDatabases();
    if (state.page === 'operations') renderOperations();
  } catch (error) {
    $('#connection').textContent = '● 管理服务暂不可用'; $('#connection').className = 'offline';
    if (!state.initialized) replace($('#content'), empty('无法连接管理服务', '请检查服务状态，然后点击刷新重试。'));
    throw error;
  } finally { state.loading = false; }
}
function renderStats() {
  $('#source-count').textContent = state.sources.length;
  const active = state.sources.filter(source => source.status === 'Active').length;
  const pending = new Set(state.sources.map(source => source.current_operation).filter(Boolean));
  for (const operation of state.operations.values()) if (!terminal.has(operation.status)) pending.add(operation.id);
  const errors = state.sources.filter(source => ['Degraded', 'MetadataStale'].includes(source.status)).length;
  replace($('#stats'), ...[
    ['数据源总数', state.sources.length, '已登记的远端连接', '▤'],
    ['运行中', active, '已激活的业务数据源', '↗'],
    ['业务虚拟库', state.databases.length, 'PostgreSQL 业务访问入口', '▦'],
    ['进行中操作', pending.size, errors ? `${errors} 个源需要关注` : '跟踪已受理的管理操作', '◷'],
  ].map(([label, value, description, icon]) => el('div', { class: 'stat' },
    el('span', { class: 'stat-label' }, label), el('span', { class: 'stat-icon' }, icon),
    el('strong', { class: 'stat-value' }, value), el('small', {}, description))));
}
function navigate(page, selected = null) {
  state.page = page; state.selected = selected; renderPage();
}
function renderPage() {
  const titles = { sources: '数据源', databases: '业务虚拟库', operations: '操作记录' };
  const descriptions = {
    sources: '管理远端连接与纳管范围，让业务数据有序接入。',
    databases: '将多个数据源映射到同一业务库，通过 schema.table 访问。',
    operations: '跟踪异步执行进度，核对结果与失败原因。',
  };
  $('#page-title').textContent = titles[state.page]; $('#breadcrumb').textContent = titles[state.page];
  $('#page-description').textContent = descriptions[state.page];
  document.querySelectorAll('[data-page]').forEach(node => {
    node.classList.toggle('active', node.dataset.page === state.page);
    if (node.dataset.page === state.page) node.setAttribute('aria-current', 'page'); else node.removeAttribute('aria-current');
  });
  const actions = [button('↻ 刷新状态', reload)];
  if (state.page === 'sources') actions.push(button('＋ 登记数据源', registerForm, 'primary', true));
  if (state.page === 'databases') actions.push(button('＋ 创建业务库', () => databaseForm(), 'primary', true));
  replace($('#page-actions'), ...actions);
  if (state.page === 'sources') { if (state.selected) renderSource(); else renderSources(); }
  else if (state.page === 'databases') renderDatabases(); else renderOperations();
  renderPending(); syncWriteButtons();
}
function sourceName(source) {
  return el('div', { class: 'source-name' }, el('span', { class: `db-icon ${source.kind}` }, source.kind === 'postgres' ? 'PG' : 'MY'),
    el('div', {}, button(source.catalog_alias, () => navigate('sources', source.id), 'link name-text'), el('span', { class: 'subtext' }, source.id)));
}
function renderSources() {
  const search = el('input', { type: 'search', placeholder: '搜索名称或数据源 ID…', 'aria-label': '搜索数据源', value: state.search,
    oninput: event => { state.search = event.target.value; renderSourceRows(); } });
  replace($('#content'), el('section', { class: 'panel' },
    el('div', { class: 'panel-heading' }, el('div', {}, el('h3', {}, '数据源列表'), el('p', {}, '先登记并核对连接，再选择 Schema、激活数据源。')),
      el('div', { class: 'toolbar' }, search)), el('div', { id: 'source-rows' })),
    el('p', { class: 'small-note' }, '凭据以引用保存；纳管范围和凭据引用变更需显式应用。源登记不会自动授予账号 SELECT 权限。'));
  renderSourceRows();
}
function renderSourceRows() {
  const container = $('#source-rows'); if (!container) return;
  const sources = state.sources.filter(source => `${source.id} ${source.catalog_alias} ${source.kind}`.toLowerCase().includes(state.search.toLowerCase()));
  replace(container, sources.length ? table(['数据源 / ID', '类型', '状态', '纳管范围', '版本', '操作'], sources.map(source => {
    const bindings = source.desired.bindings.filter(binding => binding.enabled).map(binding => binding.remote_schema);
    return el('tr', {}, el('td', {}, sourceName(source)), el('td', {}, source.kind === 'postgres' ? 'PostgreSQL' : 'MySQL'),
      el('td', {}, badge(source.status), source.current_operation ? el('span', { class: 'subtext' }, '有进行中操作') : null),
      el('td', {}, bindings.join(', ') || '未选择'),
      el('td', {}, `${source.applied_revision ?? '—'} / ${source.desired_revision}`, el('span', { class: 'subtext' }, '生效 / 期望')),
      el('td', {}, button('管理 →', () => navigate('sources', source.id), 'link')));
  })) : empty(state.sources.length ? '没有匹配的数据源' : '尚未登记数据源', '点击右上角“登记数据源”开始接入。'));
}
function renderSource() {
  replace($('#content'), button('← 返回数据源列表', () => navigate('sources'), 'link back'),
    el('div', { id: 'source-overview' }), el('div', { id: 'catalog' }));
  renderSourceOverview(); renderCatalog();
}
function sourceAction(source, action) {
  return write('POST', `${sourcePath(source.id)}/${action}`, { target: sourceTarget(source) }, actionLabels[action]);
}
function renderSourceOverview() {
  const node = $('#source-overview'); if (!node) return;
  const source = state.sources.find(item => item.id === state.selected);
  if (!source) { replace(node, empty('该数据源已不存在', '返回列表查看当前状态。')); return; }
  const definition = source.desired.definition;
  const busy = Boolean(source.current_operation);
  const actionButton = (text, callback, cls = '', unavailable = false) => {
    const node = button(text, callback, cls, true, busy || unavailable);
    node.dataset.unavailable = String(busy || unavailable); return node;
  };
  const details = [
    ['源 ID', source.id], ['业务数据库名', source.catalog_alias], ['类型', source.kind],
    ['凭据引用', definition.secret_ref], ['默认 Schema', definition.default_schema || '未设置'],
    ['生效 / 期望版本', `${source.applied_revision ?? '—'} / ${source.desired_revision}`],
    ['纳管 Schema', source.desired.bindings.map(binding => `${binding.remote_schema}${binding.enabled ? '' : '（停用）'}`).join(', ') || '未选择'],
    ['访问模式', `${definition.read_only ? '只读' : '可写'} · DDL ${definition.ddl_enabled ? '已启用' : '关闭'}`],
    ['目录更新时间', time(source.catalog_updated_at)],
  ];
  const current = source.current_operation ? state.operations.get(source.current_operation) : null;
  replace(node, el('section', { class: 'panel' },
    el('div', { class: 'panel-heading' }, sourceName(source), badge(source.status)),
    el('div', { class: 'panel-body' },
      source.last_error ? el('div', { class: 'notice error' }, source.last_error) : null,
      source.current_operation ? el('div', { class: 'notice warning' },
        `${current ? labels[current.status] ?? current.status : '操作进行中'} · ${current?.phase ?? source.current_operation}`,
        button('查看进度', () => navigate('operations'), 'link')) : null,
      source.applied_revision !== null && source.applied_revision !== source.desired_revision
        ? el('div', { class: 'notice warning' }, '期望配置尚未生效，请核对后执行“激活 / 应用”。') : null,
      el('div', { class: 'details' }, details.map(([name, value]) => el('dl', {}, el('dt', {}, name), el('dd', {}, value)))),
      el('hr'),
      el('div', { class: 'actions' },
        actionButton('探测连接', () => sourceAction(source, 'probe')),
        actionButton('发现 Schema', () => sourceAction(source, 'discover')),
        actionButton('纳管范围', () => schemaForm(source)),
        actionButton('凭据引用', () => credentialForm(source)),
        actionButton('激活 / 应用', () => sourceAction(source, 'apply'), 'primary'),
        actionButton('刷新元数据', () => sourceAction(source, 'refresh'), '', !['Active', 'MetadataStale'].includes(source.status)),
        actionButton('停用', () => confirmSource(source, 'disable'), '', source.status === 'Disabled'),
        actionButton('删除登记', () => confirmSource(source, 'delete'), 'danger', source.status !== 'Disabled')),
      el('p', { class: 'small-note' }, '激活、应用和停用可能等待长查询或悬停 Portal 排空。元数据默认手动刷新；更改范围不会改动远端数据。'))));
  syncWriteButtons();
}

let catalogEpoch = 0;
let catalogState;
function renderCatalog() {
  catalogEpoch++;
  catalogState = { source: state.selected, schema: '', object: '', tokens: [null], page: 0, data: null };
  const schema = el('input', { placeholder: 'Schema 精确名称', 'aria-label': '目录 Schema' });
  const object = el('input', { placeholder: '对象精确名称', 'aria-label': '目录对象名' });
  replace($('#catalog'), el('section', { class: 'panel' },
    el('div', { class: 'panel-heading' }, el('div', {}, el('h3', {}, '元数据目录'), el('p', {}, '分页读取已发布快照；对象详情包含列、约束、索引及采集状态。'))),
    el('form', { class: 'catalog-controls', onsubmit: guard(event => {
      event.preventDefault(); catalogState.schema = schema.value.trim(); catalogState.object = object.value.trim();
      catalogState.page = 0; catalogState.tokens = [null]; return loadCatalog();
    }) }, schema, object, el('button', { type: 'submit' }, '查询目录')),
    el('div', { id: 'catalog-results' }, empty('尚未读取目录', '点击“查询目录”按需加载，避免浏览页面时反复扫描。'))));
}
async function loadCatalog() {
  const epoch = ++catalogEpoch, selected = catalogState;
  const query = new URLSearchParams({ limit: '50' });
  if (selected.schema) query.set('schema', selected.schema);
  if (selected.object) query.set('object', selected.object);
  if (selected.tokens[selected.page]) query.set('page_token', selected.tokens[selected.page]);
  replace($('#catalog-results'), empty('正在读取目录…'));
  try {
    const data = await get(`${sourcePath(selected.source)}/catalog?${query}`);
    if (epoch !== catalogEpoch || state.page !== 'sources' || state.selected !== selected.source) return;
    selected.data = data;
    const previous = button('上一页', () => { selected.page--; return loadCatalog(); }, 'small', false, selected.page === 0);
    const next = button('下一页', () => {
      selected.tokens[selected.page + 1] = data.next_page_token; selected.page++; return loadCatalog();
    }, 'small', false, !data.next_page_token);
    replace($('#catalog-results'),
      data.stale ? el('div', { class: 'panel-body notice warning' }, '当前目录已过期；旧事实保留不代表最新状态，请检查源状态并刷新。') : el('div'),
      data.objects.length ? table(['Schema', '对象', '类型', '列数', '详情'], data.objects.map(object => el('tr', {},
        el('td', {}, object.schema), el('td', {}, object.name), el('td', {}, object.object_type), el('td', {}, object.columns.length),
        el('td', {}, button('查看事实', () => showJson(`${object.schema}.${object.name}`, object), 'link'))))) : empty('当前范围没有对象'),
      el('div', { class: 'pagination' }, el('span', {}, `共 ${data.total} 项 · 第 ${selected.page + 1} 页 · ${time(data.refreshed_at)}`),
        el('div', { class: 'actions' }, previous, next)));
  } catch (error) {
    if (epoch !== catalogEpoch) return;
    replace($('#catalog-results'), empty(error.code === '40001' ? '目录快照已变化' : '目录读取失败', error.message),
      el('div', { class: 'panel-body' }, button('从第一页重新读取', () => {
        selected.page = 0; selected.tokens = [null]; return loadCatalog();
      })));
  }
}

let dialogSubmit = null;
function openDialog(title, body, submit = null, submitLabel = '提交') {
  $('#dialog-title').textContent = title; replace($('#dialog-body'), body);
  $('#dialog-error').hidden = true;
  $('#submit-dialog').textContent = submitLabel; $('#submit-dialog').hidden = !submit;
  $('#submit-dialog').dataset.write = 'true';
  dialogSubmit = submit; syncWriteButtons();
  if (!$('#dialog').open) $('#dialog').showModal();
}
function showJson(title, data) { openDialog(title, el('pre', { class: 'object-detail' }, JSON.stringify(data, null, 2))); }
function field(label, name, value = '', options = {}) {
  const { hint, wide, ...attributes } = options;
  const input = el('input', { name, value, ...attributes });
  return el('label', { class: `field${wide ? ' wide' : ''}` }, label, input, hint ? el('small', {}, hint) : null);
}
function check(label, name, checked) {
  return el('label', { class: 'check-field' }, el('input', { type: 'checkbox', name, checked }), label);
}
function formValue(name) { return $('#editor').elements.namedItem(name).value.trim(); }
function formChecked(name) { return $('#editor').elements.namedItem(name).checked; }
function lines(value) { return [...new Set(value.split(/[\n,，]/u).map(item => item.trim()).filter(Boolean))]; }
function secretRef() {
  const reference = formValue('secret_ref');
  const file = reference.startsWith('file:') ? reference.slice(5) : null;
  if (file !== null ? !file || file.startsWith('/') || file.split('/').some(part => !part || part === '.' || part === '..')
    : !/^[A-Za-z_][A-Za-z0-9_]*$/u.test(reference)) {
    throw new Error('请填写凭据目录下的 file:相对路径或环境变量名称，不能填写连接串或密码。');
  }
  return reference;
}
function registerForm() {
  const kind = el('select', { name: 'kind' }, el('option', { value: 'postgres' }, 'PostgreSQL'), el('option', { value: 'mysql' }, 'MySQL / MySQL 协议'));
  const body = el('div', {}, el('div', { class: 'notice' }, '先在服务的凭据目录保存连接串，或配置环境变量，再填写引用。登记成功后执行探测、发现与激活。'),
    el('div', { class: 'form-grid' },
      field('源 ID', 'id', '', { required: true, placeholder: '例如 crm_pg', hint: '登记后不能在线修改 ID。' }),
      field('业务数据库名', 'catalog_alias', '', { required: true, placeholder: '例如 crm', hint: '作为该源对外的 catalog_alias。' }),
      el('label', { class: 'field' }, '类型', kind),
      field('凭据引用', 'secret_ref', '', { required: true, placeholder: 'file:crm.dsn 或环境变量名' }),
      field('纳管 Schema', 'schemas', 'public', { required: true, hint: '多个名称用逗号分隔；MySQL 只能填写连接串指定的 database。' }),
      field('默认 Schema', 'default_schema', 'public', { required: true })),
    check('只读数据源', 'read_only', true), check('启用 DDL 能力（仍受授权与能力校验）', 'ddl_enabled', false));
  const readOnly = $('[name=read_only]', body), ddl = $('[name=ddl_enabled]', body);
  readOnly.addEventListener('change', () => { if (readOnly.checked) ddl.checked = false; ddl.disabled = readOnly.checked; });
  ddl.disabled = true;
  openDialog('登记数据源', body, () => {
    const schemas = lines(formValue('schemas')), defaultSchema = formValue('default_schema');
    if (!schemas.includes(defaultSchema)) throw new Error('默认 Schema 必须包含在纳管范围中。');
    if (formValue('kind') === 'mysql' && (schemas.length !== 1 || schemas[0] !== defaultSchema)) throw new Error('MySQL 的纳管范围及默认 Schema 必须为同一个 database。');
    return write('POST', 'sources', {
      id: formValue('id'), catalog_alias: formValue('catalog_alias'), kind: formValue('kind'),
      secret_ref: secretRef(), allowed_schemas: schemas, default_schema: defaultSchema,
      read_only: formChecked('read_only'), ddl_enabled: formChecked('ddl_enabled'),
    }, '登记数据源');
  }, '登记');
}
function schemaForm(source) {
  const rows = el('div');
  function add(binding = { remote_schema: '', enabled: true }) {
    const row = el('div', { class: 'binding-row' },
      el('input', { value: binding.remote_schema, required: true, 'aria-label': '远端 Schema 名称', class: 'schema-name' }),
      el('label', { class: 'check-field' }, el('input', { type: 'checkbox', checked: binding.enabled, class: 'schema-enabled' }), '启用'));
    row.append(button('移除', () => row.remove(), 'small')); rows.append(row);
  }
  source.desired.bindings.forEach(add);
  const discoveries = [...state.operations.values()].filter(operation => operation.action.action === 'discover'
    && operation.action.id === source.id && operation.incarnation === source.incarnation
    && operation.target_revision === source.desired_revision && operation.status === 'Succeeded');
  const discovered = discoveries.sort((a, b) => b.updated_at.localeCompare(a.updated_at))[0]?.result_schemas;
  const body = el('div', {}, el('div', { class: 'notice' }, '此操作完整替换纳管范围。保存后需显式应用；默认 Schema 留空表示清除。'),
    discovered ? el('p', { class: 'small-note' }, '最近发现：', discovered.join(', ') || '无可选 Schema') : null,
    rows, button('＋ 添加 Schema', () => add(), 'small'),
    el('div', { class: 'form-section' }, field('默认 Schema', 'default_schema', source.desired.definition.default_schema)));
  openDialog('调整纳管范围', body, () => {
    const bindings = [...rows.children].map(row => ({ remote_schema: $('.schema-name', row).value.trim(), enabled: $('.schema-enabled', row).checked }));
    const defaultSchema = formValue('default_schema') || null;
    if (new Set(bindings.map(binding => binding.remote_schema)).size !== bindings.length) throw new Error('Schema 名称不能重复。');
    if (defaultSchema && !bindings.some(binding => binding.remote_schema === defaultSchema && binding.enabled)) throw new Error('默认 Schema 必须是启用的绑定。');
    return write('PUT', `${sourcePath(source.id)}/schemas`, {
      target: sourceTarget(source), bindings, default_schema: defaultSchema,
    }, '保存纳管范围');
  }, '保存期望配置');
}
function credentialForm(source) {
  openDialog('更新凭据引用', el('div', {}, el('div', { class: 'notice' }, '页面不接收明文密码。请先更新服务端凭据，再保存引用并执行应用；同一引用也可用于显式轮换。'),
    field('凭据引用', 'secret_ref', source.desired.definition.secret_ref, { required: true })),
  () => write('PATCH', sourcePath(source.id), { target: sourceTarget(source), secret_ref: secretRef() }, '更新凭据引用'), '保存期望配置');
}
function confirmSource(source, action) {
  const deleting = action === 'delete';
  openDialog(`${actionLabels[action]} · ${source.catalog_alias}`,
    el('div', { class: 'notice warning' }, deleting
      ? '确认删除这个已停用数据源的登记？不会删除远端业务数据。关联业务虚拟库映射也需要核对。'
      : '确认停用这个数据源？新的相关查询将停止准入，正在执行的查询和 Portal 需要实际排空。'),
    () => write(deleting ? 'DELETE' : 'POST', `${sourcePath(source.id)}${deleting ? '' : '/disable'}`,
      { target: sourceTarget(source) }, actionLabels[action]), deleting ? '确认删除登记' : '确认停用');
}

function renderDatabases() {
  replace($('#content'), el('div', { class: 'notice' }, '业务虚拟库是 PostgreSQL 登录入口，外部 Schema 映射只读。库 owner / CONNECT 不自动授予源 SELECT 权限；账号同名库也需显式创建。'),
    state.databases.length ? el('div', { class: 'db-grid' }, state.databases.map(database => {
      const config = database.config;
      return el('section', { class: 'panel' },
        el('div', { class: 'panel-heading' }, el('div', {}, el('h3', {}, config.name),
          el('p', {}, `owner ${config.owner} · revision ${database.revision}`)),
          el('span', { class: `badge ${config.allow_connections ? 'good' : ''}` }, config.allow_connections ? '允许连接' : '禁止新连接')),
        el('div', { class: 'panel-body' },
          config.schemas.length ? config.schemas.map(mapping => el('div', { class: 'mapping' }, el('strong', {}, mapping.name), el('span', {}, '→'),
            el('div', {}, state.sources.find(source => source.id === mapping.source)?.catalog_alias ?? mapping.source,
              el('span', { class: 'subtext' }, mapping.remote_schema)), el('span', { class: 'badge' }, '只读'))) : el('p', { class: 'small-note' }, '尚无外部 Schema 映射'),
          el('div', { class: 'actions form-section' }, button('编辑定义与映射', () => databaseForm(database), '', true), button('删除', () => deleteDatabase(database), 'danger', true))));
    })) : empty('尚未创建业务虚拟库', '创建业务库后，可将多个来源映射到不同 Schema。'));
}
function databaseForm(database = null) {
  const config = database?.config ?? { name: '', owner: '', connect: [], allow_connections: true, schemas: [] };
  const rows = el('div');
  function add(mapping = { name: '', source: '', remote_schema: '' }) {
    const sourceSelect = el('select', { class: 'mapping-source', required: true, 'aria-label': '映射数据源' },
      el('option', { value: '' }, '选择数据源'), state.sources.map(source => el('option', { value: source.id }, source.catalog_alias)));
    if (mapping.source && !state.sources.some(source => source.id === mapping.source)) sourceSelect.append(el('option', { value: mapping.source }, `${mapping.source}（源已不存在）`));
    sourceSelect.value = mapping.source;
    const schema = el('input', { required: true, class: 'mapping-remote', value: mapping.remote_schema, placeholder: '远端 Schema', 'aria-label': '映射远端 Schema' });
    sourceSelect.addEventListener('change', () => {
      const source = state.sources.find(source => source.id === sourceSelect.value);
      schema.value = source?.desired.definition.default_schema ?? '';
    });
    const row = el('div', { class: 'mapping-row' }, el('input', { required: true, class: 'mapping-name', value: mapping.name, placeholder: '本地 Schema', 'aria-label': '映射本地 Schema' }), sourceSelect, schema);
    row.append(button('移除', () => row.remove(), 'small')); rows.append(row);
  }
  config.schemas.forEach(add);
  openDialog(database ? `编辑业务库 · ${config.name}` : '创建业务虚拟库', el('div', {},
    el('div', { class: 'notice' }, '保存时完整替换定义。请保留仍需使用的映射；owner 和 CONNECT 账号必须已在服务配置中存在。'),
    el('div', { class: 'form-grid' }, field('数据库名', 'name', config.name, { required: true, readonly: database ? '' : null }),
      field('Owner 账号', 'owner', config.owner, { required: true, placeholder: '例如 fedengine' }),
      field('允许 CONNECT 的其他账号', 'connect', config.connect.join(', '), { wide: true, hint: '逗号分隔；Owner 本身拥有 CONNECT 准入。' })),
    check('允许新连接', 'allow_connections', config.allow_connections),
    el('div', { class: 'form-section' }, el('h3', {}, '外部 Schema 映射'), el('p', { class: 'small-note' }, '本地 Schema → 数据源 → 远端 Schema；远端 Schema 须在源的启用纳管范围内。')),
    rows, button('＋ 添加映射', () => add(), 'small')), () => {
      const definition = {
        name: formValue('name'), owner: formValue('owner'), connect: lines(formValue('connect')), allow_connections: formChecked('allow_connections'),
        schemas: [...rows.children].map(row => ({ name: $('.mapping-name', row).value.trim(), source: $('.mapping-source', row).value, remote_schema: $('.mapping-remote', row).value.trim() })),
      };
      if (new Set(definition.schemas.map(mapping => mapping.name)).size !== definition.schemas.length) throw new Error('本地 Schema 名称不能重复。');
      for (const mapping of definition.schemas) {
        const source = state.sources.find(source => source.id === mapping.source);
        if (!source?.desired.bindings.some(binding => binding.remote_schema === mapping.remote_schema && binding.enabled)) throw new Error(`远端 Schema ${mapping.remote_schema} 不在数据源的启用纳管范围内。`);
      }
      return write(database ? 'PUT' : 'POST', `virtual-databases${database ? `/${encodeURIComponent(config.name)}` : ''}`,
        database ? { target: { oid: database.oid, expected_revision: database.revision }, definition } : definition,
        database ? '更新业务库' : '创建业务库');
    }, '保存业务库');
}
function deleteDatabase(database) {
  openDialog(`删除业务库 · ${database.config.name}`, el('div', { class: 'notice warning' },
    '删除后现有连接的后续查询将失败，需要重新选择业务库连接。此操作不删除来源数据。'),
    () => write('DELETE', `virtual-databases/${encodeURIComponent(database.config.name)}`,
      { target: { oid: database.oid, expected_revision: database.revision } }, '删除业务库'), '确认删除业务库');
}
function renderOperations() {
  const operations = [...state.operations.values()].sort((a, b) => b.created_at.localeCompare(a.created_at));
  const signature = JSON.stringify(operations);
  if ($('#operation-rows') && state.operationSignature === signature) return;
  state.operationSignature = signature;
  if (!$('#operation-rows')) {
    const lookup = el('input', { required: true, placeholder: '输入操作编号', 'aria-label': '操作编号' });
    replace($('#content'), el('p', { class: 'operation-history' }, '显示本浏览器会话最近 40 项操作及当前源的进行中操作。可输入已知编号查询服务端记录。'),
      el('form', { class: 'toolbar form-section', onsubmit: guard(async event => {
        event.preventDefault(); const operation = await get(`operations/${encodeURIComponent(lookup.value.trim())}`);
        rememberOperation(operation.id); state.operations.set(operation.id, operation); renderOperations();
      }) }, lookup, el('button', { type: 'submit' }, '查询操作')),
      panel('操作记录', '已受理不等于执行成功；等待状态会持续更新。', el('div', { id: 'operation-rows' })));
  }
  replace($('#operation-rows'),
    operations.length ? table(['操作', '数据源', '状态 / 阶段', '更新时间', '详情'], operations.map(operation => {
        const id = operation.action.id ?? operation.action.definition?.id ?? '—';
        return el('tr', {}, el('td', {}, actionLabels[operation.action.action] ?? operation.action.action, el('span', { class: 'subtext operation-id' }, operation.id)),
          el('td', {}, state.sources.find(source => source.id === id)?.catalog_alias ?? id),
          el('td', {}, badge(operation.status), el('span', { class: 'subtext' }, operation.phase)),
          el('td', {}, time(operation.updated_at)), el('td', {}, button('查看结果', () => showJson('操作详情', state.operations.get(operation.id)), 'link')));
      })) : empty('尚无操作记录', '在页面提交操作后，可在这里跟踪进度。'));
}

document.querySelectorAll('[data-page]').forEach(node => node.addEventListener('click', () => navigate(node.dataset.page)));
$('#close-dialog').addEventListener('click', () => $('#dialog').close());
$('#cancel-dialog').addEventListener('click', () => $('#dialog').close());
$('#editor').addEventListener('submit', async event => {
  event.preventDefault(); if (!dialogSubmit || state.writing || state.pending) return;
  $('#dialog-error').hidden = true;
  try { await dialogSubmit(); } catch (error) {
    $('#dialog-error').textContent = error.message; $('#dialog-error').hidden = false;
  }
});
renderPage();
guard(reload)();
setInterval(() => { if (!document.hidden) reload().catch(() => {}); }, 2500);
document.addEventListener('visibilitychange', () => { if (!document.hidden) reload().catch(() => {}); });
