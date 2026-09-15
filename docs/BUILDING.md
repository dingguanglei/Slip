# 构建与发行

## 本机开发

需要 Rust stable、Node.js 22+、npm 和平台原生链接器。Windows 原生构建使用 Visual Studio C++ Build Tools；macOS 使用 Xcode Command Line Tools。Ubuntu 需要 C/C++ 编译工具和 Electron 的 GTK/NSS/音频库。

```bash
cargo build --locked --bin slip-web
cd desktop
npm ci
node node_modules/electron/install.js
npm start
```

应用使用当前用户的 `~/.slip`（Windows 为 `%USERPROFILE%\.slip`）保存数据。开发时可向 Electron 显式传入 `--data-dir=/absolute/path` 选择独立目录；发行应用忽略该参数。

## 平台目标

| 打包参数 | Electron 架构 | Rust target |
| --- | --- | --- |
| `mac` | darwin-arm64 | `aarch64-apple-darwin` |
| `win` | win32-x64 | `x86_64-pc-windows-msvc`（Windows 原生） |
| `linux-arm64` | linux-arm64 | `aarch64-unknown-linux-gnu` |
| `linux-x64` | linux-x64 | `x86_64-unknown-linux-gnu` |

在对应系统安装 target 后构建后端：

```bash
rustup target add aarch64-apple-darwin
cargo build --release --locked --target aarch64-apple-darwin --bin slip-web
node desktop/scripts/package.cjs mac
python3 desktop/scripts/archive.py mac
```

将示例目标替换为表中对应项。`package.cjs` 只复制白名单应用文件和指定架构的 Rust 后端；`archive.py` 生成 ZIP 或 DEB。产物位于 `dist/v0.2.0/`。Ubuntu 归档需要 `dpkg-deb`；DEB 使用 root 所有权并保留 `chrome-sandbox` 的 setuid 权限。

Linux 上交叉构建 Windows 时可使用 `x86_64-pc-windows-gnu` 和 MinGW，打包脚本会选择该目标。也可用 `SLIP_WINDOWS_TARGET` 显式指定。这只是链接目标选择，不改变应用的 x86-64 架构。

Ubuntu 发布构建使用 glibc 2.35 作为后端基线，应在 Ubuntu 22.04 或通过匹配的交叉工具链构建。不要在较新系统构建后直接声明支持更旧的 glibc。

macOS 的 Electron 44 要求 macOS 13+。ZIP 保留应用包内的符号链接与执行权限。开发者签名和 Apple 公证需要发行者自行配置；当前自动化不包含签名凭证。

## 发布流程

1. 同步 `Cargo.toml`、`Cargo.lock`、`desktop/package.json`、锁文件和更新记录中的版本。
2. 完成代码审查、格式/静态检查、生产构建及公开内容扫描。
3. 推送源码与版本标签；`release.yml` 构建四个平台，打包并生成 `SHA256SUMS`。
4. 下载并核对产物，再检查 GitHub Release 上的平台、版本和安装说明。

CI 只构建和检查生产代码。邮箱验证材料、临时脚本、账号存储和构建目录不纳入仓库。不要将凭证写入 workflow、构建参数或发行说明。
