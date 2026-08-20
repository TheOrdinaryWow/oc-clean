use std::{error::Error, fmt, str::FromStr, time::Duration as StdDuration};

const DURATION_GRAMMAR: &str = "expected <integer><unit> with unit D/W/M/Y";
const SIZE_GRAMMAR: &str = "expected <number><unit> with unit MB/GB";
const SECONDS_PER_DAY: u64 = 24 * 60 * 60;

/// A whole number of coarse, fixed-length time units.
///
/// Months and years intentionally use fixed durations: one month is 30 days
/// and one year is 365 days. This type performs no calendar arithmetic.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Duration(StdDuration);

impl Duration {
    #[must_use]
    pub fn as_millis(self) -> u128 {
        self.0.as_millis()
    }
}

impl FromStr for Duration {
    type Err = ArgumentValueError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        if input.is_empty() {
            return Err(duration_error("value is empty"));
        }
        if input.starts_with('-') {
            return Err(duration_error("negative values are unsupported"));
        }
        if input.contains('.') {
            return Err(duration_error("fractional values are unsupported"));
        }
        if !input.is_ascii() {
            return Err(duration_error("value must use ASCII characters only"));
        }

        let (number, unit) = input.split_at(input.len().saturating_sub(1));
        if number.is_empty() || unit.as_bytes()[0].is_ascii_digit() {
            return Err(duration_error("unit is missing"));
        }
        if !number.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(duration_error("value must contain ASCII digits only"));
        }

        let days_per_unit = match unit.as_bytes()[0] {
            b'D' | b'd' => 1,
            b'W' | b'w' => 7,
            b'M' => 30,
            b'Y' | b'y' => 365,
            _ => return Err(duration_error("unit is unsupported")),
        };
        let amount = number
            .parse::<u64>()
            .map_err(|_| duration_error("integer is outside the supported range"))?;
        let seconds = amount
            .checked_mul(days_per_unit)
            .and_then(|days| days.checked_mul(SECONDS_PER_DAY))
            .ok_or_else(|| duration_error("duration is outside the supported range"))?;

        Ok(Self(StdDuration::from_secs(seconds)))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Size(u64);

impl Size {
    #[must_use]
    pub const fn as_bytes(self) -> u64 {
        self.0
    }
}

impl FromStr for Size {
    type Err = ArgumentValueError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        if input.is_empty() {
            return Err(size_error("value is empty"));
        }
        if input.starts_with('-') {
            return Err(size_error("negative values are unsupported"));
        }
        if !input.is_ascii() {
            return Err(size_error("value must use ASCII characters only"));
        }
        if input
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte == b'.')
        {
            return Err(size_error("unit is missing"));
        }

        let Some((number, unit)) = input
            .len()
            .checked_sub(2)
            .map(|unit_start| input.split_at(unit_start))
        else {
            return Err(size_error("unit is missing"));
        };
        let (multiplier, decimal_places) = if unit.eq_ignore_ascii_case("MB") {
            (1_000_000_u64, 6)
        } else if unit.eq_ignore_ascii_case("GB") {
            (1_000_000_000_u64, 9)
        } else {
            return Err(size_error("unit is unsupported"));
        };

        let bytes = parse_decimal_bytes(number, multiplier, decimal_places)?;
        Ok(Self(bytes))
    }
}

fn parse_decimal_bytes(
    number: &str,
    multiplier: u64,
    decimal_places: usize,
) -> Result<u64, ArgumentValueError> {
    let mut parts = number.split('.');
    let whole = parts.next().unwrap_or_default();
    let fraction = parts.next();
    if parts.next().is_some() {
        return Err(size_error("number contains more than one decimal point"));
    }
    if whole.is_empty()
        || !whole.bytes().all(|byte| byte.is_ascii_digit())
        || fraction.is_some_and(|digits| {
            digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit())
        })
    {
        return Err(size_error(
            "number must use ASCII digits with one optional decimal point",
        ));
    }

    let whole_bytes = whole
        .parse::<u64>()
        .map_err(|_| size_error("number is outside the supported range"))?
        .checked_mul(multiplier)
        .ok_or_else(|| size_error("size is outside the supported range"))?;
    let fraction_bytes = match fraction {
        None => 0,
        Some(digits) => decimal_fraction_bytes(digits, decimal_places)?,
    };

    whole_bytes
        .checked_add(fraction_bytes)
        .ok_or_else(|| size_error("size is outside the supported range"))
}

