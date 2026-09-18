//! Checkpoint roles stored by the plugin's timeline index.

use serde::{Deserialize, Serialize};

/// Why a checkpoint observation exists.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointKind {
    /// First complete observation of a source.
    Baseline,
    /// State immediately before a consumer operation.
    Pre,
    /// State immediately after a consumer operation.
    Post,
    /// Consumer-requested observation.
    Manual,
    /// Safety observation before moving the live head backward.
    PreRewind,
    /// Observation created while recovering a source.
    Recovered,
    /// Failed capture; its generation belongs to an earlier observation.
    Failed,
    /// A request whose state did not change.
    Noop,
    /// Observation created by an idle timer.
    Auto,
}

impl CheckpointKind {
    /// SQL filter for user-facing rewind targets. Keep this beside the role
    /// definition so timeline queries do not each repeat the list.
    pub const TARGETS_SQL: &'static str = "'baseline','pre','post','manual','auto'";

    /// Stable value used by existing timeline stores and wire consumers.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Baseline => "baseline",
            Self::Pre => "pre",
            Self::Post => "post",
            Self::Manual => "manual",
            Self::PreRewind => "pre_rewind",
            Self::Recovered => "recovered",
            Self::Failed => "failed",
            Self::Noop => "noop",
            Self::Auto => "auto",
        }
    }

    /// The row's generation is a real state that may be restored.
    #[must_use]
    pub const fn is_restorable(self) -> bool {
        !matches!(self, Self::Failed)
    }

    /// This observation is a user-facing rewind target and branch member.
    #[must_use]
    pub const fn is_target(self) -> bool {
        matches!(
            self,
            Self::Baseline | Self::Pre | Self::Post | Self::Manual | Self::Auto
        )
    }
}

impl std::str::FromStr for CheckpointKind {
    type Err = UnknownCheckpointKind;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "baseline" => Ok(Self::Baseline),
            "pre" => Ok(Self::Pre),
            "post" => Ok(Self::Post),
            "manual" => Ok(Self::Manual),
            "pre_rewind" => Ok(Self::PreRewind),
            "recovered" => Ok(Self::Recovered),
            "failed" => Ok(Self::Failed),
            "noop" => Ok(Self::Noop),
            "auto" => Ok(Self::Auto),
            _ => Err(UnknownCheckpointKind(value.to_owned())),
        }
    }
}

impl std::fmt::Display for CheckpointKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.pad(self.as_str())
    }
}

/// A timeline store contained an unsupported checkpoint role.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[error("unknown checkpoint kind {0}")]
pub struct UnknownCheckpointKind(pub String);

#[cfg(test)]
mod tests {
    use super::CheckpointKind;

    #[test]
    fn storage_values_and_restore_roles_are_stable() {
        let roles = [
            ("baseline", true, true),
            ("pre", true, true),
            ("post", true, true),
            ("manual", true, true),
            ("pre_rewind", true, false),
            ("recovered", true, false),
            ("failed", false, false),
            ("noop", true, false),
            ("auto", true, true),
        ];
        for (value, restorable, target) in roles {
            let Ok(kind) = value.parse::<CheckpointKind>() else {
                unreachable!("known checkpoint role");
            };
            assert_eq!(kind.as_str(), value);
            assert_eq!(kind.is_restorable(), restorable);
            assert_eq!(kind.is_target(), target);
        }
        assert!("future".parse::<CheckpointKind>().is_err());
        let sql_roles = CheckpointKind::TARGETS_SQL
            .split(',')
            .map(|role| {
                role.trim_matches('\'')
                    .parse::<CheckpointKind>()
                    .expect("SQL role")
            })
            .collect::<Vec<_>>();
        assert_eq!(sql_roles.len(), 5);
        assert!(sql_roles.iter().all(|role| role.is_target()));
    }
}
