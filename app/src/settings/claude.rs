use settings::macros::define_settings_group;
use settings::{RespectUserSyncSetting, SupportedPlatforms, SyncToCloud};
use warp_core::features::FeatureFlag;

// User-facing toggles for the Claude Code integration features. Both default on;
// they only take effect where the corresponding `FeatureFlag` is enabled, and let
// the user opt out from the Settings UI.
define_settings_group!(ClaudeSettings, settings: [
    claude_conversations_enabled: ClaudeConversationsEnabled {
        type: bool,
        default: true,
        supported_platforms: SupportedPlatforms::ALL,
        sync_to_cloud: SyncToCloud::Globally(RespectUserSyncSetting::Yes),
        surface: settings::SettingSurfaces::GUI,
        private: false,
        toml_path: "agents.claude_code.conversations_in_command_palette",
        description: "Show past Claude Code sessions in the command palette (claude --resume).",
    },
    claude_usage_pill_enabled: ClaudeUsagePillEnabled {
        type: bool,
        default: true,
        supported_platforms: SupportedPlatforms::ALL,
        sync_to_cloud: SyncToCloud::Globally(RespectUserSyncSetting::Yes),
        surface: settings::SettingSurfaces::GUI,
        private: false,
        toml_path: "agents.claude_code.usage_pill",
        description: "Show the Claude usage pill (5-hour + weekly limits) in the tab bar.",
    }
]);

impl ClaudeSettings {
    /// The feature only exists where the flag is on (dogfood/opt-in builds);
    /// there, the user can turn it off via this setting.
    pub fn is_claude_conversations_enabled(&self) -> bool {
        FeatureFlag::ClaudeConversations.is_enabled() && *self.claude_conversations_enabled
    }

    pub fn is_claude_usage_pill_enabled(&self) -> bool {
        FeatureFlag::ClaudeUsage.is_enabled() && *self.claude_usage_pill_enabled
    }
}
