//! Keyword search: query tokenization, field scoring, and snippet extraction.

use std::collections::HashSet;

pub(super) fn normalize_optional_filter(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_lowercase())
}

pub(super) fn query_tokens(query: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    query
        .split(|c: char| c.is_whitespace() || c == ',' || c == ';' || c == '，' || c == '；')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_lowercase())
        .filter(|s| seen.insert(s.clone()))
        .collect()
}

fn ascii_boundary_contains(haystack: &str, needle: &str) -> bool {
    let mut search_start = 0;
    while let Some(relative_pos) = haystack[search_start..].find(needle) {
        let start = search_start + relative_pos;
        let end = start + needle.len();
        let before = haystack[..start]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_ascii_alphanumeric());
        let after = haystack[end..]
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric());
        if !before && !after {
            return true;
        }
        search_start = end;
    }
    false
}

fn token_matches(haystack: &str, token: &str) -> bool {
    if token.chars().all(|c| c.is_ascii_alphanumeric()) {
        ascii_boundary_contains(haystack, token)
    } else {
        haystack.contains(token)
    }
}

fn find_token_match(haystack: &str, token: &str) -> Option<usize> {
    if !token.chars().all(|c| c.is_ascii_alphanumeric()) {
        return haystack.find(token);
    }

    let mut search_start = 0;
    while let Some(relative_pos) = haystack[search_start..].find(token) {
        let start = search_start + relative_pos;
        let end = start + token.len();
        let before = haystack[..start]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_ascii_alphanumeric());
        let after = haystack[end..]
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric());
        if !before && !after {
            return Some(start);
        }
        search_start = end;
    }
    None
}

fn field_match_score(field: &str, query: &str, tokens: &[String], weight: i32) -> i32 {
    let lower = field.to_lowercase();
    let mut score = 0;
    if !query.is_empty() && lower.contains(query) {
        score += weight * 3;
    }
    for token in tokens {
        if token_matches(&lower, token) {
            score += weight;
        }
    }
    score
}

pub(super) fn memory_search_score(mf: &crate::memory::MemoryFile, query: &str, tokens: &[String]) -> i32 {
    let mut score = 0;
    score += field_match_score(&mf.name, query, tokens, 10);
    score += mf
        .tags
        .iter()
        .map(|tag| field_match_score(tag, query, tokens, 8))
        .sum::<i32>();
    score += field_match_score(&mf.description, query, tokens, 6);
    score += mf
        .links
        .iter()
        .map(|link| {
            field_match_score(&link.label, query, tokens, 2)
                + field_match_score(&link.target, query, tokens, 2)
        })
        .sum::<i32>();
    score += field_match_score(&mf.content, query, tokens, 3);
    score
}

pub(super) fn best_snippet(mf: &crate::memory::MemoryFile, query: &str, tokens: &[String]) -> String {
    let content_lower = mf.content.to_lowercase();
    let needle = if !query.is_empty() && content_lower.contains(query) {
        Some(query.to_string())
    } else {
        tokens
            .iter()
            .find(|token| find_token_match(&content_lower, token.as_str()).is_some())
            .cloned()
    };

    if let Some(needle) = needle {
        if let Some(byte_pos) = find_token_match(&content_lower, &needle) {
            let char_pos = content_lower[..byte_pos].chars().count();
            let needle_chars = needle.chars().count();
            let start = mf
                .content
                .char_indices()
                .nth(char_pos.saturating_sub(40))
                .map(|(i, _)| i)
                .unwrap_or(0);
            let end = mf
                .content
                .char_indices()
                .nth(char_pos + needle_chars + 60)
                .map(|(i, _)| i)
                .unwrap_or(mf.content.len());
            let mut s = String::new();
            if start > 0 {
                s.push_str("...");
            }
            s.push_str(&mf.content[start..end]);
            if end < mf.content.len() {
                s.push_str("...");
            }
            return s;
        }
    }

    if field_match_score(&mf.description, query, tokens, 1) > 0 {
        return mf.description.clone();
    }
    if let Some(link) = mf.links.iter().find(|link| {
        field_match_score(&link.label, query, tokens, 1)
            + field_match_score(&link.target, query, tokens, 1)
            > 0
    }) {
        return format!("See Also: {} -> {}", link.label, link.target);
    }
    mf.content.chars().take(120).collect()
}
