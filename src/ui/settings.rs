//! Settings persisted in `<config>/settings.json` and the lightweight,
//! restorable application session persisted separately in `session.json`.

use super::calendar_view::CalendarRange;
use super::compose::ComposeInit;
use super::settings_view::SettingsTab;
use super::state::MainView;
use crate::model::{AccountId, IcalSubscription, LastAction, MessageRef};
use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

/// Active theme mode, controlled by the top-bar toggle button.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ThemeMode {
    #[default]
    Dark,
    Light,
}

/// Preset palette used as the starting point for a custom theme. `Manual`
/// indicates that at least one color was changed after applying a preset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ThemePreset {
    Manual,
    #[default]
    OneDark,
    OneLight,
    NordDark,
    NordLight,
    Dracula,
    DraculaAlucard,
    GruvboxDark,
    GruvboxLight,
    TokyoNight,
    TokyoDay,
    CatppuccinMocha,
    CatppuccinLatte,
    KanagawaWave,
    KanagawaLotus,
    RosePine,
    RosePineDawn,
    EverforestDark,
    EverforestLight,
    SolarizedDark,
    SolarizedLight,
    GithubDark,
    GithubLight,
}

impl ThemePreset {
    pub const DARK: &'static [Self] = &[
        Self::OneDark,
        Self::NordDark,
        Self::Dracula,
        Self::GruvboxDark,
        Self::TokyoNight,
        Self::CatppuccinMocha,
        Self::KanagawaWave,
        Self::RosePine,
        Self::EverforestDark,
        Self::SolarizedDark,
        Self::GithubDark,
    ];

    pub const LIGHT: &'static [Self] = &[
        Self::OneLight,
        Self::NordLight,
        Self::DraculaAlucard,
        Self::GruvboxLight,
        Self::TokyoDay,
        Self::CatppuccinLatte,
        Self::KanagawaLotus,
        Self::RosePineDawn,
        Self::EverforestLight,
        Self::SolarizedLight,
        Self::GithubLight,
    ];

    pub fn for_mode(mode: ThemeMode) -> &'static [Self] {
        match mode {
            ThemeMode::Dark => Self::DARK,
            ThemeMode::Light => Self::LIGHT,
        }
    }

    pub fn mode(self) -> ThemeMode {
        match self {
            Self::OneLight
            | Self::NordLight
            | Self::DraculaAlucard
            | Self::GruvboxLight
            | Self::TokyoDay
            | Self::CatppuccinLatte
            | Self::KanagawaLotus
            | Self::RosePineDawn
            | Self::EverforestLight
            | Self::SolarizedLight
            | Self::GithubLight => ThemeMode::Light,
            Self::Manual
            | Self::OneDark
            | Self::NordDark
            | Self::Dracula
            | Self::GruvboxDark
            | Self::TokyoNight
            | Self::CatppuccinMocha
            | Self::KanagawaWave
            | Self::RosePine
            | Self::EverforestDark
            | Self::SolarizedDark
            | Self::GithubDark => ThemeMode::Dark,
        }
    }

    pub fn with_mode(self, mode: ThemeMode) -> Self {
        match (self, mode) {
            (Self::Manual, _) => Self::Manual,
            (Self::OneDark | Self::OneLight, ThemeMode::Dark) => Self::OneDark,
            (Self::OneDark | Self::OneLight, ThemeMode::Light) => Self::OneLight,
            (Self::NordDark | Self::NordLight, ThemeMode::Dark) => Self::NordDark,
            (Self::NordDark | Self::NordLight, ThemeMode::Light) => Self::NordLight,
            (Self::Dracula | Self::DraculaAlucard, ThemeMode::Dark) => Self::Dracula,
            (Self::Dracula | Self::DraculaAlucard, ThemeMode::Light) => Self::DraculaAlucard,
            (Self::GruvboxDark | Self::GruvboxLight, ThemeMode::Dark) => Self::GruvboxDark,
            (Self::GruvboxDark | Self::GruvboxLight, ThemeMode::Light) => Self::GruvboxLight,
            (Self::TokyoNight | Self::TokyoDay, ThemeMode::Dark) => Self::TokyoNight,
            (Self::TokyoNight | Self::TokyoDay, ThemeMode::Light) => Self::TokyoDay,
            (Self::CatppuccinMocha | Self::CatppuccinLatte, ThemeMode::Dark) => {
                Self::CatppuccinMocha
            }
            (Self::CatppuccinMocha | Self::CatppuccinLatte, ThemeMode::Light) => {
                Self::CatppuccinLatte
            }
            (Self::KanagawaWave | Self::KanagawaLotus, ThemeMode::Dark) => Self::KanagawaWave,
            (Self::KanagawaWave | Self::KanagawaLotus, ThemeMode::Light) => Self::KanagawaLotus,
            (Self::RosePine | Self::RosePineDawn, ThemeMode::Dark) => Self::RosePine,
            (Self::RosePine | Self::RosePineDawn, ThemeMode::Light) => Self::RosePineDawn,
            (Self::EverforestDark | Self::EverforestLight, ThemeMode::Dark) => Self::EverforestDark,
            (Self::EverforestDark | Self::EverforestLight, ThemeMode::Light) => {
                Self::EverforestLight
            }
            (Self::SolarizedDark | Self::SolarizedLight, ThemeMode::Dark) => Self::SolarizedDark,
            (Self::SolarizedDark | Self::SolarizedLight, ThemeMode::Light) => Self::SolarizedLight,
            (Self::GithubDark | Self::GithubLight, ThemeMode::Dark) => Self::GithubDark,
            (Self::GithubDark | Self::GithubLight, ThemeMode::Light) => Self::GithubLight,
        }
    }

    pub fn default_for_mode(mode: ThemeMode) -> Self {
        match mode {
            ThemeMode::Dark => Self::OneDark,
            ThemeMode::Light => Self::OneLight,
        }
    }

    pub fn palette(self) -> CustomThemePalette {
        let colors = match self {
            Self::Manual | Self::OneDark => [
                0x282c34, 0x21252b, 0x2c313c, 0xabb2bf, 0x7f848e, 0x323740, 0x61afef, 0x98c379,
                0xe5c07b, 0xe06c75,
            ],
            Self::OneLight => [
                0xfafafa, 0xf0f0f0, 0xe5e5e6, 0x383a42, 0x696c77, 0xd3d3d3, 0x4078f2, 0x50a14f,
                0xc18401, 0xe45649,
            ],
            Self::NordDark => [
                0x2e3440, 0x3b4252, 0x434c5e, 0xd8dee9, 0x9aa5b5, 0x4c566a, 0x88c0d0, 0xa3be8c,
                0xebcb8b, 0xbf616a,
            ],
            Self::NordLight => [
                0xeceff4, 0xe5e9f0, 0xd8dee9, 0x2e3440, 0x4c566a, 0xc8d0dc, 0x5e81ac, 0x4f772d,
                0xb36b00, 0xbf616a,
            ],
            Self::Dracula => [
                0x282a36, 0x21222c, 0x44475a, 0xf8f8f2, 0x9aa0b5, 0x44475a, 0xbd93f9, 0x50fa7b,
                0xf1fa8c, 0xff5555,
            ],
            Self::DraculaAlucard => [
                0xf8f8f2, 0xeeeeee, 0xe1e1e1, 0x282a36, 0x6272a4, 0xd0d0d0, 0x7c3aed, 0x1b8a3b,
                0xb7791f, 0xd7263d,
            ],
            Self::GruvboxDark => [
                0x282828, 0x1d2021, 0x3c3836, 0xebdbb2, 0xa89984, 0x504945, 0xd79921, 0xb8bb26,
                0xfabd2f, 0xfb4934,
            ],
            Self::GruvboxLight => [
                0xfbf1c7, 0xf2e5bc, 0xebdbb2, 0x3c3836, 0x7c6f64, 0xd5c4a1, 0x076678, 0x79740e,
                0xb57614, 0x9d0006,
            ],
            Self::TokyoNight => [
                0x1a1b26, 0x16161e, 0x24283b, 0xc0caf5, 0x787c99, 0x3b4261, 0x7aa2f7, 0x9ece6a,
                0xe0af68, 0xf7768e,
            ],
            Self::TokyoDay => [
                0xe1e2e7, 0xd5d6db, 0xc4c8da, 0x343b58, 0x6172b0, 0xb4b5c0, 0x2e7de9, 0x587539,
                0x8c6c3e, 0xf52a65,
            ],
            Self::CatppuccinMocha => [
                0x1e1e2e, 0x181825, 0x313244, 0xcdd6f4, 0x9399b2, 0x45475a, 0xcba6f7, 0xa6e3a1,
                0xf9e2af, 0xf38ba8,
            ],
            Self::CatppuccinLatte => [
                0xeff1f5, 0xe6e9ef, 0xdce0e8, 0x4c4f69, 0x8c8fa1, 0xccd0da, 0x8839ef, 0x40a02b,
                0xdf8e1d, 0xd20f39,
            ],
            Self::KanagawaWave => [
                0x1f1f28, 0x16161d, 0x2a2a37, 0xdcd7ba, 0x938aa9, 0x54546d, 0x7e9cd8, 0x98bb6c,
                0xe6c384, 0xe46876,
            ],
            Self::KanagawaLotus => [
                0xf2ecbc, 0xe7dba0, 0xd9c994, 0x545464, 0x8a8980, 0xc9b674, 0x4d699b, 0x6f894e,
                0x77713f, 0xc84053,
            ],
            Self::RosePine => [
                0x191724, 0x1f1d2e, 0x26233a, 0xe0def4, 0x908caa, 0x403d52, 0xc4a7e7, 0x9ccfd8,
                0xf6c177, 0xeb6f92,
            ],
            Self::RosePineDawn => [
                0xfaf4ed, 0xfffaf3, 0xf2e9e1, 0x575279, 0x9893a5, 0xdfdad9, 0x907aa9, 0x56949f,
                0xea9d34, 0xb4637a,
            ],
            Self::EverforestDark => [
                0x2d353b, 0x272e33, 0x343f44, 0xd3c6aa, 0x859289, 0x475258, 0x7fbbb3, 0xa7c080,
                0xdbbc7f, 0xe67e80,
            ],
            Self::EverforestLight => [
                0xfff9e8, 0xf2efdf, 0xe6e2cc, 0x5c6a72, 0x829181, 0xd8d3ba, 0x3a94c5, 0x8da101,
                0xdfa000, 0xf85552,
            ],
            Self::SolarizedDark => [
                0x002b36, 0x073642, 0x0b3b46, 0x93a1a1, 0x657b83, 0x33535b, 0x268bd2, 0x859900,
                0xb58900, 0xdc322f,
            ],
            Self::SolarizedLight => [
                0xfdf6e3, 0xeee8d5, 0xe5dec9, 0x586e75, 0x839496, 0xd5cdb9, 0x268bd2, 0x859900,
                0xb58900, 0xdc322f,
            ],
            Self::GithubDark => [
                0x0d1117, 0x161b22, 0x21262d, 0xc9d1d9, 0x8b949e, 0x30363d, 0x58a6ff, 0x3fb950,
                0xd29922, 0xf85149,
            ],
            Self::GithubLight => [
                0xffffff, 0xf6f8fa, 0xeaeef2, 0x1f2328, 0x656d76, 0xd0d7de, 0x0969da, 0x1a7f37,
                0x9a6700, 0xcf222e,
            ],
        };
        CustomThemePalette::from_colors(colors)
    }
}

