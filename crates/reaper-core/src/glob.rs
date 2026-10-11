//! Minimal glob matching for namespace patterns: `*` matches any run of
//! characters and `?` matches exactly one. Namespace names are ASCII.

#[must_use]
pub fn matches(pattern: &str, name: &str) -> bool {
    let (p, n) = (pattern.as_bytes(), name.as_bytes());
    let (mut pi, mut ni) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while ni < n.len() {
        if pi < p.len() && (p[pi] == b'?' || p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if pi < p.len() && p[pi] == b'*' {
            star = Some((pi, ni));
            pi += 1;
        } else if let Some((sp, sn)) = star {
            pi = sp + 1;
            ni = sn + 1;
            star = Some((sp, sn + 1));
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == b'*')
}

#[must_use]
pub fn any_matches(patterns: &[String], name: &str) -> bool {
    patterns.iter().any(|p| matches(p, name))
}

#[cfg(test)]
mod tests {
    use super::matches;

    #[test]
    fn globs() {
        assert!(matches("shop-*", "shop-prod"));
        assert!(matches("shop-*", "shop-"));
        assert!(!matches("shop-*", "shop"));
        assert!(matches("kube-system", "kube-system"));
        assert!(!matches("kube-system", "kube-system2"));
        assert!(matches("*-dev", "team-dev"));
        assert!(matches("a?c", "abc"));
        assert!(!matches("a?c", "ac"));
        assert!(matches("*", "anything"));
        assert!(matches("*a*b*", "xxaxxbxx"));
        assert!(!matches("*a*b", "xxaxxbxxc"));
    }
}
