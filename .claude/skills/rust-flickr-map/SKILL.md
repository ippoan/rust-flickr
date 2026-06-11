---
name: rust-flickr-map
generated-from: rust-flickr:c20310901a713c4d4d21c9104b5e2fca65e34dc7
paths: [src/, clients/, bindings/]
description: ippoan/rust-flickr (Rust/axum on Cloud Run、カメラ→Flickr 写真パイプライン) の構造ナビゲーション + 運用 SoT。カメラ SD 巡回 (/sync)・OAuth1.0a (/oauth)・検証取り込み (/import)・日次集計 (/stats) の配置、実 org UUID、Cloud Scheduler 2 job、digest pin 手動 deploy、SD ローテーションと古い順 upload の競争、「黙って 200」禁止の設計原則を 1 枚にまとめる。トリガー:「rust-flickr」「Flickr 取り込み」「カメラ 写真 アップロード」「SyncCamFiles」「cam_files」「flickr_photo」「flickr_tokens」「/sync が」「flickr-proxy」「SD カード 巡回」「Digest 認証 カメラ」「upBySytem」等。
---

# rust-flickr-map — ippoan/rust-flickr 構造ナビゲーション

rust-logi の gRPC FlickrService + SyncCamFiles を切り出した axum REST サービス
(Cloud Run `rust-flickr-staging` = 実運用、scratch 極小イメージ)。
2026-06-10 cutover 完了 (経緯: ippoan/cf-flickr-proxy#1 のコメント群が一次資料)。

> ここは索引 (pointer)。細部は repo 側が正。frontmatter の `generated-from` (commit-sha)
> + `paths` に変更があったら skills-check CI が warn するので、repo を読み直して更新する。

## パイプライン全体図

```
[カメラ TS-NA230WP] ←(car.mtamaramu.com = CF tunnel + Digest/MD5 認証)─┐
Cloud Scheduler rust-flickr-sync   (*/10, upload_limit:100) → POST /sync ─┤ rust-flickr
Cloud Scheduler rust-flickr-import (5-59/10, limit:500)     → POST /import┤ (Cloud Run)
front / 手動 → flickr-proxy.mtamaramu.com (cf-flickr-proxy Worker) ──────┘  ├ Supabase (logi schema)
cf-billing-monitor (毎朝 06:00 JST) → GET /stats → メールレポート           └ up.flickr.com
```

- /sync: SD 巡回 (dates→hours→files XML) → cam_files UPSERT → 未 upload 分を**古い順**に Flickr へ (同期実行)
- /import: cam_files.flickr_id 未検証分を photos.getInfo → flickr_photo 登録
- /stats: 撮影日別 files/uploaded/verified + 残数 (daily mail が消費)

## src 構成

| file | 役割 |
|---|---|
| `routes.rs` | handler 全部 + AppState (pool/flickr/cam の Option getter → 503) + require_organization |
| `cam.rs` | CamClient — CF Access ヘッダ + RFC2617 Digest (MD5) リトライ、SD XML parse (`<Dir name=>`/`<Name>`、`_!` 一時ファイル除外)、jpg/mp4 CGI 出し分け download |
| `flickr.rs` | FlickrClient — request/access token、photos.getInfo、upload_photo (OAuth1 multipart、photo は署名対象外、tags=upBySytem 互換)。endpoint は with_endpoints で test 注入 |
| `oauth1.rs` | pure 署名ヘルパ (HMAC-SHA1、既知ベクタでピン) |
| `db.rs` | sqlx 素関数。set_current_organization (RLS) を**毎 acquire 直後に必ず**呼ぶ |
| `types.rs` | req/res 型 (ts-rs export、bindings/ commit 必須 — CI が drift check) |
| `error.rs` | ApiError: 400/412(NoToken)/424(Upstream)/500(詳細 echo しない)/503(NotConfigured) |

## 運用の確定事項 (これを忘れると事故る)

- **org は `x-organization-id` ヘッダ必須**。実 org は **`536859de-d43e-4932-9d16-f60cac8fa426`** のみ
  (issue #1 旧記載の default org `00000000-…0001` は logi.organizations に**存在しない** — FK violation)
- **deploy は digest pin の手動** (MCP `deploy_service_from_image` / CI deploy-staging は STAGING_DEPLOY_ENABLED 無効で skip)。
  GHCR digest 取得→ secretKeyRef 構成ごとフル指定。env: FLICKR_*/DATABASE_URL/CAM_DIGEST_PASS = Secret Manager 参照、CAM_* その他 = plain
- **upload は古い順 (ORDER BY name)、下限は SD 実在最古日** (#11) — 巡回再開位置を下限にすると backfill 後に過去分が漏れる
- **/stats の oldest_unuploaded_date は全期間 min** — SD から消えた回収不能分 (2025 年〜) を指す。消化位置はレポート側が窓内導出 (cf-billing-monitor#8)
- **3/21〜5/23 の ~2 万件は SD ローテーションで消失済み = 永遠に flickr_id NULL** (total_unuploaded の底)
- **tokio::spawn で background upload しない** — Cloud Run CPU throttling で完走しない (rust-logi 旧実装の罠、移植時に同期化)
- 外形監視は **/health** (/healthz は Google フロントが食う — gcp-cloud-run-routing-traps skill 参照)
- proxy (flickr-proxy.mtamaramu.com) 経由は **edge ~100s** で切られる。長い /sync は Cloud Run 直 or upload_limit 小さく

## 関連

- edge: ippoan/cf-flickr-proxy (cf-flickr-proxy-map)
- daily mail: ippoan/cf-billing-monitor `src/flickr-report.ts` (/trigger-flickr で手動送信)
- 旧実装の原本: yhonda-ohishi-pub-dev/rust-logi `src/services/cam_files_service.rs` (public、tarball で読める)
