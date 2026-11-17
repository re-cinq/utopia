pub(super) fn build_version() -> &'static str {
    baked_or_cargo(option_env!("UTOPIA_VERSION"), env!("CARGO_PKG_VERSION"))
}

// An unset Docker ARG reaches the compiler as an empty string, not as absent.
fn baked_or_cargo<'a>(baked: Option<&'a str>, cargo_version: &'a str) -> &'a str {
    baked.filter(|version| !version.is_empty()).unwrap_or(cargo_version)
}

#[cfg(test)]
mod tests {
    use super::baked_or_cargo;

    #[test]
    fn returns_re_32_when_the_build_baked_in_re_32() {
        assert_eq!(baked_or_cargo(Some("re.32"), "0.1.0"), "re.32");
    }

    #[test]
    fn returns_0_1_0_when_nothing_was_baked_in() {
        assert_eq!(baked_or_cargo(None, "0.1.0"), "0.1.0");
    }

    #[test]
    fn returns_0_1_0_when_the_baked_in_version_is_empty() {
        assert_eq!(baked_or_cargo(Some(""), "0.1.0"), "0.1.0");
    }
}
