use anyhow::{Context, Result};
use std::collections::HashSet;
use std::path::Path;
use streaming_iterator::StreamingIterator;
use tree_sitter::{Language, Node, Parser, Query, QueryCursor};

use crate::diff::{FileDiff, HunkRange};

// ── Public types ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct Symbol {
    pub name: String,
    pub kind: SymbolKind,
    pub start_line: u32, // 1-indexed
    pub end_line: u32,
    pub source: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SymbolKind {
    Function,
    ImplBlock,
    Struct,
    Enum,
    Trait,
    Other(String),
}

impl std::fmt::Display for SymbolKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            SymbolKind::Function => "fn",
            SymbolKind::ImplBlock => "impl",
            SymbolKind::Struct => "struct",
            SymbolKind::Enum => "enum",
            SymbolKind::Trait => "trait",
            SymbolKind::Other(s) => s.as_str(),
        };
        write!(f, "{s}")
    }
}

#[derive(Debug, Clone)]
pub struct CallEdge {
    pub caller: String,
    pub callee: String,
}

#[derive(Debug, Clone)]
pub struct FileAstContext {
    pub file: String,
    pub changed_symbols: Vec<Symbol>,
    pub all_symbols: Vec<Symbol>,
    pub call_edges: Vec<CallEdge>,
    pub raw_diff: String,
}

// ── Language detection ────────────────────────────────────────────────────────

/// Returns (Language, lang_name_str) for supported extensions.
fn detect_language(path: &Path) -> Option<(Language, &'static str)> {
    match path.extension().and_then(|e| e.to_str()) {
        Some("rs")                                              => Some((tree_sitter_rust::LANGUAGE.into(), "rust")),
        Some("py")                                              => Some((tree_sitter_python::LANGUAGE.into(), "python")),
        Some("go")                                              => Some((tree_sitter_go::LANGUAGE.into(), "go")),
        Some("js") | Some("jsx") | Some("mjs") | Some("cjs")   => Some((tree_sitter_javascript::LANGUAGE.into(), "javascript")),
        Some("ts")                                              => Some((tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(), "typescript")),
        Some("tsx")                                             => Some((tree_sitter_typescript::LANGUAGE_TSX.into(), "typescript")),
        Some("java")                                            => Some((tree_sitter_java::LANGUAGE.into(), "java")),
        Some("c") | Some("h")                                   => Some((tree_sitter_c::LANGUAGE.into(), "c")),
        Some("cpp") | Some("cc") | Some("cxx") | Some("hpp") | Some("hxx") => Some((tree_sitter_cpp::LANGUAGE.into(), "cpp")),
        Some("rb")                                              => Some((tree_sitter_ruby::LANGUAGE.into(), "ruby")),
        Some("cs")                                              => Some((tree_sitter_c_sharp::LANGUAGE.into(), "csharp")),
        Some("sh") | Some("bash")                               => Some((tree_sitter_bash::LANGUAGE.into(), "bash")),
        Some("scala") | Some("sc")                              => Some((tree_sitter_scala::LANGUAGE.into(), "scala")),
        _ => None,
    }
}

// ── Query sources ─────────────────────────────────────────────────────────────

