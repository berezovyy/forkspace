//! Minimal shell-style glob matching for workspace-name patterns.

/// Shell-style glob match supporting `*` and `?` (no character classes).
pub fn glob_matches(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    let (mut p, mut t) = (0, 0);
    let mut backtrack: Option<(usize, usize)> = None;

    while t < text.len() {
        if p < pattern.len() && (pattern[p] == '?' || pattern[p] == text[t]) {
            p += 1;
            t += 1;
        } else if p < pattern.len() && pattern[p] == '*' {
            backtrack = Some((p, t));
            p += 1;
        } else if let Some((star_p, star_t)) = backtrack {
            p = star_p + 1;
            t = star_t + 1;
            backtrack = Some((star_p, star_t + 1));
        } else {
            return false;
        }
    }
    pattern[p..].iter().all(|&c| c == '*')
}

#[cfg(test)]
mod tests {
    use super::glob_matches;

    #[test]
    fn matches_wildcards() {
        assert!(glob_matches("feat-*", "feat-a"));
        assert!(glob_matches("a?c", "abc"));
        assert!(glob_matches("*", ""));
        assert!(glob_matches("*b*", "abc"));
        assert!(!glob_matches("a?c", "ac"));
        assert!(!glob_matches("feat-*", "fix-a"));
    }
}
