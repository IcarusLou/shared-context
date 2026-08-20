use std::{error, fmt, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
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
opaque_id!(
    TaskId,
    "tsk_",
    "Opaque identity of an Agent engineering task."
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
        ConflictId, ContextId, EventId, EvidenceId, PublicationId, ResolutionId, ReviewId,
        RevisionId, SpaceId, TaskId,
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
        assert_generated_id!(TaskId::new(), "tsk_");
        assert_generated_id!(ContextId::new(), "ctx_");
        assert_generated_id!(RevisionId::new(), "rev_");
        assert_generated_id!(EventId::new(), "evt_");
        assert_generated_id!(PublicationId::new(), "pub_");
        assert_generated_id!(EvidenceId::new(), "evd_");
        assert_generated_id!(ReviewId::new(), "rvw_");
        assert_generated_id!(ConflictId::new(), "cnf_");
        assert_generated_id!(ResolutionId::new(), "rsl_");
    }
}
