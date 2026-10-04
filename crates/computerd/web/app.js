import RFB from '/novnc/core/rfb.js';

const KEY_STORE = 'computerKey';
const RETRY_MS = 2000;
const FLASH_MS = 3000;
const NOTICE_MS = 6000;
const PASTE_WAIT_MS = 250;
const SELECTION_SETTLE_MS = 40;
const XK_V = 0x76;
const XK_CONTROL_L = 0xffe3;
const XK_ALT_L = 0xffe9;
const IS_MAC = /Mac|iPhone|iPad/.test(navigator.platform);

const el = (id) => document.getElementById(id);
const statusEl = el('status');
const listEl = el('sessions');
const screenEl = el('screen');
const messageEl = el('message');
const messageText = el('message-text');
const reconnectBtn = el('reconnect');
const barEl = el('bar');
const barScreen = el('bar-screen');
const barTitle = el('bar-title');
const barSize = el('bar-size');
const lockState = el('lock-state');
const lockBtn = el('lock');
const lockLabel = el('lock-label');
const bannerEl = el('banner');
const clipNotice = el('clip-notice');

let sessions = [];
let selected = null;
let rfb = null;
let connected = false;
let keyRejected = false;
let initial = true;
let flashed = null;
let flashTimer = null;

function readFragment() {
  const params = new URLSearchParams(location.hash.slice(1));
  const given = params.get('key');
  if (given) {
    localStorage.setItem(KEY_STORE, given);
    params.delete('key');
    const rest = params.toString();
    history.replaceState(null, '', location.pathname + location.search + (rest ? '#' + rest : ''));
  }
  const screen = Number(params.get('screen'));
  return Number.isInteger(screen) && screen > 0 ? screen : null;
}

function storedKey() {
  return localStorage.getItem(KEY_STORE) || '';
}

function writeSelection() {
  const params = new URLSearchParams(location.hash.slice(1));
  if (selected) params.set('screen', String(selected));
  else params.delete('screen');
  const rest = params.toString();
  history.replaceState(null, '', location.pathname + location.search + (rest ? '#' + rest : ''));
}

function setStatus(text, bad = false) {
  statusEl.textContent = text;
  statusEl.className = bad ? 'bad' : '';
}

function showMessage(text) {
  messageText.textContent = text;
  messageEl.hidden = !text;
}

// The lock is per page. Every new connection starts locked. Clipboard code
// asks `isUnlocked()` before it forwards anything to the screen.
let unlocked = false;

function isUnlocked() {
  return unlocked && connected && rfb !== null;
}

function setUnlocked(value) {
  unlocked = value && connected && rfb !== null;
  if (!unlocked) cancelPaste();
  if (rfb) {
    rfb.viewOnly = !unlocked;
    if (unlocked) rfb.focus();
    else rfb.blur();
  }
  renderBar();
}

let noticeTimer = null;

function showClipboardNotice() {
  clipNotice.hidden = false;
  clearTimeout(noticeTimer);
  noticeTimer = setTimeout(() => {
    clipNotice.hidden = true;
  }, NOTICE_MS);
}

let pasteTimer = null;

function cancelPaste() {
  clearTimeout(pasteTimer);
  pasteTimer = null;
}

function isPasteShortcut(e) {
  if (e.code !== 'KeyV' || e.altKey || e.shiftKey) return false;
  return IS_MAC ? e.metaKey && !e.ctrlKey : e.ctrlKey && !e.metaKey;
}

let shortcutHeld = false;

// Hands the host text to the screen, then sends Ctrl+V so the focused app
// pastes it. Ctrl is pressed again in case it was released while the text was
// being read, and released afterwards unless the user still holds the key.
function finishPaste(text) {
  cancelPaste();
  const client = rfb;
  if (!isUnlocked()) return;
  if (text) client.clipboardPasteFrom(text);
  setTimeout(() => {
    if (rfb !== client || !isUnlocked()) return;
    if (IS_MAC) client.sendKey(XK_ALT_L, 'MetaLeft', false);
    client.sendKey(XK_CONTROL_L, 'ControlLeft', true);
    client.sendKey(XK_V, 'KeyV');
    if (!shortcutHeld) client.sendKey(XK_CONTROL_L, 'ControlLeft', false);
  }, text ? SELECTION_SETTLE_MS : 0);
}

