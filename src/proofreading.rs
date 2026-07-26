//! Shared configuration and result types for optional LanguageTool support.

use serde::{Deserialize, Serialize};
use std::ops::Range;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum LanguageToolMode {
    #[default]
    Disabled,
    LocalManaged,
    ExternalUrl,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum LanguageToolLocalSource {
    #[default]
    Downloaded,
    ExistingDirectory,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum LanguageToolCoverage {
    #[default]
    GrammarOnly,
    SpellingAndGrammar,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LanguageToolSettings {
    pub mode: LanguageToolMode,
    pub local_source: LanguageToolLocalSource,
    /// Empty means Java discovery through JAVA_HOME, PATH and platform paths.
    pub java_path: String,
    pub existing_directory: String,
    pub external_url: String,
    pub coverage: LanguageToolCoverage,
    pub automatic_check: bool,
}

impl Default for LanguageToolSettings {
    fn default() -> Self {
        Self {
            mode: LanguageToolMode::Disabled,
            local_source: LanguageToolLocalSource::Downloaded,
            java_path: String::new(),
            existing_directory: String::new(),
            external_url: String::new(),
            coverage: LanguageToolCoverage::GrammarOnly,
            automatic_check: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LanguageToolState {
    #[default]
    Disabled,
    NotInstalled,
    Stopped,
    Starting,
    Ready,
    Installing,
    Error,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LanguageToolStatus {
    pub state: LanguageToolState,
    pub version: Option<String>,
    pub detail: Option<String>,
    pub progress: Option<f32>,
}

impl Default for LanguageToolStatus {
    fn default() -> Self {
        Self {
            state: LanguageToolState::Disabled,
            version: None,
            detail: None,
            progress: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProofreadingCategory {
    Spelling,
    Grammar,
    Typography,
    Style,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProofreadingIssue {
    /// UTF-8 byte range in the exact source text sent with the request.
    pub range: Range<usize>,
    pub category: ProofreadingCategory,
    pub message: String,
    pub rule_id: String,
    pub replacements: Vec<String>,
}

impl ProofreadingIssue {
    pub fn is_spelling(&self) -> bool {
        self.category == ProofreadingCategory::Spelling
    }
}
