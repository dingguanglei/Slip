'use strict';
const fs = require('node:fs/promises');
const path = require('node:path');
const root = path.resolve(__dirname, '../..');
const { version, devDependencies } = require('../package.json');
const targets = {
  mac: { platform: 'darwin', arch: 'arm64', rust: 'aarch64-apple-darwin', exe: 'slip-web' },
  win: { platform: 'win32', arch: 'x64', rust: process.env.SLIP_WINDOWS_TARGET || (process.platform === 'win32' ? 'x86_64-pc-windows-msvc' : 'x86_64-pc-windows-gnu'), exe: 'slip-web.exe' },
  'linux-arm64': { platform: 'linux', arch: 'arm64', rust: 'aarch64-unknown-linux-gnu', exe: 'slip-web' },
  'linux-x64': { platform: 'linux', arch: 'x64', rust: 'x86_64-unknown-linux-gnu', exe: 'slip-web' },
};
(async () => {
  const { packager } = await import('@electron/packager');
  const requested = process.argv.slice(2);
  for (const key of requested.length ? requested : Object.keys(targets)) {
    const target = targets[key];
    if (!target) throw new Error('Choose mac, win, linux-arm64 or linux-x64');
    const stage = path.join(root, 'desktop/staging', key);
    const appDir = path.join(stage, 'app');
    const backend = path.join(stage, 'backend');
    await fs.rm(stage, { recursive: true, force: true });
    await fs.mkdir(appDir, { recursive: true });
    await fs.mkdir(backend, { recursive: true });
    // Explicit allowlist: account stores, credentials and developer files never enter a package.
    for (const name of ['main.cjs', 'runtime.cjs', 'icon.png']) {
      await fs.copyFile(path.join(root, 'desktop', name), path.join(appDir, name));
    }
    await fs.writeFile(path.join(appDir, 'package.json'), JSON.stringify({
      name: 'slip-desktop', productName: 'Slip', version, main: 'main.cjs',
      description: 'Private conversations over email', author: 'Slip contributors', license: 'MIT',
    }, null, 2));
    const binary = path.join(root, 'target', target.rust, 'release', target.exe);
    await fs.copyFile(binary, path.join(backend, target.exe));
    await fs.chmod(path.join(backend, target.exe), 0o755);
    const outputs = await packager({
      dir: appDir, name: 'Slip', appBundleId: 'org.slip.desktop', appVersion: version, buildVersion: version,
      platform: target.platform, arch: target.arch, electronVersion: devDependencies.electron,
      out: path.join(root, 'dist', `v${version}`), overwrite: true, asar: true, prune: false,
      extraResource: [backend],
      icon: path.join(root, 'desktop', key === 'mac' ? 'icon.icns' : key === 'win' ? 'icon.ico' : 'icon.png'),
      darwinDarkModeSupport: false,
      win32metadata: { CompanyName: 'Slip', FileDescription: 'Slip private chat', ProductName: 'Slip' },
      ...(target.platform === 'linux' ? { executableName: 'slip' } : {}),
      ...(key === 'mac' ? { extendInfo: { NSHighResolutionCapable: true } } : {}),
    });
    for (const output of outputs) {
      await fs.copyFile(path.join(root, 'LICENSE'), path.join(output, 'SLIP-LICENSE.txt'));
      await fs.copyFile(path.join(root, 'docs/DESKTOP.md'), path.join(output, 'README.md'));
      console.log(output);
    }
  }
})().catch(error => { console.error(error); process.exitCode = 1; });
