//! DAX / Power Query M parser plugin — full-parse mode.
//!
//! Handles `.dax` files (DAX — Data Analysis Expressions for Power BI / SSAS)
//! and `.m` files (Power Query M language).  No tree-sitter grammar exists for
//! either language; DAX uses case-insensitive line scanning and M a small
//! tokenizer with bracket/let-depth tracking.
//!
//! DAX semantic nodes:
//!   dax_file          — root
//!   measure           — DEFINE MEASURE 'Table'[Name] = … (label = Table[Name])
//!   table_definition  — DEFINE TABLE <name> = …
//!   column_definition — DEFINE COLUMN 'Table'[Name] = …
//!   var_definition    — DEFINE VAR <name> = …
//!   evaluate          — EVALUATE block (label = query_N)
//!
//! Power Query M semantic nodes: see `m.rs` (token-based parser covering
//! inline, multi-line and section-document forms).
//!
//! Detection:
//!   .dax → "dax" unconditionally.
//!   .m   → content-based: DAX keywords → "dax"; let/in structure → "m".

mod m;

use intentumdiff_plugin_sdk::tree::{SemanticNode, SemanticNodeBuilder};

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
struct DaxParser;

// ---------------------------------------------------------------------------
// Node helpers
// ---------------------------------------------------------------------------

/// Preserve the exact byte-column origin of an M expression.
fn leaf_at(id: &str, node_type: &str, label: &str, line: u32, start_col: u32) -> SemanticNode {
    let mut end_line = line;
    let mut end_col = start_col;
    for (i, part) in label.split('\n').enumerate() {
        if i > 0 {
            end_line += 1;
            end_col = 0;
        }
        end_col += part.len() as u32;
    }
    SemanticNodeBuilder::new(
        id, node_type, label, line, start_col, end_line, end_col, String::new(),
    )
    .build()
}

fn block_node(
    id: &str,
    node_type: &str,
    label: &str,
    start: u32,
    end: u32,
    children: Vec<SemanticNode>,
) -> SemanticNode {
    SemanticNodeBuilder::new(id, node_type, label, start, 0, end, 0, String::new())
        .children(children)
        .build()
}

// ---------------------------------------------------------------------------
// DAX name extraction
// ---------------------------------------------------------------------------

/// Extract the measure/column name from the text after the MEASURE/COLUMN keyword.
/// Input example: `'Sales'[Total Revenue] =` → `Sales[Total Revenue]`
fn extract_bracket_name(rest: &str) -> String {
    if let (Some(b_start), Some(b_end)) = (rest.find('['), rest.find(']')) {
        if b_end > b_start {
            let name = &rest[b_start + 1..b_end];
            let table = rest[..b_start].trim().trim_matches('\'');
            if table.is_empty() {
                return name.to_string();
            }
            return format!("{}[{}]", table, name);
        }
    }
    // Fallback: first whitespace-delimited token
    rest.split_whitespace()
        .next()
        .unwrap_or("(anonymous)")
        .to_string()
}

// ---------------------------------------------------------------------------
// Language detection
// ---------------------------------------------------------------------------

/// First word of `content` after whitespace and `//` / `/* */` comments.
fn m_leading_word(content: &str) -> &str {
    let mut rest = content;
    loop {
        rest = rest.trim_start();
        if let Some(r) = rest.strip_prefix("//") {
            rest = r.find('\n').map_or("", |i| &r[i..]);
        } else if let Some(r) = rest.strip_prefix("/*") {
            rest = r.find("*/").map_or("", |i| &r[i + 2..]);
        } else {
            break;
        }
    }
    let end = rest
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(rest.len());
    &rest[..end]
}

pub(crate) fn detect_language_impl(filename: &str, content: &str) -> String {
    let lower = filename.to_lowercase();
    if lower.ends_with(".dax") {
        return "dax".to_string();
    }
    if lower.ends_with(".m") {
        // Content-based: DAX indicators take priority
        let upper = content.to_uppercase();
        if upper.contains("DEFINE MEASURE")
            || upper.contains("EVALUATE\n")
            || upper.contains("EVALUATE\r\n")
            || (upper.contains("EVALUATE") && upper.contains("DEFINE"))
        {
            return "dax".to_string();
        }
        // M (Power Query): let…in structure
        let cl = content.to_lowercase();
        let lead = m_leading_word(content);
        if lead == "let" || lead == "section" || cl.contains("\nlet\n")
            || cl.contains("\nlet\r\n")
            || cl.contains("\r\nlet\r\n")
            || cl.starts_with("let\n")
            || cl.starts_with("let\r\n")
        {
            return "m".to_string();
        }
    }
    String::new()
}

// ---------------------------------------------------------------------------
// DAX parser
// ---------------------------------------------------------------------------

struct DaxDefinition {
    node_type: &'static str,
    expression_type: &'static str,
    label: String,
    start: u32,
    end: u32,
    expression_start: u32,
    expression_lines: Vec<String>,
}

fn expression_after_equals(text: &str) -> String {
    text.find('=')
        .map(|idx| text[idx + 1..].trim().to_string())
        .unwrap_or_default()
}

fn name_before_equals(rest: &str) -> String {
    let before_equals = rest.split('=').next().unwrap_or(rest).trim();
    before_equals
        .split_whitespace()
        .next()
        .unwrap_or("(anonymous)")
        .trim_matches('\'')
        .to_string()
}

