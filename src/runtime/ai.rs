//! Streaming calls to AI providers. All four backends are normalized into a
//! common SSE stream; small fragments are coalesced to a frame-sized cadence
//! before crossing into gpui.

use super::{AiEditRequest, BgGlobal, Evt};
use crate::ai::AiProvider;
use anyhow::{bail, Context, Result};
use futures::StreamExt;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

pub(super) async fn edit_mail(global: Arc<BgGlobal>, request: AiEditRequest) {
    let compose_id = request.compose_id;
    let result = tokio::time::timeout(Duration::from_secs(120), perform(&global, &request)).await;
    match result {
        Ok(Ok(markdown)) => global.emit(Evt::AiMailEditFinished {
            compose_id,
            markdown: strip_markdown_fence(markdown),
        }),
        Ok(Err(error)) => global.emit(Evt::AiMailEditError {
            compose_id,
            error: format!("{error:#}"),
        }),
        Err(_) => global.emit(Evt::AiMailEditError {
            compose_id,
            error: tr!("ai-error-timeout").to_string(),
        }),
    }
}

async fn perform(global: &BgGlobal, request: &AiEditRequest) -> Result<String> {
    let config = &request.config;
    if config.model.trim().is_empty() {
        bail!(tr!("ai-error-model-missing"));
    }
    if !matches!(config.provider, AiProvider::Local) && config.api_key.trim().is_empty() {
        bail!(tr!("ai-error-key-missing"));
    }
    let prompt = render_prompt(request);
    match config.provider {
        AiProvider::OpenAi | AiProvider::Local => openai_compatible(global, request, &prompt).await,
        AiProvider::Anthropic => anthropic(global, request, &prompt).await,
        AiProvider::Gemini => gemini(global, request, &prompt).await,
    }
}

fn render_prompt(request: &AiEditRequest) -> String {
    request
        .prompt_template
        .replace("[[instruction_optional]]", request.instruction.trim())
        .replace("[[instruction]]", request.instruction.trim())
        .replace("[[subject]]", &request.subject)
        .replace("[[body]]", &request.body_markdown)
}

async fn openai_compatible(
    global: &BgGlobal,
    request: &AiEditRequest,
    prompt: &str,
) -> Result<String> {
    let config = &request.config;
    let base = config.base_url.trim().trim_end_matches('/');
    if base.is_empty() {
        bail!(tr!("ai-error-local-url-missing"));
    }
    let url = if base.ends_with("/chat/completions") {
        base.to_string()
    } else {
        format!("{base}/chat/completions")
    };
    let mut messages = Vec::new();
    if !request.system_prompt.trim().is_empty() {
        messages.push(json!({"role": "system", "content": request.system_prompt}));
    }
    messages.push(json!({"role": "user", "content": prompt}));
    let mut builder = global.http.post(url).json(&json!({
        "model": config.model.trim(),
        "stream": true,
        "messages": messages
    }));
    if !config.api_key.trim().is_empty() {
        builder = builder.bearer_auth(config.api_key.trim());
    }
    let response = builder.send().await.context(tr!("ai-error-request"))?;
    consume_sse(global, request.compose_id, response, |value| {
        value
            .pointer("/choices/0/delta/content")
            .and_then(Value::as_str)
            .map(str::to_string)
    })
    .await
}

