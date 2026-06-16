//! Cache experiment dummy test A (Refs #28)
//!
//! `cache-experiment.yml` matrix split を成立させるためだけの空 integration test。
//! 主要 dep を 1 行ずつ touch して build cost をある程度発生させる。
//! 本番ロジックには関係しない。

#[tokio::test]
async fn experiment_a_smoke() {
    use axum::Router;
    let _router: Router = Router::new();
    let _json = serde_json::json!({"experiment": "a"});
    let _id = uuid::Uuid::new_v4();
}
