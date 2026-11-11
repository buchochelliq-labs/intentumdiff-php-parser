//! PHP parser plugin — full-parse mode.
//!
//! Handles `.php`, `.phtml` files.  The host parses source with
//! tree-sitter-php and sends the CST as JSON.

use intentumdiff_plugin_sdk::{
    cst::CstNode,
    hash::structural_hash_with_memo,
    tree::{SemanticNode, SemanticNodeBuilder},
};

wit_bindgen::generate!({
    path: "wit/plugin.wit",
    world: "parser-plugin",
});

use crate::exports::intentdiff::plugin::parser::ExamplePair;
use crate::exports::intentdiff::plugin::parser::Guest;
use crate::exports::intentdiff::plugin::parser::LanguageInfoRecord;
use crate::exports::intentdiff::plugin::parser::ParserMode;

const PLUGIN_METADATA: &str = include_str!("../plugin_metadata.info");

fn language_info_for(ids: Vec<String>) -> Vec<LanguageInfoRecord> {
    let metadata = intentumdiff_plugin_sdk::metadata::parse_plugin_metadata(PLUGIN_METADATA);
    ids.into_iter()
        .map(|language_id| {
            let info = metadata.language_or_default(&language_id);
            LanguageInfoRecord {
                language_id: info.language_id,
                language_name: info.language_name,
                language_short_name: info.language_short_name,
                monaco_language: info.monaco_language,
                default_filename: info.default_filename,
                language_file_extensions: info.language_file_extensions,
                author: metadata.author().to_string(),
                plugin_version: metadata.plugin_version().to_string(),
                last_updated: metadata.last_updated().to_string(),
            }
        })
        .collect()
}
struct PhpParser;

const TRIVIA: &[&str] = &["comment", "line_comment", "block_comment", "whitespace"];

const SEMANTIC_TYPES: &[&str] = &[
    // Root
    "program",
    // Declarations
    "class_declaration",
    "interface_declaration",
    "trait_declaration",
    "enum_declaration",
    // Members
    "method_declaration",
    "function_definition",
    "property_declaration",
    "const_declaration",
    "enum_case",
    // Function/method signature structure. These must remain semantic so
    // adding parameter or return types cannot normalize to the same tree.
    "formal_parameters",
    "simple_parameter",
    "variadic_parameter",
    "primitive_type",
    "named_type",
    "union_type",
    "intersection_type",
    "nullable_type",
    // Namespace / use
    "namespace_definition",
    "namespace_use_declaration",
    "use_declaration",
    // Statements
    "expression_statement",
    "echo_statement",
    "return_statement",
    "throw_expression",
    "if_statement",
    "else_clause",
    "elseif_clause",
    "for_statement",
    "foreach_statement",
    "while_statement",
    "do_statement",
    "try_statement",
    "catch_clause",
    "finally_clause",
    "switch_statement",
    "switch_block",
    "match_expression",
    "break_statement",
    "continue_statement",
    "global_declaration",
    "static_variable_declaration",
    // Expressions
    "assignment_expression",
    "augmented_assignment_expression",
    "function_call_expression",
    "member_call_expression",
    "object_creation_expression",
    "arrow_function",
    "anonymous_function_creation_expression",
    "match_arm",
    // Identifiers / literals
    "name",
    "variable_name",
    "qualified_name",
    "string",
    // Double-quoted / heredoc / nowdoc strings are distinct tree-sitter-php
    // kinds; without them an echoed "café" -> "cafè" edit vanished (#7).
    "encapsed_string",
    "heredoc",
    "nowdoc",
    "integer",
    "boolean",
    "null",
    // Attributes
    "attribute_list",
    "attribute",
];

fn is_semantic(node_type: &str) -> bool {
    SEMANTIC_TYPES.contains(&node_type)
}

