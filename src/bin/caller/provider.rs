use crate::conversation::Message;
use crate::error::CallerError;
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::env;

#[async_trait]
pub trait ChatProvider: Send + Sync {
    async fn chat(&self, messages: &[Message]) -> Result<String, CallerError>;
    fn name(&self) -> &str;
}

// --- OpenAI ---

#[derive(Serialize)]
struct OpenAIChatRequest {
    model: String,
    messages: Vec<Message>,
}

#[derive(Deserialize)]
struct OpenAIChatResponse {
    choices: Vec<OpenAIChoice>,
}

#[derive(Deserialize)]
struct OpenAIChoice {
    message: OpenAIResponseMessage,
}

#[derive(Deserialize)]
struct OpenAIResponseMessage {
    content: Option<String>,
}

pub struct OpenAIProvider {
    client: Client,
    api_key: String,
    model: String,
}

impl OpenAIProvider {
    pub fn new(api_key: String, model: String) -> Self {
        Self {
            client: Client::new(),
            api_key,
            model,
        }
    }
}

#[async_trait]
impl ChatProvider for OpenAIProvider {
    async fn chat(&self, messages: &[Message]) -> Result<String, CallerError> {
        let request = OpenAIChatRequest {
            model: self.model.clone(),
            messages: messages.to_vec(),
        };

        let response = self
            .client
            .post("https://api.openai.com/v1/chat/completions")
            .header("Authorization", format!("Bearer {}", self.api_key))
            .json(&request)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(CallerError::Provider(format!("{}: {}", status, body)));
        }

        let chat_response: OpenAIChatResponse = response.json().await?;
        let content = chat_response
            .choices
            .first()
            .and_then(|c| c.message.content.clone())
            .ok_or_else(|| CallerError::Provider("No response content".to_string()))?;

        Ok(content)
    }

    fn name(&self) -> &str {
        "openai"
    }
}

// --- Anthropic ---

#[derive(Serialize)]
struct AnthropicChatRequest {
    model: String,
    system: String,
    messages: Vec<AnthropicMessage>,
    max_tokens: u32,
}

#[derive(Serialize)]
struct AnthropicMessage {
    role: String,
    content: String,
}

#[derive(Deserialize)]
struct AnthropicChatResponse {
    content: Vec<AnthropicContent>,
}

#[derive(Deserialize)]
struct AnthropicContent {
    text: Option<String>,
}

pub struct AnthropicProvider {
    client: Client,
    api_key: String,
    model: String,
}

impl AnthropicProvider {
    pub fn new(api_key: String, model: String) -> Self {
        Self {
            client: Client::new(),
            api_key,
            model,
        }
    }
}

#[async_trait]
impl ChatProvider for AnthropicProvider {
    async fn chat(&self, messages: &[Message]) -> Result<String, CallerError> {
        // Extract system message and filter to user/assistant only
        let system = messages
            .iter()
            .find(|m| m.role == "system")
            .map(|m| m.content.clone())
            .unwrap_or_default();

        let api_messages: Vec<AnthropicMessage> = messages
            .iter()
            .filter(|m| m.role == "user" || m.role == "assistant")
            .map(|m| AnthropicMessage {
                role: m.role.clone(),
                content: m.content.clone(),
            })
            .collect();

        let request = AnthropicChatRequest {
            model: self.model.clone(),
            system,
            messages: api_messages,
            max_tokens: 8192,
        };

        let response = self
            .client
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&request)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(CallerError::Provider(format!("{}: {}", status, body)));
        }

        let chat_response: AnthropicChatResponse = response.json().await?;
        let content = chat_response
            .content
            .first()
            .and_then(|c| c.text.clone())
            .ok_or_else(|| CallerError::Provider("No response content".to_string()))?;

        Ok(content)
    }

    fn name(&self) -> &str {
        "anthropic"
    }
}

// --- Provider selection ---

pub fn select_provider() -> Result<Box<dyn ChatProvider>, CallerError> {
    let openai_key = env::var("OPENAI_API_KEY")
        .or_else(|_| env::var("OPENAI"))
        .ok();
    let anthropic_key = env::var("ANTHROPIC_API_KEY")
        .or_else(|_| env::var("ANTHROPIC"))
        .ok();

    let preferred = env::var("PROVIDER").ok();

    match (openai_key, anthropic_key, preferred.as_deref()) {
        // Both available, check PROVIDER preference
        (Some(oai), Some(ant), Some("anthropic")) => {
            let _ = oai;
            let model = env::var("MODEL_NAME")
                .unwrap_or_else(|_| "claude-sonnet-4-5-20250929".to_string());
            Ok(Box::new(AnthropicProvider::new(ant, model)))
        }
        (Some(oai), Some(_ant), Some("openai")) | (Some(oai), Some(_ant), None) => {
            let model = env::var("MODEL_NAME").unwrap_or_else(|_| "gpt-4o".to_string());
            Ok(Box::new(OpenAIProvider::new(oai, model)))
        }
        (Some(oai), None, _) => {
            let model = env::var("MODEL_NAME").unwrap_or_else(|_| "gpt-4o".to_string());
            Ok(Box::new(OpenAIProvider::new(oai, model)))
        }
        (None, Some(ant), _) => {
            let model = env::var("MODEL_NAME")
                .unwrap_or_else(|_| "claude-sonnet-4-5-20250929".to_string());
            Ok(Box::new(AnthropicProvider::new(ant, model)))
        }
        (Some(_oai), Some(_ant), Some(other)) => {
            Err(CallerError::Config(format!(
                "Unknown PROVIDER value: '{}'. Expected 'openai' or 'anthropic'.",
                other
            )))
        }
        (None, None, _) => Err(CallerError::Config(
            "No API key found. Set OPENAI_API_KEY or ANTHROPIC_API_KEY.".to_string(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openai_provider_name() {
        let provider = OpenAIProvider::new("key".to_string(), "gpt-4o".to_string());
        assert_eq!(provider.name(), "openai");
    }

    #[test]
    fn anthropic_provider_name() {
        let provider = AnthropicProvider::new("key".to_string(), "claude-sonnet-4-5-20250929".to_string());
        assert_eq!(provider.name(), "anthropic");
    }

    #[test]
    fn anthropic_extracts_system_message() {
        let messages = vec![
            Message {
                role: "system".to_string(),
                content: "You are helpful.".to_string(),
            },
            Message {
                role: "user".to_string(),
                content: "Hello".to_string(),
            },
            Message {
                role: "assistant".to_string(),
                content: "Hi!".to_string(),
            },
        ];

        let system = messages
            .iter()
            .find(|m| m.role == "system")
            .map(|m| m.content.clone())
            .unwrap_or_default();

        assert_eq!(system, "You are helpful.");

        let api_messages: Vec<AnthropicMessage> = messages
            .iter()
            .filter(|m| m.role == "user" || m.role == "assistant")
            .map(|m| AnthropicMessage {
                role: m.role.clone(),
                content: m.content.clone(),
            })
            .collect();

        assert_eq!(api_messages.len(), 2);
        assert_eq!(api_messages[0].role, "user");
        assert_eq!(api_messages[1].role, "assistant");
    }

    #[test]
    fn anthropic_no_system_message() {
        let messages = vec![Message {
            role: "user".to_string(),
            content: "Hello".to_string(),
        }];

        let system = messages
            .iter()
            .find(|m| m.role == "system")
            .map(|m| m.content.clone())
            .unwrap_or_default();

        assert_eq!(system, "");
    }
}
