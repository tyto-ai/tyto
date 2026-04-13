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
