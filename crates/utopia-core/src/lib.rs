//! utopia-core: domain models, error types and configuration.

pub mod config;
pub mod error;
pub mod models;
pub mod secrets;
pub mod text;

pub use error::{is_deferred, is_terminal, AppError, AppResult, Deferred, Terminal};
pub use text::without_nul;

/// The marker the recall channel leaves in the `reason` of a review pair. The name-vector recall channel (0041 second cut, channel 2) proposes two
/// **different strings**, and this is a different kind of evidence strength from the same-name families (`shared_name|`, `ambiguous_name|`, `namesake_tie|`).
/// The adjudicator reads it, the governance gate checks it, and recall writes it. All of them recognize it from here and none builds its own prefix.
pub mod review_reasons {
    /// `name_vector|<cosine>`: the pair proposed by name-vector recall
    pub const NAME_VECTOR: &str = "name_vector|";

    /// This pair was proposed by name-vector recall (two similar but different strings)
    pub fn similarity_proposed(reason: Option<&str>) -> bool {
        reason.is_some_and(|r| r.starts_with(NAME_VECTOR))
    }

    /// Cosine text recorded by name-vector recall; None when the pair is not of this kind
    pub fn name_vector_cosine(reason: Option<&str>) -> Option<&str> {
        reason?.strip_prefix(NAME_VECTOR)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn only_the_name_vector_prefix_counts() {
            assert!(similarity_proposed(Some("name_vector|0.78")));
            assert_eq!(name_vector_cosine(Some("name_vector|0.78")), Some("0.78"));
            for r in [
                "ambiguous_name|0.41",
                "namesake_tie|0.55",
                "shared_name|张伟",
                "contains",
                "",
            ] {
                assert!(!similarity_proposed(Some(r)), "{r}");
                assert_eq!(name_vector_cosine(Some(r)), None, "{r}");
            }
            assert!(!similarity_proposed(None));
        }
    }
}
