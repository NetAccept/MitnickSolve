//! Lightweight UI localization. Locale dictionaries are plain JSON maps keyed
//! by stable identifiers; a missing entry falls back to English and finally to
//! the key itself. The current language is process-global because the TUI is a
//! single-window application, and tests default to English.

use std::collections::HashMap;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Language {
    En = 0,
    ZhCn = 1,
    Ja = 2,
    Es = 3,
    Fr = 4,
    De = 5,
}

pub const ALL: &[Language] = &[
    Language::En,
    Language::ZhCn,
    Language::Ja,
    Language::Es,
    Language::Fr,
    Language::De,
];

impl Language {
    pub fn index(self) -> usize {
        self as usize
    }

    pub fn id(self) -> &'static str {
        match self {
            Self::En => "en",
            Self::ZhCn => "zh-CN",
            Self::Ja => "ja",
            Self::Es => "es",
            Self::Fr => "fr",
            Self::De => "de",
        }
    }

    /// Native name shown in the language picker.
    pub fn label(self) -> &'static str {
        match self {
            Self::En => "English",
            Self::ZhCn => "简体中文",
            Self::Ja => "日本語",
            Self::Es => "Español",
            Self::Fr => "Français",
            Self::De => "Deutsch",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        let value = value.trim().to_ascii_lowercase().replace('_', "-");
        if value.starts_with("zh") {
            Some(Self::ZhCn)
        } else if value.starts_with("ja") {
            Some(Self::Ja)
        } else if value.starts_with("es") {
            Some(Self::Es)
        } else if value.starts_with("fr") {
            Some(Self::Fr)
        } else if value.starts_with("de") {
            Some(Self::De)
        } else if value.starts_with("en") || value.is_empty() {
            Some(Self::En)
        } else {
            None
        }
    }

    /// Explicit override first, then the common locale environment variables.
    pub fn detect() -> Self {
        for key in ["MSK_LANG", "LANGUAGE", "LC_ALL", "LANG"] {
            if let Ok(value) = std::env::var(key) {
                if let Some(language) = Self::parse(&value) {
                    return language;
                }
            }
        }
        Self::En
    }

    fn from_index(index: u8) -> Self {
        match index {
            1 => Self::ZhCn,
            2 => Self::Ja,
            3 => Self::Es,
            4 => Self::Fr,
            5 => Self::De,
            _ => Self::En,
        }
    }
}

static CURRENT: AtomicU8 = AtomicU8::new(Language::En as u8);

pub fn set_language(language: Language) {
    CURRENT.store(language as u8, Ordering::Relaxed);
}

pub fn language() -> Language {
    Language::from_index(CURRENT.load(Ordering::Relaxed))
}

fn load(text: &str) -> HashMap<String, String> {
    serde_json::from_str(text).unwrap_or_default()
}

fn maps() -> &'static [HashMap<String, String>; 6] {
    static MAPS: OnceLock<[HashMap<String, String>; 6]> = OnceLock::new();
    MAPS.get_or_init(|| {
        [
            load(include_str!("../locales/en.json")),
            load(include_str!("../locales/zh-CN.json")),
            load(include_str!("../locales/ja.json")),
            load(include_str!("../locales/es.json")),
            load(include_str!("../locales/fr.json")),
            load(include_str!("../locales/de.json")),
        ]
    })
}

pub fn tr(language: Language, key: &'static str) -> &'static str {
    let maps = maps();
    if let Some(value) = maps[language.index()].get(key) {
        return value.as_str();
    }
    if let Some(value) = maps[Language::En.index()].get(key) {
        return value.as_str();
    }
    key
}

/// Translate a key with the current language.
pub fn t(key: &'static str) -> &'static str {
    tr(language(), key)
}

/// Fills `{0}`, `{1}`, … placeholders. Numeric placeholders let translations
/// reorder arguments safely.
pub fn fill(template: &str, args: &[&str]) -> String {
    let mut out = template.to_string();
    for (index, value) in args.iter().enumerate() {
        out = out.replace(&format!("{{{index}}}"), value);
    }
    out
}

pub fn tf(key: &'static str, args: &[&str]) -> String {
    fill(t(key), args)
}

pub fn mode_label(id: &str) -> Option<&'static str> {
    Some(match id {
        "auto" => t("mode.auto.label"),
        "plan" => t("mode.plan.label"),
        "investigate" => t("mode.investigate.label"),
        "exploit" => t("mode.exploit.label"),
        "harden" => t("mode.harden.label"),
        _ => return None,
    })
}

pub fn mode_desc(id: &str) -> Option<&'static str> {
    Some(match id {
        "auto" => t("mode.auto.desc"),
        "plan" => t("mode.plan.desc"),
        "investigate" => t("mode.investigate.desc"),
        "exploit" => t("mode.exploit.desc"),
        "harden" => t("mode.harden.desc"),
        _ => return None,
    })
}

