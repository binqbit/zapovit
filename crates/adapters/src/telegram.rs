use application::{Error, Result, SendResult};
use domain::Block;
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::{collections::HashMap, sync::Arc, time::Duration};
use teloxide_core::types::{MessageEntity, MessageEntityKind};
use tokio::sync::{Mutex, Semaphore};
use zeroize::Zeroizing;

#[derive(Clone)]
pub struct Telegram {
    client: reqwest::Client,
    base: String,
    token: Arc<Zeroizing<String>>,
    files: Arc<Semaphore>,
    pacing: Arc<Mutex<Pacing>>,
}
struct Pacing {
    global: tokio::time::Instant,
    chats: HashMap<i64, tokio::time::Instant>,
    cooldown: tokio::time::Instant,
}
pub enum DeleteResult {
    Deleted,
    RetryAfter(i64),
    Permanent,
}
impl Telegram {
    pub fn new(base: &str, token: Zeroizing<String>) -> Result<Self> {
        let url = reqwest::Url::parse(base).map_err(|_| Error::Config)?;
        if !["http", "https"].contains(&url.scheme())
            || !url.username().is_empty()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(Error::Config);
        }
        if url.scheme() != "https"
            && !matches!(
                url.host_str(),
                Some("127.0.0.1" | "localhost" | "[::1]" | "fake-telegram")
            )
        {
            return Err(Error::Config);
        }
        if token.len() > 256 || !token.contains(':') {
            return Err(Error::Config);
        }
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(40))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| Error::Config)?;
        Ok(Self {
            client,
            base: base.trim_end_matches('/').into(),
            token: Arc::new(token),
            files: Arc::new(Semaphore::new(2)),
            pacing: Arc::new(Mutex::new(Pacing {
                global: tokio::time::Instant::now(),
                chats: HashMap::new(),
                cooldown: tokio::time::Instant::now(),
            })),
        })
    }
    pub async fn reserve_send(&self, chat: i64) {
        let mut at = {
            let mut pacing = self.pacing.lock().await;
            let now = tokio::time::Instant::now();
            pacing.chats.retain(|_, at| *at > now);
            let at = pacing
                .global
                .max(*pacing.chats.get(&chat).unwrap_or(&now))
                .max(pacing.cooldown)
                .max(now);
            pacing.global = at + Duration::from_millis(50);
            pacing.chats.insert(chat, at + Duration::from_secs(1));
            at
        };
        loop {
            tokio::time::sleep_until(at).await;
            let mut pacing = self.pacing.lock().await;
            let now = tokio::time::Instant::now();
            if pacing.cooldown <= now {
                break;
            }
            // A 429 may have arrived after this request reserved its slot.
            at = pacing
                .cooldown
                .max(pacing.global)
                .max(*pacing.chats.get(&chat).unwrap_or(&now));
            pacing.global = at + Duration::from_millis(50);
            pacing.chats.insert(chat, at + Duration::from_secs(1));
        }
    }
    async fn remaining_cooldown(&self) -> Option<i64> {
        let pacing = self.pacing.lock().await;
        let remaining = pacing
            .cooldown
            .saturating_duration_since(tokio::time::Instant::now());
        (!remaining.is_zero()).then(|| remaining.as_secs().saturating_add(1) as i64)
    }
    async fn observe_retry(&self, result: &SendResult) {
        if let SendResult::RetryAfter(seconds) = result {
            let mut pacing = self.pacing.lock().await;
            let until =
                tokio::time::Instant::now() + Duration::from_secs((*seconds).clamp(1, 3600) as u64);
            pacing.cooldown = pacing.cooldown.max(until);
            pacing.global = pacing.global.max(until);
        }
    }
    fn url(&self, method: &str) -> String {
        format!("{}/bot{}/{method}", self.base, self.token.as_str())
    }
    async fn bounded(response: reqwest::Response, max: usize) -> Result<Value> {
        if response.content_length().is_some_and(|n| n > max as u64) {
            return Err(Error::Storage);
        }
        let mut chunks = response.bytes_stream();
        let mut bytes = Zeroizing::new(Vec::new());
        while let Some(chunk) = chunks.next().await {
            let chunk = chunk.map_err(|_| Error::Storage)?;
            if bytes.len() + chunk.len() > max {
                return Err(Error::Storage);
            }
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&bytes).map_err(|_| Error::Storage)
    }
    pub async fn me(&self) -> Result<(i64, String)> {
        let r = self
            .client
            .post(self.url("getMe"))
            .json(&json!({}))
            .send()
            .await
            .map_err(|_| Error::Storage)?;
        let v = Self::bounded(r, 65536).await?;
        if v["ok"] != true {
            return Err(Error::Config);
        }
        Ok((
            v["result"]["id"].as_i64().ok_or(Error::Config)?,
            v["result"]["username"]
                .as_str()
                .ok_or(Error::Config)?
                .into(),
        ))
    }
    /// Caller persists the returned updates and cursor in one transaction before polling again.
    pub async fn poll(&self, offset: i64) -> Result<Vec<Value>> {
        let r=self.client.post(self.url("getUpdates")).json(&json!({"offset":offset,"timeout":30,"limit":100,"allowed_updates":["message","callback_query"]})).send().await.map_err(|_|Error::Storage)?;
        let value = Self::bounded(r, 8 * 1024 * 1024).await?;
        if value["ok"] != true {
            return Err(Error::Storage);
        }
        value["result"].as_array().cloned().ok_or(Error::Storage)
    }
    pub async fn answer_callback(&self, id: &str) {
        let _ = self
            .client
            .post(self.url("answerCallbackQuery"))
            .timeout(Duration::from_secs(5))
            .json(&json!({"callback_query_id":id}))
            .send()
            .await;
    }
    pub async fn delete(&self, chat: i64, message: i64) -> DeleteResult {
        if let Some(seconds) = self.remaining_cooldown().await {
            return DeleteResult::RetryAfter(seconds);
        }
        let result = self
            .client
            .post(self.url("deleteMessage"))
            .timeout(Duration::from_secs(15))
            .json(&json!({"chat_id":chat,"message_id":message}))
            .send()
            .await;
        let Ok(response) = result else {
            return DeleteResult::RetryAfter(15);
        };
        let Ok(value) = Self::bounded(response, 65536).await else {
            return DeleteResult::RetryAfter(15);
        };
        if value["ok"] == true
            || (value["error_code"] == 400
                && value["description"]
                    .as_str()
                    .is_some_and(|d| d.contains("message to delete not found")))
        {
            return DeleteResult::Deleted;
        }
        if value["error_code"] == 429 {
            let seconds = value["parameters"]["retry_after"]
                .as_i64()
                .unwrap_or(60)
                .clamp(1, 3600);
            self.observe_retry(&SendResult::RetryAfter(seconds)).await;
            return DeleteResult::RetryAfter(seconds);
        }
        if matches!(value["error_code"].as_i64(), Some(400 | 401 | 403 | 404)) {
            return DeleteResult::Permanent;
        }
        // Deletion is idempotent, so a lost response can safely be retried.
        DeleteResult::RetryAfter(15)
    }
    pub async fn send_text(
        &self,
        chat: i64,
        text: &str,
        entities: Vec<MessageEntity>,
        buttons: Vec<(String, String)>,
        reply_to: Option<i64>,
    ) -> SendResult {
        if let Some(seconds) = self.remaining_cooldown().await {
            return SendResult::RetryAfter(seconds);
        }
        self.reserve_send(chat).await;
        self.send_text_reserved(chat, text, entities, buttons, reply_to)
            .await
    }
    /// Update a non-secret navigation message and its inline buttons in place.
    pub async fn edit_menu(
        &self,
        chat: i64,
        message: i64,
        text: &str,
        buttons: Vec<(String, String)>,
    ) -> SendResult {
        if text.is_empty() || text.encode_utf16().count() > 4096 {
            return SendResult::Permanent;
        }
        if let Some(seconds) = self.remaining_cooldown().await {
            return SendResult::RetryAfter(seconds);
        }
        self.reserve_send(chat).await;
        let keyboard: Vec<_> = buttons
            .into_iter()
            .map(|(text, data)| vec![json!({"text": text, "callback_data": data})])
            .collect();
        let response = self
            .client
            .post(self.url("editMessageText"))
            .timeout(Duration::from_secs(15))
            .json(&json!({"chat_id":chat,"message_id":message,"text":text,
                "entities":[],"link_preview_options":{"is_disabled":true},
                "reply_markup":{"inline_keyboard":keyboard}}))
            .send()
            .await;
        let result = Self::classify(response).await;
        self.observe_retry(&result).await;
        result
    }
    pub async fn send_text_reserved(
        &self,
        chat: i64,
        text: &str,
        entities: Vec<MessageEntity>,
        buttons: Vec<(String, String)>,
        reply_to: Option<i64>,
    ) -> SendResult {
        if let Some(seconds) = self.remaining_cooldown().await {
            return SendResult::RetryAfter(seconds);
        }
        if text.is_empty() || text.encode_utf16().count() > 4096 {
            return SendResult::Permanent;
        }
        // Recipients must be able to copy keys and save inherited data.
        // Telegram content protection prevents those operations.
        let mut keyboard: Vec<Vec<Value>> = buttons
            .into_iter()
            .map(|(text, data)| vec![json!({"text":text,"callback_data":data})])
            .collect();
        for entity in &entities {
            if matches!(entity.kind, MessageEntityKind::Pre { .. })
                && let Some(copy) = copyable_entity_text(text, entity)
            {
                keyboard.insert(0, vec![json!({"text":"📋", "copy_text":{"text":copy}})]);
            }
        }
        let mut body = json!({"chat_id":chat,"text":text,"entities":entities,"link_preview_options":{"is_disabled":true}});
        if !keyboard.is_empty() {
            body["reply_markup"] = json!({"inline_keyboard":keyboard});
        }
        if let Some(id) = reply_to {
            body["reply_parameters"] = json!({"message_id":id,"allow_sending_without_reply":false});
        }
        let result = self
            .client
            .post(self.url("sendMessage"))
            .timeout(Duration::from_secs(15))
            .json(&body)
            .send()
            .await;
        let result = Self::classify(result).await;
        self.observe_retry(&result).await;
        result
    }
    pub async fn send_block(
        &self,
        chat: i64,
        block: &Block,
        file: Option<Zeroizing<Vec<u8>>>,
    ) -> SendResult {
        if let Some(seconds) = self.remaining_cooldown().await {
            return SendResult::RetryAfter(seconds);
        }
        self.reserve_send(chat).await;
        self.send_block_reserved(chat, block, file).await
    }
    pub async fn send_block_reserved(
        &self,
        chat: i64,
        block: &Block,
        file: Option<Zeroizing<Vec<u8>>>,
    ) -> SendResult {
        if let Some(seconds) = self.remaining_cooldown().await {
            return SendResult::RetryAfter(seconds);
        }
        match block {
            Block::Text { text } => {
                self.send_text_reserved(chat, text, vec![], vec![], None)
                    .await
            }
            Block::Copyable { text } => {
                self.send_text_reserved(
                    chat,
                    text,
                    vec![entity(text, MessageEntityKind::Pre { language: None })],
                    vec![],
                    None,
                )
                .await
            }
            Block::Spoiler { text } => {
                self.send_text_reserved(
                    chat,
                    text,
                    vec![entity(text, MessageEntityKind::Spoiler)],
                    vec![],
                    None,
                )
                .await
            }
            Block::File { name, caption, .. } => {
                let Some(bytes) = file else {
                    return SendResult::Permanent;
                };
                if bytes.len() > 10 * 1024 * 1024 || caption.encode_utf16().count() > 1024 {
                    return SendResult::Permanent;
                }
                let Ok(_permit) = self.files.try_acquire() else {
                    return SendResult::RetryAfter(1);
                };
                let part = reqwest::multipart::Part::bytes(bytes.to_vec()).file_name(name.clone());
                let form = reqwest::multipart::Form::new()
                    .text("chat_id", chat.to_string())
                    .text("caption", caption.clone())
                    .part("document", part);
                let result = self
                    .client
                    .post(self.url("sendDocument"))
                    .timeout(Duration::from_secs(60))
                    .multipart(form)
                    .send()
                    .await;
                let result = Self::classify(result).await;
                self.observe_retry(&result).await;
                result
            }
        }
    }
    async fn classify(
        result: std::result::Result<reqwest::Response, reqwest::Error>,
    ) -> SendResult {
        let Ok(response) = result else {
            return SendResult::Unknown;
        };
        let status = response.status();
        let Ok(value) = Self::bounded(response, 65536).await else {
            return SendResult::Unknown;
        };
        if status.is_success() && value["ok"] == true {
            return value["result"]["message_id"]
                .as_i64()
                .map(SendResult::Sent)
                .unwrap_or(SendResult::Unknown);
        }
        if value["ok"] == false && value["error_code"] == 429 {
            return SendResult::RetryAfter(
                value["parameters"]["retry_after"].as_i64().unwrap_or(60),
            );
        }
        if value["ok"] == false
            && matches!(value["error_code"].as_i64(), Some(400 | 401 | 403 | 404))
        {
            return SendResult::Permanent;
        }
        SendResult::Unknown
    }
    pub async fn download(&self, file_id: &str) -> Result<Zeroizing<Vec<u8>>> {
        let _permit = self.files.acquire().await.map_err(|_| Error::Internal)?;
        let r = self
            .client
            .post(self.url("getFile"))
            .json(&json!({"file_id":file_id}))
            .send()
            .await
            .map_err(|_| Error::Storage)?;
        let v = Self::bounded(r, 65536).await?;
        if v["ok"] != true {
            return Err(Error::Storage);
        }
        if v["result"]["file_size"]
            .as_u64()
            .is_some_and(|n| n > 10 * 1024 * 1024)
        {
            return Err(Error::InvalidInput);
        }
        let path = v["result"]["file_path"].as_str().ok_or(Error::Storage)?;
        if path.starts_with('/')
            || path.split('/').any(|s| s == "..")
            || path.contains(['?', '#', '\\'])
        {
            return Err(Error::Storage);
        }
        let response = self
            .client
            .get(format!(
                "{}/file/bot{}/{path}",
                self.base,
                self.token.as_str()
            ))
            .timeout(Duration::from_secs(60))
            .send()
            .await
            .map_err(|_| Error::Storage)?;
        if !response.status().is_success()
            || response
                .content_length()
                .is_some_and(|n| n > 10 * 1024 * 1024)
        {
            return Err(Error::Storage);
        }
        let mut stream = response.bytes_stream();
        let mut bytes = Zeroizing::new(Vec::new());
        while let Some(chunk) = stream.next().await {
            let c = chunk.map_err(|_| Error::Storage)?;
            if bytes.len() + c.len() > 10 * 1024 * 1024 {
                return Err(Error::InvalidInput);
            }
            bytes.extend_from_slice(&c);
        }
        Ok(bytes)
    }
}
fn copyable_entity_text<'a>(text: &'a str, entity: &MessageEntity) -> Option<&'a str> {
    if entity.length == 0 || entity.length > 256 {
        return None;
    }
    let mut units = 0;
    let mut start = None;
    let mut end = None;
    for (index, ch) in text.char_indices() {
        if units == entity.offset {
            start = Some(index);
        }
        if units == entity.offset + entity.length {
            end = Some(index);
            break;
        }
        units += ch.len_utf16();
    }
    if units == entity.offset + entity.length && end.is_none() {
        end = Some(text.len());
    }
    text.get(start?..end?)
}
pub fn entity(text: &str, kind: MessageEntityKind) -> MessageEntity {
    MessageEntity {
        kind,
        offset: 0,
        length: text.encode_utf16().count(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn entity_offsets_use_utf16_without_normalizing() {
        let text = "🔐 e\u{301} пароль";
        let e = entity(text, MessageEntityKind::Spoiler);
        assert_eq!(e.offset, 0);
        assert_eq!(e.length, 12);
    }
}
