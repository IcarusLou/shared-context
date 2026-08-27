use std::{error, fmt, str::FromStr, sync::Arc};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use sha2::{Digest, Sha256};
use uuid::{Uuid, Variant, Version};

/// Error returned when an opaque domain identifier is malformed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IdParseError {
    expected_prefix: &'static str,
    message: String,
}

impl IdParseError {
    fn new(expected_prefix: &'static str, message: impl Into<String>) -> Self {
        Self {
            expected_prefix,
            message: message.into(),
        }
    }

    /// Prefix required by this identifier type.
    #[must_use]
    pub const fn expected_prefix(&self) -> &'static str {
        self.expected_prefix
    }
}

impl fmt::Display for IdParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.expected_prefix.is_empty() {
            return write!(
                formatter,
                "expected a Repository ID of 1..64 ASCII bytes matching [A-Za-z][A-Za-z0-9._-]*: {}",
                self.message
            );
        }
        write!(
            formatter,
            "expected {} followed by a canonical RFC 4122 UUIDv4: {}",
            self.expected_prefix, self.message
        )
    }
}

impl error::Error for IdParseError {}

fn parse_uuid(value: &str, prefix: &'static str) -> Result<Uuid, IdParseError> {
    let Some(raw_uuid) = value.strip_prefix(prefix) else {
        return Err(IdParseError::new(prefix, "prefix does not match"));
    };
    let uuid = Uuid::parse_str(raw_uuid)
        .map_err(|error| IdParseError::new(prefix, format!("invalid UUID: {error}")))?;
    if uuid.get_version() != Some(Version::Random) || uuid.get_variant() != Variant::RFC4122 {
        return Err(IdParseError::new(prefix, "UUID is not version 4"));
    }
    if uuid.hyphenated().to_string() != raw_uuid {
        return Err(IdParseError::new(
            prefix,
            "UUID is not in lowercase hyphenated form",
        ));
    }
    Ok(uuid)
}

macro_rules! opaque_id {
    ($name:ident, $prefix:literal, $description:literal) => {
        #[doc = $description]
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(Uuid);

        impl $name {
            /// Creates a fresh opaque identifier from the operating system CSPRNG.
            #[must_use]
            pub fn new() -> Self {
                Self(Uuid::new_v4())
            }

            /// Returns the UUID portion of the identifier.
            #[must_use]
            pub const fn uuid(self) -> Uuid {
                self.0
            }

            /// Required human-readable prefix.
            pub const PREFIX: &'static str = $prefix;
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(formatter, "{}{}", Self::PREFIX, self.0.hyphenated())
            }
        }

        impl FromStr for $name {
            type Err = IdParseError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                parse_uuid(value, Self::PREFIX).map(Self)
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serializer.collect_str(self)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                value.parse().map_err(de::Error::custom)
            }
        }
    };
}

opaque_id!(SpaceId, "spc_", "Opaque identity of a `ContextSpace`.");

/// Maximum encoded length of one user-visible [`RepositoryId`].
pub const REPOSITORY_ID_MAX_BYTES: usize = 64;

/// JSON Schema-compatible pattern for one user-visible [`RepositoryId`].
pub const REPOSITORY_ID_PATTERN: &str = r"^[A-Za-z][A-Za-z0-9._-]{0,63}$";

/// User-visible, team-stable identity of one logical source repository.
///
/// The exact ASCII spelling is the identity: `Android` and `android` are
/// distinct. Clones share one bounded immutable string allocation.
#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RepositoryId(Arc<str>);

impl RepositoryId {
    /// Creates a unique syntactically valid identity for internal fixtures.
    ///
    /// Product Catalog creation requires an explicit user-supplied identity;
    /// this constructor is retained for deterministic domain seam tests.
    #[must_use]
    pub fn new() -> Self {
        Self(Arc::from(format!("Repository-{}", Uuid::new_v4())))
    }

    /// Returns the exact user-visible spelling.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for RepositoryId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for RepositoryId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("RepositoryId")
            .field(&self.as_str())
            .finish()
    }
}

