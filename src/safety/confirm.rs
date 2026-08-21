use std::io::{self, BufRead, Write};

/// Minimal confirmation view of the impact report.
///
/// Todo 19 may replace this adapter with its report type once that module is available.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ImpactSummary<'a> {
    pub operation: &'a str,
    pub details: &'a str,
    /// A second prompt shown when the selection is large enough to warrant one.
    pub escalation: Option<&'a str>,
}

/// Share of a population at or above which a second confirmation is required.
///
/// Expressed as a fraction so the comparison stays in integer arithmetic: `selected * 2 >= total`
/// is exact where `selected as f64 / total as f64 >= 0.5` would round near the boundary.
const ESCALATION_NUMERATOR: u64 = 1;
const ESCALATION_DENOMINATOR: u64 = 2;

/// Reports whether deleting `selected` out of `total` items warrants a second confirmation.
///
/// An empty population never escalates: there is nothing to lose, and `0 >= 0` would otherwise
/// escalate a selection that deletes nothing.
#[must_use]
pub const fn warrants_escalation(selected: u64, total: u64) -> bool {
    total > 0
        && selected > 0
        && selected.saturating_mul(ESCALATION_DENOMINATOR)
            >= total.saturating_mul(ESCALATION_NUMERATOR)
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
/// When `summary.escalation` is present, a second independent prompt follows the first and both
/// must be answered affirmatively. `dangerously_skip_confirm` bypasses both, because an operator
/// who asked for no prompts gains nothing from being asked twice.
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
    if prompt(output, input, "Proceed? [y/N] ")? == ConfirmationDecision::Refuse {
        return Ok(ConfirmationDecision::Refuse);
    }

    let Some(escalation) = summary.escalation else {
        return Ok(ConfirmationDecision::Proceed);
    };
    writeln!(output, "{escalation}")?;
    prompt(output, input, "Are you sure? [y/N] ")
}

fn prompt<R, W>(output: &mut W, input: &mut R, question: &str) -> io::Result<ConfirmationDecision>
where
    R: BufRead + ?Sized,
    W: Write + ?Sized,
{
    write!(output, "{question}")?;
    output.flush()?;

    let mut answer = String::new();
    input.read_line(&mut answer)?;
    let answer = answer.trim();
    if answer.eq_ignore_ascii_case("y") || answer.eq_ignore_ascii_case("yes") {
        Ok(ConfirmationDecision::Proceed)
    } else {
        Ok(ConfirmationDecision::Refuse)
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::{
        ConfirmationDecision, ConfirmationOptions, ImpactSummary, confirm, warrants_escalation,
    };

    fn summary() -> ImpactSummary<'static> {
        ImpactSummary {
            operation: "clean",
            details: "3 root sessions, 8 total sessions, 12.5 MB",
            escalation: None,
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

    #[test]
    fn escalation_requires_a_second_affirmative_and_only_when_present() {
        fn decide(escalation: Option<&str>, answers: &str) -> (ConfirmationDecision, String) {
            let mut input = Cursor::new(answers.as_bytes().to_vec());
            let mut output = Vec::new();
            let decision = confirm(
                &ImpactSummary {
                    operation: "clean",
                    details: "8 sessions",
                    escalation,
                },
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
            (
                decision,
                String::from_utf8(output).expect("prompt should be UTF-8"),
            )
        }

        let (decision, rendered) = decide(None, "y\n");
        assert_eq!(decision, ConfirmationDecision::Proceed);
        assert!(!rendered.contains("Are you sure?"));

        let (decision, rendered) = decide(Some("This deletes 8 of the 8 sessions."), "y\ny\n");
        assert_eq!(decision, ConfirmationDecision::Proceed);
        assert!(rendered.contains("This deletes 8 of the 8 sessions."));
        assert!(rendered.contains("Are you sure?"));

        assert_eq!(
            decide(Some("majority"), "y\nn\n").0,
            ConfirmationDecision::Refuse
        );
        let (decision, rendered) = decide(Some("majority"), "n\ny\n");
        assert_eq!(decision, ConfirmationDecision::Refuse);
        assert!(!rendered.contains("Are you sure?"));
    }

    #[test]
    fn escalation_threshold_is_exact_at_half_and_ignores_empty_selections() {
        assert!(!warrants_escalation(0, 0));
        assert!(!warrants_escalation(0, 10));
        assert!(!warrants_escalation(4, 10));
        assert!(warrants_escalation(5, 10));
        assert!(warrants_escalation(10, 10));
        // An odd population rounds toward requiring the extra prompt.
        assert!(warrants_escalation(4, 7));
        assert!(!warrants_escalation(3, 7));
    }
}
