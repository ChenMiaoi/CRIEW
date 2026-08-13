//! Persistence for lightweight TUI preferences.
//!
//! The UI stores only coarse-grained user intent here, such as enabled
//! mailboxes and the last active mailbox. Derived state stays out of this file
//! so upgrades can rebuild richer view models from stable preferences.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::infra::error::{CriewError, ErrorCode, Result};

pub const DEFAULT_MAIL_SUBSCRIPTIONS_WIDTH: u16 = 23;
pub const DEFAULT_MAIL_PREVIEW_WIDTH: u16 = 85;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SavedView {
    pub name: String,
    pub query: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiState {
    #[serde(default)]
    pub enabled_mailboxes: Vec<String>,
    #[serde(default = "default_true")]
    pub enabled_group_expanded: bool,
    #[serde(default = "default_true")]
    pub disabled_group_expanded: bool,
    #[serde(default = "default_true")]
    pub enabled_linux_subsystem_expanded: bool,
    #[serde(default = "default_true")]
    pub enabled_qemu_subsystem_expanded: bool,
    #[serde(default = "default_true")]
    pub disabled_linux_subsystem_expanded: bool,
    #[serde(default = "default_true")]
    pub disabled_qemu_subsystem_expanded: bool,
    #[serde(default)]
    pub imap_defaults_initialized: bool,
    #[serde(default)]
    pub active_mailbox: Option<String>,
    #[serde(default = "default_mail_subscriptions_width")]
    pub mail_subscriptions_width: u16,
    #[serde(default = "default_mail_preview_width")]
    pub mail_preview_width: u16,
    #[serde(default)]
    pub saved_views: Vec<SavedView>,
}

fn default_true() -> bool {
    true
}

fn default_mail_subscriptions_width() -> u16 {
    DEFAULT_MAIL_SUBSCRIPTIONS_WIDTH
}

fn default_mail_preview_width() -> u16 {
    DEFAULT_MAIL_PREVIEW_WIDTH
}

impl Default for UiState {
    fn default() -> Self {
        Self {
            enabled_mailboxes: Vec::new(),
            enabled_group_expanded: true,
            disabled_group_expanded: true,
            enabled_linux_subsystem_expanded: true,
            enabled_qemu_subsystem_expanded: true,
            disabled_linux_subsystem_expanded: true,
            disabled_qemu_subsystem_expanded: true,
            imap_defaults_initialized: false,
            active_mailbox: None,
            mail_subscriptions_width: DEFAULT_MAIL_SUBSCRIPTIONS_WIDTH,
            mail_preview_width: DEFAULT_MAIL_PREVIEW_WIDTH,
            saved_views: Vec::new(),
        }
    }
}

impl UiState {
    pub fn normalized_enabled_mailboxes(&self) -> Vec<String> {
        // Normalize once at the boundary so read/modify/write cycles do not
        // churn the file with duplicates or whitespace-only entries.
        let mut seen = HashSet::new();
        let mut mailboxes = Vec::new();
        for mailbox in &self.enabled_mailboxes {
            let normalized = mailbox.trim();
            if normalized.is_empty() {
                continue;
            }
            if seen.insert(normalized.to_string()) {
                mailboxes.push(normalized.to_string());
            }
        }
        mailboxes.sort();
        mailboxes
    }

    pub fn normalized_saved_views(&self) -> Vec<SavedView> {
        let mut seen = HashSet::new();
        let mut views = Vec::new();
        for view in &self.saved_views {
            let name = view.name.trim();
            let query = view.query.trim();
            if name.is_empty() || query.is_empty() {
                continue;
            }
            let key = name.to_ascii_lowercase();
            if seen.insert(key) {
                views.push(SavedView {
                    name: name.to_string(),
                    query: query.to_string(),
                });
            }
        }
        views.sort_by(|left, right| {
            left.name
                .to_ascii_lowercase()
                .cmp(&right.name.to_ascii_lowercase())
        });
        views
    }
}

pub fn path_for_data_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("ui-state.toml")
}

