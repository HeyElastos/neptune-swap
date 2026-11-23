//! The carrier in process: the solver's `Carrier` against a stand-in for a node's Runtime API
//! (attach, the peer capability, gossip on one topic), and the app's side as `swap_ui` speaks
//! it (`gossip_send {topic, message}`, `gossip_recv` read by `content`, base64url of the sealed
//! bytes; its own fetch runs only inside Home). Each side opens only what was sealed to it.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use ed25519_dalek::SigningKey;
use intents_protocol::{open, seal, DidKey, DEALS_TOPIC_TEST};
use serde_json::{json, Value};
use solver::carrier::Carrier;

/// Each topic's messages (sender id, content), and how far each reader has read.
#[derive(Default)]
struct Gossip {
    topics: HashMap<String, Vec<(String, String)>>,
    read: HashMap<String, usize>,
}

fn answer(g: &mut Gossip, path: &str, headers: &str, body: &Value) -> Value {
    match path {
        "/api/auth/attach" => return json!({ "token": "session" }),
        "/api/capability/request" => return json!({ "status": "granted", "token": "cap", "request_id": "1" }),
        _ => {}
    }
    // The solver's calls carry its session and capability; the app's come through Home.
    let solver = headers.contains("authorization: bearer session") && headers.contains("x-capability-token: cap");
    assert!(solver || headers.contains("x-elastos-home-token"), "a peer call without its grant");
    let messages = g.topics.entry(body["topic"].as_str().unwrap().into()).or_default();
    let data = match path.trim_start_matches("/api/provider/peer/") {
        "gossip_join" => json!({}),
        "gossip_send" => {
            messages.push((body["sender_id"].as_str().unwrap_or("app").into(), body["message"].as_str().unwrap().into()));
            json!({})
        }
        "gossip_recv" => {
            let reader = body["consumer_id"].as_str().unwrap_or("app");
            let from = g.read.insert(reader.into(), messages.len()).unwrap_or(0);
            let new: Vec<Value> = messages[from..].iter().filter(|(s, _)| Some(s.as_str()) != body["skip_sender_id"].as_str()).map(|(_, c)| json!({ "content": c })).collect();
            json!({ "messages": new })
        }
        other => panic!("unexpected op {other}"),
    };
    json!({ "status": "ok", "data": data })
}

/// Serves the stand-in on a local port; returns its URL.
fn runtime() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let gossip = Arc::new(Mutex::new(Gossip::default()));
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let (mut head, mut line) = (String::new(), String::new());
            while reader.read_line(&mut line).unwrap() > 2 {
                head.push_str(&line.to_ascii_lowercase());
                line.clear();
            }
            let length = head.lines().find_map(|l| l.strip_prefix("content-length: ")).map_or(0, |n| n.trim().parse().unwrap());
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            let path = head.split_whitespace().nth(1).unwrap().to_string();
            let reply = answer(&mut gossip.lock().unwrap(), &path, &head, &serde_json::from_slice(&body).unwrap_or(Value::Null)).to_string();
            write!(stream, "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}", reply.len()).unwrap();
        }
    });
    url
}

/// The app's `runtime::peer`, as Home's gateway receives it.
fn app_peer(url: &str, op: &str, body: Value) -> Value {
    let reply: Value = serde_json::from_str(
        &ureq::post(&format!("{url}/api/provider/peer/{op}")).set("x-elastos-home-token", "home").send_string(&body.to_string()).unwrap().into_string().unwrap(),
    )
    .unwrap();
    reply["data"].clone()
}

#[test]
fn sealed_messages_cross_the_carrier_both_ways() {
    let url = runtime();
    let coords = std::env::temp_dir().join(format!("carrier-coords-{}.json", std::process::id()));
    std::fs::write(&coords, json!({ "api_url": url, "attach_secret": "s" }).to_string()).unwrap();
    let carrier = Carrier::connect(&coords, "neptune-swap", "solver-1", "test", Duration::from_secs(1)).unwrap();
    std::fs::remove_file(&coords).unwrap();
    let (solver_key, app_key) = (SigningKey::from_bytes(&[1; 32]), SigningKey::from_bytes(&[2; 32]));
    let topic = DEALS_TOPIC_TEST;
    carrier.join(topic).unwrap();

    // The app to the solver: what `swap_ui::deliver` sends.
    let to_solver = seal(&DidKey::new(solver_key.verifying_key()), b"intent").unwrap();
    app_peer(&url, "gossip_send", json!({ "topic": topic, "message": URL_SAFE_NO_PAD.encode(&to_solver) }));
    let got = carrier.recv(topic).unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(open(&solver_key, &got[0]).unwrap(), b"intent");
    assert!(open(&app_key, &got[0]).is_err(), "sealed to the solver alone");

    // The solver to the app: what the daemon's `send_to` sends, read as `swap_ui::inbox` reads.
    carrier.send(topic, &seal(&DidKey::new(app_key.verifying_key()), b"quote").unwrap()).unwrap();
    assert!(carrier.recv(topic).unwrap().is_empty(), "the solver skips its own and what it read");
    let inbox = app_peer(&url, "gossip_recv", json!({ "topic": topic, "limit": 50 }));
    let opened: Vec<Vec<u8>> = inbox["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| URL_SAFE_NO_PAD.decode(m["content"].as_str()?).ok())
        .filter_map(|sealed| open(&app_key, &sealed).ok())
        .collect();
    assert_eq!(opened, [b"quote".to_vec()], "the app opens the quote, not its own intent");
}
