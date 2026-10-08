//! Smart-object stack modes.

use serde::{Deserialize, Serialize};

/// Layer › Smart Objects › Stack Mode: a per-pixel statistic over the smart object's layers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum StackMode {
    Entropy,
    Kurtosis,
    Maximum,
    Mean,
    Median,
    Minimum,
    Range,
    Skewness,
    StandardDeviation,
    Summation,
    Variance,
}

impl StackMode {
    pub const ALL: [StackMode; 11] = [
        StackMode::Entropy,
        StackMode::Kurtosis,
        StackMode::Maximum,
        StackMode::Mean,
        StackMode::Median,
        StackMode::Minimum,
        StackMode::Range,
        StackMode::Skewness,
        StackMode::StandardDeviation,
        StackMode::Summation,
        StackMode::Variance,
    ];

    /// The menu-id suffix (`layer.smartObjects.stackMode.<id>`).
    pub fn id(self) -> &'static str {
        match self {
            StackMode::Entropy => "entropy",
            StackMode::Kurtosis => "kurtosis",
            StackMode::Maximum => "maximum",
            StackMode::Mean => "mean",
            StackMode::Median => "median",
            StackMode::Minimum => "minimum",
            StackMode::Range => "range",
            StackMode::Skewness => "skewness",
            StackMode::StandardDeviation => "standardDeviation",
            StackMode::Summation => "summation",
            StackMode::Variance => "variance",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|m| m.id() == id)
    }

    pub fn label(self) -> &'static str {
        match self {
            StackMode::Entropy => "Entropy",
            StackMode::Kurtosis => "Kurtosis",
            StackMode::Maximum => "Maximum",
            StackMode::Mean => "Mean",
            StackMode::Median => "Median",
            StackMode::Minimum => "Minimum",
            StackMode::Range => "Range",
            StackMode::Skewness => "Skewness",
            StackMode::StandardDeviation => "Standard Deviation",
            StackMode::Summation => "Summation",
            StackMode::Variance => "Variance",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stack_mode_ids_roundtrip() {
        for m in StackMode::ALL {
            assert_eq!(StackMode::from_id(m.id()), Some(m));
        }
    }
}
