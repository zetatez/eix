#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Clone, Copy)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
}

impl Version {
    pub fn parse(text: &str) -> Option<Self> {
        let mut parts = text.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next().unwrap_or("0").parse().ok()?;
        let patch = parts.next().unwrap_or("0").parse().ok()?;
        Some(Self { major, minor, patch })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_three_parts() {
        assert_eq!(
            Version::parse("1.2.3"),
            Some(Version { major: 1, minor: 2, patch: 3 })
        );
    }
}
