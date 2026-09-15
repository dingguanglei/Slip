#!/usr/bin/env python3
"""Archive a packaged desktop application; preserve macOS symlinks and Linux sandbox permissions."""
import json
import os
from pathlib import Path
import shutil
import stat
import subprocess
import sys
import zipfile

ROOT = Path(__file__).resolve().parents[2]
VERSION = json.loads((ROOT / 'desktop/package.json').read_text())['version']
OUT = ROOT / 'dist' / f'v{VERSION}'
TARGETS = {'mac': ('darwin', 'arm64'), 'win': ('win32', 'x64'),
           'linux-arm64': ('linux', 'arm64'), 'linux-x64': ('linux', 'x64')}


def archive(key):
    platform, arch = TARGETS[key]
    source = OUT / f'Slip-{platform}-{arch}'
    if not source.is_dir():
        raise SystemExit(f'Package first: {source.name}')
    if platform != 'linux':
        suffix = 'macos-arm64' if key == 'mac' else 'windows-x64'
        destination = OUT / f'Slip-{VERSION}-{suffix}.zip'
        with zipfile.ZipFile(destination, 'w', zipfile.ZIP_DEFLATED) as zipped:
            for directory, dirs, files in os.walk(source, followlinks=False):
                for name in dirs + files:
                    item = Path(directory) / name
                    relative = item.relative_to(OUT).as_posix()
                    if item.is_symlink():
                        info = zipfile.ZipInfo(relative)
                        info.create_system = 3
                        info.external_attr = (stat.S_IFLNK | 0o777) << 16
                        zipped.writestr(info, os.readlink(item))
                    elif item.is_file():
                        zipped.write(item, relative)
    else:
        deb_arch = 'arm64' if arch == 'arm64' else 'amd64'
        stage = ROOT / 'desktop/staging' / f'deb-{deb_arch}'
        shutil.rmtree(stage, ignore_errors=True)
        app_dir = stage / 'opt/slip'
        shutil.copytree(source, app_dir, symlinks=True)
        (app_dir / 'slip').chmod(0o755)
        (app_dir / 'resources/backend/slip-web').chmod(0o755)
        (app_dir / 'chrome-sandbox').chmod(0o4755)
        control = stage / 'DEBIAN'
        control.mkdir()
        (control / 'control').write_text(f'''Package: slip-desktop
Version: {VERSION}
Section: net
Priority: optional
Architecture: {deb_arch}
Maintainer: Slip contributors
Homepage: https://github.com/dingguanglei/Slip
Installed-Size: {sum(p.stat().st_size for p in app_dir.rglob('*') if p.is_file()) // 1024}
Depends: libc6 (>= 2.35), libnss3, libatk1.0-0 | libatk1.0-0t64, libatk-bridge2.0-0 | libatk-bridge2.0-0t64, libgtk-3-0 | libgtk-3-0t64, libgbm1, libasound2 | libasound2t64, libx11-6, libxcb1, libxcomposite1, libxdamage1, libxfixes3, libxrandr2, libdrm2, libglib2.0-0 | libglib2.0-0t64
Description: Private conversations over email
 End-to-end encrypted desktop messaging over IMAP and SMTP.
 Messages and attachments are stored locally after receipt.
''')
        applications = stage / 'usr/share/applications'
        applications.mkdir(parents=True)
        (applications / 'slip.desktop').write_text('''[Desktop Entry]
Type=Application
Name=Slip
Comment=Private conversations over email
Exec=/opt/slip/slip %U
Icon=slip
Terminal=false
Categories=Network;InstantMessaging;
StartupWMClass=Slip
''')
        icons = stage / 'usr/share/icons/hicolor/512x512/apps'
        icons.mkdir(parents=True)
        shutil.copyfile(ROOT / 'desktop/icon.png', icons / 'slip.png')
        destination = OUT / f'Slip-{VERSION}-ubuntu-{deb_arch}.deb'
        subprocess.run(['dpkg-deb', '--root-owner-group', '--build', str(stage), str(destination)], check=True)
    print(destination)


if __name__ == '__main__':
    for target in sys.argv[1:] or TARGETS:
        if target not in TARGETS:
            raise SystemExit('Choose mac, win, linux-arm64 or linux-x64')
        archive(target)
