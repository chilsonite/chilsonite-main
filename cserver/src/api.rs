use std::convert::Infallible;

use crate::api::dto::{
    CommandRequestParams, CurrentUserResponse, ErrorResponse, LoginRequest, LoginResponse,
    RegisterRequest, RegisterResponse,
};
use crate::repository::get_user_tokens;
use crate::repository::UserRole;
use crate::repository::{create_user, get_user_by_id, get_user_by_username, get_user_points};
use crate::token::generate_token;
use crate::token::TokenResponse;
use crate::websocket::send_message;
use crate::AppState;
use crate::{agent, repository::create_user_points};
use anyhow::Result;
use argon2::password_hash::rand_core::OsRng;
use argon2::{
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};
use axum::response::sse::{Event, KeepAlive};
use axum::{
    extract::State,
    response::{IntoResponse, Response, Sse},
    routing::{get, post},
    Json, Router,
};
use axum_extra::extract::cookie::{Cookie, CookieJar};
use base64::engine::general_purpose::STANDARD;
use base64::Engine; // for STANDARD.decode
use chrono::Utc;
use common::Payload;
use encoding_rs::SHIFT_JIS;
use futures::stream::{self, StreamExt};
use hyper::StatusCode;
use jsonwebtoken::{decode, encode, DecodingKey, EncodingKey, Header, Validation};
use log::{debug, error, info};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use uuid::Uuid;

// Expose centralized DTO types
pub mod dto;

// Insert common error response helper
fn err(code: StatusCode, msg: &str) -> Response {
    (code, Json(json!({ "error": msg }))).into_response()
}

// Insert common auth helper
fn authenticate(state: &AppState, jar: &CookieJar) -> Result<Claims, Response> {
    let token = jar
        .get("session")
        .map(|c| c.value().to_string())
        .ok_or_else(|| err(StatusCode::UNAUTHORIZED, "Unauthenticated"))?;
    let data = decode::<Claims>(
        &token,
        &DecodingKey::from_secret(state.jwt_secret.as_ref()),
        &Validation::default(),
    )
    .map_err(|_| err(StatusCode::UNAUTHORIZED, "Invalid token"))?;
    Ok(data.claims)
}

// APIルーターを構築する関数
pub(crate) fn build_api_router(state: AppState) -> Router {
    Router::new()
        .route("/api/register", post(register))
        .route("/api/login", post(login))
        .route("/api/token", post(generate_token))
        .route("/api/agents", get(agent::list_agents))
        .route("/api/command", post(execute_command))
        .route("/api/me", get(get_current_user))
        .route("/api/me/tokens", get(list_tokens))
        .with_state(state)
}

#[utoipa::path(
    get,
    path = "/api/me",
    responses(
        (status = 200, description = "Current user info", body = CurrentUserResponse),
        (status = 401, description = "Unauthenticated", body = ErrorResponse),
        (status = 404, description = "User not found", body = ErrorResponse)
    ),
    tag = "Auth"
)]
async fn get_current_user(State(state): State<AppState>, jar: CookieJar) -> impl IntoResponse {
    let claims = match authenticate(&state, &jar) {
        Ok(c) => c,
        Err(resp) => return resp.into_response(),
    };
    // ユーザID取得
    let user_id = Uuid::parse_str(&claims.sub).unwrap();
    // ユーザ情報取得
    match get_user_by_id(&state.db_pool, user_id).await {
        Ok(Some(user)) => {
            // ポイント取得
            let pts = match get_user_points(&state.db_pool, user_id).await {
                Ok(Some(rec)) => rec.points,
                _ => 0,
            };
            let resp = CurrentUserResponse {
                id: user.id,
                username: user.username,
                role: user.role,
                points: pts,
            };
            (StatusCode::OK, Json(resp)).into_response()
        }
        _ => err(StatusCode::NOT_FOUND, "User not found").into_response(),
    }
}

