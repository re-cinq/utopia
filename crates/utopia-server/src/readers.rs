//! Reading service (0040): scanned documents and images go to the layout-recognition service MinerU (`mineru-api`); recordings go to a
//! transcription model that labels speakers.
//!
//! Recognizing a scanned document of several hundred pages takes minutes to tens of minutes. The processing task does not wait here: the first time, the file is submitted, and
//! the remote task id is recorded on the document (`documents.reader_task`), and the task is parked as `Deferred` back on the queue. Each later visit
//! asks once; if not ready it is parked again, and when ready the layout is fetched and handed to chunking. `Deferred` does not burn the retry budget, and after a process restart it resumes asking
//! with the recorded task id instead of resubmitting.
//!
//! Interface: `POST /tasks` (multipart, `files` + options) submits a task, `GET /tasks/{id}` asks for status
//! (pending / processing / completed / failed), `GET /tasks/{id}/result` fetches the result:
//! `results` is keyed by file name with the extension stripped, and `content_list` is a JSON **string**. `mineru-api`
//! does not authenticate itself; where a reverse proxy sits in front of it, the key is sent as a Bearer token.

use std::time::Duration;

use anyhow::{anyhow, Context};
use serde_json::{json, Value};
use utopia_core::models::{Document, LlmSettings};
use utopia_core::{Deferred, Terminal};
use utopia_ingest::Reading;

use crate::state::AppState;

pub mod ark_ocr;

/// How often to ask. One page of a scan takes a second or two on a GPU; asking every ten seconds is enough, and a file of tens of pages needs only a few asks
const POLL: Duration = Duration::from_secs(10);

/// How long a submitted task may wait at most. When the service swallows a task yet keeps reporting processing, the document must not stay in
/// parsing forever: six hours is enough for a thousand-page scan to finish on a CPU
const PATIENCE_HOURS: i64 = 6;

/// Timeout for uploading files and fetching results. The shared client's 20 seconds is for probes and small requests; a scan of several tens of MB cannot finish uploading in that time
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(600);

/// The reading service configured for the workspace: MinerU (the whole submit-then-ask flow below), or Ark's vision model
/// (`ark_ocr`, reads in one go, 0065). Both paths produce the same `Reading`
pub struct Ocr<'a> {
    base: &'a str,
    key: Option<&'a str>,
    backend: Option<&'a str>,
    ark: Option<ark_ocr::ArkOcr<'a>>,
}

impl<'a> Ocr<'a> {
    pub fn from_settings(s: &'a LlmSettings) -> Option<Self> {
        if !s.ocr_ready() {
            return None;
        }
        let key = s.ocr_api_key.as_deref().filter(|k| !k.is_empty());
        let base = s.ocr_base_url.as_deref()?.trim_end_matches('/');
        Some(Ocr {
            base,
            key,
            backend: s.ocr_backend.as_deref().filter(|b| !b.is_empty()),
            // ocr_ready has already guaranteed that the Ark path has a model and a key
            ark: (s.ocr_provider == "ark").then(|| {
                ark_ocr::ArkOcr::new(
                    base,
                    key.unwrap_or_default(),
                    s.ocr_model.as_deref().unwrap_or_default(),
                )
            }),
        })
    }

    fn request(
        &self,
        client: &reqwest::Client,
        method: reqwest::Method,
        path: &str,
    ) -> reqwest::RequestBuilder {
        let req = client.request(method, format!("{}/{path}", self.base));
        match self.key {
            Some(key) => req.bearer_auth(key),
            None => req,
        }
    }

    /// Connectivity test: if the service is alive, return the version it reports
    pub async fn health(&self) -> anyhow::Result<Value> {
        if let Some(ark) = &self.ark {
            return ark.health().await;
        }
        let client = crate::query_engine::http()?;
        let resp = self
            .request(&client, reqwest::Method::GET, "health")
            .send()
            .await
            .context("the OCR service is unreachable")?;
        let status = resp.status();
        if !status.is_success() {
            return Err(anyhow!("the OCR service answered {status}"));
        }
        let v: Value = resp.json().await.unwrap_or(Value::Null);
        Ok(json!({ "version": format!("MinerU {}", jstr(&v["version"])).trim().to_string() }))
    }