/// Deliberately compact color roles: gpui-component's many colors derive from
/// these ten values to keep the palette editor understandable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeColorRole {
    Background,
    Surface,
    SurfaceVariant,
    Foreground,
    MutedForeground,
    Border,
    Primary,
    Success,
    Warning,
    Danger,
}

impl ThemeColorRole {
    pub const ALL: [Self; 10] = [
        Self::Background,
        Self::Surface,
        Self::SurfaceVariant,
        Self::Foreground,
        Self::MutedForeground,
        Self::Border,
        Self::Primary,
        Self::Success,
        Self::Warning,
        Self::Danger,
    ];
}

/// Custom-theme colors stored as `0xRRGGBB`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CustomThemePalette {
    pub background: u32,
    pub surface: u32,
    pub surface_variant: u32,
    pub foreground: u32,
    pub muted_foreground: u32,
    pub border: u32,
    pub primary: u32,
    pub success: u32,
    pub warning: u32,
    pub danger: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct CustomThemeVariant {
    preset: ThemePreset,
    palette: CustomThemePalette,
}

impl CustomThemeVariant {
    fn from_preset(preset: ThemePreset) -> Self {
        Self {
            preset,
            palette: preset.palette(),
        }
    }
}

impl CustomThemePalette {
    fn from_colors(colors: [u32; 10]) -> Self {
        let [background, surface, surface_variant, foreground, muted_foreground, border, primary, success, warning, danger] =
            colors;
        Self {
            background,
            surface,
            surface_variant,
            foreground,
            muted_foreground,
            border,
            primary,
            success,
            warning,
            danger,
        }
    }

    pub fn color(self, role: ThemeColorRole) -> u32 {
        match role {
            ThemeColorRole::Background => self.background,
            ThemeColorRole::Surface => self.surface,
            ThemeColorRole::SurfaceVariant => self.surface_variant,
            ThemeColorRole::Foreground => self.foreground,
            ThemeColorRole::MutedForeground => self.muted_foreground,
            ThemeColorRole::Border => self.border,
            ThemeColorRole::Primary => self.primary,
            ThemeColorRole::Success => self.success,
            ThemeColorRole::Warning => self.warning,
            ThemeColorRole::Danger => self.danger,
        }
    }

    pub fn set_color(&mut self, role: ThemeColorRole, color: u32) {
        match role {
            ThemeColorRole::Background => self.background = color,
            ThemeColorRole::Surface => self.surface = color,
            ThemeColorRole::SurfaceVariant => self.surface_variant = color,
            ThemeColorRole::Foreground => self.foreground = color,
            ThemeColorRole::MutedForeground => self.muted_foreground = color,
            ThemeColorRole::Border => self.border = color,
            ThemeColorRole::Primary => self.primary = color,
            ThemeColorRole::Success => self.success = color,
            ThemeColorRole::Warning => self.warning = color,
            ThemeColorRole::Danger => self.danger = color,
        }
    }
}

impl Default for CustomThemePalette {
    fn default() -> Self {
        ThemePreset::OneDark.palette()
    }
}

/// Which renderer the message viewer uses for the body. `Blitz`
/// (`Faithful`, the default) rasterizes the original HTML with the Blitz
/// engine (see `ui/blitz_body.rs`); `Markdown` uses the converted markdown
/// pipeline; `Source` shows the raw HTML as code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum BodyViewMode {
    #[default]
    Blitz,
    Markdown,
    Source,
}

/// Last presentation used by the calendar view. `Calendar` is the default
/// when no explicit choice has been made.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum CalendarLayout {
    List,
    #[default]
    Calendar,
}

/// Language preference. `System` resolves to the OS locale at startup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum LanguageChoice {
    #[default]
    System,
    English,
    French,
}

impl LanguageChoice {
    /// Resolve to the locale identifier shared by Aviary and gpui-component.
    pub fn to_lang_id(self) -> &'static str {
        match self {
            Self::English => "en",
            Self::French => "fr",
            Self::System => detect_system_language(),
        }
    }
}

