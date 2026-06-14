pub(crate) fn path_matches_filter(path: &str, filter: &str) -> bool {
    let filter_owned = normalize_path_filter(filter);
    let filter = filter_owned.as_ref();
    if filter.contains('/') {
        let filter = filter.trim_end_matches('/');
        if filter.is_empty() {
            return true;
        }
        if path == filter
            || (path.starts_with(filter) && path.as_bytes().get(filter.len()) == Some(&b'/'))
        {
            return true;
        }
        #[cfg(target_os = "windows")]
        {
            let filter_lower = filter.to_ascii_lowercase();
            let path_lower = path.to_ascii_lowercase();
            if path.eq_ignore_ascii_case(filter)
                || (path_lower.starts_with(filter_lower.as_str())
                    && path_lower.as_bytes().get(filter_lower.len()) == Some(&b'/'))
            {
                return true;
            }
        }
        return false;
    }
    if path_matches_exact_or_suffix(path, filter) {
        return true;
    }
    squeezy_rank::fuzzy::fuzzy_path_score(path, filter).is_some()
}

pub(crate) fn normalize_path_filter(filter: &str) -> std::borrow::Cow<'_, str> {
    let s = if filter.contains('\\') {
        std::borrow::Cow::Owned(filter.replace('\\', "/"))
    } else {
        std::borrow::Cow::Borrowed(filter)
    };
    if let Some(rest) = s.strip_prefix("./") {
        std::borrow::Cow::Owned(rest.to_string())
    } else {
        s
    }
}

pub(crate) fn path_matches_exact_or_suffix(path: &str, filter: &str) -> bool {
    let filter = normalize_path_filter(filter);
    let filter = filter.as_ref();
    if path == filter {
        return true;
    }
    #[cfg(target_os = "windows")]
    if path.eq_ignore_ascii_case(filter) {
        return true;
    }
    if path
        .strip_suffix(filter)
        .is_some_and(|prefix| prefix.ends_with('/'))
    {
        return true;
    }
    #[cfg(target_os = "windows")]
    {
        let path_lower = path.to_ascii_lowercase();
        let filter_lower = filter.to_ascii_lowercase();
        if path_lower
            .strip_suffix(filter_lower.as_str())
            .is_some_and(|prefix| prefix.ends_with('/'))
        {
            return true;
        }
    }
    false
}
