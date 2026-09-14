use regex::Regex;
use serde_json::Value;
use std::sync::LazyLock;

static PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        r"(?i)\b(?:sk-(?:proj-)?|ghp_|github_pat_|xox[baprs]-)[A-Za-z0-9_-]{16,}",
        r"\bAKIA[A-Z0-9]{16}\b",
        r"(?is)-----BEGIN [A-Z ]*PRIVATE KEY-----.*?-----END [A-Z ]*PRIVATE KEY-----",
        r"(?i)\b(?:Bearer|Basic)\s+[A-Za-z0-9._~+/=-]{12,}",
        r"(?i)(?:password|passwd|api[_-]?key|access[_-]?token|secret)\s*[:=]\s*['\x22]?[^\s'\x22,;}]+",
        r"(?i)\b[a-z][a-z0-9+.-]*://[^\s/:@]+:[^\s/@]+@",
        r"\beyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\b",
    ]
    .into_iter()
    .map(|r| Regex::new(r).expect("static redaction pattern"))
    .collect()
});

static RANDOM_TOKEN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[A-Za-z0-9_+/=-]{32,}").expect("static token pattern"));

static STRUCTURAL_CREDENTIALS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    // The short sk- prefix needs a delimiter: task-/disk- are ordinary words.
    // An underscore can delimit a key (snapshot_sk-...), while distinctive
    // provider signatures such as ghp_ remain detectable inside words.
    [
        r"(?i)(?:^|[^\w]|_)sk-(?:proj-)?[A-Za-z0-9_-]{16,}",
        r"(?i)(?:ghp_|github_pat_|xox[baprs]-)[A-Za-z0-9_-]{16,}",
        r"AKIA[A-Z0-9]{16}",
        r"(?is)-----BEGIN [A-Z ]*PRIVATE KEY-----.*?-----END [A-Z ]*PRIVATE KEY-----",
        r"(?i)(?:Bearer|Basic)\s+[A-Za-z0-9._~+/=-]{12,}",
        r"(?i)(?:password|passwd|api[_-]?key|access[_-]?token|secret)\s*[:=]\s*['\x22]?[^\s'\x22,;}]+",
        r"(?i)[a-z][a-z0-9+.-]*://[^\s/:@]+:[^\s/@]+@",
        r"eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}",
    ]
        .into_iter()
        .map(|pattern| Regex::new(pattern).expect("structural credential pattern"))
        .collect()
});
static STRUCTURAL_TOKEN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[A-Za-z0-9+=]{32,}").expect("structural token pattern"));

/// Structural separators must not turn a descriptive ref/path into one random
/// token. Credential signatures have their own boundary rules; long opaque
/// segments still receive entropy checks. Native IDs/text use their own policy.
pub fn structural_has_secret(input: &str) -> bool {
    STRUCTURAL_CREDENTIALS
        .iter()
        .any(|pattern| pattern.is_match(input))
        || STRUCTURAL_TOKEN
            .find_iter(input)
            .any(|token| high_entropy(token.as_str()))
}

fn high_entropy(token: &str) -> bool {
    // Hashes and ULIDs are identifiers, not secrets merely because they look
    // random. Other candidates need mixed case, digits and sufficient entropy.
    if token.bytes().all(|b| b.is_ascii_hexdigit())
        || !token.bytes().any(|b| b.is_ascii_uppercase())
        || !token.bytes().any(|b| b.is_ascii_lowercase())
        || !token.bytes().any(|b| b.is_ascii_digit())
    {
        return false;
    }
    let mut counts = [0usize; 128];
    for b in token.bytes() {
        counts[b as usize] += 1;
    }
    let entropy: f64 = counts
        .iter()
        .filter(|&&n| n > 0)
        .map(|&n| {
            let p = n as f64 / token.len() as f64;
            -p * p.log2()
        })
        .sum();
    entropy >= 4.5
}

pub fn text(input: &str) -> String {
    let mut result = input.to_owned();
    for pattern in PATTERNS.iter() {
        result = pattern.replace_all(&result, "[REDACTED]").into_owned();
    }
    RANDOM_TOKEN
        .replace_all(&result, |captures: &regex::Captures<'_>| {
            if high_entropy(&captures[0]) {
                "[REDACTED]".to_owned()
            } else {
                captures[0].to_owned()
            }
        })
        .into_owned()
}

pub fn value(input: &Value) -> Value {
    match input {
        Value::String(s) => Value::String(text(s)),
        Value::Array(a) => Value::Array(a.iter().map(value).collect()),
        Value::Object(m) => Value::Object(
            m.iter()
                .map(|(k, v)| {
                    let sensitive = matches!(
                        k.to_ascii_lowercase().as_str(),
                        "password"
                            | "passwd"
                            | "secret"
                            | "api_key"
                            | "apikey"
                            | "access_token"
                            | "refresh_token"
                            | "authorization"
                            | "private_key"
                    );
                    (
                        text(k),
                        if sensitive && !v.is_null() {
                            Value::String("[REDACTED]".into())
                        } else {
                            value(v)
                        },
                    )
                })
                .collect(),
        ),
        _ => input.clone(),
    }
}
