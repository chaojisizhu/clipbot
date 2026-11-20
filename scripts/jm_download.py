# -*- coding: utf-8 -*-
# JMComic 下载包装脚本 (配合视频解析机器人使用)
# 用法: python jm_download.py <album_id 或 photo_id> <输出目录> [--photo] [--max-pages=N]
# 完成后 stdout 打印: JM_OK <下载的图片文件数>
import argparse
import os
import sys
from importlib import import_module
from threading import Lock


def create_downloader(jmcomic, option, max_pages):
    class LimitedDownloader(jmcomic.JmDownloader):
        def __init__(self):
            self.remaining_pages = max_pages if max_pages > 0 else None
            self.page_lock = Lock()
            super().__init__(option)

        def do_filter(self, detail):
            if not detail.is_photo() or self.remaining_pages is None:
                return super().do_filter(detail)
            # 在图片进入线程池之前预留整个章节的页额。
            # 章节串行，因此相册所有章节共享一个按阅读顺序递减的预算。
            with self.page_lock:
                images = detail[:self.remaining_pages]
                self.remaining_pages -= len(images)
            return images

        def download_by_photo_detail(self, photo):
            if self.remaining_pages == 0:
                return
            return super().download_by_photo_detail(photo)

    return LimitedDownloader()


def main():
    parser = argparse.ArgumentParser(description="JMComic 有界图片下载")
    parser.add_argument("target_id")
    parser.add_argument("out_dir")
    parser.add_argument("--photo", action="store_true")
    parser.add_argument("--proxy", default="")
    parser.add_argument("--threads", type=int, default=3)
    parser.add_argument("--max-pages", type=int, default=0)
    args = parser.parse_args()
    if args.max_pages < 0:
        parser.error("--max-pages 必须大于或等于 0")
    out_dir = os.path.abspath(args.out_dir)

    try:
        import_module("requests")
        jmcomic = import_module("jmcomic")
        import_module("Crypto.Cipher.AES")
    except ModuleNotFoundError as error:
        print(f"JM_ERR missing_dependency {error.name}", file=sys.stderr)
        sys.exit(3)
    except ImportError:
        print("JM_ERR Python 依赖无法导入，请检查服务使用的虚拟环境", file=sys.stderr)
        sys.exit(3)

    os.makedirs(out_dir, exist_ok=True)

    request_options = {"timeout": 20}
    if args.proxy:
        request_options["proxies"] = {"http": args.proxy, "https": args.proxy}

    threads_image = max(1, min(32, args.threads))

    option = jmcomic.JmOption.construct({
        # 各章节的 00001.jpg 等同名页面分别保存，防止相互覆盖。
        "dir_rule": {"base_dir": out_dir, "rule": "Bd/Pindex"},
        "client": {
            "impl": "api",
            "postman": {"type": "requests", "meta_data": request_options},
        },
        "download": {
            "cache": True,
            "image": {"decode": True, "suffix": ".jpg"},
            "threading": {"image": threads_image, "photo": 1},
        },
    })

    try:
        with create_downloader(jmcomic, option, args.max_pages) as downloader:
            if args.photo:
                downloader.download_photo(args.target_id)
            else:
                downloader.download_album(args.target_id)
            if downloader.download_failed_image or downloader.download_failed_photo:
                raise RuntimeError("存在下载失败的图片或章节")
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
