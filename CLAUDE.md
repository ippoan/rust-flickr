# CLAUDE.md

Claude Code 向けの本リポジトリ作業ルール。

rust-logi の Flickr 機能 (gRPC `FlickrService`) を切り出した axum REST サービス。
Cloud Run + scratch 極小イメージ + ts-rs 型生成の検証を兼ねる。

## まず読むもの

- [`README.md`](./README.md) — アーキテクチャ / エンドポイント / ビルド / デプロイ手順
- [Issue #1](https://github.com/ippoan/rust-flickr/issues/1) — 設計の根拠・PR 分割 (PR1〜PR6)

## 設計上の要点 (触る前に)

- **「黙って 200」禁止** — 切り出しの動機が「gRPC-web で全層 HTTP 200 のまま 0 件取り込みが
  続いた」事故 (Refs #1)。失敗は 412/424/500 等の HTTP ステータスで明示する。
  token 不在で 200 を返すコードを書かない。
- **org は明示パラメータ/ヘッダで受ける** — cron のハードコード org + RLS で token が
  見えなくなった事故の根治。RLS 前提の暗黙 org 依存を持ち込まない。
- **rustls 統一** — `reqwest` / `sqlx` 等を追加する時は必ず rustls feature
  (`rustls-tls` / `runtime-tokio-rustls`) を選ぶ。openssl 依存が入ると
  `FROM scratch` が成立しなくなる。
- **イメージは packaging-only** — cargo build は CI ランナー側 (sccache +
  Swatinem/rust-cache)。`Dockerfile` は musl static binary の COPY だけ
  (rust-alc-api と同方式)。Dockerfile 内で cargo を走らせない。
- **秘密 (FLICKR_* / DATABASE_URL) は Secret Manager 参照** — Cloud Run の
  plain env `value:` に直書きしない (PR5 で整備)。

## Worktree / branch 命名規則

形式: `<issue-number>-<type>-<short-description>`

- `issue-number`: 必須。先に issue を立ててから branch を作る
- `type`: `feat` | `fix` | `refactor` | `infra`

Claude Code が自動採番する `claude/...` で実装に入る場合は、対応する issue を
紐付けた上で PR description に `Refs #N` を明記する。

## ビルド / テスト

PR を出す前に手元で green に:

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

`Cargo.lock` は commit する (CI は `--locked` でビルド)。依存を追加・更新したら
lock の差分も同じ PR に含める。

## CI / デプロイ

- CI (`.github/workflows/ci.yml`) は `ippoan/ci-workflows` の `rust-ci.yml`
  (fmt/clippy/test/build) + `build-image` (musl + scratch + GHCR push) +
  `deploy-staging` (`cloud-run-deploy.yml`、WIF) で構成。
- `deploy-staging` は repo variable `STAGING_DEPLOY_ENABLED=true` まで skip
  (one-time GCP setup の手順は README 参照)。
- `coverage_100.toml` を repo root に置くと rust-ci の 100% gate が自動で有効化される。

## GitHub 自動化 (重要)

- **`main` に直 push しない。** PR を作る。
- PR / commit は `Refs #N` を使う (`Closes/Fixes/Resolves` は禁止 — auto-close 防止)。
- `mcp__github__enable_pr_auto_merge` を reflex で呼ばない (user 明示指示時のみ)。
- PR 作成後は同じ turn で `mcp__github__subscribe_pr_activity` を呼び CI を watch する。

---

_共通項を直すときは [`ippoan/claude-md`](https://github.com/ippoan/claude-md) の
`CLAUDE.md.template` を更新すること。_
