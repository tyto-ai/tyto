use crate::retrieve::CompactResult;

/// Compact one-line-per-memory listing with optional omitted-count header.
/// `omitted` is the count of memories not included in `results` because they
/// were written to `omitted_file` instead. Pass `0` and `None` when not applicable.
pub fn compact(
    results: &[CompactResult],
    omitted: usize,
    omitted_file: Option<&std::path::Path>,
) -> String {
    let total = results.len() + omitted;
    let mut header = format!("--- Memory Context ({} results", total);
    if omitted > 0
        && let Some(path) = omitted_file {
        header.push_str(&format!(
            " -- {} included in full in {}",
            omitted,
            path.display()
        ));
    }
    header.push_str(") ---\n");
    let mut out = header;
    for r in results {
        let date = r.created_at.get(..10).unwrap_or(&r.created_at);
        out.push_str(&format!(
            "[{:<18} {:.2}] {}  {}  ~{}c  {}\n",
            r.memory_type, r.importance, r.id, date, r.content_len, r.title
        ));
    }
    out.push_str("---\n");
    out
}

/// Compact listing with facts and tags shown below each entry.
pub fn summary(results: &[CompactResult]) -> String {
    let mut out = format!("--- Memory Context ({} results) ---\n", results.len());
    for r in results {
        let date = r.created_at.get(..10).unwrap_or(&r.created_at);
        out.push_str(&format!(
            "[{:<18} {:.2}] {}  {}  ~{}c  {}\n",
            r.memory_type, r.importance, r.id, date, r.content_len, r.title
        ));
        let facts: Vec<String> = r.facts_json.as_deref()
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_default();
        for fact in &facts {
            out.push_str(&format!("  - {fact}\n"));
        }
        let tags: Vec<String> = r.tags_json.as_deref()
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_default();
        if !tags.is_empty() {
            out.push_str(&format!("  tags: {}\n", tags.join(", ")));
        }
        if !facts.is_empty() || !tags.is_empty() {
            out.push('\n');
        }
    }
    out.push_str("---\n");
    out
}

#[cfg(test)]
mod tests {
    use super::{compact, summary};
    use crate::retrieve::CompactResult;

    fn make_result(id: &str, memory_type: &str, title: &str) -> CompactResult {
        CompactResult {
            id: id.to_string(),
            memory_type: memory_type.to_string(),
            title: title.to_string(),
            created_at: "2026-01-15T10:00:00Z".to_string(),
            importance: 0.85,
            score: 1.0,
            content_len: 250,
            facts_json: None,
            tags_json: None,
        }
    }

    // --- compact ---

    #[test]
    fn compact_header_shows_total_count() {
        let results = vec![make_result("id-a", "decision", "Some decision")];
        let out = compact(&results, 0, None);
        assert!(out.contains("1 results"), "header must show total count");
    }

    #[test]
    fn compact_header_includes_omitted_file_when_set() {
        let results = vec![make_result("id-a", "decision", "A")];
        let path = std::path::Path::new("/tmp/tyto-session.txt");
        let out = compact(&results, 5, Some(path));
        assert!(out.contains("6 results"), "total = shown + omitted");
        assert!(out.contains("tyto-session.txt"), "omitted file path shown");
    }

    #[test]
    fn compact_no_omitted_file_mention_when_zero() {
        let results = vec![make_result("id-a", "gotcha", "A gotcha")];
        let out = compact(&results, 0, None);
        assert!(!out.contains("included in full"), "no omitted-file text when omitted=0");
    }

    #[test]
    fn compact_entry_format() {
        let results = vec![make_result("abc-123", "decision", "My decision")];
        let out = compact(&results, 0, None);
        assert!(out.contains("abc-123"));
        assert!(out.contains("2026-01-15"));
        assert!(out.contains("~250c"));
        assert!(out.contains("My decision"));
        assert!(out.contains("0.85"));
    }

    #[test]
    fn compact_wraps_with_separator_lines() {
        let out = compact(&[], 0, None);
        assert!(out.starts_with("--- Memory Context"));
        assert!(out.ends_with("---\n"));
    }

    // --- summary ---

    #[test]
    fn summary_shows_facts() {
        let mut r = make_result("id-a", "gotcha", "A gotcha");
        r.facts_json = Some(r#"["Fact one","Fact two"]"#.to_string());
        let out = summary(&[r]);
        assert!(out.contains("  - Fact one"));
        assert!(out.contains("  - Fact two"));
    }

    #[test]
    fn summary_shows_tags() {
        let mut r = make_result("id-a", "decision", "A decision");
        r.tags_json = Some(r#"["ci","rust"]"#.to_string());
        let out = summary(&[r]);
        assert!(out.contains("tags: ci, rust"));
    }

    #[test]
    fn summary_no_extra_lines_when_no_facts_or_tags() {
        let r = make_result("id-a", "fact", "A fact");
        let out = summary(&[r]);
        assert!(!out.contains("  -"));
        assert!(!out.contains("tags:"));
    }

    #[test]
    fn summary_invalid_facts_json_silently_ignored() {
        let mut r = make_result("id-a", "decision", "A decision");
        r.facts_json = Some("not valid json".to_string());
        let out = summary(&[r]);
        assert!(!out.contains("not valid json"));
    }
}
