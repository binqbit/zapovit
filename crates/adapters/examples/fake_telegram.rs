//! Synthetic Bot API fixture for the tracked Compose integration workflow.
//! This binary is copied only to the Docker integration-fixture target.
use axum::{
    Json, Router,
    extract::{Path, State},
    routing::{get, post},
};
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::sync::Mutex;

#[derive(Default)]
struct Fixture {
    updates: Vec<Value>,
    messages: Vec<Value>,
    next_message: i64,
}

async fn api(
    State(state): State<Arc<Mutex<Fixture>>>,
    Path(method): Path<String>,
    Json(body): Json<Value>,
) -> Json<Value> {
    if method == "getUpdates" {
        let offset = body["offset"].as_i64().unwrap_or(0);
        for _ in 0..20 {
            let updates: Vec<_> = state
                .lock()
                .await
                .updates
                .iter()
                .filter(|v| v["update_id"].as_i64().unwrap_or(0) >= offset)
                .cloned()
                .collect();
            if !updates.is_empty() {
                return Json(json!({"ok":true,"result":updates}));
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        return Json(json!({"ok":true,"result":[]}));
    }
    let result = match method.as_str() {
        "getMe" => {
            json!({"id":1,"is_bot":true,"first_name":"Synthetic","username":"zapovit_synthetic_bot"})
        }
        "sendMessage" | "editMessageText" => {
            let mut state = state.lock().await;
            state.next_message += 1;
            let id = state.next_message;
            state.messages.push(body);
            json!({"message_id":id})
        }
        "setMyCommands" | "answerCallbackQuery" | "deleteMessage" => json!(true),
        _ => {
            return Json(
                json!({"ok":false,"error_code":400,"description":"Unsupported fixture method"}),
            );
        }
    };
    Json(json!({"ok":true,"result":result}))
}

#[tokio::main]
async fn main() {
    let state = Arc::new(Mutex::new(Fixture::default()));
    let router = Router::new()
        .route("/bot1:synthetic/{method}", post(api))
        .route(
            "/test/update",
            post(
                |State(state): State<Arc<Mutex<Fixture>>>, Json(update): Json<Value>| async move {
                    state.lock().await.updates.push(update);
                    Json(json!({"accepted":true}))
                },
            ),
        )
        .route(
            "/test/message-count",
            get(|State(state): State<Arc<Mutex<Fixture>>>| async move {
                Json(json!({"count":state.lock().await.messages.len()}))
            }),
        )
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("0.0.0.0:8081").await.unwrap();
    axum::serve(listener, router).await.unwrap();
}
