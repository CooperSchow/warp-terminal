use settings::macros::define_settings_group;
use settings::{RespectUserSyncSetting, SupportedPlatforms, SyncToCloud};
use warp_core::features::FeatureFlag;
use warpui::{AppContext, SingletonEntity};

// User-facing toggles for the Codex CLI integration features. All default on;
// they only take effect where the corresponding `FeatureFlag` is enabled, and let
// the user opt out from the Settings UI.
define_settings_group!(CodexSettings, settings: [
    codex_conversations_enabled: CodexConversationsEnabled {
        type: bool,
        default: true,
        supported_platforms: SupportedPlatforms::ALL,
        sync_to_cloud: SyncToCloud::Globally(RespectUserSyncSetting::Yes),
        surface: settings::SettingSurfaces::GUI,
        private: false,
        toml_path: "agents.codex.conversations_in_command_palette",
        description: "Show past Codex CLI sessions in the command palette (codex resume).",
    },
    codex_usage_pill_enabled: CodexUsagePillEnabled {
        type: bool,
        default: true,
        supported_platforms: SupportedPlatforms::ALL,
        sync_to_cloud: SyncToCloud::Globally(RespectUserSyncSetting::Yes),
        surface: settings::SettingSurfaces::GUI,
        private: false,
        toml_path: "agents.codex.usage_pill",
        description: "Show the Codex usage pill (rolling subscription limits) in the tab bar.",
    },
    codex_session_resume_enabled: CodexSessionResumeEnabled {
        type: bool,
        default: true,
        supported_platforms: SupportedPlatforms::ALL,
        sync_to_cloud: SyncToCloud::Globally(RespectUserSyncSetting::Yes),
        surface: settings::SettingSurfaces::GUI,
        private: false,
        toml_path: "agents.codex.resume_sessions_on_restore",
        description: "Reopen each tab's Codex conversation when a window is restored.",
    }
]);

impl CodexSettings {
    /// The feature only exists where the flag is on (dogfood/opt-in builds);
    /// there, the user can turn it off via this setting.
    pub fn is_codex_conversations_enabled(&self) -> bool {
        FeatureFlag::CodexConversations.is_enabled() && *self.codex_conversations_enabled
    }

    pub fn is_codex_usage_pill_enabled(&self) -> bool {
        FeatureFlag::CodexUsage.is_enabled() && *self.codex_usage_pill_enabled
    }

    pub fn is_codex_session_resume_enabled(&self) -> bool {
        FeatureFlag::CodexSessionResume.is_enabled() && *self.codex_session_resume_enabled
    }
}

/// True when resuming a pane's Codex conversation on restore is on for this
/// build and this user.
///
/// A guarded free function because both halves of the feature need it — the
/// snapshot in `pane_group::pane::terminal_pane` and the restore in
/// `pane_group` — and both run in unit-test harnesses that skip settings
/// registration, where reading the singleton outright would panic.
pub fn codex_session_resume_enabled(ctx: &AppContext) -> bool {
    ctx.has_singleton_model::<CodexSettings>()
        && CodexSettings::as_ref(ctx).is_codex_session_resume_enabled()
}
