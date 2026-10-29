//! Language for app-owned native controls; never changes the CLI or OS locale.
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum UiLanguage { #[default] Ko, En }

impl UiLanguage {
    pub fn as_str(self) -> &'static str { match self { Self::Ko => "ko", Self::En => "en" } }
    pub fn text(self, ko: &'static str, en: &'static str) -> &'static str {
        match self { Self::Ko => ko, Self::En => en }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_supported_languages_are_accepted() {
        assert_eq!(serde_json::from_str::<UiLanguage>("\"ko\"").unwrap(), UiLanguage::Ko);
        assert_eq!(serde_json::from_str::<UiLanguage>("\"en\"").unwrap(), UiLanguage::En);
        assert!(serde_json::from_str::<UiLanguage>("\"unknown\"").is_err());
        assert_eq!(UiLanguage::default().text("한국어", "English"), "한국어");
        assert_eq!(UiLanguage::En.text("한국어", "English"), "English");
    }
}
