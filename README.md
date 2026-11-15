# AllVideo

Rust 编写的 QQ 官方视频解析机器人，使用平台原站接口和分享页面提取媒体。微信视频号沿用元宝分享解析与本地 ISAAC64 解密。项目基于工作目录内 `videorust` 的 MIT 接入模块重构，保留 QQ API v2、消息模板及可选附属功能。

## 快速运行

```powershell
Copy-Item config.example.yaml config.yaml
New-Item -ItemType Directory -Force cookies
cargo build --release --locked
.\target\release\videorust.exe check
.\target\release\videorust.exe parse "https://www.bilibili.com/video/BV17x411w7KC?p=2"
.\target\release\videorust.exe probe "https://www.douyin.com/video/7611489793444171048"
```

`parse` 输出逐行 JSON。`probe` 会完整下载并清理视频，图集输出解析得到的图片数量，可用于 QQ 登录前的独立测试。HLS 与 DASH 合流需要本机 `ffmpeg`，以 `-c copy` 直接封装。默认使用两个 Tokio 工作线程。

在 `config.yaml` 填写 `qqbot.appId`、`qqbot.appSecret` 后运行 `videorust bot`。也可使用 `QQBOT_APP_ID`、`QQBOT_APP_SECRET` 环境变量。QQ 网关连接是部署验证的最后一步。

## Docker

```powershell
docker compose build
docker compose run --rm allvideo check
docker compose run --rm allvideo probe "https://www.douyin.com/video/7611489793444171048"
docker compose up -d
docker compose logs --tail 100 -f
```

Compose 默认使用 `media` 构建目标，包含 Rust 主程序与按需合流所用的裁剪版 FFmpeg。配置和 Cookie 按只读方式挂载，媒体目录使用独立数据卷。默认资源上限为 0.5 CPU、256 MiB 内存、64 个进程，日志最多保留 10 MiB。

Dockerfile 提供三个运行目标。

| 目标 | 包含内容 |
| --- | --- |
| `runtime` | Rust 二进制、TLS 证书和所需动态库 |
| `media` | `runtime` 与用于 HTTP/HLS/DASH 封装的 FFmpeg |
| `extras` | 完整 Python 环境、yt-dlp 和 JMComic，支持附属下载功能 |

```powershell
docker build --target runtime -t allvideo:rust .
docker build --target extras -t allvideo:extras .
docker build --target test -t allvideo:test .
```

构建时若网络需要本机 Clash，可增加 `--build-arg HTTP_PROXY=http://host.docker.internal:7890 --build-arg HTTPS_PROXY=http://host.docker.internal:7890`。运行时代理通过 `api.proxy` 或 `ALLVIDEO_PROXY` 配置，Windows 本机可填写 `http://127.0.0.1:7890`，Docker Desktop 容器可填写 `http://host.docker.internal:7890`。

## 登录与平台覆盖

B站可通过原站二维码登录并保存 Cookie。

```powershell
.\target\release\videorust.exe login bilibili
```

Linux 交付目录可运行 `./start.sh login bilibili`。用 B站 App 扫描生成的 `cookies/bilibili-login.png` 并在手机确认，成功后写入 `cookies/bilibili.txt`。后续启动会自动加载该文件。可用 `--cookie-file`、`--qr-file` 和 `--timeout-seconds` 指定路径与等待时间，指定 Cookie 文件路径时也可在 `auth.cookieFiles.bilibili` 配置复用。

扫码确认后会检查原站登录状态。遇到原站 HTTP 412 或网络验证时，程序保留已取得的 Cookie 并显示校验状态。二维码过期或未确认时会返回可重试的提示。

Docker 登录需要可写 Cookie 目录，可使用 `docker run --rm -it --mount type=bind,source=<Cookie目录绝对路径>,target=/app/cookies allvideo:local login bilibili`。

当前默认关闭微博、YouTube、知乎、Instagram 与 TikTok，可在 `platformEnable` 查看开关。小红书使用 `auth.cookies.xiaohongshu`，完整分享链接需保留 `xsec_token`。X 登录态使用 Netscape 文件，在 `auth.cookieFiles.twitter` 配置后启用 `ytdlp.enabled`，认证提取将优先使用本机 yt-dlp。


按平台在 `auth.cookies` 填写浏览器完整 Cookie，或在 `auth.cookieFiles` 指向 Netscape 格式文件。Cookie 文件按域名、路径、HTTPS 标志和过期时间匹配。小红书分享链接需要保留 `xsec_token`。抖音移动分享域 `iesdouyin.com` 的 Cookie 需要在 Cookie 文件中对应此域配置。

元宝登录态填写 `channels.yuanbaoCookie`，也可通过 `YUANBAO_COOKIE` 提供。配置、Cookie、环境文件、日志及构建产物已列入 Git 忽略规则。

已实现的专用原生适配包括 B站普通视频与番剧、抖音、快手、小红书、微博、AcFun、知乎视频、X、皮皮虾、最右、微视、好看、虎牙、即梦、美拍、皮皮搞笑、豆包、头条与西瓜，以及部分海外平台页面数据。部分平台仍需补充专用签名、有效分享链接或登录态，逐平台实现和验证状态见 [平台记录](docs/PLATFORMS.md)。

