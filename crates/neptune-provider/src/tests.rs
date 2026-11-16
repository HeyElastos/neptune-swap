use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::thread;

use serde_json::{json, Value};

use super::Provider;

fn call(provider: &mut Provider, request: Value) -> Value {
    let (line, _) = provider.handle_line(&request.to_string());
    serde_json::from_str(&line).unwrap()
}

fn configured(rpc: &str) -> Provider {
    let mut provider = Provider::default();
    let reply = call(
        &mut provider,
        json!({ "op": "init", "config": { "networks": [{ "id": "neptune-testnet", "rpc": rpc }] } }),
    );
    assert_eq!(reply, json!({ "status": "ok" }));
    provider
}

/// A one-shot JSON-RPC node on loopback that answers `chain_height`.
fn fake_node(reply: Value) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream);
        let mut length = 0;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                length = value.trim().parse().unwrap();
            }
            if line == "\r\n" {
                break;
            }
        }
        let mut body = vec![0; length];
        reader.read_exact(&mut body).unwrap();
        let request: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(request["method"], "chain_height");
        let body = reply.to_string();
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
            body.len()
        );
        reader.into_inner().write_all(response.as_bytes()).unwrap();
    });
    format!("http://{addr}")
}

#[test]
fn networks_lists_ids_without_node_addresses() {
    let mut provider = configured("http://127.0.0.1:9797");
    let reply = call(&mut provider, json!({ "op": "networks" }));
    assert_eq!(reply, json!({ "status": "ok", "data": { "networks": [{ "id": "neptune-testnet" }] } }));
}

#[test]
fn only_loopback_ip_literals_are_accepted_as_node_addresses() {
    for rpc in ["http://localhost:9797", "http://node.example.com:9797", "http://10.0.0.5:9797", "https://127.0.0.1:9797"] {
        let mut provider = Provider::default();
        let reply = call(
            &mut provider,
            json!({ "op": "init", "config": { "networks": [{ "id": "neptune-main", "rpc": rpc }] } }),
        );
        assert_eq!(reply["code"], "invalid_config", "{rpc} was accepted");
    }
    configured("http://[::1]:9797/");
}

#[test]
fn status_reads_the_height_from_the_local_node() {
    let rpc = fake_node(json!({ "jsonrpc": "2.0", "id": 1, "result": { "height": 21_337 } }));
    let mut provider = configured(&rpc);
    let reply = call(&mut provider, json!({ "op": "status", "network": "neptune-testnet" }));
    assert_eq!(reply, json!({ "status": "ok", "data": { "network": "neptune-testnet", "height": 21_337 } }));
}

#[test]
fn status_reports_a_node_error_and_an_unreachable_node() {
    let rpc = fake_node(json!({ "jsonrpc": "2.0", "id": 1, "error": { "code": -32601, "message": "no" } }));
    let mut provider = configured(&rpc);
    let reply = call(&mut provider, json!({ "op": "status", "network": "neptune-testnet" }));
    assert_eq!(reply["code"], "node_error");

    let closed = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
    let mut provider = configured(&format!("http://{closed}"));
    let reply = call(&mut provider, json!({ "op": "status", "network": "neptune-testnet" }));
    assert_eq!(reply["code"], "node_unreachable");
    let port = closed.port().to_string();
    assert!(!reply.to_string().contains(&port), "reply leaks the node address: {reply}");
}

#[test]
fn unknown_networks_ops_and_fields_are_refused() {
    let mut provider = configured("http://127.0.0.1:9797");
    assert_eq!(call(&mut provider, json!({ "op": "status", "network": "neptune-main" }))["code"], "unknown_network");
    assert_eq!(call(&mut provider, json!({ "op": "broadcast" }))["code"], "invalid_request");
    assert_eq!(call(&mut provider, json!({ "op": "networks", "rpc": "x" }))["code"], "invalid_request");
}

#[test]
fn the_runtime_invocation_envelope_is_accepted_and_shutdown_stops() {
    let mut provider = configured("http://127.0.0.1:9797");
    let reply = call(&mut provider, json!({ "op": "networks", "_runtime_invocation": { "any": "thing" } }));
    assert_eq!(reply["status"], "ok");
    let (line, shutdown) = provider.handle_line(r#"{"op":"shutdown"}"#);
    assert_eq!((line.as_str(), shutdown), (r#"{"status":"ok"}"#, true));
}

#[test]
fn the_runtime_bridge_init_keeps_the_startup_networks() {
    let mut provider = configured("http://127.0.0.1:9799");
    let generic = json!({ "op": "init", "config": {
        "base_path": "", "allowed_paths": [], "read_only": false, "encryption_key": ""
    } });
    assert_eq!(call(&mut provider, generic)["status"], "ok");
    let networks = call(&mut provider, json!({ "op": "networks" }));
    assert_eq!(networks["data"]["networks"][0]["id"], "neptune-testnet");
}

#[test]
fn remote_nodes_need_the_development_flag() {
    let mut provider = Provider::default();
    let remote = json!({ "networks": [{ "id": "neptune-main", "rpc": "https://node.example.org" }] });
    let refused = call(&mut provider, json!({ "op": "init", "config": remote }));
    assert_eq!(refused["status"], "error");
    let mut allowed = remote.clone();
    allowed["allow_remote_nodes"] = json!(true);
    assert_eq!(call(&mut provider, json!({ "op": "init", "config": allowed }))["status"], "ok");
    // Still no plain http to a remote host, and no paths or odd characters.
    for rpc in ["http://node.example.org", "https://node.example.org/x?y", "https://", "ftp://h"] {
        let config = json!({ "allow_remote_nodes": true, "networks": [{ "id": "neptune-main", "rpc": rpc }] });
        assert_eq!(call(&mut provider, json!({ "op": "init", "config": config }))["status"], "error", "{rpc}");
    }
}
