//! The elastOS Runtime as the capsule sees it: the Home launch token, `POST /api/provider/<op>`
//! calls with it, and the handshake that tells Home the app is ready. Only the web build talks to
//! a Runtime; a native development window is never inside Home.

#[cfg(not(target_arch = "wasm32"))]
use serde_json::Value;

/// A provider error: its code when the provider sent one, and a sentence.
pub struct ProviderError {
    pub code: String,
    pub message: String,
}

#[cfg(target_arch = "wasm32")]
mod web {
    use js_sys::{Object, Reflect};
    use serde_json::Value;
    use wasm_bindgen::{closure::Closure, JsCast, JsValue};
    use wasm_bindgen_futures::JsFuture;
    use web_sys::{Headers, Request, RequestInit, Response};

    use super::ProviderError;

    fn window() -> web_sys::Window {
        web_sys::window().expect("a browser window")
    }

    fn param(fragment: &str, name: &str) -> Option<String> {
        fragment
            .trim_start_matches(['#', '?'])
            .split('&')
            .find_map(|pair| pair.strip_prefix(name)?.strip_prefix('='))
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    }

    /// Home puts the launch token in the URL fragment as `home_token`.
    pub fn home_token() -> Option<String> {
        param(&window().location().hash().ok()?, "home_token")
    }

    /// A query parameter, for a page opened outside Home during development.
    pub fn query(name: &str) -> Option<String> {
        param(&window().location().search().ok()?, name)
    }

    fn failed(message: impl Into<String>) -> ProviderError {
        ProviderError { code: String::new(), message: message.into() }
    }

    pub async fn provider(op: &str, body: &Value, token: &str) -> Result<Value, ProviderError> {
        let headers = Headers::new().map_err(|_| failed("headers"))?;
        headers.set("content-type", "application/json").map_err(|_| failed("headers"))?;
        headers.set("x-elastos-home-token", token).map_err(|_| failed("headers"))?;
        let init = RequestInit::new();
        init.set_method("POST");
        init.set_headers(&headers);
        init.set_body(&JsValue::from_str(&body.to_string()));
        let request = Request::new_with_str_and_init(&format!("/api/provider/neptune/{op}"), &init)
            .map_err(|_| failed("The request could not be made."))?;
        let response: Response = JsFuture::from(window().fetch_with_request(&request))
            .await
            .map_err(|_| failed("The Runtime did not answer."))?
            .unchecked_into();
        if response.status() == 401 || response.status() == 403 {
            return Err(failed("This Home has not given the app Neptune access."));
        }
        let text = JsFuture::from(response.text().map_err(|_| failed("unreadable reply"))?)
            .await
            .ok()
            .and_then(|t| t.as_string())
            .unwrap_or_default();
        let reply: Value = serde_json::from_str(&text)
            .map_err(|_| failed(format!("The Runtime answered {}.", response.status())))?;
        if reply["status"] == "ok" {
            return Ok(reply.get("data").cloned().unwrap_or(Value::Null));
        }
        Err(ProviderError {
            code: reply["code"].as_str().unwrap_or_default().to_string(),
            message: reply["message"].as_str().unwrap_or("Request failed.").to_string(),
        })
    }

    fn post_to_home(kind: &str, token: &str, nonce: &str) {
        let Ok(Some(parent)) = window().parent() else { return };
        let message = Object::new();
        let _ = Reflect::set(&message, &"type".into(), &kind.into());
        let _ = Reflect::set(&message, &"homeToken".into(), &token.into());
        let _ = Reflect::set(&message, &"documentNonce".into(), &nonce.into());
        let _ = parent.post_message(&message, "*");
    }

    /// Tells Home the app is ready, and that it is going when the page hides.
    pub fn announce(token: &str) {
        let Ok(crypto) = window().crypto() else { return };
        let nonce = crypto.random_uuid();
        let (token_out, nonce_out) = (token.to_string(), nonce.clone());
        let unloading = Closure::<dyn FnMut()>::new(move || post_to_home("home:app-unloading", &token_out, &nonce_out));
        let _ = window().add_event_listener_with_callback("pagehide", unloading.as_ref().unchecked_ref());
        unloading.forget();
        post_to_home("home:app-ready", token, &nonce);
    }

    pub fn now_secs() -> u64 {
        (js_sys::Date::now() / 1000.0) as u64
    }
}

#[cfg(target_arch = "wasm32")]
pub use web::{announce, home_token, now_secs, provider, query};

#[cfg(not(target_arch = "wasm32"))]
pub fn home_token() -> Option<String> {
    None
}

#[cfg(not(target_arch = "wasm32"))]
pub fn query(name: &str) -> Option<String> {
    std::env::var(format!("NEPTUNE_INTENTS_{}", name.to_uppercase())).ok()
}

#[cfg(not(target_arch = "wasm32"))]
pub fn announce(_token: &str) {}

#[cfg(not(target_arch = "wasm32"))]
pub fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

#[cfg(not(target_arch = "wasm32"))]
pub async fn provider(_op: &str, _body: &Value, _token: &str) -> Result<Value, ProviderError> {
    Err(ProviderError { code: String::new(), message: "Not in Home.".into() })
}
