# Contributing

欢迎通过 Issue 讨论问题或提交范围清晰的 Pull Request。提交前请说明具体场景、预期行为、实际行为，以及操作系统和 Slip 版本。

## 开发

安装 Rust stable、Node.js 22+ 和 npm，按 [构建说明](docs/BUILDING.md) 启动应用。提交前完成：

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo build --locked --bins
node --check web/app.js
node --check desktop/main.cjs
node --check desktop/runtime.cjs
```

修改收发或本地存储时，应在隔离环境验证公钥交换、发送失败、附件完整性、退出登录，以及保存成功前不得删除远端副本的行为。在 PR 描述中写明验证范围与结果，不附私人邮件或凭证。

## 公开内容边界

提交生产代码、必要文档和脱敏的产品截图。邮箱实测脚本、测试模块、演示入口、虚构会话种子、临时日志、测试数据和本地工作记录均保留在仓库之外，不纳入提交或发行包。

不要提交邮箱授权码、访问令牌、身份密钥、邮箱导出、聊天数据库、附件目录、个人绝对路径或真实账号截图。构建产物仅通过 Release 分发。

界面默认使用浅色主题。保持图标、间距、字号和交互一致；图标按钮必须有可访问名称，错误与安全异常必须让用户看得见。

## 安全问题

不要在公开 Issue 中粘贴敏感材料。按 [安全说明](SECURITY.md) 报告漏洞。
