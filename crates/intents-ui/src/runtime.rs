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

    /// Waits `ms` milliseconds without blocking the page.
    pub async fn sleep_ms(ms: u32) {
        let promise = js_sys::Promise::new(&mut |resolve, _| {
            let _ = window().set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, ms as i32);
        });
        let _ = JsFuture::from(promise).await;
    }

    /// A block time in the viewer's locale and time zone.
    pub fn format_time(ms: u64) -> String {
        let date = js_sys::Date::new(&JsValue::from_f64(ms as f64));
        let options = Object::new();
        for (key, value) in [("dateStyle", "medium"), ("timeStyle", "short")] {
            let _ = Reflect::set(&options, &key.into(), &value.into());
        }
        date.to_locale_string("default", &options).as_string().unwrap_or_default()
    }

    /// Home's clipboard bridge (`home:clipboard-*`, schema v1). Home sends `ready` with the
    /// frame's generation; a write request names a purpose Home's policy allows for this app.
    mod clipboard {
        use std::cell::RefCell;
        use std::collections::HashMap;

        use js_sys::{Object, Reflect};
        use wasm_bindgen::{closure::Closure, JsCast, JsValue};

        type Done = Box<dyn FnOnce(Result<(), String>)>;

        #[derive(Default)]
        struct State {
            parent_origin: Option<String>,
            generation: Option<String>,
            pending: HashMap<String, Done>,
            listening: bool,
        }

        thread_local! { static STATE: RefCell<State> = RefCell::default(); }

        fn get(v: &JsValue, key: &str) -> Option<String> {
            Reflect::get(v, &key.into()).ok()?.as_string()
        }

        /// Starts listening for Home's clipboard messages; call once, early.
        pub fn listen() {
            if STATE.with(|s| std::mem::replace(&mut s.borrow_mut().listening, true)) {
                return;
            }
            let on_message = Closure::<dyn FnMut(web_sys::MessageEvent)>::new(|event: web_sys::MessageEvent| {
                let data = event.data();
                match get(&data, "type").as_deref() {
                    Some("home:clipboard-ready") => STATE.with(|s| {
                        let mut s = s.borrow_mut();
                        s.parent_origin = get(&data, "parentOrigin");
                        s.generation = get(&data, "generation");
                    }),
                    Some("home:clipboard-result") => {
                        let Some(id) = get(&data, "requestId") else { return };
                        let ok = Reflect::get(&data, &"ok".into()).ok().and_then(|v| v.as_bool()) == Some(true);
                        let done = STATE.with(|s| s.borrow_mut().pending.remove(&id));
                        if let Some(done) = done {
                            done(if ok { Ok(()) } else { Err(get(&data, "error").unwrap_or_else(|| "denied".into())) });
                        }
                    }
                    _ => {}
                }
            });
            let _ = super::window().add_event_listener_with_callback("message", on_message.as_ref().unchecked_ref());
            on_message.forget();
        }

        pub fn write(token: &str, purpose: &str, text: &str, done: Done) {
            let (origin, generation) = STATE.with(|s| {
                let s = s.borrow();
                (s.parent_origin.clone(), s.generation.clone())
            });
            let (Some(origin), Some(generation)) = (origin, generation) else {
                done(Err("unavailable".into()));
                return;
            };
            let Ok(Some(parent)) = super::window().parent() else {
                done(Err("unavailable".into()));
                return;
            };
            let id = super::window().crypto().map(|c| c.random_uuid()).unwrap_or_default();
            let message = Object::new();
            for (key, value) in [
                ("type", "home:clipboard-request"),
                ("schema", "elastos.home.clipboard.request/v1"),
                ("requestId", id.as_str()),
                ("homeToken", token),
                ("parentOrigin", origin.as_str()),
                ("generation", generation.as_str()),
                ("operation", "write"),
                ("purpose", purpose),
                ("mime_type", "text/plain"),
                ("text", text),
            ] {
                let _ = Reflect::set(&message, &key.into(), &value.into());
            }
            STATE.with(|s| s.borrow_mut().pending.insert(id, done));
            let _ = parent.post_message(&message, "*");
        }
    }

    pub use clipboard::{listen as listen_clipboard, write as copy_text};
}

#[cfg(target_arch = "wasm32")]
pub use web::{announce, copy_text, format_time, home_token, listen_clipboard, now_secs, provider, query, sleep_ms};

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
pub fn listen_clipboard() {}

/// Natively there is no provider to wait for.
#[cfg(not(target_arch = "wasm32"))]
pub async fn sleep_ms(_ms: u32) {}

#[cfg(not(target_arch = "wasm32"))]
pub fn copy_text(_token: &str, _purpose: &str, _text: &str, done: Box<dyn FnOnce(Result<(), String>)>) {
    done(Err("unavailable".into()));
}

/// A block time as UTC (the native development window has no locale to follow).
#[cfg(not(target_arch = "wasm32"))]
pub fn format_time(ms: u64) -> String {
    let secs = ms / 1000;
    let (days, rest) = (secs / 86_400, secs % 86_400);
    // Days since 1970-01-01 to a civil date (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year}-{month:02}-{day:02} {:02}:{:02} UTC", rest / 3600, rest % 3600 / 60)
}

#[cfg(not(target_arch = "wasm32"))]
pub fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

#[cfg(not(target_arch = "wasm32"))]
pub async fn provider(_op: &str, _body: &Value, _token: &str) -> Result<Value, ProviderError> {
    Err(ProviderError { code: String::new(), message: "Not in Home.".into() })
}
