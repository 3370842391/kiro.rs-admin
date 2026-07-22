'use strict';
let KEY = '', UID = '';
const $ = (id) => document.getElementById(id);
const yuan = (c) => (c / 100).toFixed(2);
function toast(m) { const t = $('toast'); t.textContent = m; t.classList.add('show'); setTimeout(() => t.classList.remove('show'), 2400); }

async function api(path, method = 'GET', body, withKey = true) {
  const opt = { method, headers: {} };
  if (withKey) opt.headers['Authorization'] = 'Bearer ' + KEY;
  if (body) { opt.headers['Content-Type'] = 'application/json'; opt.body = JSON.stringify(body); }
  const r = await fetch('/wholesale' + path, opt);
  const j = await r.json().catch(() => ({}));
  if (!r.ok) throw new Error(j.error || ('HTTP ' + r.status));
  return j;
}

function authTab(name) {
  $('login-box').classList.toggle('hide', name !== 'login');
  $('reg-box').classList.toggle('hide', name !== 'register');
  $('tab-login').classList.toggle('on', name === 'login');
  $('tab-reg').classList.toggle('on', name === 'register');
  $('auth-msg').textContent = '';
}

async function doRegister() {
  const username = $('r-user').value.trim(), password = $('r-pass').value, email = $('r-email').value.trim();
  try {
    const r = await api('/register', 'POST', { username, password, email: email || null }, false);
    $('reg-out').classList.remove('hide');
    $('reg-out').innerHTML = `<div class="ok">注册成功！请务必保存你的 API Key（只显示这一次）：</div>
      <pre>UID: ${r.uid}\nAPI Key: ${r.apiKey}</pre>`;
    toast('注册成功，已自动登录');
    KEY = r.apiKey; UID = r.uid; enter();
  } catch (e) { $('auth-msg').textContent = e.message; }
}

async function doLogin() {
  const username = $('l-user').value.trim(), password = $('l-pass').value;
  try {
    const r = await api('/login', 'POST', { username, password }, false);
    KEY = r.apiKey; UID = r.uid;
    localStorage.setItem('ws_key', KEY); localStorage.setItem('ws_uid', UID);
    enter();
  } catch (e) { $('auth-msg').textContent = e.message; }
}

function logout() { localStorage.removeItem('ws_key'); localStorage.removeItem('ws_uid'); location.reload(); }

function enter() {
  $('auth').classList.add('hide'); $('app').classList.remove('hide');
  $('api-doc').textContent =
`# 每 60s 补齐号池到目标数（示例 target=5）
curl -X POST ${location.origin}/wholesale/sync \\
  -H "Authorization: Bearer ${KEY}" \\
  -H "Content-Type: application/json" \\
  -d '{"uid":"${UID}","target":5}'

# 只读当前号池（不补号/不扣费）
curl ${location.origin}/wholesale/pool -H "Authorization: Bearer ${KEY}"`;
  refresh();
}

function statusBadge(s) {
  const zh = { active: '正常', low_quota: '额度不足', dead: '已死亡' }[s] || s;
  return `<span class="badge b-${s}">${zh}</span>`;
}
function fmtDur(sec) {
  sec = Math.max(0, sec | 0);
  if (sec < 60) return sec + '秒';
  if (sec < 3600) return Math.floor(sec / 60) + '分' + (sec % 60) + '秒';
  return Math.floor(sec / 3600) + '时' + Math.floor((sec % 3600) / 60) + '分';
}
function warrantyLeft(until) {
  const left = Math.floor((new Date(until) - Date.now()) / 1000);
  return left > 0 ? fmtDur(left) : '<span class="muted">已过保</span>';
}

function renderPool(res) {
  $('s-alive').textContent = res.alive;
  $('s-target').textContent = res.target;
  $('s-balance').textContent = yuan(res.balanceCents);
  $('s-uid').textContent = res.uid || UID;
  if (!$('c-target').value) $('c-target').value = res.target;
  const rows = (res.pool || []).map((p) => {
    const dead = p.status === 'dead';
    const keyCell = p.apiKey
      ? `<span class="key">${p.apiKey.slice(0, 14)}…</span> <button class="sm" onclick="copyKey('${p.apiKey}')">复制</button>`
      : '<span class="muted">—</span>';
    return `<tr class="${dead ? 'dead' : ''}">
      <td>${p.publicId}</td>
      <td>${statusBadge(p.status)}</td>
      <td>${keyCell}</td>
      <td class="muted">${(p.motherId || '').slice(0, 14)}</td>
      <td class="muted">${p.quota || (dead ? '—' : '正常')}</td>
      <td>${dead ? '<span class="err">存活 ' + fmtDur(p.aliveSecs) + '</span>' : fmtDur(p.aliveSecs)}</td>
      <td>${dead ? '<span class="muted">—</span>' : warrantyLeft(p.warrantyUntil)}</td>
    </tr>`;
  }).join('');
  $('pool-body').innerHTML = rows || '<tr><td colspan="7" class="muted">号池为空，点「立即同步补号」</td></tr>';
}

async function refresh() {
  try { renderPool(await api('/pool')); }
  catch (e) { if (/无效|停用|缺少/.test(e.message)) { logout(); } else { toast('刷新失败: ' + e.message); } }
}

async function doSync() {
  const target = parseInt($('c-target').value, 10);
  $('sync-msg').textContent = '同步中…';
  try {
    const r = await api('/sync', 'POST', { uid: UID, target: Number.isFinite(target) ? target : undefined });
    renderPool(r);
    let msg = `本次补 ${r.addedThisCall} 个，扣 ${yuan(r.chargedCents)} 元。`;
    if (r.shortfall) {
      msg += r.shortfall.reason === 'insufficient_balance' ? ' ⚠ 余额不足已停止，请充值。'
        : ` ⚠ 库存不足，还差 ${r.shortfall.missing || 0} 个。`;
    }
    $('sync-msg').textContent = msg;
  } catch (e) { $('sync-msg').textContent = '同步失败: ' + e.message; }
}

async function doRedeem() {
  const code = $('cdk-code').value.trim(); if (!code) return;
  try { const r = await api('/redeem', 'POST', { code }); toast('充值成功，余额 ' + yuan(r.balanceCents) + ' 元'); $('cdk-code').value = ''; refresh(); }
  catch (e) { toast('充值失败: ' + e.message); }
}

function copyKey(k) { navigator.clipboard.writeText(k).then(() => toast('已复制 API Key')).catch(() => toast('复制失败')); }

// 自动登录 + 每 60s 轮询
const sk = localStorage.getItem('ws_key'), su = localStorage.getItem('ws_uid');
if (sk && su) { KEY = sk; UID = su; enter(); }
setInterval(() => { if (KEY && !$('app').classList.contains('hide')) refresh(); }, 60000);
