//! Disposition of same-name terms between packages.
//!
//! When an import collides on a key, `owl_import`'s default judgment is [`Disposition::KeyTaken`] -- skip and report,
//! with no automatic suffix. That reason ("a reimport cannot tell which one it created last time") targets a suffix that was **guessed**;
//! this module gives a **declared** disposition, so the reimport result is the same and that constraint does not apply.
//!
//! Why it is needed: prepackaged packages collide pairwise in about 20 places, and they fall into two classes --
//!
//! - `org:Organization` and `schema:Organization` are **the same thing**, so skipping is right,
//!   but it should not be reported as a "conflict" that makes the user adjudicate something with nothing to decide
//! - `org:role` (a position within an organization) and `schema:role` (a character an actor plays) are **merely the same name**,
//!   and skipping would throw away the reason W3C Org exists
//!
//! Only covers the prepackaged packages. Vocabularies a user imports by hand are not here -- that is an explicit intent, so a collision should be reported to them.

/// Disposition of same-name terms.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Alignment {
    /// Same meaning: the existing one is it; no need to create again. Skip, but record as "aligned" rather than "conflict"
    SameAs,
    /// Same name, different meaning: create it under this key instead
    Rename(&'static str),
}

/// (incoming IRI, already-occupied IRI) -> disposition.
///
/// Upstream's own declared alignments are copied first: W3C Org declares its correspondence with FOAF,
/// and PROV-O declares correspondences with FOAF and Dublin Core. Here we record only those that actually collide between our packages.
const TABLE: &[(&str, &str, Alignment)] = &[
    // ── W3C Org × schema.org ──────────────────────────────────────────
    (
        "http://www.w3.org/ns/org#Organization",
        "https://schema.org/Organization",
        Alignment::SameAs,
    ),
    (
        "http://www.w3.org/ns/org#identifier",
        "https://schema.org/identifier",
        Alignment::SameAs,
    ),
    (
        "http://www.w3.org/ns/org#location",
        "https://schema.org/location",
        Alignment::SameAs,
    ),
    // org:Role is "the role carried by a position" (paired with Post and Membership);
    // schema:role is the playing-a-part relation in creative works. Same name, unrelated
    (
        "http://www.w3.org/ns/org#Role",
        "https://schema.org/Role",
        Alignment::Rename("org_role"),
    ),
    // org:member is a membership relation with a tenure (concretized via Membership);
    // schema:member is a general membership. The granularity differs, so both are needed
    (
        "http://www.w3.org/ns/org#member",
        "https://schema.org/member",
        Alignment::Rename("org_member"),
    ),
    (
        "http://www.w3.org/ns/org#memberOf",
        "https://schema.org/memberOf",
        Alignment::Rename("org_member_of"),
    ),
    // ── PROV-O × schema.org ───────────────────────────────────────────
    // prov:Agent is the **superclass** of Person and Organization, not a synonym.
    // Judging it SameAs would erase the abstraction layer of "agent" entirely
    (
        "http://www.w3.org/ns/prov#Agent",
        "https://schema.org/agent",
        Alignment::Rename("prov_agent"),
    ),
    // ── FOAF × schema.org ─────────────────────────────────────────────
    (
        "http://xmlns.com/foaf/0.1/Person",
        "https://schema.org/Person",
        Alignment::SameAs,
    ),
    (
        "http://xmlns.com/foaf/0.1/Organization",
        "https://schema.org/Organization",
        Alignment::SameAs,
    ),
    (
        "http://xmlns.com/foaf/0.1/Project",
        "https://schema.org/Project",
        Alignment::SameAs,
    ),
    (
        "http://xmlns.com/foaf/0.1/name",
        "https://schema.org/name",
        Alignment::SameAs,
    ),
    (
        "http://xmlns.com/foaf/0.1/knows",
        "https://schema.org/knows",
        Alignment::SameAs,
    ),
    (
        "http://xmlns.com/foaf/0.1/givenName",
        "https://schema.org/givenName",
        Alignment::SameAs,
    ),
    (
        "http://xmlns.com/foaf/0.1/familyName",
        "https://schema.org/familyName",
        Alignment::SameAs,
    ),
    (
        "http://xmlns.com/foaf/0.1/gender",
        "https://schema.org/gender",
        Alignment::SameAs,
    ),
    (
        "http://xmlns.com/foaf/0.1/logo",
        "https://schema.org/logo",
        Alignment::SameAs,
    ),
    (
        "http://xmlns.com/foaf/0.1/thumbnail",
        "https://schema.org/thumbnail",
        Alignment::SameAs,
    ),
    (
        "http://xmlns.com/foaf/0.1/title",
        "https://schema.org/title",
        Alignment::SameAs,
    ),
    (
        "http://xmlns.com/foaf/0.1/member",
        "https://schema.org/member",
        Alignment::SameAs,
    ),
    // foaf:Agent and prov:Agent are synonyms (PROV-O officially aligns them this way),
    // but neither equals schema:agent
    (
        "http://xmlns.com/foaf/0.1/Agent",
        "https://schema.org/agent",
        Alignment::Rename("foaf_agent"),
    ),
    // foaf:status is the online presence of the instant-messaging era; schema:status is order / action status
    (
        "http://xmlns.com/foaf/0.1/status",
        "https://schema.org/status",
        Alignment::Rename("foaf_status"),
    ),
];

