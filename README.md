# LC-Deck

局域网远控工具（类 ToDesk），**仅限本地局域网使用**，无需公网、无需注册账号。
同一份二进制既是被控端也是主控端，支持 macOS 与 Windows 互控。

![平台](https://img.shields.io/badge/platform-macOS%20%7C%20Windows-blue) ![协议](https://img.shields.io/badge/protocol-TCP%2FUDP%20LAN-green)

## 功能

- **局域网自动发现**：UDP 广播，设备列表实时刷新，也可手动输入 IP 连接
- **远程桌面**：屏幕实时画面（JPEG 编码，默认 15 fps / 70 画质 / 最大宽 1920，均可在设置中调整）
- **键鼠控制**：鼠标移动/左右中键/滚轮、键盘与组合键转发；被控端为 macOS 时 Ctrl 自动映射为 Command
- **接入确认**：被控时弹窗确认（设备名 / ID / IP / 模式），或设置控制密码 + 自动接受
- **仅观看模式**：只看画面，不转发键鼠
- **剪贴板同步**：会话中双向同步文本
- **隐私开关**：一键「关闭允许被控制」，广播即刻下线

## 快速开始

### macOS

```bash
cargo build --release
./target/release/lc_deck
```

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
> 若提示防火墙，请对专用（家庭/工作）网络放行。

### 互相控制

1. 两台设备连入**同一局域网**，各自运行 LC-Deck
2. 主窗口会出现对方设备（绿点 = 允许被控）
3. 点 **控制**（可输入键鼠）或 **仅观看**
4. 对端弹窗确认后，自动打开远控窗口

远控窗口支持：全屏切换、仅观看切换、发送本地剪贴板、状态栏显示 fps / RTT / 分辨率。

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
├── ui.rs             主窗口（设备列表/设置/弹窗）
├── ui_remote.rs      远控视口（画面渲染 + 输入捕获）
├── keys.rs           按键名映射
├── clipboard_sync.rs 剪贴板封装
├── config.rs         配置持久化
└── platform.rs       平台适配（设备ID/权限入口/CJK字体）
```

核心设计：**归一化坐标**。鼠标位置以 0..1 传输，被控端按自身屏幕尺寸还原，
因此 Retina/普通屏、不同分辨率之间天然对齐，无需协商 DPI。

## 打包与发布

```bash
./scripts/package.sh          # 当前平台
git tag v0.1.0 && git push --tags   # 触发 CI 三平台构建并发布 Release
```

## 已知限制

- 未加密传输（内网可信环境可接受；后续可引入 TLS）
- 仅捕获主显示器
- JPEG 编码在 CPU 进行，4K 高帧率下建议调低「画面最大宽度」
- Linux 未官方支持（依赖均已跨平台，理论上可自行编译）
