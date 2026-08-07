use regex::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};
use thiserror::Error;

const REDACTED: &str = "[REDACTED]";
const PRIVATE_KEY_BEGIN_PREFIX: &[u8] = b"-----BEGIN ";
const PRIVATE_KEY_END_PREFIX: &[u8] = b"-----END ";
const PRIVATE_KEY_LABEL: &[u8] = b"PRIVATE KEY";
const PRIVATE_KEY_TRAILING_DASHES: u8 = 5;

/// Incrementally recognizes exactly the same PEM boundary grammar as the
/// built-in private-key redaction expression. Partial candidates are retained
/// across process frames, while line endings reset only the candidate state.
#[derive(Debug)]
pub(crate) struct PrivateKeyStreamRedaction {
    active: bool,
    prefix_match: usize,
    label_candidate: bool,
    label_tail: [u8; PRIVATE_KEY_LABEL.len()],
    label_tail_len: usize,
    trailing_dashes: u8,
}

impl Default for PrivateKeyStreamRedaction {
    fn default() -> Self {
        Self {
            active: false,
            prefix_match: 0,
            label_candidate: false,
            label_tail: [0; PRIVATE_KEY_LABEL.len()],
            label_tail_len: 0,
            trailing_dashes: 0,
        }
    }
}

impl PrivateKeyStreamRedaction {
    #[must_use]
    pub(crate) fn is_active(&self) -> bool {
        self.active
    }

    /// Returns true when any part of this frame must be treated as private-key
    /// material, including a boundary candidate continued by another frame.
    pub(crate) fn inspect_frame(&mut self, bytes: &[u8]) -> bool {
        let sensitive_at_start = self.active || self.has_pending_candidate();
        let mut private_key_started = false;
        for &byte in bytes {
            if matches!(byte, b'\n' | b'\r') {
                self.reset_candidate();
                continue;
            }
            private_key_started |= self.inspect_byte(byte);
        }
        sensitive_at_start || private_key_started || self.active || self.has_pending_candidate()
    }

    fn inspect_byte(&mut self, byte: u8) -> bool {
        if !self.label_candidate {
            self.advance_prefix(byte);
            return false;
        }

        if self.trailing_dashes > 0 {
            if byte == b'-' {
                self.trailing_dashes += 1;
                if self.trailing_dashes == PRIVATE_KEY_TRAILING_DASHES {
                    let private_key_started = !self.active;
                    self.active = !self.active;
                    self.reset_candidate();
                    return private_key_started;
                }
                return false;
            }
            self.reset_candidate();
            self.advance_prefix(byte);
            return false;
        }

        if byte.is_ascii_uppercase() || byte == b' ' {
            self.push_label_byte(byte);
        } else if byte == b'-' && self.label_tail() == PRIVATE_KEY_LABEL {
            self.trailing_dashes = 1;
        } else {
            self.reset_candidate();
            self.advance_prefix(byte);
        }
        false
    }

    fn advance_prefix(&mut self, byte: u8) {
        let prefix = if self.active {
            PRIVATE_KEY_END_PREFIX
        } else {
            PRIVATE_KEY_BEGIN_PREFIX
        };
        self.prefix_match = advance_literal_match(prefix, self.prefix_match, byte);
        if self.prefix_match == prefix.len() {
            self.prefix_match = 0;
            self.label_candidate = true;
            self.label_tail_len = 0;
            self.trailing_dashes = 0;
        }
    }

    fn push_label_byte(&mut self, byte: u8) {
        if self.label_tail_len < self.label_tail.len() {
            self.label_tail[self.label_tail_len] = byte;
            self.label_tail_len += 1;
        } else {
            self.label_tail.copy_within(1.., 0);
            self.label_tail[PRIVATE_KEY_LABEL.len() - 1] = byte;
        }
    }

    fn label_tail(&self) -> &[u8] {
        &self.label_tail[..self.label_tail_len]
    }

    fn has_pending_candidate(&self) -> bool {
        self.prefix_match > 0 || self.label_candidate
    }

    fn reset_candidate(&mut self) {
        self.prefix_match = 0;
        self.label_candidate = false;
        self.label_tail_len = 0;
        self.trailing_dashes = 0;
    }
}