fn detect_system_language() -> &'static str {
    if let Some(locale) = sys_locale::get_locale() {
        let lower = locale.to_ascii_lowercase();
        if lower.starts_with("fr") {
            return "fr";
        }
        if lower.starts_with("en") {
            return "en";
        }
    }
    for var in ["LC_ALL", "LC_MESSAGES", "LANG"] {
        if let Ok(val) = std::env::var(var) {
            let lower = val.to_ascii_lowercase();
            if lower.starts_with("fr") {
                return "fr";
            }
            if lower.starts_with("en") {
                return "en";
            }
        }
    }
    "en"
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Settings {
    #[serde(flatten)]
    pub global: GlobalSettings,
    pub accounts: HashMap<AccountId, AccountSettings>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct AppSession {
    pub main_view: MainView,
    pub mailbox: MailboxSession,
    pub sender_history_expanded: bool,
    pub contacts: ContactsSession,
    pub calendar: CalendarSession,
    pub kanban_preview: Option<(AccountId, String)>,
    pub settings_tab: SettingsTab,
    pub inline_reply: Option<InlineReplySession>,
    /// Composers detached into their own OS windows. Inline composers live in
    /// `mailbox.tabs` so their exact ordering is retained.
    pub detached_composes: Vec<ComposeInit>,
    pub event_composes: Vec<EventComposeSession>,
}

impl AppSession {
    fn path() -> Option<PathBuf> {
        directories::ProjectDirs::from("be", "acetics", "aviary")
            .map(|d| d.config_dir().join("session.json"))
    }

    pub(crate) fn load() -> Self {
        let Some(path) = Self::path() else {
            return Self::default();
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
                log::warn!("unreadable session.json ({e:#}); starting fresh");
                let _ = std::fs::remove_file(&path);
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    /// Writes an already-serialized session atomically (temp file + rename) —
    /// a kill mid-write must not destroy the previous session. 0600 like
    /// settings.json: the snapshot contains draft contents and message ids.
    pub(crate) fn store(json: &[u8]) {
        let Some(path) = Self::path() else { return };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let tmp = path.with_extension("json.tmp");
        let written = write_settings_file(&tmp, json).and_then(|_| std::fs::rename(&tmp, &path));
        if let Err(e) = written {
            log::warn!("failed to save session: {e:#}");
        }
    }

    pub(crate) fn remove_file() {
        if let Some(path) = Self::path() {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// How search results are ordered. Persisted alongside the scope, for the same
/// reason: it is a habit, not a per-query decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum MailSearchSort {
    /// Best match first, as a search engine does.
    #[default]
    Relevance,
    /// Newest first, like every other list in the application.
    Date,
}

/// How wide the mail search reaches. Persisted, because it is a habit rather
/// than a per-query choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum MailSearchScope {
    /// The folder currently selected in the tree.
    Folder,
    /// Every folder of the accounts in scope.
    #[default]
    Everywhere,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct MailboxSession {
    pub selected_folder_id: Option<String>,
    pub unified_selected_account: Option<AccountId>,
    pub selected_message: Option<MessageRef>,
    pub search_query: String,
    pub search_history: Vec<String>,
    pub search_scope: MailSearchScope,
    pub search_sort: MailSearchSort,
    pub show_flagged_only: bool,
    pub tag_filters: HashMap<AccountId, HashSet<String>>,
    pub expanded_quoted_sections: HashSet<String>,
    pub sent_messages: HashMap<String, Vec<SentMessageSession>>,
    pub expanded_sent_messages: HashSet<String>,
    pub collapsed_message_sections: HashSet<String>,
    /// Conversation groups left expanded, as `(account, conversation)` pairs.
    /// Absent from sessions written before conversation grouping existed.
    #[serde(default)]
    pub expanded_conversations: HashSet<(AccountId, String)>,
    pub tabs: Vec<SessionViewerTab>,
    pub active_tab: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) enum SessionViewerTab {
    Message(MessageRef),
    Compose(Box<ComposeInit>),
}

/// Session metadata for a reply/forward card. Its body is referenced by id
/// and restored from SQLite like every other message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct SentMessageSession {
    pub action: LastAction,
    pub message: MessageRef,
    pub sent_id: Option<String>,
    pub internet_message_id: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct ContactsSession {
    pub selected: Option<String>,
    pub query: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct CalendarSession {
    pub range: CalendarRange,
    pub anchor: Option<NaiveDate>,
    pub selected: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct InlineReplySession {
    pub displayed_message_id: String,
    pub reply_target_id: String,
    pub compose: ComposeInit,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct EventComposeSession {
    pub detached: bool,
    pub account_id: AccountId,
    pub subject: String,
    pub location: String,
    pub attendees: String,
    pub description: String,
    pub start_date: NaiveDate,
    pub start_time: String,
    pub end_date: NaiveDate,
    pub end_time: String,
    pub all_day: bool,
    pub online_meeting: bool,
    pub mode: EventComposeSessionMode,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) enum EventComposeSessionMode {
    Create,
    Edit { event_id: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GlobalSettings {
    pub theme_mode: ThemeMode,
    /// Enables the custom palette. Light/dark mode remains in `theme_mode`, in
    /// particular so Blitz receives the correct ColorScheme.
    #[serde(default)]
    pub custom_theme_enabled: bool,
    #[serde(default)]
    pub theme_preset: ThemePreset,
    #[serde(default)]
    pub custom_theme_palette: CustomThemePalette,
    /// Custom-theme state is kept independently for each mode so changing
    /// light/dark never reapplies a dark palette in light mode (or loses the
    /// user's edits when switching back).
    #[serde(default)]
    custom_theme_dark_variant: Option<CustomThemeVariant>,
    #[serde(default)]
    custom_theme_light_variant: Option<CustomThemeVariant>,
    pub ui_scale: f32,
    pub body_font_size: f32,
    /// Enables Vim-inspired single-key commands in lists and the reader. They
    /// remain disabled while typing.
    pub vim_keybindings: bool,
    pub show_remote_images: bool,
    /// Forces Inter in incoming HTML bodies, including when the sender
    /// declares a custom font family.
    #[serde(default, alias = "force_uniform_fonts")]
    pub force_uniform_font_family: bool,
    /// Forces `body_font_size` in incoming HTML bodies, including when the
    /// sender declares custom font sizes.
    #[serde(default)]
    pub force_uniform_font_size: bool,
    /// Makes "Reply all" the labelled primary reader action and moves
    /// "Reply" to the secondary icon button.
    #[serde(default)]
    pub reply_all_primary: bool,
    /// Keeps faithful HTML previews on a browser-like light canvas even when
    /// the application theme is dark.
    #[serde(default)]
    pub force_light_email_preview: bool,
    /// Opens the main window maximized.
    ///
    /// Normally a window manager's own rule would decide this, but under
    /// Wayland gpui sends the initial `wl_surface.commit` *before*
    /// `xdg_toplevel.set_app_id`: the compositor has to answer that commit
    /// with a size while the window still has no app id, so any rule matching
    /// on it — niri's `open-maximized`, and equally a Sway or Hyprland rule —
    /// is evaluated against nothing and never fires. Asking for the maximized
    /// state ourselves goes through `set_maximized` instead, which no rule
    /// matching has to succeed for.
    #[serde(default)]
    pub start_maximized: bool,
    /// Splits quoted history in an email body into collapsible sub-blocks.
    /// Quoted fragments are collapsed on first display.
    pub collapse_quoted_messages: bool,
    /// Folds the message list into one row per conversation. On by default,
    /// as in every mainstream client; search results stay flat regardless.
    #[serde(default = "default_group_by_conversation")]
    pub group_by_conversation: bool,
    pub notifications_enabled: bool,
    pub tray_enabled: bool,
    /// Enables an undo window before common mutations (move, delete, tag,
    /// read state, and follow-up state).
    #[serde(default)]
    pub action_delay_enabled: bool,
    /// Duration of the undo window for common mutations.
    #[serde(default = "default_action_delay_secs")]
    pub action_delay_secs: u32,
    /// Delay before submitting an outgoing message to the provider. During
    /// this window, the send notification offers an undo action.
    pub send_delay_secs: u32,
    /// Preferred visibility of optional recipient fields in new compositions.
    /// A draft that already contains addresses still forces the corresponding
    /// field open.
    #[serde(default)]
    pub compose_show_cc: bool,
    #[serde(default)]
    pub compose_show_bcc: bool,
    pub azure_client_id: String,
    pub azure_tenant: String,
    pub google_client_id: String,
    pub google_client_secret: String,
    /// Writing assistant configuration (provider, templates, and API access).
    pub ai: crate::ai::AiSettings,
    /// Optional self-hosted or external LanguageTool proofreading service.
    #[serde(default)]
    pub languagetool: crate::proofreading::LanguageToolSettings,
    /// Last account used as the contextual sender / settings account. Mail
    /// navigation and the Kanban board itself are always unified.
    pub last_account_id: Option<String>,
    /// Account used for new content when no more specific context (such as a
    /// selected mailbox folder) identifies the sender/calendar owner.
    #[serde(default)]
    pub default_account_id: Option<String>,
    /// Accounts unchecked in the unified navigation. Exclusion is persisted
    /// instead of inclusion so a newly-added account participates by default.
    pub unified_hidden_account_ids: Vec<String>,
    /// Calendars hidden from the calendar view. Kept separate from the mail
    /// scope so hiding a calendar never removes the account from the inbox.
    pub calendar_hidden_account_ids: Vec<String>,
    /// Read-only external iCalendar subscriptions displayed alongside the
    /// provider calendars.
    #[serde(default)]
    pub ical_subscriptions: Vec<IcalSubscription>,
    /// Monotonic local id allocator for subscriptions.
    #[serde(default)]
    pub ical_subscription_seq: u64,
    /// Last calendar presentation selected by the user.
    pub calendar_layout: CalendarLayout,
    /// Number of days covered by the calendar list's "upcoming" sub-mode.
    #[serde(default = "default_calendar_upcoming_days")]
    pub calendar_upcoming_days: u32,
    /// Number of week rows fitting the scrolling calendar grid's viewport
    /// (5 ≈ one month, 9 ≈ two months).
    #[serde(default = "default_calendar_grid_weeks")]
    pub calendar_grid_weeks: u32,
    /// Continuous scrolling in the calendar grid (months flow together).
    /// When disabled, the grid pages one month at a time.
    #[serde(default = "default_calendar_infinite_scroll")]
    pub calendar_infinite_scroll: bool,
    /// Account groups explicitly expanded in the unified folder tree. Empty
    /// by default so a multi-account startup remains compact.
    pub expanded_folder_account_ids: Vec<String>,
    /// Whether the cross-account favourite-folders section is collapsed.
    pub favorite_folders_collapsed: bool,
    /// User-chosen display order of accounts (raw `AccountId.0` strings).
    pub account_order: Vec<String>,
    pub language: LanguageChoice,
    /// Default renderer used for incoming HTML message bodies.
    pub body_view_mode: BodyViewMode,
    /// Maximum width of the email preview container in logical pixels.
    /// `0.0` means disabled.
    pub preview_max_width: f32,
    /// Logical email-cache quota in mebibytes.
    pub mail_cache_limit_mb: u64,
    /// Senders whose incoming mail is moved to the junk folder on arrival,
    /// stored as bare lowercase addresses.
    ///
    /// Global rather than per-account on purpose: unlike a signature or a
    /// kanban column, this list is applied by Aviary and not by the server, so
    /// its natural unit is the person using Aviary, not one of their mailboxes.
    /// Blocking a sender once should hold wherever they write.
    #[serde(default)]
    pub blocked_senders: Vec<String>,
}

/// Presentation options passed together to the various renderers for
/// incoming bodies (main reader, conversation, and quotes).
#[derive(Debug, Clone, Copy)]
pub(crate) struct MailBodyOptions {
    pub show_remote_images: bool,
    pub force_uniform_font_family: bool,
    pub force_uniform_font_size: bool,
    pub force_light_theme: bool,
    pub font_size: f32,
}

/// Form a blocked sender is stored and compared in: the bare address, lowercased.
///
/// The display name is dropped on purpose — it is chosen by the sender, and a
/// spammer changing "Contact A" to "Contact B" would otherwise walk straight
/// past the list. Returns `None` for anything with no address in it, which is
/// what keeps an unparseable `From` from blocking every other unparseable one.
pub(crate) fn normalized_sender(from: &str) -> Option<String> {
    super::util::extract_email(from).map(|address| address.trim().to_lowercase())
}

impl GlobalSettings {
    /// Whether mail from this `From` header is blocked. Takes the raw header,
    /// so every call site normalizes the same way.
    pub(crate) fn sender_is_blocked(&self, from: &str) -> bool {
        normalized_sender(from).is_some_and(|address| self.blocked_senders.contains(&address))
    }

    /// Adds a sender to the block list, reporting whether it was new.
    pub(crate) fn block_sender(&mut self, from: &str) -> bool {
        let Some(address) = normalized_sender(from) else {
            return false;
        };
        if self.blocked_senders.contains(&address) {
            return false;
        }
        self.blocked_senders.push(address);
        self.blocked_senders.sort();
        true
    }

    /// Removes a sender from the block list, reporting whether it was there.
    pub(crate) fn unblock_sender(&mut self, from: &str) -> bool {
        let Some(address) = normalized_sender(from) else {
            return false;
        };
        let before = self.blocked_senders.len();
        self.blocked_senders.retain(|blocked| blocked != &address);
        self.blocked_senders.len() != before
    }

    pub fn uses_custom_theme(&self) -> bool {
        self.custom_theme_enabled
    }

    fn custom_theme_variant(&self, mode: ThemeMode) -> Option<CustomThemeVariant> {
        match mode {
            ThemeMode::Dark => self.custom_theme_dark_variant,
            ThemeMode::Light => self.custom_theme_light_variant,
        }
    }

    fn set_custom_theme_variant(&mut self, mode: ThemeMode, variant: CustomThemeVariant) {
        match mode {
            ThemeMode::Dark => self.custom_theme_dark_variant = Some(variant),
            ThemeMode::Light => self.custom_theme_light_variant = Some(variant),
        }
    }

    fn remember_current_custom_theme(&mut self) {
        self.set_custom_theme_variant(
            self.theme_mode,
            CustomThemeVariant {
                preset: self.theme_preset,
                palette: self.custom_theme_palette,
            },
        );
    }

    fn activate_custom_theme_variant(&mut self, mode: ThemeMode, variant: CustomThemeVariant) {
        self.custom_theme_enabled = true;
        self.theme_mode = mode;
        self.theme_preset = variant.preset;
        self.custom_theme_palette = variant.palette;
        self.set_custom_theme_variant(mode, variant);
    }

    pub(crate) fn select_custom_theme_preset(&mut self, preset: ThemePreset) {
        self.activate_custom_theme_variant(preset.mode(), CustomThemeVariant::from_preset(preset));
    }

    pub(crate) fn edit_custom_theme_color(&mut self, role: ThemeColorRole, color: u32) {
        // Once a preset becomes Manual its paired preset can no longer be
        // inferred. Seed the other mode before discarding that information.
        if self.theme_preset != ThemePreset::Manual {
            let other_mode = match self.theme_mode {
                ThemeMode::Dark => ThemeMode::Light,
                ThemeMode::Light => ThemeMode::Dark,
            };
            if self.custom_theme_variant(other_mode).is_none() {
                let paired = self.theme_preset.with_mode(other_mode);
                self.set_custom_theme_variant(other_mode, CustomThemeVariant::from_preset(paired));
            }
        }

        self.custom_theme_palette.set_color(role, color);
        self.custom_theme_enabled = true;
        self.theme_preset = ThemePreset::Manual;
        self.remember_current_custom_theme();
    }

    pub(crate) fn select_custom_theme_mode(&mut self, mode: ThemeMode) {
        if self.custom_theme_enabled {
            self.remember_current_custom_theme();
        }

        let variant = self.custom_theme_variant(mode).unwrap_or_else(|| {
            let paired = self.theme_preset.with_mode(mode);
            let preset = if paired == ThemePreset::Manual {
                ThemePreset::default_for_mode(mode)
            } else {
                paired
            };
            CustomThemeVariant::from_preset(preset)
        });
        self.activate_custom_theme_variant(mode, variant);
    }

    pub(crate) fn select_builtin_theme_mode(&mut self, mode: ThemeMode) {
        if self.custom_theme_enabled {
            self.remember_current_custom_theme();
        }
        self.theme_mode = mode;
        self.custom_theme_enabled = false;
    }

    pub(crate) fn mail_body_options(&self) -> MailBodyOptions {
        MailBodyOptions {
            show_remote_images: self.show_remote_images,
            force_uniform_font_family: self.force_uniform_font_family,
            force_uniform_font_size: self.force_uniform_font_size,
            force_light_theme: self.force_light_email_preview,
            font_size: self.body_font_size.clamp(9.0, 32.0),
        }
    }

    /// Effective delay for common actions. The stored duration is retained
    /// while the option is disabled so it can be restored when re-enabled.
    pub(crate) fn effective_action_delay_secs(&self) -> u32 {
        if self.action_delay_enabled {
            self.action_delay_secs.clamp(1, 300)
        } else {
            0
        }
    }
}

impl Default for GlobalSettings {
    fn default() -> Self {
        Self {
            theme_mode: ThemeMode::default(),
            custom_theme_enabled: false,
            theme_preset: ThemePreset::default(),
            custom_theme_palette: CustomThemePalette::default(),
            custom_theme_dark_variant: None,
            custom_theme_light_variant: None,
            ui_scale: 1.0,
            body_font_size: 14.0,
            vim_keybindings: false,
            show_remote_images: false,
            force_uniform_font_family: false,
            force_uniform_font_size: false,
            reply_all_primary: false,
            force_light_email_preview: false,
            start_maximized: false,
            collapse_quoted_messages: true,
            group_by_conversation: default_group_by_conversation(),
            notifications_enabled: true,
            tray_enabled: true,
            action_delay_enabled: false,
            action_delay_secs: default_action_delay_secs(),
            send_delay_secs: 10,
            compose_show_cc: false,
            compose_show_bcc: false,
            azure_client_id: String::new(),
            azure_tenant: default_tenant(),
            google_client_id: String::new(),
            google_client_secret: String::new(),
            ai: crate::ai::AiSettings::default(),
            languagetool: crate::proofreading::LanguageToolSettings::default(),
            last_account_id: None,
            default_account_id: None,
            unified_hidden_account_ids: Vec::new(),
            calendar_hidden_account_ids: Vec::new(),
            ical_subscriptions: Vec::new(),
            ical_subscription_seq: 0,
            calendar_layout: CalendarLayout::default(),
            calendar_upcoming_days: default_calendar_upcoming_days(),
            calendar_grid_weeks: default_calendar_grid_weeks(),
            calendar_infinite_scroll: true,
            expanded_folder_account_ids: Vec::new(),
            favorite_folders_collapsed: false,
            account_order: Vec::new(),
            language: LanguageChoice::default(),
            body_view_mode: BodyViewMode::default(),
            preview_max_width: 0.0,
            mail_cache_limit_mb: 500,
            blocked_senders: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountSettings {
    pub fetch_limit: usize,
    pub sender_history_limit: usize,
    pub auto_refresh_secs: u32,
    /// Messages kept at the top of the message list, independently from the
    /// provider's star/follow-up flag. Pinning is an Aviary presentation
    /// preference because the public provider APIs don't expose a common
    /// native pin state (Microsoft Graph only exposes `flag`).
    pub pinned_message_ids: Vec<String>,
    /// Messages hidden from the list until their deadline, newest deadline
    /// last. Like [`Self::pinned_message_ids`] this is an Aviary-side state:
    /// no provider exposes a common "snooze" the three backends could share,
    /// and keeping the message where it is means its id never changes — which
    /// an IMAP move would not guarantee, `UID MOVE` minting a new one and the
    /// COPY+EXPUNGE fallback not even reporting it.
    #[serde(default)]
    pub snoozed_messages: Vec<SnoozedMessage>,
    /// Provider folder ids surfaced in the favourites section.
    pub pinned_folder_ids: Vec<String>,
    /// Folder nodes explicitly expanded in the navigation tree. New branches
    /// remain collapsed until the user opens them.
    pub expanded_folder_ids: Vec<String>,
    /// Tags shown as columns in the kanban view, in display order.
    pub kanban_tag_columns: Vec<String>,
    /// User-defined signatures (rich blocks, HTML, and images).
    pub signatures: Vec<crate::model::Signature>,
    /// Rich email templates specific to this account/mailbox.
    pub templates: Vec<crate::model::Template>,
    /// One-click recipes available for messages belonging to this account.
    #[serde(default)]
    pub quick_actions: Vec<QuickAction>,
    pub signature_seq: i64,
    pub template_seq: i64,
    #[serde(default)]
    pub quick_action_seq: i64,
    /// User-chosen display name override. Empty ⇒ provider value.
    pub display_name_override: String,
    /// User-chosen color override packed as `0xRRGGBB`.
    pub color_override: Option<u32>,
}

/// A message put off until `until`, hidden from the list in the meantime.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SnoozedMessage {
    pub id: String,
    pub until: chrono::DateTime<chrono::Utc>,
}

impl Default for AccountSettings {
    fn default() -> Self {
        Self {
            fetch_limit: 20,
            sender_history_limit: 5,
            auto_refresh_secs: 60,
            pinned_message_ids: Vec::new(),
            snoozed_messages: Vec::new(),
            pinned_folder_ids: Vec::new(),
            expanded_folder_ids: Vec::new(),
            kanban_tag_columns: Vec::new(),
            signatures: Vec::new(),
            templates: Vec::new(),
            quick_actions: Vec::new(),
            signature_seq: 0,
            template_seq: 0,
            quick_action_seq: 0,
            display_name_override: String::new(),
            color_override: None,
        }
    }
}

/// Account-local one-click mail recipe. Provider-owned folder and tag ids are
/// intentionally stored here instead of using display names: rename events can
/// update them without making execution ambiguous.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct QuickAction {
    pub id: i64,
    pub name: String,
    pub icon: QuickActionIcon,
    /// Packed sRGB (`0xRRGGBB`).
    pub color: u32,
    /// At most the first two favorite actions are rendered directly.
    pub favorite: bool,
    pub forward: Option<QuickForward>,
    pub reply: Option<QuickReply>,
    pub add_tags: Vec<String>,
    pub remove_tags: Vec<String>,
    pub mark_read: Option<bool>,
    pub set_flagged: Option<bool>,
    pub move_to_folder_id: Option<String>,
}

impl Default for QuickAction {
    fn default() -> Self {
        Self {
            id: 0,
            name: String::new(),
            icon: QuickActionIcon::Zap,
            color: 0xE5A50A,
            favorite: false,
            forward: None,
            reply: None,
            add_tags: Vec::new(),
            remove_tags: Vec::new(),
            mark_read: None,
            set_flagged: None,
            move_to_folder_id: None,
        }
    }
}

impl QuickAction {
    pub(crate) fn has_steps(&self) -> bool {
        self.forward.is_some()
            || self.reply.is_some()
            || !self.add_tags.is_empty()
            || !self.remove_tags.is_empty()
            || self.mark_read.is_some()
            || self.set_flagged.is_some()
            || self.move_to_folder_id.is_some()
    }

    pub(crate) fn targets_are_disjoint(&self) -> bool {
        !self
            .add_tags
            .iter()
            .any(|tag| self.remove_tags.contains(tag))
    }

    pub(crate) fn sends_at_most_once(&self) -> bool {
        self.forward.is_none() || self.reply.is_none()
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum QuickActionIcon {
    #[default]
    Zap,
    Forward,
    Reply,
    Folder,
    Tag,
    Archive,
}

impl QuickActionIcon {
    pub(crate) fn asset(self) -> &'static str {
        match self {
            Self::Zap => "zap",
            Self::Forward => "forward",
            Self::Reply => "reply",
            Self::Folder => "folder-open",
            Self::Tag => "tag",
            Self::Archive => "archive",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct QuickForward {
    pub to: String,
    pub cc: String,
    pub bcc: String,
    /// Rich prefix placed before the default signature and quoted message.
    pub note_blocks: Vec<crate::blocks::Block>,
    pub note_images: Vec<crate::model::InlineImage>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct QuickReply {
    /// Reply to every original recipient, excluding the current account.
    pub reply_all: bool,
    /// Rich response placed before the default signature and quoted message.
    pub body_blocks: Vec<crate::blocks::Block>,
    pub body_images: Vec<crate::model::InlineImage>,
}

fn default_tenant() -> String {
    "common".to_string()
}

fn default_action_delay_secs() -> u32 {
    5
}

/// Existing installations have no value stored for this; defaulting to `true`
/// rather than `false` means the setting behaves the same on a fresh install
/// and after an update.
fn default_group_by_conversation() -> bool {
    true
}

fn default_calendar_upcoming_days() -> u32 {
    30
}

fn default_calendar_grid_weeks() -> u32 {
    5
}

fn default_calendar_infinite_scroll() -> bool {
    true
}

impl Settings {
    pub fn account_mut(&mut self, id: &AccountId) -> &mut AccountSettings {
        self.accounts.entry(id.clone()).or_default()
    }

    pub fn account_or_default(&self, id: Option<&AccountId>) -> AccountSettings {
        id.and_then(|i| self.accounts.get(i))
            .cloned()
            .unwrap_or_default()
    }

    /// Puts a message off until `until`, replacing any deadline it already had.
    pub(crate) fn snooze_message(
        &mut self,
        account_id: &AccountId,
        id: &str,
        until: DateTime<Utc>,
    ) {
        let snoozed = &mut self.account_mut(account_id).snoozed_messages;
        snoozed.retain(|entry| entry.id != id);
        snoozed.push(SnoozedMessage {
            id: id.to_string(),
            until,
        });
    }

    /// Cancels a message's deadline, reporting whether it had one.
    pub(crate) fn unsnooze_message(&mut self, account_id: &AccountId, id: &str) -> bool {
        let Some(account) = self.accounts.get_mut(account_id) else {
            return false;
        };
        let before = account.snoozed_messages.len();
        account.snoozed_messages.retain(|entry| entry.id != id);
        account.snoozed_messages.len() != before
    }

    pub(crate) fn snoozed_until(&self, account_id: &AccountId, id: &str) -> Option<DateTime<Utc>> {
        self.accounts
            .get(account_id)?
            .snoozed_messages
            .iter()
            .find(|entry| entry.id == id)
            .map(|entry| entry.until)
    }

    /// Deadlines that have come due, cleared from the settings as they are
    /// returned. Draining rather than reading is what makes waking a message
    /// happen once: the caller marks each unread, and a second pass over the
    /// same entries would fight the user marking it read again.
    pub(crate) fn take_due_snoozes(&mut self, now: DateTime<Utc>) -> Vec<(AccountId, String)> {
        let mut due = Vec::new();
        for (account_id, account) in &mut self.accounts {
            account.snoozed_messages.retain(|entry| {
                if entry.until <= now {
                    due.push((account_id.clone(), entry.id.clone()));
                    false
                } else {
                    true
                }
            });
        }
        due
    }

    fn path() -> Option<PathBuf> {
        directories::ProjectDirs::from("be", "acetics", "aviary")
            .map(|d| d.config_dir().join("settings.json"))
    }

    pub fn load() -> Self {
        let Some(path) = Self::path() else {
            return Self::default();
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => Self::from_json(&text).unwrap_or_else(|e| {
                log::warn!("unreadable settings.json ({e:#}); using defaults");
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    fn from_json(text: &str) -> serde_json::Result<Self> {
        let mut value: serde_json::Value = serde_json::from_str(text)?;
        // Before typography controls were independent, `force_uniform_fonts`
        // enabled both the family and size overrides. Preserve that choice on
        // first load while the serde alias above maps the legacy key to the
        // new family field.
        if let Some(root) = value.as_object_mut() {
            if !root.contains_key("force_uniform_font_size") {
                if let Some(enabled) = root.get("force_uniform_fonts").and_then(|v| v.as_bool()) {
                    root.insert(
                        "force_uniform_font_size".to_string(),
                        serde_json::Value::Bool(enabled),
                    );
                }
            }
        }
        serde_json::from_value(value)
    }

    pub fn save(&self) {
        let Some(path) = Self::path() else { return };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match serde_json::to_string_pretty(self) {
            Ok(json) => {
                if let Err(e) = write_settings_file(&path, json.as_bytes()) {
                    log::warn!("failed to save settings: {e:#}");
                }
            }
            Err(e) => log::warn!("failed to serialize settings: {e:#}"),
        }
    }
}

#[cfg(unix)]
fn write_settings_file(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::fs::PermissionsExt;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)?;
    // `mode` applies only at creation; also protect settings files created by
    // older Aviary versions before writing a secret subscription URL.
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    file.write_all(bytes)
}

#[cfg(not(unix))]
fn write_settings_file(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    std::fs::write(path, bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_rendering_settings() {
        assert_eq!(BodyViewMode::default(), BodyViewMode::Blitz);
        assert_eq!(CalendarLayout::default(), CalendarLayout::Calendar);
        assert!(!GlobalSettings::default().force_uniform_font_family);
        assert!(!GlobalSettings::default().force_uniform_font_size);
        assert!(!GlobalSettings::default().reply_all_primary);
        assert!(!GlobalSettings::default().force_light_email_preview);
    }

    /// The display name is the sender's to choose; only the address is
    /// matched, and case never distinguishes two mailboxes.
    #[test]
    fn blocking_matches_the_address_whatever_the_display_name() {
        let mut settings = GlobalSettings::default();
        assert!(settings.block_sender("Contact A <contact@example.com>"));

        assert!(settings.sender_is_blocked("Someone Else <CONTACT@Example.com>"));
        assert!(settings.sender_is_blocked("contact@example.com"));
        assert!(!settings.sender_is_blocked("other@example.com"));
    }

    /// A `From` with no address in it must not become a wildcard: two
    /// unparseable headers are not the same sender.
    #[test]
    fn an_addressless_sender_is_never_blocked() {
        let mut settings = GlobalSettings::default();

        assert!(!settings.block_sender("Contact A"));
        assert!(settings.blocked_senders.is_empty());
        assert!(!settings.sender_is_blocked("Contact B"));
    }

    #[test]
    fn blocking_is_idempotent_and_reversible() {
        let mut settings = GlobalSettings::default();
        assert!(settings.block_sender("contact@example.com"));
        assert!(!settings.block_sender("Contact A <contact@example.com>"));
        assert_eq!(settings.blocked_senders.len(), 1);

        assert!(settings.unblock_sender("CONTACT@EXAMPLE.COM"));
        assert!(settings.blocked_senders.is_empty());
        assert!(!settings.unblock_sender("contact@example.com"));
    }

    #[test]
    fn legacy_uniform_typography_setting_enables_both_controls() {
        let mut json = serde_json::to_value(Settings::default()).expect("serialization");
        let root = json.as_object_mut().expect("settings object");
        root.remove("force_uniform_font_family");
        root.remove("force_uniform_font_size");
        root.insert(
            "force_uniform_fonts".to_string(),
            serde_json::Value::Bool(true),
        );

        let loaded =
            Settings::from_json(&serde_json::to_string(&json).unwrap()).expect("deserialization");

        assert!(loaded.global.force_uniform_font_family);
        assert!(loaded.global.force_uniform_font_size);
    }

    #[test]
    fn existing_settings_may_lack_reply_action_preference() {
        let mut json = serde_json::to_value(Settings::default()).expect("serialization");
        json.as_object_mut()
            .expect("settings object")
            .remove("reply_all_primary");

        let loaded: Settings = serde_json::from_value(json).expect("deserialization");

        assert!(!loaded.global.reply_all_primary);
    }

    #[test]
    fn current_configuration_round_trip() {
        let mut settings = Settings::default();
        settings.global.body_font_size = 16.0;
        settings.global.calendar_layout = CalendarLayout::List;
        settings.global.default_account_id = Some("account@example.com".to_string());
        settings.global.compose_show_cc = true;
        settings.global.compose_show_bcc = true;
        settings.global.force_light_email_preview = true;

        let json = serde_json::to_string(&settings).expect("serialization");
        let loaded: Settings = serde_json::from_str(&json).expect("deserialization");

        assert_eq!(loaded.global.body_font_size, 16.0);
        assert_eq!(loaded.global.calendar_layout, CalendarLayout::List);
        assert!(loaded.global.compose_show_cc);
        assert!(loaded.global.compose_show_bcc);
        assert!(loaded.global.force_light_email_preview);
        assert!(loaded.global.mail_body_options().force_light_theme);
        assert_eq!(
            loaded.global.default_account_id.as_deref(),
            Some("account@example.com")
        );
        assert!(loaded.accounts.is_empty());
    }

    #[test]
    fn quick_action_configuration_round_trips_and_old_accounts_default_empty() {
        let account_id = AccountId("mailbox@example.test".into());
        let mut settings = Settings::default();
        settings
            .account_mut(&account_id)
            .quick_actions
            .push(QuickAction {
                id: 1,
                name: "Route A".into(),
                favorite: true,
                forward: Some(QuickForward {
                    to: "Contact A <contact-a@example.test>".into(),
                    note_blocks: vec![crate::blocks::Block {
                        id: 1,
                        kind: crate::blocks::BlockKind::Paragraph("Synthetic note".into()),
                    }],
                    ..QuickForward::default()
                }),
                add_tags: vec!["tag-a".into()],
                move_to_folder_id: Some("folder-a".into()),
                ..QuickAction::default()
            });
        settings
            .account_mut(&account_id)
            .quick_actions
            .push(QuickAction {
                id: 2,
                name: "Reply A".into(),
                reply: Some(QuickReply {
                    reply_all: true,
                    body_blocks: vec![crate::blocks::Block {
                        id: 1,
                        kind: crate::blocks::BlockKind::Paragraph("Synthetic response".into()),
                    }],
                    ..QuickReply::default()
                }),
                ..QuickAction::default()
            });

        let encoded = serde_json::to_value(&settings).unwrap();
        let loaded: Settings = serde_json::from_value(encoded.clone()).unwrap();
        let action = &loaded.accounts[&account_id].quick_actions[0];
        assert_eq!(action.name, "Route A");
        assert!(action.favorite);
        assert!(action.has_steps());
        assert!(action.targets_are_disjoint());
        assert!(action.sends_at_most_once());
        let reply = &loaded.accounts[&account_id].quick_actions[1];
        assert!(reply.reply.as_ref().is_some_and(|reply| reply.reply_all));
        assert!(reply.sends_at_most_once());

        let mut legacy = encoded;
        let account = legacy["accounts"]["mailbox@example.test"]
            .as_object_mut()
            .unwrap();
        account.remove("quick_actions");
        account.remove("quick_action_seq");
        let loaded: Settings = serde_json::from_value(legacy).unwrap();
        assert!(loaded.accounts[&account_id].quick_actions.is_empty());
        assert_eq!(loaded.accounts[&account_id].quick_action_seq, 0);
    }

    #[test]
    fn existing_settings_may_lack_default_account() {
        let mut json = serde_json::to_value(Settings::default()).expect("serialization");
        json.as_object_mut()
            .and_then(|root| root.remove("default_account_id"));

        let loaded: Settings = serde_json::from_value(json).expect("deserialization");

        assert!(loaded.global.default_account_id.is_none());
    }

    #[test]
    fn existing_settings_may_lack_recipient_visibility() {
        let mut json = serde_json::to_value(Settings::default()).expect("serialization");
        let root = json.as_object_mut().expect("settings object");
        root.remove("compose_show_cc");
        root.remove("compose_show_bcc");

        let loaded: Settings = serde_json::from_value(json).expect("deserialization");

        assert!(!loaded.global.compose_show_cc);
        assert!(!loaded.global.compose_show_bcc);
    }

    #[test]
    fn existing_settings_may_lack_forced_light_email_preview() {
        let mut json = serde_json::to_value(Settings::default()).expect("serialization");
        json.as_object_mut()
            .expect("settings object")
            .remove("force_light_email_preview");

        let loaded: Settings = serde_json::from_value(json).expect("deserialization");

        assert!(!loaded.global.force_light_email_preview);
        assert!(!loaded.global.mail_body_options().force_light_theme);
    }

    /// Off by default: the window manager places windows, and a client that
    /// maximizes itself uninvited fights whatever the user already configured.
    /// The setting exists for the case where that rule cannot fire — see the
    /// field's documentation.
    #[test]
    fn existing_settings_may_lack_start_maximized() {
        assert!(!GlobalSettings::default().start_maximized);

        let mut json = serde_json::to_value(Settings::default()).expect("serialization");
        json.as_object_mut()
            .expect("settings object")
            .remove("start_maximized");

        let loaded: Settings = serde_json::from_value(json).expect("deserialization");

        assert!(!loaded.global.start_maximized);
    }

    #[test]
    fn existing_settings_may_lack_calendar_upcoming_days() {
        let mut json = serde_json::to_value(Settings::default()).expect("serialization");
        let root = json.as_object_mut().expect("settings object");
        root.remove("calendar_upcoming_days");
        root.remove("calendar_grid_weeks");
        root.remove("calendar_infinite_scroll");

        let loaded: Settings = serde_json::from_value(json).expect("deserialization");

        assert_eq!(loaded.global.calendar_upcoming_days, 30);
        assert_eq!(loaded.global.calendar_grid_weeks, 5);
        assert!(loaded.global.calendar_infinite_scroll);
    }

    #[test]
    fn existing_settings_may_lack_ical_subscriptions() {
        let mut json = serde_json::to_value(Settings::default()).expect("serialization");
        let root = json.as_object_mut().expect("settings object");
        root.remove("ical_subscriptions");
        root.remove("ical_subscription_seq");

        let loaded: Settings = serde_json::from_value(json).expect("deserialization");

        assert!(loaded.global.ical_subscriptions.is_empty());
        assert_eq!(loaded.global.ical_subscription_seq, 0);
    }

    #[test]
    fn existing_settings_may_lack_languagetool_configuration() {
        let mut json = serde_json::to_value(Settings::default()).expect("serialization");
        json.as_object_mut()
            .expect("settings object")
            .remove("languagetool");

        let loaded: Settings = serde_json::from_value(json).expect("deserialization");

        assert_eq!(
            loaded.global.languagetool,
            crate::proofreading::LanguageToolSettings::default()
        );
        assert!(loaded.global.languagetool.automatic_check);
    }

    /// The session round-trips through its own session.json (see
    /// `ui::session_store`), not through Settings — whose serialization
    /// must, conversely, no longer contain it.
    #[test]
    fn compose_session_round_trip() {
        let mut session = AppSession {
            main_view: MainView::Calendar,
            ..AppSession::default()
        };
        session
            .mailbox
            .tabs
            .push(SessionViewerTab::Compose(Box::new(ComposeInit {
                compose_id: Some(42),
                pending_send: true,
                from_account_id: Some(AccountId("account@example.test".to_string())),
                to: "Contact A <contact-a@example.test>".to_string(),
                subject: "Persistent draft".to_string(),
                body_kinds: Some(vec![crate::blocks::BlockKind::Paragraph(
                    "Still writing".to_string(),
                )]),
                skip_signature: true,
                ..ComposeInit::default()
            })));
        session.mailbox.active_tab = Some(0);
        session.mailbox.search_history =
            vec!["Project status".to_string(), "Contact A".to_string()];

        let json = serde_json::to_string(&session).expect("serialization");
        let loaded: AppSession = serde_json::from_str(&json).expect("deserialization");

        assert_eq!(loaded.main_view, MainView::Calendar);
        assert_eq!(loaded.mailbox.active_tab, Some(0));
        assert_eq!(
            loaded.mailbox.search_history,
            ["Project status", "Contact A"]
        );
        let SessionViewerTab::Compose(compose) = &loaded.mailbox.tabs[0] else {
            panic!("expected compose tab");
        };
        assert_eq!(compose.compose_id, Some(42));
        assert!(compose.pending_send);

        let settings_json =
            serde_json::to_value(Settings::default()).expect("settings serialization");
        assert!(
            settings_json.get("session").is_none(),
            "settings.json must no longer embed the session"
        );
        assert_eq!(compose.subject, "Persistent draft");
        assert_eq!(compose.to, "Contact A <contact-a@example.test>");
        assert_eq!(
            compose.body_kinds.as_deref(),
            Some(
                &[crate::blocks::BlockKind::Paragraph(
                    "Still writing".to_string()
                )][..]
            )
        );
    }

    #[test]
    fn message_session_contains_only_sqlite_references() {
        let reference = MessageRef {
            account_id: AccountId("account-a".into()),
            id: "message-a".into(),
        };
        let mut session = AppSession::default();
        session.mailbox.selected_message = Some(reference.clone());
        session
            .mailbox
            .tabs
            .push(SessionViewerTab::Message(reference.clone()));
        session.mailbox.sent_messages.insert(
            "source-a".into(),
            vec![SentMessageSession {
                action: LastAction::Replied,
                message: reference,
                sent_id: Some("sent-a".into()),
                internet_message_id: None,
            }],
        );

        let json = serde_json::to_string(&session).expect("session serialization");
        let loaded: AppSession = serde_json::from_str(&json).expect("session deserialization");

        assert_eq!(
            loaded
                .mailbox
                .selected_message
                .as_ref()
                .map(|message| message.id.as_str()),
            Some("message-a")
        );
        assert!(!json.contains("body"));
        assert!(!json.contains("inline_images"));
        assert!(!json.contains("attachments"));
    }

    #[test]
    fn existing_settings_may_lack_custom_theme() {
        let mut json = serde_json::to_value(Settings::default()).expect("serialization");
        let root = json.as_object_mut().expect("settings object");
        root.remove("custom_theme_enabled");
        root.remove("theme_preset");
        root.remove("custom_theme_palette");
        root.remove("custom_theme_dark_variant");
        root.remove("custom_theme_light_variant");

        let loaded: Settings = serde_json::from_value(json).expect("deserialization");

        assert!(!loaded.global.uses_custom_theme());
        assert_eq!(loaded.global.theme_preset, ThemePreset::OneDark);
        assert_eq!(
            loaded.global.custom_theme_palette,
            ThemePreset::OneDark.palette()
        );
    }

    #[test]
    fn custom_theme_round_trip() {
        let mut settings = Settings::default();
        settings.global.custom_theme_enabled = true;
        settings.global.theme_mode = ThemeMode::Light;
        settings.global.theme_preset = ThemePreset::Manual;
        settings
            .global
            .custom_theme_palette
            .set_color(ThemeColorRole::Primary, 0x123456);

        let json = serde_json::to_string(&settings).expect("serialization");
        let loaded: Settings = serde_json::from_str(&json).expect("deserialization");

        assert!(loaded.global.uses_custom_theme());
        assert_eq!(loaded.global.theme_mode, ThemeMode::Light);
        assert_eq!(loaded.global.theme_preset, ThemePreset::Manual);
        assert_eq!(loaded.global.custom_theme_palette.primary, 0x123456);
    }

    #[test]
    fn manual_custom_palette_switches_modes_and_restores_each_variant() {
        let mut global = GlobalSettings::default();
        global.select_custom_theme_preset(ThemePreset::NordDark);
        global.edit_custom_theme_color(ThemeColorRole::Primary, 0x123456);

        global.select_custom_theme_mode(ThemeMode::Light);

        assert_eq!(global.theme_mode, ThemeMode::Light);
        assert_eq!(global.theme_preset, ThemePreset::NordLight);
        assert_eq!(
            global.custom_theme_palette,
            ThemePreset::NordLight.palette()
        );

        global.edit_custom_theme_color(ThemeColorRole::Primary, 0xabcdef);
        global.select_custom_theme_mode(ThemeMode::Dark);

        assert_eq!(global.theme_mode, ThemeMode::Dark);
        assert_eq!(global.theme_preset, ThemePreset::Manual);
        assert_eq!(global.custom_theme_palette.primary, 0x123456);

        global.select_custom_theme_mode(ThemeMode::Light);

        assert_eq!(global.theme_mode, ThemeMode::Light);
        assert_eq!(global.theme_preset, ThemePreset::Manual);
        assert_eq!(global.custom_theme_palette.primary, 0xabcdef);
    }

    #[test]
    fn legacy_manual_custom_palette_gets_a_light_counterpart() {
        let mut settings = Settings::default();
        settings.global.custom_theme_enabled = true;
        settings.global.theme_preset = ThemePreset::Manual;
        settings
            .global
            .custom_theme_palette
            .set_color(ThemeColorRole::Primary, 0x123456);

        let mut json = serde_json::to_value(settings).expect("serialization");
        let root = json.as_object_mut().expect("settings object");
        root.remove("custom_theme_dark_variant");
        root.remove("custom_theme_light_variant");
        let mut loaded: Settings = serde_json::from_value(json).expect("deserialization");

        loaded.global.select_custom_theme_mode(ThemeMode::Light);
        assert_eq!(loaded.global.theme_mode, ThemeMode::Light);
        assert_eq!(
            loaded.global.custom_theme_palette,
            ThemePreset::OneLight.palette()
        );

        loaded.global.select_custom_theme_mode(ThemeMode::Dark);
        assert_eq!(loaded.global.custom_theme_palette.primary, 0x123456);
    }

    #[test]
    fn presets_are_grouped_and_paired_by_mode() {
        assert_eq!(ThemePreset::DARK.len(), ThemePreset::LIGHT.len());
        assert!(ThemePreset::DARK
            .iter()
            .all(|preset| preset.mode() == ThemeMode::Dark));
        assert!(ThemePreset::LIGHT
            .iter()
            .all(|preset| preset.mode() == ThemeMode::Light));

        for (&dark, &light) in ThemePreset::DARK.iter().zip(ThemePreset::LIGHT) {
            assert_eq!(dark.with_mode(ThemeMode::Light), light);
            assert_eq!(light.with_mode(ThemeMode::Dark), dark);
        }
    }

    #[test]
    fn account_customizations_round_trip() {
        let mut settings = Settings::default();
        let account_id = AccountId("account@example.com".to_string());
        let account = settings.account_mut(&account_id);
        account.display_name_override = "Work".to_string();
        account.color_override = Some(0x4A90E2);

        let json = serde_json::to_string(&settings).expect("serialization");
        let loaded: Settings = serde_json::from_str(&json).expect("deserialization");
        let account = loaded
            .accounts
            .get(&account_id)
            .expect("account customization persisted");

        assert_eq!(account.display_name_override, "Work");
        assert_eq!(account.color_override, Some(0x4A90E2));
    }
}