fn start_dax_definition(
    trimmed: &str,
    upper: &str,
    lineno: u32,
    eval_counter: &mut usize,
) -> Option<DaxDefinition> {
    let words: Vec<&str> = upper.split_whitespace().collect();
    if words.is_empty() {
        return None;
    }

    let (kw, rest_skip) = if words[0] == "DEFINE" && words.len() > 1 {
        (words[1], 2usize)
    } else {
        (words[0], 1usize)
    };

    match kw {
        "MEASURE" => {
            let kw_pos = upper.find("MEASURE").map(|p| p + 7).unwrap_or(upper.len());
            let rest = trimmed[kw_pos..].trim();
            let label = extract_bracket_name(rest);
            let expression = expression_after_equals(rest);
            Some(DaxDefinition {
                node_type: "measure",
                expression_type: "measure_expression",
                label,
                start: lineno,
                end: lineno,
                expression_start: lineno,
                expression_lines: if expression.is_empty() {
                    vec![]
                } else {
                    vec![expression]
                },
            })
        }
        "TABLE" if words.len() > rest_skip => {
            let kw_pos = upper.find("TABLE").map(|p| p + 5).unwrap_or(upper.len());
            let rest = trimmed[kw_pos..].trim();
            let expression = expression_after_equals(rest);
            Some(DaxDefinition {
                node_type: "table_definition",
                expression_type: "table_expression",
                label: name_before_equals(rest),
                start: lineno,
                end: lineno,
                expression_start: lineno,
                expression_lines: if expression.is_empty() {
                    vec![]
                } else {
                    vec![expression]
                },
            })
        }
        "COLUMN" => {
            let kw_pos = upper.find("COLUMN").map(|p| p + 6).unwrap_or(upper.len());
            let rest = trimmed[kw_pos..].trim();
            let label = extract_bracket_name(rest);
            let expression = expression_after_equals(rest);
            Some(DaxDefinition {
                node_type: "column_definition",
                expression_type: "column_expression",
                label,
                start: lineno,
                end: lineno,
                expression_start: lineno,
                expression_lines: if expression.is_empty() {
                    vec![]
                } else {
                    vec![expression]
                },
            })
        }
        "VAR" if words.len() > rest_skip => {
            let kw_pos = upper.find("VAR").map(|p| p + 3).unwrap_or(upper.len());
            let rest = trimmed[kw_pos..].trim();
            let expression = expression_after_equals(rest);
            Some(DaxDefinition {
                node_type: "var_definition",
                expression_type: "var_expression",
                label: name_before_equals(rest),
                start: lineno,
                end: lineno,
                expression_start: lineno,
                expression_lines: if expression.is_empty() {
                    vec![]
                } else {
                    vec![expression]
                },
            })
        }
        "EVALUATE" => {
            let kw_pos = upper.find("EVALUATE").map(|p| p + 8).unwrap_or(upper.len());
            let expression = trimmed[kw_pos..].trim().to_string();
            let label = format!("query_{}", eval_counter);
            *eval_counter += 1;
            Some(DaxDefinition {
                node_type: "evaluate",
                expression_type: "query_expression",
                label,
                start: lineno,
                end: lineno,
                expression_start: lineno,
                expression_lines: if expression.is_empty() {
                    vec![]
                } else {
                    vec![expression]
                },
            })
        }
        _ => None,
    }
}

fn push_dax_definition(
    children: &mut Vec<SemanticNode>,
    counter: &mut usize,
    definition: DaxDefinition,
    source: &str,
) {
    let id = format!("0.{}", counter);
    *counter += 1;
    let lines: Vec<&str> = source.lines().collect();
    let declaration_line = lines[definition.start as usize];
    let opening_start = declaration_line.len() - declaration_line.trim_start().len();
    let expression = definition.expression_lines.join("\n").trim().to_string();
    let expression_children = if expression.is_empty() {
        vec![]
    } else {
        // The semantic label is trimmed, but its Rust SourceSpan must start at
        // the real expression bytes, not at the declaration keyword (#9).
        let expression_line = lines[definition.expression_start as usize];
        let first_expression = expression.split('\n').next().unwrap_or("");
        let expression_col = expression_line.find(first_expression).unwrap_or(
            expression_line.len() - expression_line.trim_start().len(),
        );
        let mut expression_node = leaf_at(
            &format!("{}.0", id),
            definition.expression_type,
            &expression,
            definition.expression_start,
            expression_col as u32,
        );
        // Blank and comment lines are skipped, so the label's line count does
        // not locate the last expression byte; the definition end line does.
        expression_node.position.end_line = definition.end;
        expression_node.position.end_col =
            lines[definition.end as usize].trim_end().len() as u32;
        vec![expression_node]
    };
    let mut declaration = block_node(
        &id,
        definition.node_type,
        &definition.label,
        definition.start,
        definition.end,
        expression_children,
    );
    declaration.position.start_col = opening_start as u32;
    // A declaration covers its keyword line through the last expression byte,
    // so the expression child lies inside it (inline measures previously had
    // a vacuous (row,0)-(row,0) extent, #9; multi-line ones ended at col 0).
    declaration.position.end_col = lines[definition.end as usize].trim_end().len() as u32;
    children.push(declaration);
}

