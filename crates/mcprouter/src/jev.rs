//! A minimal client for TypeSafe's System One endpoint.
//!
//! One `POST /v1/systemone` per routing decision. Deliberately small — the crate needs one
//! request shape and two answer kinds, not an SDK — and on the same `reqwest` as `mcpclient`.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;

pub const API_KEY_ENV: &str = "TYPESAFE_API_KEY";
pub const BASE_URL_ENV: &str = "TYPESAFE_BASE_URL";
pub const DEFAULT_BASE_URL: &str = "https://api.typesafe.ai";
pub const DEFAULT_MODEL: &str = "jev-latest";

const ATTEMPTS: u32 = 3;
const TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("no TypeSafe API key: set {API_KEY_ENV}")]
    MissingKey,
    #[error("TypeSafe request failed: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("TypeSafe returned HTTP {status}: {body}")]
    Status { status: u16, body: String },
    #[error("TypeSafe response is missing `{0}`")]
    MissingAnswer(String),
}

/// A `choice` question: one label picked from `criteria`, with a probability per label.
#[derive(Debug, Clone, Serialize)]
pub struct Choice {
    pub instructions: String,
    pub criteria: BTreeMap<String, String>,
}

/// A `noul` question: probability that the statement holds.
#[derive(Debug, Clone, Serialize)]
pub struct Noul {
    pub instructions: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Question {
    Choice(Choice),
    Noul(Noul),
}

#[derive(Debug, Clone, Default)]
pub struct Answers {
    pub choices: BTreeMap<String, ChoiceAnswer>,
    pub nouls: BTreeMap<String, f64>,
    pub input_tokens: u64,
}

#[derive(Debug, Clone, Default)]
pub struct ChoiceAnswer {
    pub choice: String,
    pub confidence: f64,
    pub probabilities: BTreeMap<String, f64>,
}

#[derive(Debug, Clone)]
pub struct Jev {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
    model: String,
}

impl Jev {
    /// From the environment: `TYPESAFE_API_KEY` (required) and `TYPESAFE_BASE_URL` (optional).
    pub fn from_env() -> Result<Self, Error> {
        let key = std::env::var(API_KEY_ENV)
            .ok()
            .filter(|k| !k.trim().is_empty());
        Self::new(key.ok_or(Error::MissingKey)?, DEFAULT_MODEL)
    }

    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Result<Self, Error> {
        let base_url = std::env::var(BASE_URL_ENV)
            .ok()
            .filter(|u| !u.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
        Ok(Self {
            http: reqwest::Client::builder().timeout(TIMEOUT).build()?,
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key: api_key.into(),
            model: model.into(),
        })
    }

    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into().trim_end_matches('/').to_string();
        self
    }

    /// Ask every question about `state` in one call.
    pub async fn system_one(
        &self,
        state: &Value,
        questions: &BTreeMap<String, Question>,
    ) -> Result<Answers, Error> {
        let body =
            serde_json::json!({ "model": self.model, "state": state, "questions": questions });
        let url = format!("{}/v1/systemone", self.base_url);
        let mut attempt = 0;
        let value: Value = loop {
            attempt += 1;
            let sent = self
                .http
                .post(&url)
                .bearer_auth(&self.api_key)
                .json(&body)
                .send()
                .await;
            let retry_after = Duration::from_millis(500 * 2u64.pow(attempt - 1));
            match sent {
                Ok(resp) if resp.status().is_success() => break resp.json().await?,
                Ok(resp) => {
                    let status = resp.status().as_u16();
                    let retryable = status == 429 || status >= 500;
                    if retryable && attempt < ATTEMPTS {
                        tokio::time::sleep(retry_after).await;
                        continue;
                    }
                    let body = resp.text().await.unwrap_or_default();
                    return Err(Error::Status {
                        status,
                        body: body.chars().take(300).collect(),
                    });
                }
                Err(e) if attempt < ATTEMPTS && (e.is_connect() || e.is_timeout()) => {
                    tokio::time::sleep(retry_after).await;
                }
                Err(e) => return Err(e.into()),
            }
        };
        Ok(parse_answers(&value))
    }
}

fn parse_answers(value: &Value) -> Answers {
    let mut out = Answers {
        input_tokens: value["usage"]["input_tokens"].as_u64().unwrap_or(0),
        ..Default::default()
    };
    let Some(answers) = value["answers"].as_object() else {
        return out;
    };
    for (id, a) in answers {
        match a["type"].as_str() {
            Some("choice") => {
                let probabilities = a["probabilities"]
                    .as_object()
                    .map(|m| {
                        m.iter()
                            .filter_map(|(k, v)| Some((k.clone(), v.as_f64()?)))
                            .collect()
                    })
                    .unwrap_or_default();
                out.choices.insert(
                    id.clone(),
                    ChoiceAnswer {
                        choice: a["choice"].as_str().unwrap_or("").to_string(),
                        confidence: a["confidence"].as_f64().unwrap_or(0.0),
                        probabilities,
                    },
                );
            }
            Some("noul") => {
                if let Some(p) = a["noul"].as_f64() {
                    out.nouls.insert(id.clone(), p);
                }
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_choice_and_noul_answers() {
        let v = serde_json::json!({
            "answers": {
                "pick": {"type": "choice", "choice": "a", "confidence": 0.8, "probabilities": {"a": 0.8, "b": 0.2}},
                "acts": {"type": "noul", "noul": 0.9},
                "later": {"type": "score", "score": 1.0}
            },
            "usage": {"input_tokens": 42}
        });
        let a = parse_answers(&v);
        assert_eq!(a.input_tokens, 42);
        assert_eq!(a.choices["pick"].probabilities["b"], 0.2);
        assert_eq!(a.nouls["acts"], 0.9);
        assert!(a.choices.len() == 1 && a.nouls.len() == 1);
    }

    #[test]
    fn questions_serialise_with_type_tag() {
        let q = Question::Noul(Noul {
            instructions: "x".into(),
        });
        assert_eq!(
            serde_json::to_value(q).unwrap(),
            serde_json::json!({"type": "noul", "instructions": "x"})
        );
    }
}