fn label_for(node: &CstNode) -> String {
    if node.is_leaf() {
        return node.text_or_empty().to_string();
    }
    // Literal containers label with their captured source text (SDK-shared, issue #47).
    if let Some(label) = intentumdiff_plugin_sdk::ts_convert::literal_label(node) {
        return label;
    }
    match node.node_type.as_str() {
        "class_declaration"
        | "interface_declaration"
        | "trait_declaration"
        | "enum_declaration"
        | "namespace_definition" => {
            for child in &node.children {
                if child.node_type == "name" || child.node_type == "qualified_name" {
                    return child.text_or_empty().to_string();
                }
            }
        }
        "method_declaration" | "function_definition" => {
            for child in &node.children {
                if child.node_type == "name" {
                    return child.text_or_empty().to_string();
                }
            }
        }
        "attribute" => {
            for child in &node.children {
                if child.node_type == "name" || child.node_type == "qualified_name" {
                    return child.text_or_empty().to_string();
                }
            }
        }
        "namespace_use_declaration" | "use_declaration" => {
            for child in &node.children {
                if child.node_type == "name" || child.node_type == "qualified_name" {
                    return child.text_or_empty().to_string();
                }
            }
        }
        _ => {}
    }
    for child in &node.children {
        if child.node_type == "name" || child.node_type == "variable_name" {
            return child.text_or_empty().to_string();
        }
    }
    node.node_type.clone()
}

fn is_class_like(node_type: &str) -> bool {
    matches!(
        node_type,
        "class_declaration" | "interface_declaration" | "trait_declaration" | "enum_declaration"
    )
}

fn is_method_like(node_type: &str) -> bool {
    matches!(
        node_type,
        "method_declaration"
            | "function_definition"
            | "arrow_function"
            | "anonymous_function_creation_expression"
    )
}

/// Byte offset of the start of every line of a source, computed once per parse
/// so that slicing each node's source text stays linear in the file size (#9).
struct LineIndex<'a> {
    source: &'a str,
    starts: Vec<usize>,
}

impl<'a> LineIndex<'a> {
    fn new(source: &'a str) -> Self {
        let starts = std::iter::once(0)
            .chain(source.match_indices('\n').map(|(i, _)| i + 1))
            .collect();
        Self { source, starts }
    }

    /// Absolute byte offset of a 0-based row and UTF-8 byte column, or `None`
    /// if it lies outside the source or past the end of its line.
    fn offset(&self, row: u32, col: u32) -> Option<usize> {
        let line_start = *self.starts.get(row as usize)?;
        let line_end = self
            .starts
            .get(row as usize + 1)
            .map_or(self.source.len(), |next| next - 1);
        let absolute = line_start.checked_add(col as usize)?;
        (absolute <= line_end).then_some(absolute)
    }

    /// Read a CST node's original UTF-8 source slice from its Tree-sitter
    /// coordinates. A typed PHP parameter is one semantic signature, not merely
    /// a variable name: dropping the type from its label can hide a
    /// source-anchored parent modification behind individual primitive_type
    /// insertions (#5).
    fn node_text(&self, node: &CstNode) -> Option<&'a str> {
        let start = self.offset(node.start_line, node.start_col)?;
        let end = self.offset(node.end_line, node.end_col)?;
        self.source.get(start..end)
    }
}

fn convert(
    node: &CstNode,
    id_prefix: &str,
    parent_class: Option<&str>,
    memo: &mut std::collections::HashMap<usize, String>,
    source: &str,
) -> Option<SemanticNode> {
    // The parser already knows each node's exact source byte range. Preserve
    // the full typed-parameter signature for matching and presentation while
    // keeping granular primitive_type children for precise type edits.
    let lines = LineIndex::new(source);
    let label_with_signature = |candidate: &CstNode| {
        // String literals label with their complete source payload: the SDK's
        // literal_label truncates at 120 chars, which would hide edits past it (#7).
        if matches!(
            candidate.node_type.as_str(),
            "simple_parameter" | "string" | "encapsed_string" | "heredoc" | "nowdoc"
        ) {
            if let Some(text) = lines.node_text(candidate) {
                if !text.is_empty() {
                    return text.to_string();
                }
            }
        }
        label_for(candidate)
    };
    convert_semantic_classed(
        node,
        id_prefix,
        parent_class,
        memo,
        &|_| false,
        &is_semantic,
        &is_class_like,
        &is_method_like,
        &label_with_signature,
    )
}

use intentumdiff_plugin_sdk::ts_convert::{convert_semantic_classed, node_to_cst};

