use tree_sitter::{Language, Node, Parser, Query, QueryCursor, StreamingIterator};

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

#[derive(Copy, Clone)]
pub enum ChunkingStrategy {
    Code,
    Structural,
}

#[derive(Copy, Clone)]
pub enum Lang {
    // Strategy A: Code — extract named symbols
    Rust,
    Python,
    TypeScript,
    Tsx,
    JavaScript,
    Go,
    Cpp,
    Java,
    C,
    Bash,
    Ruby,
    CSharp,
    Php,
    Scala,
    Swift,
    Elixir,
    Lua,
    Haskell,
    Nix,
    Solidity,
    Kotlin,
    OCaml,
    R,
    Zig,
    Erlang,
    Ql,
    Elm,
    Powershell,
    Dart,
    ObjC,
    TlaPlus,
    // Strategy B: Structural — extract top-level sections/blocks
    Css,
    Json,
    Html,
    Yaml,
    Hcl,
    Toml,
    Markdown,
    EmbeddedTemplate,
    Diff,
    Xml,
    Sql,
}

impl Lang {
    pub fn from_extension(ext: &str) -> Option<Self> {
        match ext {
            "rs" => Some(Self::Rust),
            "py" | "pyi" => Some(Self::Python),
            "ts" => Some(Self::TypeScript),
            "tsx" => Some(Self::Tsx),
            "js" | "mjs" | "cjs" => Some(Self::JavaScript),
            "go" => Some(Self::Go),
            "cpp" | "cc" | "cxx" => Some(Self::Cpp),
            "java" => Some(Self::Java),
            "c" | "h" => Some(Self::C),
            "sh" | "bash" | "zsh" => Some(Self::Bash),
            "rb" => Some(Self::Ruby),
            "cs" => Some(Self::CSharp),
            "php" => Some(Self::Php),
            "scala" | "sc" => Some(Self::Scala),
            "swift" => Some(Self::Swift),
            "ex" | "exs" => Some(Self::Elixir),
            "lua" => Some(Self::Lua),
            "hs" => Some(Self::Haskell),
            "nix" => Some(Self::Nix),
            "sol" => Some(Self::Solidity),
            "kt" | "kts" => Some(Self::Kotlin),
            "ml" | "mli" => Some(Self::OCaml),
            "r" => Some(Self::R),
            "zig" => Some(Self::Zig),
            "erl" | "hrl" => Some(Self::Erlang),
            "ql" => Some(Self::Ql),
            "elm" => Some(Self::Elm),
            "ps1" | "psm1" => Some(Self::Powershell),
            "dart" => Some(Self::Dart),
            "m" | "mm" => Some(Self::ObjC),
            "tla" => Some(Self::TlaPlus),
            "css" => Some(Self::Css),
            "json" | "jsonc" => Some(Self::Json),
            "html" | "htm" => Some(Self::Html),
            "yaml" | "yml" => Some(Self::Yaml),
            "hcl" | "tf" => Some(Self::Hcl),
            "toml" => Some(Self::Toml),
            "md" | "mdx" => Some(Self::Markdown),
            "erb" | "ejs" => Some(Self::EmbeddedTemplate),
            "diff" | "patch" => Some(Self::Diff),
            "xml" => Some(Self::Xml),
            "sql" => Some(Self::Sql),
            _ => None,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::Python => "python",
            Self::TypeScript => "typescript",
            Self::Tsx => "tsx",
            Self::JavaScript => "javascript",
            Self::Go => "go",
            Self::Cpp => "cpp",
            Self::Java => "java",
            Self::C => "c",
            Self::Bash => "bash",
            Self::Ruby => "ruby",
            Self::CSharp => "csharp",
            Self::Php => "php",
            Self::Scala => "scala",
            Self::Swift => "swift",
            Self::Elixir => "elixir",
            Self::Lua => "lua",
            Self::Haskell => "haskell",
            Self::Nix => "nix",
            Self::Solidity => "solidity",
            Self::Kotlin => "kotlin",
            Self::OCaml => "ocaml",
            Self::R => "r",
            Self::Zig => "zig",
            Self::Erlang => "erlang",
            Self::Ql => "ql",
            Self::Elm => "elm",
            Self::Powershell => "powershell",
            Self::Dart => "dart",
            Self::ObjC => "objc",
            Self::TlaPlus => "tlaplus",
            Self::Css => "css",
            Self::Json => "json",
            Self::Html => "html",
            Self::Yaml => "yaml",
            Self::Hcl => "hcl",
            Self::Toml => "toml",
            Self::Markdown => "markdown",
            Self::EmbeddedTemplate => "embedded_template",
            Self::Diff => "diff",
            Self::Xml => "xml",
            Self::Sql => "sql",
        }
    }

