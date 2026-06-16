//! Cache experiment dummy test B (Refs #28)
//!
//! `cache_experiment_a.rs` と pair で matrix split を作る。
//! 本番ロジックには関係しない。

#[tokio::test]
async fn experiment_b_smoke() {
    use axum::Router;
    let _router: Router = Router::new();
    let _json = serde_json::json!({"experiment": "b"});
    let _id = uuid::Uuid::new_v4();
}