fn symbol_query_source(lang_name: &str) -> &'static str {
    match lang_name {
        "rust" => {
            "(function_item name: (identifier) @name) @item
             (impl_item) @item
             (struct_item name: (type_identifier) @name) @item
             (enum_item name: (type_identifier) @name) @item
             (trait_item name: (type_identifier) @name) @item"
        }
        "python" => {
            "(function_definition name: (identifier) @name) @item
             (class_definition    name: (identifier) @name) @item"
        }
        "go" => {
            "(function_declaration name: (identifier) @name) @item
             (method_declaration name: (field_identifier) @name) @item
             (type_declaration (type_spec name: (type_identifier) @name)) @item"
        }
        "javascript" => {
            "(function_declaration name: (identifier) @name) @item
             (class_declaration name: (identifier) @name) @item
             (method_definition name: (property_identifier) @name) @item"
        }
        "typescript" => {
            "(function_declaration name: (identifier) @name) @item
             (class_declaration name: (type_identifier) @name) @item
             (method_definition name: (property_identifier) @name) @item
             (interface_declaration name: (type_identifier) @name) @item
             (type_alias_declaration name: (type_identifier) @name) @item"
        }
        "java" => {
            "(method_declaration name: (identifier) @name) @item
             (class_declaration name: (identifier) @name) @item
             (interface_declaration name: (identifier) @name) @item
             (enum_declaration name: (identifier) @name) @item"
        }
        "c" => {
            "(function_definition declarator: (function_declarator declarator: (identifier) @name)) @item
             (struct_specifier name: (type_identifier) @name) @item
             (enum_specifier name: (type_identifier) @name) @item"
        }
        "cpp" => {
            "(function_definition declarator: (function_declarator declarator: (identifier) @name)) @item
             (function_definition declarator: (function_declarator declarator: (qualified_identifier name: (identifier) @name))) @item
             (class_specifier name: (type_identifier) @name) @item
             (struct_specifier name: (type_identifier) @name) @item
             (namespace_definition name: (namespace_identifier) @name) @item"
        }
        "ruby" => {
            "(method name: (identifier) @name) @item
             (singleton_method name: (identifier) @name) @item
             (class name: (constant) @name) @item
             (module name: (constant) @name) @item"
        }
        "csharp" => {
            "(method_declaration name: (identifier) @name) @item
             (class_declaration name: (identifier) @name) @item
             (interface_declaration name: (identifier) @name) @item
             (struct_declaration name: (identifier) @name) @item
             (enum_declaration name: (identifier) @name) @item"
        }
        "bash" => {
            "(function_definition name: (word) @name) @item"
        }
        "scala" => {
            "(function_definition name: (identifier) @name) @item
             (class_definition name: (identifier) @name) @item
             (object_definition name: (identifier) @name) @item
             (trait_definition name: (identifier) @name) @item"
        }
        _ => "",
    }
}

fn call_query_source(lang_name: &str) -> &'static str {
    match lang_name {
        "rust" => {
            "(call_expression
               function: [
                 (identifier) @callee
                 (field_expression field: (field_identifier) @callee)
                 (scoped_identifier name: (identifier) @callee)
               ]
             )"
        }
        "python" => {
            "(call function: (identifier) @callee)
             (call function: (attribute attribute: (identifier) @callee))"
        }
        "go" => {
            "(call_expression function: (identifier) @callee)
             (call_expression function: (selector_expression field: (field_identifier) @callee))"
        }
        "javascript" | "typescript" => {
            "(call_expression function: (identifier) @callee)
             (call_expression function: (member_expression property: (property_identifier) @callee))"
        }
        "java" => {
            "(method_invocation name: (identifier) @callee)
             (object_creation_expression type: (type_identifier) @callee)"
        }
        "c" | "cpp" => {
            "(call_expression function: (identifier) @callee)
             (call_expression function: (field_expression field: (field_identifier) @callee))"
        }
        "ruby" => {
            "(call method: (identifier) @callee)"
        }
        "csharp" => {
            "(invocation_expression function: (identifier_name) @callee)
             (invocation_expression function: (member_access_expression name: (identifier_name) @callee))"
        }
        "bash" => {
            "(command name: (command_name (word) @callee))"
        }
        "scala" => {
            "(call_expression function: (identifier) @callee)
             (call_expression function: (field_expression field: (identifier) @callee))"
        }
        _ => "",
    }
}

// ── Core extraction ───────────────────────────────────────────────────────────

