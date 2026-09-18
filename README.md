# LC-Deck

局域网远控工具（类 ToDesk），**仅限本地局域网使用**，无需公网、无需注册账号。
同一份二进制既是被控端也是主控端，支持 macOS 与 Windows 互控。

![平台](https://img.shields.io/badge/platform-macOS%20%7C%20Windows-blue) ![协议](https://img.shields.io/badge/protocol-TCP%2FUDP%20LAN-green)

## 功能

- **局域网自动发现**：UDP 广播，设备列表实时刷新，也可手动输入 IP 连接
- **远程桌面**：屏幕实时画面（JPEG 编码，默认 24 fps / 70 画质 / 最大宽 1920，均可在设置中调整，会话中即时生效）
- **键鼠控制**：鼠标移动/左右中键/滚轮、键盘与组合键转发；被控端为 macOS 时 Ctrl 自动映射为 Command
- **接入确认**：被控时弹窗确认（设备名 / ID / IP / 模式），或设置控制密码 + 自动接受
- **仅观看模式**：只看画面，不转发键鼠
- **剪贴板同步**：会话中双向同步文本
- **隐私开关**：一键「关闭允许被控制」，广播立即标记为不可控，对端无法连接

## 快速开始

### macOS

从源码构建：

```bash
cargo build --release
./target/release/lc_deck
```

从 [GitHub Releases](../../releases) 下载 `lc_deck-v*-aarch64-apple-darwin.tar.gz`（Apple 芯片）
或 `x86_64-apple-darwin`（Intel），解压后即可。

#### 首次打开被 Gatekeeper 拦截

本项目**没有 Apple Developer ID 签名**，macOS 会对从浏览器下载的文件打上隔离标记，
首次双击会弹「无法验证『lc_deck』是否包含恶意软件」。这不是软件有问题。

**推荐：用安装脚本**（产物内已附带），它会清除隔离标记并把程序包装成 `LC-Deck.app`
装到 `/Applications`，双击即用：

```bash
cd ~/Downloads/lc_deck-v*-apple-darwin
./install-macos.sh
```

包装成 `.app` 还有一个好处：屏幕录制 / 辅助功能权限是按 bundle id 记录的，
脚本使用固定的 `top.swwarn.lcdeck`，**升级版本不用重新授权**。

不想跑脚本的话，直接移除隔离标记也可以（对目录递归，文件内容不受影响）：

```bash
xattr -dr com.apple.quarantine ~/Downloads/lc_deck-v*-apple-darwin
```

另外两个等效办法：

- Finder 中 **右键 → 打开**，弹窗里会多出一个「打开」按钮
- 若已经点过「好」，到 **系统设置 → 隐私与安全性** 底部点「仍要打开」

> 想从根上消除该提示，需要用 Developer ID 证书签名并公证（见下方「签名与公证」）。

首次在 **被控端** 使用需要授予两项系统权限（应用内「🔑 macOS 权限说明」有直达按钮）：

1. **屏幕录制** —— 否则画面黑屏或报错
2. **辅助功能** —— 否则无法注入键鼠

授权后若不生效，重启应用。

### Windows

在 Windows 机器上安装 [Rust](https://rustup.rs/)（MSVC 工具链）后：

```powershell
cargo build --release
.\target\release\lc_deck.exe
```

或直接下载 [GitHub Releases](../../releases) 中的 `lc_deck-v*-x86_64-pc-windows-msvc.zip`。

> Windows Defender 首次运行可能拦截未签名程序，选择「仍要运行」即可。

#### 防火墙放行（Windows 上最常见的问题）

Windows 防火墙默认**阻止入站**，不放行的话这台机器既不会被发现、也无法被连接
（表现为：别的设备列表里没有它，或点「控制」一直转圈后失败）。

首次运行通常会弹窗，对**专用网络**放行即可。若没弹窗或已点过拒绝，管理员 PowerShell 执行：

```powershell
netsh advfirewall firewall add rule name="LC-Deck TCP" dir=in action=allow protocol=TCP localport=48500 profile=private
netsh advfirewall firewall add rule name="LC-Deck UDP" dir=in action=allow protocol=UDP localport=48501 profile=private
```

端口改过的话把上面的 `48500` / `48501` 换成实际值。

### 互相控制

1. 两台设备连入**同一局域网**，各自运行 LC-Deck
2. 主窗口会出现对方设备（绿点 = 允许被控）
3. 点 **控制**（可输入键鼠）或 **仅观看**
4. 对端弹窗确认后，自动打开远控窗口

远控窗口支持：全屏切换、仅观看切换、发送本地剪贴板、状态栏显示 fps / RTT / 分辨率。

### 设备发现不到？按顺序排查

1. **对端确实在运行** —— 且没被最小化到后台服务
2. **同一网段** —— 若一台连了 VPN 或虚拟机网卡，广播可能发到别的网络。
   本机会向**所有网卡的子网广播地址**通告（不只是 `255.255.255.255`），
   但仍需两边在同一广播域
3. **Windows 防火墙** —— 见上文，需要放行入站 TCP/UDP
4. **macOS 防火墙** —— 系统设置 → 网络 → 防火墙，若开启需允许 `lc_deck` 接收传入连接
5. **兜底：手动连接** —— 在主窗口底部输入对端 IP（对端的 IP 显示在它的「本机 IP」一栏），
   不依赖广播。能连上就说明只是发现问题，不是网络不通

启动日志会打印实际广播目标（`RUST_LOG=info ./lc_deck`），可用于确认网卡枚举是否正确。

## 连接方式

| 端口 | 协议 | 用途 |
|------|------|------|
| 48500 | TCP | 控制信道 + 视频流 |
| 48501 | UDP | 广播发现（每 2 秒通告，7 秒过期） |

两端口均可在设置中修改。

## 安全说明

- 所有流量仅在内网传输，不做任何中继/穿透
- 传输未加密：建议仅在**可信局域网**使用；可设置控制密码防止未授权接入
- 接入确认弹窗默认开启（除非设置了密码并勾选自动接受）

## 架构

```
src/
├── main.rs           入口
├── protocol.rs       消息协议（长度前缀 + 类型 + JSON/二进制载荷）
├── discovery.rs      UDP 广播发现
├── server.rs         被控端（监听/握手/鉴权/确认/会话）
├── capture.rs        屏幕捕获（VideoRecorder 优先 + 轮询兜底，编码独立线程）
├── input_exec.rs     键鼠注入（enigo）
├── client.rs         主控端（连接/解码/输入发送/心跳）
├── ui.rs             主窗口（本机卡片/设备列表/设置/弹窗/Toast）
├── ui_remote.rs      远控视口（画面渲染 + 输入捕获）
├── theme.rs          视觉主题（配色令牌 + 卡片/胶囊/开关等复用组件）
├── keys.rs           按键名映射
├── clipboard_sync.rs 剪贴板封装
├── config.rs         配置持久化
└── platform.rs       平台适配（设备ID/权限入口/CJK字体/时基）
```

核心设计：**归一化坐标**。鼠标位置以 0..1 传输，被控端按自身屏幕尺寸还原，
因此 Retina/普通屏、不同分辨率之间天然对齐，无需协商 DPI。

## 打包与发布

```bash
./scripts/package.sh          # 当前平台
git tag v0.1.6 && git push --tags   # 触发 CI 三平台构建并发布 Release
```

> CI 只在 **推送 `v*` 标签** 或手动 `workflow_dispatch` 时运行，仅推 `main` 不会触发。

## 签名与公证（可选）

默认发布产物是**未签名**的，因此 macOS 会弹 Gatekeeper 警告（见上文）。
若你有 Apple Developer Program 会员资格，可配置 CI 自动签名 + 公证，产物即可直接双击打开。

需要准备：

1. 从开发者后台下载 **Developer ID Application** 证书，导出为 `.p12`
2. 在 [appleid.apple.com](https://appleid.apple.com) 生成 **App 专用密码**
3. 在仓库 Settings → Secrets 添加：

| Secret | 内容 |
|--------|------|
| `MACOS_CERTIFICATE` | `.p12` 的 Base64（`base64 -i cert.p12`） |
| `MACOS_CERTIFICATE_PASSWORD` | `.p12` 导出密码 |
| `MACOS_SIGN_IDENTITY` | 形如 `Developer ID Application: 你的名字 (TEAMID)` |
| `APPLE_ID` | Apple ID 邮箱 |
| `APPLE_APP_PASSWORD` | App 专用密码 |
| `APPLE_TEAM_ID` | 10 位 Team ID |

未配置时 CI 会跳过签名步骤，构建行为与现在完全一致。

## 已知限制

- 未加密传输（内网可信环境可接受；后续可引入 TLS）
- 仅捕获主显示器
- JPEG 编码在 CPU 进行，4K 高帧率下建议调低「画面最大宽度」
- Linux 未官方支持（依赖均已跨平台，理论上可自行编译）
