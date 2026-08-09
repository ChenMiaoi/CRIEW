//! Reply composition helpers for the TUI.
//!
//! Reply generation keeps header normalization, recipient filtering, and quoted
//! body construction together so preview and send flows share one consistent
//! interpretation of what the outbound message should look like.

use std::collections::HashSet;
use std::process::Command as ProcessCommand;

use crate::infra::mail_parser::parse_headers;
use crate::infra::mail_store::ThreadRow;

use super::preview::extract_mail_body_text;

const GIT_SENDEMAIL_FROM_ARGS: &[&str] = &["config", "sendemail.from"];
const GIT_USER_NAME_LOOKUP_ARGS: &[&str] = &["config", "user.name"];
const GIT_USER_EMAIL_LOOKUP_ARGS: &[&str] = &["config", "user.email"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ReplyIdentity {
    pub display: String,
    pub email: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ReplySeed {
    pub from: String,
    pub to: String,
    pub cc: String,
    pub subject: String,
    pub in_reply_to: String,
    pub references: Vec<String>,
    pub body: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ReplyPreviewLineKind {
    Header,
    Blank,
    Authored,
    QuoteAttribution,
    Quoted,
    Placeholder,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ReplyPreviewLine {
    pub kind: ReplyPreviewLineKind,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ReplyPreview {
    pub content: String,
    pub lines: Vec<ReplyPreviewLine>,
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PreparedReplyMessage {
    pub from: String,
    pub from_email: Option<String>,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub subject: String,
    pub in_reply_to: String,
    pub references: Vec<String>,
    pub body: String,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct ReplyPreviewRequest<'a> {
    pub from: &'a str,
    pub to: &'a str,
    pub cc: &'a str,
    pub subject: &'a str,
    pub in_reply_to: &'a str,
    pub references: &'a [String],
    pub body: &'a [String],
    pub self_addresses: &'a [String],
}

pub(super) fn resolve_git_identity() -> std::result::Result<ReplyIdentity, String> {
    if let Some(value) = git_config_value(GIT_SENDEMAIL_FROM_ARGS)? {
        return parse_identity(&value).ok_or_else(|| {
            "git config sendemail.from is set but does not contain a valid email address"
                .to_string()
        });
    }

    let email = git_config_value(GIT_USER_EMAIL_LOOKUP_ARGS)?.ok_or_else(|| {
        "git email identity missing; set git config sendemail.from or user.email".to_string()
    })?;
    let name = git_config_value(GIT_USER_NAME_LOOKUP_ARGS)?;

    let display = if let Some(name) = name {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            email.clone()
        } else {
            format!("{trimmed} <{email}>")
        }
    } else {
        email.clone()
    };

    Ok(ReplyIdentity { display, email })
}

pub(super) fn build_reply_seed(
    raw: &[u8],
    thread: &ThreadRow,
    identity: &ReplyIdentity,
    self_addresses: &[String],
) -> ReplySeed {
    let parsed = parse_headers(raw, thread.message_id.clone());
    let headers = parse_header_block(raw);
    let self_set = collect_self_addresses(identity, self_addresses);

    let mut to = normalize_to_recipient_values(header_values(&headers, "to"), &self_set);
    let mut cc_dedup = self_set.clone();
    cc_dedup.extend(
        to.iter()
            .filter_map(|value| extract_email_address(value))
            .map(|value| value.to_ascii_lowercase()),
    );
    let cc = normalize_recipient_values(header_values(&headers, "cc"), &cc_dedup);

    if to.is_empty()
        && let Some(author) = normalize_recipient_display(if parsed.from_addr.trim().is_empty() {
            &thread.from_addr
        } else {
            &parsed.from_addr
        })
        && let Some(email) = extract_email_address(&author)
        && !self_set.contains(&email.to_ascii_lowercase())
    {
        to.push(author);
    }

    let mut references = parsed.references;
    if !parsed.message_id.trim().is_empty()
        && !references.iter().any(|value| value == &parsed.message_id)
    {
        references.push(parsed.message_id.clone());
    }

    let sent_at = parsed
        .date
        .as_deref()
        .or(thread.date.as_deref())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("<unknown sent time>");
    let author = if parsed.from_addr.trim().is_empty() {
        thread.from_addr.as_str()
    } else {
        parsed.from_addr.as_str()
    };

    ReplySeed {
        from: identity.display.clone(),
        to: to.join(", "),
        cc: cc.join(", "),
        subject: normalize_reply_subject(if parsed.subject.trim().is_empty() {
            &thread.subject
        } else {
            &parsed.subject
        }),
        in_reply_to: parsed.message_id,
        references,
        body: build_reply_body(raw, sent_at, author),
    }
}

pub(super) fn render_reply_preview(request: ReplyPreviewRequest<'_>) -> ReplyPreview {
    let (prepared, errors) = prepare_reply_message(request);
    let body_lines = render_reply_preview_body_lines(request.body);
    let lines = render_prepared_reply_preview_lines(&prepared, &body_lines);
    ReplyPreview {
        content: render_reply_preview_text(&lines),
        lines,
        errors,
        warnings: build_reply_preview_warnings(&body_lines),
    }
}

pub(super) fn prepare_reply_message(
    request: ReplyPreviewRequest<'_>,
) -> (PreparedReplyMessage, Vec<String>) {
    let mut errors = Vec::new();

    let from = normalize_header_value(request.from);
    let from_email = extract_email_address(&from);
    if from_email.is_none() {
        errors.push("From is missing a valid email address".to_string());
    }

    // Treat the sender and any configured self aliases as one dedup set so the
    // preview can warn about genuinely empty recipient lists after self-removal.
    let mut self_set = request
        .self_addresses
        .iter()
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| !value.is_empty())
        .collect::<HashSet<String>>();
    if let Some(email) = from_email.as_deref() {
        self_set.insert(email.to_ascii_lowercase());
    }

    let normalized_to = normalize_to_recipient_values([request.to.to_string()], &self_set);
    let mut cc_dedup = self_set;
    cc_dedup.extend(
        normalized_to
            .iter()
            .filter_map(|value| extract_email_address(value))
            .map(|value| value.to_ascii_lowercase()),
    );
    let normalized_cc = normalize_recipient_values([request.cc.to_string()], &cc_dedup);
    if normalized_to.is_empty() && normalized_cc.is_empty() {
        errors.push("reply preview has no recipients after removing self".to_string());
    }

    let in_reply_to = normalize_message_id(request.in_reply_to);
    let mut normalized_references =
        normalize_message_ids(request.references.iter().map(String::as_str));
    let independent_message = in_reply_to.is_empty() && normalized_references.is_empty();
    let subject = if independent_message {
        normalize_header_value(request.subject)
    } else {
        normalize_reply_subject(request.subject)
    };
    if subject.is_empty() || subject == "Re:" {
        errors.push("Subject is empty".to_string());
    }

    if in_reply_to.is_empty() && !independent_message {
        errors.push("In-Reply-To is missing".to_string());
    }
    if normalized_references.is_empty() && !in_reply_to.is_empty() {
        normalized_references.push(in_reply_to.clone());
    }
    if !in_reply_to.is_empty()
        && !normalized_references
            .iter()
            .any(|value| value == &in_reply_to)
    {
        // Ensure the direct parent is always present even if the original
        // message had a truncated or malformed References chain.
        normalized_references.push(in_reply_to.clone());
    }

    (
        PreparedReplyMessage {
            from,
            from_email,
            to: normalized_to,
            cc: normalized_cc,
            subject,
            in_reply_to,
            references: normalized_references,
            body: render_reply_body(request.body),
        },
        errors,
    )
}

fn render_prepared_reply_preview_lines(
    message: &PreparedReplyMessage,
    body_lines: &[ReplyPreviewLine],
) -> Vec<ReplyPreviewLine> {
    let mut lines = vec![
        ReplyPreviewLine {
            kind: ReplyPreviewLineKind::Header,
            text: format!("From: {}", message.from),
        },
        ReplyPreviewLine {
            kind: ReplyPreviewLineKind::Header,
            text: format!("To: {}", render_recipient_line(&message.to)),
        },
        ReplyPreviewLine {
            kind: ReplyPreviewLineKind::Header,
            text: format!("Cc: {}", render_recipient_line(&message.cc)),
        },
        ReplyPreviewLine {
            kind: ReplyPreviewLineKind::Header,
            text: format!("Subject: {}", message.subject),
        },
        ReplyPreviewLine {
            kind: ReplyPreviewLineKind::Header,
            text: format!("In-Reply-To: {}", render_message_id(&message.in_reply_to)),
        },
        ReplyPreviewLine {
            kind: ReplyPreviewLineKind::Header,
            text: format!(
                "References: {}",
                render_references_line(&message.references)
            ),
        },
        ReplyPreviewLine {
            kind: ReplyPreviewLineKind::Blank,
            text: String::new(),
        },
    ];
    lines.extend(body_lines.iter().cloned());
    lines
}

fn render_reply_preview_text(lines: &[ReplyPreviewLine]) -> String {
    lines
        .iter()
        .map(|line| line.text.as_str())
        .collect::<Vec<&str>>()
        .join("\n")
}

fn git_config_value(args: &[&str]) -> std::result::Result<Option<String>, String> {
    let output = ProcessCommand::new("git")
        .args(args)
        .output()
        .map_err(|error| format!("failed to run git {}: {error}", args.join(" ")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if stderr.is_empty() {
            return Ok(None);
        }
        return Err(format!("git {} failed: {stderr}", args.join(" ")));
    }

    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if value.is_empty() {
        Ok(None)
    } else {
        Ok(Some(value))
    }
}

fn parse_identity(value: &str) -> Option<ReplyIdentity> {
    let display = normalize_header_value(value);
    let email = extract_email_address(&display)?;
    Some(ReplyIdentity { display, email })
}

fn collect_self_addresses(identity: &ReplyIdentity, self_addresses: &[String]) -> HashSet<String> {
    let mut collected = HashSet::new();
    collected.insert(identity.email.to_ascii_lowercase());
    for value in self_addresses {
        let normalized = value.trim().to_ascii_lowercase();
        if !normalized.is_empty() {
            collected.insert(normalized);
        }
    }
    collected
}

fn build_reply_body(raw: &[u8], sent_at: &str, author: &str) -> Vec<String> {
    let body_text = extract_mail_body_text(raw);
    let mut lines = vec![String::new(), format!("On {sent_at}, {author} wrote:")];
    if body_text.trim().is_empty() {
        lines.push("> <empty mail body>".to_string());
        return lines;
    }

    // Quote line-by-line instead of prefixing the whole block so editing stays
    // predictable when the user deletes or reflows only part of the quoted mail.
    lines.extend(
        body_text
            .lines()
            .map(|line| {
                let trimmed = line.trim_end();
                if trimmed.is_empty() {
                    ">".to_string()
                } else {
                    format!("> {trimmed}")
                }
            })
            .collect::<Vec<String>>(),
    );
    lines
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
            headers.push((name, normalize_header_value(&current_value)));
            current_value.clear();
        }

        if let Some((name, value)) = line.split_once(':') {
            current_name = Some(name.trim().to_ascii_lowercase());
            current_value.push_str(value.trim());
        }
    }

    if let Some(name) = current_name.take() {
        headers.push((name, normalize_header_value(&current_value)));
    }

    headers
}

fn header_values(headers: &[(String, String)], name: &str) -> Vec<String> {
    headers
        .iter()
        .filter(|(header_name, _)| header_name == name)
        .filter_map(|(_, value)| {
            let normalized = normalize_header_value(value);
            if normalized.is_empty() {
                None
            } else {
                Some(normalized)
            }
        })
        .collect()
}

fn normalize_to_recipient_values<I>(values: I, self_addresses: &HashSet<String>) -> Vec<String>
where
    I: IntoIterator<Item = String>,
{
    let normalized = collect_recipient_values(values);
    let filtered = filter_self_recipients(&normalized, self_addresses);
    if filtered.is_empty()
        && normalized.len() == 1
        && recipient_matches_self(&normalized[0], self_addresses)
    {
        normalized
    } else {
        filtered
    }
}

fn normalize_recipient_values<I>(values: I, self_addresses: &HashSet<String>) -> Vec<String>
where
    I: IntoIterator<Item = String>,
{
    let normalized = collect_recipient_values(values);
    filter_self_recipients(&normalized, self_addresses)
}

fn collect_recipient_values<I>(values: I) -> Vec<String>
where
    I: IntoIterator<Item = String>,
{
    let mut dedup = HashSet::new();
    let mut recipients = Vec::new();

    for value in values {
        for entry in split_recipient_line(&value) {
            let Some(display) = normalize_recipient_display(&entry) else {
                continue;
            };
            let key = extract_email_address(&display)
                .map(|email| email.to_ascii_lowercase())
                .unwrap_or_else(|| display.to_ascii_lowercase());
            if !dedup.insert(key) {
                continue;
            }
            recipients.push(display);
        }
    }

    recipients
}

fn filter_self_recipients(recipients: &[String], self_addresses: &HashSet<String>) -> Vec<String> {
    recipients
        .iter()
        .filter(|recipient| !recipient_matches_self(recipient, self_addresses))
        .cloned()
        .collect()
}

fn recipient_matches_self(value: &str, self_addresses: &HashSet<String>) -> bool {
    extract_email_address(value)
        .map(|email| self_addresses.contains(&email.to_ascii_lowercase()))
        .unwrap_or_else(|| self_addresses.contains(&value.to_ascii_lowercase()))
}

fn normalize_recipient_display(value: &str) -> Option<String> {
    let normalized = normalize_header_value(value);
    if normalized.is_empty() {
        None
    } else {
        Some(normalized)
    }
}

fn normalize_header_value(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn normalize_message_ids<'a, I>(values: I) -> Vec<String>
where
    I: IntoIterator<Item = &'a str>,
{
    let mut dedup = HashSet::new();
    values
        .into_iter()
        .map(normalize_message_id)
        .filter(|value| !value.is_empty())
        .filter(|value| dedup.insert(value.clone()))
        .collect()
}

fn normalize_message_id(value: &str) -> String {
    value
        .trim()
        .trim_matches(|character| matches!(character, '<' | '>' | '"' | ','))
        .trim()
        .to_string()
}

fn render_reply_body(body: &[String]) -> String {
    render_reply_preview_text(&render_reply_preview_body_lines(body))
}

fn render_reply_preview_body_lines(body: &[String]) -> Vec<ReplyPreviewLine> {
    let normalized_body = body
        .iter()
        .map(|line| line.trim_end().to_string())
        .collect::<Vec<String>>();
    if normalized_body.iter().all(|line| line.trim().is_empty()) {
        return vec![ReplyPreviewLine {
            kind: ReplyPreviewLineKind::Placeholder,
            text: "<empty body>".to_string(),
        }];
    }

    normalized_body
        .into_iter()
        .map(classify_reply_preview_line)
        .collect()
}

fn classify_reply_preview_line(text: String) -> ReplyPreviewLine {
    let trimmed = text.trim();
    let kind = if trimmed.is_empty() {
        ReplyPreviewLineKind::Blank
    } else if trimmed.starts_with('>') {
        ReplyPreviewLineKind::Quoted
    } else if is_quote_attribution_line(trimmed) {
        ReplyPreviewLineKind::QuoteAttribution
    } else {
        ReplyPreviewLineKind::Authored
    };

    ReplyPreviewLine { kind, text }
}

fn build_reply_preview_warnings(body_lines: &[ReplyPreviewLine]) -> Vec<String> {
    if body_lines
        .iter()
        .any(|line| matches!(line.kind, ReplyPreviewLineKind::Authored))
    {
        Vec::new()
    } else {
        vec![
            "draft has no authored reply content; only quoted text and generated scaffolding are visible"
                .to_string(),
        ]
    }
}

fn is_quote_attribution_line(value: &str) -> bool {
    value
        .strip_prefix("On ")
        .is_some_and(|remaining| remaining.ends_with(" wrote:"))
}

fn render_recipient_line(values: &[String]) -> String {
    if values.is_empty() {
        "<none>".to_string()
    } else {
        values.join(", ")
    }
}

fn render_message_id(value: &str) -> String {
    let normalized = normalize_message_id(value);
    if normalized.is_empty() {
        "<none>".to_string()
    } else {
        format!("<{normalized}>")
    }
}

fn render_references_line(values: &[String]) -> String {
    if values.is_empty() {
        "<none>".to_string()
    } else {
        values
            .iter()
            .map(|value| render_message_id(value))
            .collect::<Vec<String>>()
            .join(" ")
    }
}

fn reply_subject_prefix_len(subject: &str) -> Option<usize> {
    let lowered = subject.to_ascii_lowercase();
    for prefix in ["re:", "fwd:", "fw:"] {
        if lowered.starts_with(prefix) {
            return Some(prefix.len());
        }
    }
    None
}

pub(super) fn normalize_reply_subject(subject: &str) -> String {
    let mut trimmed = subject.trim();
    while let Some(prefix_len) = reply_subject_prefix_len(trimmed) {
        trimmed = trimmed[prefix_len..].trim_start();
    }

    if trimmed.is_empty() {
        "Re:".to_string()
    } else {
        format!("Re: {trimmed}")
    }
}

pub(super) fn split_recipient_line(value: &str) -> Vec<String> {
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
                if let Some(recipient) = normalize_recipient_display(&current) {
                    recipients.push(recipient);
                }
                current.clear();
            }
            _ => current.push(character),
        }
    }

    if let Some(recipient) = normalize_recipient_display(&current) {
        recipients.push(recipient);
    }

    recipients
}

pub(super) fn extract_email_address(value: &str) -> Option<String> {
    if let Some((_, tail)) = value.rsplit_once('<')
        && let Some((email, _)) = tail.split_once('>')
    {
        let normalized = normalize_message_id(email);
        if !normalized.is_empty() {
            return Some(normalized);
        }
    }

    let candidate = value
        .split_whitespace()
        .find(|token| token.contains('@'))
        .map(normalize_message_id)?;
    if candidate.is_empty() {
        None
    } else {
        Some(candidate)
    }
}

#[cfg(test)]
mod tests {
    use crate::infra::mail_store::ThreadRow;

    use super::{
        ReplyIdentity, ReplyPreviewLineKind, ReplyPreviewRequest, build_reply_seed,
        extract_email_address, normalize_reply_subject, prepare_reply_message,
        render_reply_preview,
    };

    fn sample_thread(subject: &str, message_id: &str) -> ThreadRow {
        ThreadRow {
            thread_id: 1,
            mail_id: 1,
            depth: 0,
            subject: subject.to_string(),
            from_addr: "Alice <alice@example.com>".to_string(),
            message_id: message_id.to_string(),
            in_reply_to: None,
            date: Some("Fri, 6 Mar 2026 09:30:00 +0000".to_string()),
            raw_path: None,
        }
    }

    fn identity() -> ReplyIdentity {
        ReplyIdentity {
            display: "CRIEW Test <criew@example.com>".to_string(),
            email: "criew@example.com".to_string(),
        }
    }

    #[test]
    fn normalize_reply_subject_keeps_single_re_prefix() {
        assert_eq!(
            normalize_reply_subject("Re: [PATCH v3 2/7] mm: fix foo"),
            "Re: [PATCH v3 2/7] mm: fix foo"
        );
        assert_eq!(
            normalize_reply_subject("re: Re: [PATCH] demo"),
            "Re: [PATCH] demo"
        );
        assert_eq!(
            normalize_reply_subject("fwd: [PATCH] demo"),
            "Re: [PATCH] demo"
        );
    }

    #[test]
    fn build_reply_seed_dedups_and_removes_self() {
        let raw = b"Message-ID: <patch@example.com>\r\nSubject: [PATCH] demo\r\nFrom: Alice <alice@example.com>\r\nTo: CRIEW Test <criew@example.com>, Bob <bob@example.com>\r\nCc: Bob <bob@example.com>; Alice <alice@example.com>\r\nDate: Fri, 6 Mar 2026 09:30:00 +0000\r\n\r\nbody line\r\n";
        let thread = sample_thread("[PATCH] demo", "patch@example.com");

        let seed = build_reply_seed(
            raw,
            &thread,
            &identity(),
            &[identity().email.clone(), "alias@example.com".to_string()],
        );

        assert_eq!(seed.from, "CRIEW Test <criew@example.com>");
        assert_eq!(seed.to, "Bob <bob@example.com>");
        assert_eq!(seed.cc, "Alice <alice@example.com>");
        assert_eq!(seed.subject, "Re: [PATCH] demo");
        assert_eq!(seed.in_reply_to, "patch@example.com");
        assert_eq!(seed.references, vec!["patch@example.com"]);
        assert_eq!(
            seed.body[1],
            "On Fri, 6 Mar 2026 09:30:00 +0000, Alice <alice@example.com> wrote:"
        );
        assert_eq!(seed.body[2], "> body line");
    }

    #[test]
    fn build_reply_seed_preserves_single_self_to() {
        let raw = b"Message-ID: <patch@example.com>\r\nSubject: [PATCH] demo\r\nFrom: Alice <alice@example.com>\r\nTo: CRIEW Test <criew@example.com>\r\nDate: Fri, 6 Mar 2026 09:30:00 +0000\r\n\r\nbody line\r\n";
        let thread = sample_thread("[PATCH] demo", "patch@example.com");

        let seed = build_reply_seed(
            raw,
            &thread,
            &identity(),
            &[identity().email.clone(), "alias@example.com".to_string()],
        );

        assert_eq!(seed.to, "CRIEW Test <criew@example.com>");
        assert!(seed.cc.is_empty());
    }

    #[test]
    fn build_reply_seed_falls_back_to_thread_metadata_and_empty_body() {
        let raw = b"Message-ID: <patch@example.com>\r\n\r\n";
        let mut thread = sample_thread("[PATCH] fallback", "thread@example.com");
        thread.from_addr = "Thread Author <author@example.com>".to_string();
        thread.date = Some("Sat, 7 Mar 2026 12:00:00 +0000".to_string());

        let seed = build_reply_seed(raw, &thread, &identity(), &[identity().email.clone()]);

        assert_eq!(seed.to, "Thread Author <author@example.com>");
        assert!(seed.cc.is_empty());
        assert_eq!(seed.subject, "Re: [PATCH] fallback");
        assert_eq!(seed.in_reply_to, "patch@example.com");
        assert_eq!(seed.references, vec!["patch@example.com"]);
        assert_eq!(
            seed.body[1],
            "On Sat, 7 Mar 2026 12:00:00 +0000, Thread Author <author@example.com> wrote:"
        );
        assert_eq!(seed.body[2], "> <empty mail body>");
    }

    #[test]
    fn build_reply_seed_handles_folded_headers_and_blank_body_lines() {
        let raw = b"Message-ID: <patch@example.com>\r\nSubject: [PATCH] folded\r\nFrom: Alice <alice@example.com>\r\nTo: \"Doe, Jane\" <jane@example.com>,\r\n Bob <bob@example.com>\r\nCc: Carol <carol@example.com>;\r\n\tCRIEW Test <criew@example.com>\r\nDate: Fri, 6 Mar 2026 09:30:00 +0000\r\n\r\nline one\r\n\r\nline two\r\n";
        let thread = sample_thread("[PATCH] folded", "patch@example.com");

        let seed = build_reply_seed(raw, &thread, &identity(), &[identity().email.clone()]);

        assert_eq!(
            seed.to,
            "\"Doe, Jane\" <jane@example.com>, Bob <bob@example.com>"
        );
        assert_eq!(seed.cc, "Carol <carol@example.com>");
        assert_eq!(seed.body[2], "> line one");
        assert_eq!(seed.body[3], ">");
        assert_eq!(seed.body[4], "> line two");
    }

    #[test]
    fn prepare_reply_message_uses_parent_when_references_missing() {
        let (message, errors) = prepare_reply_message(ReplyPreviewRequest {
            from: "CRIEW Test <criew@example.com>",
            to: "Bob <bob@example.com>",
            cc: "",
            subject: "[PATCH] demo",
            in_reply_to: " <parent@example.com>, ",
            references: &[],
            body: &[
                "line one   ".to_string(),
                String::new(),
                "line two".to_string(),
            ],
            self_addresses: &[identity().email.clone()],
        });

        assert!(errors.is_empty());
        assert_eq!(message.subject, "Re: [PATCH] demo");
        assert_eq!(message.in_reply_to, "parent@example.com");
        assert_eq!(message.references, vec!["parent@example.com"]);
        assert_eq!(message.body, "line one\n\nline two");
    }

    #[test]
    fn prepare_reply_message_allows_independent_compose_without_thread_headers() {
        let (message, errors) = prepare_reply_message(ReplyPreviewRequest {
            from: "CRIEW Test <criew@example.com>",
            to: "Recipient <recipient@example.com>",
            cc: "",
            subject: "New message",
            in_reply_to: "",
            references: &[],
            body: &["hello".to_string()],
            self_addresses: &[identity().email.clone()],
        });

        assert!(errors.is_empty());
        assert!(message.in_reply_to.is_empty());
        assert!(message.references.is_empty());
        assert_eq!(message.subject, "New message");
    }

    #[test]
    fn prepare_reply_message_adds_parent_to_existing_references_and_filters_self() {
        let (message, errors) = prepare_reply_message(ReplyPreviewRequest {
            from: " CRIEW Test <criew@example.com> ",
            to: "Bob <bob@example.com>; \"Doe, Jane\" <jane@example.com>; CRIEW Test <criew@example.com>",
            cc: "Carol <carol@example.com>, criew@example.com",
            subject: "fwd: [PATCH] demo",
            in_reply_to: " <parent@example.com>, ",
            references: &["<older@example.com>".to_string()],
            body: &["body".to_string()],
            self_addresses: &[identity().email.clone(), "alias@example.com".to_string()],
        });

        assert!(errors.is_empty());
        assert_eq!(
            message.to,
            vec![
                "Bob <bob@example.com>".to_string(),
                "\"Doe, Jane\" <jane@example.com>".to_string()
            ]
        );
        assert_eq!(message.cc, vec!["Carol <carol@example.com>".to_string()]);
        assert_eq!(message.subject, "Re: [PATCH] demo");
        assert_eq!(
            message.references,
            vec![
                "older@example.com".to_string(),
                "parent@example.com".to_string()
            ]
        );
    }

    #[test]
    fn preview_validation_reports_missing_recipients() {
        let preview = render_reply_preview(ReplyPreviewRequest {
            from: "CRIEW Test <criew@example.com>",
            to: "",
            cc: "criew@example.com",
            subject: "Re: [PATCH] demo",
            in_reply_to: "patch@example.com",
            references: &["patch@example.com".to_string()],
            body: &[String::new()],
            self_addresses: &[identity().email.clone()],
        });

        assert!(!preview.errors.is_empty());
        assert!(
            preview
                .errors
                .iter()
                .any(|value| value.contains("no recipients"))
        );
        assert!(
            preview
                .warnings
                .iter()
                .any(|value| value.contains("no authored reply content"))
        );
        assert!(preview.content.contains("To: <none>"));
    }

    #[test]
    fn preview_keeps_single_self_to_recipient() {
        let preview = render_reply_preview(ReplyPreviewRequest {
            from: "CRIEW Test <criew@example.com>",
            to: "CRIEW Test <criew@example.com>",
            cc: "",
            subject: "Re: [PATCH] demo",
            in_reply_to: "patch@example.com",
            references: &["patch@example.com".to_string()],
            body: &[String::new()],
            self_addresses: &[identity().email.clone()],
        });

        assert!(preview.errors.is_empty());
        assert!(
            preview
                .content
                .contains("To: CRIEW Test <criew@example.com>")
        );
    }

    #[test]
    fn render_reply_preview_reports_missing_headers_and_renders_placeholders() {
        let preview = render_reply_preview(ReplyPreviewRequest {
            from: "CRIEW Test",
            to: "",
            cc: "",
            subject: "   ",
            in_reply_to: "   ",
            references: &[],
            body: &["   ".to_string(), String::new()],
            self_addresses: &[],
        });

        assert!(
            preview
                .errors
                .iter()
                .any(|value| value == "From is missing a valid email address")
        );
        assert!(
            preview
                .errors
                .iter()
                .any(|value| value == "reply preview has no recipients after removing self")
        );
        assert!(
            preview
                .errors
                .iter()
                .any(|value| value == "Subject is empty")
        );
        assert!(preview.content.contains("To: <none>"));
        assert!(preview.content.contains("Cc: <none>"));
        assert!(preview.content.contains("Subject: "));
        assert!(preview.content.contains("In-Reply-To: <none>"));
        assert!(preview.content.contains("References: <none>"));
        assert!(preview.content.ends_with("<empty body>"));
        assert!(
            preview
                .warnings
                .iter()
                .any(|value| value.contains("no authored reply content"))
        );
    }

    #[test]
    fn render_reply_preview_warns_when_body_only_contains_generated_quote_scaffolding() {
        let preview = render_reply_preview(ReplyPreviewRequest {
            from: "CRIEW Test <criew@example.com>",
            to: "Bob <bob@example.com>",
            cc: "",
            subject: "Re: [PATCH] demo",
            in_reply_to: "patch@example.com",
            references: &["patch@example.com".to_string()],
            body: &[
                String::new(),
                "On Fri, 6 Mar 2026 09:30:00 +0000, Alice wrote:".to_string(),
                "> quoted line".to_string(),
                ">".to_string(),
            ],
            self_addresses: &[identity().email.clone()],
        });

        assert!(preview.errors.is_empty());
        assert_eq!(preview.warnings.len(), 1);
        assert!(
            preview.warnings[0].contains("no authored reply content"),
            "unexpected warning: {}",
            preview.warnings[0]
        );
        assert!(
            preview
                .lines
                .iter()
                .any(|line| matches!(line.kind, ReplyPreviewLineKind::QuoteAttribution))
        );
        assert!(
            preview
                .lines
                .iter()
                .any(|line| matches!(line.kind, ReplyPreviewLineKind::Quoted))
        );
    }

    #[test]
    fn render_reply_preview_marks_authored_reply_lines() {
        let preview = render_reply_preview(ReplyPreviewRequest {
            from: "CRIEW Test <criew@example.com>",
            to: "Bob <bob@example.com>",
            cc: "",
            subject: "Re: [PATCH] demo",
            in_reply_to: "patch@example.com",
            references: &["patch@example.com".to_string()],
            body: &[
                "Looks good to me.".to_string(),
                String::new(),
                "On Fri, 6 Mar 2026 09:30:00 +0000, Alice wrote:".to_string(),
                "> quoted line".to_string(),
            ],
            self_addresses: &[identity().email.clone()],
        });

        assert!(preview.errors.is_empty());
        assert!(preview.warnings.is_empty());
        assert!(preview.lines.iter().any(|line| {
            matches!(line.kind, ReplyPreviewLineKind::Authored) && line.text == "Looks good to me."
        }));
    }

    #[test]
    fn extracts_email_from_display_or_bare_value() {
        assert_eq!(
            extract_email_address("Alice <alice@example.com>"),
            Some("alice@example.com".to_string())
        );
        assert_eq!(
            extract_email_address("alice@example.com"),
            Some("alice@example.com".to_string())
        );
        assert_eq!(extract_email_address("Alice Example"), None);
        assert_eq!(extract_email_address("<>"), None);
    }
}
