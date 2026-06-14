//! Shared lexical tokenization helpers for rankers.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TokenCase {
    Preserve,
    Lowercase,
}

pub(crate) fn split_tokens(
    input: &str,
    is_separator: impl FnMut(char) -> bool,
    case: TokenCase,
) -> Vec<String> {
    let mut tokens = Vec::new();
    for_each_token(input, is_separator, case, |token| {
        tokens.push(token.to_owned());
    });
    tokens
}

pub(crate) fn split_tokens_both(
    input: &str,
    mut is_separator: impl FnMut(char) -> bool,
) -> (Vec<String>, Vec<String>) {
    let mut lower_tokens = Vec::new();
    let mut raw_tokens = Vec::new();
    let mut lower_current = String::new();
    let mut raw_current = String::new();

    for ch in input.chars() {
        if is_separator(ch) {
            flush_token_pair(
                &mut lower_current,
                &mut raw_current,
                &mut lower_tokens,
                &mut raw_tokens,
            );
            continue;
        }
        lower_current.extend(ch.to_lowercase());
        raw_current.push(ch);
    }
    flush_token_pair(
        &mut lower_current,
        &mut raw_current,
        &mut lower_tokens,
        &mut raw_tokens,
    );

    (lower_tokens, raw_tokens)
}

pub(crate) fn for_each_token(
    input: &str,
    mut is_separator: impl FnMut(char) -> bool,
    case: TokenCase,
    mut visit: impl FnMut(&str),
) {
    let mut current = String::new();
    for ch in input.chars() {
        if is_separator(ch) {
            flush_token(&mut current, &mut visit);
            continue;
        }
        match case {
            TokenCase::Preserve => current.push(ch),
            TokenCase::Lowercase => current.extend(ch.to_lowercase()),
        }
    }
    flush_token(&mut current, &mut visit);
}

pub(crate) fn path_token_separator(ch: char) -> bool {
    matches!(ch, '/' | '\\' | '_' | '-' | '.') || ch.is_whitespace()
}

pub(crate) fn bm25_token_separator(ch: char) -> bool {
    ch.is_whitespace()
        || matches!(
            ch,
            '_' | '-'
                | '/'
                | '.'
                | ':'
                | '('
                | ')'
                | '<'
                | '>'
                | '&'
                | ','
                | ';'
                | '\''
                | '"'
                | '['
                | ']'
                | '{'
                | '}'
                | '!'
                | '?'
                | '*'
                | '='
                | '|'
                | '@'
                | '#'
                | '$'
                | '%'
                | '^'
                | '~'
                | '`'
                | '+'
                | '\\'
        )
}

pub(crate) fn split_compound_identifier(name: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let chars: Vec<char> = name.chars().collect();

    for (i, &ch) in chars.iter().enumerate() {
        if is_compound_identifier_separator(ch) {
            if !current.is_empty() {
                tokens.push(std::mem::take(&mut current));
            }
            continue;
        }

        let prev = if i > 0 {
            chars.get(i - 1).copied()
        } else {
            None
        };
        let next = chars.get(i + 1).copied();

        let camel_boundary = match (prev, ch) {
            (Some(p), c) if p.is_ascii_lowercase() && c.is_ascii_uppercase() => true,
            (Some(p), c)
                if p.is_ascii_uppercase()
                    && c.is_ascii_uppercase()
                    && matches!(next, Some(n) if n.is_ascii_lowercase()) =>
            {
                true
            }
            (Some(p), c) if p.is_ascii_alphabetic() && c.is_ascii_digit() => true,
            (Some(p), c) if p.is_ascii_digit() && c.is_ascii_alphabetic() => true,
            _ => false,
        };

        if camel_boundary && !current.is_empty() {
            tokens.push(std::mem::take(&mut current));
        }
        current.push(ch);
    }
    if !current.is_empty() {
        tokens.push(current);
    }

    tokens
        .into_iter()
        .map(|tok| tok.to_lowercase())
        .filter(|tok| !tok.is_empty())
        .collect()
}

fn is_compound_identifier_separator(ch: char) -> bool {
    matches!(ch, '_' | '-' | '/' | '.' | ' ' | ':')
}

fn flush_token(current: &mut String, visit: &mut impl FnMut(&str)) {
    if !current.is_empty() {
        visit(current);
        current.clear();
    }
}

fn flush_token_pair(
    lower_current: &mut String,
    raw_current: &mut String,
    lower_tokens: &mut Vec<String>,
    raw_tokens: &mut Vec<String>,
) {
    if !lower_current.is_empty() {
        lower_tokens.push(std::mem::take(lower_current));
        raw_tokens.push(std::mem::take(raw_current));
    }
}