pub fn permission_label(id: &str) -> String {
    match id {
        "auto" => t("permission.auto.label").to_string(),
        "manual" => t("permission.manual.label").to_string(),
        "bypass" => t("permission.bypass.label").to_string(),
        other => other.to_string(),
    }
}

pub fn permission_desc(id: &str) -> String {
    match id {
        "auto" => t("permission.auto.desc").to_string(),
        "manual" => t("permission.manual.desc").to_string(),
        "bypass" => t("permission.bypass.desc").to_string(),
        other => other.to_string(),
    }
}

pub fn effort_label(id: &str) -> String {
    match id {
        "default" => t("effort.default").to_string(),
        "minimal" => t("effort.minimal").to_string(),
        "low" => t("effort.low").to_string(),
        "medium" => t("effort.medium").to_string(),
        "high" => t("effort.high").to_string(),
        other => other.to_string(),
    }
}

pub fn effort_desc(id: &str) -> Option<&'static str> {
    Some(match id {
        "default" => t("effort.default.desc"),
        "minimal" => t("effort.minimal.desc"),
        "low" => t("effort.low.desc"),
        "medium" => t("effort.medium.desc"),
        "high" => t("effort.high.desc"),
        _ => return None,
    })
}

pub fn severity_label(severity: &str) -> String {
    match severity {
        "critical" => t("findings.critical").to_string(),
        "high" => t("findings.high").to_string(),
        "medium" => t("findings.medium").to_string(),
        "low" => t("findings.low").to_string(),
        "info" => t("findings.info").to_string(),
        other => other.to_string(),
    }
}

pub fn finding_status_label(status: &str) -> String {
    match status {
        "open" => t("findings.status.open").to_string(),
        "confirmed" => t("findings.status.confirmed").to_string(),
        "fixed" => t("findings.status.fixed").to_string(),
        "false_positive" => t("findings.status.false_positive").to_string(),
        other => other.to_string(),
    }
}

pub fn state_word(state: &str) -> &'static str {
    match state {
        "running" => t("state.running"),
        "waiting_input" => t("state.ready"),
        "failed" => t("state.failed"),
        "cancelled" => t("state.cancelled"),
        "done" => t("state.done"),
        "saved" => t("state.saved"),
        _ => "",
    }
}

/// The kernel emits these fixed English reasons with approval requests.
pub fn approval_reason(reason: &str) -> String {
    match reason {
        "execute a command" => t("approval.reason.exec").to_string(),
        "write input to a terminal session" => t("approval.reason.write_input").to_string(),
        "write a file in the environment workspace" => t("approval.reason.write_file").to_string(),
        "spawn a subagent with delegated tool access" => t("approval.reason.spawn").to_string(),
        "tool requires approval" => t("approval.reason.default").to_string(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_common_locale_names() {
        assert_eq!(Language::parse("zh-CN"), Some(Language::ZhCn));
        assert_eq!(Language::parse("zh_Hans"), Some(Language::ZhCn));
        assert_eq!(Language::parse("ja-JP"), Some(Language::Ja));
        assert_eq!(Language::parse("en_US.UTF-8"), Some(Language::En));
        assert_eq!(Language::parse("es-419"), Some(Language::Es));
        assert_eq!(Language::parse("fr_FR"), Some(Language::Fr));
        assert_eq!(Language::parse("de-DE"), Some(Language::De));
        assert_eq!(Language::parse("ko-KR"), None);
    }

    #[test]
    fn every_language_has_a_native_label() {
        for language in ALL {
            assert!(!language.label().is_empty());
            assert!(!language.id().is_empty());
        }
    }

    #[test]
    fn missing_keys_fall_back_to_english_then_the_key() {
        assert_eq!(
            tr(Language::ZhCn, "definitely.missing.key"),
            "definitely.missing.key"
        );
        assert!(!tr(Language::ZhCn, "app.tagline").is_empty());
    }

    #[test]
    fn placeholders_are_replaced_by_index() {
        assert_eq!(fill("{1} of {0}", &["one", "two"]), "two of one");
        assert!(tf("footer.ctx", &["12.3k"]).contains("12.3k"));
    }
}