/// Consult this table on a name collision. If neither IRI is in the prepackaged packages, return `None` and take the original `KeyTaken`.
pub fn lookup(incoming_iri: &str, existing_iri: &str) -> Option<Alignment> {
    TABLE
        .iter()
        .find(|(a, b, _)| *a == incoming_iri && *b == existing_iri)
        .map(|(_, _, al)| *al)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Namespace prefixes are used only in tests to build IRIs; the table holds full IRIs,
    // because that is what gets compared character-for-character against projection output, and concatenation would hide typos
    const SCHEMA: &str = "https://schema.org/";
    const ORG: &str = "http://www.w3.org/ns/org#";
    const PROV: &str = "http://www.w3.org/ns/prov#";
    const FOAF: &str = "http://xmlns.com/foaf/0.1/";

    #[test]
    fn same_as_and_rename_are_both_reachable() {
        assert_eq!(
            lookup(
                &format!("{ORG}Organization"),
                &format!("{SCHEMA}Organization")
            ),
            Some(Alignment::SameAs)
        );
        assert_eq!(
            lookup(&format!("{ORG}Role"), &format!("{SCHEMA}Role")),
            Some(Alignment::Rename("org_role"))
        );
    }

    /// Combinations not in the table must fall back to `KeyTaken` -- **the default is to report a conflict, not to guess**
    #[test]
    fn unknown_pairs_fall_through() {
        assert_eq!(
            lookup("http://example.com/a#Foo", &format!("{SCHEMA}Foo")),
            None
        );
        assert_eq!(
            lookup(&format!("{PROV}Entity"), "http://example.com/b#Entity"),
            None
        );
    }

    /// Direction-sensitive: the table is (incoming, already-held), and the reverse lookup finds nothing.
    /// Different package install orders hit different entries, so this must not rely on symmetry to get by
    #[test]
    fn lookup_is_directional() {
        assert!(lookup(
            &format!("{SCHEMA}Organization"),
            &format!("{ORG}Organization")
        )
        .is_none());
    }

    /// A key produced by rename must pass the same constraints as `validate_key`: lowercase alphanumerics and underscores, at most 40
    #[test]
    fn renamed_keys_are_valid() {
        for (_, _, al) in TABLE {
            if let Alignment::Rename(k) = al {
                assert!(k.len() <= 40, "{k} is over 40 characters");
                assert!(
                    k.chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
                    "{k} 含非法字符"
                );
            }
        }
    }

    #[test]
    fn no_duplicate_pairs() {
        let mut seen: Vec<(&str, &str)> = TABLE.iter().map(|(a, b, _)| (*a, *b)).collect();
        let n = seen.len();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(
            seen.len(),
            n,
            "alignment table has a duplicate (incoming, taken) pair"
        );
    }

    /// The FOAF constants are not referenced outside the tests; this confirms they are spelled correctly
    #[test]
    fn foaf_namespace_is_used() {
        assert!(lookup(&format!("{FOAF}Person"), &format!("{SCHEMA}Person")).is_some());
    }
}

/// The IRIs in the alignment table must really appear in the packages.
///
/// The table is hand-written strings; one wrong character silently disables it -- collisions still take `KeyTaken`,
/// only they never match anymore. An upstream prefix change (schema.org moved from `http://` to `https://` once) is the same kind of silent failure.
/// So project the real packages and check each entry.
#[cfg(test)]
mod against_real_packs {
    use super::*;
    use std::collections::HashSet;
    use utopia_ingest::ontology_rdf::{project, RdfFormat};

    /// Project all the prepackaged packages and collect the IRIs that appeared
    fn all_iris() -> HashSet<String> {
        let mut out = HashSet::new();
        for p in crate::ontology_packs::PACKS {
            let bytes = crate::ontology_packs::bytes(p).expect(p.id);
            let fmt = RdfFormat::detect(p.filename, &bytes);
            let proj =
                project(&bytes, fmt).unwrap_or_else(|e| panic!("{} projection failed: {e}", p.id));
            out.extend(proj.classes.iter().map(|c| c.iri.clone()));
            out.extend(proj.properties.iter().map(|p| p.iri.clone()));
        }
        out
    }

    #[test]
    fn every_alignment_iri_exists_in_some_pack() {
        let iris = all_iris();
        let mut missing = Vec::new();
        for (incoming, existing, _) in TABLE {
            if !iris.contains(*incoming) {
                missing.push(*incoming);
            }
            if !iris.contains(*existing) {
                missing.push(*existing);
            }
        }
        assert!(
            missing.is_empty(),
            "对齐表里这些 IRI 在任何预制包里都找不到，表已失效：\n  {}",
            missing.join("\n  ")
        );
    }
}
