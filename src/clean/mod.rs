mod cleanup;
pub mod command;
mod output;
mod reclaim;
mod selection;
mod signal;

#[cfg(test)]
mod tests;

use std::fmt;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PhaseId {
    P1,
    P2,
    P3,
    P3b,
    P4,
    P5,
    P6,
    P7,
    P8,
    P9,
    P10,
    P11,
    P12,
    P13,
    P14,
    P15,
    P16,
    P17,
    P17b,
    P18,
    P19,
    P20,
    P21,
}

impl fmt::Display for PhaseId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

pub trait PhaseObserver {
    fn entered(&self, phase: PhaseId);
}

pub(super) struct NoopPhaseObserver;

impl PhaseObserver for NoopPhaseObserver {
    fn entered(&self, _phase: PhaseId) {}
}
