//! Telegram alert transport over fetch (docker `TelegramAlertNotifier`'s HTTP
//! half). Message text and suppression rules come from
//! `vela_relay_core::alert`; the suppression slot lives in the chain's
//! TreasuryDO.

use serde::{Deserialize, Serialize};
use worker::Delay;

const TELEGRAM_TIMEOUT_MS: u64 = 5_000;

#[derive(Serialize)]
struct TelegramSendMessage<'a> {
    chat_id: &'a str,
    text: &'a str,
    disable_web_page_preview: bool,
}

#[derive(Deserialize)]
struct TelegramResponse {
    ok: bool,
}

/// One sendMessage attempt; `true` only on Telegram's own `ok`.
pub async fn send_message(bot_token: &str, chat_id: &str, text: &str) -> bool {
    use futures_util::future::{Either, select};

    let request = async {
        let body = serde_json::to_string(&TelegramSendMessage {
            chat_id,
            text,
            disable_web_page_preview: true,
        })
        .ok()?;
        let headers = worker::Headers::new();
        headers.set("content-type", "application/json").ok()?;
        let mut init = worker::RequestInit::new();
        init.with_method(worker::Method::Post)
            .with_headers(headers)
            .with_body(Some(worker::wasm_bindgen::JsValue::from_str(&body)));
        let request = worker::Request::new_with_init(
            &format!("https://api.telegram.org/bot{bot_token}/sendMessage"),
            &init,
        )
        .ok()?;
        let mut response = worker::Fetch::Request(request).send().await.ok()?;
        if !(200..300).contains(&response.status_code()) {
            return None;
        }
        response
            .json::<TelegramResponse>()
            .await
            .ok()
            .filter(|reply| reply.ok)
    };
    let deadline = Delay::from(std::time::Duration::from_millis(TELEGRAM_TIMEOUT_MS));
    match select(std::pin::pin!(request), deadline).await {
        Either::Left((result, _)) => result.is_some(),
        Either::Right(((), _)) => false,
    }
}