    pub fn chunking_strategy(&self) -> ChunkingStrategy {
        match self {
            Self::Css | Self::Json | Self::Html | Self::Yaml | Self::Hcl
            | Self::Toml | Self::Markdown | Self::EmbeddedTemplate | Self::Diff
            | Self::Xml | Self::Sql => ChunkingStrategy::Structural,
            _ => ChunkingStrategy::Code,
        }
    }

    fn tree_sitter_language(&self) -> Language {
        match self {
            Self::Rust => tree_sitter_rust::LANGUAGE.into(),
            Self::Python => tree_sitter_python::LANGUAGE.into(),
            Self::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Self::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
            Self::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
            Self::Go => tree_sitter_go::LANGUAGE.into(),
            Self::Cpp => tree_sitter_cpp::LANGUAGE.into(),
            Self::Java => tree_sitter_java::LANGUAGE.into(),
            Self::C => tree_sitter_c::LANGUAGE.into(),
            Self::Bash => tree_sitter_bash::LANGUAGE.into(),
            Self::Ruby => tree_sitter_ruby::LANGUAGE.into(),
            Self::CSharp => tree_sitter_c_sharp::LANGUAGE.into(),
            Self::Php => tree_sitter_php::LANGUAGE_PHP.into(),
            Self::Scala => tree_sitter_scala::LANGUAGE.into(),
            Self::Swift => tree_sitter_swift::LANGUAGE.into(),
            Self::Elixir => tree_sitter_elixir::LANGUAGE.into(),
            Self::Lua => tree_sitter_lua::LANGUAGE.into(),
            Self::Haskell => tree_sitter_haskell::LANGUAGE.into(),
            Self::Nix => tree_sitter_nix::LANGUAGE.into(),
            Self::Solidity => tree_sitter_solidity::LANGUAGE.into(),
            Self::Kotlin => tree_sitter_kotlin_ng::LANGUAGE.into(),
            Self::OCaml => tree_sitter_ocaml::LANGUAGE_OCAML.into(),
            Self::R => tree_sitter_r::LANGUAGE.into(),
            Self::Zig => tree_sitter_zig::LANGUAGE.into(),
            Self::Erlang => tree_sitter_erlang::LANGUAGE.into(),
            Self::Ql => tree_sitter_ql::LANGUAGE.into(),
            Self::Elm => tree_sitter_elm::LANGUAGE.into(),
            Self::Powershell => tree_sitter_powershell::LANGUAGE.into(),
            Self::Dart => tree_sitter_dart::LANGUAGE.into(),
            Self::ObjC => tree_sitter_objc::LANGUAGE.into(),
            Self::TlaPlus => tree_sitter_tlaplus::LANGUAGE.into(),
            Self::Css => tree_sitter_css::LANGUAGE.into(),
            Self::Json => tree_sitter_json::LANGUAGE.into(),
            Self::Html => tree_sitter_html::LANGUAGE.into(),
            Self::Yaml => tree_sitter_yaml::LANGUAGE.into(),
            Self::Hcl => tree_sitter_hcl::LANGUAGE.into(),
            Self::Toml => tree_sitter_toml_ng::LANGUAGE.into(),
            Self::Markdown => tree_sitter_md::LANGUAGE.into(),
            Self::EmbeddedTemplate => tree_sitter_embedded_template::LANGUAGE.into(),
            Self::Diff => tree_sitter_diff::LANGUAGE.into(),
            Self::Xml => tree_sitter_xml::LANGUAGE_XML.into(),
            Self::Sql => tree_sitter_sequel::LANGUAGE.into(),
        }
    }