// noVNC stops keydown events, so the page's own keydown listener runs first
// (capture) and lets the browser's paste event happen. That event carries the
// text without a permission prompt. The Clipboard API is the fallback.
screenEl.addEventListener(
  'keyup',
  (e) => {
    shortcutHeld = IS_MAC ? e.metaKey : e.ctrlKey;
  },
  true,
);

screenEl.addEventListener(
  'keydown',
  (e) => {
    shortcutHeld = IS_MAC ? e.metaKey : e.ctrlKey;
    if (!isUnlocked() || !isPasteShortcut(e)) return;
    e.stopPropagation();
    if (e.repeat || pasteTimer !== null) return;
    pasteTimer = setTimeout(async () => {
      pasteTimer = null;
      try {
        finishPaste(await navigator.clipboard.readText());
      } catch (error) {
        console.debug('clipboard read failed', error);
        showClipboardNotice();
      }
    }, PASTE_WAIT_MS);
  },
  true,
);

document.addEventListener(
  'paste',
  (e) => {
    if (pasteTimer === null) return;
    e.preventDefault();
    finishPaste(e.clipboardData?.getData('text/plain') ?? '');
  },
  true,
);

function copyToHost(text) {
  if (!isUnlocked() || !text) return;
  navigator.clipboard.writeText(text).catch((error) => {
    console.debug('clipboard write failed', error);
    showClipboardNotice();
  });
}

function renderBar() {
  barEl.hidden = !selected || !(connected || sessionOn(selected));
  if (barEl.hidden) return;
  const open = isUnlocked();
  barEl.classList.toggle('unlocked', open);
  barScreen.textContent = `Screen ${selected}`;
  barTitle.textContent = sessionOn(selected)?.title ?? '';
  const canvas = connected ? screenEl.querySelector('canvas') : null;
  barSize.textContent = canvas ? `${canvas.width}x${canvas.height}` : '';
  lockState.textContent = open ? 'You are in control' : 'View only';
  lockLabel.textContent = open ? 'Lock' : 'Unlock';
  lockBtn.classList.toggle('unlock', !open);
  lockBtn.disabled = !connected;
  lockBtn.title = open ? 'Stop sending mouse and keyboard to the screen' : 'Send mouse and keyboard to the screen';
}

function sessionOn(screen) {
  return sessions.find((s) => s.screen === screen);
}

function render() {
  listEl.replaceChildren();
  for (const s of sessions) {
    const item = document.createElement('li');
    const button = document.createElement('button');
    const title = document.createElement('span');
    title.className = 'title';
    title.textContent = s.title;
    const meta = document.createElement('span');
    meta.className = 'meta';
    const started = new Date(s.started * 1000).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
    const where = s.screen ? `Screen ${s.screen}` : 'no screen';
    const watching = s.viewers > 0 ? `, ${s.viewers} watching` : '';
    meta.textContent = `${where}, started ${started}${watching}`;
    button.append(title, meta);
    if (s.screen) button.addEventListener('click', () => select(s.screen));
    else button.disabled = true;
    item.append(button);
    const classes = [];
    if (s.screen && s.screen === selected) classes.push('selected');
    if (s.screen && s.screen === flashed) classes.push('flash');
    item.className = classes.join(' ');
    listEl.append(item);
  }
  if (sessions.length === 0) setStatus('No sessions yet.');
  else setStatus('');
  bannerEl.hidden = !(connected && selected && !sessionOn(selected));
  renderBar();
}

function flash(screen) {
  flashed = screen;
  clearTimeout(flashTimer);
  flashTimer = setTimeout(() => {
    flashed = null;
    render();
  }, FLASH_MS);
}

function disconnect() {
  if (rfb) {
    const old = rfb;
    rfb = null;
    old.disconnect();
  }
  connected = false;
  unlocked = false;
  screenEl.replaceChildren();
}

