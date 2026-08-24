use std::collections::VecDeque;

const REDACTED: &str = "<redacted>";
const SENSITIVE_KEYS: &[&str] = &[
    "access_token",
    "authorization",
    "client_secret",
    "code_verifier",
    "cookie",
    "id_token",
    "openai_api_key",
    "password",
    "refresh_token",
    "secret",
    "set-cookie",
    "token",
];

/// Redacts common bearer tokens, cookies, API keys, and secret-valued fields from one log line.
pub(crate) fn redact_line(line: &str) -> String {
    let mut redacted = line.to_owned();

    for key in SENSITIVE_KEYS {
        redacted = redact_assignment(&redacted, key);
        redacted = redact_json_value(&redacted, key);
        redacted = redact_header_value(&redacted, key);
    }

    redact_bearer_tokens(&redacted)
}

fn redact_assignment(input: &str, key: &str) -> String {
    let lower = input.to_ascii_lowercase();
    let needle = format!("{key}=");
    let mut output = String::with_capacity(input.len());
    let mut cursor = 0;

    while let Some(relative) = lower[cursor..].find(&needle) {
        let start = cursor + relative;
        let value_start = start + needle.len();
        output.push_str(&input[cursor..value_start]);
        output.push_str(REDACTED);
        let value_end = input[value_start..]
            .find(|character: char| {
                character.is_whitespace() || character == '&' || character == ';'
            })
            .map_or(input.len(), |offset| value_start + offset);
        cursor = value_end;
    }
    output.push_str(&input[cursor..]);
    output
}

fn redact_json_value(input: &str, key: &str) -> String {
    let lower = input.to_ascii_lowercase();
    let quoted_key = format!("\"{key}\"");
    let mut output = String::with_capacity(input.len());
    let mut cursor = 0;

    while let Some(relative) = lower[cursor..].find(&quoted_key) {
        let key_start = cursor + relative;
        let mut index = key_start + quoted_key.len();
        let bytes = input.as_bytes();
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        if bytes.get(index) != Some(&b':') {
            output.push_str(&input[cursor..index]);
            cursor = index;
            continue;
        }
        index += 1;
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        if bytes.get(index) != Some(&b'\"') {
            output.push_str(&input[cursor..index]);
            cursor = index;
            continue;
        }

        let value_start = index + 1;
        let Some(value_end) = find_json_string_end(input, value_start) else {
            output.push_str(&input[cursor..]);
            return output;
        };
        output.push_str(&input[cursor..value_start]);
        output.push_str(REDACTED);
        cursor = value_end;
    }
    output.push_str(&input[cursor..]);
    output
}

fn find_json_string_end(input: &str, start: usize) -> Option<usize> {
    let bytes = input.as_bytes();
    let mut index = start;
    let mut escaped = false;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' if !escaped => escaped = true,
            b'\"' if !escaped => return Some(index),
            _ => escaped = false,
        }
        index += 1;
    }
    None
}

fn redact_header_value(input: &str, key: &str) -> String {
    let lower = input.to_ascii_lowercase();
    let needle = format!("{key}:");
    let mut output = String::with_capacity(input.len());
    let mut cursor = 0;

    while let Some(relative) = lower[cursor..].find(&needle) {
        let start = cursor + relative;
        let value_start = start + needle.len();
        output.push_str(&input[cursor..value_start]);
        let whitespace_len = input[value_start..]
            .chars()
            .take_while(|character| character.is_whitespace())
            .map(char::len_utf8)
            .sum::<usize>();
        output.push_str(&input[value_start..value_start + whitespace_len]);
        output.push_str(REDACTED);
        let content_start = value_start + whitespace_len;
        let value_end = input[content_start..]
            .find([',', '\r', '\n'])
            .map_or(input.len(), |offset| content_start + offset);
        cursor = value_end;
    }
    output.push_str(&input[cursor..]);
    output
}

fn redact_bearer_tokens(input: &str) -> String {
    let lower = input.to_ascii_lowercase();
    let needle = "bearer ";
    let mut output = String::with_capacity(input.len());
    let mut cursor = 0;
    while let Some(relative) = lower[cursor..].find(needle) {
        let start = cursor + relative;
        let value_start = start + needle.len();
        output.push_str(&input[cursor..value_start]);
        output.push_str(REDACTED);
        let value_end = input[value_start..]
            .find(|character: char| {
                character.is_whitespace() || character == ',' || character == '"'
            })
            .map_or(input.len(), |offset| value_start + offset);
        cursor = value_end;
    }
    output.push_str(&input[cursor..]);
    output
}

pub(crate) struct StderrRing {
    capacity: usize,
    bytes: usize,
    lines: VecDeque<String>,
}

impl StderrRing {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            capacity,
            bytes: 0,
            lines: VecDeque::new(),
        }
    }

    pub(crate) fn push(&mut self, line: &str) {
        if self.capacity == 0 {
            return;
        }
        let mut line = redact_line(line);
        if line.len() > self.capacity {
            line = utf8_suffix(&line, self.capacity).to_owned();
        }
        let cost = line.len() + usize::from(!self.lines.is_empty());
        while self.bytes.saturating_add(cost) > self.capacity {
            let Some(removed) = self.lines.pop_front() else {
                break;
            };
            self.bytes = self.bytes.saturating_sub(removed.len());
            if !self.lines.is_empty() {
                self.bytes = self.bytes.saturating_sub(1);
            }
        }
        if !self.lines.is_empty() {
            self.bytes += 1;
        }
        self.bytes += line.len();
        self.lines.push_back(line);
    }

    pub(crate) fn tail(&self) -> Option<String> {
        (!self.lines.is_empty()).then(|| self.lines.iter().cloned().collect::<Vec<_>>().join("\n"))
    }
}

fn utf8_suffix(input: &str, max_bytes: usize) -> &str {
    if input.len() <= max_bytes {
        return input;
    }
    let mut start = input.len() - max_bytes;
    while !input.is_char_boundary(start) {
        start += 1;
    }
    &input[start..]
}

#[cfg(test)]
mod tests {
    use super::{StderrRing, redact_line};

    #[test]
    fn redacts_common_secret_forms() {
        let line = concat!(
            "Authorization: Bearer abc.def, ",
            "OPENAI_API_KEY=sk-test ",
            "{\"access_token\":\"token-value\",\"message\":\"ok\"}"
        );
        let redacted = redact_line(line);
        assert!(!redacted.contains("abc.def"));
        assert!(!redacted.contains("sk-test"));
        assert!(!redacted.contains("token-value"));
        assert!(redacted.contains("<redacted>"));
        assert!(redacted.contains("\"message\":\"ok\""));
    }

    #[test]
    fn stderr_ring_is_bounded_and_redacted() {
        let mut ring = StderrRing::new(24);
        ring.push("first");
        ring.push("token=top-secret");
        ring.push("last");
        let tail = ring.tail().expect("tail");
        assert!(tail.len() <= 24);
        assert!(!tail.contains("top-secret"));
        assert!(tail.contains("last"));
    }
}
