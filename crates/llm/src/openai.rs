use anyhow::Result;

/// Wire format selector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiFormat {
    /// POST {base_url}/chat/completions (OpenAI Chat Completions API)
    OpenAiChat,
    /// POST {base_url}/responses (OpenAI Responses API)
    OpenAiResponses,
}

/// Per-request thinking/reasoning toggle. Mapped to wire-format fields:
/// `chat_template_kwargs.enable_thinking` for openai-chat,
/// `reasoning.effort` for openai-responses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Thinking {
    /// Send no thinking-related field; let the server decide.
    Auto,
    /// Force thinking on.
    On,
    /// Force thinking off.
    Off,
}

/// Merge user-supplied `extra_body` fields into the request body. termcmp's
/// own keys (model, messages, max_tokens, …) are written AFTER the merge so
/// they always win on collision — a stray `model` in extra_body can't
/// redirect the request. A non-object `extra_body` is ignored.
fn merge_extra_body(body: &mut serde_json::Value, extra: Option<&serde_json::Value>) {
    let (Some(obj), Some(serde_json::Value::Object(extra))) = (body.as_object_mut(), extra) else {
        return;
    };
    for (k, v) in extra {
        obj.entry(k.clone()).or_insert_with(|| v.clone());
    }
}

/// POST {base_url}/chat/completions. Returns completion strings (multi-line responses are split by the caller).
#[allow(clippy::too_many_arguments)]
pub async fn chat_completion(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    model: &str,
    system: &str,
    user: &str,
    max_tokens: u32,
    thinking_budget: Option<u32>,
    thinking: Thinking,
    extra_body: Option<&serde_json::Value>,
) -> Result<Vec<String>> {
    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
    let mut body = serde_json::json!({
        "model": model,
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": user}
        ],
        "max_tokens": max_tokens,
        "temperature": 0.1
    });
    if let Some(tb) = thinking_budget {
        body["thinking_budget"] = serde_json::json!(tb);
    }
    match thinking {
        Thinking::On => {
            body["chat_template_kwargs"] = serde_json::json!({"enable_thinking": true});
        }
        Thinking::Off => {
            body["chat_template_kwargs"] = serde_json::json!({"enable_thinking": false});
        }
        Thinking::Auto => {}
    }
    merge_extra_body(&mut body, extra_body);
    tracing::debug!(
        "llm: POST {url} model={model} max_tokens={max_tokens} system={}B user={}B",
        system.len(),
        user.len()
    );
    tracing::trace!("llm: system prompt: {system}");
    tracing::trace!("llm: user prompt: {user}");
    let mut req = client.post(&url).json(&body);
    // Local servers (llama.cpp, ollama, vLLM) often run without auth; an
    // empty key means "send no Authorization header", not "Bearer <empty>".
    if !api_key.is_empty() {
        req = req.header("Authorization", format!("Bearer {api_key}"));
    }
    let resp = req.send().await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        tracing::warn!("llm: chat/completions returned {status}: {text}");
        return Ok(vec![]);
    }
    let json: serde_json::Value = resp.json().await?;
    let texts: Vec<String> = json
        .get("choices")
        .and_then(|c| c.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|c| {
                    c.get("message")
                        .and_then(|m| m.get("content"))
                        .and_then(|c| c.as_str())
                })
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();
    tracing::debug!("llm: chat/completions -> {} text(s)", texts.len());
    tracing::trace!("llm: chat/completions texts: {texts:?}");
    Ok(texts)
}

