//! DAX / Power Query M parser plugin — full-parse mode.
//!
//! Handles `.dax` files (DAX — Data Analysis Expressions for Power BI / SSAS)
//! and `.m` files (Power Query M language).  No tree-sitter grammar exists for
//! either language; this plugin uses case-insensitive line scanning.
//!
//! DAX semantic nodes:
//!   dax_file          — root
//!   measure           — DEFINE MEASURE 'Table'[Name] = … (label = Table[Name])
//!   table_definition  — DEFINE TABLE <name> = …
//!   column_definition — DEFINE COLUMN 'Table'[Name] = …
//!   var_definition    — DEFINE VAR <name> = …
//!   evaluate          — EVALUATE block (label = query_N)
//!
//! Power Query M semantic nodes:
//!   m_file            — root
//!   query             — let … in block (label = query_N)
//!   step              — named step inside a let block (label = step name)
//!   step_expression   — right-hand side of a step
//!
//! Detection:
//!   .dax → "dax" unconditionally.
//!   .m   → content-based: DAX keywords → "dax"; let/in structure → "m".

use intentumdiff_plugin_sdk::tree::{SemanticNode, SemanticNodeBuilder};

wit_bindgen::generate!({
    path: "wit/plugin.wit",
    world: "parser-plugin",
});

use crate::exports::intentumdiff::plugin::parser::ExamplePair;
use crate::exports::intentumdiff::plugin::parser::Guest;
use crate::exports::intentumdiff::plugin::parser::LanguageInfoRecord;
use crate::exports::intentumdiff::plugin::parser::ParserMode;

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

fn leaf(id: &str, node_type: &str, label: &str, line: u32) -> SemanticNode {
    SemanticNodeBuilder::new(id, node_type, label, line, 0, line, 0, String::new()).build()
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

fn strip_m_trailing_comma(value: &str) -> String {
    value.trim().trim_end_matches(',').trim().to_string()
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
        if cl.contains("\nlet\n")
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
) {
    let id = format!("0.{}", counter);
    *counter += 1;
    let expression = definition.expression_lines.join("\n").trim().to_string();
    let expression_children = if expression.is_empty() {
        vec![]
    } else {
        vec![leaf(
            &format!("{}.0", id),
            definition.expression_type,
            &expression,
            definition.expression_start,
        )]
    };
    children.push(block_node(
        &id,
        definition.node_type,
        &definition.label,
        definition.start,
        definition.end,
        expression_children,
    ));
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
                push_dax_definition(&mut children, &mut counter, previous);
            }
            active = Some(definition);
            continue;
        }

        if let Some(definition) = active.as_mut() {
            definition.expression_lines.push(trimmed.to_string());
            definition.end = lineno;
        }
    }

    if let Some(definition) = active.take() {
        push_dax_definition(&mut children, &mut counter, definition);
    }

    let root = SemanticNodeBuilder::new(
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

    match serde_json::to_string(&root) {
        Ok(s) => s,
        Err(e) => format!(r#"{{"error":"Serialisation error: {}"}}"#, e),
    }
}

// ---------------------------------------------------------------------------
// Power Query M parser
// ---------------------------------------------------------------------------

pub(crate) fn parse_m(source: &str) -> String {
    let mut children: Vec<SemanticNode> = Vec::new();
    let mut counter: usize = 0;
    let total_lines = source.lines().count().saturating_sub(1) as u32;

    let mut in_let = false;
    let mut query_id: Option<String> = None;
    let mut query_start: u32 = 0;
    let mut query_counter: usize = 0;
    let mut step_children: Vec<SemanticNode> = Vec::new();
    let mut step_count: usize = 0;

    for (idx, line) in source.lines().enumerate() {
        let lineno = idx as u32;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let lower_trimmed = trimmed.to_lowercase();

        // Detect "let" keyword starting a query
        if !in_let
            && (lower_trimmed == "let"
                || lower_trimmed.starts_with("let ")
                || lower_trimmed.starts_with("let\t"))
        {
            in_let = true;
            query_start = lineno;
            step_children.clear();
            step_count = 0;
            let qid = format!("0.{}", counter);
            counter += 1;
            query_id = Some(qid);
            continue;
        }

        // Detect "in" keyword closing a query
        if in_let
            && (lower_trimmed == "in"
                || lower_trimmed.starts_with("in ")
                || lower_trimmed.starts_with("in\t"))
        {
            in_let = false;
            let label = format!("query_{}", query_counter);
            query_counter += 1;
            let qid = query_id.take().unwrap_or_else(|| {
                let i = format!("0.{}", counter);
                counter += 1;
                i
            });
            let n = block_node(
                &qid,
                "query",
                &label,
                query_start,
                lineno,
                step_children.clone(),
            );
            children.push(n);
            step_children.clear();
            continue;
        }

        // Collect steps inside let block
        if in_let {
            if let Some(eq_pos) = trimmed.find('=') {
                // Strip trailing comma from name; handle #"quoted names"
                let raw_name = trimmed[..eq_pos].trim();
                let name = raw_name
                    .trim_start_matches('#')
                    .trim_matches('"')
                    .to_string();
                if !name.is_empty() {
                    let qid_ref = query_id.as_deref().unwrap_or("0");
                    let step_id = format!("{}.{}", qid_ref, step_count);
                    step_count += 1;
                    let expression = strip_m_trailing_comma(&trimmed[eq_pos + 1..]);
                    let mut children = Vec::new();
                    if !expression.is_empty() {
                        let expression_id = format!("{}.0", step_id);
                        children.push(leaf(&expression_id, "step_expression", &expression, lineno));
                    }
                    step_children.push(block_node(
                        &step_id, "step", &name, lineno, lineno, children,
                    ));
                }
            }
        }
    }

    // Drain unclosed let block
    if in_let {
        let label = format!("query_{}", query_counter);
        let qid = query_id.take().unwrap_or_else(|| {
            let i = format!("0.{}", counter);
            counter += 1;
            i
        });
        let n = block_node(
            &qid,
            "query",
            &label,
            query_start,
            total_lines,
            step_children,
        );
        children.push(n);
    }

    let root =
        SemanticNodeBuilder::new("0", "m_file", "m_file", 0, 0, total_lines, 0, String::new())
            .children(children)
            .build();

    match serde_json::to_string(&root) {
        Ok(s) => s,
        Err(e) => format!(r#"{{"error":"Serialisation error: {}"}}"#, e),
    }
}

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
}
