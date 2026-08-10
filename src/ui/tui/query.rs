//! Structured thread-query parsing for the TUI search overlay.
//!
//! The parser deliberately stays small and deterministic: terms are combined
//! with AND semantics, a leading `-` negates a term, and an unqualified term
//! keeps the original subject/from/message-id substring behavior.

use chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, Utc};

use crate::infra::mail_store::ThreadRow;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ThreadQuery {
    terms: Vec<QueryTerm>,
}

impl ThreadQuery {
    pub(super) fn is_empty(&self) -> bool {
        self.terms.is_empty()
    }

    pub(super) fn matches(&self, row: &ThreadRow, review_status: Option<&str>) -> bool {
        self.terms.iter().all(|term| {
            let matched = match term.field {
                QueryField::Any => contains_any(row, &term.value),
                QueryField::Subject => contains_case_insensitive(&row.subject, &term.value),
                QueryField::From => contains_case_insensitive(&row.from_addr, &term.value),
                QueryField::MessageId => contains_case_insensitive(&row.message_id, &term.value),
                QueryField::After => date_matches(row.date.as_deref(), &term.value, true),
                QueryField::Before => date_matches(row.date.as_deref(), &term.value, false),
                QueryField::Review => review_status.is_some_and(|status| {
                    status.eq_ignore_ascii_case(&term.value)
                        || (term.value == "all" && status != "none")
                }),
                QueryField::Patch => is_patch_subject(&row.subject),
            };
            if term.negated { !matched } else { matched }
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct QueryTerm {
    field: QueryField,
    value: String,
    negated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QueryField {
    Any,
    Subject,
    From,
    MessageId,
    After,
    Before,
    Review,
    Patch,
}

pub(super) fn parse_query(input: &str) -> std::result::Result<ThreadQuery, String> {
    let tokens = tokenize(input)?;
    let mut terms = Vec::with_capacity(tokens.len());
    for token in tokens {
        let (negated, token) = if let Some(value) = token.strip_prefix('-') {
            (true, value)
        } else {
            (false, token.as_str())
        };
        if token.is_empty() {
            return Err("query term '-' is missing a value".to_string());
        }

        let (field, value) = match token.split_once(':') {
            Some((name, value)) => {
                let field = match name.to_ascii_lowercase().as_str() {
                    "subject" | "subj" => QueryField::Subject,
                    "from" | "sender" => QueryField::From,
                    "id" | "message" | "message-id" => QueryField::MessageId,
                    "after" => QueryField::After,
                    "before" => QueryField::Before,
                    "review" => QueryField::Review,
                    "is" => QueryField::Patch,
                    _ => {
                        return Err(format!(
                            "unknown query field '{name}'; use subject:, from:, id:, after:, before:, review:, or is:patch"
                        ));
                    }
                };
                (field, value.to_string())
            }
            None => (QueryField::Any, token.to_string()),
        };
        let value = value.trim().to_ascii_lowercase();
        if value.is_empty() {
            return Err(format!("query term '{token}' is missing a value"));
        }
        if matches!(field, QueryField::After | QueryField::Before)
            && parse_query_date(&value).is_none()
        {
            return Err(format!("invalid date '{value}'; use YYYY-MM-DD"));
        }
        if matches!(field, QueryField::Review)
            && !matches!(value.as_str(), "needs-review" | "reviewed" | "all" | "none")
        {
            return Err(format!(
                "invalid review status '{value}'; use needs-review, reviewed, all, or none"
            ));
        }
        if matches!(field, QueryField::Patch) && value != "patch" {
            return Err(format!(
                "invalid is: value '{value}'; only is:patch is supported"
            ));
        }
        terms.push(QueryTerm {
            field,
            value,
            negated,
        });
    }
    Ok(ThreadQuery { terms })
}

fn tokenize(input: &str) -> std::result::Result<Vec<String>, String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut escaped = false;

    for character in input.chars() {
        if escaped {
            current.push(character);
            escaped = false;
            continue;
        }
        if character == '\\' {
            escaped = true;
            continue;
        }
        if let Some(expected) = quote {
            if character == expected {
                quote = None;
            } else {
                current.push(character);
            }
            continue;
        }
        match character {
            '\'' | '"' => quote = Some(character),
            character if character.is_whitespace() => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(character),
        }
    }

    if escaped {
        current.push('\\');
    }
    if let Some(character) = quote {
        return Err(format!("unterminated query quote {character}"));
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    Ok(tokens)
}

fn contains_any(row: &ThreadRow, value: &str) -> bool {
    contains_case_insensitive(&row.subject, value)
        || contains_case_insensitive(&row.from_addr, value)
        || contains_case_insensitive(&row.message_id, value)
}

fn contains_case_insensitive(haystack: &str, needle: &str) -> bool {
    haystack.to_ascii_lowercase().contains(needle)
}

fn is_patch_subject(subject: &str) -> bool {
    subject
        .split_whitespace()
        .next()
        .is_some_and(|token| token.eq_ignore_ascii_case("[patch]"))
        || subject.to_ascii_lowercase().starts_with("[patch ")
}

fn date_matches(value: Option<&str>, query_date: &str, after: bool) -> bool {
    let Some(value) = value else { return false };
    let Some(message_date) = parse_message_date(value) else {
        return false;
    };
    let Some(query_date) = parse_query_date(query_date) else {
        return false;
    };
    if after {
        message_date.date_naive() >= query_date
    } else {
        message_date.date_naive() <= query_date
    }
}

fn parse_query_date(value: &str) -> Option<chrono::NaiveDate> {
    NaiveDate::parse_from_str(value, "%Y-%m-%d").ok()
}

fn parse_message_date(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc2822(value)
        .or_else(|_| DateTime::parse_from_rfc3339(value))
        .map(|value| value.with_timezone(&Utc))
        .or_else(|_| {
            NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S")
                .map(|value| DateTime::<Utc>::from_naive_utc_and_offset(value, Utc))
        })
        .or_else(|_| {
            NaiveDate::parse_from_str(value, "%Y-%m-%d").map(|date| {
                DateTime::<Utc>::from_naive_utc_and_offset(date.and_time(NaiveTime::MIN), Utc)
            })
        })
        .ok()
}

#[cfg(test)]
mod tests {
    use super::{parse_query, tokenize};
    use crate::infra::mail_store::ThreadRow;

    fn row(subject: &str, from: &str, message_id: &str, date: Option<&str>) -> ThreadRow {
        ThreadRow {
            thread_id: 1,
            mail_id: 1,
            depth: 0,
            subject: subject.to_string(),
            from_addr: from.to_string(),
            message_id: message_id.to_string(),
            in_reply_to: None,
            date: date.map(ToOwned::to_owned),
            raw_path: None,
        }
    }

    #[test]
    fn tokenizes_quotes_and_escaped_spaces() {
        assert_eq!(
            tokenize(r#"subject:"mm cleanup" from:alice\ bob"#).expect("tokens"),
            vec!["subject:mm cleanup", "from:alice bob"]
        );
        assert!(tokenize("subject:'unfinished").is_err());
    }

    #[test]
    fn parses_legacy_terms_and_structured_fields() {
        let query = parse_query("subject:cleanup from:alice id:patch is:patch").expect("query");
        let message = row(
            "[PATCH] mm cleanup",
            "Alice <alice@example.com>",
            "patch@example.com",
            Some("Fri, 6 Mar 2026 09:30:00 +0000"),
        );
        assert!(query.matches(&message, Some("needs-review")));
        assert!(
            parse_query("cleanup")
                .expect("query")
                .matches(&message, None)
        );
    }

    #[test]
    fn supports_negation_review_and_date_filters() {
        let message = row(
            "[PATCH] net fix",
            "Alice <alice@example.com>",
            "patch@example.com",
            Some("Fri, 6 Mar 2026 09:30:00 +0000"),
        );
        assert!(
            parse_query(
                "-subject:documentation after:2026-03-01 before:2026-03-10 review:reviewed"
            )
            .expect("query")
            .matches(&message, Some("reviewed"))
        );
        assert!(
            !parse_query("review:reviewed")
                .expect("query")
                .matches(&message, None)
        );
    }

    #[test]
    fn rejects_unknown_fields_and_invalid_values() {
        assert!(parse_query("label:foo").is_err());
        assert!(parse_query("after:not-a-date").is_err());
        assert!(parse_query("review:pending").is_err());
        assert!(parse_query("is:reply").is_err());
    }
}
