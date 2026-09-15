'use strict';
const { spawn } = require('node:child_process');
const path = require('node:path');

// Do not inherit mailbox credentials or server policy from the launching shell.
function backendEnvironment(source, dataDir) {
  const env = {};
  for (const key of ['PATH', 'Path', 'SystemRoot', 'SYSTEMROOT', 'WINDIR', 'TEMP', 'TMP', 'TMPDIR', 'HOME', 'USERPROFILE', 'LOCALAPPDATA', 'APPDATA', 'LANG', 'LC_ALL', 'TZ']) {
    if (source[key] !== undefined) env[key] = source[key];
  }
  env.SLIP_HOME = dataDir;
  env.SLIP_WEB_PORT = '0';
  return env;
}
function backendPath(resources, platform) {
  return path.join(resources, 'backend', platform === 'win32' ? 'slip-web.exe' : 'slip-web');
}
function localUrl(value, origin, downloads = false) {
  try {
    const url = new URL(value);
    return url.origin === origin && !url.username && !url.password &&
      (url.pathname === '/' || (downloads && /^\/download\/[0-9a-f]{32}$/.test(url.pathname)));
  } catch { return false; }
}
function startBackend({ executable, dataDir, environment = process.env, timeout = 20000 }) {
  const child = spawn(executable, ['--parent-stdio'], {
    env: backendEnvironment(environment, dataDir),
    stdio: ['pipe', 'pipe', 'pipe'], windowsHide: true,
  });
  let closed = false;
  const stop = () => {
    if (closed) return;
    closed = true;
    child.stdin.end(); // Rust exits on EOF, including after an Electron crash.
    const fallback = setTimeout(() => { if (child.exitCode === null) child.kill(); }, 1500);
    fallback.unref();
    child.once('exit', () => clearTimeout(fallback));
  };
  child.stdin.on('error', () => {});
  // Never forward backend stderr: it may include account identifiers or paths.
  child.stderr.resume();
  const ready = new Promise((resolve, reject) => {
    let pending = '';
    const timer = setTimeout(() => { stop(); reject(new Error('本地通信服务启动超时')); }, timeout);
    child.once('error', () => { clearTimeout(timer); reject(new Error('无法启动本地通信服务')); });
    child.once('exit', () => { clearTimeout(timer); reject(new Error('本地通信服务已退出')); });
    child.stdout.on('data', bytes => {
      pending = (pending + bytes.toString('utf8')).slice(-4096);
      const match = pending.match(/Slip Web: (http:\/\/127\.0\.0\.1:(\d+)\/#([0-9a-f]{32}))/);
      if (match && Number(match[2]) > 0 && Number(match[2]) <= 65535) {
        clearTimeout(timer);
        const url = match[1];
        resolve({ child, stop, url, origin: new URL(url).origin });
      }
    });
  });
  return { child, stop, ready };
}
module.exports = { backendEnvironment, backendPath, localUrl, startBackend };
