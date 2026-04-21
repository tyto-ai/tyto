use tree_sitter::{Language, Parser, Query, QueryCursor};

#[derive(Debug, Clone)]
pub struct Chunk {
    pub symbol_name: String,
    pub qualified_name: String,
    pub symbol_kind: String,
    pub signature: Option<String>,
    pub doc_comment: Option<String>,
    pub body_preview: Option<String>,
    pub line_start: usize,
    pub line_end: usize,
    pub language: String,
}

pub enum Lang {
    Rust,
    Python,
}

impl Lang {
    pub fn from_extension(ext: &str) -> Option<Self> {
        match ext {
            "rs" => Some(Self::Rust),
            "py" | "pyi" => Some(Self::Python),
            _ => None,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::Python => "python",
        }
    }

    fn tree_sitter_language(&self) -> Language {
        match self {
            Self::Rust => tree_sitter_rust::LANGUAGE.into(),
            Self::Python => tree_sitter_python::LANGUAGE.into(),
        }
    }

    fn query_str(&self) -> &'static str {
        match self {
            Self::Rust => RUST_QUERY,
            Self::Python => PYTHON_QUERY,
        }
    }
}

// Captures: @symbol (whole node), @name (identifier node), @kind is encoded in each alternative
const RUST_QUERY: &str = r#"
(function_item name: (identifier) @name) @symbol
(struct_item name: (type_identifier) @name) @symbol
(enum_item name: (type_identifier) @name) @symbol
(trait_item name: (type_identifier) @name) @symbol
"#;

const PYTHON_QUERY: &str = r#"
(function_definition name: (identifier) @name) @symbol
(class_definition name: (identifier) @name) @symbol
"#;

pub fn parse_file(source: &str, _file_path: &str, lang: &Lang) -> Vec<Chunk> {
    let ts_lang = lang.tree_sitter_language();
    let mut parser = Parser::new();
    if parser.set_language(&ts_lang).is_err() {
        return vec![];
    }
    let tree = match parser.parse(source, None) {
        Some(t) => t,
        None => return vec![],
    };

    let query = match Query::new(&ts_lang, lang.query_str()) {
        Ok(q) => q,
        Err(_) => return vec![],
    };

    let symbol_idx = match query.capture_index_for_name("symbol") {
        Some(i) => i,
        None => return vec![],
    };
    let name_idx = match query.capture_index_for_name("name") {
        Some(i) => i,
        None => return vec![],
    };

    let source_bytes = source.as_bytes();
    let mut cursor = QueryCursor::new();
    let mut chunks = Vec::new();

    for m in cursor.matches(&query, tree.root_node(), source_bytes) {
        let mut symbol_node = None;
        let mut name_text = String::new();

        for cap in m.captures {
            if cap.index == symbol_idx {
                symbol_node = Some(cap.node);
            } else if cap.index == name_idx {
                if let Ok(t) = cap.node.utf8_text(source_bytes) {
                    name_text = t.to_string();
                }
            }
        }

        let node = match symbol_node {
            Some(n) => n,
            None => continue,
        };
        if name_text.is_empty() {
            continue;
        }

        let kind = node_kind_to_symbol_kind(node.kind());
        let qualified_name = if kind == "function" || kind == "method" {
            qualified_name_for(node, &name_text, source_bytes)
        } else {
            name_text.clone()
        };
        let signature = extract_signature(node, source_bytes, lang);
        let doc_comment = extract_doc_comment(node, source_bytes);
        let body_preview = extract_body_preview(node, source_bytes, lang);
        let start = node.start_position();
        let end = node.end_position();

        chunks.push(Chunk {
            symbol_name: name_text,
            qualified_name,
            symbol_kind: kind,
            signature,
            doc_comment,
            body_preview,
            line_start: start.row + 1,
            line_end: end.row + 1,
            language: lang.name().to_string(),
        });
    }

    chunks
}

fn node_kind_to_symbol_kind(kind: &str) -> String {
    match kind {
        "function_item" => "function",
        "function_definition" => "function",
        "struct_item" => "struct",
        "enum_item" => "enum",
        "trait_item" => "trait",
        "class_definition" => "class",
        _ => "symbol",
    }
    .to_string()
}

