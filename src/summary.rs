//! AI summaries for long videos.
//!
//! This is one of only two features that send content to a third party (the
//! other is opt-in SponsorBlock). It is off by default and only runs when the
//! user presses the button. The endpoint, model and key all belong to the user;
//! anything OpenAI-compatible works, including a local server.

use crate::api::Api;
use serde_json::Value;
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct SummaryConfig {
    pub endpoint: String,
    pub model: String,
    pub key: String,
}

const SYSTEM_PROMPT: &str = "\
You help someone watch long video-game walkthroughs, playthroughs and commentary \
videos. You get only the caption transcript, with timestamps like [12:34].

Write a compact, spoiler-aware guide:
1. A two-sentence overview of what the video covers.
2. An ordered list of the 5-9 main beats, each starting with its [timestamp].
3. \"Worth knowing\": collectibles, tough fights, missable items, backtracking, \
or commentary worth hearing. Skip this section if the transcript has none.
4. \"Skip to\": one or two timestamps where the good part starts.

Use only what the transcript says. If something is unclear, say so instead of \
inventing it. No preamble, no closing pleasantries.";

pub async fn summarize(
    api: &Api,
    config: &SummaryConfig,
    video_id: &str,
    title: &str,
    channel: &str,
) -> Result<String, String> {
    let transcript = api.transcript(video_id).await?;
    let transcript = clip(&transcript, 60_000);
    complete(
        config,
        serde_json::json!([
            {"role": "system", "content": SYSTEM_PROMPT},
            {
                "role": "user",
                "content": format!(
                    "Title: {title}\nChannel: {channel}\n\nTranscript:\n{transcript}"
                )
            }
        ]),
        900,
    )
    .await
}

/// A one-line round trip used by Settings → Test connection. It verifies the
/// endpoint, model and key without needing a video or captions.
pub async fn test(config: &SummaryConfig) -> Result<String, String> {
    let content = complete(
        config,
        serde_json::json!([
            {"role": "user", "content": "Reply with exactly: OK"}
        ]),
        8,
    )
    .await?;
    Ok(content.trim().to_string())
}

async fn complete(
    config: &SummaryConfig,
    messages: Value,
    max_tokens: u32,
) -> Result<String, String> {
    let endpoint = config.endpoint.trim().trim_end_matches('/');
    if endpoint.is_empty() {
        return Err("Set an API endpoint in Settings → AI summaries.".into());
    }

    let body = serde_json::json!({
        "model": config.model,
        "temperature": 0.3,
        "max_tokens": max_tokens,
        "messages": messages,
    });

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(150))
        .connect_timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| e.to_string())?;
    let response = client
        .post(format!("{endpoint}/chat/completions"))
        .bearer_auth(config.key.trim())
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("Could not reach the AI endpoint: {e}"))?;

    let status = response.status();
    let text = response.text().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        let hint = match status.as_u16() {
            401 | 403 => "The API key was rejected.",
            404 => "The endpoint or model was not found.",
            429 => "The AI endpoint is rate limiting this key.",
            _ => "The AI endpoint returned an error.",
        };
        return Err(format!("{hint} (HTTP {})", status.as_u16()));
    }

    let value: Value = serde_json::from_str(&text)
        .map_err(|_| "The AI endpoint returned a non-JSON response.".to_string())?;
    value
        .get("choices")
        .and_then(|choices| choices.get(0))
        .and_then(|choice| choice.get("message"))
        .and_then(|message| message.get("content"))
        .and_then(|content| content.as_str())
        .map(|content| content.trim().to_string())
        .filter(|content| !content.is_empty())
        .ok_or_else(|| "The AI endpoint returned an empty summary.".to_string())
}

fn clip(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[transcript truncated]", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clips_on_a_character_boundary() {
        let text = "aé".repeat(10);
        let clipped = clip(&text, 5);
        assert!(clipped.starts_with("aéa"));
        assert!(clipped.ends_with("[transcript truncated]"));
    }
}
