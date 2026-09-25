//! HTTP-level test against a tiny local fake Ollama server (no real network,
//! no real Ollama): verifies `OllamaClient` actually speaks the wire protocol
//! it constructs -- request shape and response parsing -- rather than only its
//! internal serialization, which the rest of `ollama_client.rs`'s unit tests
//! already cover without a socket.
use super::*;
use std::{
    io::{Read as _, Write as _},
    net::TcpListener,
    thread,
};

/// Serves exactly `bodies.len()` requests on 127.0.0.1 in order, each with a
/// canned 200 response, then stops. Real Ollama would reject a fabricated
/// response as easily as this fake serves one; this test only exercises this
/// codebase's client -- request construction and response parsing over an
/// actual socket -- not Ollama's own server behavior.
fn spawn_fake_ollama(bodies: Vec<String>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback listener");
    let addr = listener.local_addr().unwrap();
    thread::spawn(move || {
        for body in bodies {
            let (mut stream, _) = listener.accept().expect("accept loopback connection");
            let mut buffer = [0u8; 8192];
            // A full read isn't required for this fixed script of fake
            // requests: ureq sends the whole request before waiting for a
            // response, and this stream is closed right after replying.
            let _ = stream.read(&mut buffer);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });
    format!("http://{addr}")
}

#[test]
fn list_models_parses_a_real_http_response() {
    let url = spawn_fake_ollama(vec![
        r#"{"models":[{"name":"qwen3:8b"},{"name":"qwen2.5:7b-instruct"}]}"#.into(),
    ]);
    let client = OllamaClient::new(&url).unwrap();
    let models = client.list_models().unwrap();
    assert_eq!(models, vec!["qwen3:8b".to_string(), "qwen2.5:7b-instruct".to_string()]);
}

#[test]
fn chat_round_trips_a_structured_response_over_the_wire() {
    let chat_content = serde_json::json!({
        "title": "Weekly sync",
        "summary": "The team discussed the roadmap.",
        "key_points": ["Roadmap reviewed"],
        "decisions": [],
        "action_items": [],
        "open_questions": [],
        "language": "en"
    })
    .to_string();
    let chat_response = serde_json::json!({"message": {"content": chat_content}}).to_string();
    let url = spawn_fake_ollama(vec![chat_response]);
    let client = OllamaClient::new(&url).unwrap();
    let raw = client
        .chat("qwen3:8b", "system prompt", "transcript text", summary_schema(), 4096)
        .unwrap();
    let parsed: crate::summary_types::SummaryOutput = serde_json::from_str(&raw).unwrap();
    assert_eq!(parsed.title, "Weekly sync");
    assert_eq!(parsed.key_points, vec!["Roadmap reviewed".to_string()]);
}

#[test]
fn model_not_found_returns_a_pull_hint() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback listener");
    let addr = listener.local_addr().unwrap();
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept loopback connection");
        let mut buffer = [0u8; 8192];
        let _ = stream.read(&mut buffer);
        let body = r#"{"error":"model not found"}"#;
        let response = format!(
            "HTTP/1.1 404 Not Found\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = stream.write_all(response.as_bytes());
        let _ = stream.flush();
    });
    let client = OllamaClient::new(&format!("http://{addr}")).unwrap();
    let error = client
        .chat("qwen3:8b", "system", "user", summary_schema(), 4096)
        .unwrap_err();
    assert!(error.contains("ollama pull qwen3:8b"), "{error}");
}
