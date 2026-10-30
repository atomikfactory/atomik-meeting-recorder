//! HTTP-level test against a tiny local fake Ollama server (no real network,
//! no real Ollama): verifies `OllamaClient` actually speaks the wire protocol
//! it constructs -- request shape and response parsing -- rather than only its
//! internal serialization, which the rest of `ollama_client.rs`'s unit tests
//! already cover without a socket.
use super::*;
use std::{
    io::{Read as _, Write as _},
    net::{TcpListener, TcpStream},
    thread,
};

/// Reads a full HTTP/1.1 request (headers, then `Content-Length` bytes of
/// body, if any) off `stream`. A POST's body can arrive in a separate TCP
/// segment from its headers; responding and closing the socket before every
/// byte the client wrote has been read from the kernel's receive buffer can
/// make the OS send RST instead of a clean close, which the client-side HTTP
/// library sees as a transport error indistinguishable from "server down" --
/// so this fake must fully drain the request before it replies.
fn read_full_request(stream: &mut TcpStream) {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let read = stream.read(&mut chunk).unwrap_or(0);
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
        let Some(header_end) = find_header_end(&buffer) else {
            continue;
        };
        let content_length = content_length_of(&buffer[..header_end]);
        if buffer.len() >= header_end + 4 + content_length {
            break;
        }
    }
}
fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}
fn content_length_of(headers: &[u8]) -> usize {
    String::from_utf8_lossy(headers)
        .lines()
        .find_map(|line| {
            line.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .map(|v| v.trim().to_string())
        })
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}
fn respond(stream: &mut TcpStream, status: &str, body: &str) {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

/// Serves exactly `bodies.len()` requests on 127.0.0.1 in order, each fully
/// drained then answered with a canned 200 response, then stops. Real Ollama
/// would reject a fabricated response as easily as this fake serves one; this
/// test only exercises this codebase's client -- request construction and
/// response parsing over an actual socket -- not Ollama's own server behavior.
fn spawn_fake_ollama(bodies: Vec<String>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback listener");
    let addr = listener.local_addr().unwrap();
    thread::spawn(move || {
        for body in bodies {
            let (mut stream, _) = listener.accept().expect("accept loopback connection");
            read_full_request(&mut stream);
            respond(&mut stream, "200 OK", &body);
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
    assert_eq!(
        models,
        vec!["qwen3:8b".to_string(), "qwen2.5:7b-instruct".to_string()]
    );
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
        .chat(
            "qwen3:8b",
            "system prompt",
            "transcript text",
            summary_schema(),
            4096,
        )
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
        read_full_request(&mut stream);
        respond(
            &mut stream,
            "404 Not Found",
            r#"{"error":"model not found"}"#,
        );
    });
    let client = OllamaClient::new(&format!("http://{addr}")).unwrap();
    let error = client
        .chat("qwen3:8b", "system", "user", summary_schema(), 4096)
        .unwrap_err();
    assert!(error.contains("ollama pull qwen3:8b"), "{error}");
}
