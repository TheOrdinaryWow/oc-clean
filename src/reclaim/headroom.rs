use std::io;
use std::num::NonZeroU64;
use std::path::Path;

/// Source of filesystem free-space measurements.
pub trait FreeSpaceProvider {
    /// Returns available bytes on the filesystem containing `directory`.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when the filesystem cannot be queried.
    fn available_space(&self, directory: &Path) -> io::Result<u64>;
}

/// Filesystem-backed free-space provider.
#[derive(Clone, Copy, Debug, Default)]
pub struct Fs2FreeSpaceProvider;

impl FreeSpaceProvider for Fs2FreeSpaceProvider {
    fn available_space(&self, directory: &Path) -> io::Result<u64> {
        fs2::available_space(directory)
    }
}

/// Free-space provider that always returns an injected byte count.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FixedFreeSpaceProvider(pub u64);

impl FreeSpaceProvider for FixedFreeSpaceProvider {
    fn available_space(&self, _directory: &Path) -> io::Result<u64> {
        Ok(self.0)
    }
}

/// Non-negative rational fraction used to calculate the safety margin.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MarginFraction {
    numerator: u64,
    denominator: NonZeroU64,
}

impl MarginFraction {
    /// Default safety margin of ten percent.
    pub const DEFAULT: Self = Self {
        numerator: 1,
        denominator: NonZeroU64::new(10).expect("ten is non-zero"),
    };

    /// Creates a margin from a numerator and a non-zero denominator.
    #[must_use]
    pub const fn new(numerator: u64, denominator: NonZeroU64) -> Self {
        Self {
            numerator,
            denominator,
        }
    }

    fn bytes_for(self, projected_live_bytes: u64) -> io::Result<u64> {
        let product = u128::from(projected_live_bytes) * u128::from(self.numerator);
        let margin = product.div_ceil(u128::from(self.denominator.get()));
        u64::try_from(margin).map_err(|_| arithmetic_overflow())
    }
}

impl Default for MarginFraction {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Inputs shared by the pre-delete and post-delete headroom gates.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HeadroomInput {
    /// Live bytes computed as `(page_count - freelist_count) * page_size`.
    pub current_live_bytes: u64,
    /// Bytes attributed to the sessions selected for deletion.
    pub selected_session_bytes: u64,
    /// Size of the original database copy required by the backup fallback.
    pub full_original_size: u64,
    pub one_batch_wal_allowance: u64,
    pub margin_fraction: MarginFraction,
    pub hardlink_supported: bool,
}

impl HeadroomInput {
    pub const DEFAULT_MARGIN_FRACTION: MarginFraction = MarginFraction::DEFAULT;
}

/// Result of comparing measured free space with the reclaim requirement.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HeadroomVerdict {
    Sufficient,
    InsufficientWithShortfall { shortfall_bytes: u64 },
}

/// Full headroom calculation exposed for reporting and gate decisions.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HeadroomEstimate {
    pub current_live_bytes: u64,
    pub projected_post_delete_live_bytes: u64,
    pub one_batch_wal_allowance: u64,
    pub margin_bytes: u64,
    pub backup_copy_bytes: u64,
    pub required_bytes: u64,
    pub available_bytes: u64,
    pub verdict: HeadroomVerdict,
}

