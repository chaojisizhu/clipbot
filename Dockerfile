FROM rust:1-slim-bookworm AS build
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY tests ./tests
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target \
    cargo build --release --locked && cp target/release/clipbot /usr/local/bin/clipbot
RUN mkdir -p /runtime-root/usr/local/bin /runtime-root/etc/ssl/certs /runtime-root/app/temp_videos /runtime-root/app/temp_jm \
    && cp /usr/local/bin/clipbot /runtime-root/usr/local/bin/ \
    && ldd /usr/local/bin/clipbot | awk '{for(i=1;i<=NF;i++) if($i ~ /^\//) print $i}' \
       | xargs -I '{}' cp --parents '{}' /runtime-root \
    && cp /etc/ssl/certs/ca-certificates.crt /runtime-root/etc/ssl/certs/ \
    && printf 'hosts: files dns\n' > /runtime-root/etc/nsswitch.conf \
    && chown -R 65532:65532 /runtime-root/app

FROM build AS test
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target \
    NO_PROXY=localhost,127.0.0.1,::1 no_proxy=localhost,127.0.0.1,::1 cargo test --locked --all-targets

FROM rust:1-slim-bookworm AS ffmpeg-build
RUN apt-get update && apt-get install -y --no-install-recommends make pkg-config libssl-dev libx264-dev curl xz-utils \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /ffmpeg
RUN curl --fail --location https://ffmpeg.org/releases/ffmpeg-9.0.2.tar.xz -o source.tar.xz \
    && echo '8c3850283eb25fa026482078a04051e0be17347b09ef81a0849bec15a96e002e  source.tar.xz' | sha256sum -c - \
    && tar -xJf source.tar.xz --strip-components=1 \
    && sed -i 's/-lx264/-l:libx264.a/' /usr/lib/x86_64-linux-gnu/pkgconfig/x264.pc \
    && ./configure --disable-everything --disable-autodetect --disable-programs --enable-ffmpeg --enable-ffprobe \
       --enable-gpl --enable-version3 --enable-libx264 --pkg-config-flags=--static \
       --disable-doc --disable-debug --disable-x86asm --disable-shared --enable-static \
       --disable-avdevice --disable-swresample --enable-swscale \
       --enable-network --enable-openssl --enable-pthreads \
       --enable-protocol=file,http,https,tcp,tls,crypto \
       --enable-demuxer=mov,mpegts,hls,matroska,aac,concat \
       --enable-muxer=mp4 --enable-parser=aac,h264,hevc \
       --enable-decoder=aac,h264,hevc --enable-encoder=libx264 \
       --enable-filter=scale,pad,fps,setsar,format \
       --enable-bsf=aac_adtstoasc,h264_mp4toannexb,extract_extradata \
    && make -j2 ffmpeg ffprobe \
    && strip ffmpeg ffprobe \
    && mkdir -p /media-root/usr/local/bin \
    && cp ffmpeg ffprobe /media-root/usr/local/bin/ \
    && ldd ffmpeg ffprobe | awk '{for(i=1;i<=NF;i++) if($i ~ /^\//) print $i}' | sort -u \
       | xargs -I '{}' cp --parents '{}' /media-root

FROM scratch AS runtime
COPY --from=build /runtime-root/ /
WORKDIR /app
COPY --chown=65532:65532 config.example.yaml ./config.example.yaml
ENV RUST_LOG=info
ENV SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/clipbot"]
CMD ["bot"]

FROM runtime AS media
COPY --from=ffmpeg-build /media-root/ /
COPY --from=ffmpeg-build /ffmpeg/COPYING.GPLv3 /usr/share/licenses/ffmpeg/COPYING.GPLv3

FROM debian:bookworm-slim AS extras
USER root
COPY scripts/requirements-extras.txt /tmp/requirements-extras.txt
RUN apt-get update && apt-get install -y --no-install-recommends python3 python3-venv ca-certificates \
    && python3 -m venv /opt/extras \
    && /opt/extras/bin/pip install --no-cache-dir -r /tmp/requirements-extras.txt \
    && rm -rf /var/lib/apt/lists/*
COPY --from=media /usr/local/bin/ /usr/local/bin/
WORKDIR /app
COPY --chown=65532:65532 config.example.yaml ./config.example.yaml
RUN mkdir -p /app/temp_videos /app/temp_jm && chown -R 65532:65532 /app
ENV PATH="/opt/extras/bin:${PATH}"
COPY scripts/jm_download.py ./scripts/jm_download.py
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/clipbot"]
CMD ["bot"]

FROM media AS default