    fn query_str(&self) -> &'static str {
        match self {
            Self::Rust => RUST_QUERY,
            Self::Python => PYTHON_QUERY,
            Self::TypeScript | Self::Tsx => TYPESCRIPT_QUERY,
            Self::JavaScript => JAVASCRIPT_QUERY,
            Self::Go => GO_QUERY,
            Self::Cpp => CPP_QUERY,
            Self::Java => JAVA_QUERY,
            Self::C => C_QUERY,
            Self::Bash => BASH_QUERY,
            Self::Ruby => RUBY_QUERY,
            Self::CSharp => CSHARP_QUERY,
            Self::Php => PHP_QUERY,
            Self::Scala => SCALA_QUERY,
            Self::Swift => SWIFT_QUERY,
            Self::Elixir => ELIXIR_QUERY,
            Self::Lua => LUA_QUERY,
            Self::Haskell => HASKELL_QUERY,
            Self::Nix => NIX_QUERY,
            Self::Solidity => SOLIDITY_QUERY,
            Self::Kotlin => KOTLIN_QUERY,
            Self::OCaml => OCAML_QUERY,
            Self::R => R_QUERY,
            Self::Zig => ZIG_QUERY,
            Self::Erlang => ERLANG_QUERY,
            Self::Ql => QL_QUERY,
            Self::Elm => ELM_QUERY,
            Self::Powershell => POWERSHELL_QUERY,
            Self::Dart => DART_QUERY,
            Self::ObjC => OBJC_QUERY,
            Self::TlaPlus => TLAPLUS_QUERY,
            Self::Css => CSS_QUERY,
            Self::Json => JSON_QUERY,
            Self::Html => HTML_QUERY,
            Self::Yaml => YAML_QUERY,
            Self::Hcl => HCL_QUERY,
            Self::Toml => TOML_QUERY,
            Self::Markdown => MARKDOWN_QUERY,
            Self::EmbeddedTemplate => EMBEDDED_TEMPLATE_QUERY,
            Self::Diff => DIFF_QUERY,
            Self::Xml => XML_QUERY,
            Self::Sql => SQL_QUERY,
        }
    }

    // The node kind that delimits a function/method body. Used to split
    // signature from body in extract_signature. Defaults to "block".
    fn body_node_kind(&self) -> &'static str {
        match self {
            Self::JavaScript | Self::TypeScript | Self::Tsx => "statement_block",
            Self::C | Self::Cpp | Self::Php => "compound_statement",
            Self::Swift => "function_body",
            Self::Elixir => "do_block",
            _ => "block",
        }
    }
}

// --- Strategy A: Code queries (extract named symbols) ---

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

const TYPESCRIPT_QUERY: &str = r#"
(function_declaration name: (identifier) @name) @symbol
(method_definition name: (property_identifier) @name) @symbol
(class_declaration name: (type_identifier) @name) @symbol
(interface_declaration name: (type_identifier) @name) @symbol
(type_alias_declaration name: (type_identifier) @name) @symbol
"#;

const JAVASCRIPT_QUERY: &str = r#"
(function_declaration name: (identifier) @name) @symbol
(method_definition name: (property_identifier) @name) @symbol
(class_declaration name: (identifier) @name) @symbol
"#;

const GO_QUERY: &str = r#"
(function_declaration name: (identifier) @name) @symbol
(method_declaration name: (field_identifier) @name) @symbol
(type_declaration (type_spec name: (type_identifier) @name)) @symbol
"#;

const CPP_QUERY: &str = r#"
(function_definition declarator: (function_declarator declarator: (identifier) @name)) @symbol
(class_specifier name: (type_identifier) @name) @symbol
(struct_specifier name: (type_identifier) @name) @symbol
"#;

const JAVA_QUERY: &str = r#"
(method_declaration name: (identifier) @name) @symbol
(class_declaration name: (identifier) @name) @symbol
(interface_declaration name: (identifier) @name) @symbol
(enum_declaration name: (identifier) @name) @symbol
"#;

