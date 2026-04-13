use std::sync::LazyLock;

use regex::Regex;

/// Each entry is (pattern, replacement). The replacement may use `$1`, `$2`, etc.
/// to preserve surrounding context while redacting only the sensitive value.
///
/// Patterns are sourced from the gitleaks default ruleset and supplemented with
/// common token formats. Update this list when adding integrations with new providers.
static PATTERNS: LazyLock<Vec<(Regex, &str)>> = LazyLock::new(|| {
    vec![
        // PEM private key blocks (matched first - high specificity, multi-line)
        (Regex::new(r"-----BEGIN [A-Z ]+-----[\s\S]*?-----END [A-Z ]+-----").unwrap(), "[REDACTED]"),

        // JWTs (three base64url segments separated by dots)
        (Regex::new(r"eyJ[A-Za-z0-9_\-]+\.[A-Za-z0-9_\-]+\.[A-Za-z0-9_\-]+").unwrap(), "[REDACTED]"),

        // AWS access key IDs
        (Regex::new(r"(A3T[A-Z0-9]|AKIA|AGPA|AIDA|AROA|AIPA|ANPA|ANVA|ASIA)[A-Z0-9]{16}").unwrap(), "[REDACTED]"),

        // OpenAI / Anthropic / generic sk- tokens
        (Regex::new(r"sk-[A-Za-z0-9_\-]{10,}").unwrap(), "[REDACTED]"),

        // Stripe secret and restricted keys
        (Regex::new(r"(sk|rk)_(live|test)_[0-9a-zA-Z]{24,}").unwrap(), "[REDACTED]"),

        // GitHub tokens
        (Regex::new(r"ghp_[A-Za-z0-9]{36}").unwrap(), "[REDACTED]"),
        (Regex::new(r"gho_[A-Za-z0-9]{36}").unwrap(), "[REDACTED]"),
        (Regex::new(r"ghu_[A-Za-z0-9]{36}").unwrap(), "[REDACTED]"),
        (Regex::new(r"ghs_[A-Za-z0-9]{36}").unwrap(), "[REDACTED]"),
        (Regex::new(r"github_pat_[A-Za-z0-9_]{82}").unwrap(), "[REDACTED]"),

        // Slack tokens
        (Regex::new(r"xox[baprs]-[A-Za-z0-9\-]{10,}").unwrap(), "[REDACTED]"),

        // Google API keys
        (Regex::new(r"AIza[0-9A-Za-z\-_]{35}").unwrap(), "[REDACTED]"),

        // Google OAuth client secrets
        (Regex::new(r"GOCSPX-[A-Za-z0-9\-_]{28}").unwrap(), "[REDACTED]"),

        // Cloudflare API tokens (40 hex chars)
        (Regex::new(r#"(?i)cloudflare.{0,20}['"][0-9a-f]{40}['"]"#).unwrap(), "[REDACTED]"),

        // npm tokens
        (Regex::new(r"npm_[A-Za-z0-9]{36}").unwrap(), "[REDACTED]"),

        // Heroku API keys (UUID-like)
        (Regex::new(r"(?i)heroku.{0,20}[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}").unwrap(), "[REDACTED]"),

        // Twilio account SID and auth token
        (Regex::new(r"AC[a-z0-9]{32}").unwrap(), "[REDACTED]"),
        (Regex::new(r"SK[a-z0-9]{32}").unwrap(), "[REDACTED]"),

        // SendGrid API keys
        (Regex::new(r"SG\.[A-Za-z0-9\-_]{22}\.[A-Za-z0-9\-_]{43}").unwrap(), "[REDACTED]"),

        // Environment variable assignments with token-like values (20+ chars, no spaces).
        // Matched last as a catch-all. Group 1 preserves the key name.
        (
            Regex::new(r#"(?i)((?:token|secret|password|key|auth|api_key)\s*=\s*['"]?)[A-Za-z0-9_\-\.]{20,}['"]?"#).unwrap(),
            "${1}[REDACTED]",
        ),
    ]
});

pub fn sanitize(input: &str) -> String {
    let mut output = input.to_string();
    for (pattern, replacement) in PATTERNS.iter() {
        output = pattern.replace_all(&output, *replacement).into_owned();
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_sk_token() {
        let s = sanitize("use sk-abc123XYZabc123XYZabc for the API");
        assert!(!s.contains("sk-abc"), "sk- token should be redacted: {s}");
        assert!(s.contains("[REDACTED]"));
    }

    #[test]
    fn redacts_jwt() {
        let s = sanitize("token: eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJ1c2VyIn0.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c");
        assert!(!s.contains("eyJ"), "JWT should be redacted: {s}");
    }

    #[test]
    fn redacts_pem_block() {
        let s = sanitize("key: -----BEGIN RSA PRIVATE KEY-----\nMIIEowIBAAK\n-----END RSA PRIVATE KEY-----");
        assert!(!s.contains("MIIEowIBAAK"), "PEM block should be redacted: {s}");
    }

    #[test]
    fn leaves_normal_text_unchanged() {
        let s = sanitize("tower-sessions used for auth, not JWT libraries");
        assert_eq!(s, "tower-sessions used for auth, not JWT libraries");
    }

    #[test]
    fn env_var_redaction_preserves_key_name() {
        let s = sanitize("TOKEN=supersecretvalue1234567");
        assert!(s.contains("TOKEN="), "key name should be preserved: {s}");
        assert!(!s.contains("supersecretvalue"), "value should be redacted: {s}");
        assert!(s.contains("[REDACTED]"));
    }

    #[test]
    fn redacts_github_token() {
        // Pattern requires exactly 36 alphanumeric chars after ghp_
        let s = sanitize("auth: ghp_aBcDeFgHiJkLmNoPqRsTuVwXyZabcd123456");
        assert!(!s.contains("ghp_"), "GitHub token should be redacted: {s}");
        assert!(s.contains("[REDACTED]"));
    }

    #[test]
    fn short_env_var_value_not_redacted() {
        let s = sanitize("TOKEN=short");
        assert_eq!(s, "TOKEN=short", "short values should not be redacted");
    }
}
