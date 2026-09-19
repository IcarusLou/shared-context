---
status: accepted
---

# Use readable team-defined repository identities

Repository identities are exact-case ASCII names chosen by the team, such as `Android`, `iOS`, or `FE`, rather than generated UUIDs. This makes EngineeringReferences portable across installations with different checkout paths while deliberately keeping remotes and filesystem topology out of identity; the restricted 64-byte grammar avoids Unicode normalization and path-safety ambiguity, and legacy `rpo_` UUID values remain readable without rewriting append-only history.
