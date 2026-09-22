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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum AiProvider {
    #[default]
    OpenAi,
    Anthropic,
    Gemini,
    Local,
}

impl AiProvider {
    pub const ALL: [AiProvider; 4] = [
        AiProvider::OpenAi,
        AiProvider::Anthropic,
        AiProvider::Gemini,
        AiProvider::Local,
    ];
}

/// One API key per provider.
///
/// The same type plays two roles on [`AiSettings`]: the keys actually in use
/// (`api_keys`, memory only, loaded from the OS keyring by `crate::ai_keys`)
/// and the plaintext fallback (`plaintext_api_keys`), which is what reaches
/// `settings.json` — and only for a provider whose key the keyring refused.
/// The serde names are those of the fields `settings.json` used to carry, so
/// files written before the keyring still load and get migrated.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AiApiKeys {
    #[serde(
        rename = "openai_api_key",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub openai: String,
    #[serde(
        rename = "anthropic_api_key",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub anthropic: String,
    #[serde(
        rename = "gemini_api_key",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub gemini: String,
    #[serde(
        rename = "local_api_key",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub local: String,
}

impl AiApiKeys {
    pub fn get(&self, provider: AiProvider) -> &str {
        match provider {
            AiProvider::OpenAi => &self.openai,
            AiProvider::Anthropic => &self.anthropic,
            AiProvider::Gemini => &self.gemini,
            AiProvider::Local => &self.local,
        }
    }

    pub fn set(&mut self, provider: AiProvider, key: String) {
        match provider {
            AiProvider::OpenAi => self.openai = key,
            AiProvider::Anthropic => self.anthropic = key,
            AiProvider::Gemini => self.gemini = key,
            AiProvider::Local => self.local = key,
        }
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        AiProvider::ALL.iter().all(|p| self.get(*p).is_empty())
    }
}

/// Never prints a key: `Settings` derives `Debug`, and a debug dump of it
/// must not leak a secret into the logs.
impl std::fmt::Debug for AiApiKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mark = |key: &str| if key.is_empty() { "<empty>" } else { "<set>" };
        f.debug_struct("AiApiKeys")
            .field("openai", &mark(&self.openai))
            .field("anthropic", &mark(&self.anthropic))
            .field("gemini", &mark(&self.gemini))
            .field("local", &mark(&self.local))
            .finish()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiSettings {
    pub provider: AiProvider,
    /// Keys in use, memory only. Seeded from `plaintext_api_keys` at load,
    /// then from the OS keyring by `crate::ai_keys` in the background.
    #[serde(skip)]
    pub api_keys: AiApiKeys,
    /// Plaintext fallback persisted in `settings.json`: empty unless the
    /// keyring refused a key (no Secret Service, locked store…). Also where a
    /// pre-keyring `settings.json` keeps its keys until they are migrated.
    #[serde(flatten)]
    pub plaintext_api_keys: AiApiKeys,
    pub openai_model: String,
    pub anthropic_model: String,
    pub gemini_model: String,
    /// OpenAI-compatible base URL. `/chat/completions` is appended if absent.
    pub local_base_url: String,
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
            api_keys: AiApiKeys::default(),
            plaintext_api_keys: AiApiKeys::default(),
            openai_model: "gpt-5-mini".to_string(),
            anthropic_model: "claude-sonnet-5".to_string(),
            gemini_model: "gemini-3.5-flash-lite".to_string(),
            local_base_url: "http://127.0.0.1:11434/v1".to_string(),
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
        let (model, base_url) = match self.provider {
            AiProvider::OpenAi => (
                self.openai_model.clone(),
                "https://api.openai.com/v1".to_string(),
            ),
            AiProvider::Anthropic => (
                self.anthropic_model.clone(),
                "https://api.anthropic.com/v1".to_string(),
            ),
            AiProvider::Gemini => (
                self.gemini_model.clone(),
                "https://generativelanguage.googleapis.com/v1beta".to_string(),
            ),
            AiProvider::Local => (self.local_model.clone(), self.local_base_url.clone()),
        };
        AiConfig {
            provider: self.provider,
            api_key: self.api_keys.get(self.provider).to_string(),
            model,
            base_url,
        }
    }
}

#[derive(Clone)]
pub struct AiConfig {
    pub provider: AiProvider,
    /// May be empty when the keyring has not answered yet: `runtime::ai`
    /// then reads it from the keyring itself before giving up.
    pub api_key: String,
    pub model: String,
    pub base_url: String,
}

/// Same rule as [`AiApiKeys`]: the config travels inside `Cmd`, which is
/// `Debug`, so the key is never formatted.
impl std::fmt::Debug for AiConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AiConfig")
            .field("provider", &self.provider)
            .field("api_key", &(!self.api_key.is_empty()))
            .field("model", &self.model)
            .field("base_url", &self.base_url)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::{AiProvider, AiSettings};

    #[test]
    fn default_gemini_model_is_flash_lite() {
        assert_eq!(AiSettings::default().gemini_model, "gemini-3.5-flash-lite");
    }

    #[test]
    fn keys_in_use_are_never_serialized_nor_debug_printed() {
        let mut settings = AiSettings::default();
        settings
            .api_keys
            .set(AiProvider::OpenAi, "sk-test-in-keyring".into());
        let json = serde_json::to_string(&settings).unwrap();
        assert!(!json.contains("sk-test-in-keyring"));
        assert!(!json.contains("openai_api_key"));
        assert!(!format!("{settings:?}").contains("sk-test-in-keyring"));
        assert!(!format!("{:?}", settings.active_config()).contains("sk-test-in-keyring"));
        assert_eq!(settings.active_config().api_key, "sk-test-in-keyring");
    }

    #[test]
    fn plaintext_fallback_round_trips_under_the_legacy_field_names() {
        let mut settings = AiSettings::default();
        settings
            .plaintext_api_keys
            .set(AiProvider::Gemini, "gm-test-fallback".into());
        let json = serde_json::to_value(&settings).unwrap();
        assert_eq!(json["gemini_api_key"], "gm-test-fallback");
        assert!(json.get("openai_api_key").is_none());
        let back: AiSettings = serde_json::from_value(json).unwrap();
        assert_eq!(back.plaintext_api_keys.gemini, "gm-test-fallback");
        assert!(back.api_keys.is_empty());
    }
}
