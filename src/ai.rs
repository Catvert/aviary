//! Shared configuration and types for the AI writing assistant.
//!
//! HTTP calls live in `runtime::ai`; this module contains only serializable
//! data used by settings and the UI protocol.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiPromptPreset {
    pub id: u64,
    pub name: String,
    /// Free-form template. Available variables: `[[instruction]]` (required),
    /// `[[instruction_optional]]`, `[[subject]]`, and `[[body]]`.
    pub prompt: String,
}

impl AiPromptPreset {
    pub fn requires_instruction(&self) -> bool {
        self.prompt.contains("[[instruction]]")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum AiProvider {
    #[default]
    OpenAi,
    Anthropic,
    Gemini,
    Local,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiSettings {
    pub provider: AiProvider,
    pub openai_api_key: String,
    pub openai_model: String,
    pub anthropic_api_key: String,
    pub anthropic_model: String,
    pub gemini_api_key: String,
    pub gemini_model: String,
    /// OpenAI-compatible base URL. `/chat/completions` is appended if absent.
    pub local_base_url: String,
    pub local_api_key: String,
    pub local_model: String,
    pub system_prompt: String,
    /// Model used only to translate a received message in the reader.
    /// `[[instruction]]` contains the target language.
    pub reader_translation_prompt: String,
    pub reader_translation_target: String,
    pub prompts: Vec<AiPromptPreset>,
    pub prompt_seq: u64,
    /// Distinguishes first launch from a list deliberately emptied by the user.
    pub prompts_initialized: bool,
    /// Indicates that the reader's initial values have been created.
    pub reader_translation_initialized: bool,
    pub reader_translation_target_initialized: bool,
}

impl Default for AiSettings {
    fn default() -> Self {
        Self {
            provider: AiProvider::default(),
            openai_api_key: String::new(),
            openai_model: "gpt-5-mini".to_string(),
            anthropic_api_key: String::new(),
            anthropic_model: "claude-sonnet-5".to_string(),
            gemini_api_key: String::new(),
            gemini_model: "gemini-3.5-flash-lite".to_string(),
            local_base_url: "http://127.0.0.1:11434/v1".to_string(),
            local_api_key: String::new(),
            local_model: "llama3.2".to_string(),
            system_prompt: String::new(),
            reader_translation_prompt: String::new(),
            reader_translation_target: String::new(),
            prompts: Vec::new(),
            prompt_seq: 0,
            prompts_initialized: false,
            reader_translation_initialized: false,
            reader_translation_target_initialized: false,
        }
    }
}

impl AiSettings {
    /// Initializes prompts once from the active catalog. They then become
    /// regular user data and no longer follow the interface language.
    pub fn ensure_prompt_defaults(&mut self) {
        if !self.prompts_initialized {
            self.system_prompt = crate::tr!("settings-ai-default-system-prompt").to_string();
            let defaults = [
                (
                    crate::tr!("compose-ai-generate"),
                    crate::tr!("settings-ai-default-prompt-generate"),
                ),
                (
                    crate::tr!("compose-ai-correct"),
                    crate::tr!("settings-ai-default-prompt-correct"),
                ),
                (
                    crate::tr!("compose-ai-rephrase"),
                    crate::tr!("settings-ai-default-prompt-rephrase"),
                ),
                (
                    crate::tr!("compose-ai-translate"),
                    crate::tr!("settings-ai-default-prompt-translate"),
                ),
            ];
            self.prompts = defaults
                .into_iter()
                .enumerate()
                .map(|(index, (name, prompt))| AiPromptPreset {
                    id: index as u64 + 1,
                    name: name.to_string(),
                    prompt: prompt.to_string(),
                })
                .collect();
            self.prompt_seq = self.prompts.len() as u64;
            self.prompts_initialized = true;
        }

        if !self.reader_translation_initialized {
            self.reader_translation_prompt =
                crate::tr!("settings-ai-default-reader-translation-prompt").to_string();
            self.reader_translation_initialized = true;
        }
        if !self.reader_translation_target_initialized {
            self.reader_translation_target =
                crate::tr!("settings-ai-default-reader-translation-target").to_string();
            self.reader_translation_target_initialized = true;
        }
    }

    pub fn active_config(&self) -> AiConfig {
        let (api_key, model, base_url) = match self.provider {
            AiProvider::OpenAi => (
                self.openai_api_key.clone(),
                self.openai_model.clone(),
                "https://api.openai.com/v1".to_string(),
            ),
            AiProvider::Anthropic => (
                self.anthropic_api_key.clone(),
                self.anthropic_model.clone(),
                "https://api.anthropic.com/v1".to_string(),
            ),
            AiProvider::Gemini => (
                self.gemini_api_key.clone(),
                self.gemini_model.clone(),
                "https://generativelanguage.googleapis.com/v1beta".to_string(),
            ),
            AiProvider::Local => (
                self.local_api_key.clone(),
                self.local_model.clone(),
                self.local_base_url.clone(),
            ),
        };
        AiConfig {
            provider: self.provider,
            api_key,
            model,
            base_url,
        }
    }
}

#[derive(Debug, Clone)]
pub struct AiConfig {
    pub provider: AiProvider,
    pub api_key: String,
    pub model: String,
    pub base_url: String,
}

#[cfg(test)]
mod tests {
    use super::AiSettings;

    #[test]
    fn default_gemini_model_is_flash_lite() {
        assert_eq!(AiSettings::default().gemini_model, "gemini-3.5-flash-lite");
    }
}
