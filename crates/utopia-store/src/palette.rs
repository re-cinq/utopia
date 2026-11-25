//! Colour palette for entity classes: a Morandi palette, plus a deterministic function that **picks a colour by key**.
//!
//! ## Why this exists
//!
//! Previously every automatically created class got the same `#8ea5bd` — imported ones, ones built by type resolution,
//! ones laid down when a KB is created, all that same grey-blue. The product has a curated palette, but it is **only reachable when a person manually
//! opens the colour picker**. So: install schema.org (1010 classes) and
//! you get 1010 classes sharing one colour, and a grey graph.
//!
//! The capability has always been there, it just was not wired into the automatic path. This module is that piece of wiring.
//!
//! ## Why a hash rather than round-robin
//!
//! Round-robin (the nth class takes the nth colour) needs a counter to maintain, and classes are created from several paths
//! (manual, import, resolution, KB creation), so the counter would have to be shared across paths — a state that can drift out of sync.
//! Hashing by key needs no state: **the same key is always the same colour**, whoever created it,
//! whichever number it was, however many times it was rebuilt. Re-importing an ontology does not change colours.
//!
//! ## Why not `DefaultHasher`
//!
//! The seed of `std::collections::hash_map::DefaultHasher` is **different in every process**
//! (to guard against hash-collision attacks). Colouring with it means a class changes colour every time the server restarts —
//! and colour is how users recognise things. So here a FNV-1a is hand-written: fixed constants,
//! fixed results, identical across processes and machines.

/// The entity-class palette. **This set is pre-existing and was not replaced in this change** — a Morandi palette was tried and rejected
/// on a real graph: low saturation with mid lightness was tuned for paper, and against a near-black canvas the whole thing turns grey,
/// with classes indistinguishable from each other. A dark background needs colours whose saturation holds up.
///
/// **Changing this must be matched by changing `ENTITY_PALETTE` in `web/src/palette.ts`**:
/// colours picked by hand and colours picked automatically must come from the same set, otherwise a graph would show two colour schemes.
/// A test guards this (see the end of this file), and it fails if the change is missed.
pub const ENTITY_PALETTE: &[&str] = &[
    "#7fd0ff", "#5fa8ff", "#5fd4d0", "#63e2b7", "#4cc38a", "#a8d878", "#ffd479", "#f2b66d",
    "#ff9d76", "#ff8a9e", "#ff9daf", "#e797d8", "#c4a5ff", "#9fa8ff", "#8ea5bd", "#b3b9c4",
];

/// The shape of a class: **square = declared by a vocabulary, circle = grown from the corpus**.
///
/// Previously every automatically created class was hardcoded to `circle` — as with colour, the capability was there (the canvas has
/// `NodeSquareShellProgram`, and the legend changes to square as well), it just nobody supplied a value.
///
/// **Why not hash it the way colour is hashed**: shape has only two values, so a hash would be random —
/// whether `person` is square or circle means nothing; it would be noise. Shape is sparse and eye-catching,
/// so it should carry a real distinction.
///
/// **Why an IRI rather than "top-level class"**: "top-level class" sounds more natural, but many KBs' classes are
/// flat (ones created by resolution have no parent class), which would turn everything into "all squares",
/// merely flipping the problem over. Whether an IRI exists is **deterministic and known on the spot**:
/// imported vocabularies carry IRIs, and ones grown from the corpus do not.
///
/// This is also what the product has been talking about all along — **making clear where a thing came from**.
/// On one graph you can tell at a glance "this is a class declared in the ontology" from "this is a class grown from the documents".
pub fn shape_for(iri: &str) -> &'static str {
    if iri.trim().is_empty() {
        "circle"
    } else {
        "square"
    }
}