pub(crate) fn parse_dax(source: &str) -> String {
    let mut children: Vec<SemanticNode> = Vec::new();
    let mut counter: usize = 0;
    let mut eval_counter: usize = 0;
    let total_lines = source.lines().count().saturating_sub(1) as u32;
    let mut active: Option<DaxDefinition> = None;

    for (idx, line) in source.lines().enumerate() {
        let lineno = idx as u32;
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with("//") || trimmed.starts_with("--") {
            continue;
        }

        let upper = trimmed.to_uppercase();
        let words: Vec<&str> = upper.split_whitespace().collect();
        if words.is_empty() {
            continue;
        }

        if words[0] == "DEFINE" && words.len() == 1 {
            continue;
        }

        if let Some(definition) = start_dax_definition(trimmed, &upper, lineno, &mut eval_counter) {
            if let Some(previous) = active.take() {
                push_dax_definition(&mut children, &mut counter, previous, source);
            }
            active = Some(definition);
            continue;
        }

        if let Some(definition) = active.as_mut() {
            if definition.expression_lines.is_empty() {
                definition.expression_start = lineno;
            }
            definition.expression_lines.push(trimmed.to_string());
            definition.end = lineno;
        }
    }

    if let Some(definition) = active.take() {
        push_dax_definition(&mut children, &mut counter, definition, source);
    }

    let mut root = SemanticNodeBuilder::new(
        "0",
        "dax_file",
        "dax_file",
        0,
        0,
        total_lines,
        0,
        String::new(),
    )
    .children(children)
    .build();
    // The root spans the whole document, ending after its last byte.
    let content = source.trim_end();
    root.position.end_line = content.matches('\n').count() as u32;
    root.position.end_col = (content.len() - content.rfind('\n').map_or(0, |i| i + 1)) as u32;

    match serde_json::to_string(&root) {
        Ok(s) => s,
        Err(e) => format!(r#"{{"error":"Serialisation error: {}"}}"#, e),
    }
}

// Power Query M lives in `m.rs` (token-based; see #11).
use crate::m::parse_m;

// ---------------------------------------------------------------------------
// WIT guest impl
// ---------------------------------------------------------------------------

impl Guest for DaxParser {
    fn get_parser_mode() -> ParserMode {
        ParserMode::FullParse
    }
    fn grammar_id() -> String {
        "dax".to_string()
    }
    fn detect_language(filename: String, content: String) -> String {
        detect_language_impl(&filename, &content)
    }
    fn preprocess_source(source: String) -> String {
        source
    }
    fn example(language: String) -> ExamplePair {
        if language == "m" {
            return ExamplePair {
                old: "let\n    Source = Sql.Database(\"server\", \"sales\"),\n    FilteredRows = Table.SelectRows(Source, each [Amount] > 0)\nin\n    FilteredRows\n".to_string(),
                new: "let\n    Source = Sql.Database(\"server\", \"sales\"),\n    FilteredRows = Table.SelectRows(Source, each [Amount] > 100),\n    RenamedColumns = Table.RenameColumns(FilteredRows, {{\"Amount\", \"OrderAmount\"}})\nin\n    RenamedColumns\n".to_string(),
            };
        }
        ExamplePair {
            old: "DEFINE\nMEASURE Sales[Total Sales] = SUM(Sales[Amount])\nMEASURE Sales[Sales YTD] =\n    TOTALYTD(SUM(Sales[Amount]), Dates[Date])\n\nEVALUATE\nSUMMARIZECOLUMNS(Dates[Year], \"Total\", [Total Sales])\n".to_string(),
            new: "DEFINE\nMEASURE Sales[Total Sales] = SUM(Sales[Amount])\nMEASURE Sales[Sales YTD] =\n    TOTALYTD(SUM(Sales[Amount]), Dates[Date])\nMEASURE Sales[Sales Growth %] =\n    DIVIDE([Total Sales] - CALCULATE([Total Sales], SAMEPERIODLASTYEAR(Dates[Date])),\n           CALCULATE([Total Sales], SAMEPERIODLASTYEAR(Dates[Date])))\n\nEVALUATE\nSUMMARIZECOLUMNS(Dates[Year], \"Total\", [Total Sales], \"Growth\", [Sales Growth %])\n".to_string(),
        }
    }
    fn process(input: String, language: String, _filename: String) -> String {
        if language == "m" {
            parse_m(&input)
        } else {
            parse_dax(&input)
        }
    }
    fn trivia_node_types() -> Vec<String> {
        vec![]
    }
    fn language_ids() -> Vec<String> {
        vec!["dax".to_string(), "m".to_string()]
    }
    fn language_info() -> Vec<LanguageInfoRecord> {
        language_info_for(Self::language_ids())
    }
    fn priority() -> i32 {
        0
    }
}

export!(DaxParser);

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    intentumdiff_plugin_sdk::plugin_compliance_tests! {
        process: parse_dax,
        detect_fn: detect_language_impl,
        detect_cases: [
            ("report.dax", "",                    "dax"),
            ("model.m",    "DEFINE MEASURE 'S'[X] = 1\nEVALUATE 'S'", "dax"),
            ("query.m",    "let\n  Source = 1\nin\n  Source",          "m"),
            ("data.sql",   "",                    ""),
            ("file.txt",   "",                    ""),
        ],
        grammar_id: "dax",
        language_ids: ["dax", "m"],
    }

    const DAX_SAMPLE: &str = "DEFINE\n\
        MEASURE 'Sales'[Total Revenue] = SUM(Sales[Amount])\n\
        MEASURE 'Sales'[Count] = COUNTROWS(Sales)\n\
        TABLE DateTable = CALENDAR(DATE(2020,1,1), DATE(2025,12,31))\n\
        VAR MaxSales = MAX(Sales[Revenue])\n\
        EVALUATE\n'Sales'";

    const M_SAMPLE: &str = "let\n\
        Source = Sql.Database(\"server\", \"db\"),\n\
        FilteredRows = Table.SelectRows(Source, each [Amount] > 0),\n\
        RenamedCols = Table.RenameColumns(FilteredRows, {{\"OldName\", \"NewName\"}})\n\
    in\n\
        RenamedCols";

    #[test]
    fn test_dax_valid_json() {
        let out = parse_dax(DAX_SAMPLE);
        intentumdiff_plugin_sdk::testing::assert_valid_json(&out, "DAX_SAMPLE");
        intentumdiff_plugin_sdk::testing::assert_no_error(&out, "DAX_SAMPLE");
    }

    #[test]
    fn test_dax_root_type() {
        let out = parse_dax(DAX_SAMPLE);
        intentumdiff_plugin_sdk::testing::assert_root_node_type(&out, "dax_file", "DAX_SAMPLE");
    }

    #[test]
    fn test_dax_measures_found() {
        let out = parse_dax(DAX_SAMPLE);
        intentumdiff_plugin_sdk::testing::assert_contains_node_type(&out, "measure", "DAX_SAMPLE");
    }

    #[test]
    fn test_dax_table_definition() {
        let out = parse_dax(DAX_SAMPLE);
        intentumdiff_plugin_sdk::testing::assert_contains_node_type(
            &out,
            "table_definition",
            "DAX_SAMPLE",
        );
    }

    #[test]
    fn test_dax_evaluate() {
        let out = parse_dax(DAX_SAMPLE);
        intentumdiff_plugin_sdk::testing::assert_contains_node_type(&out, "evaluate", "DAX_SAMPLE");
    }

    #[test]
    fn test_dax_definitions_include_expression_children() {
        let out = parse_dax(
            "DEFINE\nMEASURE Sales[Sales YTD] =\n    TOTALYTD(SUM(Sales[Amount]), Dates[Date])\n\nEVALUATE\nSUMMARIZECOLUMNS(Dates[Year], \"Total\", [Sales YTD])\n",
        );
        intentumdiff_plugin_sdk::testing::assert_contains_node_type(
            &out,
            "measure_expression",
            "DAX_SAMPLE",
        );
        intentumdiff_plugin_sdk::testing::assert_contains_node_type(
            &out,
            "query_expression",
            "DAX_SAMPLE",
        );
        assert!(
            out.contains("TOTALYTD"),
            "expected multi-line measure expression in semantic output: {}",
            out
        );
    }

    #[test]
    fn test_dax_measure_expression_changes_structural_hash() {
        let old = parse_dax("MEASURE Sales[Total Sales] = SUM(Sales[Amount])\n");
        let new = parse_dax("MEASURE Sales[Total Sales] = SUM(Sales[NetAmount])\n");
        let old_v: serde_json::Value = serde_json::from_str(&old).unwrap();
        let new_v: serde_json::Value = serde_json::from_str(&new).unwrap();
        let old_hash = &old_v["children"][0]["structural_hash"];
        let new_hash = &new_v["children"][0]["structural_hash"];
        assert_ne!(
            old_hash, new_hash,
            "changed measure expression should affect measure hash"
        );
    }

    #[test]
    fn test_dax_evaluate_expression_changes_structural_hash() {
        let old = parse_dax("EVALUATE\nSUMMARIZECOLUMNS(Dates[Year], \"Total\", [Total Sales])\n");
        let new =
            parse_dax("EVALUATE\nSUMMARIZECOLUMNS(Dates[Year], \"Growth\", [Sales Growth %])\n");
        let old_v: serde_json::Value = serde_json::from_str(&old).unwrap();
        let new_v: serde_json::Value = serde_json::from_str(&new).unwrap();
        let old_hash = &old_v["children"][0]["structural_hash"];
        let new_hash = &new_v["children"][0]["structural_hash"];
        assert_ne!(
            old_hash, new_hash,
            "changed query expression should affect evaluate hash"
        );
    }

    #[test]
    fn test_dax_evaluate_expression_uses_expression_source_range() {
        let out = parse_dax("EVALUATE\nSUMMARIZECOLUMNS(Dates[Year])\n");
        let value: serde_json::Value = serde_json::from_str(&out).unwrap();
        let expression = &value["children"][0]["children"][0];
        assert_eq!(expression["node_type"], "query_expression");
        assert_eq!(expression["position"]["start_line"], 1);
        assert!(
            expression["position"]["end_col"].as_u64().unwrap() > 0,
            "query expression must not be zero-length"
        );
    }

    #[test]
    fn test_m_query_output_change_is_semantic() {
        let old = parse_m(
            "let\n    Source = 1,\n    FilteredRows = Source\nin\n    FilteredRows\n",
        );
        let new = parse_m(
            "let\n    Source = 1,\n    FilteredRows = Source\nin\n    Source\n",
        );
        let old_v: serde_json::Value = serde_json::from_str(&old).unwrap();
        let new_v: serde_json::Value = serde_json::from_str(&new).unwrap();
        let old_query = &old_v["children"][0];
        let new_query = &new_v["children"][0];

        assert_ne!(
            old_query["structural_hash"],
            new_query["structural_hash"],
            "changing the expression after IN must alter the query hash"
        );
        assert_eq!(
            old_query["children"].as_array().unwrap().last().unwrap()["node_type"],
            "query_output"
        );
        assert_eq!(
            new_query["children"].as_array().unwrap().last().unwrap()["label"],
            "Source"
        );
    }

    #[test]
    fn test_m_query_output_exact_source_byte_span() {
        for (output, expected_end_col) in [("FilteredRows", 16_u64), ("Source", 10_u64)] {
            let source = format!(
                "let\n    Source = 1,\n    FilteredRows = Source\nin\n    {output}\n"
            );
            let parsed: serde_json::Value = serde_json::from_str(&parse_m(&source)).unwrap();
            let node = parsed["children"][0]["children"]
                .as_array().unwrap().last().unwrap();
            assert_eq!(node["node_type"], "query_output");
            assert_eq!(node["label"], output);
            assert_eq!(node["position"]["start_line"], 4);
            assert_eq!(node["position"]["end_line"], 4);
            assert_eq!(node["position"]["start_col"], 4);
            assert_eq!(node["position"]["end_col"], expected_end_col);
        }
    }

    #[test]
    fn test_m_inline_query_output_exact_source_byte_span() {
        let parsed: serde_json::Value = serde_json::from_str(&parse_m(
            "let\n    Source = 1\n  in Source\n",
        )).unwrap();
        let node = parsed["children"][0]["children"]
            .as_array().unwrap().last().unwrap();
        assert_eq!(node["node_type"], "query_output");
        assert_eq!(node["label"], "Source");
        assert_eq!(node["position"]["start_line"], 2);
        assert_eq!(node["position"]["end_line"], 2);
        assert_eq!(node["position"]["start_col"], 5);
        assert_eq!(node["position"]["end_col"], 11);
    }

    #[test]
    fn test_m_valid_json() {
        let out = parse_m(M_SAMPLE);
        intentumdiff_plugin_sdk::testing::assert_valid_json(&out, "M_SAMPLE");
        intentumdiff_plugin_sdk::testing::assert_no_error(&out, "M_SAMPLE");
    }

    #[test]
    fn test_m_root_type() {
        let out = parse_m(M_SAMPLE);
        intentumdiff_plugin_sdk::testing::assert_root_node_type(&out, "m_file", "M_SAMPLE");
    }

    #[test]
    fn test_m_query_and_steps() {
        let out = parse_m(M_SAMPLE);
        intentumdiff_plugin_sdk::testing::assert_contains_node_type(&out, "query", "M_SAMPLE");
        intentumdiff_plugin_sdk::testing::assert_contains_node_type(&out, "step", "M_SAMPLE");
    }

    #[test]
    fn test_m_steps_include_expression_children() {
        let out = parse_m("let\n    Source = 1,\n    Total = Source + 2\nin\n    Total\n");
        intentumdiff_plugin_sdk::testing::assert_contains_node_type(
            &out,
            "step_expression",
            "M expression child",
        );
        assert!(
            out.contains("Source + 2"),
            "expected step expression text in semantic output: {}",
            out
        );
    }

    #[test]
    fn test_m_step_expression_changes_structural_hash() {
        let old = parse_m("let\n    FilteredRows = Table.SelectRows(Source, each [Amount] > 0)\nin\n    FilteredRows\n");
        let new = parse_m("let\n    FilteredRows = Table.SelectRows(Source, each [Amount] > 100)\nin\n    FilteredRows\n");
        let old_v: serde_json::Value = serde_json::from_str(&old).unwrap();
        let new_v: serde_json::Value = serde_json::from_str(&new).unwrap();
        let old_hash = &old_v["children"][0]["children"][0]["structural_hash"];
        let new_hash = &new_v["children"][0]["children"][0]["structural_hash"];
        assert_ne!(
            old_hash, new_hash,
            "changed step expression should affect step hash"
        );
    }

    #[test]
    fn test_m_step_expression_exact_edited_numeric_source_spans() {
        // This is the source-judged 74-language corpus mutation which used to
        // report the edit at column 62 outside a fabricated 0..45 leaf span.
        for number in ["100", "101"] {
            let source = format!(
                "let\n    Source = Sql.Database(\"server\", \"sales\"),\n    FilteredRows = Table.SelectRows(Source, each [Amount] > {number})\nin\n    FilteredRows\n"
            );
            let parsed: serde_json::Value = serde_json::from_str(&parse_m(&source)).unwrap();
            let leaf = &parsed["children"][0]["children"][1]["children"][0];
            assert_eq!(leaf["node_type"], "step_expression");
            assert_eq!(leaf["position"]["start_line"], 2);
            assert_eq!(leaf["position"]["end_line"], 2);
            assert_eq!(leaf["position"]["start_col"], 19);
            assert_eq!(leaf["position"]["end_col"], 64);

            let source_line = source.lines().nth(2).unwrap();
            let numeric_start = source_line.rfind(number).unwrap() as u64;
            assert_eq!(numeric_start, 60);
            // The corpus changes the last byte of 100 -> 101 at column 62.
            assert_eq!(source_line.as_bytes()[62], number.as_bytes()[2]);
            let start = leaf["position"]["start_col"].as_u64().unwrap();
            let end = leaf["position"]["end_col"].as_u64().unwrap();
            assert!(start <= 62 && 63 <= end);
        }
    }

    #[test]
    fn test_m_step_expression_byte_span_accounts_for_unicode_and_rhs_whitespace() {
        let source = "let\n  #\"Café\" = \t Table.SelectRows(Source, each [Amount] > 1),\nin\n  #\"Café\"\n";
        let parsed: serde_json::Value = serde_json::from_str(&parse_m(source)).unwrap();
        let leaf = &parsed["children"][0]["children"][0]["children"][0];
        let line = source.lines().nth(1).unwrap();
        let expression = "Table.SelectRows(Source, each [Amount] > 1)";
        let start = line.find(expression).unwrap() as u64;
        assert!(line[..start as usize].contains('é'));
        assert_eq!(leaf["node_type"], "step_expression");
        assert_eq!(leaf["label"], expression);
        assert_eq!(leaf["position"]["start_col"], start);
        assert_eq!(leaf["position"]["end_col"], start + expression.len() as u64);
        assert_eq!(leaf["position"]["start_line"], 1);
        assert_eq!(leaf["position"]["end_line"], 1);
    }

    #[test]
    fn test_m_playground_example_is_power_query() {
        let example = <DaxParser as Guest>::example("m".to_string());
        assert!(example.old.contains("let"));
        assert!(example.new.contains("RenamedColumns"));
        assert!(!example.old.contains("DEFINE"));
    }

    #[test]
    fn test_measure_label_contains_table() {
        let src = "MEASURE 'Sales'[Revenue] = SUM(Sales[Amount])";
        let out = parse_dax(src);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        let children = v["children"].as_array().unwrap();
        assert!(!children.is_empty(), "expected at least one child");
        let label = children[0]["label"].as_str().unwrap_or("");
        assert!(
            label.contains("Revenue"),
            "label should contain measure name, got {:?}",
            label
        );
    }

    #[test]
    fn dax_measure_and_expression_use_actual_utf8_source_byte_extents() {
        let line = "  MEASURE Sales[Order Count] = COUNTROWS(Sales)";
        let source = format!("DEFINE\n{line}\n");
        let tree: serde_json::Value = serde_json::from_str(&parse_dax(&source)).unwrap();
        let measure = &tree["children"][0];
        assert_eq!(measure["node_type"], "measure");
        assert_eq!(measure["label"], "Sales[Order Count]");
        assert_eq!(measure["position"]["start_line"].as_u64(), Some(1));
        assert_eq!(measure["position"]["start_col"].as_u64(), Some(2));
        assert_eq!(measure["position"]["end_line"].as_u64(), Some(1));
        assert_eq!(measure["position"]["end_col"].as_u64(), Some(line.len() as u64));
        let expression = &measure["children"][0];
        assert_eq!(expression["node_type"], "measure_expression");
        assert_eq!(expression["label"], "COUNTROWS(Sales)");
        assert_eq!(expression["position"]["start_col"].as_u64(),
                   Some(line.find("COUNTROWS(Sales)").unwrap() as u64));
        assert_eq!(expression["position"]["end_col"].as_u64(), Some(line.len() as u64));
    }

    #[test]
    fn m_step_name_has_nonempty_original_source_extent() {
        let source = "let\n    UnusedPreview = Source,\nin\n    UnusedPreview\n";
        let tree: serde_json::Value = serde_json::from_str(&parse_m(source)).unwrap();
        let step = &tree["children"][0]["children"][0];
        assert_eq!(step["node_type"], "step");
        assert_eq!(step["label"], "UnusedPreview");
        assert_eq!(step["position"]["start_line"].as_u64(), Some(1));
        assert_eq!(step["position"]["end_line"].as_u64(), Some(1));
        assert_eq!(step["position"]["start_col"].as_u64(), Some(4));
        // The step covers its name through its expression (so the
        // step_expression child lies inside it) and is never zero-length.
        assert_eq!(step["position"]["end_col"].as_u64(),
                   Some("    UnusedPreview = Source".len() as u64));
        let line = source.lines().nth(1).unwrap();
        assert!(line[4..].starts_with("UnusedPreview"));
    }

    // ---- #11: complete inline and multi-line Power Query M expressions ----

    fn m_tree(source: &str) -> serde_json::Value {
        let out = parse_m(source);
        intentumdiff_plugin_sdk::testing::assert_valid_json(&out, source);
        intentumdiff_plugin_sdk::testing::assert_no_error(&out, source);
        serde_json::from_str(&out).unwrap()
    }

    /// Slice of `source` covered by `node`'s 0-based row / UTF-8 byte span.
    fn span_text<'s>(source: &'s str, node: &serde_json::Value) -> &'s str {
        let starts: Vec<usize> = std::iter::once(0)
            .chain(source.match_indices('\n').map(|(i, _)| i + 1))
            .collect();
        let p = &node["position"];
        let off = |l: &str, c: &str| {
            starts[p[l].as_u64().unwrap() as usize] + p[c].as_u64().unwrap() as usize
        };
        &source[off("start_line", "start_col")..off("end_line", "end_col")]
    }

    fn find<'v>(node: &'v serde_json::Value, ty: &str, out: &mut Vec<&'v serde_json::Value>) {
        if node["node_type"] == ty {
            out.push(node);
        }
        for c in node["children"].as_array().into_iter().flatten() {
            find(c, ty, out);
        }
    }

    fn nodes<'v>(tree: &'v serde_json::Value, ty: &str) -> Vec<&'v serde_json::Value> {
        let mut out = Vec::new();
        find(tree, ty, &mut out);
        out
    }

    fn assert_children_within_parents(source: &str, node: &serde_json::Value) {
        let key = |n: &serde_json::Value, l: &str, c: &str| {
            (n["position"][l].as_u64().unwrap(), n["position"][c].as_u64().unwrap())
        };
        for c in node["children"].as_array().into_iter().flatten() {
            assert!(key(c, "start_line", "start_col") >= key(node, "start_line", "start_col")
                && key(c, "end_line", "end_col") <= key(node, "end_line", "end_col"),
                "{} outside {} in {:?}", c["id"], node["id"], source);
            assert_children_within_parents(source, c);
        }
    }

    /// Each pair must change a hash and the changed leaf must cover the edit.
    fn assert_m_pair_changes(old: &str, new: &str, ty: &str, old_label: &str, new_label: &str) {
        let (o, n) = (m_tree(old), m_tree(new));
        assert_ne!(o["structural_hash"], n["structural_hash"], "{old:?} -> {new:?}");
        for (src, tree, label) in [(old, &o, old_label), (new, &n, new_label)] {
            let hits: Vec<_> = nodes(tree, ty).into_iter()
                .filter(|x| x["label"] == label).collect();
            assert_eq!(hits.len(), 1, "{ty} {label:?} in {src:?}: {tree}");
            assert_eq!(span_text(src, hits[0]).split_whitespace().collect::<Vec<_>>(),
                       label.split_whitespace().collect::<Vec<_>>());
            assert_children_within_parents(src, tree);
        }
    }

    #[test]
    fn m_inline_let_value_is_semantic() {
        assert_m_pair_changes("let x = 1 in x\n", "let x = 2 in x\n", "step_expression", "1", "2");
        let t = m_tree("let x = 1 in x\n");
        let q = &t["children"][0];
        assert_eq!(q["node_type"], "query");
        assert_eq!(q["children"][0]["label"], "x");
        assert_eq!(q["children"][1]["node_type"], "query_output");
        assert_eq!(q["children"][1]["label"], "x");
        assert_eq!(q["children"][1]["position"]["start_col"], 13);
        assert_eq!(q["children"][1]["position"]["end_col"], 14);
        let e = &q["children"][0]["children"][0];
        assert_eq!((e["position"]["start_col"].as_u64(), e["position"]["end_col"].as_u64()),
                   (Some(8), Some(9)));
    }

    #[test]
    fn m_inline_let_unicode_string_value_is_semantic() {
        let (old, new) = ("let label = \"λ\" in label\n", "let label = \"μ\" in label\n");
        assert_m_pair_changes(old, new, "step_expression", "\"λ\"", "\"μ\"");
        let t = m_tree(old);
        let e = &t["children"][0]["children"][0]["children"][0];
        // UTF-8 byte columns: `"λ"` is 4 bytes starting at byte 12.
        assert_eq!((e["position"]["start_col"].as_u64(), e["position"]["end_col"].as_u64()),
                   (Some(12), Some(16)));
    }

    #[test]
    fn m_multiline_binding_value_is_semantic() {
        assert_m_pair_changes(
            "let\n    x =\n        1\nin\n    x\n",
            "let\n    x =\n        2\nin\n    x\n",
            "step_expression", "1", "2",
        );
        let t = m_tree("let\n    x =\n        1\nin\n    x\n");
        let step = &t["children"][0]["children"][0];
        assert_eq!(step["label"], "x");
        assert_eq!(step["position"]["start_line"], 1);
        assert_eq!(step["position"]["end_line"], 2);
        let e = &step["children"][0];
        assert_eq!(e["position"]["start_line"], 2);
        assert_eq!(e["position"]["start_col"], 8);
    }

    #[test]
    fn m_continuation_binding_is_semantic() {
        assert_m_pair_changes(
            "let\n  x = 1 +\n    2\nin x\n",
            "let\n  x = 1 +\n    3\nin x\n",
            "step_expression", "1 + 2", "1 + 3",
        );
    }

    #[test]
    fn m_output_continuation_is_semantic() {
        assert_m_pair_changes(
            "let\n  x = 1\nin\n  x +\n  2\n",
            "let\n  x = 1\nin\n  x +\n  3\n",
            "query_output", "x + 2", "x + 3",
        );
    }

    #[test]
    fn m_several_bindings_per_line_and_nested_structures() {
        let src = "let a = {1, 2}, b = [p = 1, q = a{0}], c = let z = 3 in z + 1, #\"d e\" = (x) => x, f = \"in, let\" in f\n";
        let t = m_tree(src);
        let q = &t["children"][0];
        let names: Vec<_> = q["children"].as_array().unwrap().iter()
            .map(|c| c["label"].as_str().unwrap().to_string()).collect();
        assert_eq!(names, ["a", "b", "c", "d e", "f", "f"]);
        let exprs: Vec<_> = nodes(&t, "step_expression").iter()
            .map(|c| c["label"].as_str().unwrap().to_string()).collect();
        assert_eq!(exprs, ["{1, 2}", "[p = 1, q = a{0}]", "let z = 3 in z + 1", "(x) => x", "\"in, let\""]);
        for e in nodes(&t, "step_expression") {
            assert_eq!(span_text(src, e), e["label"].as_str().unwrap());
        }
        assert_children_within_parents(src, &t);
    }

    #[test]
    fn m_layout_and_comments_do_not_change_hashes() {
        let base = m_tree("let\n    Source = Table.FromRows({{1, \"a\"}}),\n    Kept = Table.SelectRows(Source, each [Column1] > 0)\nin\n    Kept\n");
        for variant in [
            "// leading comment\nlet\n    Source = Table.FromRows({{1, \"a\"}}),\n    Kept = Table.SelectRows(Source, each [Column1] > 0)\nin\n    Kept\n",
            "let Source = Table.FromRows({{1, \"a\"}}), Kept = Table.SelectRows(Source, each [Column1] > 0) in Kept\n",
            "let\n  Source =\n    Table.FromRows({{1, \"a\"}}), /* block */\n  Kept = Table.SelectRows(Source,\n      each [Column1] > 0) // trailing\nin\n  Kept",
        ] {
            assert_eq!(base["structural_hash"], m_tree(variant)["structural_hash"], "{variant:?}");
        }
        // Text inside string literals is content, not a comment or layout.
        let a = m_tree("let x = \"a // b\" in x\n");
        let b = m_tree("let x = \"a // c\" in x\n");
        assert_ne!(a["structural_hash"], b["structural_hash"]);
        let a = m_tree("let x = \"a  b\" in x\n");
        let b = m_tree("let x = \"a b\" in x\n");
        assert_ne!(a["structural_hash"], b["structural_hash"]);
    }

    #[test]
    fn m_identity_is_deterministic() {
        let src = "let\n    x =\n        1\nin\n    x\n";
        assert_eq!(parse_m(src), parse_m(src));
    }

    #[test]
    fn m_non_let_and_section_documents_keep_their_content() {
        assert_m_pair_changes("1 + 2\n", "1 + 3\n", "expression", "1 + 2", "1 + 3");
        let old = "section Sales;\nshared Total = let s = 1 in s;\nHelper = 5;\n";
        let new = "section Sales;\nshared Total = let s = 2 in s;\nHelper = 5;\n";
        assert_m_pair_changes(old, new, "step_expression", "1", "2");
        let t = m_tree(old);
        let sec = &t["children"][0];
        assert_eq!(sec["node_type"], "section");
        assert_eq!(sec["label"], "Sales");
        let members = sec["children"].as_array().unwrap();
        assert_eq!(members[0]["label"], "Total");
        assert_eq!(members[0]["children"][0]["node_type"], "modifier");
        assert_eq!(members[1]["label"], "Helper");
        assert_eq!(members[1]["children"][0]["node_type"], "member_expression");
        let unshared = m_tree("section Sales;\nTotal = let s = 1 in s;\nHelper = 5;\n");
        assert_ne!(t["structural_hash"], unshared["structural_hash"]);
    }

    #[test]
    fn m_incomplete_and_odd_inputs_never_panic() {
        for src in [
            "", "let", "let x", "let x =", "let x = (1, [2", "let x = \"unterminated",
            "let #\"open", "/* open comment", "in x", "let x = 1 in", "section", "section S; x",
            "let\n    Source = Sql.Database(\"server\", \"sales\"),\n    FilteredRows = Table.SelectRows(Source, each [Amount] > 10",
            "let é = \"ü\" in é", "let x = 1..5 in x", "let x = 0xFF + 1.5e3 in x\r\n",
        ] {
            let t = m_tree(src);
            assert_children_within_parents(src, &t);
            let _ = span_text(src, &t);
        }
    }

    #[test]
    fn m_detects_inline_let_and_section_files() {
        assert_eq!(detect_language_impl("q.m", "let x = 1 in x\n"), "m");
        assert_eq!(detect_language_impl("q.m", "// c\nlet\n x = 1\nin x"), "m");
        assert_eq!(detect_language_impl("q.m", "section S;\nx = 1;"), "m");
        assert_eq!(detect_language_impl("q.m", "letter = 1"), "");
    }

    #[test]
    fn dax_multiline_definitions_contain_their_expressions() {
        let src = "DEFINE\nMEASURE Sales[Sales YTD] =\n    TOTALYTD(SUM(Sales[Amount]),\n\n    // note\n        Dates[Date])\nEVALUATE ROW(\"label\", \"λ\")\n";
        let tree: serde_json::Value = serde_json::from_str(&parse_dax(src)).unwrap();
        assert_children_within_parents(src, &tree);
        let m = &tree["children"][0];
        let e = &m["children"][0];
        assert_eq!(e["position"]["end_line"], 5);
        assert!(span_text(src, e).ends_with("Dates[Date])"));
        let q = &tree["children"][1]["children"][0];
        assert_eq!(span_text(src, q), "ROW(\"label\", \"λ\")");
    }

    // ---- #13: a nested `let` that starts a line is not a new document ----

    #[test]
    fn m_nested_let_on_new_line_stays_in_enclosing_output() {
        let old = "let x = 1 in if true then let y = 2 in y else 0";
        let base = m_tree(old);
        let queries = base["children"].as_array().unwrap();
        assert_eq!(queries.len(), 1, "{base}");
        let output = &nodes(&base, "query_output")[0];
        assert_eq!(output["label"], "if true then let y = 2 in y else 0");
        for variant in [
            "let x = 1 in if true then\nlet y = 2 in y else 0",
            "let x = 1 in if true then 1 else\nlet y = 2 in y",
            "let x = 1 in\nlet y = 2 in y",
            "let x = 1 in x +\nlet y = 2 in y",
            "let x = 1 in f(\nlet y = 2 in y)",
            "let x = 1 in each\nlet y = 2 in y",
        ] {
            let t = m_tree(variant);
            assert_eq!(t["children"].as_array().unwrap().len(), 1, "{variant:?}: {t}");
            assert_children_within_parents(variant, &t);
        }
        let reflowed = m_tree("let x = 1 in if true then\nlet y = 2 in y else 0");
        assert_eq!(nodes(&reflowed, "query_output")[0]["label"], output["label"]);
        assert_eq!(reflowed["structural_hash"], base["structural_hash"]);
        // Two genuinely separate top-level documents are still split.
        for two in ["let a = 1 in a\nlet b = 2 in b\n", "let a = 1 in if a then 1 else 0\nlet b = 2 in b\n",
                    "let a = 1 in f(a)\nlet b = 2 in b\n", "let a = 1 in let c = 3 in c\nlet b = 2 in b\n"] {
            let t = m_tree(two);
            assert_eq!(t["children"].as_array().unwrap().len(), 2, "{two:?}: {t}");
        }
    }
}
