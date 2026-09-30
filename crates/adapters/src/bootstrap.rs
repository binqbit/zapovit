use crate::settings::StorageCredentials;
use application::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::Duration;
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

/// The admin token is used only by this one-shot command, never by the bot process.
pub async fn storage_init(
    endpoint: &str,
    token: &str,
    bucket: &str,
    credentials: &StorageCredentials,
    capacity: u64,
) -> Result<()> {
    validate_credentials(credentials)?;
    if token.is_empty() || capacity == 0 {
        return Err(Error::Config);
    }
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| Error::Config)?;
    async fn call(
        client: &reqwest::Client,
        endpoint: &str,
        token: &str,
        method: &str,
        body: Option<Value>,
    ) -> Result<Value> {
        let url = format!("{}/v2/{method}", endpoint.trim_end_matches('/'));
        let request = if let Some(body) = body {
            client.post(url).json(&body)
        } else {
            client.get(url)
        };
        let response = request
            .bearer_auth(token)
            .send()
            .await
            .map_err(|_| Error::Storage)?;
        if !response.status().is_success() {
            return Err(Error::Storage);
        }
        response.json().await.map_err(|_| Error::Storage)
    }
    let mut status = None;
    for _ in 0..15 {
        match call(&client, endpoint, token, "GetClusterStatus", None).await {
            Ok(value) => {
                status = Some(value);
                break;
            }
            Err(_) => tokio::time::sleep(Duration::from_secs(2)).await,
        }
    }
    let status = status.ok_or(Error::Storage)?;
    let nodes = status["nodes"].as_array().ok_or(Error::Storage)?;
    if nodes.len() != 1 {
        return Err(Error::Config);
    }
    if status["layoutVersion"] == 0 {
        call(&client,endpoint,token,"UpdateClusterLayout",Some(json!({"roles":[{"id":nodes[0]["id"],"zone":"local","capacity":capacity,"tags":["zapovit"]}]}))).await?;
        call(
            &client,
            endpoint,
            token,
            "ApplyClusterLayout",
            Some(json!({"version":1})),
        )
        .await?;
    }
    let buckets = call(&client, endpoint, token, "ListBuckets", None).await?;
    let existing = buckets.as_array().ok_or(Error::Storage)?.iter().find(|b| {
        b["globalAliases"]
            .as_array()
            .is_some_and(|a| a.iter().any(|x| x == bucket))
    });
    let bucket_id = if let Some(b) = existing {
        b["id"].as_str().ok_or(Error::Storage)?.to_owned()
    } else {
        call(
            &client,
            endpoint,
            token,
            "CreateBucket",
            Some(json!({"globalAlias":bucket})),
        )
        .await?["id"]
            .as_str()
            .ok_or(Error::Storage)?
            .to_owned()
    };
    // An existing ID must never silently switch to another secret. Garage retains
    // deleted IDs, so importing one again also fails instead of recreating it.
    match key_info(&client, endpoint, token, &credentials.access_key_id).await? {
        Some(key) => {
            if key.access_key_id != credentials.access_key_id
                || key.secret_access_key != credentials.secret_access_key
                || key.expired
            {
                return Err(Error::Config);
            }
        }
        None => {
            #[derive(Serialize)]
            #[serde(rename_all = "camelCase")]
            struct ImportKey<'a> {
                access_key_id: &'a str,
                secret_access_key: &'a str,
                name: &'a str,
            }
            let response = client
                .post(format!("{}/v2/ImportKey", endpoint.trim_end_matches('/')))
                .bearer_auth(token)
                .json(&ImportKey {
                    access_key_id: &credentials.access_key_id,
                    secret_access_key: &credentials.secret_access_key,
                    name: "zapovit-app",
                })
                .send()
                .await
                .map_err(|_| Error::Storage)?;
            if !response.status().is_success() {
                return Err(Error::Storage);
            }
        }
    }
    call(
        &client,
        endpoint,
        token,
        &format!("UpdateKey?id={}", credentials.access_key_id),
        Some(json!({"deny":{"createBucket":true}})),
    )
    .await?;
    call(&client,endpoint,token,"DenyBucketKey",Some(json!({"bucketId":bucket_id,"accessKeyId":credentials.access_key_id,"permissions":{"owner":true,"read":false,"write":false}}))).await?;
    call(&client,endpoint,token,"AllowBucketKey",Some(json!({"bucketId":bucket_id,"accessKeyId":credentials.access_key_id,"permissions":{"owner":false,"read":true,"write":true}}))).await?;
    Ok(())
}

// Use Garage's generated-key format, with random IDs supplied by generate-env.
// Human-selected IDs risk colliding with Garage's permanent key tombstones.
fn validate_credentials(credentials: &StorageCredentials) -> Result<()> {
    let id = &credentials.access_key_id;
    let secret = &credentials.secret_access_key;
    if id.len() != 26
        || !id.starts_with("GK")
        || !id[2..].bytes().all(|byte| byte.is_ascii_hexdigit())
        || secret.len() != 64
        || !secret.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(Error::Config);
    }
    Ok(())
}

#[derive(Deserialize, Zeroize, ZeroizeOnDrop)]
#[serde(rename_all = "camelCase")]
struct KeyInfo {
    access_key_id: String,
    secret_access_key: String,
    expired: bool,
}

