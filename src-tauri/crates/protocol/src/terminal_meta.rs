//! Reserved Terminal metadata. Automatic `title` remains independent of the custom name.

use serde_json::{Map, Value};
use std::fmt;

pub const CUSTOM_TITLE_KEY: &str = "customTitle";
/// Unicode scalar values, not bytes, grapheme clusters or UTF-16 code units.
pub const CUSTOM_TITLE_MAX_CHARS: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CustomTitleError {
    Type,
    Blank,
    Controls,
    TooLong,
    NonCanonical,
}

impl fmt::Display for CustomTitleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Type => "customTitle must be a string (or null in an update to clear it)",
            Self::Blank => "customTitle must not be blank",
            Self::Controls => "customTitle must not contain control or line-separator characters",
            Self::TooLong => "customTitle must contain at most 256 Unicode scalar values",
            Self::NonCanonical => "customTitle must be stored in normalized form",
        })
    }
}

impl std::error::Error for CustomTitleError {}

/// Reject controls before trimming; preserve internal spaces, case and composition.
pub fn normalize_custom_title(text: &str) -> Result<String, CustomTitleError> {
    if text
        .chars()
        .any(|c| c.is_control() || matches!(c, '\u{2028}' | '\u{2029}'))
    {
        return Err(CustomTitleError::Controls);
    }
    let text = text.trim_matches(char::is_whitespace);
    if text.is_empty() {
        return Err(CustomTitleError::Blank);
    }
    if text.chars().count() > CUSTOM_TITLE_MAX_CHARS {
        return Err(CustomTitleError::TooLong);
    }
    Ok(text.to_owned())
}

/// Normalize only the reserved key. `true` means explicitly touched, even an equal-value
/// retry or clearing an already absent key; these still require a durable write.
pub fn normalize_custom_title_patch(
    meta: &mut Map<String, Value>,
) -> Result<bool, CustomTitleError> {
    match meta.get_mut(CUSTOM_TITLE_KEY) {
        None => Ok(false),
        Some(Value::Null) => Ok(true),
        Some(Value::String(s)) => {
            *s = normalize_custom_title(s)?;
            Ok(true)
        }
        Some(_) => Err(CustomTitleError::Type),
    }
}

/// Stored records never contain null or a noncanonical custom name.
pub fn validate_custom_title_record(meta: &Map<String, Value>) -> Result<(), CustomTitleError> {
    match meta.get(CUSTOM_TITLE_KEY) {
        None => Ok(()),
        Some(Value::String(s)) if normalize_custom_title(s)? == *s => Ok(()),
        Some(Value::String(_)) => Err(CustomTitleError::NonCanonical),
        Some(_) => Err(CustomTitleError::Type),
    }
}

/// Defensive reader for observed metadata: invalid values are not custom names.
pub fn custom_title(meta: &Map<String, Value>) -> Option<&str> {
    validate_custom_title_record(meta).ok()?;
    meta.get(CUSTOM_TITLE_KEY)?.as_str()
}

/// Repair just the reserved field on restore, without discarding other metadata or a Terminal.
/// Returns whether the field was changed or removed; callers must not log its contents.
pub fn sanitize_restored_custom_title(meta: &mut Map<String, Value>) -> bool {
    let Some(value) = meta.get(CUSTOM_TITLE_KEY) else {
        return false;
    };
    let normalized = value.as_str().and_then(|s| normalize_custom_title(s).ok());
    match normalized {
        Some(s) if value == &Value::String(s.clone()) => false,
        Some(s) => {
            meta.insert(CUSTOM_TITLE_KEY.into(), Value::String(s));
            true
        }
        None => {
            meta.remove(CUSTOM_TITLE_KEY);
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn custom_title_normalization_and_scalar_limits() {
        assert_eq!(
            normalize_custom_title("\u{2003} Review  queue \u{a0}").unwrap(),
            "Review  queue"
        );
        assert!(normalize_custom_title(&"🦀".repeat(256)).is_ok());
        assert_eq!(
            normalize_custom_title(&"🦀".repeat(257)),
            Err(CustomTitleError::TooLong)
        );
        for s in ["", "  ", "\u{2003}"] {
            assert_eq!(normalize_custom_title(s), Err(CustomTitleError::Blank));
        }
        for s in [
            "\nname", "name\r", "a\tb", "a\0b", "\u{7f}", "\u{85}", "\u{9f}", "\u{2028}",
            "\u{2029}",
        ] {
            assert_eq!(
                normalize_custom_title(s),
                Err(CustomTitleError::Controls),
                "{s:?}"
            );
        }
        // BOM is not Unicode White_Space; composition is deliberately unchanged.
        for s in ["\u{feff}name", "e\u{301}", "👩‍💻"] {
            assert_eq!(normalize_custom_title(s).unwrap(), s);
        }
    }

    #[test]
    fn custom_title_patch_record_and_restore() {
        let mut meta = json!({"title":"automatic", "other":42})
            .as_object()
            .unwrap()
            .clone();
        assert!(!normalize_custom_title_patch(&mut meta).unwrap());
        for v in [json!(false), json!(42), json!([]), json!({})] {
            meta.insert(CUSTOM_TITLE_KEY.into(), v);
            assert_eq!(
                normalize_custom_title_patch(&mut meta),
                Err(CustomTitleError::Type)
            );
        }
        meta.insert(CUSTOM_TITLE_KEY.into(), json!(" name "));
        assert!(normalize_custom_title_patch(&mut meta).unwrap());
        assert_eq!(custom_title(&meta), Some("name"));
        assert_eq!(meta["title"], "automatic");
        assert_eq!(meta["other"], 42);
        assert!(normalize_custom_title_patch(&mut meta).unwrap());
        for v in [Value::Null, json!("\ninvalid"), json!(7)] {
            meta.insert(CUSTOM_TITLE_KEY.into(), v);
            assert!(custom_title(&meta).is_none());
            assert!(sanitize_restored_custom_title(&mut meta));
            assert!(!meta.contains_key(CUSTOM_TITLE_KEY));
        }
        meta.insert(CUSTOM_TITLE_KEY.into(), json!(" name "));
        assert!(sanitize_restored_custom_title(&mut meta));
        assert_eq!(custom_title(&meta), Some("name"));
        meta.insert(CUSTOM_TITLE_KEY.into(), Value::Null);
        assert!(normalize_custom_title_patch(&mut meta).unwrap());
        assert!(validate_custom_title_record(&meta).is_err());
    }
}