impl fmt::Display for RepositoryId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for RepositoryId {
    type Err = IdParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let bytes = value.as_bytes();
        if bytes.is_empty() {
            return Err(IdParseError::new("", "value is empty"));
        }
        if bytes.len() > REPOSITORY_ID_MAX_BYTES {
            return Err(IdParseError::new("", "value exceeds 64 bytes"));
        }
        if !bytes[0].is_ascii_alphabetic() {
            return Err(IdParseError::new(
                "",
                "the first byte must be an ASCII letter",
            ));
        }
        if let Some(byte) = bytes[1..]
            .iter()
            .find(|byte| !(byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')))
        {
            return Err(IdParseError::new(
                "",
                format!("byte 0x{byte:02x} is not allowed"),
            ));
        }
        Ok(Self(Arc::from(value)))
    }
}

impl Serialize for RepositoryId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for RepositoryId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(de::Error::custom)
    }
}

opaque_id!(
    RepositoryGroupId,
    "rpg_",
    "Opaque identity of one explicitly configured local repository group."
);
opaque_id!(
    ReferenceId,
    "ref_",
    "Opaque identity of one persistent Engineering Reference observation."
);
opaque_id!(
    TaskId,
    "tsk_",
    "Opaque identity of an Agent engineering task."
);
opaque_id!(
    TaskSessionId,
    "tss_",
    "Opaque identity of one Task's local runtime session."
);
opaque_id!(
    ExternalSessionId,
    "xss_",
    "Opaque local identity of one external Agent session container."
);
opaque_id!(
    TaskIntentRevisionId,
    "tir_",
    "Opaque identity of one immutable Task Intent revision."
);
opaque_id!(
    SignalId,
    "sig_",
    "Opaque identity of one Task Signal record."
);
opaque_id!(
    CaptureId,
    "cap_",
    "Opaque identity of one redacted local Capture record."
);
opaque_id!(
    WorkEpisodeId,
    "wep_",
    "Opaque identity of one Task's aggregated work episode."
);
opaque_id!(
    WorkObservationId,
    "wob_",
    "Opaque identity of one normalized work observation."
);
opaque_id!(
    AgentCheckpointId,
    "ckp_",
    "Opaque identity of one Agent engineering checkpoint."
);
opaque_id!(
    CheckpointClaimId,
    "clm_",
    "Opaque identity of one structured checkpoint claim."
);
opaque_id!(
    CandidateBuildId,
    "bld_",
    "Opaque identity of one automatic Candidate build."
);
opaque_id!(
    SpaceRecommendationId,
    "rec_",
    "Opaque identity of one Candidate Space recommendation."
);

/// Stable grouping key for Candidates produced from one Task Intent revision.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ProposedSpaceGroupKey(Uuid);

impl ProposedSpaceGroupKey {
    pub const PREFIX: &'static str = "psg_";

    /// Derives the stable grouping identity for one exact Task Intent revision.
    #[must_use]
    pub fn from_task_intent(task_id: TaskId, intent_revision_id: TaskIntentRevisionId) -> Self {
        let mut seed = Vec::new();
        for value in [task_id.to_string(), intent_revision_id.to_string()] {
            seed.extend_from_slice(&(value.len() as u64).to_be_bytes());
            seed.extend_from_slice(value.as_bytes());
        }
        let digest = Sha256::digest(seed);
        let mut bytes = [0_u8; 16];
        bytes.copy_from_slice(&digest[..16]);
        bytes[6] = (bytes[6] & 0x0f) | 0x40;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        Self(Uuid::from_bytes(bytes))
    }

    /// Returns the stable UUID portion of the key.
    #[must_use]
    pub const fn uuid(self) -> Uuid {
        self.0
    }

    pub(crate) fn from_stable_seed(seed: &[u8]) -> Self {
        let digest = Sha256::digest(seed);
        let mut bytes = [0_u8; 16];
        bytes.copy_from_slice(&digest[..16]);
        bytes[6] = (bytes[6] & 0x0f) | 0x40;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        Self(Uuid::from_bytes(bytes))
    }
}

impl fmt::Display for ProposedSpaceGroupKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}{}", Self::PREFIX, self.0.hyphenated())
    }
}

impl FromStr for ProposedSpaceGroupKey {
    type Err = IdParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        parse_uuid(value, Self::PREFIX).map(Self)
    }
}

impl Serialize for ProposedSpaceGroupKey {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for ProposedSpaceGroupKey {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::from_str(&value).map_err(de::Error::custom)
    }
}

