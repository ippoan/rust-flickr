# rust-flickr

`rust-logi` の gRPC `logi.flickr.FlickrService` を切り出した **Rust + axum の REST サービス**。
Cloud Run で稼働し、**scratch 極小イメージ** (musl static + rustls) と
**Rust → TypeScript 型生成 (ts-rs)** を検証する。設計の全体像は
[Issue #1](https://github.com/ippoan/rust-flickr/issues/1) を参照。

## アーキテクチャ (目標)

```
[ front (nuxt) ]──REST──┐
[ Cloud Scheduler ]──REST──┤
                          ▼
   [ edge Worker (REST proxy / auth / CORS) ]   ← CF API として公開
                          ▼ fetch (+ GCP ID token)
   [ rust-flickr (Cloud Run, axum, scratch 極小) ] ── Supabase / Flickr API
```

## エンドポイント

| Method | Path | 状態 | 説明 |
| --- | --- | --- | --- |
| GET | `/healthz` | ✅ PR1 | 死活。`{status, service, version}` を返す |
| GET | `/oauth/url` | ⏳ PR2 | OAuth1.0a request token 取得 → 認可 URL 返却 |
| POST | `/oauth/callback` | ⏳ PR2 | verifier → access token 交換 + `flickr_tokens` UPSERT |
| POST | `/import` | ⏳ PR3 | 未検証 `cam_files.flickr_id` を検証して `flickr_photo` 登録 |

設計原則 (Refs #1):

- **「黙って 200」禁止** — 失敗は HTTP ステータス (412/424/500 等) で明示する
- **org は明示パラメータ/ヘッダで受ける** — RLS ハードコード依存を廃止
- **rustls 統一** — `reqwest` / `sqlx` を入れる時は rustls feature に統一
  (openssl 依存を持ち込まない = scratch 成立条件)

## ローカル開発

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo run            # → http://localhost:8080/healthz
PORT=3000 cargo run  # ポート変更
```

## イメージ (scratch + musl static)

cargo build は CI ランナー側で行い (sccache + Swatinem/rust-cache が効く)、
Docker は **prebuilt binary を `FROM scratch` に COPY するだけ** の packaging-only
構成 (rust-alc-api と同方式)。

ローカルで組む場合:

```sh
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl --locked
mkdir -p ctx && cp target/x86_64-unknown-linux-musl/release/rust-flickr ctx/ && cp Dockerfile ctx/
docker build -t rust-flickr ctx
docker run --rm -p 8080:8080 rust-flickr
```

CI の `build-image` job が binary size / image size (compressed layers) を
Job Summary に記録する。

## CI / デプロイ

| job | 内容 |
| --- | --- |
| `ci / fmt,clippy,test,build` | `ippoan/ci-workflows` の `rust-ci.yml` reusable |
| `build-image` | musl static build → `FROM scratch` packaging → GHCR push (`ghcr.io/ippoan/rust-flickr`) |
| `deploy-staging` | `cloud-run-deploy.yml` reusable で `rust-flickr-staging` へ digest-pinned deploy (WIF auth) |

`deploy-staging` は repo variable **`STAGING_DEPLOY_ENABLED=true`** を設定するまで
skip される。下記 one-time setup 完了後に variable を入れた瞬間 deploy が動き始める。

### One-time setup (user 手動)

`secrets-inventory-gcp` / `release-wave-gcp` と同じ「GCP key 0 個」構成
(runtime: attached SA + ADC / deploy: WIF + GitHub OIDC)。

```sh
PROJECT=cloudsql-sv
REGION=asia-northeast1
PROJECT_NUMBER=$(gcloud projects describe "$PROJECT" --format='value(projectNumber)')

# 1) Deployer SA (staging-deploy@cloudsql-sv、既存) に本 repo からの impersonate を許可
gcloud iam service-accounts add-iam-policy-binding \
  staging-deploy@$PROJECT.iam.gserviceaccount.com \
  --project="$PROJECT" \
  --role="roles/iam.workloadIdentityUser" \
  --member="principalSet://iam.googleapis.com/projects/$PROJECT_NUMBER/locations/global/workloadIdentityPools/gh-actions-pool/attribute.repository/ippoan/rust-flickr"

# 2) GHCR package を public 化 (AR pull-through cache 経由 pull のため)
#    https://github.com/orgs/ippoan/packages/container/rust-flickr/settings

# 3) Cloud Run service 初回作成 (cloud-run-deploy.yml は `services update` = 既存前提)
gcloud run deploy rust-flickr-staging \
  --project="$PROJECT" \
  --region="$REGION" \
  --image="asia-northeast1-docker.pkg.dev/$PROJECT/ghcr/ippoan/rust-flickr:latest" \
  --allow-unauthenticated \
  --ingress=all

# 4) repo variable を設定して deploy-staging を有効化
#    Settings → Secrets and variables → Actions → Variables:
#      STAGING_DEPLOY_ENABLED = true
```

`FLICKR_*` / `DATABASE_URL` の Secret Manager 投入と runtime SA 整備は PR5 (Refs #1)。

### 計測 (PR1 の検証項目)

- **イメージサイズ**: CI Job Summary の `Image size (compressed layers)` を記録
- **cold start**: deploy 後、アイドル状態から計測

```sh
URL=$(gcloud run services describe rust-flickr-staging \
  --project=cloudsql-sv --region=asia-northeast1 --format='value(status.url)')
# インスタンス 0 の状態 (≥15 分放置) から:
curl -s -o /dev/null -w 'total=%{time_total}s\n' "$URL/healthz"   # cold
curl -s -o /dev/null -w 'total=%{time_total}s\n' "$URL/healthz"   # warm
```