可选本机 `yt-dlp` 通过 `ytdlp.enabled` 开启。默认值为关闭。其进程设置解析时限、输出上限、请求间隔和重试上限。YouTube 动态签名、PO Token 和权限内容的原生全链路仍待完善。

## 请求节奏与资源管理

- HTTP 连接和匿名会话复用，按平台串行请求，默认间隔 800 ms 并加入 0–250 ms 抖动。
- 相同链接并发合并，短期缓存同时受条目数、字节预算和 TTL 限制，默认最多 128 项、8 MiB、120 秒。
- 401、403、412 触发平台冷却，429 遵从 `Retry-After`。访问验证会终止补充解析链，等待人工补充权限或完成原站验证。
- 全局解析并发默认 3，活跃消息任务最多 4，单消息最多解析 3 个链接。消息队列满载时跳过溢出事件并限频告警，网关心跳继续运行。
- 直链视频单连接流式下载，以 64 KiB 缓冲写盘。QQ 文件上传遵从预上传接口提供的并发数，每文件最多同时上传两片，配额缺省时串行。日志分别记录解析、下载或合流、QQ 上传耗时。视频号仅处理前 128 KiB 加密前缀。
- 下载、合流和上传结束后通过文件所有权守卫清理临时媒体。正常失败、超时、任务取消均有清理路径。
- 原生视频单文件默认 190 MiB，活动临时媒体默认合计 512 MiB。合流前预留输出预算，超限结果会失败并清理。
- 图集素材每张最多 10 MB，逐张上传；OpenAI 图片溯源按配置最多 50 MiB，并发为 1。附属 JM 的限页、字节预算及流式打包单独管理。

开源原站协议、会话与限流处理的参考链接收录在 [平台记录](docs/PLATFORMS.md)。测量结果和已知限制收录在 [验证记录](docs/VALIDATION.md)。

## 开发验证

```powershell
cargo fmt --all -- --check
cargo test --locked --all-targets
cargo clippy --all-targets -- -D warnings
# 本机有 ffmpeg 与 ffprobe 时验证 HTTP 双轨与 HLS 合流
cargo test --test remux_http -- --ignored
# 本机有 Python 时验证传统 ZIP 加密互通
cargo test python_zipfile_can_decrypt_generated_archive -- --ignored
```

MIT 许可证覆盖 Rust 项目，保留原接入代码的授权声明。Docker 中 FFmpeg 的源码版本、校验摘要及编译参数可在 Dockerfile 查阅，其源码提供于 FFmpeg 官方发布目录。[FFmpeg 源码](https://ffmpeg.org/releases/)

生产环境的目录、服务和验证结果见 [生产部署](docs/PRODUCTION.md)。

## 图集输出

抖音实况图集拿到短片后，按原顺序合成一条 MP4。允许相同参数的 H.264 视频及 AAC 音轨，直接复制编码流。机器人同时处理一个实况图集，每组最多 12 段，输入合计与成品各最多 64 MiB，并遵守临时文件总预算。缺少短片或编码参数不一致时可返回静态图集。合并需要 `ffmpeg` 与 `ffprobe`，Linux 媒体镜像自带两者。

图集会将文字与本次图片合成一条 Markdown 消息，默认最多 30 张，可通过 `output.maxMarkdownImages` 调整；0 使用默认值 30。程序读取 PNG、JPEG、GIF、WebP 的实际编码元信息及 EXIF 方向，按宽度最多 750 像素等比声明显示尺寸。图片逐个下载，单张上限为官方素材接口的 10 MB，处理过程中不解码像素或重新编码。

图片通过 QQ 专用素材 API `media-images/prepare`、预签名 PUT 和 `media-images/complete` 上传，Markdown 使用返回的 `qqbot-media://image/v1/...` 引用。图片资源强校验保持开启，准备失败和配置截断会在图文中显示数量，最终拒绝会另发明确失败提示。

成功发送后，仅删除本项目本批创建的素材。官方说明已发送消息保存了图片副本，删除素材不影响该消息。本项目素材所有权记录保存在媒体临时目录的 `.allvideo-markdown-materials.json` 中，重启或后续图集会继续清理到期记录。网络结果未确认的素材先保留五分钟，避免影响可能仍在处理的消息。损坏记录会阻止继续创建素材并返回提示。

富媒体上传的临时 HTTP 图链与 X 原站图链在实际 Markdown 强校验中均返回过 `40034141`，因此图集使用专用素材引用。手机与 PC 的最终显示效果仍由实际 QQ 客户端决定。

素材协议与尺寸格式参考官方文档和开源实现。[官方素材管理](https://bot.q.qq.com/wiki/develop/api-v2/server-inter/message/material-management.html) [官方尺寸实现](https://github.com/tencent-connect/qqbot-nodejs/blob/main/src/protocol/utils/image-size.ts) [Gensokyo 素材实现](https://github.com/Te-River/Gensokyo-NewQQ/blob/main/handlers/media_asset.go) [El Bot 转存实测](https://docs.bot.elpsy.cn/development/codex-remote.html)
