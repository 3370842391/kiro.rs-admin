'use strict';
let KEY = '';
const $ = (id) => document.getElementById(id);
const yuan = (c) => (c / 100).toFixed(2);
function toast(m) { const t = $('toast'); t.textContent = m; t.classList.add('show'); setTimeout(() => t.classList.remove('show'), 2200); }

async function api(path, method = 'GET', body) {
  const opt = { method, headers: { 'Authorization': 'Bearer ' + KEY } };
  if (body) { opt.headers['Content-Type'] = 'application/json'; opt.body = JSON.stringify(body); }
  const r = await fetch('/wholesale' + path, opt);
  const j = await r.json().catch(() => ({}));
  if (!r.ok) throw new Error(j.error || ('HTTP ' + r.status));
  return j;
}

function saveKey() {
  KEY = $('admin-key').value.trim();
  if (!KEY) { $('key-msg').textContent = '请输入 key'; return; }
  api('/admin/customers').then(() => {
    localStorage.setItem('ws_admin_key', KEY);
    $('keybar').classList.add('hide'); $('panel').classList.remove('hide');
    loadUsers(); loadMothers();
  }).catch((e) => { $('key-msg').textContent = '验证失败: ' + e.message; });
}

function tab(name) {
  ['users', 'mothers', 'cdk', 'upload'].forEach((n) => $('t-' + n).classList.toggle('hide', n !== name));
  document.querySelectorAll('.tabs button').forEach((b) => b.classList.toggle('on', b.textContent.includes(
    { users: '用户', mothers: '母号', cdk: 'CDK', upload: '上号' }[name])));
  if (name === 'cdk') loadCdks();
}

async function loadUsers() {
  try {
    const { customers } = await api('/admin/customers');
    $('users-body').innerHTML = customers.map((c) => `<tr>
      <td>${c.id}</td><td>${c.uid}</td><td>${esc(c.username)}</td>
      <td class="num">${c.aliveCount}/${c.target}</td>
      <td class="num">${yuan(c.balanceCents)}</td>
      <td>${c.disabled ? '<span class="err">停用</span>' : '<span class="ok">正常</span>'}</td>
      <td>
        <button style="padding:2px 8px" onclick="adjust(${c.id})">改余额</button>
        <button style="padding:2px 8px" onclick="setTarget(${c.id},${c.target})">改目标</button>
        <button style="padding:2px 8px" onclick="toggleUser(${c.id},${!c.disabled})">${c.disabled ? '启用' : '停用'}</button>
      </td></tr>`).join('') || '<tr><td colspan="7" class="muted">暂无用户</td></tr>';
  } catch (e) { toast('加载用户失败: ' + e.message); }
}

async function adjust(id) {
  const s = prompt('调整余额（元，正=充值，负=扣减）：'); if (s === null) return;
  const delta = Math.round(parseFloat(s) * 100); if (!Number.isFinite(delta) || delta === 0) return;
  const reason = prompt('原因（必填）：') || '管理员调整';
  try { const r = await api('/admin/balance', 'POST', { customerId: id, deltaCents: delta, reason }); toast('新余额 ' + yuan(r.balanceCents)); loadUsers(); }
  catch (e) { toast('失败: ' + e.message); }
}
async function setTarget(id, cur) {
  const s = prompt('常驻号池目标数：', cur); if (s === null) return;
  const t = parseInt(s, 10); if (!Number.isFinite(t)) return;
  try { await api('/admin/target', 'POST', { customerId: id, target: t }); toast('已更新'); loadUsers(); }
  catch (e) { toast('失败: ' + e.message); }
}
async function toggleUser(id, dis) {
  try { await api('/admin/disabled', 'POST', { customerId: id, disabled: dis }); loadUsers(); }
  catch (e) { toast('失败: ' + e.message); }
}

async function loadMothers() {
  try {
    const { mothers } = await api('/admin/mothers');
    $('mothers-body').innerHTML = mothers.map((m) => `<tr>
      <td>${m.directoryId}</td>
      <td><span class="badge b-${m.state}">${m.state}</span></td>
      <td class="num">${m.childAlive}/<span class="err">${m.childDead}</span></td>
      <td class="muted">${(m.addedAt || '').slice(0, 10)}</td>
      <td class="muted">${m.lastDeathAt ? m.lastDeathAt.slice(11, 19) : '—'}</td>
      <td>
        <button style="padding:2px 8px" onclick="mstate('${m.directoryId}','dead')">退役</button>
        <button style="padding:2px 8px" onclick="mstate('${m.directoryId}','active')">恢复</button>
      </td></tr>`).join('') || '<tr><td colspan="6" class="muted">暂无母号</td></tr>';
  } catch (e) { toast('加载母号失败: ' + e.message); }
}
async function mstate(id, state) {
  try { await api('/admin/mother-state', 'POST', { directoryId: id, state }); toast('已设为 ' + state); loadMothers(); }
  catch (e) { toast('失败: ' + e.message); }
}

async function genCdk() {
  const cents = Math.round(parseFloat($('cdk-yuan').value) * 100);
  const count = parseInt($('cdk-count').value, 10);
  const batch = $('cdk-batch').value.trim() || null;
  if (!Number.isFinite(cents) || cents <= 0 || !Number.isFinite(count) || count <= 0) { toast('面额/数量非法'); return; }
  try {
    const r = await api('/admin/cdks', 'POST', { valueCents: cents, count, batch });
    const out = $('cdk-out'); out.classList.remove('hide');
    out.textContent = r.codes.join('\n');
    toast('已生成 ' + r.generated + ' 张'); loadCdks();
  } catch (e) { toast('失败: ' + e.message); }
}
async function loadCdks() {
  try {
    const unused = $('cdk-unused').checked ? '1' : '0';
    const { cdks } = await api('/admin/cdks?limit=300&unused=' + unused);
    $('cdks-body').innerHTML = cdks.map((c) => `<tr>
      <td style="font-family:Consolas,monospace">${c.code}</td>
      <td class="num">${yuan(c.valueCents)}</td>
      <td>${c.redeemedAt ? '<span class="muted">已用</span>' : (c.disabled ? '<span class="err">作废</span>' : '<span class="ok">未用</span>')}</td>
      <td class="muted">${esc(c.batch || '')}</td></tr>`).join('') || '<tr><td colspan="4" class="muted">暂无</td></tr>';
  } catch (e) { toast('加载CDK失败: ' + e.message); }
}

async function uploadTest() {
  let arr;
  try { arr = JSON.parse($('upload-json').value); if (!Array.isArray(arr)) throw 0; }
  catch { toast('JSON 格式错误，应为数组'); return; }
  try {
    const r = await api('/admin/upload-test', 'POST', { accounts: arr });
    const out = $('upload-out'); out.classList.remove('hide');
    out.textContent = JSON.stringify(r.results, null, 2);
    const ok = r.results.filter((x) => x.ok).length;
    toast(`入池 ${ok}/${r.results.length}`); loadMothers();
  } catch (e) { toast('失败: ' + e.message); }
}

function esc(s) { return String(s || '').replace(/[&<>]/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;' }[c])); }

// 自动登录
const saved = localStorage.getItem('ws_admin_key');
if (saved) { $('admin-key').value = saved; saveKey(); }
