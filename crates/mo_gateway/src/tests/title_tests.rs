//! Unit tests for the `title` module — production code lives in
//! `mo_gateway/src/title.rs`. Wired from there with `#[cfg(test)] #[path = "tests/title_tests.rs"] mod tests;` so the tests keep `use super::*` access
//! to the module's items (private ones included).

use super::*;
use axum::{Router, routing::post};
use serde_json::{Value, json};

fn sse_payload(deltas: &[Value]) -> String {
    let mut out = String::new();
    for delta in deltas {
        out.push_str(&format!(
            "data: {}\n\n",
            json!({ "choices": [{ "delta": delta }] })
        ));
    }
    out.push_str("data: [DONE]\n\n");
    out
}

/// A tiny mock LLM: responses are keyed on the first user message so
/// parallel tests stay deterministic. Each request also asserts it
/// carried the title system prompt and no hard `max_tokens` cap (the
/// length restriction lives in the prompt; a cap is what broke title
/// generation for reasoning models).
///
/// * message contains "tool"      -> assistant tool call (unusable title)
/// * message contains "empty"     -> empty assistant content
/// * message contains "reasoning" -> reasoning_content, then content
/// * otherwise                    -> plain text title
async fn mock_llm() -> String {
    let router = Router::new().route(
        "/chat/completions",
        post(|body: axum::extract::Json<Value>| async move {
            let system = body["messages"]
                .as_array()
                .and_then(|msgs| msgs.iter().find(|m| m["role"] == "system"))
                .and_then(|m| m["content"].as_str())
                .unwrap_or("");
            assert!(
                system.contains("short title"),
                "title request must carry the title system prompt: {system}"
            );
            assert!(
                body.get("max_tokens").is_none(),
                "title request must not hard-cap output tokens: {body:?}"
            );
            let user = body["messages"]
                .as_array()
                .and_then(|msgs| msgs.iter().find(|m| m["role"] == "user"))
                .and_then(|m| m["content"].as_str())
                .unwrap_or("");
            let body = if user.contains("tool") {
                sse_payload(&[
                    json!({ "role": "assistant" }),
                    json!({
                        "tool_calls": [{
                            "index": 0,
                            "id": "call_t",
                            "type": "function",
                            "function": {
                                "name": "read_file",
                                "arguments": r#"{"path":"x"}"#,
                            }
                        }]
                    }),
                ])
            } else if user.contains("empty") {
                sse_payload(&[json!({ "role": "assistant" })])
            } else if user.contains("reasoning") {
                // A reasoning model streams `reasoning_content` first;
                // the title only appears in `content` afterwards.
                sse_payload(&[
                    json!({ "role": "assistant" }),
                    json!({ "reasoning_content": "The user wants " }),
                    json!({ "reasoning_content": "a title about notes." }),
                    json!({ "content": "Summarize notes.txt" }),
                ])
            } else {
                sse_payload(&[
                    json!({ "role": "assistant" }),
                    json!({ "content": "Explore notes.txt" }),
                ])
            };
            (
                [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                body,
            )
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn placeholder_title_has_new_session_prefix() {
    let title = placeholder_title();
    assert!(title.starts_with("New session - "), "got: {title}");
    assert!(title.len() > "New session - ".len());
}

#[tokio::test]
async fn no_model_config_skips_generation() {
    let result = generate_title("hello", "", "", None).await.unwrap();
    assert_eq!(result, None);
}

#[tokio::test]
async fn generates_title_from_first_message() {
    let base_url = mock_llm().await;
    let title = generate_title(
        "Read notes.txt and summarize it",
        &base_url,
        "mock-model",
        None,
    )
    .await
    .unwrap();
    assert_eq!(title.as_deref(), Some("Explore notes.txt"));
}

#[tokio::test]
async fn reasoning_model_title_comes_from_content_not_reasoning() {
    let base_url = mock_llm().await;
    // A reasoning model streams `reasoning_content` first; the title
    // must be read from the `content` that follows, never from the
    // reasoning text.
    let title = generate_title("reasoning model please", &base_url, "mock-model", None)
        .await
        .unwrap();
    assert_eq!(title.as_deref(), Some("Summarize notes.txt"));
}

#[tokio::test]
async fn tool_call_or_empty_response_keeps_placeholder() {
    let base_url = mock_llm().await;
    // The model answers with a tool call -> no usable title.
    let title = generate_title("do the tool thing", &base_url, "mock-model", None)
        .await
        .unwrap();
    assert_eq!(title, None);
    // Empty content -> no usable title.
    let title = generate_title("empty answer please", &base_url, "mock-model", None)
        .await
        .unwrap();
    assert_eq!(title, None);
}
