# cam scrape (SyncCamFiles) の Cloudflare Worker 移行 設計

Refs #39 / Part of #36

## 対象範囲

- **移行する**: `POST /sync` 相当の一連処理 — `src/cam.rs` (`CamClient`: CF Access
  ヘッダ + RFC2617 Digest(MD5) リトライ、SD カード XML 巡回、jpg/mp4 CGI 出し分け
  download) + `src/routes.rs::sync_cam_files` (巡回 → `cam_files` UPSERT → 古い順
  Flickr upload、SD_ZOMBIE センチネル判定、upload floor 計算)
- **移行しない (Cloud Run に残す)**: `/import` (Flickr `photos.getInfo` 検証)、
  `/stats` (日次集計)、`/oauth` (OAuth1.0a 認可フロー)。理由: カメラ CGI との通信を
  持つのは `/sync` だけであり、issue #36 の動機 (cam-* secret の 1 本化) もここに
  閉じる。他 3 エンドポイントを動かす理由が無い
- 上記の分割自体が是か (= 本当に `/sync` だけ切り出せるか、DB スキーマ上の依存で
  Cloud Run 側が pool を持ち続ける必要があるか) は本 doc のレビューで確認する

## 配置先: 新規 repo

`cf-flickr-proxy` (既存) には積まない。`cf-flickr-proxy-map` skill の規範
「org をデフォルト注入しない」「依存ゼロを保つ」「素の fetch handler で足りる」は
**薄い proxy であることが前提**の制約であり、digest 認証・XML parse・DB 書き込み・
Flickr upload という実体のあるビジネスロジックを持ち込むと proxy の設計原則と
衝突する。新規 repo (仮称 `ippoan/cf-flickr-cam-worker`) を作る。

## Open Questions (要検証 — 実装 PR 前に解消する)

以下は実機検証済みの org 内前例が無いため、本 plan では選択肢を並べるに留め
断定しない。

### 1. DB 接続方式 (Supabase Postgres, RLS `set_current_organization`)

現行は sqlx で生 SQL、`set_current_organization($1)` を接続取得直後に毎回呼んで
session-local RLS を効かせている。Workers から直接この方式を踏襲する手段:

- **(a) Hyperdrive + node-postgres 系ドライバ (TCP)**: RLS パターンをそのまま
  踏襲できる。org 内に Hyperdrive 利用の前例なし (`grep -rn Hyperdrive */wrangler.*`
  で 0 件) — 要 spike。接続プーリング挙動が Cloud Run の sqlx pool と異なる点も
  要確認
- **(b) Supabase REST (PostgREST) + service role key**: TCP 不要で楽だが、
  `set_current_organization()` は session GUC を立てる関数であり、PostgREST の
  各リクエストが独立接続である場合 RLS session が保持されない懸念がある。
  RPC 1 呼び出しでトランザクション内に完結させる設計に作り直す必要があり、
  現行ロジックの単純移植ではなくなる
- **(c) 薄い RPC 層を挟む**: Cloud Run 側 (rust-flickr 自体、または新規小サービス)
  に `cam_files` 用の narrow HTTP endpoint を残し、Worker はそれを叩く。RLS
  パターンは Postgres session を持つ側 (Cloud Run) に閉じ込められるので安全だが
  「Worker 化」の恩恵 (Cloud Run 依存を切る) が薄れる

推奨は (a) だが Hyperdrive の前例が無いため、**実装着手前に spike で実証**する。

### 2. Digest 認証 (RFC2617 / MD5) の Workers runtime 実現方式

Workers の `SubtleCrypto` は MD5 を持たない (SHA-1/256/384/512 のみ)。
`nodejs_compat` フラグ + `node:crypto` の `createHash('md5')` が使えるか、
または純 JS MD5 実装 (`spark-md5` 等の依存追加) が必要か要検証。
`nodejs_compat` 自体は org 内に前例多数 (nuxt-egov / secrets-inventory 等) だが
`createHash` MD5 の可否はそれとは別に確認する。

### 3. XML parse / OAuth1 HMAC-SHA1 / multipart upload

- XML parse (`<Dir name="...">` / `<Name>...</Name>` 抽出): 既存ロジックは単純な
  タグ抽出なので `fast-xml-parser` か正規表現で素直に移植可能 (要検証事項ではない)
- HMAC-SHA1 (Flickr OAuth1 署名): `crypto.subtle.sign('HMAC', ...)` で標準対応、
  問題なし
- multipart upload (Flickr Upload API): Web platform `FormData` + `fetch` で
  Rust の `reqwest::multipart` 相当を代替可能、問題なし

### 4. Cron Trigger の実行時間制約

Cloud Scheduler → `POST /sync` (`*/10`, upload_limit:100) を Workers
`scheduled()` ハンドラ (`[triggers] crons`) に置き換える。1 回の巡回で
download+upload を `upload_limit` 件ぶん逐次実行するため、Workers の
CPU time / wall-clock 制限内に収まるか要検証。収まらない場合は
`upload_limit` を下げる、または複数回の cron 起動に分割する対応を検討

## Secret 構造 (#36 準拠)

cam 設定 6 secret (`digest_user` / `digest_pass` / `machine_name` / `jpg_cgi` /
`mp4_cgi` / `sdcard_cgi`) は JSON 1 本 (`rust-flickr-cam-config`) にまとめ、
CF Secrets Store (Worker binding) + GCP Secret Manager (SoT バックアップ、
org 規約) に投入する。投入は secret-inject skill 経由 (値を会話・log に載せない)。
Flickr consumer key/secret、DB 接続情報 (方式は上記 Open Question 次第) も同様に
CF Secrets Store binding で配布する。

## ロールアウト手順

1. Worker を staging で「読み取りのみ (upload は行わない dry-run モード)」で
   並行稼働させ、Cloud Run `/sync` と `cam_files` UPSERT 結果を突合する
2. 突合が一致したら Worker 側で実 upload を有効化し、Cloud Scheduler
   `rust-flickr-sync` job を無効化 (削除ではなく disable — 切り戻し可能に)
3. 一定期間 (目安 1〜2 週間) 安定稼働を確認後、Cloud Run 側の cam 関連コード
   (`cam.rs`、`sync_cam_files`、関連 `db.rs` 関数) と Cloud Scheduler job を撤去
4. 旧 cam-* 6 secret を Secret Audit workflow で quarantine → 30 日後 destroy (#36)
5. README / CLAUDE.md / `rust-flickr-map` skill のパイプライン図を更新
   (`/sync` を Worker 側に差し替え)

## 移植時に精度を落としてはいけないロジック (既存テストをそのまま移植する)

- RFC2617 Digest response の既知ベクタ (`digest_response_rfc2617_known_vector`)
- SD_ZOMBIE センチネル判定 (camera CGI が `text/plain` を返す = SD 上に実体が
  無いケースを `flickr_id='SD_ZOMBIE'` で自動 mark)
- upload floor 計算 ((date, hour) 粒度、SD 実在最古を下限にする — 日付粒度だけだと
  過去の「永久ゴースト」が残数に積まれ続ける #24 の教訓)
- upload は **古い順 (ORDER BY name)**、下限は巡回再開位置ではなく SD 実在最古日
  (#11 — 巡回再開位置を下限にすると backfill 後に過去分が漏れる)
- `_!` を含むファイル名 (カメラの一時ファイル) の除外