pub fn extract_context(
    file_diff: &FileDiff,
    source_root: &Path,
    raw_diff: &str,
) -> Result<FileAstContext> {
    let path = match file_diff.source_path() {
        Some(p) => p,
        None => {
            return Ok(FileAstContext {
                file: "<deleted>".into(),
                changed_symbols: vec![],
                all_symbols: vec![],
                call_edges: vec![],
                raw_diff: raw_diff.to_owned(),
            });
        }
    };

    let file_name = path.display().to_string();

    let (lang, lang_name) = match detect_language(path) {
        Some(l) => l,
        None => {
            return Ok(FileAstContext {
                file: file_name,
                changed_symbols: vec![],
                all_symbols: vec![],
                call_edges: vec![],
                raw_diff: raw_diff.to_owned(),
            });
        }
    };

    let abs_path = source_root.join(path);
    let source = match std::fs::read_to_string(&abs_path) {
        Ok(s) => s,
        Err(_) => {
            return Ok(FileAstContext {
                file: file_name,
                changed_symbols: vec![],
                all_symbols: vec![],
                call_edges: vec![],
                raw_diff: raw_diff.to_owned(),
            });
        }
    };

    let mut parser = Parser::new();
    parser
        .set_language(&lang)
        .context("Failed to set tree-sitter language")?;

    let tree = parser
        .parse(source.as_bytes(), None)
        .context("tree-sitter parse returned None")?;

    let root = tree.root_node();
    let all_symbols = collect_symbols(root, &source, &lang, lang_name)?;
    let changed_symbols = symbols_overlapping_hunks(&all_symbols, &file_diff.hunks);
    let call_edges = collect_call_edges(&changed_symbols, root, &source, &lang, lang_name)?;

    Ok(FileAstContext {
        file: file_name,
        changed_symbols,
        all_symbols,
        call_edges,
        raw_diff: raw_diff.to_owned(),
    })
}

// ── Symbol collection ─────────────────────────────────────────────────────────

fn collect_symbols(
    root: Node<'_>,
    source: &str,
    lang: &Language,
    lang_name: &str,
) -> Result<Vec<Symbol>> {
    let query_src = symbol_query_source(lang_name);
    if query_src.is_empty() {
        return Ok(vec![]);
    }

    let query = Query::new(lang, query_src).context("Failed to compile symbol query")?;
    let item_idx = query.capture_index_for_name("item");
    let name_idx = query.capture_index_for_name("name");

    let mut symbols: Vec<Symbol> = Vec::new();
    let mut seen: HashSet<(usize, usize)> = HashSet::new();

    let source_bytes = source.as_bytes();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(&query, root, source_bytes);

    // tree-sitter 0.24 uses streaming_iterator::StreamingIterator
    while let Some(m) = matches.next() {
        let item_node = m
            .captures
            .iter()
            .find(|c| Some(c.index) == item_idx)
            .map(|c| c.node);
        let name_node = m
            .captures
            .iter()
            .find(|c| Some(c.index) == name_idx)
            .map(|c| c.node);

        let item_node = match item_node {
            Some(n) => n,
            None => continue,
        };

        let range = (item_node.start_byte(), item_node.end_byte());
        if !seen.insert(range) {
            continue;
        }

        let name: String = name_node
            .and_then(|n| n.utf8_text(source_bytes).ok())
            .unwrap_or("<anonymous>")
            .to_owned();

        let kind = match item_node.kind() {
            // Rust
            "function_item"                                     => SymbolKind::Function,
            "impl_item"                                         => SymbolKind::ImplBlock,
            "struct_item"                                       => SymbolKind::Struct,
            "enum_item"                                         => SymbolKind::Enum,
            "trait_item"                                        => SymbolKind::Trait,
            // functions (multi-language)
            "function_definition"
            | "function_declaration"
            | "method_declaration"
            | "method_definition"
            | "method"
            | "singleton_method"                                => SymbolKind::Function,
            // structs
            "struct_specifier" | "struct_declaration"           => SymbolKind::Struct,
            // enums
            "enum_specifier" | "enum_declaration"               => SymbolKind::Enum,
            // traits / interfaces
            "trait_definition" | "interface_declaration"        => SymbolKind::Trait,
            // classes
            "class_definition"
            | "class_declaration"
            | "class_specifier"
            | "class"                                           => SymbolKind::Other("class".into()),
            // other named constructs
            "module"                                            => SymbolKind::Other("module".into()),
            "namespace_definition"                              => SymbolKind::Other("namespace".into()),
            "type_declaration" | "type_alias_declaration"       => SymbolKind::Other("type".into()),
            "object_definition"                                 => SymbolKind::Other("object".into()),
            other                                               => SymbolKind::Other(other.into()),
        };

        let node_src: String = item_node
            .utf8_text(source_bytes)
            .unwrap_or("<encoding error>")
            .to_owned();

        symbols.push(Symbol {
            name,
            kind,
            start_line: item_node.start_position().row as u32 + 1,
            end_line: item_node.end_position().row as u32 + 1,
            source: node_src,
        });
    }

    Ok(symbols)
}

// ── Hunk-to-symbol mapping ────────────────────────────────────────────────────