fn advance_literal_match(pattern: &[u8], matched: usize, byte: u8) -> usize {
    let mut candidate = (matched + 1).min(pattern.len());
    while candidate > 0 {
        let overlap = candidate - 1;
        if pattern[overlap] == byte
            && (overlap == 0 || pattern[..overlap] == pattern[matched - overlap..matched])
        {
            return candidate;
        }
        candidate -= 1;
    }
    0
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RedactionConfig {
    /// Exact secret values supplied by an administrator. Empty and very short literals
    /// are ignored to prevent destructive over-redaction.
    #[serde(default)]
    pub literals: Vec<String>,
    /// Additional regular expressions whose full match will be redacted.
    #[serde(default)]
    pub patterns: Vec<String>,
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum RedactionError {
    #[error("invalid redaction pattern `{pattern}`: {message}")]
    InvalidPattern { pattern: String, message: String },
}

#[derive(Clone, Debug)]
pub struct Redactor {
    literals: Vec<String>,
    custom_patterns: Vec<Regex>,
    quoted_credentials: Regex,
    credentials: Regex,
    bearer: Regex,
    provider_tokens: Regex,
    private_key: Regex,
}

impl Redactor {
    pub fn new(config: &RedactionConfig) -> Result<Self, RedactionError> {
        let mut literals = config
            .literals
            .iter()
            .filter(|literal| literal.chars().count() >= 4)
            .cloned()
            .collect::<Vec<_>>();
        literals.sort_by_key(|right| std::cmp::Reverse(right.len()));
        literals.dedup();

        let custom_patterns = config
            .patterns
            .iter()
            .map(|pattern| {
                RegexBuilder::new(pattern)
                    .size_limit(1 << 20)
                    .build()
                    .map_err(|error| RedactionError::InvalidPattern {
                        pattern: pattern.clone(),
                        message: error.to_string(),
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;

        Ok(Self {
            literals,
            custom_patterns,
            quoted_credentials: builtin(
                r#"(?i)\b((?:[a-z0-9]+[_-])*(?:api[_-]?key|access[_-]?token|refresh[_-]?token|client[_-]?secret|authorization|password))\b[\"']?(\s*[:=]\s*)(?:\"[^\"\r\n]*\"|'[^'\r\n]*')"#,
            )?,
            credentials: builtin(
                r#"(?i)\b((?:[a-z0-9]+[_-])*(?:api[_-]?key|access[_-]?token|refresh[_-]?token|client[_-]?secret|authorization|password))\b[\"']?(\s*[:=]\s*)[^\s,\"']{4,}"#,
            )?,
            bearer: builtin(r"(?i)\bbearer\s+[A-Za-z0-9._~+/=-]{8,}")?,
            provider_tokens: builtin(
                r"\b(?:sk-(?:ant-)?[A-Za-z0-9_-]{8,}|AIza[A-Za-z0-9_-]{16,})\b",
            )?,
            private_key: builtin(
                r"(?s)-----BEGIN [A-Z ]*PRIVATE KEY-----.*?-----END [A-Z ]*PRIVATE KEY-----",
            )?,
        })
    }

    #[must_use]
    pub fn redact(&self, input: &str) -> String {
        let mut output = input.to_owned();
        for literal in &self.literals {
            output = output.replace(literal, REDACTED);
        }
        output = self.bearer.replace_all(&output, REDACTED).into_owned();
        output = self
            .quoted_credentials
            .replace_all(&output, "$1$2[REDACTED]")
            .into_owned();
        output = self
            .credentials
            .replace_all(&output, "$1$2[REDACTED]")
            .into_owned();
        output = self
            .provider_tokens
            .replace_all(&output, REDACTED)
            .into_owned();
        output = self.private_key.replace_all(&output, REDACTED).into_owned();
        for pattern in &self.custom_patterns {
            output = pattern.replace_all(&output, REDACTED).into_owned();
        }
        output
    }
}

fn builtin(pattern: &str) -> Result<Regex, RedactionError> {
    Regex::new(pattern).map_err(|error| RedactionError::InvalidPattern {
        pattern: pattern.to_owned(),
        message: error.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::{REDACTED, RedactionConfig, Redactor};

    #[test]
    fn redacts_builtin_and_configured_secrets() {
        let redactor = Redactor::new(&RedactionConfig {
            literals: vec!["company-private-value".to_owned()],
            patterns: vec![r"CUSTOM-[0-9]+".to_owned()],
        })
        .unwrap_or_else(|error| panic!("redactor: {error}"));
        let input = "Authorization: Bearer abcdefghijklmnop\napi_key=topsecretvalue\n\
                     OPENAI_API_KEY=projectsecretvalue\n\
                     {\"client_secret\": \"json-secret-value with spaces\"}\n\
                     sk-ant-abcdefghijk CUSTOM-123 company-private-value";
        let output = redactor.redact(input);
        assert!(!output.contains("abcdefghijklmnop"));
        assert!(!output.contains("topsecretvalue"));
        assert!(!output.contains("projectsecretvalue"));
        assert!(!output.contains("json-secret-value"));
        assert!(!output.contains("company-private-value"));
        assert!(output.matches(REDACTED).count() >= 6);
    }
}
