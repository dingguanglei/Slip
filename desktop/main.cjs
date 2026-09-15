'use strict';
const { app, BrowserWindow, Menu, dialog, session, nativeTheme } = require('electron');
const path = require('node:path');
const { startBackend, backendPath, localUrl } = require('./runtime.cjs');

let mainWindow, backend, quitting = false;
app.setName('Slip');
app.setAppUserModelId('org.slip.desktop');
// No persistent renderer session or remembered login.
const partition = `slip-${process.pid}`;
if (!app.requestSingleInstanceLock()) app.quit();
else {
  app.on('second-instance', () => {
    if (mainWindow) { if (mainWindow.isMinimized()) mainWindow.restore(); mainWindow.show(); mainWindow.focus(); }
  });
  app.whenReady().then(async () => {
    nativeTheme.themeSource = 'light';
    const dataOverride = !app.isPackaged && process.argv.find(arg => arg.startsWith('--data-dir='));
    const dataDir = dataOverride ? path.resolve(dataOverride.slice(11)) : path.join(app.getPath('home'), '.slip');
    const executable = app.isPackaged ? backendPath(process.resourcesPath, process.platform) :
      path.join(__dirname, '..', 'target', 'debug', process.platform === 'win32' ? 'slip-web.exe' : 'slip-web');
    backend = startBackend({ executable, dataDir });
    try {
      const ready = await backend.ready;
      if (quitting) return;
      const browserSession = session.fromPartition(partition, { cache: false });
      browserSession.setPermissionRequestHandler((_contents, _permission, callback) => callback(false));
      browserSession.setPermissionCheckHandler(() => false);
      // The UI and all attachments come from our own ephemeral loopback server.
      browserSession.webRequest.onBeforeRequest((details, callback) => {
        const allowed = details.url.startsWith(ready.origin + '/') || details.url.startsWith('blob:' + ready.origin + '/');
        callback({ cancel: !allowed });
      });
      browserSession.on('will-download', (event, item, contents) => {
        if (!mainWindow || contents !== mainWindow.webContents || !localUrl(item.getURL(), ready.origin, true) || !new URL(item.getURL()).pathname.startsWith('/download/')) {
          event.preventDefault(); return;
        }
        // Keep Electron's native Save As dialog; never silently choose a path.
        item.setSaveDialogOptions({ title: '保存附件', buttonLabel: '保存' });
      });
      mainWindow = new BrowserWindow({
        title: 'Slip', width: 1180, height: 820, minWidth: 820, minHeight: 640,
        show: false, backgroundColor: '#f7f9f5', autoHideMenuBar: true,
        icon: path.join(__dirname, 'icon.png'),
        webPreferences: { partition, nodeIntegration: false, contextIsolation: true, sandbox: true,
          webSecurity: true, allowRunningInsecureContent: false, webviewTag: false, spellcheck: false },
      });
      mainWindow.webContents.setWindowOpenHandler(() => ({ action: 'deny' }));
      mainWindow.webContents.on('will-attach-webview', event => event.preventDefault());
      mainWindow.webContents.on('will-navigate', (event, url) => { if (!localUrl(url, ready.origin, true)) event.preventDefault(); });
      mainWindow.webContents.on('will-redirect', (event, url) => { if (!localUrl(url, ready.origin, true)) event.preventDefault(); });
      mainWindow.webContents.on('page-title-updated', event => { event.preventDefault(); });
      mainWindow.once('ready-to-show', () => mainWindow.show());
      mainWindow.on('closed', () => { mainWindow = null; app.quit(); });
      const menu = [
        ...(process.platform === 'darwin' ? [{ label: 'Slip', submenu: [{ role: 'about' }, { type: 'separator' }, { label: '退出 Slip', role: 'quit' }] }] : []),
        { label: '编辑', submenu: [{ role: 'undo' }, { role: 'redo' }, { type: 'separator' }, { role: 'cut' }, { role: 'copy' }, { role: 'paste' }, { role: 'selectAll' }] },
        { label: '窗口', submenu: [{ role: 'minimize' }, { role: 'zoom' }, { type: 'separator' }, { label: '退出 Slip', role: 'quit' }] },
      ];
      Menu.setApplicationMenu(Menu.buildFromTemplate(menu));
      await mainWindow.loadURL(ready.url);
      ready.child.once('exit', () => {
        if (!quitting) { dialog.showErrorBox('Slip', '本地通信服务已退出。请重新打开应用，本地聊天记录仍保留。'); app.quit(); }
      });
    } catch (error) {
      if (!quitting) dialog.showErrorBox('Slip 无法启动', error.message);
      app.quit();
    }
  });
  app.on('before-quit', () => { quitting = true; if (backend) backend.stop(); });
  app.on('window-all-closed', () => app.quit());
}
