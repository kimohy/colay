use regex::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};
use thiserror::Error;

const REDACTED: &str = "[REDACTED]";

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