impl SpaceRecommendationId {
    pub(crate) fn from_stable_seed(seed: &[u8]) -> Self {
        let digest = Sha256::digest(seed);
        let mut bytes = [0_u8; 16];
        bytes.copy_from_slice(&digest[..16]);
        bytes[6] = (bytes[6] & 0x0f) | 0x40;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        Self(Uuid::from_bytes(bytes))
    }
}
opaque_id!(
    CandidateId,
    "cnd_",
    "Opaque identity of an unassigned `ContextCandidate`."
);
opaque_id!(
    SubmissionId,
    "sub_",
    "Opaque identity of one Candidate creation operation."
);
opaque_id!(
    ConfirmationId,
    "cfm_",
    "Opaque identity of one Candidate confirmation fact."
);
opaque_id!(
    SpaceAssociationId,
    "asc_",
    "Opaque identity of one Context-to-Space association snapshot."
);
opaque_id!(ContextId, "ctx_", "Opaque identity of a `ContextItem`.");
opaque_id!(
    RevisionId,
    "rev_",
    "Opaque identity of an Intent or Context revision."
);
opaque_id!(EventId, "evt_", "Opaque identity of an immutable event.");
opaque_id!(
    PublicationId,
    "pub_",
    "Opaque identity of a publication transition."
);
opaque_id!(
    EvidenceId,
    "evd_",
    "Opaque identity of an evidence snapshot."
);
opaque_id!(ReviewId, "rvw_", "Opaque identity of a review record.");
opaque_id!(
    ConflictId,
    "cnf_",
    "Opaque identity of a semantic conflict."
);
opaque_id!(
    ResolutionId,
    "rsl_",
    "Opaque identity of a conflict resolution."
);

#[cfg(test)]
mod tests {
    use std::{collections::HashSet, str::FromStr};

    use uuid::{Variant, Version};

    use super::{
        AgentCheckpointId, CandidateBuildId, CandidateId, CaptureId, CheckpointClaimId,
        ConfirmationId, ConflictId, ContextId, EventId, EvidenceId, ExternalSessionId,
        ProposedSpaceGroupKey, PublicationId, ReferenceId, RepositoryGroupId, RepositoryId,
        ResolutionId, ReviewId, RevisionId, SignalId, SpaceAssociationId, SpaceId,
        SpaceRecommendationId, SubmissionId, TaskId, TaskIntentRevisionId, TaskSessionId,
        WorkEpisodeId, WorkObservationId,
    };

    #[test]
    fn generated_ids_have_the_right_prefix_and_random_uuid_version() {
        let ids: Vec<_> = (0..256).map(|_| EventId::new()).collect();
        let rendered: HashSet<_> = ids.iter().map(ToString::to_string).collect();

        assert_eq!(rendered.len(), ids.len());
        for id in ids {
            assert!(id.to_string().starts_with(EventId::PREFIX));
            assert_eq!(id.uuid().get_version(), Some(Version::Random));
            assert_eq!(id.uuid().get_variant(), Variant::RFC4122);
        }
    }

    #[test]
    fn parsing_requires_the_exact_type_prefix_and_canonical_uuid_v4() {
        let valid = SpaceId::new().to_string();
        assert!(SpaceId::from_str(&valid).is_ok());
        assert!(SpaceId::from_str(&valid.replace("spc_", "ctx_")).is_err());
        assert!(SpaceId::from_str("spc_00000000-0000-1000-8000-000000000000").is_err());
        assert!(SpaceId::from_str("spc_00000000000040008000000000000000").is_err());

        let repository_group = RepositoryGroupId::new().to_string();
        assert!(RepositoryGroupId::from_str(&repository_group).is_ok());
        assert!(RepositoryGroupId::from_str(&repository_group.replace("rpg_", "rpo_")).is_err());
    }