async fn key_info(
    client: &reqwest::Client,
    endpoint: &str,
    token: &str,
    access_key_id: &str,
) -> Result<Option<KeyInfo>> {
    let response = client
        .get(format!("{}/v2/GetKeyInfo", endpoint.trim_end_matches('/')))
        .query(&[("id", access_key_id), ("showSecretKey", "true")])
        .bearer_auth(token)
        .send()
        .await
        .map_err(|_| Error::Storage)?;
    let status = response.status();
    let body = Zeroizing::new(response.text().await.map_err(|_| Error::Storage)?);
    if status == reqwest::StatusCode::NOT_FOUND {
        let error: Value = serde_json::from_str(&body).map_err(|_| Error::Storage)?;
        return if error["code"] == "NoSuchAccessKey" {
            Ok(None)
        } else {
            Err(Error::Storage)
        };
    }
    if !status.is_success() {
        return Err(Error::Storage);
    }
    serde_json::from_str(&body)
        .map(Some)
        .map_err(|_| Error::Storage)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Json, Router,
        extract::{Query, State},
        http::{HeaderMap, StatusCode},
        response::IntoResponse,
        routing::{get, post},
    };
    use std::{collections::HashMap, sync::Arc};
    use tokio::sync::Mutex;

    #[derive(Default)]
    struct StateData {
        key: Option<Value>,
        imports: usize,
        grants: Vec<Value>,
        updates: Vec<Value>,
    }

    #[tokio::test]
    async fn bootstrap_reuses_env_key_and_rejects_secret_mismatch_before_grants() {
        let state = Arc::new(Mutex::new(StateData::default()));
        let api = Router::new()
            .route(
                "/v2/GetClusterStatus",
                get(|| async { Json(json!({"layoutVersion":1,"nodes":[{"id":"node"}]})) }),
            )
            .route(
                "/v2/ListBuckets",
                get(|| async {
                    Json(json!([{"id":"bucket-id","globalAliases":["zapovit"]}]))
                }),
            )
            .route(
                "/v2/GetKeyInfo",
                get(
                    |State(state): State<Arc<Mutex<StateData>>>,
                     Query(query): Query<HashMap<String, String>>,
                     headers: HeaderMap| async move {
                        assert_eq!(headers["authorization"], "Bearer synthetic-admin-token");
                        assert_eq!(query["showSecretKey"], "true");
                        match state.lock().await.key.clone() {
                            Some(key) => {
                                assert_eq!(query["id"], key["accessKeyId"]);
                                Json(key).into_response()
                            }
                            None => (
                                StatusCode::NOT_FOUND,
                                Json(json!({"code":"NoSuchAccessKey"})),
                            )
                                .into_response(),
                        }
                    },
                ),
            )
            .route(
                "/v2/ImportKey",
                post(
                    |State(state): State<Arc<Mutex<StateData>>>, Json(mut key): Json<Value>| async move {
                        let mut state = state.lock().await;
                        assert!(state.key.is_none());
                        key["expired"] = json!(false);
                        state.key = Some(key);
                        state.imports += 1;
                        Json(json!({}))
                    },
                ),
            )
            .route(
                "/v2/UpdateKey",
                post(
                    |State(state): State<Arc<Mutex<StateData>>>,
                     Query(query): Query<HashMap<String, String>>,
                     Json(body): Json<Value>| async move {
                        let mut state = state.lock().await;
                        assert_eq!(query["id"], state.key.as_ref().unwrap()["accessKeyId"]);
                        state.updates.push(body);
                        Json(json!({}))
                    },
                ),
            )
            .route(
                "/v2/DenyBucketKey",
                post(|Json(body): Json<Value>| async move {
                    assert_eq!(
                        body["permissions"],
                        json!({"owner":true,"read":false,"write":false})
                    );
                    Json(json!({}))
                }),
            )
            .route(
                "/v2/AllowBucketKey",
                post(
                    |State(state): State<Arc<Mutex<StateData>>>, Json(body): Json<Value>| async move {
                        state.lock().await.grants.push(body);
                        Json(json!({}))
                    },
                ),
            )
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, api).await.unwrap() });
        let mut credentials = StorageCredentials {
            access_key_id: format!("GK{}", "12".repeat(12)),
            secret_access_key: "ab".repeat(32),
        };
        for _ in 0..2 {
            storage_init(
                &endpoint,
                "synthetic-admin-token",
                "zapovit",
                &credentials,
                1_000_000,
            )
            .await
            .unwrap();
        }
        credentials.secret_access_key = "cd".repeat(32);
        assert!(matches!(
            storage_init(
                &endpoint,
                "synthetic-admin-token",
                "zapovit",
                &credentials,
                1_000_000,
            )
            .await,
            Err(Error::Config)
        ));
        let state = state.lock().await;
        assert_eq!(state.imports, 1);
        assert_eq!(
            state.updates,
            vec![json!({"deny":{"createBucket":true}}); 2]
        );
        assert_eq!(state.grants.len(), 2);
        assert!(state.grants.iter().all(|grant| {
            grant["bucketId"] == "bucket-id"
                && grant["permissions"] == json!({"owner":false,"read":true,"write":true})
        }));
        server.abort();
    }
}