/// Walk up the parent chain to find the enclosing impl block's type name.
/// Returns "TypeName::function_name" for methods, bare name for top-level functions.
fn qualified_name_for(node: tree_sitter::Node<'_>, name: &str, source: &[u8]) -> String {
    let mut current = node.parent();
    while let Some(parent) = current {
        match parent.kind() {
            "declaration_list" => {
                current = parent.parent();
            }
            "impl_item" | "trait_item" => {
                // Find the type/name child
                for i in 0..parent.child_count() {
                    if let Some(child) = parent.child(i) {
                        if matches!(child.kind(), "type_identifier" | "generic_type" | "scoped_type_identifier") {
                            if let Ok(type_name) = child.utf8_text(source) {
                                // Strip generic params for readability: "Vec<T>" -> "Vec"
                                let base = type_name.split('<').next().unwrap_or(type_name);
                                return format!("{base}::{name}");
                            }
                        }
                    }
                }
                return name.to_string();
            }
            "class_definition" | "class_body" => {
                // Python: method inside class
                if parent.kind() == "class_definition" {
                    for i in 0..parent.child_count() {
                        if let Some(child) = parent.child(i) {
                            if child.kind() == "identifier" {
                                if let Ok(class_name) = child.utf8_text(source) {
                                    return format!("{class_name}.{name}");
                                }
                            }
                        }
                    }
                }
                current = parent.parent();
            }
            "source_file" | "module" => break,
            _ => {
                current = parent.parent();
            }
        }
    }
    name.to_string()
}

fn extract_signature(node: tree_sitter::Node<'_>, source: &[u8], lang: &Lang) -> Option<String> {
    let body_kind = match lang {
        Lang::Rust => "block",
        Lang::Python => "block",
    };
    // Find the body node and take everything before it as the signature.
    let body_start = (0..node.child_count())
        .filter_map(|i| node.child(i))
        .find(|c| c.kind() == body_kind)
        .map(|c| c.start_byte());

    let sig_range = if let Some(end) = body_start {
        node.start_byte()..end
    } else {
        // No body (e.g. trait method declaration) - take the whole node text
        node.start_byte()..node.end_byte()
    };

    let text = std::str::from_utf8(&source[sig_range]).ok()?;
    let sig = text
        .lines()
        .map(|l| l.trim_end())
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_string();
    // Trim trailing whitespace and trailing brace/colon artifacts
    let sig = sig.trim_end_matches(|c| c == '{' || c == ':' || c == ' ').to_string();
    if sig.is_empty() { None } else { Some(sig) }
}

fn extract_doc_comment(node: tree_sitter::Node<'_>, source: &[u8]) -> Option<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut prev = node.prev_named_sibling();
    while let Some(sib) = prev {
        let kind = sib.kind();
        if kind == "line_comment" || kind == "block_comment" {
            let text = sib.utf8_text(source).unwrap_or("").trim().to_string();
            if text.starts_with("///") || text.starts_with("//!") || text.starts_with("/**") {
                lines.push(text);
                prev = sib.prev_named_sibling();
                continue;
            }
        }
        // Python docstring: first child of block is expression_statement containing string
        // Handled separately via body scanning below.
        break;
    }
    lines.reverse();

    // Python: check first statement in body for a string literal (docstring)
    if lines.is_empty() {
        let body_node = (0..node.child_count())
            .filter_map(|i| node.child(i))
            .find(|c| c.kind() == "block");
        if let Some(body) = body_node {
            if let Some(first) = body.named_child(0) {
                if first.kind() == "expression_statement" {
                    if let Some(str_node) = first.named_child(0) {
                        if str_node.kind() == "string" {
                            if let Ok(text) = str_node.utf8_text(source) {
                                let cleaned = text
                                    .trim_matches(|c| c == '"' || c == '\'' || c == '\n')
                                    .trim()
                                    .to_string();
                                if !cleaned.is_empty() {
                                    return Some(cleaned);
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    if lines.is_empty() { None } else { Some(lines.join("\n")) }
}

fn extract_body_preview(node: tree_sitter::Node<'_>, source: &[u8], _lang: &Lang) -> Option<String> {
    let body = (0..node.child_count())
        .filter_map(|i| node.child(i))
        .find(|c| c.kind() == "block")?;

    let body_text = body.utf8_text(source).ok()?;
    let preview: Vec<&str> = body_text
        .lines()
        .skip(1) // skip the opening `{`
        .filter(|l| !l.trim().is_empty())
        .take(8)
        .collect();
    if preview.is_empty() {
        None
    } else {
        Some(preview.join("\n"))
    }
}

/// Build the text representation fed to the embedding model.
pub fn build_embed_text(chunk: &Chunk, file_path: &str) -> String {
    let mut parts = vec![
        format!("{}: {}", chunk.symbol_kind, chunk.symbol_name),
        format!("File: {file_path}"),
    ];
    if let Some(ref sig) = chunk.signature {
        parts.push(format!("Signature: {sig}"));
    }
    if let Some(ref doc) = chunk.doc_comment {
        let first_line = doc.lines().next().unwrap_or("").trim();
        if !first_line.is_empty() {
            // Strip comment markers for cleaner embedding
            let cleaned = first_line
                .trim_start_matches("///")
                .trim_start_matches("//!")
                .trim_start_matches("/**")
                .trim();
            parts.push(format!("Doc: {cleaned}"));
        }
    }
    if let Some(ref preview) = chunk.body_preview {
        parts.push(preview.clone());
    }
    parts.join("\n")
}
