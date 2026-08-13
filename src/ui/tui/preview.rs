//! Mail preview extraction for the preview pane.
//!
//! The preview pane should remain useful even when raw mail is missing or only
//! partially parsed, so this module is intentionally biased toward graceful
//! fallback text instead of strict MIME fidelity.

use std::fs;

use crate::infra::mail_store::ThreadRow;

use super::{PREVIEW_RECIPIENT_PREVIEW_LIMIT, PREVIEW_TAB_SPACES};

#[derive(Debug, Clone)]
pub(super) struct MailPreview {
    pub warning: Option<String>,
    pub content: String,
}

pub(super) fn load_mail_preview(thread: &ThreadRow) -> MailPreview {
    let subject = normalized_subject_or_default(&thread.subject);
    let fallback_from = normalized_header_or_default(&thread.from_addr, "<unknown sender>");
    let fallback_sent = thread.date.as_deref().and_then(non_empty_normalized_header);

    let Some(path) = thread.raw_path.as_ref() else {
        // Thread rows can outlive raw-file cleanup or fixture gaps. Rendering a
        // synthetic preview keeps navigation usable instead of failing closed.
        return MailPreview {
            warning: None,
            content: format_preview_with_headers(
                &fallback_from,
                fallback_sent.as_deref().unwrap_or("<unknown sent time>"),
                "<none>",
                "<none>",
                &subject,
                "<raw mail file unavailable>",
            ),
        };
    };

    let content = match fs::read(path) {
        Ok(value) => value,
        Err(error) => {
            return MailPreview {
                warning: None,
                content: format_preview_with_headers(
                    &fallback_from,
                    fallback_sent.as_deref().unwrap_or("<unknown sent time>"),
                    "<none>",
                    "<none>",
                    &subject,
                    &format!("<failed to read {}: {}>", path.display(), error),
                ),
            };
        }
    };

    build_mail_preview(&content, &subject, &fallback_from, fallback_sent.as_deref())
}

#[cfg(test)]
pub(super) fn extract_mail_preview(
    raw: &[u8],
    fallback_subject: &str,
    fallback_from: &str,
    fallback_sent: Option<&str>,
) -> String {
    build_mail_preview(raw, fallback_subject, fallback_from, fallback_sent).content
}

#[cfg(test)]
pub(super) fn preview_warning_message(raw: &[u8]) -> Option<String> {
    let headers = parse_preview_header_block(raw);
    build_preview_warning(&headers)
}

fn build_mail_preview(
    raw: &[u8],
    fallback_subject: &str,
    fallback_from: &str,
    fallback_sent: Option<&str>,
) -> MailPreview {
    // Re-parse a narrow header view here instead of sharing richer sync data so
    // preview rendering can tolerate incomplete stored metadata and still use
    // whatever survives in the raw message.
    let headers = parse_preview_header_block(raw);

    let from = preview_header_value(&headers, "from")
        .or_else(|| non_empty_normalized_header(fallback_from))
        .unwrap_or_else(|| "<unknown sender>".to_string());
    let sent = preview_header_value(&headers, "date")
        .or_else(|| fallback_sent.and_then(non_empty_normalized_header))
        .unwrap_or_else(|| "<unknown sent time>".to_string());
    let to = preview_recipient_line(&headers, "to");
    let cc = preview_recipient_line(&headers, "cc");
    let subject = preview_header_value(&headers, "subject")
        .or_else(|| non_empty_normalized_header(fallback_subject))
        .unwrap_or_else(|| "(no subject)".to_string());
    let body = extract_mail_body_preview(raw);

    MailPreview {
        warning: build_preview_warning(&headers),
        content: format_preview_with_headers(&from, &sent, &to, &cc, &subject, &body),
    }
}

fn format_preview_with_headers(
    from: &str,
    sent: &str,
    to: &str,
    cc: &str,
    subject: &str,
    body: &str,
) -> String {
    format!("From: {from}\nSent: {sent}\nTo: {to}\nCc: {cc}\nSubject: {subject}\n\n{body}")
}

fn normalized_subject_or_default(subject: &str) -> String {
    non_empty_normalized_header(subject).unwrap_or_else(|| "(no subject)".to_string())
}

