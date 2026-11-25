//! Preset ontology packs: the starting point when a library is created.
//!
//! When a library is created, **nothing is laid down**: 0009 removed the built-in entity classes, 0010 and `#125` removed the seed relations,
//! and 0011 moved `mapped_to` to the semantic layer, so the seeding mechanism has retired entirely. So these packs are not
//! "supplements" but the **only source** of the ontology.
//!
//! The original ten seed relations had no type signatures at all, whereas the extraction prompt supports signatures,
//! e.g. `- buys_from (employee|team → *)`. Without signatures, direction can only be described in prose,
//! and prose cannot constrain direction. Of schema.org's 1,521 properties, 1,488 carry
//! domain + range: the direction is **declared**, not described. See `docs/decisions/0008`.
//!
//! **The files are embedded into the binary, not downloaded at runtime**: the README promises the whole system can run in a fully offline
//! intranet, and runtime fetching would break that promise. The source is stored gzip-compressed (1.7 MB → 316 KB),
//! and decompressed in [`bytes`].

use utopia_core::{AppError, AppResult};

/// An optional preset ontology.
///
/// `classes` / `properties` are **display counts taken on the day of the fetch**, for the library-creation UI;
/// the number actually created is whatever the import returns in its plan: the projection only covers the constructs consumable right now.
pub struct Pack {
    pub id: &'static str,
    pub name: &'static str,
    pub summary: &'static str,
    /// The file name passed to `owl_import`. **The format is determined by the extension** (`RdfFormat::detect`),
    /// so the real suffix must be kept here.
    pub filename: &'static str,
    pub classes: u32,
    pub properties: u32,
    gz: &'static [u8],
}

pub const PACKS: &[Pack] = &[
    Pack {
        id: "schema-org",
        name: "schema.org",
        summary: "People, organizations, products, events, creative works",
        filename: "schema-org.ttl",
        classes: 1010,
        properties: 1676,
        gz: include_bytes!("../packs/schema-org.ttl.gz"),
    },
    Pack {
        id: "w3c-org",
        name: "W3C Org",
        summary: "Departments, posts, memberships, reporting lines",
        filename: "w3c-org.ttl",
        classes: 13,
        properties: 34,
        gz: include_bytes!("../packs/w3c-org.ttl.gz"),
    },
    Pack {
        id: "prov-o",
        name: "PROV-O",
        summary: "Provenance: who produced what, when, from which source",
        filename: "prov-o.ttl",
        classes: 49,
        properties: 69,
        gz: include_bytes!("../packs/prov-o.ttl.gz"),
    },
    Pack {
        id: "foaf",
        name: "FOAF",
        summary: "People and social relations",
        filename: "foaf.rdf",
        classes: 12,
        properties: 62,
        gz: include_bytes!("../packs/foaf.rdf.gz"),
    },
    Pack {
        id: "iof-core",
        name: "IOF Core",
        summary: "Industrial manufacturing",
        filename: "iof-core.rdf",
        classes: 294,
        properties: 75,
        gz: include_bytes!("../packs/iof-core.rdf.gz"),
    },
];

pub fn get(id: &str) -> Option<&'static Pack> {
    PACKS.iter().find(|p| p.id == id)
}

/// Decompress the source. **Decompressed on every call**: library creation is an infrequent action, not worth keeping 1.7 MB resident.
pub fn bytes(pack: &Pack) -> AppResult<Vec<u8>> {
    use std::io::Read;
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(pack.gz)
        .read_to_end(&mut out)
        .map_err(|e| {
            AppError::Other(anyhow::anyhow!(
                "ontology pack {} failed to extract: {e}",
                pack.id
            ))
        })?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every pack can be decompressed, and what comes out is not an empty file.
    /// `include_bytes!` guarantees the file exists, but not that it is a valid gzip.
    #[test]
    fn every_pack_decompresses() {
        for p in PACKS {
            let b = bytes(p).unwrap_or_else(|e| panic!("{}: {e}", p.id));
            assert!(
                b.len() > 10_000,
                "{} extracted to only {} bytes",
                p.id,
                b.len()
            );
        }
    }

    /// The file-name suffix decides the format detection; if it is wrong, the whole pack is fed to the parser as a different syntax.
    #[test]
    fn filenames_carry_a_format_suffix() {
        for p in PACKS {
            assert!(
                p.filename.ends_with(".ttl") || p.filename.ends_with(".rdf"),
                "{} 的文件名没有可判定的后缀：{}",
                p.id,
                p.filename
            );
        }
    }

    #[test]
    fn ids_are_unique() {
        let mut ids: Vec<_> = PACKS.iter().map(|p| p.id).collect();
        ids.sort_unstable();
        let n = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), n, "pack id is duplicated");
    }
}
