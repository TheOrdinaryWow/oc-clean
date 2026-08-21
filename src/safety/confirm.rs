use std::io::{self, BufRead, Write};

/// Minimal confirmation view of the impact report.
///
/// Todo 19 may replace this adapter with its report type once that module is available.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ImpactSummary<'a> {
    pub operation: &'a str,
    pub details: &'a str,
}

/// Runtime facts that control whether confirmation may read or write a terminal.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConfirmationOptions {
    pub stdin_is_terminal: bool,
    pub stdout_is_terminal: bool,
    pub json: bool,
    pub dangerously_skip_confirm: bool,
}

impl ConfirmationOptions {
    const fn is_interactive(self) -> bool {
        self.stdin_is_terminal
            && self.stdout_is_terminal
            && !self.json
            && !self.dangerously_skip_confirm
    }
}

/// Policy result kept separate from command errors until a destructive command applies it.
///
/// Todo 28/31 will route disk-headroom conflicts directly through [`Self::Refuse`] in every mode.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConfirmationDecision {
    Proceed,
    Refuse,
}

/// Confirms a destructive operation according to the interactive contract.
///
/// Non-interactive calls perform no input or output. Their default is [`ConfirmationDecision::Refuse`],
/// while `dangerously_skip_confirm` selects [`ConfirmationDecision::Proceed`]. Interactive calls
/// accept only `y` or `yes`, ignoring ASCII case and surrounding whitespace.
///
/// # Errors
///
/// Returns an I/O error when rendering or reading an interactive prompt fails.
pub fn confirm<R, W>(
    summary: &ImpactSummary<'_>,
    options: ConfirmationOptions,
    input: &mut R,
    output: &mut W,
) -> io::Result<ConfirmationDecision>
where
    R: BufRead + ?Sized,
    W: Write + ?Sized,
{
    if options.dangerously_skip_confirm {
        return Ok(ConfirmationDecision::Proceed);
    }
    if !options.is_interactive() {
        return Ok(ConfirmationDecision::Refuse);
    }

    writeln!(output, "{} impact:", summary.operation)?;
    writeln!(output, "{}", summary.details)?;
    write!(output, "Proceed? [y/N] ")?;
    output.flush()?;

    let mut answer = String::new();
    input.read_line(&mut answer)?;
    if answer.trim().eq_ignore_ascii_case("y") || answer.trim().eq_ignore_ascii_case("yes") {
        Ok(ConfirmationDecision::Proceed)
    } else {
        Ok(ConfirmationDecision::Refuse)
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::{ConfirmationDecision, ConfirmationOptions, ImpactSummary, confirm};

    fn summary() -> ImpactSummary<'static> {
        ImpactSummary {
            operation: "clean",
            details: "3 root sessions, 8 total sessions, 12.5 MB",
        }
    }

    #[test]
    fn piped_stdio_emits_no_prompt_and_refuses() {
        let mut input = Cursor::new(b"yes\n");
        let mut output = Vec::new();

        let decision = confirm(
            &summary(),
            ConfirmationOptions {
                stdin_is_terminal: false,
                stdout_is_terminal: false,
                json: false,
                dangerously_skip_confirm: false,
            },
            &mut input,
            &mut output,
        )
        .unwrap();

        assert_eq!(decision, ConfirmationDecision::Refuse);
        assert_eq!(output, b"");
    }

    #[test]
    fn skip_confirm_proceeds_without_emitting_a_prompt() {
        let mut input = Cursor::new(Vec::<u8>::new());
        let mut output = Vec::new();

        let decision = confirm(
            &summary(),
            ConfirmationOptions {
                stdin_is_terminal: false,
                stdout_is_terminal: false,
                json: false,
                dangerously_skip_confirm: true,
            },
            &mut input,
            &mut output,
        )
        .unwrap();

        assert_eq!(decision, ConfirmationDecision::Proceed);
        assert_eq!(output, b"");
    }

    #[test]
    fn json_disables_prompts() {
        let confirmation = ConfirmationOptions {
            stdin_is_terminal: true,
            stdout_is_terminal: true,
            json: true,
            dangerously_skip_confirm: false,
        };
        let mut input = Cursor::new(b"yes\n");
        let mut output = Vec::new();

        assert_eq!(
            confirm(&summary(), confirmation, &mut input, &mut output).unwrap(),
            ConfirmationDecision::Refuse
        );
        assert_eq!(output, b"");
    }

    #[test]
    fn interactive_confirmation_requires_an_explicit_affirmative() {
        for (answer, expected) in [
            ("yes\n", ConfirmationDecision::Proceed),
            ("y\n", ConfirmationDecision::Proceed),
            ("\n", ConfirmationDecision::Refuse),
            ("maybe\n", ConfirmationDecision::Refuse),
        ] {
            let mut input = Cursor::new(answer.as_bytes());
            let mut output = Vec::new();
            let decision = confirm(
                &summary(),
                ConfirmationOptions {
                    stdin_is_terminal: true,
                    stdout_is_terminal: true,
                    json: false,
                    dangerously_skip_confirm: false,
                },
                &mut input,
                &mut output,
            )
            .unwrap();

            assert_eq!(decision, expected);
            let rendered = String::from_utf8(output).unwrap();
            assert!(rendered.contains("clean"));
            assert!(rendered.contains("3 root sessions"));
            assert!(rendered.contains("Proceed? [y/N]"));
        }
    }
}