pub fn load(path: &Path) -> Result<Option<UiState>> {
    if !path.exists() {
        return Ok(None);
    }

    let content = fs::read_to_string(path).map_err(|error| {
        CriewError::with_source(
            ErrorCode::Io,
            format!("failed to read ui state {}", path.display()),
            error,
        )
    })?;

    let state = toml::from_str::<UiState>(&content).map_err(|error| {
        CriewError::with_source(
            ErrorCode::ConfigParse,
            format!("failed to parse ui state {}", path.display()),
            error,
        )
    })?;

    Ok(Some(state))
}

pub fn save(path: &Path, state: &UiState) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            CriewError::with_source(
                ErrorCode::Io,
                format!("failed to create ui state directory {}", parent.display()),
                error,
            )
        })?;
    }

    let content = toml::to_string_pretty(state).map_err(|error| {
        CriewError::with_source(
            ErrorCode::ConfigParse,
            format!("failed to serialize ui state {}", path.display()),
            error,
        )
    })?;

    fs::write(path, content).map_err(|error| {
        CriewError::with_source(
            ErrorCode::Io,
            format!("failed to write ui state {}", path.display()),
            error,
        )
    })?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{
        DEFAULT_MAIL_PREVIEW_WIDTH, DEFAULT_MAIL_SUBSCRIPTIONS_WIDTH, SavedView, UiState, load,
        path_for_data_dir, save,
    };

    fn temp_dir(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("criew-ui-state-{label}-{nonce}"));
        fs::create_dir_all(&path).expect("create temp dir");
        path
    }

    #[test]
    fn roundtrip_ui_state_file() {
        let root = temp_dir("roundtrip");
        let path = path_for_data_dir(&root);
        let state = UiState {
            enabled_mailboxes: vec!["bpf".to_string(), "linux-mm".to_string(), "bpf".to_string()],
            enabled_group_expanded: false,
            disabled_group_expanded: true,
            enabled_linux_subsystem_expanded: false,
            enabled_qemu_subsystem_expanded: true,
            disabled_linux_subsystem_expanded: true,
            disabled_qemu_subsystem_expanded: false,
            imap_defaults_initialized: true,
            active_mailbox: Some("bpf".to_string()),
            mail_subscriptions_width: 28,
            mail_preview_width: 84,
            saved_views: vec![SavedView {
                name: "patches".to_string(),
                query: "is:patch".to_string(),
            }],
        };

        save(&path, &state).expect("save state");
        let loaded = load(&path).expect("load state").expect("state exists");

        assert_eq!(
            loaded.normalized_enabled_mailboxes(),
            vec!["bpf".to_string(), "linux-mm".to_string()]
        );
        assert!(!loaded.enabled_group_expanded);
        assert!(loaded.disabled_group_expanded);
        assert!(!loaded.enabled_linux_subsystem_expanded);
        assert!(loaded.enabled_qemu_subsystem_expanded);
        assert!(loaded.disabled_linux_subsystem_expanded);
        assert!(!loaded.disabled_qemu_subsystem_expanded);
        assert!(loaded.imap_defaults_initialized);
        assert_eq!(loaded.active_mailbox.as_deref(), Some("bpf"));
        assert_eq!(loaded.mail_subscriptions_width, 28);
        assert_eq!(loaded.mail_preview_width, 84);
        assert_eq!(loaded.saved_views, state.saved_views);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn missing_imap_defaults_marker_defaults_to_false() {
        let root = temp_dir("legacy-marker");
        let path = path_for_data_dir(&root);

        fs::write(
            &path,
            "enabled_mailboxes = [\"bpf\"]\nenabled_group_expanded = true\ndisabled_group_expanded = true\nactive_mailbox = \"bpf\"\n",
        )
        .expect("write legacy ui state");

        let loaded = load(&path).expect("load state").expect("state exists");

        assert!(!loaded.imap_defaults_initialized);
        assert!(loaded.enabled_linux_subsystem_expanded);
        assert!(loaded.enabled_qemu_subsystem_expanded);
        assert!(loaded.disabled_linux_subsystem_expanded);
        assert!(loaded.disabled_qemu_subsystem_expanded);
        assert_eq!(
            loaded.mail_subscriptions_width,
            DEFAULT_MAIL_SUBSCRIPTIONS_WIDTH
        );
        assert_eq!(loaded.mail_preview_width, DEFAULT_MAIL_PREVIEW_WIDTH);
        assert!(loaded.saved_views.is_empty());

        let _ = fs::remove_dir_all(root);
    }
}