fn non_empty_normalized_header(value: &str) -> Option<String> {
    let normalized = normalize_preview_header_whitespace(value);
    if normalized.is_empty() {
        None
    } else {
        Some(normalized)
    }
}

fn normalized_header_or_default(value: &str, default: &str) -> String {
    non_empty_normalized_header(value).unwrap_or_else(|| default.to_string())
}

fn parse_preview_header_block(raw: &[u8]) -> Vec<(String, String)> {
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
            headers.push((name, normalize_preview_header_whitespace(&current_value)));
            current_value.clear();
        }

        if let Some((name, value)) = line.split_once(':') {
            current_name = Some(name.trim().to_ascii_lowercase());
            current_value.push_str(value.trim());
        }
    }

    if let Some(name) = current_name.take() {
        headers.push((name, normalize_preview_header_whitespace(&current_value)));
    }

    headers
}

fn preview_header_value(headers: &[(String, String)], name: &str) -> Option<String> {
    headers
        .iter()
        .find(|(header_name, _)| header_name == name)
        .and_then(|(_, value)| non_empty_normalized_header(value))
}

fn preview_header_values(headers: &[(String, String)], name: &str) -> Vec<String> {
    headers
        .iter()
        .filter(|(header_name, _)| header_name == name)
        .filter_map(|(_, value)| non_empty_normalized_header(value))
        .collect()
}

fn preview_recipient_line(headers: &[(String, String)], name: &str) -> String {
    let mut recipients = Vec::new();
    for value in preview_header_values(headers, name) {
        recipients.extend(split_recipient_list(&value));
    }

    if recipients.is_empty() {
        return "<none>".to_string();
    }

    if recipients.len() <= PREVIEW_RECIPIENT_PREVIEW_LIMIT {
        return recipients.join("; ");
    }

    format!(
        "{}; ...",
        recipients[..PREVIEW_RECIPIENT_PREVIEW_LIMIT].join("; ")
    )
}

fn split_recipient_list(value: &str) -> Vec<String> {
    let mut recipients = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut escaped = false;
    let mut angle_depth = 0usize;

    for character in value.chars() {
        if in_quotes {
            current.push(character);
            if escaped {
                escaped = false;
                continue;
            }

            match character {
                '\\' => escaped = true,
                '"' => in_quotes = false,
                _ => {}
            }
            continue;
        }

        match character {
            '"' => {
                in_quotes = true;
                current.push(character);
            }
            '<' => {
                angle_depth += 1;
                current.push(character);
            }
            '>' => {
                angle_depth = angle_depth.saturating_sub(1);
                current.push(character);
            }
            ',' | ';' if angle_depth == 0 => {
                if let Some(recipient) = non_empty_normalized_header(&current) {
                    recipients.push(recipient);
                }
                current.clear();
            }
            _ => current.push(character),
        }
    }

    if let Some(recipient) = non_empty_normalized_header(&current) {
        recipients.push(recipient);
    }

    recipients
}