function select(screen) {
  initial = false;
  if (screen === selected && rfb) return;
  selected = screen;
  writeSelection();
  connect();
  render();
}

function connect() {
  disconnect();
  renderBar();
  if (!selected) {
    showMessage(sessions.length ? 'Pick a screen on the left.' : 'No sessions yet.');
    return;
  }
  const screen = selected;
  showMessage(`Connecting to screen ${screen}...`);
  const scheme = location.protocol === 'https:' ? 'wss' : 'ws';
  const key = storedKey();
  const url = `${scheme}://${location.host}/ws/${screen}?key=${encodeURIComponent(key)}`;
  const client = new RFB(screenEl, url, { credentials: { password: key } });
  client.scaleViewport = true;
  client.resizeSession = false;
  client.viewOnly = true;
  client.addEventListener('connect', () => {
    if (rfb !== client) return;
    connected = true;
    showMessage('');
    setUnlocked(false);
    render();
  });
  client.addEventListener('clipboard', (e) => copyToHost(e.detail.text));
  client.addEventListener('securityfailure', () => {
    if (rfb !== client) return;
    showMessage('The VNC password was refused. Open the link from the computer logs again.', false);
  });
  client.addEventListener('disconnect', () => {
    if (rfb !== client) return;
    rfb = null;
    connected = false;
    unlocked = false;
    screenEl.replaceChildren();
    if (keyRejected) showMessage('Wrong key. Open the link from the computer logs again.');
    else if (!sessionOn(screen)) showMessage(`Screen ${screen} ended.`);
    else showMessage(`Disconnected from screen ${screen}.`);
    render();
  });
  rfb = client;
}

reconnectBtn.addEventListener('click', connect);
lockBtn.addEventListener('click', () => setUnlocked(!isUnlocked()));

function handle(name, data) {
  if (name === 'sessions') {
    sessions = JSON.parse(data);
    if (initial) {
      initial = false;
      if (selected && sessionOn(selected)) connect();
      else if (selected) showMessage(`Screen ${selected} is not open.`);
      else showMessage(sessions.length ? 'Pick a screen on the left.' : 'No sessions yet.');
    } else if (!selected && !rfb) {
      showMessage(sessions.length ? 'Pick a screen on the left.' : 'No sessions yet.');
    }
    render();
  } else if (name === 'show') {
    const screen = Number(data);
    flash(screen);
    select(screen);
    render();
  }
}

async function listen() {
  for (;;) {
    try {
      const response = await fetch('/events', { headers: { 'X-Viewer-Key': storedKey() } });
      if (response.status === 401) {
        keyRejected = true;
        setStatus('Wrong key.', true);
        showMessage('Wrong key. Open the link from the computer logs again.');
        return;
      }
      if (!response.ok) throw new Error(String(response.status));
      keyRejected = false;
      const reader = response.body.pipeThrough(new TextDecoderStream()).getReader();
      let buffer = '';
      for (;;) {
        const { value, done } = await reader.read();
        if (done) break;
        buffer += value;
        let end;
        while ((end = buffer.indexOf('\n\n')) >= 0) {
          const block = buffer.slice(0, end);
          buffer = buffer.slice(end + 2);
          let name = 'message';
          const data = [];
          for (const line of block.split('\n')) {
            if (line.startsWith('event:')) name = line.slice(6).trim();
            else if (line.startsWith('data:')) data.push(line.slice(5).trimStart());
          }
          if (data.length) handle(name, data.join('\n'));
        }
      }
    } catch (error) {
      console.debug('event stream failed', error);
    }
    setStatus('Disconnected from the computer, retrying...', true);
    await new Promise((resolve) => setTimeout(resolve, RETRY_MS));
  }
}

window.addEventListener('hashchange', () => {
  const screen = readFragment();
  if (screen && screen !== selected) select(screen);
});

selected = readFragment();
writeSelection();
if (!storedKey()) {
  setStatus('No key. Open the link from the computer logs.', true);
  showMessage('No key. Open the link from the computer logs.');
} else {
  showMessage('Loading...');
  listen();
}
