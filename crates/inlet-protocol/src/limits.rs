//! Instance limits from the discovery document (`limits`, format version 2).
//!
//! The document lists rules that differ from the release defaults, and the
//! defaults themselves are not in it, so this crate keeps a copy of the ones
//! a client needs (Fluxer's `DEFAULT_RESTRICTED_LIMITS`). Resolution follows
//! the instance docs: start from the defaults, sort rules by the number of
//! filters (fewest first), skip rules whose traits are not all present, let a
//! rule without filters set a value and a rule with filters raise it.
//! The server still checks every limit.

use serde_json::Value;

use crate::instance::Discovery;

/// `max_attachment_file_size`.
pub const MAX_ATTACHMENT_FILE_SIZE: &str = "max_attachment_file_size";
/// `max_attachments_per_message`.
pub const MAX_ATTACHMENTS_PER_MESSAGE: &str = "max_attachments_per_message";
/// `max_message_length`.
pub const MAX_MESSAGE_LENGTH: &str = "max_message_length";
/// `max_embeds_per_message`.
pub const MAX_EMBEDS_PER_MESSAGE: &str = "max_embeds_per_message";
/// `max_reactions_per_message`.
pub const MAX_REACTIONS_PER_MESSAGE: &str = "max_reactions_per_message";
/// `max_bio_length`.
pub const MAX_BIO_LENGTH: &str = "max_bio_length";
/// `max_voice_message_duration`.
pub const MAX_VOICE_MESSAGE_DURATION: &str = "max_voice_message_duration";

/// Highest attachment size this crate will upload, whatever the instance
/// says (Fluxer's own ceiling for a premium account is 500 MiB).
pub const UPLOAD_HARD_CAP: u64 = 500 * 1024 * 1024;

/// Most bytes this crate uploads in one message, whatever the instance allows
/// (the files are in memory; see [`crate::upload`]).
pub const MAX_UPLOAD_TOTAL: u64 = 512 * 1024 * 1024;

/// Fluxer's default for a key this crate knows (the restricted defaults).
pub fn default_limit(key: &str) -> Option<u64> {
    Some(match key {
        MAX_ATTACHMENT_FILE_SIZE => 25 * 1024 * 1024,
        MAX_ATTACHMENTS_PER_MESSAGE => 10,
        MAX_MESSAGE_LENGTH => 2000,
        MAX_EMBEDS_PER_MESSAGE => 10,
        MAX_REACTIONS_PER_MESSAGE => 30,
        MAX_BIO_LENGTH => 320,
        MAX_VOICE_MESSAGE_DURATION => 1200,
        _ => return None,
    })
}

fn str_list(v: Option<&Value>) -> Vec<&str> {
    match v {
        Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    }
}

impl Discovery {
    /// Resolves a limit for an account with `traits` (from
    /// [`crate::CurrentUser::traits`]). Returns the rule value, or the
    /// default when no rule sets it, or `None` for a key without a known
    /// default and no matching rule.
    pub fn resolve_limit(&self, key: &str, traits: &[String]) -> Option<u64> {
        let mut value = default_limit(key);
        let rules = match self.limits.get("rules") {
            Some(Value::Array(r)) => r,
            _ => return value,
        };
        let mut sorted: Vec<(usize, &Value)> = rules
            .iter()
            .map(|r| {
                let f = r.get("filters");
                let n = str_list(f.and_then(|f| f.get("traits"))).len()
                    + str_list(f.and_then(|f| f.get("guildFeatures"))).len();
                (n, r)
            })
            .collect();
        sorted.sort_by_key(|(n, _)| *n); // stable: ties keep the published order
        for (filter_count, rule) in sorted {
            let f = rule.get("filters");
            let wanted = str_list(f.and_then(|f| f.get("traits")));
            if !wanted.iter().all(|t| traits.iter().any(|have| have == t)) {
                continue;
            }
            // A user evaluation never has guild features.
            if !str_list(f.and_then(|f| f.get("guildFeatures"))).is_empty() {
                continue;
            }
            let Some(v) = rule
                .get("overrides")
                .and_then(|o| o.get(key))
                .and_then(Value::as_i64)
                .filter(|v| *v >= 0)
                .map(|v| v as u64)
            else {
                continue;
            };
            value = Some(if filter_count == 0 {
                v
            } else {
                value.map_or(v, |cur| cur.max(v))
            });
        }
        value
    }

