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
            } else if cap.index == name_idx
                && let Ok(t) = cap.node.utf8_text(source_bytes) {
                name_text = t.to_string();
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
                    if let Some(child) = parent.child(i)
                        && matches!(child.kind(), "type_identifier" | "generic_type" | "scoped_type_identifier")
                        && let Ok(type_name) = child.utf8_text(source) {
                        let base = type_name.split('<').next().unwrap_or(type_name);
                        return format!("{base}::{name}");
                    }
                }
                return name.to_string();
            }
            "class_definition" | "class_body" => {
                // Python: method inside class
                if parent.kind() == "class_definition" {
                    for i in 0..parent.child_count() {
                        if let Some(child) = parent.child(i)
                            && child.kind() == "identifier"
                            && let Ok(class_name) = child.utf8_text(source) {
                            return format!("{class_name}.{name}");
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
    let sig = sig.trim_end_matches(['{', ':', ' ']).to_string();
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
        if let Some(body) = body_node
            && let Some(first) = body.named_child(0)
            && first.kind() == "expression_statement"
            && let Some(str_node) = first.named_child(0)
            && str_node.kind() == "string"
            && let Ok(text) = str_node.utf8_text(source) {
            let cleaned = text
                .trim_matches(|c| c == '"' || c == '\'' || c == '\n')
                .trim()
                .to_string();
            if !cleaned.is_empty() {
                return Some(cleaned);
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

#[cfg(test)]
mod tests {
    use super::{build_embed_text, parse_file, Chunk, Lang};

    fn parse_rust(src: &str) -> Vec<Chunk> {
        parse_file(src, "src/lib.rs", &Lang::Rust)
    }

    fn parse_python(src: &str) -> Vec<Chunk> {
        parse_file(src, "foo.py", &Lang::Python)
    }

    fn find<'a>(chunks: &'a [Chunk], name: &str) -> &'a Chunk {
        chunks.iter().find(|c| c.symbol_name == name)
            .unwrap_or_else(|| panic!("chunk '{name}' not found; got: {:?}", chunks.iter().map(|c| &c.symbol_name).collect::<Vec<_>>()))
    }

    // --- Rust: basic symbol extraction ---

    #[test]
    fn rust_top_level_function() {
        let chunks = parse_rust("fn greet(name: &str) -> String { format!(\"hello {name}\") }");
        assert!(!chunks.is_empty(), "should extract at least one chunk");
        let c = find(&chunks, "greet");
        assert_eq!(c.symbol_kind, "function");
        assert_eq!(c.qualified_name, "greet");
        assert_eq!(c.language, "rust");
    }

    #[test]
    fn rust_struct() {
        let chunks = parse_rust("struct Foo { x: i32 }");
        let c = find(&chunks, "Foo");
        assert_eq!(c.symbol_kind, "struct");
    }

    #[test]
    fn rust_enum() {
        let chunks = parse_rust("enum Color { Red, Green, Blue }");
        let c = find(&chunks, "Color");
        assert_eq!(c.symbol_kind, "enum");
    }

    #[test]
    fn rust_trait() {
        let chunks = parse_rust("trait Speak { fn speak(&self) -> String; }");
        let c = find(&chunks, "Speak");
        assert_eq!(c.symbol_kind, "trait");
    }

    #[test]
    fn rust_method_qualified_name() {
        let src = r#"
struct Counter { n: u32 }
impl Counter {
    fn increment(&mut self) { self.n += 1; }
}
"#;
        let chunks = parse_rust(src);
        let c = find(&chunks, "increment");
        assert_eq!(c.qualified_name, "Counter::increment",
            "method inside impl should get qualified name");
    }

    #[test]
    fn rust_line_numbers_are_one_indexed() {
        let src = "fn first() {}\nfn second() {}";
        let chunks = parse_rust(src);
        let first = find(&chunks, "first");
        let second = find(&chunks, "second");
        assert_eq!(first.line_start, 1);
        assert_eq!(second.line_start, 2);
    }

    #[test]
    fn rust_doc_comment_extracted() {
        let src = r#"
/// This is a doc comment.
fn documented() -> i32 { 0 }
"#;
        let chunks = parse_rust(src);
        let c = find(&chunks, "documented");
        assert!(
            c.doc_comment.as_deref().unwrap_or("").contains("This is a doc comment"),
            "doc comment should be captured"
        );
    }

    #[test]
    fn rust_signature_strips_body() {
        let src = "fn add(a: i32, b: i32) -> i32 { a + b }";
        let chunks = parse_rust(src);
        let c = find(&chunks, "add");
        let sig = c.signature.as_deref().unwrap_or("");
        assert!(sig.contains("fn add"), "signature should include fn keyword and name");
        assert!(sig.contains("-> i32"), "signature should include return type");
        assert!(!sig.contains("a + b"), "signature must not include function body");
    }

    #[test]
    fn rust_empty_source_returns_no_chunks() {
        assert!(parse_rust("").is_empty());
        assert!(parse_rust("// just a comment").is_empty());
        assert!(parse_rust("use std::fmt;").is_empty());
    }

    // --- Python: basic symbol extraction ---

    #[test]
    fn python_function() {
        let chunks = parse_python("def greet(name):\n    return f'hello {name}'\n");
        let c = find(&chunks, "greet");
        assert_eq!(c.symbol_kind, "function");
        assert_eq!(c.language, "python");
    }

    #[test]
    fn python_class() {
        let chunks = parse_python("class Animal:\n    pass\n");
        let c = find(&chunks, "Animal");
        assert_eq!(c.symbol_kind, "class");
    }

    #[test]
    fn python_method_qualified_name() {
        let src = "class Dog:\n    def bark(self):\n        print('woof')\n";
        let chunks = parse_python(src);
        let c = find(&chunks, "bark");
        assert_eq!(c.qualified_name, "Dog.bark",
            "method inside class should get qualified name");
    }

    #[test]
    fn python_docstring_extracted() {
        let src = "def documented():\n    \"\"\"This does something useful.\"\"\"\n    pass\n";
        let chunks = parse_python(src);
        let c = find(&chunks, "documented");
        assert!(
            c.doc_comment.as_deref().unwrap_or("").contains("something useful"),
            "Python docstring should be captured"
        );
    }

    // --- build_embed_text ---

    #[test]
    fn embed_text_includes_kind_and_name() {
        let chunk = Chunk {
            symbol_name: "my_func".to_string(),
            qualified_name: "MyStruct::my_func".to_string(),
            symbol_kind: "function".to_string(),
            signature: None,
            doc_comment: None,
            body_preview: None,
            line_start: 1,
            line_end: 5,
            language: "rust".to_string(),
        };
        let text = build_embed_text(&chunk, "src/lib.rs");
        assert!(text.contains("function: my_func"));
        assert!(text.contains("File: src/lib.rs"));
    }

    #[test]
    fn embed_text_includes_signature_when_present() {
        let chunk = Chunk {
            symbol_name: "add".to_string(),
            qualified_name: "add".to_string(),
            symbol_kind: "function".to_string(),
            signature: Some("fn add(a: i32, b: i32) -> i32".to_string()),
            doc_comment: None,
            body_preview: None,
            line_start: 1,
            line_end: 1,
            language: "rust".to_string(),
        };
        let text = build_embed_text(&chunk, "src/math.rs");
        assert!(text.contains("Signature: fn add(a: i32, b: i32) -> i32"));
    }

    #[test]
    fn embed_text_strips_doc_comment_markers() {
        let chunk = Chunk {
            symbol_name: "foo".to_string(),
            qualified_name: "foo".to_string(),
            symbol_kind: "function".to_string(),
            signature: None,
            doc_comment: Some("/// Returns the answer.".to_string()),
            body_preview: None,
            line_start: 1,
            line_end: 1,
            language: "rust".to_string(),
        };
        let text = build_embed_text(&chunk, "src/lib.rs");
        // Comment markers stripped, content kept
        assert!(text.contains("Returns the answer"));
        assert!(!text.contains("///"));
    }
}
