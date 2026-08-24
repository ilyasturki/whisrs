//! Generic HTTP ASR sidecar transcription backend.
//!
//! This backend keeps the Rust daemon independent from Python/PyTorch by
//! sending WAV audio to a local HTTP sidecar.

use async_trait::async_trait;
use reqwest::multipart;
use serde::Deserialize;
use tracing::{debug, warn};

use super::{TranscriptionBackend, TranscriptionConfig};

/// Keep a guardrail so a runaway recording does not create an unbounded
/// multipart request.
const MAX_FILE_SIZE: usize = 1024 * 1024 * 1024;

/// Generic HTTP ASR sidecar transcription backend.
pub struct AsrSidecarBackend {
    client: reqwest::Client,
    url: String,
}

impl AsrSidecarBackend {
    /// Create a new sidecar backend with the transcription URL.
    pub fn new(url: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            url,
        }
    }
}

/// Response from the ASR sidecar.
#[derive(Debug, Deserialize)]
pub struct AsrSidecarResponse {
    /// Plain text transcript. Sidecars may also return richer diarized output,
    /// but whisrs currently consumes the flattened text for typing.
    pub text: String,
}

#[derive(Debug, Deserialize)]
struct AsrSidecarErrorResponse {
    error: Option<String>,
    detail: Option<serde_json::Value>,
}

impl AsrSidecarErrorResponse {
    fn message(&self) -> String {
        if let Some(error) = &self.error {
            return error.clone();
        }
        match &self.detail {
            Some(serde_json::Value::String(detail)) => detail.clone(),
            Some(detail) => detail.to_string(),
            None => "unknown sidecar error".to_string(),
        }
    }
}

#[async_trait]
impl TranscriptionBackend for AsrSidecarBackend {
    async fn transcribe(
        &self,
        audio: &[u8],
        config: &TranscriptionConfig,
    ) -> anyhow::Result<String> {
        if audio.len() > MAX_FILE_SIZE {
            anyhow::bail!(
                "audio file too large ({} bytes, max {} bytes / 1GB)",
                audio.len(),
                MAX_FILE_SIZE
            );
        }

        if audio.is_empty() {
            anyhow::bail!("cannot transcribe empty audio");
        }

        if self.url.trim().is_empty() {
            anyhow::bail!("no ASR sidecar URL configured");
        }

        debug!(
            "sending {} bytes to ASR sidecar (model={}, language={})",
            audio.len(),
            config.model,
            config.language
        );

        let file_part = multipart::Part::bytes(audio.to_vec())
            .file_name("audio.wav")
            .mime_str("audio/wav")?;

        let mut form = multipart::Form::new()
            .part("file", file_part)
            .text("model", config.model.clone());

        if config.language != "auto" {
            form = form.text("language", config.language.clone());
        }
        if let Some(prompt) = &config.prompt {
            form = form.text("hotwords", prompt.clone());
        }

        let response = self.client.post(&self.url).multipart(form).send().await?;
        let status = response.status();
        let body = response.text().await?;

        if !status.is_success() {
            if let Ok(err_resp) = serde_json::from_str::<AsrSidecarErrorResponse>(&body) {
                anyhow::bail!(
                    "ASR sidecar error ({}): {}",
                    status.as_u16(),
                    err_resp.message()
                );
            }
            anyhow::bail!("ASR sidecar error ({}): {}", status.as_u16(), body);
        }

        let parsed: AsrSidecarResponse = serde_json::from_str(&body)?;
        let text = parsed.text.trim().to_string();

        if text.is_empty() {
            warn!("ASR sidecar returned empty transcription");
        }

        Ok(text)
    }

    // Uses the default transcribe_stream (collect + transcribe). Model-specific
    // streaming behavior belongs in the sidecar process.
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn transcribe_rejects_empty_audio() {
        let backend = AsrSidecarBackend::new("http://127.0.0.1:8765/transcribe".to_string());
        let config = TranscriptionConfig {
            language: "en".to_string(),
            model: "test-asr-model".to_string(),
            prompt: None,
            keyterms: Vec::new(),
        };
        let err = backend.transcribe(&[], &config).await.unwrap_err();
        assert!(err.to_string().contains("empty audio"));
    }

    #[tokio::test]
    async fn transcribe_rejects_missing_url() {
        let backend = AsrSidecarBackend::new(String::new());
        let config = TranscriptionConfig {
            language: "en".to_string(),
            model: "test-asr-model".to_string(),
            prompt: None,
            keyterms: Vec::new(),
        };
        let err = backend.transcribe(&[1, 2, 3], &config).await.unwrap_err();
        assert!(err.to_string().contains("sidecar URL"));
    }

    #[test]
    fn parse_asr_sidecar_response() {
        let body = r#"{"text": "Hello world"}"#;
        let parsed: AsrSidecarResponse = serde_json::from_str(body).unwrap();
        assert_eq!(parsed.text, "Hello world");
    }

    #[test]
    fn parse_asr_sidecar_error() {
        let body = r#"{"error": "model failed to load"}"#;
        let parsed: AsrSidecarErrorResponse = serde_json::from_str(body).unwrap();
        assert_eq!(parsed.message(), "model failed to load");
    }

    #[test]
    fn parse_fastapi_error_detail() {
        let body = r#"{"detail": "request asked for wrong model"}"#;
        let parsed: AsrSidecarErrorResponse = serde_json::from_str(body).unwrap();
        assert_eq!(parsed.message(), "request asked for wrong model");
    }
}
