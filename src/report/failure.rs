//! Terminal and machine-readable rendering for a failed command.
//!
//! Diagnostics are off by default, so a failure must never depend on `--log` to be
//! visible. These renderers own that guarantee: human runs get a colored stderr line,
//! and `--json` runs get one parsable object so scripted callers can branch on `kind`.

use std::io::{self, IsTerminal, Write};

use owo_colors::{OwoColorize, Stream};

use crate::error::Error;

/// Writes a failure as a colored line plus an optional next-step hint.
///
/// # Errors
///
/// Returns the underlying writer error when the diagnostic cannot be written.
pub fn write_human(error: &Error, output: &mut dyn Write) -> io::Result<()> {
    // A canceled operation is an answer, not a fault. Prefixing a deliberate `no` with `error:`
    // tells the operator the program broke when it did exactly what they asked.
    if let Error::Canceled { reason } = error {
        return writeln!(output, "{reason}");
    }
    writeln!(
        output,
        "{} {error}",
        "error:".if_supports_color(Stream::Stderr, |text| text.red().bold().to_string())
    )?;
    if let Some(hint) = error.hint() {
        writeln!(
            output,
            "{} {hint}",
            "hint:".if_supports_color(Stream::Stderr, |text| text.yellow().bold().to_string())
        )?;
    }
    Ok(())
}

/// Writes a failure as one JSON object on the report stream.
///
/// The object carries `kind`, `exit_code`, `message`, and an optional `hint`, which lets a
/// script or agent classify the failure without matching on the human message.
///
/// # Errors
///
/// Returns the underlying writer error when the diagnostic cannot be written.
pub fn write_json(error: &Error, output: &mut dyn Write) -> io::Result<()> {
    let mut object = serde_json::Map::new();
    object.insert("error".to_owned(), serde_json::Value::Bool(true));
    object.insert(
        "kind".to_owned(),
        serde_json::Value::String(error.kind().to_owned()),
    );
    object.insert(
        "exit_code".to_owned(),
        serde_json::Value::from(error.exit_code()),
    );
    object.insert(
        "message".to_owned(),
        serde_json::Value::String(error.to_string()),
    );
    if let Some(hint) = error.hint() {
        object.insert(
            "hint".to_owned(),
            serde_json::Value::String(hint.to_owned()),
        );
    }
    writeln!(output, "{}", serde_json::Value::Object(object))
}

/// Reports whether stderr can render ANSI styling.
#[must_use]
pub fn stderr_is_terminal() -> bool {
    io::stderr().is_terminal()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{write_human, write_json};
    use crate::error::Error;

    fn busy() -> Error {
        Error::DatabaseBusy {
            holders: vec!["opencode (pid 42)".to_owned()],
        }
    }

    #[test]
    fn human_output_names_the_failure_and_its_next_step() {
        let mut buffer = Vec::new();
        write_human(&busy(), &mut buffer).expect("writing to a vector cannot fail");
        let rendered = String::from_utf8(buffer).expect("output is UTF-8");

        assert!(rendered.contains("error:"));
        assert!(rendered.contains("opencode (pid 42)"));
        assert!(rendered.contains("hint:"));
        assert!(rendered.contains("stop OpenCode"));
    }

    #[test]
    fn json_output_is_one_parsable_object_carrying_the_stable_kind() {
        let mut buffer = Vec::new();
        write_json(&busy(), &mut buffer).expect("writing to a vector cannot fail");
        let rendered = String::from_utf8(buffer).expect("output is UTF-8");
        let value: serde_json::Value =
            serde_json::from_str(rendered.trim()).expect("output parses as JSON");

        assert_eq!(value["kind"], "database_busy");
        assert_eq!(value["exit_code"], 5);
        assert_eq!(value["error"], true);
        assert!(
            value["message"]
                .as_str()
                .expect("message is a string")
                .contains("pid 42")
        );
    }

    #[test]
    fn a_failure_without_a_hint_omits_the_field() {
        let mut buffer = Vec::new();
        write_json(
            &Error::NotFound {
                path: PathBuf::from("/tmp/missing.db"),
            },
            &mut buffer,
        )
        .expect("writing to a vector cannot fail");
        let value: serde_json::Value = serde_json::from_slice(&buffer).expect("output parses");

        assert_eq!(value["kind"], "not_found");
        assert!(value.get("hint").is_none());
    }
}