    /// The largest file the account may attach, capped at [`UPLOAD_HARD_CAP`].
    pub fn max_attachment_file_size(&self, traits: &[String]) -> u64 {
        self.resolve_limit(MAX_ATTACHMENT_FILE_SIZE, traits)
            .unwrap_or(25 * 1024 * 1024)
            .min(UPLOAD_HARD_CAP)
    }

    /// The most files in one message.
    pub fn max_attachments_per_message(&self, traits: &[String]) -> u64 {
        self.resolve_limit(MAX_ATTACHMENTS_PER_MESSAGE, traits)
            .unwrap_or(10)
            .min(100)
    }

    /// The instance's brand colour (`app_public.branding.theme_color`, `#rrggbb`) as 24-bit RGB.
    pub fn theme_color(&self) -> Option<u32> {
        let s = self
            .app_public
            .get("branding")?
            .get("theme_color")?
            .as_str()?;
        let hex = s.strip_prefix('#')?;
        if hex.len() != 6 {
            return None;
        }
        u32::from_str_radix(hex, 16).ok()
    }

    /// Whether the instance has pre-signed attachment uploads
    /// (`features.presigned_attachment_uploads`).
    pub fn presigned_attachment_uploads(&self) -> bool {
        self.features
            .get("presigned_attachment_uploads")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn doc(limits: Value) -> Discovery {
        serde_json::from_value(json!({
            "api_code_version": 1,
            "endpoints": {"api_public": "https://x.example/api", "gateway": "wss://x.example/gw"},
            "limits": limits,
            "app_public": {"branding": {"theme_color": "#4641d9"}},
            "features": {"presigned_attachment_uploads": true}
        }))
        .unwrap()
    }

    #[test]
    fn rules_set_then_raise() {
        let d = doc(json!({"version": 2, "rules": [
            {"id": "premium", "filters": {"traits": ["premium"]},
             "overrides": {"max_attachment_file_size": 524288000, "max_message_length": 4000}},
            {"id": "default", "overrides": {"max_attachment_file_size": 104857600, "max_message_length": 3000}},
            {"id": "guild", "filters": {"guildFeatures": ["X"]}, "overrides": {"max_message_length": 9000}},
            {"id": "neg", "overrides": {"max_attachments_per_message": -1}}
        ]}));
        let none: Vec<String> = vec![];
        let premium = vec!["premium".to_string()];
        assert_eq!(d.max_attachment_file_size(&none), 104_857_600);
        assert_eq!(d.max_attachment_file_size(&premium), 524_288_000);
        assert_eq!(d.resolve_limit(MAX_MESSAGE_LENGTH, &none), Some(3000));
        assert_eq!(d.resolve_limit(MAX_MESSAGE_LENGTH, &premium), Some(4000));
        assert_eq!(d.max_attachments_per_message(&none), 10);
        assert_eq!(d.resolve_limit("unknown_key", &none), None);
        assert_eq!(d.theme_color(), Some(0x4641d9));
        assert!(d.presigned_attachment_uploads());

        let empty = doc(Value::Null);
        assert_eq!(empty.max_attachment_file_size(&none), 25 * 1024 * 1024);
        let huge = doc(
            json!({"rules": [{"id": "d", "overrides": {"max_attachment_file_size": 9_000_000_000_000u64}}]}),
        );
        assert_eq!(huge.max_attachment_file_size(&none), UPLOAD_HARD_CAP);
    }
}