fn decimal_fraction_bytes(digits: &str, decimal_places: usize) -> Result<u64, ArgumentValueError> {
    let significant_digits = digits.trim_end_matches('0');
    if significant_digits.len() > decimal_places {
        return Err(size_error("fraction resolves to less than one byte"));
    }
    if significant_digits.is_empty() {
        return Ok(0);
    }

    let fraction = significant_digits
        .parse::<u64>()
        .map_err(|_| size_error("fraction is outside the supported range"))?;
    let scale = 10_u64.pow(
        u32::try_from(decimal_places - significant_digits.len())
            .map_err(|_| size_error("fraction is outside the supported range"))?,
    );
    fraction
        .checked_mul(scale)
        .ok_or_else(|| size_error("fraction is outside the supported range"))
}

fn duration_error(detail: &str) -> ArgumentValueError {
    ArgumentValueError::InvalidArgument {
        argument: "--older-than",
        reason: format!("{detail}; {DURATION_GRAMMAR}"),
    }
}

fn size_error(detail: &str) -> ArgumentValueError {
    ArgumentValueError::InvalidArgument {
        argument: "--larger-than",
        reason: format!("{detail}; {SIZE_GRAMMAR}"),
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ArgumentValueError {
    InvalidArgument {
        argument: &'static str,
        reason: String,
    },
}

impl fmt::Display for ArgumentValueError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidArgument { argument, reason } => {
                write!(formatter, "invalid value for {argument}: {reason}")
            }
        }
    }
}

impl Error for ArgumentValueError {}

#[cfg(test)]
mod tests {
    use clap::{Arg, ArgAction, Command, error::ErrorKind};

    use super::{ArgumentValueError, Duration, Size};

    const DAY_MILLIS: u128 = 24 * 60 * 60 * 1_000;

    #[test]
    fn duration_accepts_coarse_whole_units() {
        for (input, expected_days) in [
            ("30D", 30),
            ("30d", 30),
            ("4W", 28),
            ("6M", 180),
            ("1Y", 365),
        ] {
            let parsed = input.parse::<Duration>().expect("valid coarse duration");
            assert_eq!(parsed.as_millis(), expected_days * DAY_MILLIS, "{input}");
        }
    }

    #[test]
    fn duration_rejects_values_outside_the_coarse_grammar() {
        for input in ["30", "30s", "30m", "30h", "30days", "-1D", "0.5D", ""] {
            assert!(
                matches!(
                    input.parse::<Duration>(),
                    Err(ArgumentValueError::InvalidArgument {
                        argument: "--older-than",
                        ..
                    })
                ),
                "{input:?}"
            );
        }
    }

    #[test]
    fn duration_months_and_years_use_fixed_day_counts() {
        assert_eq!(
            "6M".parse::<Duration>().expect("valid months").as_millis(),
            180 * DAY_MILLIS
        );
        assert_eq!(
            "1Y".parse::<Duration>().expect("valid year").as_millis(),
            365 * DAY_MILLIS
        );
    }

    #[test]
    fn size_accepts_decimal_si_units() {
        for (input, expected_bytes) in [
            ("100MB", 100_000_000),
            ("1.5GB", 1_500_000_000),
            ("1.5gb", 1_500_000_000),
        ] {
            let parsed = input.parse::<Size>().expect("valid SI size");
            assert_eq!(parsed.as_bytes(), expected_bytes, "{input}");
        }
    }

    #[test]
    fn size_rejects_values_outside_the_decimal_si_grammar() {
        for input in ["100MiB", "100KB", "100", "100B", "-1GB", "1.5.5GB"] {
            assert!(
                matches!(
                    input.parse::<Size>(),
                    Err(ArgumentValueError::InvalidArgument {
                        argument: "--larger-than",
                        ..
                    })
                ),
                "{input}"
            );
        }
    }

    fn clean_command() -> Command {
        Command::new("oc-clean").subcommand(
            Command::new("clean")
                .arg(
                    Arg::new("older-than")
                        .long("older-than")
                        .value_parser(clap::value_parser!(Duration)),
                )
                .arg(
                    Arg::new("dry-run")
                        .long("dry-run")
                        .action(ArgAction::SetTrue),
                ),
        )
    }

    #[test]
    fn clap_accepts_the_duration_newtype_for_clean() {
        let matches = clean_command()
            .try_get_matches_from(["oc-clean", "clean", "--older-than", "30D", "--dry-run"])
            .expect("valid clean arguments");
        let clean = matches.subcommand_matches("clean").expect("clean command");

        assert_eq!(
            clean.get_one::<Duration>("older-than"),
            Some(&"30D".parse::<Duration>().expect("valid duration"))
        );
        assert!(clean.get_flag("dry-run"));
    }

    #[test]
    fn clap_reports_invalid_duration_as_usage_error() {
        let error = clean_command()
            .try_get_matches_from(["oc-clean", "clean", "--older-than", "30m"])
            .expect_err("minutes must be rejected");
        let message = error.to_string();

        assert_eq!(error.kind(), ErrorKind::ValueValidation);
        assert_eq!(error.exit_code(), 2);
        assert!(message.contains("D/W/M/Y"), "{message}");
    }
}