const C_QUERY: &str = r#"
(function_definition declarator: (function_declarator declarator: (identifier) @name)) @symbol
(struct_specifier name: (type_identifier) @name) @symbol
"#;

const BASH_QUERY: &str = r#"
(function_definition name: (word) @name) @symbol
"#;

const RUBY_QUERY: &str = r#"
(method name: (identifier) @name) @symbol
(singleton_method name: (identifier) @name) @symbol
(class name: (constant) @name) @symbol
(module name: (constant) @name) @symbol
"#;

const CSHARP_QUERY: &str = r#"
(method_declaration name: (identifier) @name) @symbol
(class_declaration name: (identifier) @name) @symbol
(interface_declaration name: (identifier) @name) @symbol
(struct_declaration name: (identifier) @name) @symbol
(enum_declaration name: (identifier) @name) @symbol
"#;

const PHP_QUERY: &str = r#"
(function_definition name: (name) @name) @symbol
(method_declaration name: (name) @name) @symbol
(class_declaration name: (name) @name) @symbol
"#;

const SCALA_QUERY: &str = r#"
(function_definition name: (identifier) @name) @symbol
(class_definition name: (identifier) @name) @symbol
(object_definition name: (identifier) @name) @symbol
(trait_definition name: (identifier) @name) @symbol
"#;

const SWIFT_QUERY: &str = r#"
(function_declaration name: (simple_identifier) @name) @symbol
(class_declaration name: (type_identifier) @name) @symbol
(protocol_declaration name: (type_identifier) @name) @symbol
"#;

