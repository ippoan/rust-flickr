# Packaging-only Dockerfile (rust-alc-api と同方式)。
#
# cargo build は CI ランナー側で実行する (sccache + Swatinem/rust-cache が効く)。
# ここは musl static binary を scratch に COPY するだけ — OS レイヤなしの
# 極小イメージ検証 (Refs #1)。
#
# CI (ci.yml の build-image job) は ctx/ に以下を用意して `docker build ctx` する:
#   ctx/rust-flickr   ... x86_64-unknown-linux-musl の release binary (strip 済)
#   ctx/Dockerfile    ... 本ファイル
#
# ローカルで組む場合:
#   cargo build --release --target x86_64-unknown-linux-musl --locked
#   mkdir -p ctx && cp target/x86_64-unknown-linux-musl/release/rust-flickr ctx/ && cp Dockerfile ctx/
#   docker build -t rust-flickr ctx
FROM scratch
COPY rust-flickr /rust-flickr
EXPOSE 8080
ENTRYPOINT ["/rust-flickr"]
