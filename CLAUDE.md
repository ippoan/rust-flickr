# CLAUDE.md

rust-logi の Flickr 機能 (gRPC `FlickrService`) を切り出した axum REST サービス。
Cloud Run + scratch 極小イメージ + ts-rs 型生成の検証を兼ねる。

詳細 (アーキテクチャ・経緯・gotcha・エンドポイント) は rust-flickr-map skill / [`README.md`](./README.md) / [Issue #1](https://github.com/ippoan/rust-flickr/issues/1) を参照。

## 設計規範 (必ず守ること)

- **「黙って 200」禁止** — 失敗は 412/424/500 等の HTTP ステータスで明示する。token 不在で 200 を返すコードを書かない。
- **org は明示パラメータ/ヘッダで受ける** — RLS 前提の暗黙 org 依存を持ち込まない。
- **rustls 統一** — `reqwest` / `sqlx` 等を追加する時は必ず rustls feature を選ぶ。openssl 依存が入ると `FROM scratch` が成立しなくなる。
- **イメージは packaging-only** — `Dockerfile` 内で cargo を走らせない。cargo build は CI ランナー側 (sccache + Swatinem/rust-cache)。
- **秘密 (FLICKR_* / DATABASE_URL) は Secret Manager 参照** — Cloud Run の plain env `value:` に直書きしない。

## ビルド / テスト

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

`Cargo.lock` は commit する (CI は `--locked` でビルド)。依存を追加・更新したら lock の差分も同じ PR に含める。

## GitHub 自動化

- **`main` に直 push しない。** PR を作る。
- PR / commit は `Refs #N` を使う (`Closes/Fixes/Resolves` は禁止 — auto-close 防止)。
- `mcp__github__enable_pr_auto_merge` を reflex で呼ばない (user 明示指示時のみ)。
- PR 作成後は同じ turn で `mcp__github__subscribe_pr_activity` を呼び CI を watch する。

---

_共通項を直すときは [`ippoan/claude-md`](https://github.com/ippoan/claude-md) の `CLAUDE.md.template` を更新すること。_