async fn anthropic(global: &BgGlobal, request: &AiEditRequest, prompt: &str) -> Result<String> {
    let config = &request.config;
    let url = format!("{}/messages", config.base_url.trim_end_matches('/'));
    let mut payload = json!({
        "model": config.model.trim(),
        "max_tokens": 8192,
        "stream": true,
        "messages": [{"role": "user", "content": prompt}]
    });
    if !request.system_prompt.trim().is_empty() {
        payload["system"] = json!(request.system_prompt);
    }
    let response = global
        .http
        .post(url)
        .header("x-api-key", config.api_key.trim())
        .header("anthropic-version", "2023-06-01")
        .json(&payload)
        .send()
        .await
        .context(tr!("ai-error-request"))?;
    consume_sse(global, request.compose_id, response, |value| {
        (value.pointer("/delta/type").and_then(Value::as_str) == Some("text_delta"))
            .then(|| {
                value
                    .pointer("/delta/text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string()
            })
            .filter(|text| !text.is_empty())
    })
    .await
}

async fn gemini(global: &BgGlobal, request: &AiEditRequest, prompt: &str) -> Result<String> {
    let config = &request.config;
    let model = urlencoding::encode(config.model.trim());
    let url = format!(
        "{}/models/{model}:streamGenerateContent?alt=sse",
        config.base_url.trim_end_matches('/')
    );
    let mut payload = json!({
        "contents": [{"role": "user", "parts": [{"text": prompt}]}]
    });
    if !request.system_prompt.trim().is_empty() {
        payload["systemInstruction"] = json!({"parts": [{"text": request.system_prompt}]});
    }
    let response = global
        .http
        .post(url)
        .header("x-goog-api-key", config.api_key.trim())
        .json(&payload)
        .send()
        .await
        .context(tr!("ai-error-request"))?;
    consume_sse(global, request.compose_id, response, |value| {
        let text = value
            .pointer("/candidates/0/content/parts")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<String>();
        (!text.is_empty()).then_some(text)
    })
    .await
}

async fn consume_sse(
    global: &BgGlobal,
    compose_id: u64,
    response: reqwest::Response,
    extract: impl Fn(&Value) -> Option<String>,
) -> Result<String> {
    let response = ensure_success(response).await?;
    let mut stream = response.bytes_stream();
    let mut pending = Vec::<u8>::new();
    let mut output = String::new();
    let mut ui_pending = String::new();
    let mut ticker = tokio::time::interval(Duration::from_millis(32));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    ticker.tick().await;

    loop {
        tokio::select! {
            chunk = stream.next() => {
                let Some(chunk) = chunk else {
                    break;
                };
                let chunk = chunk.context(tr!("ai-error-read-response"))?;
                for line in take_sse_lines(&mut pending, &chunk) {
                    process_sse_line(&line, &extract, &mut output, &mut ui_pending)?;
                }
            }
            _ = ticker.tick() => emit_ai_delta(global, compose_id, &mut ui_pending),
        }
    }
    if !pending.is_empty() {
        process_sse_line(&pending, &extract, &mut output, &mut ui_pending)?;
    }
    emit_ai_delta(global, compose_id, &mut ui_pending);
    if output.trim().is_empty() {
        bail!(tr!("ai-error-empty-response"));
    }
    Ok(output)
}

/// Appends a network chunk and extracts only complete SSE lines. Remaining
/// bytes may include a split UTF-8 character.
fn take_sse_lines(pending: &mut Vec<u8>, chunk: &[u8]) -> Vec<Vec<u8>> {
    pending.extend_from_slice(chunk);
    let mut lines = Vec::new();
    while let Some(newline) = pending.iter().position(|byte| *byte == b'\n') {
        let mut line = pending.drain(..=newline).collect::<Vec<_>>();
        line.pop();
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        lines.push(line);
    }
    lines
}

fn process_sse_line(
    line: &[u8],
    extract: &impl Fn(&Value) -> Option<String>,
    output: &mut String,
    ui_pending: &mut String,
) -> Result<()> {
    let line = std::str::from_utf8(line).context(tr!("ai-error-invalid-response"))?;
    let Some(data) = line.strip_prefix("data:") else {
        return Ok(());
    };
    let data = data.trim_start();
    if data.is_empty() || data == "[DONE]" {
        return Ok(());
    }
    let value: Value = serde_json::from_str(data).context(tr!("ai-error-invalid-response"))?;
    if let Some(detail) = value.pointer("/error/message").and_then(Value::as_str) {
        bail!(tr!("ai-error-stream", { detail: detail }));
    }
    if let Some(delta) = extract(&value) {
        output.push_str(&delta);
        ui_pending.push_str(&delta);
    }
    Ok(())
}

fn emit_ai_delta(global: &BgGlobal, compose_id: u64, pending: &mut String) {
    if pending.is_empty() {
        return;
    }
    global.emit(Evt::AiMailEditChunk {
        compose_id,
        delta: std::mem::take(pending),
    });
}

async fn ensure_success(response: reqwest::Response) -> Result<reqwest::Response> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let text = response
        .text()
        .await
        .context(tr!("ai-error-read-response"))?;
    let detail = serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|value| {
            value
                .pointer("/error/message")
                .or_else(|| value.pointer("/error/error/message"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| text.chars().take(500).collect());
    bail!(tr!("ai-error-service", { status: status, detail: detail }));
}

fn strip_markdown_fence(text: String) -> String {
    let trimmed = text.trim();
    if !trimmed.starts_with("```") || !trimmed.ends_with("```") {
        return trimmed.to_string();
    }
    let Some(first_newline) = trimmed.find('\n') else {
        return trimmed.to_string();
    };
    trimmed[first_newline + 1..trimmed.len() - 3]
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::{render_prompt, strip_markdown_fence, take_sse_lines};

    #[test]
    fn renders_prompt_template_without_hardcoded_operation() {
        let request = crate::runtime::AiEditRequest {
            compose_id: 1,
            config: crate::ai::AiConfig {
                provider: crate::ai::AiProvider::Local,
                api_key: String::new(),
                model: "test".into(),
                base_url: "http://localhost/v1".into(),
            },
            system_prompt: "system".into(),
            prompt_template: "Instruction: [[instruction]]\nSubject: [[subject]]\n[[body]]".into(),
            instruction: "in plain English".into(),
            subject: "Hello".into(),
            body_markdown: "Body".into(),
        };
        assert_eq!(
            render_prompt(&request),
            "Instruction: in plain English\nSubject: Hello\nBody"
        );
    }

    #[test]
    fn removes_redundant_markdown_fence() {
        assert_eq!(
            strip_markdown_fence("```markdown\nHello\n```".into()),
            "Hello"
        );
        assert_eq!(strip_markdown_fence("Hello".into()), "Hello");
    }

    #[test]
    fn decodes_sse_event_split_inside_utf8_character() {
        let source = "data: {\"text\":\"東京\"}\n".as_bytes();
        let split = source
            .windows(2)
            .position(|pair| pair == [0xe6, 0x9d])
            .expect("UTF-8 character")
            + 1;
        let mut pending = Vec::new();
        assert!(take_sse_lines(&mut pending, &source[..split]).is_empty());
        let lines = take_sse_lines(&mut pending, &source[split..]);
        assert_eq!(lines.len(), 1);
        assert_eq!(
            std::str::from_utf8(&lines[0]).unwrap(),
            "data: {\"text\":\"東京\"}"
        );
        assert!(pending.is_empty());
    }
}
