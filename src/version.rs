//! Lenient semantic-version parsing: a leading `v` is accepted and `1.2`
//! is read as `1.2.0`.

use semver::Version;

pub fn parse(s: &str) -> Option<Version> {
    let t = s.trim();
    let t = t.strip_prefix('v').or_else(|| t.strip_prefix('V')).unwrap_or(t);
    if let Ok(v) = Version::parse(t) {
        return Some(v);
    }
    let split = t.find(['-', '+']).unwrap_or(t.len());
    let (core, rest) = t.split_at(split);
    let parts: Vec<&str> = core.split('.').collect();
    if parts.is_empty() || parts.len() > 2 || parts.iter().any(|p| p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit())) {
        return None;
    }
    let padded = if parts.len() == 1 { format!("{}.0.0{rest}", parts[0]) } else { format!("{}.{}.0{rest}", parts[0], parts[1]) };
    Version::parse(&padded).ok()
}

/// Same release ignoring build metadata.
pub fn same(a: &Version, b: &Version) -> bool {
    a.major == b.major && a.minor == b.minor && a.patch == b.patch && a.pre == b.pre
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leading_v_and_padding() {
        assert_eq!(parse("v1.0.5").unwrap(), Version::new(1, 0, 5));
        assert_eq!(parse("V2.1").unwrap(), Version::new(2, 1, 0));
        assert_eq!(parse(" 3 ").unwrap(), Version::new(3, 0, 0));
        assert_eq!(parse("1.2.3-rc.1").unwrap().pre.as_str(), "rc.1");
        assert_eq!(parse("1.2-rc1").unwrap().pre.as_str(), "rc1");
        assert!(parse("nightly").is_none());
        assert!(parse("1.2.3.4").is_none());
        assert!(parse("").is_none());
    }

    #[test]
    fn ordering() {
        assert!(parse("1.0.10").unwrap() > parse("1.0.9").unwrap());
        assert!(parse("1.0.5-rc1").unwrap() < parse("1.0.5").unwrap());
        assert!(same(&parse("1.0.5+abc").unwrap(), &parse("v1.0.5").unwrap()));
    }
}