fn symbols_overlapping_hunks(symbols: &[Symbol], hunks: &[HunkRange]) -> Vec<Symbol> {
    symbols
        .iter()
        .filter(|sym| {
            hunks.iter().any(|h| {
                sym.start_line <= h.end_line && sym.end_line >= h.start_line
            })
        })
        .cloned()
        .collect()
}

// ── Call graph ────────────────────────────────────────────────────────────────

fn collect_call_edges(
    changed_symbols: &[Symbol],
    root: Node<'_>,
    source: &str,
    lang: &Language,
    lang_name: &str,
) -> Result<Vec<CallEdge>> {
    let query_src = call_query_source(lang_name);
    if query_src.is_empty() || changed_symbols.is_empty() {
        return Ok(vec![]);
    }

    let query = Query::new(lang, query_src).context("Failed to compile call query")?;
    let callee_idx = match query.capture_index_for_name("callee") {
        Some(i) => i,
        None => return Ok(vec![]),
    };

    let source_bytes = source.as_bytes();
    let mut edges: Vec<CallEdge> = Vec::new();
    let mut seen: HashSet<(String, String)> = HashSet::new();

    for sym in changed_symbols {
        let sym_node = find_node_at_lines(root, sym.start_line, sym.end_line);
        let node_to_query = sym_node.unwrap_or(root);

        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(&query, node_to_query, source_bytes);

        while let Some(m) = matches.next() {
            for cap in m.captures.iter().filter(|c| c.index == callee_idx) {
                if let Ok(callee_str) = cap.node.utf8_text(source_bytes) {
                    let callee: String = callee_str.to_string();
                    let key = (sym.name.clone(), callee.clone());
                    if seen.insert(key) {
                        edges.push(CallEdge {
                            caller: sym.name.clone(),
                            callee,
                        });
                    }
                }
            }
        }
    }

    Ok(edges)
}

// ── Utility ───────────────────────────────────────────────────────────────────

fn find_node_at_lines<'a>(root: Node<'a>, start_line: u32, end_line: u32) -> Option<Node<'a>> {
    let start_row = (start_line - 1) as usize;
    let end_row = (end_line - 1) as usize;

    let mut best: Option<Node<'a>> = None;
    let mut stack = vec![root];

    while let Some(node) = stack.pop() {
        let ns = node.start_position().row;
        let ne = node.end_position().row;

        if ns <= start_row && ne >= end_row {
            let is_smaller = best.is_none_or(|b: Node<'_>| {
                (ne - ns) < (b.end_position().row - b.start_position().row)
            });
            if is_smaller {
                best = Some(node);
            }
            let mut tree_cursor = node.walk();
            for child in node.children(&mut tree_cursor) {
                stack.push(child);
            }
        }
    }

    best
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::HunkRange;

    const RUST_SOURCE: &str = r#"
fn add(a: i32, b: i32) -> i32 {
    a + b
}

fn multiply(a: i32, b: i32) -> i32 {
    let result = add(a, b);
    result * 2
}

struct Calculator {
    value: i32,
}
"#;

    #[test]
    fn test_collect_symbols() {
        let lang: Language = tree_sitter_rust::LANGUAGE.into();
        let mut parser = Parser::new();
        parser.set_language(&lang).unwrap();
        let tree = parser.parse(RUST_SOURCE.as_bytes(), None).unwrap();
        let symbols =
            collect_symbols(tree.root_node(), RUST_SOURCE, &lang, "rust").unwrap();
        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"add"));
        assert!(names.contains(&"multiply"));
        assert!(names.contains(&"Calculator"));
    }

    #[test]
    fn test_symbols_overlapping_hunks() {
        let lang: Language = tree_sitter_rust::LANGUAGE.into();
        let mut parser = Parser::new();
        parser.set_language(&lang).unwrap();
        let tree = parser.parse(RUST_SOURCE.as_bytes(), None).unwrap();
        let all = collect_symbols(tree.root_node(), RUST_SOURCE, &lang, "rust").unwrap();

        let hunks = vec![HunkRange {
            start_line: 6,
            end_line: 9,
        }];
        let changed = symbols_overlapping_hunks(&all, &hunks);
        assert!(changed.iter().any(|s| s.name == "multiply"));
        assert!(!changed.iter().any(|s| s.name == "add"));
    }
}
