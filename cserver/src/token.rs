use crate::api::dto::ErrorResponse;
use crate::{api::Claims, repository::create_token, AppState};
use axum::{extract::State, response::IntoResponse, Json};
use axum_extra::extract::CookieJar;
use chrono::Utc;
use hyper::StatusCode;
use jsonwebtoken::{decode, DecodingKey, Validation};
use serde::Serialize;
use serde_json::json;
use utoipa::ToSchema;
use uuid::Uuid;

// APIレスポンス用のトークン情報
#[derive(Serialize, ToSchema)]
pub(crate) struct TokenResponse {
    pub token: String,
    // UNIXタイムスタンプで表現される有効期限
    pub expires_at: i64,
}

// 新しいトークンを生成し、ストアに登録するAPIハンドラ
#[utoipa::path(
    post,
    path = "/api/token",
    responses(
        (status = 201, description = "Token generated", body = TokenResponse),
        (status = 401, description = "Unauthenticated", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    tag = "Auth"
)]
pub(crate) async fn generate_token(
    State(state): State<AppState>,
    jar: CookieJar,
) -> impl IntoResponse {
    // 認証済みユーザのJWTをCookieから取得
    let cookie = match jar.get("session") {
        Some(c) => c.value().to_string(),
        None => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error":"Unauthenticated"})),
            )
                .into_response()
        }
    };
    // JWT検証
    let token_data = match decode::<Claims>(
        &cookie,
        &DecodingKey::from_secret(state.jwt_secret.as_ref()),
        &Validation::default(),
    ) {
        Ok(data) => data.claims,
        Err(_) => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error":"Invalid token"})),
            )
                .into_response()
        }
    };
    // UUIDに変換
    let user_id = Uuid::parse_str(&token_data.sub).unwrap();
    // トークン文字列を新規生成
    let new_token = Uuid::now_v7().to_string();
    let now = Utc::now();
    let expires = now + chrono::Duration::hours(24);
    // DBに保存
    if let Err(e) = create_token(&state.db_pool, &new_token, user_id, expires, now).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response();
    }
    // レスポンス
    (
        StatusCode::CREATED,
        Json(TokenResponse {
            token: new_token.clone(),
            expires_at: expires.timestamp(),
        }),
    )
        .into_response()
}
