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

在源码目录里一条命令装好（构建 → 打包 `LC-Deck.app` → 在桌面建快捷入口 → 启动）：

```bash
./scripts/install-local.sh            # 加 --no-run 则只安装不启动
```

它会装在 `~/Applications/LC-Deck.app`，桌面上的 `LC-Deck.app` 是指向它的符号链接。
以后改了代码重新跑一次这个脚本即可，桌面入口不用重建。

只构建、直接跑：

```bash
cargo build --release
./target/release/lc_deck
```

图标由 `scripts/gen-icon.py` 现算（纯 Python，无第三方依赖，配色取自 `src/theme.rs`），
产物在 `scripts/icon-build/`，删掉即可重新生成。

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
4. **去对方设备上点「允许」** —— 对方窗口会自动弹到前台，弹出的确认框有 90 秒时限
5. 确认后主控端自动打开远控窗口

> 点完没反应通常是这一步：主控端在等对方确认，本机不会有任何进度条。
> 若对方始终没弹窗，多半是防火墙拦了入站，见上文排查。

远控窗口支持：全屏切换、仅观看切换、发送本地剪贴板，
状态栏显示 fps / **延迟估算** / 下行带宽 / 分辨率（鼠标悬停有分段明细）。

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

## 延迟

远控窗口状态栏的 **≈XX ms** 就是端到端延迟估算，它是三段实测值相加：

```
≈ 被控端（抓帧 → 写出发送缓冲） + 单程网络（RTT/2） + 本地解码
```

三段都在各自一端实测，不依赖两端时钟同步，因此数字可信。鼠标悬停能看到分段明细；
对端是旧版本（未回填流水线耗时）时只显示 RTT。

### 增量更新（v0.1.13 起）

远控时屏幕上真正变化的区域很小（鼠标、高亮、输入框），整帧 JPEG 等于把没变的
像素也编一遍。现在被控端按 128px 图块与上一帧比对，**只编码变化的块**：

| | 整帧 1080p | 8 个 128px 图块 |
|---|---|---|
| 编码 | 16.5 ms | **1.0 ms** |
| 数据量 | 245 KB | **16 KB** |
| 解码 | 7.6 ms | <1 ms |
| 上屏 | 整帧上传 8 MB | 局部上传 ~64 KB |

变化检测本身只要 0.45ms（1080p 全扫）。脏块超过 35% 时自动退回整帧
（那时逐块编码反而更慢：铺满 1080p 需 135 块共 17.3ms，略高于整帧 16.5ms）。

代价是**不能丢帧**：整帧可以丢旧的取新的，图块丢一块主控端就永远缺一块。
所以整条链路改成阻塞式背压（宁可慢下来也不丢），主控端用「同格只留最后一块」
的队列保证上屏量有上界。协议版本 ≥2 才启用，旧版对端自动回退整帧。

**优化前后各环节的处理方式**

| 环节 | 优化前 | 现在 |
|------|--------|------|
| 被控端抓帧 | 无论是否在操作，都按设定帧率固定等待，每帧白带最多一整个帧间隔（24fps → 41ms） | **操作期间解除节流**（近 600ms 内有键鼠输入即改为 8ms 上限），节奏交给流水线本身；空闲 600ms 后回落设定帧率省带宽 |
| 被控端发送 | 先等 50ms 取帧、写完帧才处理控制消息 | 每轮**先清空控制消息再发最新帧**，Pong 不再被一整帧的发送耗时挡在后面 |
| 主控端收帧 | 读与解码同一线程，解码 10~25ms 期间内核接收缓冲堆积旧帧，越慢延迟越高 | **读 / 解码分离**：读线程始终抽干内核缓冲，解码线程只保留最新帧，旧帧直接丢弃 |
| 主控端输入 | 鼠标移动逐条进无界队列；每发一条事件就查一次系统剪贴板；心跳按循环次数计 | **移动事件在入队时合并**（只保留最新落点，按键/点击严格保序）；剪贴板改为 600ms 轮询；心跳改固定 1s |
| 单帧处理 | 全图遍历两遍（RGBA→RGB、再转 Color32），纹理更新还整份克隆一次画面 | JPEG 直接用 `into_rgb8` 接管解码缓冲；纹理更新把 `Arc` 交给 egui，省掉每帧 8MB（4K 为 33MB）的纯拷贝 |

**实测成本构成**（Apple Silicon，release 构建，`cargo test --release bench_encode -- --ignored --nocapture`）

| 源分辨率 → 传输分辨率 | 降采样 + 去 alpha | JPEG 编码 | 帧大小 |
|---|---|---|---|
| 1920×1080（不缩放） | 1.2 ms | 16.8 ms | 245 KB |
| 2560×1600 → 1920×1200 | 12.9 ms | 22.8 ms | 358 KB |
| 3840×2160 → 1920×1080 | 13.4 ms | 16.8 ms | 364 KB |

主控端解码（`bench_decode`）：1920×1080 约 7.6 ms，1920×1200 约 10.1 ms。

也就是说这套**全帧 JPEG 流**架构的实测下限在 **30~60 ms** 量级
（被控端 20~36ms + 网络 2~10ms + 解码 8~10ms + 渲染），内网千兆约在 40ms 左右；
WiFi 上传输时间会明显变大，此时看状态栏的带宽读数就能判断瓶颈在画质设置还是链路。

> 想再往 10ms 级别压，需要换掉编码链路：硬件编码的 H.264/HEVC（macOS VideoToolbox /
> Windows Media Foundation）+ 低延迟解码器，或改成只传变化区域的增量更新。
> 这是另一个量级的改造，当前版本没做。

### 自己复现这些数字

```bash
cargo test --release bench_encode -- --ignored --nocapture   # 被控端编码
cargo test --release bench_decode -- --ignored --nocapture   # 主控端解码
```

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
./scripts/package.sh                    # 当前平台
git tag v0.1.11 && git push --tags      # 触发 CI 三平台构建并发布 Release
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
- JPEG 全帧编码在 CPU 进行，端到端延迟实测下限在 30~60ms 量级（见上文「延迟」）；
  想进 10ms 级别需要改造为硬件编码的 H.264/HEVC
- 两侧版本不一致时仍可连接，但对端为旧版本时看不到「延迟估算」的分段明细
- Linux 未官方支持（依赖均已跨平台，理论上可自行编译）