    #[test]
    fn repository_ids_are_bounded_readable_exact_and_legacy_compatible() {
        assert!(
            std::mem::size_of::<RepositoryId>() <= 2 * std::mem::size_of::<usize>(),
            "RepositoryId must remain a compact shared string handle"
        );
        for value in [
            "Android",
            "iOS",
            "FE",
            "server.api_v2",
            "rpo_00000000-0000-4000-8000-000000000901",
        ] {
            let id = RepositoryId::from_str(value).unwrap();
            assert_eq!(id.as_str(), value);
            assert_eq!(id.to_string(), value);
            assert_eq!(serde_json::to_string(&id).unwrap(), format!("\"{value}\""));
            assert_eq!(
                serde_json::from_str::<RepositoryId>(&format!("\"{value}\"")).unwrap(),
                id
            );
        }
        assert_ne!(
            RepositoryId::from_str("Android").unwrap(),
            RepositoryId::from_str("android").unwrap()
        );
        let mut sorted = [
            RepositoryId::from_str("Server").unwrap(),
            RepositoryId::from_str("FE").unwrap(),
            RepositoryId::from_str("Android").unwrap(),
        ];
        sorted.sort();
        assert_eq!(
            sorted.map(|repository_id| repository_id.to_string()),
            ["Android", "FE", "Server"]
        );
        for value in [
            "",
            "1Android",
            "Android/iOS",
            "Android iOS",
            "Android\\iOS",
            "安卓",
        ] {
            assert!(RepositoryId::from_str(value).is_err(), "accepted {value:?}");
        }
        assert!(RepositoryId::from_str(&format!("A{}", "x".repeat(63))).is_ok());
        assert!(RepositoryId::from_str(&format!("A{}", "x".repeat(64))).is_err());
    }

    #[test]
    fn every_domain_id_type_uses_its_stable_prefix_and_uuid_v4() {
        macro_rules! assert_generated_id {
            ($id:expr, $prefix:literal) => {{
                let id = $id;
                assert!(id.to_string().starts_with($prefix));
                assert_eq!(id.uuid().get_version(), Some(Version::Random));
                assert_eq!(id.uuid().get_variant(), Variant::RFC4122);
            }};
        }

        assert_generated_id!(SpaceId::new(), "spc_");
        let repository_id = RepositoryId::new();
        assert!(repository_id.to_string().starts_with("Repository-"));
        assert!(RepositoryId::from_str(repository_id.as_str()).is_ok());
        assert_generated_id!(RepositoryGroupId::new(), "rpg_");
        assert_generated_id!(ReferenceId::new(), "ref_");
        assert_generated_id!(TaskId::new(), "tsk_");
        assert_generated_id!(TaskSessionId::new(), "tss_");
        assert_generated_id!(ExternalSessionId::new(), "xss_");
        assert_generated_id!(TaskIntentRevisionId::new(), "tir_");
        assert_generated_id!(SignalId::new(), "sig_");
        assert_generated_id!(CaptureId::new(), "cap_");
        assert_generated_id!(WorkEpisodeId::new(), "wep_");
        assert_generated_id!(WorkObservationId::new(), "wob_");
        assert_generated_id!(AgentCheckpointId::new(), "ckp_");
        assert_generated_id!(CheckpointClaimId::new(), "clm_");
        assert_generated_id!(CandidateBuildId::new(), "bld_");
        assert_generated_id!(SpaceRecommendationId::new(), "rec_");
        assert_generated_id!(CandidateId::new(), "cnd_");
        assert_generated_id!(SubmissionId::new(), "sub_");
        assert_generated_id!(ConfirmationId::new(), "cfm_");
        assert_generated_id!(SpaceAssociationId::new(), "asc_");
        assert_generated_id!(ContextId::new(), "ctx_");
        assert_generated_id!(RevisionId::new(), "rev_");
        assert_generated_id!(EventId::new(), "evt_");
        assert_generated_id!(PublicationId::new(), "pub_");
        assert_generated_id!(EvidenceId::new(), "evd_");
        assert_generated_id!(ReviewId::new(), "rvw_");
        assert_generated_id!(ConflictId::new(), "cnf_");
        assert_generated_id!(ResolutionId::new(), "rsl_");
    }

    #[test]
    fn proposed_space_group_key_is_stable_per_task_intent_revision() {
        let task_id = TaskId::new();
        let intent_revision_id = TaskIntentRevisionId::new();
        let key = ProposedSpaceGroupKey::from_task_intent(task_id, intent_revision_id);
        assert!(key.to_string().starts_with("psg_"));
        assert_eq!(key.uuid().get_version(), Some(Version::Random));
        assert_eq!(key.uuid().get_variant(), Variant::RFC4122);
        assert_eq!(
            key,
            ProposedSpaceGroupKey::from_task_intent(task_id, intent_revision_id)
        );
        assert_ne!(
            key,
            ProposedSpaceGroupKey::from_task_intent(task_id, TaskIntentRevisionId::new())
        );
        assert_ne!(
            key,
            ProposedSpaceGroupKey::from_task_intent(TaskId::new(), intent_revision_id)
        );
    }
}