// コマンド実行リクエストを処理し、結果をSSEでストリーミングするAPIハンドラ
#[utoipa::path(
    post,
    path = "/api/command",
    request_body = CommandRequestParams,
    responses(
        (status = 200, description = "Stream command output via SSE", body = ()),
        (status = 401, description = "Unauthenticated", body = ErrorResponse),
        (status = 403, description = "Forbidden", body = ErrorResponse),
        (status = 404, description = "Agent not found", body = ErrorResponse)
    ),
    tag = "Agent"
)]
async fn execute_command(
    State(state): State<AppState>,
    jar: CookieJar,
    Json(req): Json<CommandRequestParams>,
) -> impl IntoResponse {
    let claims = match authenticate(&state, &jar) {
        Ok(c) => c,
        Err(resp) => return resp,
    };
    // 管理者権限チェック
    if claims.role != UserRole::Admin {
        return err(StatusCode::FORBIDDEN, "管理者のみ実行可能");
    }
    let request_id = Uuid::now_v7().to_string(); // Use now_v7() for current time
    info!(
        "[{}] Received API command request for agent {}: {}",
        request_id, req.agent_id, req.command
    );

    // 指定されたエージェントが存在するか確認
    if let Some(agent) = state.agents.get(&req.agent_id) {
        // 結果ストリーミング用のMPSCチャネルを作成
        let (tx, rx) = mpsc::channel::<Payload>(10);

        // CommandResponseMapにsenderを登録
        {
            let mut map = state.command_responses.lock().await;
            map.insert(request_id.clone(), tx);
        }

        // エージェントにCommandRequestペイロードを送信
        let payload = Payload::CommandRequest {
            request_id: request_id.clone(),
            command: req.command.clone(),
        };

        let agent_conn_clone = agent.clone();
        let req_id_clone = request_id.clone();
        let agent_id_clone = req.agent_id.clone();
        let cmd_resp_map_clone = state.command_responses.clone();

        // 別のタスクでメッセージ送信を実行（エラーハンドリングを含む）
        tokio::spawn(async move {
            if let Err(e) = send_message(&agent_conn_clone.sink, payload).await {
                error!(
                    "[{}] Failed to send command request to agent {}: {}",
                    req_id_clone, agent_id_clone, e
                );
                // 送信失敗時はCommandResponseMapからsenderを削除
                let mut map = cmd_resp_map_clone.lock().await;
                map.remove(&req_id_clone);
            } else {
                info!(
                    "[{}] Sent command request to agent {}",
                    req_id_clone, agent_id_clone
                );
            }
        });

        // MPSCレシーバーからSSEストリームを作成
        // flat_mapを使用して、1つのペイロードから複数のSSEイベントを生成可能にする
        let stream = ReceiverStream::new(rx).flat_map(move |payload: Payload| {
            // クロージャでリクエストIDをキャプチャ
            let captured_request_id = request_id.clone();
            match payload {
                Payload::CommandResponseChunk { chunk_id, stream_type, data, .. } => {
                    // Base64デコード
                    match STANDARD.decode(&data) {
                        Ok(decoded_bytes) => {
                            // まずUTF-8としてデコードを試みる
                            let decoded_string = match String::from_utf8(decoded_bytes.clone()) {
                                Ok(s) => s,
                                Err(_) => {
                                    // UTF-8デコード失敗時はShift_JISとしてデコードを試みる
                                    let decoded_shift_jis = SHIFT_JIS.decode_without_bom_handling_and_without_replacement(&decoded_bytes);
                                    if let Some(cow) = decoded_shift_jis {
                                        debug!("[{}] Decoded chunk {} using SHIFT_JIS.", captured_request_id, chunk_id);
                                        cow.into_owned()
                                    } else {
                                        // Shift_JISでもデコード失敗時は損失許容でUTF-8に変換
                                        error!(
                                            "[{}] Failed to decode chunk {} using SHIFT_JIS. Using lossy UTF-8.",
                                            captured_request_id, chunk_id
                                        );
                                        String::from_utf8_lossy(&decoded_bytes).to_string()
                                    }
                                }
                            };

                            // ストリームタイプに応じてイベントタイプを設定 (1: stdout, 2: stderr)
                            let event_type = if stream_type == 1 { "stdout" } else { "stderr" };
                            debug!(
                                "[{}] SSE processing chunk {} ({}): {} bytes -> '{}'",
                                captured_request_id,
                                chunk_id,
                                event_type,
                                decoded_bytes.len(),
                                decoded_string.escape_debug() // 制御文字などをエスケープしてログ出力
                            );

                            // デコードされた文字列を行ごとに分割し、各行を個別のSSEイベントとして送信
                            let events: Vec<Result<Event, Infallible>> = decoded_string
                                .lines() // '\n' または '\r\n' で分割
                                .map(|line| {
                                    // SSEデータフィールドは改行を含められないため、trim_end_matches('\r')で末尾の\rを削除
                                    let trimmed_line = line.trim_end_matches('\r');
                                    Ok(Event::default().event(event_type).data(trimmed_line.to_string()))
                                })
                                .collect();

                            // イベントのストリームを返す
                            stream::iter(events)
                        }
                        Err(e) => { // Base64デコード失敗
                            error!("[{}] Failed to decode base64 chunk {}: {}", captured_request_id, chunk_id, e);
                            // エラーイベントを含むストリームを返す
                            stream::iter(vec![Ok(Event::default()
                                .event("error")
                                .data(format!("Base64 decode error: {}", e)))])
                        }
                    }
                }
                Payload::CommandResponseTransferComplete { success, exit_code, error_message, .. } => {
                    // コマンド完了通知
                    info!("[{}] SSE processing command completion. Success: {}, ExitCode: {:?}, Error: {:?}", captured_request_id, success, exit_code, error_message);
                    let mut events = Vec::new();
                    if success {
                        // 成功時は終了コードを含む "done" イベントを送信
                         events.push(Ok(Event::default().event("done").data(format!("ExitCode: {:?}", exit_code))));
                    } else {
                        // 失敗時はエラーメッセージを行ごとに分割して "error" イベントとして送信
                        let error_str = error_message.unwrap_or_else(|| "Unknown error".to_string());
                        for line in error_str.lines() {
                             events.push(Ok(Event::default().event("error").data(line.trim_end_matches('\r').to_string())));
                        }
                        // 失敗時も終了コードがあれば "done" イベントとして送信
                        events.push(Ok(Event::default().event("done").data(format!("ExitCode: {:?}", exit_code))));
                    }
                    // 完了/エラーイベントのストリームを返す
                    stream::iter(events)
                }
                 _ => { // 予期しないペイロードタイプ
                    error!("[{}] Received unexpected payload type in SSE stream", captured_request_id);
                     // 内部エラーイベントを含むストリームを返す
                    stream::iter(vec![Ok(Event::default()
                        .event("error")
                        .data("Internal server error: Unexpected payload"))])
                }
            }
        });

        // SSEレスポンスを生成
        return Sse::new(stream)
            .keep_alive(KeepAlive::default())
            .into_response();
    }
    // 指定されたエージェントが見つからない場合
    error!("[{}] Agent not found: {}", request_id, req.agent_id);
    // 404 Not Found エラーを返す (SSEではなく通常のJSONレスポンス)
    return err(StatusCode::NOT_FOUND, "Agent not found");
}