const ELIXIR_QUERY: &str = r#"
(call target: (identifier) @_def
      (arguments . (call target: (identifier) @name))
      (#match? @_def "^def(p|macro|macrop)?$")) @symbol
"#;

const LUA_QUERY: &str = r#"
(function_declaration name: (identifier) @name) @symbol
"#;

const HASKELL_QUERY: &str = r#"
(function name: (variable) @name) @symbol
(data_type name: (name) @name) @symbol
(type_synomym name: (name) @name) @symbol
"#;

const NIX_QUERY: &str = r#"
(binding attrpath: (attrpath (identifier) @name) expression: (function_expression)) @symbol
"#;

const SOLIDITY_QUERY: &str = r#"
(function_definition name: (identifier) @name) @symbol
(contract_declaration name: (identifier) @name) @symbol
(event_definition name: (identifier) @name) @symbol
(modifier_definition name: (identifier) @name) @symbol
"#;

const KOTLIN_QUERY: &str = r#"
(function_declaration name: (identifier) @name) @symbol
(class_declaration name: (identifier) @name) @symbol
(object_declaration name: (identifier) @name) @symbol
"#;

const OCAML_QUERY: &str = r#"
(let_binding pattern: (value_name) @name) @symbol
(type_definition (type_binding name: (type_constructor) @name)) @symbol
(module_definition (module_binding (module_name) @name)) @symbol
"#;

const R_QUERY: &str = r#"
(binary_operator lhs: (identifier) @name rhs: (function_definition)) @symbol
"#;

const ZIG_QUERY: &str = r#"
(function_declaration name: (identifier) @name) @symbol
"#;

const ERLANG_QUERY: &str = r#"
(function_clause name: (atom) @name) @symbol
"#;

const QL_QUERY: &str = r#"
(classlessPredicate name: (predicateName) @name) @symbol
(dataclass name: (className) @name) @symbol
"#;

const ELM_QUERY: &str = r#"
(value_declaration (function_declaration_left (lower_case_identifier) @name)) @symbol
(type_alias_declaration name: (upper_case_identifier) @name) @symbol
(type_declaration name: (upper_case_identifier) @name) @symbol
"#;

const POWERSHELL_QUERY: &str = r#"
(function_statement (function_name) @name) @symbol
"#;

const DART_QUERY: &str = r#"
(function_signature name: (identifier) @name) @symbol
(class_declaration name: (identifier) @name) @symbol
(mixin_declaration name: (identifier) @name) @symbol
"#;

const OBJC_QUERY: &str = r#"
(class_interface name: (identifier) @name) @symbol
(class_implementation name: (identifier) @name) @symbol
(protocol_declaration name: (identifier) @name) @symbol
"#;

const TLAPLUS_QUERY: &str = r#"
(operator_definition name: (identifier) @name) @symbol
(function_definition name: (identifier) @name) @symbol
"#;

// --- Strategy B: Structural queries (extract top-level sections) ---
// These use only @symbol; the name is derived from the node's first line.

const CSS_QUERY: &str = r#"
(rule_set) @symbol
(at_rule) @symbol
"#;

const JSON_QUERY: &str = r#"
(document (object (pair) @symbol))
"#;

const HTML_QUERY: &str = r#"
(element) @symbol
"#;

const YAML_QUERY: &str = r#"
(block_mapping_pair) @symbol
"#;

const HCL_QUERY: &str = r#"
(block) @symbol
(attribute) @symbol
"#;

const TOML_QUERY: &str = r#"
(table) @symbol
(table_array_element) @symbol
"#;

const MARKDOWN_QUERY: &str = r#"
(atx_heading) @symbol
(setext_heading) @symbol
"#;

const EMBEDDED_TEMPLATE_QUERY: &str = r#"
(template) @symbol
"#;

const DIFF_QUERY: &str = r#"
(hunk) @symbol
"#;

const XML_QUERY: &str = r#"
(element) @symbol
"#;

const SQL_QUERY: &str = r#"
(statement) @symbol
"#;

// Iterate node children compatible with tree-sitter 0.26 (child() takes u32).
fn node_children(node: Node<'_>) -> impl Iterator<Item = Node<'_>> {
    (0..node.child_count()).filter_map(move |i| node.child(i as u32))
}

pub fn parse_file(source: &str, _file_path: &str, lang: &Lang) -> Vec<Chunk> {
    match lang.chunking_strategy() {
        ChunkingStrategy::Code => parse_code(source, lang),
        ChunkingStrategy::Structural => parse_structural(source, lang),
    }
}

fn parse_code(source: &str, lang: &Lang) -> Vec<Chunk> {
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
    let body_kind = lang.body_node_kind();

    let mut matches = cursor.matches(&query, tree.root_node(), source_bytes);
    while let Some(m) = matches.next() {
        let mut symbol_node = None;
        let mut name_text = String::new();

        for cap in m.captures {
            if cap.index == symbol_idx {
                symbol_node = Some(cap.node);
            } else if cap.index == name_idx
                && let Ok(t) = cap.node.utf8_text(source_bytes)
            {
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
        let signature = extract_signature(node, source_bytes, body_kind);
        let doc_comment = extract_doc_comment(node, source_bytes);
        let body_preview = extract_body_preview(node, source_bytes, body_kind);
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

fn parse_structural(source: &str, lang: &Lang) -> Vec<Chunk> {
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

    let source_bytes = source.as_bytes();
    let mut cursor = QueryCursor::new();
    let mut chunks = Vec::new();

    let mut matches = cursor.matches(&query, tree.root_node(), source_bytes);
    while let Some(m) = matches.next() {
        let node = match m.captures.iter().find(|c| c.index == symbol_idx) {
            Some(c) => c.node,
            None => continue,
        };

        let node_text = match node.utf8_text(source_bytes) {
            Ok(t) => t,
            Err(_) => continue,
        };

        // Derive the section name from the first non-empty line of the node.
        let symbol_name: String = node_text
            .lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("")
            .trim()
            .chars()
            .take(120)
            .collect();
        if symbol_name.is_empty() {
            continue;
        }

        let body_preview: String = node_text
            .lines()
            .filter(|l| !l.trim().is_empty())
            .take(8)
            .collect::<Vec<_>>()
            .join("\n");

        let start = node.start_position();
        let end = node.end_position();

        chunks.push(Chunk {
            qualified_name: symbol_name.clone(),
            symbol_name,
            symbol_kind: "section".to_string(),
            signature: None,
            doc_comment: None,
            body_preview: Some(body_preview),
            line_start: start.row + 1,
            line_end: end.row + 1,
            language: lang.name().to_string(),
        });
    }

    chunks
}

fn node_kind_to_symbol_kind(kind: &str) -> String {
    if kind.contains("method") {
        "method"
    } else if kind.contains("function") {
        "function"
    } else if kind.contains("class") {
        "class"
    } else if kind.contains("struct") {
        "struct"
    } else if kind.contains("enum") {
        "enum"
    } else if kind.contains("interface") || kind.contains("trait") || kind.contains("protocol") {
        "interface"
    } else if kind.contains("type") {
        "type"
    } else if kind.contains("module") || kind.contains("object") {
        "module"
    } else {
        "symbol"
    }
    .to_string()
}

/// Walk up the parent chain to find the enclosing impl block's type name.
fn qualified_name_for(node: Node<'_>, name: &str, source: &[u8]) -> String {
    let mut current = node.parent();
    while let Some(parent) = current {
        match parent.kind() {
            "declaration_list" => {
                current = parent.parent();
            }
            "impl_item" | "trait_item" => {
                for child in node_children(parent) {
                    if matches!(child.kind(), "type_identifier" | "generic_type" | "scoped_type_identifier")
                        && let Ok(type_name) = child.utf8_text(source)
                    {
                        let base = type_name.split('<').next().unwrap_or(type_name);
                        return format!("{base}::{name}");
                    }
                }
                return name.to_string();
            }
            "class_definition" | "class_body" => {
                if parent.kind() == "class_definition" {
                    for child in node_children(parent) {
                        if child.kind() == "identifier"
                            && let Ok(class_name) = child.utf8_text(source)
                        {
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

fn extract_signature(node: Node<'_>, source: &[u8], body_kind: &str) -> Option<String> {
    let body_start = node_children(node)
        .find(|c| c.kind() == body_kind)
        .map(|c| c.start_byte());

    let sig_range = if let Some(end) = body_start {
        node.start_byte()..end
    } else {
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
    let sig = sig.trim_end_matches(['{', ':', ' ']).to_string();
    if sig.is_empty() { None } else { Some(sig) }
}

fn extract_doc_comment(node: Node<'_>, source: &[u8]) -> Option<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut prev = node.prev_named_sibling();
    while let Some(sib) = prev {
        let kind = sib.kind();
        if kind.contains("comment") {
            let text = sib.utf8_text(source).unwrap_or("").trim().to_string();
            if text.starts_with("///") || text.starts_with("//!") || text.starts_with("/**") {
                lines.push(text);
                prev = sib.prev_named_sibling();
                continue;
            }
        }
        break;
    }
    lines.reverse();

    // Python: check first statement in body for a string literal (docstring)
    if lines.is_empty() {
        let body_node = node_children(node).find(|c| c.kind() == "block");
        if let Some(body) = body_node
            && let Some(first) = body.named_child(0)
            && first.kind() == "expression_statement"
            && let Some(str_node) = first.named_child(0)
            && str_node.kind() == "string"
            && let Ok(text) = str_node.utf8_text(source)
        {
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

fn extract_body_preview(node: Node<'_>, source: &[u8], body_kind: &str) -> Option<String> {
    let body = node_children(node).find(|c| c.kind() == body_kind)?;
    let body_text = body.utf8_text(source).ok()?;
    let preview: Vec<&str> = body_text
        .lines()
        .skip(1)
        .filter(|l| !l.trim().is_empty())
        .take(8)
        .collect();
    if preview.is_empty() { None } else { Some(preview.join("\n")) }
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
        assert_eq!(c.symbol_kind, "interface");
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

    // --- TypeScript ---

    fn parse_ts(src: &str) -> Vec<Chunk> {
        parse_file(src, "src/index.ts", &Lang::TypeScript)
    }

    #[test]
    fn typescript_function_declaration() {
        let chunks = parse_ts("function greet(name: string): string { return `hello ${name}`; }");
        let c = find(&chunks, "greet");
        assert_eq!(c.symbol_kind, "function");
        assert_eq!(c.language, "typescript");
    }

    #[test]
    fn typescript_class_declaration() {
        let chunks = parse_ts("class Animal { constructor(public name: string) {} }");
        let c = find(&chunks, "Animal");
        assert_eq!(c.symbol_kind, "class");
    }

    #[test]
    fn typescript_interface_declaration() {
        let chunks = parse_ts("interface Flyable { fly(): void; }");
        let c = find(&chunks, "Flyable");
        assert_eq!(c.symbol_kind, "interface");
    }

    #[test]
    fn typescript_type_alias() {
        let chunks = parse_ts("type Callback = (err: Error | null) => void;");
        let c = find(&chunks, "Callback");
        assert_eq!(c.symbol_kind, "type");
    }

    #[test]
    fn typescript_method_in_class() {
        let src = "class Dog { bark(): string { return 'woof'; } }";
        let chunks = parse_ts(src);
        let c = find(&chunks, "bark");
        assert_eq!(c.symbol_kind, "method");
    }

    // --- Go ---

    fn parse_go(src: &str) -> Vec<Chunk> {
        parse_file(src, "main.go", &Lang::Go)
    }

    #[test]
    fn go_function_declaration() {
        let src = "package main\nfunc Greet(name string) string { return \"hello \" + name }\n";
        let chunks = parse_go(src);
        let c = find(&chunks, "Greet");
        assert_eq!(c.symbol_kind, "function");
        assert_eq!(c.language, "go");
    }

    #[test]
    fn go_method_declaration() {
        let src = "package main\ntype Animal struct{}\nfunc (a *Animal) Speak() string { return \"...\" }\n";
        let chunks = parse_go(src);
        let c = find(&chunks, "Speak");
        assert_eq!(c.symbol_kind, "method");
    }

    #[test]
    fn go_type_declaration() {
        let src = "package main\ntype Config struct { Host string; Port int }\n";
        let chunks = parse_go(src);
        let c = find(&chunks, "Config");
        // Go type declarations are captured at the type_declaration node level, which maps to "type".
        assert_eq!(c.symbol_kind, "type");
    }

    // --- JavaScript ---

    fn parse_js(src: &str) -> Vec<Chunk> {
        parse_file(src, "index.js", &Lang::JavaScript)
    }

    #[test]
    fn javascript_function_declaration() {
        let chunks = parse_js("function greet(name) { return `hello ${name}`; }");
        let c = find(&chunks, "greet");
        assert_eq!(c.symbol_kind, "function");
        assert_eq!(c.language, "javascript");
    }

    #[test]
    fn javascript_class_declaration() {
        let chunks = parse_js("class Animal { constructor(name) { this.name = name; } }");
        let c = find(&chunks, "Animal");
        assert_eq!(c.symbol_kind, "class");
    }

    #[test]
    fn javascript_method_in_class() {
        let src = "class Dog { bark() { return 'woof'; } }";
        let chunks = parse_js(src);
        let c = find(&chunks, "bark");
        assert_eq!(c.symbol_kind, "method");
    }

    // --- All-language query compilation smoke test ---

    #[test]
    fn all_language_queries_compile() {
        use tree_sitter::Query;
        let langs = [
            Lang::Rust, Lang::Python, Lang::TypeScript, Lang::Tsx, Lang::JavaScript,
            Lang::Go, Lang::Cpp, Lang::Java, Lang::C, Lang::Bash, Lang::Ruby,
            Lang::CSharp, Lang::Php, Lang::Scala, Lang::Swift, Lang::Elixir,
            Lang::Lua, Lang::Haskell, Lang::Nix, Lang::Solidity, Lang::Kotlin,
            Lang::OCaml, Lang::R, Lang::Zig, Lang::Erlang, Lang::Ql, Lang::Elm,
            Lang::Powershell, Lang::Dart, Lang::ObjC, Lang::TlaPlus,
            Lang::Css, Lang::Json, Lang::Html, Lang::Yaml, Lang::Hcl,
            Lang::Toml, Lang::Markdown, Lang::EmbeddedTemplate, Lang::Diff,
            Lang::Xml, Lang::Sql,
        ];
        for lang in &langs {
            let ts_lang = lang.tree_sitter_language();
            Query::new(&ts_lang, lang.query_str())
                .unwrap_or_else(|e| panic!("Query for {} failed to compile: {e}", lang.name()));
        }
    }

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
        assert!(text.contains("Returns the answer"));
        assert!(!text.contains("///"));
    }
}