fn normalize_preview_header_whitespace(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn build_preview_warning(headers: &[(String, String)]) -> Option<String> {
    let content_type = preview_header_value(headers, "content-type");
    let transfer_encoding = preview_header_value(headers, "content-transfer-encoding");
    let content_type_lower = content_type
        .as_deref()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let transfer_encoding_lower = transfer_encoding
        .as_deref()
        .unwrap_or_default()
        .to_ascii_lowercase();

    let non_plain_content = (!content_type_lower.is_empty()
        && !content_type_lower.starts_with("text/plain")
        && !content_type_lower.starts_with("message/rfc822"))
        || content_type_lower.contains("multipart/")
        || content_type_lower.contains("text/html");
    let encoded_content = transfer_encoding_lower.contains("base64")
        || transfer_encoding_lower.contains("quoted-printable");

    if !non_plain_content && !encoded_content {
        return None;
    }

    let mut lines = vec![
        "[!] NON-PLAIN-TEXT MAIL".to_string(),
        "[!] Preview is best-effort only.".to_string(),
        "[!] Parse artifacts/errors are normal here.".to_string(),
    ];
    if let Some(value) = content_type {
        lines.push(format!("[!] Content-Type: {value}"));
    }
    if let Some(value) = transfer_encoding {
        lines.push(format!("[!] Transfer-Encoding: {value}"));
    }

    Some(lines.join("\n"))
}

pub(super) fn extract_mail_body_text(raw: &[u8]) -> String {
    let body_start = find_subslice(raw, b"\r\n\r\n")
        .map(|index| index + 4)
        .or_else(|| find_subslice(raw, b"\n\n").map(|index| index + 2))
        .unwrap_or(0);

    let body = &raw[body_start..];
    let transfer_encoding = preview_header_value(
        &parse_preview_header_block(raw),
        "content-transfer-encoding",
    )
    .unwrap_or_default()
    .to_ascii_lowercase();
    let decoded = match transfer_encoding.as_str() {
        value if value.contains("quoted-printable") => decode_quoted_printable(body),
        value if value.contains("base64") => decode_base64(body),
        _ => body.to_vec(),
    };
    let text = String::from_utf8_lossy(&decoded).replace("\r\n", "\n");
    let stripped = strip_first_mime_part_headers(&text);

    let sanitized = sanitize_preview_text(&stripped);

    sanitized
        .lines()
        .map(str::trim_end)
        .skip_while(|line| line.trim().is_empty())
        .collect::<Vec<&str>>()
        .join("\n")
}

pub(super) fn extract_mail_body_preview(raw: &[u8]) -> String {
    let body_text = extract_mail_body_text(raw);
    if body_text.trim().is_empty() {
        "<empty mail body>".to_string()
    } else {
        body_text
    }
}

fn decode_quoted_printable(raw: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(raw.len());
    let mut index = 0;
    while index < raw.len() {
        if raw[index] == b'=' {
            if index + 2 < raw.len() && raw[index + 1] == b'\r' && raw[index + 2] == b'\n' {
                index += 3;
                continue;
            }
            if index + 2 < raw.len() && raw[index + 1] == b'\n' {
                index += 2;
                continue;
            }
            if index + 2 < raw.len() {
                let high = hex_value(raw[index + 1]);
                let low = hex_value(raw[index + 2]);
                if let (Some(high), Some(low)) = (high, low) {
                    output.push((high << 4) | low);
                    index += 3;
                    continue;
                }
            }
        }
        output.push(raw[index]);
        index += 1;
    }
    output
}

fn hex_value(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

fn decode_base64(raw: &[u8]) -> Vec<u8> {
    let mut output = Vec::new();
    let mut buffer = 0u32;
    let mut bits = 0u8;
    for value in raw.iter().copied() {
        let Some(value) = base64_value(value) else {
            continue;
        };
        buffer = (buffer << 6) | u32::from(value);
        bits += 6;
        while bits >= 8 {
            bits -= 8;
            output.push(((buffer >> bits) & 0xff) as u8);
        }
    }
    output
}

fn base64_value(value: u8) -> Option<u8> {
    match value {
        b'A'..=b'Z' => Some(value - b'A'),
        b'a'..=b'z' => Some(value - b'a' + 26),
        b'0'..=b'9' => Some(value - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

fn sanitize_preview_text(input: &str) -> String {
    let mut sanitized = String::with_capacity(input.len());
    for character in input.chars() {
        match character {
            '\n' => sanitized.push('\n'),
            '\t' => sanitized.push_str(PREVIEW_TAB_SPACES),
            _ if character.is_control() => {}
            _ => sanitized.push(character),
        }
    }
    sanitized
}

fn strip_first_mime_part_headers(body: &str) -> String {
    let lines: Vec<&str> = body.lines().collect();
    let Some(first_non_empty_index) = lines.iter().position(|line| !line.trim().is_empty()) else {
        return String::new();
    };

    let boundary = lines[first_non_empty_index].trim();
    if !boundary.starts_with("--") {
        return body.to_string();
    }

    let mut cursor = first_non_empty_index + 1;
    while cursor < lines.len() && !lines[cursor].trim().is_empty() {
        cursor += 1;
    }

    if cursor >= lines.len() {
        return body.to_string();
    }

    let content_start = cursor + 1;
    let mut content = Vec::new();
    let closing_boundary = format!("{boundary}--");
    for line in &lines[content_start..] {
        let trimmed = line.trim();
        if trimmed == boundary || trimmed == closing_boundary {
            break;
        }
        content.push(line.trim_end());
    }

    if content.is_empty() {
        body.to_string()
    } else {
        content.join("\n")
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }

    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}
