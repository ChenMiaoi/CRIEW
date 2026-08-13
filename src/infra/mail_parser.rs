//! Minimal mail-header parsing used by sync and reply flows.
//!
//! CRIEW only needs a narrow subset of RFC mail parsing for threading and
//! reply generation, so this module deliberately extracts just the fields that
//! affect visible behavior instead of pulling a heavier MIME model everywhere.

use std::collections::HashSet;

#[derive(Debug, Clone)]
pub struct ParsedMailHeaders {
    pub message_id: String,
    pub subject: String,
    pub from_addr: String,
    pub to_addresses: Vec<String>,
    pub cc_addresses: Vec<String>,
    pub date: Option<String>,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
    pub list_id: Option<String>,
    pub change_id: Option<String>,
    pub trailers: Vec<ParsedTrailer>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedTrailer {
    pub kind: String,
    pub value: String,
}

pub fn parse_headers(raw: &[u8], fallback_message_id: String) -> ParsedMailHeaders {
    let headers = parse_header_block(raw);

    let message_id = header_value(&headers, "message-id")
        .and_then(|value| parse_message_ids(&value).into_iter().next())
        .unwrap_or(fallback_message_id);

    let in_reply_to = header_value(&headers, "in-reply-to")
        .and_then(|value| parse_message_ids(&value).into_iter().next());

    let mut references = header_value(&headers, "references")
        .map(|value| parse_message_ids(&value))
        .unwrap_or_default();

    if references.is_empty()
        && let Some(reply_to) = in_reply_to.as_ref()
    {
        // Some mails only provide `In-Reply-To`. Reusing it as a one-element
        // reference chain keeps threading and reply reconstruction consistent
        // with the more complete cases.
        references.push(reply_to.clone());
    }

    let mut dedup = HashSet::new();
    references.retain(|id| dedup.insert(id.clone()));

    let to_addresses = header_value(&headers, "to")
        .map(|value| parse_address_list(&value))
        .unwrap_or_default();
    let cc_addresses = header_value(&headers, "cc")
        .map(|value| parse_address_list(&value))
        .unwrap_or_default();

    ParsedMailHeaders {
        message_id,
        subject: header_value(&headers, "subject").unwrap_or_default(),
        from_addr: header_value(&headers, "from").unwrap_or_default(),
        to_addresses,
        cc_addresses,
        date: header_value(&headers, "date").filter(|value| !value.is_empty()),
        in_reply_to,
        references,
        list_id: header_value(&headers, "list-id").filter(|value| !value.is_empty()),
        change_id: header_value(&headers, "change-id")
            .or_else(|| header_value(&headers, "x-change-id"))
            .or_else(|| header_value(&headers, "x-series-change-id"))
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty()),
        trailers: parse_review_trailers(raw),
    }
}

fn parse_review_trailers(raw: &[u8]) -> Vec<ParsedTrailer> {
    let text = String::from_utf8_lossy(raw);
    let body = if let Some(separator) = text.find("\r\n\r\n") {
        &text[separator + 4..]
    } else if let Some(separator) = text.find("\n\n") {
        &text[separator + 2..]
    } else {
        return Vec::new();
    };

    let mut trailers: Vec<ParsedTrailer> = Vec::new();
    let mut current_index: Option<usize> = None;
    for raw_line in body.lines() {
        let line = raw_line.trim_end_matches('\r');
        if line.trim_start().starts_with('>') {
            current_index = None;
            continue;
        }
        if let Some(index) = current_index
            && (line.starts_with(' ') || line.starts_with('\t'))
            && !line.trim().is_empty()
        {
            trailers[index].value.push(' ');
            trailers[index].value.push_str(line.trim());
            continue;
        }

        let Some((kind, value)) = review_trailer_line(line) else {
            current_index = None;
            continue;
        };

        trailers.push(ParsedTrailer { kind, value });
        current_index = Some(trailers.len() - 1);
    }

    trailers
}

fn review_trailer_line(line: &str) -> Option<(String, String)> {
    if line.trim_start().starts_with('>') {
        return None;
    }

    let (kind, value) = line.split_once(':')?;
    let kind = canonical_review_trailer_kind(kind.trim())?;
    let value = value.trim();
    if value.is_empty() {
        return None;
    }

    Some((kind.to_string(), value.to_string()))
}

fn canonical_review_trailer_kind(kind: &str) -> Option<&'static str> {
    [
        "Reviewed-by",
        "Acked-by",
        "Tested-by",
        "Reported-by",
        "Suggested-by",
        "Co-developed-by",
        "Signed-off-by",
    ]
    .into_iter()
    .find(|candidate| candidate.eq_ignore_ascii_case(kind))
}