/// A class's key → colour. The same key always gets the same colour.
///
/// FNV-1a, constants fixed. Do not swap in `DefaultHasher` — it reseeds per process,
/// so colours all change on every server restart, and users recognise things by colour.
pub fn color_for_key(key: &str) -> &'static str {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325; // FNV offset basis
    for b in key.as_bytes() {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3); // FNV prime
    }
    // **Avalanche mixing**: plain FNV followed by modulo clusters — measured: person/project/research_lab
    // collide into the same bucket, and eight common keys spread over only five colours. This step mixes the high bits into the low bits,
    // and the same eight keys then spread out. (The palette length is not prime, so the low bits alone carry little information)
    hash ^= hash >> 33;
    hash = hash.wrapping_mul(0xff51_afd7_ed55_8ccd);
    hash ^= hash >> 33;
    ENTITY_PALETTE[(hash % ENTITY_PALETTE.len() as u64) as usize]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The same key always gets the same colour** — across calls and across processes.
    /// A few concrete values are pinned here: change the hash implementation and every existing KB's colours jump at once.
    #[test]
    fn the_same_key_always_gets_the_same_colour() {
        for _ in 0..3 {
            assert_eq!(color_for_key("organization"), color_for_key("organization"));
        }
        // Pin the concrete results: not because these few values are meant to look nice,
        // but so that "changing the implementation = every existing KB's colours change" must be an explicit decision
        assert_eq!(color_for_key("person"), "#ff9d76");
        assert_eq!(color_for_key("organization"), "#e797d8");
        assert_eq!(color_for_key("product"), "#f2b66d");
    }

    /// Adjacent keys should not collide into the same colour — otherwise on one graph "person" and "product" cannot be told apart.
    /// Global collision-freedom is not guaranteed (a dozen-odd colours cannot hold thousands of classes), but the common few must spread out.
    #[test]
    fn the_common_keys_spread_across_the_palette() {
        let keys = [
            "person",
            "organization",
            "product",
            "event",
            "place",
            "document",
            "project",
            "team",
        ];
        let colours: std::collections::HashSet<_> = keys.iter().map(|k| color_for_key(k)).collect();
        assert!(
            colours.len() >= 6,
            "八个常见 key 只散到 {} 个色：{colours:?}",
            colours.len()
        );
    }

    /// The shape carries provenance: vocabulary-declared ones are square, corpus-grown ones are circles.
    #[test]
    fn the_shape_says_where_the_class_came_from() {
        assert_eq!(shape_for("https://schema.org/Person"), "square");
        assert_eq!(shape_for(""), "circle");
        assert_eq!(
            shape_for("   "),
            "circle",
            "whitespace also counts as no IRI"
        );
    }

    /// **The front end's colorForKey must match this one bit for bit.**
    ///
    /// When a new class is created, the front end first picks a colour from the key to display, and if the user does not change it, that is what gets saved;
    /// whereas the import/resolution path is computed on the back end. If the two compute differently, the same key gets a different colour
    /// depending on "who created it" — and this produces **no error whatsoever**.
    ///
    /// This can only check that the front-end copy exists and its structure is right; numerical agreement is guaranteed by
    /// "the same set of constants + the same operations", and each side's comment points to the other.
    /// Keys containing CJK characters need particular care: the JS side must iterate over UTF-8 bytes (TextEncoder),
    /// iterating over char codes gives a different result.
    #[test]
    fn the_frontend_has_a_matching_hash() {
        let ts = include_str!("../../../web/src/palette.ts");
        assert!(
            ts.contains("export function colorForKey"),
            "前端缺 colorForKey——新建类的颜色就会跟后端对不上"
        );
        // Getting any one of these three wrong silently shifts the computed colour
        assert!(
            ts.contains("0xcbf29ce484222325n"),
            "FNV initial value is wrong"
        );
        assert!(ts.contains("0x100000001b3n"), "FNV prime is wrong");
        assert!(
            ts.contains("0xff51afd7ed558ccdn"),
            "avalanche mixing constant is wrong"
        );
        assert!(
            ts.contains("TextEncoder"),
            "必须按 UTF-8 字节遍历：按 char code 走，中文 key 会算出别的颜色"
        );
    }

    /// **The front-end and back-end palettes must be the same set.**
    ///
    /// Hand-picked colours come from the front end's `ENTITY_PALETTE`, automatically picked ones from here. If the two drift apart,
    /// one graph ends up with two colour schemes, and no compile-time check speaks up —
    /// it is the same kind of error as `sources::KINDS` falling out of sync between front and back end (that time the symptom was that
    /// the UI let you select it but creation failed with "invalid kind"; only an end-to-end run would hit it)
    #[test]
    fn the_frontend_palette_matches_this_one() {
        let ts = include_str!("../../../web/src/palette.ts");
        let start = ts
            .find("export const ENTITY_PALETTE")
            .expect("frontend cannot find ENTITY_PALETTE");
        let body = &ts[start..start + ts[start..].find("];").expect("palette has no end") + 2];
        let front: Vec<&str> = body
            .lines()
            .filter_map(|l| {
                let t = l.trim().trim_end_matches(',').trim_matches('"');
                t.starts_with('#').then_some(t)
            })
            .collect();
        assert_eq!(
            front, ENTITY_PALETTE,
            "前后端色板不一致——改了一边就要改另一边"
        );
    }
}
