const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const $ = (id) => document.getElementById(id);

const STATUS = {
  checking: ['检测中…', '正在检查网络'],
  online: ['已联网', '校园网认证正常'],
  off_campus: ['未连接校园网', '连上 buptportal 后会自动登录'],
  needs_login: ['校园网未认证', '自动登录已关闭'],
  logging_in: ['正在登录…', '正在向认证网关提交账号'],
  failed: ['登录失败', ''],
  no_account: ['未设置账号', '请在下方填写学号和密码'],
};

const KEYSTORE = {
  macos: '密码保存在 macOS 钥匙串中',
  windows: '密码保存在 Windows 凭据管理器中',
  linux: '密码保存在系统密钥环中',
};

let view = null;
let editing = false;

const clock = (sec) =>
  new Date(sec * 1000).toLocaleTimeString('zh-CN', { hour: '2-digit', minute: '2-digit' });

function renderSnapshot(s) {
  const [label, fallback] = STATUS[s.status] ?? STATUS.checking;
  $('hero').dataset.status = s.status;
  $('status-label').textContent = label;

  $('status-detail').textContent = s.detail || fallback;
  const meta = [];
  if (s.last_check) meta.push(`上次检测 ${clock(s.last_check)}`);
  if (s.logins) meta.push(`已自动登录 ${s.logins} 次`);
  $('status-meta').textContent = meta.join(' · ');
  $('check').disabled = false;

  const list = $('events');
  list.replaceChildren(
    ...s.events.map((e) => {
      const li = document.createElement('li');
      li.className = e.level;
      const time = document.createElement('time');
      time.textContent = clock(e.time);
      const text = document.createElement('span');
      text.textContent = e.text;
      li.append(time, text);
      return li;
    }),
  );
  $('events-empty').hidden = s.events.length > 0;
}

function renderAccount() {
  const { username } = view.settings;
  // 密码在后台读取，只要学号已保存就视为已设置；读取失败时状态栏会提示
  const saved = Boolean(username);
  const showForm = editing || !saved;
  $('account-view').hidden = showForm;
  $('account-form').hidden = !showForm;
  $('cancel').hidden = !saved;
  if (saved) {
    $('account-name').textContent = username;
    $('account-hint').textContent = KEYSTORE[view.platform] ?? '密码保存在系统凭据库中';
  }
  if (showForm && !$('username').value) $('username').value = username;
}

function renderSettings() {
  $('auto-login').checked = view.settings.auto_login;
  $('autostart').checked = view.autostart;
  $('notify').checked = view.settings.notify;
  if (document.activeElement !== $('gateway')) $('gateway').value = view.settings.gateway;
  $('version').textContent = view.version;
}

async function refresh() {
  view = await invoke('get_state');
  renderSnapshot(view.snapshot);
  renderAccount();
  renderSettings();
}

function showError(id, message) {
  $(id).textContent = message;
  $(id).hidden = !message;
}

$('check').addEventListener('click', () => {
  $('check').disabled = true;
  $('hero').dataset.status = 'checking';
  $('status-label').textContent = STATUS.checking[0];
  invoke('check_now');
});

$('account-form').addEventListener('submit', async (e) => {
  e.preventDefault();
  showError('form-error', '');
  $('save').disabled = true;
  try {
    await invoke('save_account', { username: $('username').value, password: $('password').value });
    $('password').value = '';
    editing = false;
    await refresh();
  } catch (err) {
    showError('form-error', String(err));
  } finally {
    $('save').disabled = false;
  }
});

$('toggle-password').addEventListener('click', () => {
  const input = $('password');
  const show = input.type === 'password';
  input.type = show ? 'text' : 'password';
  $('toggle-password').textContent = show ? '隐藏' : '显示';
});

$('edit').addEventListener('click', () => {
  editing = true;
  $('username').value = view.settings.username;
  renderAccount();
  $('password').focus();
});

$('cancel').addEventListener('click', () => {
  editing = false;
  $('password').value = '';
  showError('form-error', '');
  renderAccount();
});

// 两步确认，避免误删
let removeTimer = 0;
$('remove').addEventListener('click', async () => {
  const btn = $('remove');
  if (!removeTimer) {
    btn.textContent = '确认移除';
    removeTimer = setTimeout(() => {
      btn.textContent = '移除';
      removeTimer = 0;
    }, 3000);
    return;
  }
  clearTimeout(removeTimer);
  removeTimer = 0;
  btn.textContent = '移除';
  await invoke('clear_account');
  $('username').value = '';
  await refresh();
});

function bindSwitch(id, command) {
  $(id).addEventListener('change', async (e) => {
    const enabled = e.target.checked;
    try {
      await invoke(command, { enabled });
    } catch (err) {
      e.target.checked = !enabled;
      console.error(err);
    }
  });
}
bindSwitch('auto-login', 'set_auto_login');
bindSwitch('autostart', 'set_autostart');
bindSwitch('notify', 'set_notify');

$('gateway').addEventListener('change', async (e) => {
  try {
    e.target.value = await invoke('set_gateway', { gateway: e.target.value });
    showError('gateway-error', '');
  } catch (err) {
    showError('gateway-error', String(err));
  }
});

$('open-log').addEventListener('click', () => invoke('open_log'));

listen('state', (e) => {
  if (view) view.snapshot = e.payload;
  renderSnapshot(e.payload);
});
listen('settings', refresh);

refresh();
