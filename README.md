# AllVideo

使用 Rust 编写的 QQ 官方机器人，可从分享链接解析视频、图片和实况图集，并将媒体回复到群聊或私聊。

核心解析直接访问平台原站接口或分享页面。微信视频号通过元宝分享解析取得播放信息，再在本地下载、解密。抖音普通图集与实况图集支持基于已有登录态的原生 Web 请求签名。

[下载发布包](https://github.com/chaojisizhu/allvideo/releases/latest) · [配置示例](config.example.yaml)

## 功能

- 接入 QQ 官方 API v2，支持分享链接自动解析、命令触发、消息去重和媒体上传。
- 视频流式下载，支持 MP4、HLS 和 H.264/AAC DASH 音视频合流。
- 普通图集以一条 Markdown 消息发送，按图片实际宽高和 EXIF 方向等比显示。
- 抖音实况图集按原顺序下载短片，合成为一个 MP4；支持有界的异尺寸片段转换。
- B站 App 扫码登录并保存 Cookie，支持普通视频、分P和番剧适配。
- 按平台控制请求节奏，复用连接与会话，提供缓存、限流冷却和临时文件预算。
- 可选启用本机 yt-dlp、JMComic 和 OpenAI 图片溯源功能。

## 快速开始

QQ 消息收发需要在 [QQ 开放平台](https://q.qq.com/) 创建机器人，取得 AppID、AppSecret，并配置对应消息事件及使用权限。

### Linux 发布包

从 Releases 下载 `allvideo-v0.2.9-linux-x86_64.tar.gz`，解压后进入程序目录。发布包包含 `videorust`、裁剪版 FFmpeg 和 ffprobe，目标环境为 Ubuntu 22.04+ / Debian 12+ 的 x86_64 系统。运行环境需有 CA 证书、libgcc 和 OpenSSL 3。

```bash
cp config.example.yaml config.linux.yaml
chmod +x videorust ffmpeg ffprobe start.sh
```

编辑 `config.linux.yaml`，填写 `qqbot.appId`、`qqbot.appSecret`，再启动。

```bash
./start.sh check
./start.sh bot
```

启动脚本固定读取同目录的 `config.linux.yaml`。使用源码或直接调用二进制时，可通过 `--config` 指定其他配置文件。

### Windows 发布包

下载 `allvideo-v0.2.9-windows-x86_64.zip`，解压后在程序目录打开 PowerShell。

```powershell
Copy-Item config.example.yaml config.yaml
```

编辑 `config.yaml` 中的 QQ 凭据，再运行。

```powershell
.\videorust.exe check
.\videorust.exe bot
```

Windows 发布包中的主程序可独立启动。HLS/DASH 合流和实况视频合并需要另行安装 FFmpeg、ffprobe，并将它们加入 `PATH`。也可通过 `output.ffmpegCommand` 指定 FFmpeg 路径，程序会优先查找同目录的 ffprobe。

## Docker 部署

在仓库根目录准备配置和 Cookie 目录。

```bash
cp config.example.yaml config.yaml
mkdir -p cookies
```

编辑配置后执行。

```bash
docker compose build
docker compose run --rm allvideo check
docker compose up -d
docker compose logs --tail 100 -f
```

Compose 默认构建 `media` 镜像，以非 root 用户运行，配置及 Cookie 目录按只读方式挂载，临时媒体使用独立数据卷。默认限制为 0.5 CPU、256 MiB 内存和 64 个进程或线程。

| 构建目标 | 内容 |
| --- | --- |
| `runtime` | Rust 主程序、证书和运行库 |
| `media` | `runtime`、FFmpeg 和 ffprobe |
| `extras` | `media`、Python、yt-dlp 和 JMComic |
| `test` | 编译并执行默认测试 |

需要可选 Python 功能时，可将 Compose 的构建目标改为 `extras`。B站扫码会写入 Cookie 文件，使用 Docker 登录时需为该次运行提供可写的 Cookie 挂载，随后重启机器人加载。

## 配置与登录态

完整字段见 `config.example.yaml`，修改配置后重启进程。

| 配置项 | 用途 |
| --- | --- |
| `qqbot.appId`、`qqbot.appSecret` | QQ 官方机器人凭据 |
| `qqbot.autoParse`、`qqbot.commandPrefix` | 自动解析和手动命令 |
| `auth.cookies.<平台>` | 浏览器完整 Cookie 请求头 |
| `auth.cookieFiles.<平台>` | Netscape 格式 Cookie 文件 |
| `channels.yuanbaoCookie` | 微信视频号所需的元宝登录态 |
| `platformEnable` | 平台启停开关 |
| `api.proxy` | 网络请求失败时使用的代理 |
| `output.maxMarkdownImages` | 单条图集显示上限，默认 30 张 |
| `output.ffmpegCommand` | FFmpeg 命令或可执行文件路径 |
| `resources.maxTempBytes` | 活跃临时媒体的磁盘预算 |

QQ 与元宝凭据也可通过 `QQBOT_APP_ID`、`QQBOT_APP_SECRET`、`YUANBAO_COOKIE` 环境变量传入。代理可通过 `ALLVIDEO_PROXY` 设置。

### B站扫码登录

```bash
./start.sh login bilibili
```

Windows 对应命令为 `videorust.exe login bilibili`。用 B站 App 扫描生成的 `cookies/bilibili-login.png` 并确认，成功后保存 `cookies/bilibili.txt`，后续启动自动加载。

可用 `--cookie-file`、`--qr-file` 和 `--timeout-seconds` 指定输出位置与等待时间。Cookie 保存后会检查原站登录状态，访问验证及权限要求仍由原站决定。

### 其他平台

- **抖音**：部分公开内容可直接解析。需要登录的图集或实况内容使用 `auth.cookies.douyin`，Web 详情签名从该会话读取 UIFID。使用 Cookie 文件时，需保留 `douyin.com`、`iesdouyin.com` 各自正确的域范围。
- **小红书**：在 `auth.cookies.xiaohongshu` 配置登录态，分享链接应保留 `xsec_token`。
- **X / Twitter**：登录态使用 `auth.cookieFiles.twitter` 指向的 Netscape 文件，并启用 `ytdlp.enabled`；认证提取使用本机 yt-dlp。
- **微信视频号**：使用 `channels.yuanbaoCookie`。元宝登录过期时需要更新 Cookie。

配置文件、Cookie、环境文件、日志和构建产物已加入 Git 忽略规则。发布包使用空凭据示例配置。提交 Issue 或日志时，请移除账号凭据、Cookie、带签名的媒体地址及私人会话信息。

## 平台覆盖与限制

已包含 B站、抖音、小红书、快手、X、AcFun、皮皮虾、最右、微视、好看视频、虎牙、即梦、美拍、皮皮搞笑、豆包、头条与西瓜等原站适配。

B站普通视频、抖音视频与图集、小红书视频、X 视频与图集等链路经过实际下载或 QQ 回复验证。部分适配仍有实现或验证缺口，原站更新、作品权限和网络出口会影响解析结果。

默认关闭微博、YouTube、知乎、Instagram 和 TikTok。YouTube 动态播放器签名、PO Token 及部分平台权限内容仍有实现或验证缺口，配置开关可在 `platformEnable` 中调整。

### 抖音实况合并

程序按原页顺序处理取得的实况短片。同规格 H.264 视频和兼容 AAC 音轨直接复制编码流；不同视频规格在以下限制内统一画布后合并。

- 每组最多 12 段，同时处理一组实况图集。
- 异规格转换总时长不超过 60 秒，单片不超过 1080p，音轨参数须相同。
- 单线程转换，画布长边最多 1280 像素，保持原比例并补黑边。
- 输入、转换片和成品各项总量最多 64 MiB，同时受全局临时磁盘预算约束。

取得的短片不完整时会提示页数；无法合并且有静态图时回退发送原图集。普通图集默认最多 30 张，单张素材上限为 10 MB。图片准备失败、配置截断和 QQ 拒绝发送均有提示。

## 命令行工具

```bash
videorust --config config.yaml check
videorust --config config.yaml parse '<分享链接>'
videorust --config config.yaml probe '<分享链接>'
videorust --help
```

`check` 检查配置加载情况。`parse` 输出逐行 JSON。`probe` 完整下载普通视频，或下载并合并抖音实况短片，验证后清理临时媒体；普通静态图集输出解析到的图片数量。

## 性能与资源控制

| 项目 | 默认值或处理方式 |
| --- | --- |
| Tokio 工作线程 | 2 |
| 解析并发 / 活跃消息任务 | 3 / 4 |
| 同平台请求间隔 | 800 ms，加 0–250 ms 抖动 |
| 解析缓存 | 128 项、8 MiB、120 秒 |
| 单视频 / 临时媒体总量 | 190 MiB / 512 MiB |
| 直链下载 | 单连接流式下载、64 KiB 写缓冲 |
| QQ 文件上传 | 遵从平台配额，每文件最多并发 2 片 |
| 媒体处理 | 优先复制编码流，失败、超时和取消时清理临时文件 |

HTTP 401、403、412 触发平台冷却，429 遵从 `Retry-After`。同一抖音作品的短链和长链共用缓存，单个作品缺少媒体时只短暂记录该作品的失败。解析、下载或合流、QQ 上传耗时分别记录，便于定位网络与处理瓶颈。

## 源码构建与验证

需要支持 Rust 2024 edition 的 Rust 工具链。媒体合流及实况功能需要 FFmpeg 和 ffprobe。

```bash
git clone https://github.com/chaojisizhu/allvideo.git
cd allvideo
cargo build --release --locked
cp config.example.yaml config.yaml
./target/release/videorust check
```

Windows 使用 `target\release\videorust.exe`。可选的 Python 功能需另行安装对应依赖，并在配置中启用。

```bash
cargo fmt --all -- --check
cargo test --locked --all-targets
cargo clippy --locked --all-targets -- -D warnings

# 需要本机 FFmpeg、ffprobe 及测试所用编码器
cargo test --locked --test remux_http -- --ignored
cargo test --locked --test remux_live_album -- --ignored
```

开发中使用代理时，运行代理配置在 `api.proxy`；Docker 构建可通过 `HTTP_PROXY`、`HTTPS_PROXY` 构建参数传入。Docker Desktop 访问宿主机代理需使用 `host.docker.internal` 地址。

## 许可证与参考

Rust 主项目使用 MIT 许可证，保留既有接入代码的授权声明。Linux 发布包附带的 FFmpeg 启用 libx264，使用 GPLv3，具体许可证随包提供。相关依赖遵循各自许可证。[项目许可证](LICENSE)

QQ 协议接入参考腾讯官方 SDK，抖音 Web 签名参考开源原生实现。FFmpeg 的版本、源码校验和构建参数记录在 Dockerfile 中。[QQ 官方 SDK](https://github.com/tencent-connect/qqbot-nodejs) · [抖音签名参考](https://github.com/Evil0ctal/Douyin_TikTok_Download_API/blob/main/src/dtk/signing/native/websign.py) · [FFmpeg 构建](Dockerfile)
