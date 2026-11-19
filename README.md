# BUPT Autologin

北邮校园网 (buptportal) 掉线自动认证工具，支持 Windows、macOS、Linux。

常驻系统托盘 / 菜单栏，开机、换楼、睡眠唤醒后检测到校园网未认证，就自动登录，不用再手动打开认证页。

> 非官方工具，与北京邮电大学信息化技术中心无关。

## 特点

- **装好就不用管**：首次填一次学号和密码，之后开机自动在后台运行
- **省资源**：后台常驻约 25 MB 内存，空闲时 CPU 占用为 0。主界面关闭后 WebView 会被销毁，不留在后台
- **省流量**：平时每 3 秒只读一次本机网卡 IP（不发网络请求）；IP 变化、睡眠唤醒时立即检测，否则每 60 秒才探测一次外网
- **密码安全**：密码存放在系统凭据库（macOS 钥匙串 / Windows 凭据管理器 / Linux Secret Service），不写入任何明文文件
- **不受代理影响**：检测和登录请求都直连网关，开着 Clash、Tailscale 等代理或 VPN 也能正常工作。macOS 上请求绑定物理网卡，系统弹出认证窗口、暂时停用 Wi-Fi 默认路由时也能连上网关
- **失败退避**：密码错误等情况下，重试间隔从 30 秒逐步拉长到 10 分钟，避免频繁请求

## 安装

到 [Releases](../../releases) 下载对应系统的安装包：

| 系统 | 文件 |
| --- | --- |
| Windows | `*_x64-setup.exe` 或 `*.msi` |
| macOS（Apple 芯片） | `*_aarch64.dmg` |
| macOS（Intel） | `*_x64.dmg` |
| Linux | `*.deb` 或 `*.AppImage` |

**macOS 首次打开**：安装包没有经过 Apple 公证，首次打开时会被拦截。请在「系统设置 → 隐私与安全性」里点「仍要打开」，或者执行：

```bash
xattr -dr com.apple.quarantine "/Applications/BUPT Autologin.app"
```

**Linux**：托盘图标需要 `libayatana-appindicator3`（GNOME 还需要安装 AppIndicator 扩展），密码保存需要 GNOME Keyring 或 KWallet。

## 使用

1. 打开应用，填写学号和校园网密码，点「保存并登录」
2. 关闭窗口，应用会继续在托盘 / 菜单栏运行
3. 托盘图标颜色表示当前状态：绿色是已联网，橙色是正在登录，红色是登录失败或未设置账号，灰色是未连接校园网

托盘菜单里可以立即检测、暂停自动登录、开关开机启动。

## 工作原理

1. 访问 `http://captive.apple.com/hotspot-detect.html` 和 `http://connect.rom.miui.com/generate_204` 判断能否上网
2. 不能上网时，检查认证网关 `http://10.3.8.216` 是否可达，可达说明连在校园网 (buptportal) 里
3. 和网页一样，先打开 `/index` 拿到会话 cookie，再向 `/login` 提交表单 `user`、`pass`
4. 每 2 秒查询一次 `/dial`，直到返回 `{"code": 0}`（拨号完成），最后再检测一次外网确认。提交被拒绝时，网关在跳转地址的 `errmsg` 里给出原因，会显示在界面上

本工具面向无线网 buptportal；有线网使用其他网关，需要的话可以在「设置 → 高级」里修改网关地址。

每一步的请求和响应状态都会写入日志（不含密码），主界面底部「查看日志」可以打开。登录失败时，网关最后返回的页面会保存到配置目录下的 `last-login-response.html`。

## 从源码构建

需要 [Rust](https://rustup.rs) 和 Node.js；Linux 还需要 `libwebkit2gtk-4.1-dev libayatana-appindicator3-dev librsvg2-dev libdbus-1-dev`。

```bash
npm install
npm run dev     # 开发运行
npm run build   # 构建当前平台的安装包
cargo test --manifest-path src-tauri/Cargo.toml
```

推送 `v*` 标签后，GitHub Actions 会自动构建三个平台的安装包，并创建草稿 Release。

## License

MIT
