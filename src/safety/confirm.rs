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
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConfirmationDecision {
    Proceed,
    Refuse(RefusalReason),
}

/// Why a confirmation did not proceed, so the caller can word the outcome honestly.
///
/// A person answering `no` made a decision; a stream that cannot be asked, or an answer that was
/// never recognized, did not. Rendering all three the same way would tell an operator their
/// deliberate `no` was a program failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RefusalReason {
    /// The operator answered in the negative.
    Declined,
    /// Every attempt produced an answer that was neither affirmative nor negative.
    Unanswered,
    /// The invocation could not present a prompt at all.
    NotInteractive,
}

/// How many times one question may be asked before the command gives up.
///
/// The first ask plus two retries. A mistyped answer is common enough to deserve a retry, and an
/// operator who cannot produce a recognized answer three times will not on the fourth.
pub const CONFIRMATION_ATTEMPTS: u32 = 3;

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
        return Ok(ConfirmationDecision::Refuse(RefusalReason::NotInteractive));
    }

    writeln!(output, "{} impact:", summary.operation)?;
    writeln!(output, "{}", summary.details)?;
    match prompt(output, input, "Proceed? [y/n] ")? {
        ConfirmationDecision::Proceed => {}
        refusal @ ConfirmationDecision::Refuse(_) => return Ok(refusal),
    }

    let Some(escalation) = summary.escalation else {
        return Ok(ConfirmationDecision::Proceed);
    };
    writeln!(output, "{escalation}")?;
    prompt(output, input, "Are you sure? [y/n] ")
}

/// Asks one question until it is answered or the attempt allowance runs out.
///
/// An unrecognized answer is re-asked rather than treated as a refusal, because a typo is not a
/// decision. An explicit negative ends the question immediately: the operator already decided,
/// and asking again would be pestering them into a different answer.
///
/// End-of-input stops the loop. A closed stream will not produce a different answer on the next
/// attempt, so retrying would spin without ever blocking.
fn prompt<R, W>(output: &mut W, input: &mut R, question: &str) -> io::Result<ConfirmationDecision>
where
    R: BufRead + ?Sized,
    W: Write + ?Sized,
{
    for remaining in (0..CONFIRMATION_ATTEMPTS).rev() {
        write!(output, "{question}")?;
        output.flush()?;

        let mut answer = String::new();
        if input.read_line(&mut answer)? == 0 {
            return Ok(ConfirmationDecision::Refuse(RefusalReason::Unanswered));
        }
        let answer = answer.trim();
        if answer.eq_ignore_ascii_case("y") || answer.eq_ignore_ascii_case("yes") {
            return Ok(ConfirmationDecision::Proceed);
        }
        if answer.eq_ignore_ascii_case("n") || answer.eq_ignore_ascii_case("no") {
            return Ok(ConfirmationDecision::Refuse(RefusalReason::Declined));
        }
        if remaining > 0 {
            writeln!(
                output,
                "Please answer `y` or `n` ({remaining} {} left).",
                if remaining == 1 {
                    "attempt"
                } else {
                    "attempts"
                }
            )?;
        }
    }
    Ok(ConfirmationDecision::Refuse(RefusalReason::Unanswered))
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::{
        CONFIRMATION_ATTEMPTS, ConfirmationDecision, ConfirmationOptions, ImpactSummary,
        RefusalReason, confirm, warrants_escalation,
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

        assert_eq!(
            decision,
            ConfirmationDecision::Refuse(RefusalReason::NotInteractive)
        );
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
            ConfirmationDecision::Refuse(RefusalReason::NotInteractive)
        );
        assert_eq!(output, b"");
    }

    #[test]
    fn interactive_confirmation_requires_an_explicit_affirmative() {
        for (answer, expected) in [
            ("yes\n", ConfirmationDecision::Proceed),
            ("y\n", ConfirmationDecision::Proceed),
            ("YES\n", ConfirmationDecision::Proceed),
            ("  y  \n", ConfirmationDecision::Proceed),
            ("n\n", ConfirmationDecision::Refuse(RefusalReason::Declined)),
            (
                "NO\n",
                ConfirmationDecision::Refuse(RefusalReason::Declined),
            ),
            (
                "\n\n\n",
                ConfirmationDecision::Refuse(RefusalReason::Unanswered),
            ),
            (
                "maybe\nwhat\nhuh\n",
                ConfirmationDecision::Refuse(RefusalReason::Unanswered),
            ),
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
            assert!(rendered.contains("Proceed? [y/n]"));
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
            ConfirmationDecision::Refuse(RefusalReason::Declined)
        );
        let (decision, rendered) = decide(Some("majority"), "n\ny\n");
        assert_eq!(
            decision,
            ConfirmationDecision::Refuse(RefusalReason::Declined)
        );
        assert!(!rendered.contains("Are you sure?"));
    }

    #[test]
    fn an_unrecognized_answer_is_re_asked_and_the_allowance_is_per_question() {
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

        // A typo costs an attempt, not the command.
        let (decision, rendered) = decide(None, "wat\ny\n");
        assert_eq!(decision, ConfirmationDecision::Proceed);
        assert_eq!(rendered.matches("Proceed? [y/n]").count(), 2);
        assert!(rendered.contains("2 attempts left"));

        // The third attempt is still honored.
        let (decision, rendered) = decide(None, "wat\nhuh\ny\n");
        assert_eq!(decision, ConfirmationDecision::Proceed);
        assert_eq!(rendered.matches("Proceed? [y/n]").count(), 3);
        assert!(rendered.contains("1 attempt left"));

        // The fourth is not offered.
        let (decision, rendered) = decide(None, "wat\nhuh\neh\ny\n");
        assert_eq!(
            decision,
            ConfirmationDecision::Refuse(RefusalReason::Unanswered)
        );
        assert_eq!(
            rendered.matches("Proceed? [y/n]").count(),
            CONFIRMATION_ATTEMPTS as usize
        );

        // The escalation prompt carries its own allowance rather than sharing the first one.
        let (decision, rendered) = decide(Some("majority"), "wat\ny\nhuh\ny\n");
        assert_eq!(decision, ConfirmationDecision::Proceed);
        assert_eq!(rendered.matches("Are you sure? [y/n]").count(), 2);
    }

    #[test]
    fn a_closed_stream_stops_asking_instead_of_spinning() {
        let mut input = Cursor::new(Vec::<u8>::new());
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

        assert_eq!(
            decision,
            ConfirmationDecision::Refuse(RefusalReason::Unanswered)
        );
        let rendered = String::from_utf8(output).expect("prompt should be UTF-8");
        assert_eq!(rendered.matches("Proceed? [y/n]").count(), 1);
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