fn parse_source(source: &str) -> Result<CstNode, String> {
    let mut parser = tree_sitter::Parser::new();
    let lang = tree_sitter_php::LANGUAGE_PHP.into();
    parser
        .set_language(&lang)
        .map_err(|_| "Failed to load php grammar".to_string())?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| "Parse failed".to_string())?;
    Ok(node_to_cst(tree.root_node(), source.as_bytes()))
}

fn process_impl(source: &str) -> String {
    let root: CstNode = match parse_source(source) {
        Ok(n) => n,
        Err(e) => return format!(r#"{{\"error\":\"{}\"}}"#, e),
    };
    let mut memo: std::collections::HashMap<usize, String> = std::collections::HashMap::new();
    let sem = match convert(&root, "0", None, &mut memo, source) {
        Some(n) => n,
        None => return r#"{"error":"Empty semantic tree"}"#.to_string(),
    };
    match serde_json::to_string(&sem) {
        Ok(s) => s,
        Err(e) => format!(r#"{{"error":"Serialisation error: {}"}}"#, e),
    }
}

impl Guest for PhpParser {
    fn get_parser_mode() -> ParserMode {
        ParserMode::FullParse
    }
    fn grammar_id() -> String {
        "php".to_string()
    }
    fn detect_language(filename: String, _content: String) -> String {
        let lower = filename.to_lowercase();
        if lower.ends_with(".php") || lower.ends_with(".phtml") || lower.ends_with(".php8") {
            "php".to_string()
        } else {
            String::new()
        }
    }
    fn preprocess_source(source: String) -> String {
        source
    }
    fn example(_language: String) -> ExamplePair {
        ExamplePair {
            old: "<?php\nfunction greet($name) {\n    echo \"Hello, \" . $name . \"!\\n\";\n}\n\nfunction add($a, $b) {\n    return $a + $b;\n}\n".to_string(),
            new: "<?php\nfunction greet(string $name): void {\n    echo \"Hello, {$name}!\\n\";\n}\n\nfunction add(int $a, int $b): int {\n    return $a + $b;\n}\n\nfunction multiply(int $a, int $b): int {\n    return $a * $b;\n}\n".to_string(),
        }
    }
    fn process(input: String, _language: String, _filename: String) -> String {
        process_impl(&input)
    }
    fn trivia_node_types() -> Vec<String> {
        TRIVIA.iter().map(|s| s.to_string()).collect()
    }
    fn language_ids() -> Vec<String> {
        vec!["php".to_string()]
    }
    fn language_info() -> Vec<LanguageInfoRecord> {
        language_info_for(Self::language_ids())
    }
    fn priority() -> i32 {
        0
    }
}

export!(PhpParser);

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exports::intentdiff::plugin::parser::Guest;
    use intentumdiff_plugin_sdk::testing as t;

    #[test]
    fn grammar_id_nonempty() {
        assert!(!PhpParser::grammar_id().is_empty());
    }

    #[test]
    fn language_ids_contain_grammar_id() {
        let gid = PhpParser::grammar_id();
        let ids = PhpParser::language_ids();
        assert!(
            ids.contains(&gid),
            "language_ids {:?} must contain {:?}",
            ids,
            gid
        );
    }

    #[test]
    fn detect_language_known_ext() {
        let r = PhpParser::detect_language("test.php".to_string(), "".to_string());
        assert_eq!(r.as_str(), "php");
    }

    #[test]
    fn detect_language_unknown_ext() {
        let r =
            PhpParser::detect_language("test.xyz_notareal_ext_9z8y".to_string(), "".to_string());
        assert_eq!(r.as_str(), "");
    }

    #[test]
    fn parser_mode_is_full_parse() {
        assert!(matches!(
            PhpParser::get_parser_mode(),
            ParserMode::FullParse
        ));
    }

    #[test]
    fn process_impl_accepts_raw_example_source() {
        let example = PhpParser::example(PhpParser::grammar_id());
        let out = process_impl(&example.old);
        t::assert_valid_json(&out, "process(raw example)");
        assert!(!out.contains("\"error\""), "{out}");
    }
    #[test]
    fn parameter_and_return_type_changes_are_semantic() {
        let before = "<?php\nfunction add($a, $b) {\n    return $a + $b;\n}\n";
        let after = "<?php\nfunction add(int $a, int $b): int {\n    return $a + $b;\n}\n";

        let before_tree = process_impl(before);
        let after_tree = process_impl(after);

        t::assert_valid_json(&before_tree, "before signature");
        t::assert_valid_json(&after_tree, "after signature");
        assert_ne!(
            before_tree, after_tree,
            "typed signature must change the semantic tree"
        );
        assert!(
            after_tree.contains("primitive_type") && after_tree.contains("int"),
            "typed signature must retain its type evidence: {after_tree}"
        );
    }

    fn collect_nodes_of_type<'a>(
        tree: &'a serde_json::Value,
        node_type: &str,
        out: &mut Vec<&'a serde_json::Value>,
    ) {
        if tree["node_type"] == node_type {
            out.push(tree);
        }
        if let Some(children) = tree["children"].as_array() {
            for child in children {
                collect_nodes_of_type(child, node_type, out);
            }
        }
    }

    #[test]
    fn typed_php_parameter_label_and_span_include_complete_declaration() {
        let before = "<?php\nfunction add($a, $b) {\n    return $a + $b;\n}\n";
        let after = "<?php\nfunction add(int $a, int $b): int {\n    return $a + $b;\n}\n";
        let old_tree: serde_json::Value = serde_json::from_str(&process_impl(before)).unwrap();
        let new_tree: serde_json::Value = serde_json::from_str(&process_impl(after)).unwrap();
        let mut old_params = Vec::new();
        let mut new_params = Vec::new();
        collect_nodes_of_type(&old_tree, "simple_parameter", &mut old_params);
        collect_nodes_of_type(&new_tree, "simple_parameter", &mut new_params);
        assert_eq!(old_params.len(), 2, "untyped parameters remain structured");
        assert_eq!(new_params.len(), 2, "typed parameters remain structured");
        assert_eq!(old_params[0]["label"], "$a");
        assert_eq!(new_params[0]["label"], "int $a");
        assert_eq!(new_params[1]["label"], "int $b");
        assert_ne!(
            old_params[0]["structural_hash"], new_params[0]["structural_hash"],
            "type addition must alter parameter semantics"
        );
        assert_eq!(new_params[0]["position"]["start_line"], 1);
        assert_eq!(new_params[0]["position"]["end_line"], 1);
        assert_eq!(new_params[0]["position"]["start_col"], 13);
        assert_eq!(new_params[0]["position"]["end_col"], 19);
        assert_eq!(new_params[1]["position"]["start_col"], 21);
        assert_eq!(new_params[1]["position"]["end_col"], 27);
        assert!(
            new_tree.to_string().contains("primitive_type"),
            "granular type evidence must not disappear"
        );
    }

    fn first_node_of_type(tree: &serde_json::Value, node_type: &str) -> serde_json::Value {
        let mut found = Vec::new();
        collect_nodes_of_type(tree, node_type, &mut found);
        assert!(!found.is_empty(), "no {node_type} in {tree}");
        found[0].clone()
    }

    #[test]
    fn echoed_double_quoted_unicode_string_edit_is_retained() {
        // Exact pair from dynamic/php/unicode-value (#7).
        let before = "<?php echo \"caf\u{e9}\";\n";
        let after = "<?php echo \"caf\u{e8}\";\n";
        let old_tree: serde_json::Value = serde_json::from_str(&process_impl(before)).unwrap();
        let new_tree: serde_json::Value = serde_json::from_str(&process_impl(after)).unwrap();
        let old_s = first_node_of_type(&old_tree, "encapsed_string");
        let new_s = first_node_of_type(&new_tree, "encapsed_string");
        assert_eq!(old_s["label"], "\"caf\u{e9}\"");
        assert_eq!(new_s["label"], "\"caf\u{e8}\"");
        assert_ne!(old_s["structural_hash"], new_s["structural_hash"]);
        // 0-based row, UTF-8 byte columns: `"café"` is 7 bytes starting at col 11.
        assert_eq!(new_s["position"]["start_line"], 0);
        assert_eq!(new_s["position"]["start_col"], 11);
        assert_eq!(new_s["position"]["end_col"], 18);
        let echo_old = first_node_of_type(&old_tree, "echo_statement");
        let echo_new = first_node_of_type(&new_tree, "echo_statement");
        assert_ne!(echo_old["structural_hash"], echo_new["structural_hash"]);
    }

    #[test]
    fn identical_and_spacing_only_echo_strings_are_preserved() {
        let src = "<?php echo \"caf\u{e9}\";\n";
        assert_eq!(process_impl(src), process_impl(src), "deterministic");
        let spaced = "<?php echo   \"caf\u{e9}\" ;\n";
        let a: serde_json::Value = serde_json::from_str(&process_impl(src)).unwrap();
        let b: serde_json::Value = serde_json::from_str(&process_impl(spaced)).unwrap();
        let sa = first_node_of_type(&a, "encapsed_string");
        let sb = first_node_of_type(&b, "encapsed_string");
        assert_eq!(sa["label"], sb["label"]);
        assert_eq!(sa["structural_hash"], sb["structural_hash"]);
        assert_eq!(a["structural_hash"], b["structural_hash"]);
    }

    #[test]
    fn long_string_payload_is_not_truncated() {
        let long = "x".repeat(200);
        let before = format!("<?php echo \"{long}a\";\n");
        let after = format!("<?php echo \"{long}b\";\n");
        let old_tree: serde_json::Value = serde_json::from_str(&process_impl(&before)).unwrap();
        let new_tree: serde_json::Value = serde_json::from_str(&process_impl(&after)).unwrap();
        let old_s = first_node_of_type(&old_tree, "encapsed_string");
        let new_s = first_node_of_type(&new_tree, "encapsed_string");
        assert_eq!(old_s["label"].as_str().unwrap(), format!("\"{long}a\""));
        assert_ne!(old_s["label"], new_s["label"]);
    }

    #[test]
    fn heredoc_and_single_quoted_strings_keep_payload() {
        let single: serde_json::Value =
            serde_json::from_str(&process_impl("<?php echo 'caf\u{e9}';\n")).unwrap();
        assert_eq!(
            first_node_of_type(&single, "string")["label"],
            "'caf\u{e9}'"
        );
        let heredoc: serde_json::Value =
            serde_json::from_str(&process_impl("<?php echo <<<EOT\ncaf\u{e9}\nEOT;\n")).unwrap();
        let h = first_node_of_type(&heredoc, "heredoc");
        assert!(h["label"].as_str().unwrap().contains("caf\u{e9}"), "{h}");
    }

    fn string_heavy_source(lines: usize) -> String {
        let mut src = String::from("<?php\n");
        for i in 0..lines {
            src.push_str(&format!("echo \"x{i}\";\n"));
        }
        src
    }

    #[test]
    fn string_heavy_file_parses_in_linear_time() {
        // Issue #9: locating each string's source slice rescanned the file
        // from the start, so 20,000 one-string lines took seconds.
        let src = string_heavy_source(20_000);
        let started = std::time::Instant::now();
        let out = process_impl(&src);
        let elapsed = started.elapsed();
        // Generous bound for unoptimized test builds; the quadratic scan
        // alone needed ~28 s in a test build (~6 s optimized).
        assert!(elapsed.as_secs_f64() < 10.0, "took {elapsed:?}");
        let tree: serde_json::Value = serde_json::from_str(&out).unwrap();
        let mut strings = Vec::new();
        collect_nodes_of_type(&tree, "encapsed_string", &mut strings);
        assert_eq!(strings.len(), 20_000);
        for (i, s) in strings.iter().enumerate() {
            assert_eq!(s["label"].as_str().unwrap(), format!("\"x{i}\""));
            assert_eq!(s["position"]["start_line"], i as u64 + 1);
            assert_eq!(s["position"]["start_col"], 5);
        }
    }

    #[test]
    fn process_impl_empty_returns_valid_json() {
        let out = process_impl("");
        t::assert_valid_json(&out, "process(empty)");
    }

    #[test]
    fn process_impl_whitespace_returns_valid_json() {
        let out = process_impl("   \n  ");
        t::assert_valid_json(&out, "process(whitespace)");
    }
}
