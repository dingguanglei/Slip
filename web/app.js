'use strict';
const $ = id => document.getElementById(id);
const token = location.hash.slice(1) || sessionStorage.getItem('slip-token') || '';
if (location.hash) { sessionStorage.setItem('slip-token', token); history.replaceState(null, '', '/'); }
let account = '', contact = '', state = null, busy = false, polling = false, selectedFiles = [];
const drafts = new Map(), fileDrafts = new Map(), objectUrls = new Map();
let renderKey = '';
function showView(view) {
  const profile = view === 'profile';
  document.body.classList.toggle('profile-view', profile);
  $('profile-panel').hidden = !profile;
  $('chat-panel').hidden = profile;
  for (const name of ['profile', 'chat']) {
    $('nav-'+name).classList.toggle('active', name === view);
    $('nav-'+name).setAttribute('aria-pressed', String(name === view));
  }
}
$('nav-profile').onclick = () => showView('profile');
$('nav-chat').onclick = () => showView('chat');
const key = () => `${account}\n${contact}`;
function notice(text) { $('notice').textContent = text; $('notice').hidden = !text; }
async function api(action, extra = {}) {
  const response = await fetch('/api', { method: 'POST', headers: { 'Content-Type': 'application/json', Authorization: `Bearer ${token}` }, body: JSON.stringify({ action, account, contact, ...extra }) });
  const result = await response.json();
  if (!response.ok) throw new Error(result.error || '操作失败');
  return result;
}
function element(tag, className, text) { const node = document.createElement(tag); if (className) node.className = className; if (text !== undefined) node.textContent = text; return node; }
function saveDraft() { drafts.set(key(), $('draft').value); fileDrafts.set(key(), selectedFiles); }
function restoreDraft() { $('draft').value = drafts.get(key()) || ''; selectedFiles = fileDrafts.get(key()) || []; renderFiles(); }
async function loadAccounts(preferred) {
  const result = await api('accounts');
  $('account').replaceChildren(...result.accounts.map(a => { const option = element('option', '', a); option.value = a; return option; }));
  account = preferred || result.accounts[0] || ''; $('account').value = account;
  document.body.classList.toggle('signed-out', !account);
  $('login-screen').hidden = !!account;
  if (!account) { $('status').textContent = '尚未登录'; $('first-login-address').focus(); }
  else { showView('chat'); await refresh(); }
}
function renderSessions() {
  const term = $('search').value.toLowerCase();
  const sessions = (state?.sessions || []).filter(s => `${s.contact} ${s.alias || ''}`.toLowerCase().includes(term));
  $('sessions').replaceChildren(...sessions.map(s => {
    const button = element('button', `session${s.contact === contact ? ' active' : ''}`); button.setAttribute('aria-label', s.contact);
    button.append(element('span', 'avatar', (s.alias || s.contact)[0].toUpperCase()));
    const copy = element('div', 'session-copy'); copy.append(element('div', 'session-name', s.alias || s.contact.split('@')[0]), element('div', 'preview', s.messages ? s.preview.replace(/^me: /, '我：') : s.contact)); button.append(copy);
    if (s.unread) button.append(element('span', 'unread', s.unread));
    button.onclick = async () => { showView('chat'); saveDraft(); contact = s.contact; restoreDraft(); renderKey = ''; await refresh(); await api('read').catch(e => notice(e.message)); };
    return button;
  }));
  if (!sessions.length) $('sessions').append(element('p', 'empty-list', term ? '没有匹配的联系人' : '还没有对话。点击 ＋ 添加好友邮箱。'));
}
async function mediaBlob(messageId, index, selectedAccount, selectedContact) {
  const cacheKey = `${selectedAccount}\n${selectedContact}\n${messageId}\n${index}`;
  if (objectUrls.has(cacheKey)) return objectUrls.get(cacheKey);
  const response = await fetch('/media', {method: 'POST', headers: {'Content-Type': 'application/json', Authorization: `Bearer ${token}`}, body: JSON.stringify({account: selectedAccount, contact: selectedContact, id: messageId, index})});
  if (!response.ok) throw new Error('附件读取失败');
  const url = URL.createObjectURL(await response.blob()); objectUrls.set(cacheKey, url); return url;
}
function renderMessages() {
  if (!contact) return;
  const data = state.conversation?.contact === contact ? state.conversation.messages : [];
  const nextKey = JSON.stringify([account, contact, data]);
  if (nextKey === renderKey) return;
  renderKey = nextKey;
  const box = $('messages'); const atBottom = box.scrollHeight - box.scrollTop - box.clientHeight < 90;
  box.replaceChildren();
  if (!data.length) box.append(element('div', 'welcome', '等待开始对话'));
  for (const msg of data) {
    const mine = msg.sender === 'me';
    const row = element('article', `message${mine ? ' mine' : ''}`);
    if (msg.body) row.append(element('div', 'bubble', msg.body));
    (msg.media || []).forEach((media, index) => {
      const card = element('div', 'file-card');
      const a = account, c = contact;
      if (['image/png','image/jpeg','image/gif','image/webp','image/bmp'].includes(media.mime)) {
        const img = element('img', 'attachment-image'); img.alt = media.name; card.append(img);
        mediaBlob(msg.id, index, a, c).then(url => {img.src = url;}).catch(() => {img.alt = '图片加载失败';});
      }
      const download = element('button', 'file-download', `↓ ${media.name} · ${formatSize(media.size)}`);
      download.onclick = async () => {
        try {
          const response = await fetch('/download-ticket', {method:'POST', headers:{'Content-Type':'application/json',Authorization:`Bearer ${token}`}, body:JSON.stringify({account:a,contact:c,id:msg.id,index})});
          if (!response.ok) throw new Error('附件读取失败');
          const result = await response.json();
          const link = element('a'); link.href = result.url; link.download = media.name;
          document.body.append(link); link.click(); link.remove();
        } catch(e) { notice(e.message); }
      };
      card.append(download); row.append(card);
    });
    const status = msg.status === 'failed' ? '发送失败' : msg.status === 'sending' ? '发送中' : mine ? '已交邮件服务器' : '已保存本地';
    const meta = element('div', 'message-meta', `${String(msg.date).slice(11,16)}${mine ? (msg.status === 'failed' ? ' · !' : msg.status === 'sending' ? ' · ◷' : ' · ✓') : ''}`);
    meta.title = `${msg.date} · ${status}${msg.encrypted ? ' · 已加密' : ''}`; meta.setAttribute('aria-label', meta.title); row.append(meta);
    if (mine && msg.status === 'failed') { const retry = element('button', 'secondary', '重试最近失败消息'); retry.onclick = () => run(() => api('retry')); row.append(retry); }
    box.append(row);
  }
  if (atBottom || !busy) box.scrollTop = box.scrollHeight;
}
function controls() {
  const info = state?.info?.address === contact ? state.info : null;
  const ready = !!contact && !!info?.encryption_active;
  const session = state?.sessions?.find(s => s.contact === contact);
  $('contact-title').textContent = session?.alias || contact.split('@')[0] || 'Slip';
  $('contact-title').title = contact;
  const security = $('security-label');
  security.replaceChildren();
  security.title = !contact ? '' : info?.pending_fingerprint ? '密钥发生变化，请核对后再发送' : ready ? '端到端加密' : '等待好友确认';
  security.setAttribute('aria-label', security.title);
  if (ready) {
    const svg = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
    for (const [name,value] of Object.entries({viewBox:'0 0 24 24',fill:'none',stroke:'currentColor','stroke-width':'1.7','stroke-linecap':'round','stroke-linejoin':'round','aria-hidden':'true',class:'icon'})) svg.setAttribute(name,value);
    const path = document.createElementNS('http://www.w3.org/2000/svg','path');
    path.setAttribute('d','M7 10V7a5 5 0 0 1 10 0v3M5 10h14v11H5zM12 14v3');
    svg.append(path); security.append(svg);
  } else security.textContent = security.title;
  $('draft').disabled = !contact || busy;
  $('send').disabled = !ready || busy;
  $('pick-files').disabled = !contact || busy;
  $('composer-hint').textContent = ready ? 'Enter 发送 · Shift + Enter 换行' : '等待公钥交换';
  $('key-banner').hidden = !contact || ready;
  $('exchange').disabled = busy;
  $('add-submit').disabled = busy;
  $('account').disabled = busy;
  $('logout').disabled = busy;
  $('login-open').disabled = busy;
  $('profile-address').textContent = account;
  $('profile-fingerprint').textContent = state?.fingerprint || '尚未生成';
  $('my-fingerprint').textContent = state?.fingerprint || '连接邮箱后生成';
  $('peer-fingerprint').textContent = info?.peer_fingerprint || '尚未收到公钥';
  $('pending-key').hidden = !info?.pending_fingerprint;
  $('pending-fingerprint').textContent = info?.pending_fingerprint || '';
}
async function refresh() {
  if (!account || polling) return;
  polling = true;
  const a = account, c = contact;
  try { const result = await api('state'); if (a !== account || c !== contact) return; state = result; $('status').title = result.status; $('status').setAttribute('aria-label',result.status); renderSessions(); renderMessages(); controls(); }
  catch(e) {notice(e.message);} finally { polling = false; }
}
async function run(fn) { if (busy) return; busy = true; controls(); try { await fn(); } catch(e) {notice(e.message);} finally { busy = false; await refresh(); controls(); } }
function formatSize(n) { return n < 1024 ? `${n} B` : n < 1024*1024 ? `${(n/1024).toFixed(1)} KB` : `${(n/1024/1024).toFixed(1)} MB`; }
function renderFiles() { $('file-list').replaceChildren(...selectedFiles.map((file,index) => {const chip = element('button', 'file-chip', `${file.name} (${formatSize(file.size)}) ×`); chip.type='button'; chip.onclick=()=>{selectedFiles = selectedFiles.filter((_,i)=>i!==index);renderFiles();};return chip;})); }
function fileBase64(file) { return new Promise((resolve,reject) => {const reader = new FileReader();reader.onload=()=>resolve({name:file.name,data:String(reader.result).split(',')[1]});reader.onerror=reject;reader.readAsDataURL(file);}); }
$('files').onchange = () => { const next = [...selectedFiles, ...$('files').files]; if (next.length > 8 || next.reduce((n,f)=>n+f.size,0)>12*1024*1024) {notice('最多 8 个附件，总大小不超过 12 MB');} else {selectedFiles=next;renderFiles();} $('files').value=''; };
$('pick-files').onclick = () => $('files').click();
$('account').onchange = async () => { saveDraft(); account = $('account').value; contact = ''; state = null; $('sessions').replaceChildren(); renderKey='';restoreDraft();$('messages').replaceChildren(element('div','welcome','选择好友开始聊天'));controls();await refresh(); };
$('search').oninput = renderSessions;
$('add-open').onclick = () => {if(!account) return $('login-dialog').showModal(); $('add-dialog').showModal();};
$('login-open').onclick = () => $('login-dialog').showModal();
$('info-open').onclick = () => $('info-dialog').showModal();
document.querySelectorAll('[data-close]').forEach(button => button.onclick = () => $(button.dataset.close).close());
$('add-form').onsubmit = e => {e.preventDefault();run(async()=>{const c=$('new-contact').value.trim().toLowerCase();await api('contact',{contact:c});saveDraft();contact=c;restoreDraft();$('add-dialog').close();$('new-contact').value='';showView('chat');notice('正在发送好友申请…');await api('exchange',{contact:c});notice('好友申请已发送，等待对方交换公钥。');});};
async function loginFrom(prefix, first) {
  const submit = $(prefix+'submit'), error = $(prefix+'error');
  if (submit.disabled) return;
  submit.disabled = true; error.textContent = '正在验证邮箱…';
  try {
    const result = await api('login', {account:$(prefix+'address').value.trim(), password:$(prefix+'password').value});
    $(prefix+'password').value = '';
    if (!first) $('login-dialog').close();
    if (account) saveDraft();
    contact = ''; state = null; renderKey = ''; selectedFiles = []; $('draft').value=''; renderFiles();
    $('messages').replaceChildren(element('div','welcome','选择好友，开始聊天'));
    error.textContent = ''; notice('');
    await loadAccounts(result.account);
  } catch(e) { error.textContent = e.message; }
  finally { submit.disabled = false; }
}
$('login-form').onsubmit = e => { e.preventDefault(); loginFrom('login-', false); };
$('first-login-form').onsubmit = e => { e.preventDefault(); loginFrom('first-login-', true); };
$('logout').onclick = () => run(async () => {
  await api('logout');
  drafts.clear(); fileDrafts.clear();
  for (const url of objectUrls.values()) URL.revokeObjectURL(url);
  objectUrls.clear(); account=''; contact=''; state=null; renderKey=''; selectedFiles=[];
  $('draft').value=''; renderFiles(); $('sessions').replaceChildren(); $('messages').replaceChildren();
  $('first-login-address').value=''; $('first-login-password').value=''; $('first-login-error').textContent='';
  $('login-password').value=''; notice(''); await loadAccounts();
});
$('exchange').onclick = () => run(async()=>{await api('exchange');notice('公钥已发送。对方添加你的邮箱并交换公钥后即可聊天。');});
$('sync').onclick = () => run(async()=>{const result=await api('sync');notice(`本次保存 ${result.saved} 条，清理 ${result.burned} 封接收邮件`);});
$('trust').onclick = () => run(async()=>{await api('trust',{fingerprint:state.info.pending_fingerprint});$('info-dialog').close();notice('新密钥已信任');});
$('composer').onsubmit = e => {e.preventDefault();if($('send').disabled || (!$('draft').value.trim() && !selectedFiles.length))return;run(async()=>{const draftKey=key();const targetAccount=account,targetContact=contact;const text=$('draft').value;const sentFiles=selectedFiles;await api('send',{account:targetAccount,contact:targetContact,body:text,files:await Promise.all(sentFiles.map(fileBase64))});drafts.delete(draftKey);fileDrafts.delete(draftKey);if(key()===draftKey){$('draft').value='';selectedFiles=[];renderFiles();}notice('');});};
$('draft').onkeydown = e => {if(e.key==='Enter'&&!e.shiftKey&&!e.isComposing){e.preventDefault();$('composer').requestSubmit();}};
loadAccounts().catch(e=>{ $('first-login-error').textContent=e.message; notice(e.message); });
setInterval(refresh,2000);
