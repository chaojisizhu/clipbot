# -*- coding: utf-8 -*-
# JMComic 下载包装脚本 (配合视频解析机器人使用)
# 用法: python jm_download.py <album_id 或 photo_id> <输出目录> [--photo]
# 完成后 stdout 打印: JM_OK <下载的图片文件数>
import os
import sys


def main():
    if len(sys.argv) < 3:
        print("JM_ERR 参数不足: jm_download.py <id> <out_dir> [--photo] [--proxy=URL]", file=sys.stderr)
        sys.exit(2)

    target_id = sys.argv[1]
    out_dir = os.path.abspath(sys.argv[2])
    is_photo = "--photo" in sys.argv
    proxy = ""
    for a in sys.argv:
        if a.startswith("--proxy="):
            proxy = a.split("=", 1)[1]

    try:
        import jmcomic
    except ImportError:
        print("JM_ERR 未安装 jmcomic 库，请先执行: pip install jmcomic", file=sys.stderr)
        sys.exit(3)

    os.makedirs(out_dir, exist_ok=True)

    postman = {"timeout": 20, "type": "requests"}
    if proxy:
        postman["proxies"] = {"http": proxy, "https": proxy}

    threads_image = 10
    for a in sys.argv:
        if a.startswith("--threads="):
            try:
                threads_image = max(1, min(32, int(a.split("=", 1)[1])))
            except ValueError:
                pass

    option = jmcomic.JmOption.construct({
        "dir_rule": {"base_dir": out_dir, "rule": "Bd"},
        "client": {
            "impl": "api",
            "postman": postman,
        },
        "download": {
            "cache": True,
            "image": {"decode": True, "suffix": ".jpg"},
            # 多线程提速: 图片并发 + 章节并发
            "threading": {"image": threads_image, "photo": 4},
        },
    })

    try:
        if is_photo:
            jmcomic.download_photo(target_id, option)
        else:
            jmcomic.download_album(target_id, option)
    except Exception as e:  # noqa: BLE001
        print(f"JM_ERR 下载失败: {e}", file=sys.stderr)
        sys.exit(4)

    # base_dir 即本次下载目录(每次调用唯一)，直接遍历统计图片
    count = 0
    for root, _dirs, files in os.walk(out_dir):
        for f in files:
            if f.lower().endswith((".jpg", ".jpeg", ".png", ".webp")):
                count += 1
    print(f"JM_OK {count}")
    sys.exit(0)


if __name__ == "__main__":
    main()