    /// Reads this file. If reading does not finish, return the error carrying `Deferred`, and the caller propagates it up unchanged
    pub async fn read(
        &self,
        state: &AppState,
        doc: &Document,
        bytes: Vec<u8>,
    ) -> anyhow::Result<Reading> {
        if let Some(ark) = &self.ark {
            return ark.read(&bytes).await;
        }
        let pool = &state.pool;
        let stored = utopia_store::documents::reader_task(pool, doc.id).await?;
        let current = stored.as_ref().filter(|t| {
            t["reader"] == "ocr" && t["sha256"] == doc.sha256.as_str() && t["service"] == self.base
        });
        let Some(task) = current else {
            // Never submitted; or the file was submitted in an older version, or to the service before it was replaced: void it and resubmit
            if stored.is_some() {
                utopia_store::documents::clear_reader_task(pool, doc.id).await?;
            }
            let task_id = self.submit(&doc.filename, bytes).await?;
            let task = json!({
                "reader": "ocr",
                "service": self.base,
                "task_id": task_id,
                "sha256": doc.sha256,
                "submitted_at": chrono::Utc::now(),
            });
            if !utopia_store::documents::claim_reader_task(pool, doc.id, &task).await? {
                tracing::info!(document = %doc.id, "another run submitted this file first");
            }
            return Err(anyhow!("waiting for the OCR service").context(Deferred::new(POLL)));
        };

        let task_id = task["task_id"].as_str().unwrap_or_default();
        let client = crate::query_engine::http()?;
        let resp = self
            .request(&client, reqwest::Method::GET, &format!("tasks/{task_id}"))
            .send()
            .await
            .context("the OCR service is unreachable")?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            // The service restarted, or the result passed its retention period: the task is gone, so resubmit on the next round
            utopia_store::documents::clear_reader_task(pool, doc.id).await?;
            return Err(anyhow!("the OCR service no longer knows the task")
                .context(Deferred::new(Duration::from_secs(1))));
        }
        let status: Value = resp
            .error_for_status()
            .context("the OCR service could not report the task")?
            .json()
            .await?;
        match status["status"].as_str() {
            Some("completed") => {}
            Some("failed") => {
                // Clear the task id, so an ordinary retry resubmits once: failures like insufficient GPU memory or an overloaded service may not fail again next time
                utopia_store::documents::clear_reader_task(pool, doc.id).await?;
                return Err(anyhow!(
                    "The OCR service could not read this file: {}",
                    status["error"].as_str().unwrap_or("no reason given")
                ));
            }
            _ => {
                let submitted = task["submitted_at"]
                    .as_str()
                    .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok());
                if submitted.is_some_and(|t| {
                    chrono::Utc::now().signed_duration_since(t)
                        > chrono::Duration::hours(PATIENCE_HOURS)
                }) {
                    utopia_store::documents::clear_reader_task(pool, doc.id).await?;
                    return Err(anyhow!(
                        "The OCR service did not finish reading this file within {PATIENCE_HOURS} hours"
                    )
                    .context(Terminal));
                }
                return Err(anyhow!("waiting for the OCR service").context(Deferred::new(POLL)));
            }
        }

        let result: Value = self
            .request(
                &client,
                reqwest::Method::GET,
                &format!("tasks/{task_id}/result"),
            )
            .timeout(TRANSFER_TIMEOUT)
            .send()
            .await
            .context("the OCR service is unreachable")?
            .error_for_status()
            .context("the OCR service could not return the result")?
            .json()
            .await?;
        // Only one file is submitted at a time, so the result has one item; the key is the file name as the service normalized it, and its rules are not guessed
        let entry = result["results"]
            .as_object()
            .and_then(|m| m.values().next())
            .ok_or_else(|| anyhow!("The OCR service returned no result for this file"))?;
        let list = match &entry["content_list"] {
            Value::String(s) => serde_json::from_str(s)
                .context("The OCR service returned a content list that is not JSON")?,
            other => other.clone(),
        };
        let model = ["mineru", jstr(&result["version"]), jstr(&result["backend"])]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        Ok(utopia_ingest::mineru::reading(&list, &model))
    }

    async fn submit(&self, filename: &str, bytes: Vec<u8>) -> anyhow::Result<String> {
        let client = crate::query_engine::http()?;
        let part = reqwest::multipart::Part::bytes(bytes)
            .file_name(filename.to_string())
            .mime_str("application/octet-stream")?;
        let mut form = reqwest::multipart::Form::new()
            .part("files", part)
            .text("return_content_list", "true")
            .text("return_md", "false");
        if let Some(backend) = self.backend {
            form = form.text("backend", backend.to_string());
        }
        let resp = self
            .request(&client, reqwest::Method::POST, "tasks")
            .multipart(form)
            .timeout(TRANSFER_TIMEOUT)
            .send()
            .await
            .context("the OCR service is unreachable")?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!(
                "The OCR service refused this file ({status}): {}",
                body.chars().take(300).collect::<String>()
            ));
        }
        let v: Value = resp.json().await?;
        v["task_id"]
            .as_str()
            .map(String::from)
            .ok_or_else(|| anyhow!("The OCR service accepted the file but returned no task id"))
    }
}

