# AGENTS.md

北邮校园网 (buptportal) 掉线自动认证工具。Tauri 2 桌面应用：Rust 后端常驻托盘/菜单栏，`ui/` 是无构建步骤的纯 HTML/JS 前端。面向用户的说明见 README.md。

## 常用命令

```bash
npm install
npm run dev                                         # 开发运行
npm run build                                       # 构建当前平台安装包
cd src-tauri && cargo test                          # 单元测试（含用本地端口模拟网关和外网探测地址的流程测试）
cd src-tauri && cargo clippy --all-targets          # 提交前保持无警告
```

## 代码结构（src-tauri/src）

| 文件 | 职责 |
| --- | --- |
| `main.rs` | Tauri 入口、前端可调用的 command、主窗口按需创建（关闭即销毁 WebView） |
| `monitor.rs` | 后台监控线程：网卡指纹、睡眠唤醒检测、检测/登录调度、退避、事件和日志 |
| `portal.rs` | 外网探测、网关可达判断、登录流程（/index → /login → 轮询 /dial）、解析网关错误页 |
| `http.rs` | 自带的极简 HTTP/1.1 客户端（仅 http + IPv4），macOS 上把连接绑定到物理网卡 |
| `config.rs` | `settings.json`（非敏感设置）和系统凭据库里的密码 |
| `tray.rs` / `icon.rs` | 托盘菜单；按状态颜色运行时绘制托盘图标 |

前端 `ui/app.js` 通过 `invoke` 调用 `main.rs` 里的 command，通过 `listen('state')` 接收 `Snapshot`。

## 必须知道的设计决策

- **不要换回 ureq/reqwest 等 HTTP 库。** macOS 检测到 Wi-Fi 需要网页认证时，会把该网卡的路由优先级设为 Never（系统日志里是 `SetInterfaceRank(en0) = Never`），直到用户在系统认证弹窗里登录。这期间没有默认路由，只有绑定网卡（`IP_BOUND_IF`）的连接能到达网关。现成的库不能指定网卡，所以 `http.rs` 用 socket2 自己发请求。
- **所有请求直连、不走代理**，绑定物理网卡也让 Tailscale 等 VPN 不影响检测。物理网卡的判定在 `http::physical_ipv4()`，`monitor.rs` 的网络变化指纹也用它。
- **省电省流量是硬要求**：平时每 3 秒只读本机网卡地址（无网络请求）；只有地址变化、睡眠唤醒、手动触发或到期（在线 60 秒 / 离线 30 秒）才发探测。不要加高频轮询。
- **避免账号被锁**：网关回复密码错误时不立即重试，按 `rejection_backoff` 拉长间隔；更新密码或登录成功后重新计数。合盖时系统每隔一两分钟 DarkWake 十来秒，Wi-Fi 重连后第一次外网探测常常失败，但本机其实仍已认证，此时提交登录会得到"用户名或密码错误 (0x01000004)"或"认证失败 (0x01000001)"。所以提交前先看 `/index` 是否已是认证成功页、再探测一次外网；提交后收到错误框也先确认外网，能上网就不算失败。登录途中系统睡着导致的超时不计失败；"自动登录失败"通知在恢复联网前只弹一次。
- **密码只存系统凭据库**（keyring），不写文件、不写日志。日志里只能出现学号。
- 网关的错误原因在 `<div class="ui error message">` 或跳转地址的 `errmsg` 参数里；页面脚本里的 `alert(...)` 多是模板，不能当成失败原因。

## 约定

- 代码注释、UI 文案、日志、提交信息都用中文，风格简洁，与现有代码一致。
- 改动网关交互时，在 `portal.rs` 的 `flow_tests` 里用本地模拟网关补测试；外网探测也用 `probe_server` 模拟，测试不访问外网。
- 版本号要同时改三处：`package.json`、`src-tauri/tauri.conf.json`、`src-tauri/Cargo.toml`（再跑一次 cargo 更新 `Cargo.lock`）。`package-lock.json` 里的版本一直没跟着改，不用动。
- 提交信息格式：首行一句话概括，空行后用 `- ` 列出要点，最后一条写"版本 x.y.z"。

## 发布流程

1. 在 main 上改版本号并提交，打带注释的标签：`git tag -a vX.Y.Z -m vX.Y.Z`
2. `git push origin main && git push origin vX.Y.Z`
3. `.github/workflows/release.yml` 会构建 macOS（aarch64、x86_64）、Linux、Windows，并创建草稿 Release（约 8 分钟）。用 `gh run list --workflow release.yml` / `gh run watch <id>` 查看进度
4. 全部成功后发布：`gh release edit vX.Y.Z --draft=false --latest`

## 排查问题（macOS）

- 应用日志：`~/Library/Logs/io.github.deyangliu123.buptautologin/bupt-autologin.log`（超过 512 KB 转存为 `.log.old`）
- 设置和最近一次登录失败的网关页面：`~/Library/Application Support/io.github.deyangliu123.buptautologin/`（`settings.json`、`last-login-response.html`）
- 睡眠/唤醒时间线：`pmset -g log | grep -E "\s(Sleep|Wake|DarkWake)\s"`
- 系统网络日志：zsh 里 `log` 是内建命令，要写 `/usr/bin/log show --start ... --end ... --style compact`；关注 `com.apple.captive`、`IPConfiguration`、`SetInterfaceRank`、`CaptiveNetworkAssistant`
- 本机安装新版：退出旧版 → 用 dmg 里的 app 替换 `/Applications/BUPT Autologin.app` → `xattr -dr com.apple.quarantine "/Applications/BUPT Autologin.app"`（安装包没有经过 Apple 公证）→ 重新打开。设置和钥匙串里的密码会保留