pub fn normalize_subject(subject: &str) -> String {
    let mut normalized = subject.trim().to_ascii_lowercase();

    loop {
        let trimmed = normalized.trim_start();
        if let Some(rest) = trimmed.strip_prefix("re:") {
            normalized = rest.trim_start().to_string();
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("fwd:") {
            normalized = rest.trim_start().to_string();
            continue;
        }
        break;
    }

    loop {
        let trimmed = normalized.trim_start();
        if !trimmed.starts_with('[') {
            normalized = trimmed.to_string();
            break;
        }

        if let Some(index) = trimmed.find(']') {
            normalized = trimmed[index + 1..].trim_start().to_string();
            continue;
        }

        normalized = trimmed.to_string();
        break;
    }

    normalized.trim().to_string()
}

fn parse_header_block(raw: &[u8]) -> Vec<(String, String)> {
    let text = String::from_utf8_lossy(raw);
    let mut headers = Vec::new();

    let mut current_name: Option<String> = None;
    let mut current_value = String::new();

    for raw_line in text.lines() {
        let line = raw_line.trim_end_matches('\r');
        if line.is_empty() {
            break;
        }

        if line.starts_with(' ') || line.starts_with('\t') {
            if current_name.is_some() {
                let fragment = line.trim();
                if !fragment.is_empty() {
                    if !current_value.is_empty() {
                        current_value.push(' ');
                    }
                    current_value.push_str(fragment);
                }
            }
            continue;
        }

        if let Some(name) = current_name.take() {
            headers.push((name, current_value.trim().to_string()));
            current_value.clear();
        }

        if let Some((name, value)) = line.split_once(':') {
            current_name = Some(name.trim().to_ascii_lowercase());
            current_value.push_str(value.trim());
        }
    }

    if let Some(name) = current_name.take() {
        headers.push((name, current_value.trim().to_string()));
    }

    headers
}

fn header_value(headers: &[(String, String)], name: &str) -> Option<String> {
    headers
        .iter()
        .find(|(header_name, _)| header_name == name)
        .map(|(_, value)| value.trim().to_string())
}

/// Parse RFC-style display-name addresses into normalized mailbox values.
///
/// CRIEW only needs the mailbox part for matching a user's identity. The
/// parser still tracks quoted strings and angle brackets so a display name or
/// a quoted comma does not split one recipient into multiple values.
pub fn parse_address_list(raw: &str) -> Vec<String> {
    let mut values = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut angle_depth = 0usize;

    for character in raw.chars() {
        match character {
            '"' => {
                in_quotes = !in_quotes;
                current.push(character);
            }
            '<' if !in_quotes => {
                angle_depth = angle_depth.saturating_add(1);
                current.push(character);
            }
            '>' if !in_quotes => {
                angle_depth = angle_depth.saturating_sub(1);
                current.push(character);
            }
            ',' if !in_quotes && angle_depth == 0 => {
                push_normalized_address(&mut values, &current);
                current.clear();
            }
            _ => current.push(character),
        }
    }

    push_normalized_address(&mut values, &current);
    values
}

/// Normalize one display-name or bare mailbox value for identity matching.
pub fn normalize_email_address(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    let candidate = trimmed
        .rfind('<')
        .and_then(|start| trimmed[start + 1..].find('>').map(|end| (start, end)))
        .map(|(start, end)| &trimmed[start + 1..start + 1 + end])
        .unwrap_or(trimmed)
        .trim()
        .trim_matches(['"', '\'', '<', '>', ',', ';']);

    let candidate = candidate
        .split_whitespace()
        .find(|part| part.contains('@'))
        .unwrap_or(candidate)
        .trim_matches(['"', '\'', '<', '>', ',', ';']);

    if candidate.is_empty()
        || !candidate.contains('@')
        || candidate.contains(['<', '>', '"', '\'', '\n', '\r'])
    {
        return None;
    }

    Some(candidate.to_ascii_lowercase())
}

fn push_normalized_address(values: &mut Vec<String>, raw: &str) {
    let Some(address) = normalize_email_address(raw) else {
        return;
    };
    if !values.iter().any(|value| value == &address) {
        values.push(address);
    }
}

fn parse_message_ids(raw: &str) -> Vec<String> {
    let mut ids = Vec::new();

    let mut capture = false;
    let mut current = String::new();
    for ch in raw.chars() {
        if ch == '<' {
            capture = true;
            current.clear();
            continue;
        }

        if ch == '>' {
            if capture {
                let normalized = normalize_message_id(&current);
                if !normalized.is_empty() {
                    ids.push(normalized);
                }
            }
            capture = false;
            current.clear();
            continue;
        }

        if capture {
            current.push(ch);
        }
    }

    if ids.is_empty() {
        for part in raw.split_whitespace() {
            let normalized = normalize_message_id(part);
            if !normalized.is_empty() {
                ids.push(normalized);
            }
        }
    }

    ids
}

fn normalize_message_id(value: &str) -> String {
    value
        .trim()
        .trim_matches('<')
        .trim_matches('>')
        .trim_matches('"')
        .trim_matches(',')
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::{normalize_email_address, normalize_subject, parse_address_list, parse_headers};

    #[test]
    fn parses_basic_headers_and_reference_chain() {
        let raw = b"Message-ID: <root@example.com>\r\nSubject: [PATCH] demo\r\nFrom: Alice <alice@example.com>\r\nTo: Bob <bob@example.com>\r\nCc: List <list@example.com>\r\nReferences: <a@example.com> <b@example.com>\r\nIn-Reply-To: <b@example.com>\r\nX-Series-Change-ID: demo-series\r\n\r\nbody\r\n";

        let parsed = parse_headers(raw, "fallback@example.com".to_string());
        assert_eq!(parsed.message_id, "root@example.com");
        assert_eq!(parsed.to_addresses, vec!["bob@example.com"]);
        assert_eq!(parsed.cc_addresses, vec!["list@example.com"]);
        assert_eq!(parsed.in_reply_to.as_deref(), Some("b@example.com"));
        assert_eq!(parsed.references, vec!["a@example.com", "b@example.com"]);
        assert_eq!(parsed.change_id.as_deref(), Some("demo-series"));
    }

    #[test]
    fn falls_back_to_generated_message_id() {
        let raw = b"Subject: no id\r\n\r\nbody\r\n";
        let parsed = parse_headers(raw, "synthetic@example.com".to_string());
        assert_eq!(parsed.message_id, "synthetic@example.com");
    }

    #[test]
    fn folds_continuation_lines() {
        let raw = b"Message-ID: <fold@example.com>\r\nReferences: <a@example.com>\r\n <b@example.com>\r\n\r\n";
        let parsed = parse_headers(raw, "fallback@example.com".to_string());
        assert_eq!(parsed.references, vec!["a@example.com", "b@example.com"]);
    }

    #[test]
    fn normalizes_common_subject_prefixes() {
        assert_eq!(normalize_subject("Re: [PATCH v2 0/3] Demo"), "demo");
        assert_eq!(normalize_subject("fwd:  Re: status"), "status");
    }

    #[test]
    fn parses_review_trailers_and_ignores_quoted_values() {
        let raw = b"Message-ID: <patch@example.com>\nSubject: [PATCH] demo\n\nReviewed-by: Alice Reviewer <alice@example.com>\nAcked-by: Bob <bob@example.com>\n > Reviewed-by: quoted@example.com\nTested-by: Carol <carol@example.com>\n";

        let parsed = parse_headers(raw, "fallback@example.com".to_string());

        assert_eq!(
            parsed.trailers,
            vec![
                super::ParsedTrailer {
                    kind: "Reviewed-by".to_string(),
                    value: "Alice Reviewer <alice@example.com>".to_string(),
                },
                super::ParsedTrailer {
                    kind: "Acked-by".to_string(),
                    value: "Bob <bob@example.com>".to_string(),
                },
                super::ParsedTrailer {
                    kind: "Tested-by".to_string(),
                    value: "Carol <carol@example.com>".to_string(),
                },
            ]
        );
    }

    #[test]
    fn folds_review_trailer_continuation_lines() {
        let raw = b"Message-ID: <patch@example.com>\r\n\r\nReviewed-by: Alice\r\n <alice@example.com>\r\nSigned-off-by: Author <author@example.com>\r\n";

        let parsed = parse_headers(raw, "fallback@example.com".to_string());

        assert_eq!(parsed.trailers[0].value, "Alice <alice@example.com>");
        assert_eq!(parsed.trailers[1].kind, "Signed-off-by");
    }

    #[test]
    fn parses_quoted_commas_and_bare_addresses() {
        assert_eq!(
            parse_address_list("\"Kernel, Bot\" <bot@example.com>, reviewer@example.com"),
            vec!["bot@example.com", "reviewer@example.com"]
        );
        assert_eq!(
            normalize_email_address("Alice <ALICE@EXAMPLE.COM>"),
            Some("alice@example.com".to_string())
        );
    }
}