/// How long at most to wait to transcribe one recording. OpenAI takes 25MB at once, roughly an hour of compressed audio; a local service reading one hour
/// of recording takes tens of minutes on a CPU
const TRANSCRIBE_TIMEOUT: Duration = Duration::from_secs(1800);

/// The transcription model configured for the workspace: OpenAI's `/audio/transcriptions`, which needs `diarized_json`
pub struct Transcriber<'a> {
    base: &'a str,
    key: Option<&'a str>,
    model: &'a str,
}

impl<'a> Transcriber<'a> {
    pub fn from_settings(s: &'a LlmSettings) -> Option<Self> {
        Some(Transcriber {
            base: s.transcribe_base_url.as_deref()?.trim_end_matches('/'),
            key: s.transcribe_api_key.as_deref().filter(|k| !k.is_empty()),
            model: s.transcribe_model.as_deref()?,
        })
    }

    async fn transcribe(
        &self,
        filename: &str,
        bytes: Vec<u8>,
        timeout: Duration,
    ) -> anyhow::Result<Value> {
        let client = crate::query_engine::http()?;
        let part = reqwest::multipart::Part::bytes(bytes)
            .file_name(filename.to_string())
            .mime_str("application/octet-stream")?;
        let form = reqwest::multipart::Form::new()
            .part("file", part)
            .text("model", self.model.to_string())
            .text("response_format", "diarized_json")
            // For recordings over 30 seconds, the diarize model requires a chunking strategy
            .text("chunking_strategy", "auto");
        let mut req = client
            .post(format!("{}/audio/transcriptions", self.base))
            .multipart(form)
            .timeout(timeout);
        if let Some(key) = self.key {
            req = req.bearer_auth(key);
        }
        let resp = req
            .send()
            .await
            .context("the transcription model is unreachable")?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!(
                "The transcription model refused this recording ({status}): {}",
                body.chars().take(300).collect::<String>()
            ));
        }
        resp.json()
            .await
            .context("The transcription model returned something that is not JSON")
    }

    /// Reads this recording. One request reads it all: there is no remote task to ask again; results that cannot tell speakers apart
    /// are rejected in `transcript::reading` (`NoSpeakers`)
    pub async fn read(&self, doc: &Document, bytes: Vec<u8>) -> anyhow::Result<Reading> {
        let response = self
            .transcribe(&doc.filename, bytes, TRANSCRIBE_TIMEOUT)
            .await?;
        utopia_ingest::transcript::reading(&response, self.model)
    }

    /// Connectivity test: sends one second of silence. The endpoint and model accept it as ok; silence has no speakers to label,
    /// so whether labeling works can only be known once a first real recording arrives
    pub async fn check(&self) -> anyhow::Result<()> {
        self.transcribe("silence.wav", silent_wav(), Duration::from_secs(60))
            .await
            .map(|_| ())
    }
}

/// A WAV of one second of 16kHz mono silence
fn silent_wav() -> Vec<u8> {
    let (rate, samples): (u32, u32) = (16_000, 16_000);
    let data_len = samples * 2;
    let mut w = Vec::with_capacity(44 + data_len as usize);
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&(36 + data_len).to_le_bytes());
    w.extend_from_slice(b"WAVEfmt ");
    w.extend_from_slice(&16u32.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes());
    w.extend_from_slice(&rate.to_le_bytes());
    w.extend_from_slice(&(rate * 2).to_le_bytes());
    w.extend_from_slice(&2u16.to_le_bytes());
    w.extend_from_slice(&16u16.to_le_bytes());
    w.extend_from_slice(b"data");
    w.extend_from_slice(&data_len.to_le_bytes());
    w.resize(44 + data_len as usize, 0);
    w
}

fn jstr(v: &Value) -> &str {
    v.as_str().unwrap_or_default()
}