/// POST {base_url}/responses. Returns completion strings (one per output_text block).
#[allow(clippy::too_many_arguments)]
pub async fn responses_completion(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    model: &str,
    instructions: &str,
    input: &str,
    max_tokens: u32,
    thinking: Thinking,
    extra_body: Option<&serde_json::Value>,
) -> Result<Vec<String>> {
    let url = format!("{}/responses", base_url.trim_end_matches('/'));
    let mut body = serde_json::json!({
        "model": model,
        "instructions": instructions,
        "input": [{"role": "user", "content": input}],
        "max_output_tokens": max_tokens
    });
    match thinking {
        Thinking::On => {
            body["reasoning"] = serde_json::json!({"effort": "high"});
        }
        Thinking::Off => {
            body["reasoning"] = serde_json::json!({"effort": "minimal"});
        }
        Thinking::Auto => {}
    }
    merge_extra_body(&mut body, extra_body);
    tracing::debug!(
        "llm: POST {url} model={model} max_output_tokens={max_tokens} instructions={}B input={}B",
        instructions.len(),
        input.len()
    );
    tracing::trace!("llm: instructions: {instructions}");
    tracing::trace!("llm: input: {input}");
    let mut req = client.post(&url).json(&body);
    if !api_key.is_empty() {
        req = req.header("Authorization", format!("Bearer {api_key}"));
    }
    let resp = req.send().await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        tracing::warn!("llm: responses returned {status}: {text}");
        return Ok(vec![]);
    }
    let json: serde_json::Value = resp.json().await?;
    let texts: Vec<String> = json
        .get("output")
        .and_then(|o| o.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|item| {
                    item.get("content")
                        .and_then(|c| c.as_array())
                        .and_then(|blocks| {
                            blocks
                                .iter()
                                .find(|b| {
                                    b.get("type").and_then(|t| t.as_str()) == Some("output_text")
                                })
                                .and_then(|b| b.get("text").and_then(|t| t.as_str()))
                        })
                })
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();
    tracing::debug!("llm: responses -> {} text(s)", texts.len());
    tracing::trace!("llm: responses texts: {texts:?}");
    Ok(texts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// Chat-completions response body used by the default capture server.
    const CHAT_BODY: &str = r#"{"choices":[{"message":{"content":"ls -la"}}]}"#;
    /// Responses-API response body used by responses_completion tests.
    const RESPONSES_BODY: &str =
        r#"{"output":[{"content":[{"type":"output_text","text":"git status"}]}]}"#;

    /// Spin up a one-shot loopback HTTP server that captures the raw request
    /// head and returns a canned JSON body with the given status. Returns the
    /// bound port and a handle that yields the captured request bytes.
    async fn capture_server_with(
        status: u16,
        body: &str,
    ) -> (u16, tokio::task::JoinHandle<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        // The body reference can't cross the spawn boundary; own a copy.
        let body = body.to_string();
        let handle = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let mut captured = Vec::new();
            // Read until we've seen the full header block (blank line).
            loop {
                let n = sock.read(&mut buf).await.unwrap();
                if n == 0 {
                    break;
                }
                captured.extend_from_slice(&buf[..n]);
                if captured.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            let reason = if status == 200 { "OK" } else { "Error" };
            let resp = format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = sock.write_all(resp.as_bytes()).await;
            captured
        });
        (port, handle)
    }

    /// One-shot loopback server returning a chat-completions body (HTTP 200).
    async fn capture_server() -> (u16, tokio::task::JoinHandle<Vec<u8>>) {
        capture_server_with(200, CHAT_BODY).await
    }

    fn head(captured: &[u8]) -> String {
        let end = captured
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .unwrap_or(captured.len());
        String::from_utf8_lossy(&captured[..end]).to_string()
    }

    /// Extract and parse the request body (bytes after the header block).
    fn body_json(captured: &[u8]) -> serde_json::Value {
        let start = captured
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .map(|p| p + 4)
            .unwrap_or(0);
        serde_json::from_slice(&captured[start..]).unwrap()
    }

    #[tokio::test]
    async fn empty_api_key_omits_authorization_header() {
        let (port, handle) = capture_server().await;
        let client = reqwest::Client::new();
        let base = format!("http://127.0.0.1:{port}");
        let texts = chat_completion(
            &client,
            &base,
            "",
            "m",
            "sys",
            "usr",
            256,
            None,
            Thinking::Auto,
            None,
        )
        .await
        .unwrap();
        assert_eq!(texts, vec!["ls -la".to_string()]);
        let head = head(&handle.await.unwrap());
        assert!(
            !head.to_lowercase().contains("authorization"),
            "empty key must not send an Authorization header; got:\n{head}"
        );
    }

    #[tokio::test]
    async fn nonempty_api_key_sends_bearer_header() {
        let (port, handle) = capture_server().await;
        let client = reqwest::Client::new();
        let base = format!("http://127.0.0.1:{port}");
        chat_completion(
            &client,
            &base,
            "sekret",
            "m",
            "sys",
            "usr",
            256,
            None,
            Thinking::Auto,
            None,
        )
        .await
        .unwrap();
        let head = head(&handle.await.unwrap());
        assert!(
            head.to_lowercase().contains("authorization: bearer sekret"),
            "non-empty key must send a Bearer header; got:\n{head}"
        );
    }

    #[tokio::test]
    async fn extra_body_merged_with_termcmp_keys_winning() {
        let (port, handle) = capture_server().await;
        let client = reqwest::Client::new();
        let base = format!("http://127.0.0.1:{port}");
        // enable_thinking is server-specific; model collides with termcmp's own.
        let extra = serde_json::json!({
            "chat_template_kwargs": {"enable_thinking": false},
            "model": "HACKED"
        });
        chat_completion(
            &client,
            &base,
            "",
            "real-model",
            "sys",
            "usr",
            256,
            None,
            Thinking::Auto,
            Some(&extra),
        )
        .await
        .unwrap();
        let body = body_json(&handle.await.unwrap());
        assert_eq!(
            body["chat_template_kwargs"]["enable_thinking"], false,
            "server-specific field must be merged in"
        );
        assert_eq!(
            body["model"], "real-model",
            "termcmp's model must win on collision"
        );
    }

    #[tokio::test]
    async fn thinking_off_sets_enable_thinking_false() {
        let (port, handle) = capture_server().await;
        let client = reqwest::Client::new();
        let base = format!("http://127.0.0.1:{port}");
        chat_completion(
            &client,
            &base,
            "",
            "m",
            "sys",
            "usr",
            256,
            None,
            Thinking::Off,
            None,
        )
        .await
        .unwrap();
        let body = body_json(&handle.await.unwrap());
        assert_eq!(
            body["chat_template_kwargs"]["enable_thinking"], false,
            "Thinking::Off must set enable_thinking=false"
        );
    }
    #[tokio::test]
    async fn responses_happy_path() {
        let (port, handle) = capture_server_with(200, RESPONSES_BODY).await;
        let client = reqwest::Client::new();
        let base = format!("http://127.0.0.1:{port}");
        let texts = responses_completion(
            &client,
            &base,
            "",
            "m",
            "sys",
            "usr",
            256,
            Thinking::Auto,
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            texts,
            vec!["git status".to_string()],
            "output_text blocks must be returned as completion strings"
        );
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn responses_non_success_returns_empty() {
        let (port, handle) = capture_server_with(500, "{}").await;
        let client = reqwest::Client::new();
        let base = format!("http://127.0.0.1:{port}");
        let texts = responses_completion(
            &client,
            &base,
            "",
            "m",
            "sys",
            "usr",
            256,
            Thinking::Auto,
            None,
        )
        .await
        .unwrap();
        assert!(
            texts.is_empty(),
            "non-success responses must yield no texts; got {texts:?}"
        );
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn responses_thinking_on_sets_effort_high() {
        let (port, handle) = capture_server_with(200, RESPONSES_BODY).await;
        let client = reqwest::Client::new();
        let base = format!("http://127.0.0.1:{port}");
        responses_completion(
            &client,
            &base,
            "",
            "m",
            "sys",
            "usr",
            256,
            Thinking::On,
            None,
        )
        .await
        .unwrap();
        let body = body_json(&handle.await.unwrap());
        assert_eq!(
            body["reasoning"]["effort"], "high",
            "Thinking::On must set reasoning.effort=high"
        );
    }

    #[tokio::test]
    async fn responses_thinking_off_sets_effort_minimal() {
        let (port, handle) = capture_server_with(200, RESPONSES_BODY).await;
        let client = reqwest::Client::new();
        let base = format!("http://127.0.0.1:{port}");
        responses_completion(
            &client,
            &base,
            "",
            "m",
            "sys",
            "usr",
            256,
            Thinking::Off,
            None,
        )
        .await
        .unwrap();
        let body = body_json(&handle.await.unwrap());
        assert_eq!(
            body["reasoning"]["effort"], "minimal",
            "Thinking::Off must set reasoning.effort=minimal"
        );
    }

    #[tokio::test]
    async fn responses_empty_key_omits_auth() {
        let (port, handle) = capture_server_with(200, RESPONSES_BODY).await;
        let client = reqwest::Client::new();
        let base = format!("http://127.0.0.1:{port}");
        responses_completion(
            &client,
            &base,
            "",
            "m",
            "sys",
            "usr",
            256,
            Thinking::Auto,
            None,
        )
        .await
        .unwrap();
        let head = head(&handle.await.unwrap());
        assert!(
            !head.to_lowercase().contains("authorization"),
            "empty key must not send an Authorization header; got:\n{head}"
        );
    }

    #[tokio::test]
    async fn responses_extra_body_merged() {
        let (port, handle) = capture_server_with(200, RESPONSES_BODY).await;
        let client = reqwest::Client::new();
        let base = format!("http://127.0.0.1:{port}");
        let extra = serde_json::json!({"foo": "bar", "model": "HACKED"});
        responses_completion(
            &client,
            &base,
            "",
            "real-model",
            "sys",
            "usr",
            256,
            Thinking::Auto,
            Some(&extra),
        )
        .await
        .unwrap();
        let body = body_json(&handle.await.unwrap());
        assert_eq!(
            body["foo"], "bar",
            "custom extra_body field must be merged in"
        );
        assert_eq!(
            body["model"], "real-model",
            "termcmp's model must win on collision"
        );
    }

    #[tokio::test]
    async fn chat_non_success_returns_empty() {
        let (port, handle) = capture_server_with(429, "{}").await;
        let client = reqwest::Client::new();
        let base = format!("http://127.0.0.1:{port}");
        let texts = chat_completion(
            &client,
            &base,
            "",
            "m",
            "sys",
            "usr",
            256,
            None,
            Thinking::Auto,
            None,
        )
        .await
        .unwrap();
        assert!(
            texts.is_empty(),
            "non-success chat must yield no texts; got {texts:?}"
        );
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn chat_thinking_on_sets_enable_thinking_true() {
        let (port, handle) = capture_server().await;
        let client = reqwest::Client::new();
        let base = format!("http://127.0.0.1:{port}");
        chat_completion(
            &client,
            &base,
            "",
            "m",
            "sys",
            "usr",
            256,
            None,
            Thinking::On,
            None,
        )
        .await
        .unwrap();
        let body = body_json(&handle.await.unwrap());
        assert_eq!(
            body["chat_template_kwargs"]["enable_thinking"], true,
            "Thinking::On must set enable_thinking=true"
        );
    }

    #[tokio::test]
    async fn chat_thinking_budget_included() {
        let (port, handle) = capture_server().await;
        let client = reqwest::Client::new();
        let base = format!("http://127.0.0.1:{port}");
        chat_completion(
            &client,
            &base,
            "",
            "m",
            "sys",
            "usr",
            256,
            Some(1024),
            Thinking::Auto,
            None,
        )
        .await
        .unwrap();
        let body = body_json(&handle.await.unwrap());
        assert_eq!(
            body["thinking_budget"], 1024,
            "thinking budget must be included when set"
        );
    }
}
