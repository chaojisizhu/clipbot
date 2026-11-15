"""从明确的文件清单创建 GitHub Release 包，不读取运行配置或 Cookie。"""

import argparse
import hashlib
import io
from pathlib import Path
import tarfile
import tomllib
import zipfile


def sha256(data):
    return hashlib.sha256(data).hexdigest()


def text_file(path):
    return path.read_text(encoding="utf-8-sig").replace("\r\n", "\n").encode("utf-8")


def add_manifest(files):
    files["SHA256SUMS"] = "".join(
        f"{sha256(data)}  {name}\n" for name, data in sorted(files.items())
    ).encode("ascii")


def package_linux(output, prefix, files):
    destination = output / f"{prefix}.tar.gz"
    with tarfile.open(destination, "w:gz") as archive:
        for name, data in sorted(files.items()):
            entry = tarfile.TarInfo(f"{prefix}/{name}")
            entry.size = len(data)
            entry.mode = 0o755 if name in {"videorust", "ffmpeg", "ffprobe", "start.sh", "install-extras.sh"} else 0o644
            archive.addfile(entry, io.BytesIO(data))
    return destination


def package_windows(output, prefix, files):
    destination = output / f"{prefix}.zip"
    with zipfile.ZipFile(destination, "w", compression=zipfile.ZIP_DEFLATED) as archive:
        for name, data in sorted(files.items()):
            archive.writestr(f"{prefix}/{name}", data)
    return destination


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--linux-dir", type=Path, required=True, help="Linux 编译产物所在目录")
    parser.add_argument("--windows-exe", type=Path, required=True)
    parser.add_argument("--output", type=Path, default=Path("dist/releases"))
    args = parser.parse_args()
    root = Path(__file__).resolve().parent.parent
    version = tomllib.loads((root / "Cargo.toml").read_text(encoding="utf-8"))["package"]["version"]
    args.output.mkdir(parents=True, exist_ok=True)

    common = {
        "config.example.yaml": text_file(root / "config.example.yaml"),
        "LICENSE": text_file(root / "LICENSE"),
        "scripts/jm_download.py": text_file(root / "scripts/jm_download.py"),
    }
    linux = dict(common)
    for name in ["videorust", "ffmpeg", "ffprobe"]:
        data = (args.linux_dir / name).read_bytes()
        if data[:4] != b"\x7fELF" or int.from_bytes(data[18:20], "little") != 62:
            raise ValueError(f"{name} 不是 Linux x86_64 ELF 文件")
        linux[name] = data
    for name in ["start.sh", "install-extras.sh", "requirements-extras.txt", "FFmpeg-LICENSE.txt"]:
        linux[name] = text_file(args.linux_dir / name)
    linux["README.txt"] = f"""AllVideo {version} Linux x86_64

支持 Ubuntu 22.04+ / Debian 12+，需要 ca-certificates、libgcc-s1 和 OpenSSL 3。
包内提供 Rust 主程序、FFmpeg 和 ffprobe。

进入解压目录后执行
  sha256sum -c SHA256SUMS
  cp config.example.yaml config.linux.yaml
  # 编辑 config.linux.yaml，填写 QQBot AppID/AppSecret 及按需使用的平台登录态。
  ./start.sh check
  ./start.sh bot

B站扫码登录
  ./start.sh login bilibili

启用可选 yt-dlp 或 JM 后，安装 Python 3 与 venv 并执行 ./install-extras.sh。
机器人配置、Cookie 和下载文件由使用者自行保存。
详细说明 https://github.com/chaojisizhu/allvideo
""".encode("utf-8")
    linux["SOURCES.txt"] = b"""Rust source: https://github.com/chaojisizhu/allvideo
FFmpeg 9.0.2: https://ffmpeg.org/releases/ffmpeg-9.0.2.tar.xz
x264 source: https://deb.debian.org/debian/pool/main/x/x264/x264_0.164.3095+gitbaee400.orig.tar.gz
x264 Debian patches: https://deb.debian.org/debian/pool/main/x/x264/x264_0.164.3095+gitbaee400-3.debian.tar.xz
Build flags are recorded in the repository Dockerfile. The bundled FFmpeg is GPLv3.
"""
    add_manifest(linux)

    windows = dict(common)
    windows_exe = args.windows_exe.read_bytes()
    if not windows_exe.startswith(b"MZ"):
        raise ValueError("Windows 产物不是 PE 可执行文件")
    windows["videorust.exe"] = windows_exe
    windows["README.txt"] = f"""AllVideo {version} Windows x86_64

在 PowerShell 进入解压目录后执行
  Copy-Item config.example.yaml config.yaml
  # 编辑 config.yaml，填写 QQBot AppID/AppSecret 及按需使用的平台登录态。
  .\\videorust.exe check
  .\\videorust.exe bot

B站扫码登录
  .\\videorust.exe login bilibili

HLS、音画合流与实况图集需要另行安装 FFmpeg 和 ffprobe 并加入 PATH。
实况规格统一需要 FFmpeg 含 libx264、scale、pad、fps、setsar、format。
启用 JM 时，根据本机环境把 jmcomic.python 设为 python 或可执行文件路径。
详细说明 https://github.com/chaojisizhu/allvideo
""".encode("utf-8")
    add_manifest(windows)
    artifacts = [
        package_linux(args.output, f"allvideo-v{version}-linux-x86_64", linux),
        package_windows(args.output, f"allvideo-v{version}-windows-x86_64", windows),
    ]
    checksums = args.output / "SHA256SUMS"
    checksums.write_text(
        "".join(f"{sha256(path.read_bytes())}  {path.name}\n" for path in artifacts),
        encoding="ascii", newline="\n",
    )
    for path in [*artifacts, checksums]:
        print(f"{path.name} ({path.stat().st_size} bytes)")


if __name__ == "__main__":
    main()