// JWTのクレーム
#[derive(Serialize, Deserialize)]
pub struct Claims {
    pub sub: String,
    pub role: UserRole,
    pub exp: usize,
}

// ユーザ登録エンドポイント
#[utoipa::path(
    post,
    path = "/api/register",
    request_body = RegisterRequest,
    responses(
        (status = 201, description = "User created", body = RegisterResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    tag = "Auth"
)]
async fn register(
    State(state): State<AppState>,
    Json(req): Json<RegisterRequest>,
) -> impl IntoResponse {
    let user_id = Uuid::now_v7();
    // Generate salt and hash password
    let salt = SaltString::generate(OsRng);
    let password_hash = Argon2::default()
        .hash_password(req.password.as_bytes(), &salt)
        .unwrap()
        .to_string();
    let now = Utc::now();
    // users テーブルに登録
    if let Err(e) = create_user(
        &state.db_pool,
        user_id,
        &req.username,
        &password_hash,
        UserRole::User,
        now,
    )
    .await
    {
        return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string());
    }
    // user_points 初期化
    // 初期段階では1000ポイントを付与
    if let Err(e) = create_user_points(&state.db_pool, user_id, 1000, now).await {
        return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string());
    }
    return (
        StatusCode::CREATED,
        Json(json!({ "user_id": user_id.to_string() })),
    )
        .into_response();
}

// ログインエンドポイント
#[utoipa::path(
    post,
    path = "/api/login",
    request_body = LoginRequest,
    responses(
        (status = 200, description = "Login successful", body = LoginResponse),
        (status = 401, description = "Invalid credentials", body = ErrorResponse)
    ),
    tag = "Auth"
)]
async fn login(
    State(state): State<AppState>,
    jar: CookieJar,
    Json(req): Json<LoginRequest>,
) -> Result<(CookieJar, Json<serde_json::Value>), (StatusCode, Json<serde_json::Value>)> {
    // ユーザ取得
    let user = match get_user_by_username(&state.db_pool, &req.username).await {
        Ok(Some(u)) => u,
        _ => {
            return Err((
                StatusCode::UNAUTHORIZED,
                Json(json!({"error":"Invalid credentials"})),
            ))
        }
    };
    // パスワード検証
    let parsed = PasswordHash::new(&user.password_hash).unwrap();
    if Argon2::default()
        .verify_password(req.password.as_bytes(), &parsed)
        .is_err()
    {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(json!({"error":"Invalid credentials"})),
        ));
    }
    // JWT生成
    let exp = (Utc::now().timestamp() + 3600) as usize;
    let claims = Claims {
        sub: user.id.to_string(),
        role: user.role.clone(),
        exp,
    };
    let token = encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(state.jwt_secret.as_ref()),
    )
    .unwrap();
    // Cookieに設定
    let cookie = Cookie::build(("session", token))
        .path("/")
        .http_only(true)
        .build();
    let jar = jar.add(cookie);
    Ok((jar, Json(json!({"success": true}))))
}

// 新規: 認証済みユーザの有効トークン一覧取得エンドポイント
#[utoipa::path(
    get,
    path = "/api/me/tokens",
    responses(
        (status = 200, description = "List of tokens", body = [TokenResponse]),
        (status = 401, description = "Unauthenticated", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    tag = "Auth"
)]
async fn list_tokens(State(state): State<AppState>, jar: CookieJar) -> impl IntoResponse {
    let claims = match authenticate(&state, &jar) {
        Ok(c) => c,
        Err(resp) => return resp,
    };
    let user_id = Uuid::parse_str(&claims.sub).unwrap();

    // DBからトークン取得
    match get_user_tokens(&state.db_pool, user_id).await {
        Ok(recs) => {
            let resp: Vec<TokenResponse> = recs
                .into_iter()
                .map(|r| TokenResponse {
                    token: r.token,
                    expires_at: r.expires_at.timestamp(),
                })
                .collect();
            (StatusCode::OK, Json(resp)).into_response()
        }
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}