/// Measures filesystem free space and evaluates reclaim headroom.
///
/// # Errors
///
/// Returns an I/O error when free space cannot be measured or when the byte calculation exceeds
/// `u64`.
pub fn evaluate_headroom(
    provider: &impl FreeSpaceProvider,
    database_path: &Path,
    input: HeadroomInput,
) -> io::Result<HeadroomEstimate> {
    let database_directory = database_path
        .parent()
        .filter(|directory| !directory.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let available_bytes = provider.available_space(database_directory)?;
    let projected_post_delete_live_bytes = input
        .current_live_bytes
        .saturating_sub(input.selected_session_bytes);
    let margin_bytes = input
        .margin_fraction
        .bytes_for(projected_post_delete_live_bytes)?;
    let backup_copy_bytes = if input.hardlink_supported {
        0
    } else {
        input.full_original_size
    };
    let required_bytes = projected_post_delete_live_bytes
        .checked_add(backup_copy_bytes)
        .and_then(|required| required.checked_add(input.one_batch_wal_allowance))
        .and_then(|required| required.checked_add(margin_bytes))
        .ok_or_else(arithmetic_overflow)?;
    let verdict = if available_bytes >= required_bytes {
        HeadroomVerdict::Sufficient
    } else {
        HeadroomVerdict::InsufficientWithShortfall {
            shortfall_bytes: required_bytes - available_bytes,
        }
    };

    Ok(HeadroomEstimate {
        current_live_bytes: input.current_live_bytes,
        projected_post_delete_live_bytes,
        one_batch_wal_allowance: input.one_batch_wal_allowance,
        margin_bytes,
        backup_copy_bytes,
        required_bytes,
        available_bytes,
        verdict,
    })
}

fn arithmetic_overflow() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "disk-headroom calculation overflowed u64",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const DATABASE_PATH: &str = "/database-dir/opencode.db";

    fn input() -> HeadroomInput {
        HeadroomInput {
            current_live_bytes: 1_000,
            selected_session_bytes: 400,
            full_original_size: 1_200,
            one_batch_wal_allowance: 100,
            margin_fraction: HeadroomInput::DEFAULT_MARGIN_FRACTION,
            hardlink_supported: true,
        }
    }

    #[test]
    fn p9_refuses_when_projected_rebuild_cannot_fit() {
        let estimate = evaluate_headroom(
            &FixedFreeSpaceProvider(750),
            Path::new(DATABASE_PATH),
            input(),
        )
        .expect("headroom evaluation should succeed");

        assert_eq!(estimate.projected_post_delete_live_bytes, 600);
        assert_eq!(estimate.required_bytes, 760);
        assert_eq!(
            estimate.verdict,
            HeadroomVerdict::InsufficientWithShortfall {
                shortfall_bytes: 10
            }
        );
    }

    #[test]
    fn p9_allows_run_that_fits_only_after_projected_deletion() {
        let estimate = evaluate_headroom(
            &FixedFreeSpaceProvider(800),
            Path::new(DATABASE_PATH),
            input(),
        )
        .expect("headroom evaluation should succeed");

        assert!(estimate.available_bytes < estimate.current_live_bytes);
        assert_eq!(estimate.verdict, HeadroomVerdict::Sufficient);
    }

    #[test]
    fn copy_fallback_budgets_full_original_file() {
        let hardlink = evaluate_headroom(
            &FixedFreeSpaceProvider(u64::MAX),
            Path::new(DATABASE_PATH),
            input(),
        )
        .expect("hardlink estimate should succeed");
        let copy_fallback = evaluate_headroom(
            &FixedFreeSpaceProvider(u64::MAX),
            Path::new(DATABASE_PATH),
            HeadroomInput {
                hardlink_supported: false,
                ..input()
            },
        )
        .expect("copy-fallback estimate should succeed");

        assert_eq!(hardlink.backup_copy_bytes, 0);
        assert_eq!(copy_fallback.backup_copy_bytes, 1_200);
        assert!(copy_fallback.required_bytes > hardlink.required_bytes);
    }

    #[test]
    fn margin_and_wal_allowance_are_configurable() {
        let estimate = evaluate_headroom(
            &FixedFreeSpaceProvider(u64::MAX),
            Path::new(DATABASE_PATH),
            HeadroomInput {
                one_batch_wal_allowance: 37,
                margin_fraction: MarginFraction::new(
                    1,
                    NonZeroU64::new(4).expect("four is non-zero"),
                ),
                ..input()
            },
        )
        .expect("custom estimate should succeed");

        assert_eq!(estimate.one_batch_wal_allowance, 37);
        assert_eq!(estimate.margin_bytes, 150);
        assert_eq!(estimate.required_bytes, 787);
    }
}
