use serde::{Deserialize, Serialize};

use crate::{CapturedOutput, redaction::PrivateKeyStreamRedaction};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MalformedJsonLine {
    pub line_number: usize,
    pub error: String,
    pub redacted_line: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct JsonLines {
    pub values: Vec<serde_json::Value>,
    pub malformed: Vec<MalformedJsonLine>,
    pub incomplete_due_to_truncation: bool,
    pub invalid_utf8: bool,
}

/// Parses captured provider output while retaining redacted evidence for malformed lines.
#[must_use]
pub fn parse_json_lines(output: &CapturedOutput) -> JsonLines {
    let lossy = String::from_utf8_lossy(&output.bytes);
    let mut result = JsonLines {
        incomplete_due_to_truncation: output.truncated,
        invalid_utf8: output.invalid_utf8,
        ..JsonLines::default()
    };
    let mut private_key_redaction = PrivateKeyStreamRedaction::default();
    for (index, raw) in lossy.lines().enumerate() {
        if raw.trim().is_empty() {
            continue;
        }
        let private_key_sensitive = private_key_redaction.inspect_frame(raw.as_bytes());
        private_key_redaction.inspect_frame(b"\n");
        if private_key_sensitive {
            result.malformed.push(MalformedJsonLine {
                line_number: index + 1,
                error: "private-key material was redacted before JSON parsing".to_owned(),
                redacted_line: "[REDACTED PRIVATE KEY]".to_owned(),
            });
            continue;
        }
        match serde_json::from_str(raw) {
            Ok(value) => result.values.push(value),
            Err(error) => result.malformed.push(MalformedJsonLine {
                line_number: index + 1,
                error: error.to_string(),
                redacted_line: output.redactor.redact(raw),
            }),
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use crate::{CapturedOutput, RedactionConfig, Redactor, parse_json_lines};

    #[test]
    fn malformed_lines_only_expose_redacted_evidence() {
        let output = CapturedOutput::for_test(
            b"{\"ok\":true}\napi_key=supersecret\n".to_vec(),
            Redactor::new(&RedactionConfig::default())
                .unwrap_or_else(|error| panic!("redactor: {error}")),
        );
        let parsed = parse_json_lines(&output);
        assert_eq!(parsed.values.len(), 1);
        assert_eq!(parsed.malformed.len(), 1);
        assert!(!parsed.malformed[0].redacted_line.contains("supersecret"));
    }

    #[test]
    fn multiline_private_key_evidence_preserves_line_accounting() {
        let output = CapturedOutput::for_test(
            b"-----BEGIN PRIVATE KEY-----\nprivate-body-line\n-----END PRIVATE KEY-----\nnot-json\n"
                .to_vec(),
            Redactor::new(&RedactionConfig::default())
                .unwrap_or_else(|error| panic!("redactor: {error}")),
        );
        let parsed = parse_json_lines(&output);
        assert_eq!(parsed.malformed.len(), 4);
        assert_eq!(parsed.malformed[3].line_number, 4);
        assert!(
            parsed.malformed[..3]
                .iter()
                .all(|line| line.redacted_line == "[REDACTED PRIVATE KEY]")
        );
        assert_eq!(parsed.malformed[3].redacted_line, "not-json");
    }

    #[test]
    fn invalid_private_key_end_keeps_later_evidence_redacted() {
        let output = CapturedOutput::for_test(
            b"-----BEGIN PRIVATE KEY-----\nprivate-body\n-----ENDxPRIVATE KEY-----\nstill-private\n"
                .to_vec(),
            Redactor::new(&RedactionConfig::default())
                .unwrap_or_else(|error| panic!("redactor: {error}")),
        );

        let parsed = parse_json_lines(&output);
        assert_eq!(parsed.malformed.len(), 4);
        assert!(
            parsed
                .malformed
                .iter()
                .all(|line| line.redacted_line == "[REDACTED PRIVATE KEY]")
        );
    }

    #[test]
    fn valid_json_inside_private_key_material_is_never_returned_as_a_value() {
        let output = CapturedOutput::for_test(
            b"-----BEGIN PRIVATE KEY-----\n\"valid-json-secret\"\n-----END PRIVATE KEY-----\n"
                .to_vec(),
            Redactor::new(&RedactionConfig::default())
                .unwrap_or_else(|error| panic!("redactor: {error}")),
        );

        let parsed = parse_json_lines(&output);
        assert!(parsed.values.is_empty());
        assert_eq!(parsed.malformed.len(), 3);
        assert!(
            parsed
                .malformed
                .iter()
                .all(|line| line.redacted_line == "[REDACTED PRIVATE KEY]")
        );
    }
}
