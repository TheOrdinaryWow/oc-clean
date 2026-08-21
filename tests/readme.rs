use std::collections::{BTreeMap, BTreeSet};

use clap::{Arg, CommandFactory};
use oc_clean::cli::Cli;

const README: &str = include_str!("../README.md");
const ERROR_SOURCE: &str = include_str!("../src/error.rs");

#[test]
fn flag_table_matches_clap_in_both_directions() {
    let expected = clap_flags();
    let documented = option_rows();

    assert_eq!(documented, expected);
}

#[test]
fn exit_code_table_covers_every_error_variant() {
    let expected = expected_exit_codes();
    let source_variants = error_variants();
    let documented = exit_code_rows();

    assert_eq!(source_variants, expected.keys().cloned().collect());
    assert_eq!(documented, expected);
}

#[test]
fn prose_paragraphs_are_single_lines() {
    let mut previous_was_prose = false;
    let mut in_code_block = false;

    for (index, line) in README.lines().enumerate() {
        if line.starts_with("```") {
            in_code_block = !in_code_block;
            previous_was_prose = false;
            continue;
        }
        if in_code_block {
            continue;
        }

        let is_prose = !line.is_empty()
            && !line.starts_with('#')
            && !line.starts_with('|')
            && !line.starts_with("- ")
            && !line.starts_with("1. ")
            && !line.starts_with('>');
        assert!(
            !(previous_was_prose && is_prose),
            "README lines {} and {} form a soft-wrapped paragraph",
            index,
            index + 1
        );
        previous_was_prose = is_prose;
    }
}

#[test]
fn roadmap_names_planned_cleanup_integrations() {
    let roadmap = section("Roadmap");

    assert!(roadmap.contains("AFT"));
    assert!(roadmap.contains("Magic Context"));
}

#[test]
fn readme_uses_ascii_text() {
    assert!(
        README.is_ascii(),
        "README must use ASCII text without emoji"
    );
}

fn clap_flags() -> BTreeSet<(String, String, Option<String>)> {
    let command = Cli::command();
    let global_ids = command
        .get_arguments()
        .filter(|argument| is_documented_flag(argument))
        .map(|argument| argument.get_id().to_string())
        .collect::<BTreeSet<_>>();
    let mut flags = command
        .get_arguments()
        .filter(|argument| is_documented_flag(argument))
        .map(|argument| flag_record("Global", argument))
        .collect::<BTreeSet<_>>();

    for subcommand in command.get_subcommands() {
        flags.extend(
            subcommand
                .get_arguments()
                .filter(|argument| {
                    is_documented_flag(argument) && !global_ids.contains(argument.get_id().as_str())
                })
                .map(|argument| flag_record(subcommand.get_name(), argument)),
        );
    }

    flags
}

fn is_documented_flag(argument: &Arg) -> bool {
    argument.get_long().is_some() && argument.get_id().as_str() != "help"
}

fn flag_record(scope: &str, argument: &Arg) -> (String, String, Option<String>) {
    let long = argument.get_long().expect("long option");
    let signature = argument
        .get_action()
        .takes_values()
        .then(|| argument.get_value_names())
        .flatten()
        .map_or_else(
            || format!("--{long}"),
            |names| {
                let values = names
                    .iter()
                    .map(|name| format!("<{name}>"))
                    .collect::<Vec<_>>()
                    .join(" ");
                format!("--{long} {values}")
            },
        );
    (
        scope.to_owned(),
        signature,
        argument
            .get_env()
            .map(|environment| environment.to_string_lossy().into_owned()),
    )
}

fn option_rows() -> BTreeSet<(String, String, Option<String>)> {
    markdown_rows("Options")
        .map(|cells| {
            assert_eq!(cells.len(), 4, "option rows must have four columns");
            let environment = match cells[2].as_str() {
                "CLI only" => None,
                value => Some(value.to_owned()),
            };
            (cells[0].clone(), cells[1].clone(), environment)
        })
        .collect()
}

fn expected_exit_codes() -> BTreeMap<String, i32> {
    [
        ("Io", 1),
        ("Sqlite", 1),
        ("InvalidArgument", 2),
        ("NotFound", 3),
        ("SchemaIncompatible", 4),
        ("DatabaseBusy", 5),
        ("InsufficientDiskSpace", 6),
        ("ReclaimUnavailable", 6),
        ("IntegrityCheckFailed", 7),
        ("Interrupted", 8),
        ("UnsupportedPlatform", 9),
        ("PartialSuccess", 10),
        ("SwapRollbackFailed", 11),
    ]
    .into_iter()
    .map(|(variant, code)| (variant.to_owned(), code))
    .collect()
}

fn error_variants() -> BTreeSet<String> {
    let enum_body = ERROR_SOURCE
        .split_once("pub enum Error {")
        .expect("Error enum declaration")
        .1
        .split_once("\n}")
        .expect("Error enum terminator")
        .0;

    enum_body
        .lines()
        .filter_map(|line| {
            let line = line.strip_prefix("    ")?;
            let variant = line.split_once(" {")?.0;
            variant
                .chars()
                .all(|character| character.is_ascii_alphanumeric())
                .then(|| variant.to_owned())
        })
        .collect()
}

fn exit_code_rows() -> BTreeMap<String, i32> {
    let rows = markdown_rows("Exit Codes").collect::<Vec<_>>();
    assert!(
        rows.iter()
            .any(|cells| cells.first().is_some_and(|code| code == "0")),
        "exit-code table must document complete success"
    );
    rows.into_iter()
        .filter(|cells| cells.first().is_some_and(|code| code != "0"))
        .map(|cells| {
            assert_eq!(cells.len(), 3, "exit-code rows must have three columns");
            let code = cells[0].parse::<i32>().expect("numeric exit code");
            (cells[1].clone(), code)
        })
        .collect()
}

fn markdown_rows(heading: &str) -> impl Iterator<Item = Vec<String>> + '_ {
    section(heading).lines().filter_map(|line| {
        if !line.starts_with("| ") || line.contains("---") {
            return None;
        }
        let cells = line
            .trim_matches('|')
            .split('|')
            .map(|cell| cell.trim().trim_matches('`').to_owned())
            .collect::<Vec<_>>();
        (!matches!(cells.first().map(String::as_str), Some("Scope" | "Code"))).then_some(cells)
    })
}

fn section(heading: &str) -> &str {
    let marker = format!("## {heading}\n");
    README
        .split_once(&marker)
        .unwrap_or_else(|| panic!("README must contain {marker:?}"))
        .1
        .split("\n## ")
        .next()
        .expect("section body")
}
