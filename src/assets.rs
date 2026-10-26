//! Choosing the right release asset for an install kind and architecture.

use crate::detect::InstallKind;
use crate::release::{Asset, Release};
use std::collections::BTreeMap;

/// Per-kind asset name patterns (`*` and `?` wildcards, case-insensitive,
/// optional `{arch}` placeholder that expands to each alias of the machine's
/// architecture).
#[derive(Debug, Clone)]
pub struct AssetPatterns {
    map: BTreeMap<InstallKind, Vec<String>>,
}

impl Default for AssetPatterns {
    fn default() -> AssetPatterns {
        let mut map = BTreeMap::new();
        map.insert(InstallKind::Pacman, vec!["*.pkg.tar.zst".to_string()]);
        map.insert(InstallKind::Deb, vec!["*.deb".to_string()]);
        map.insert(InstallKind::Rpm, vec!["*.rpm".to_string()]);
        map.insert(InstallKind::AppImage, vec!["*.AppImage".to_string()]);
        map.insert(InstallKind::Tarball, vec!["*.tar.gz".to_string()]);
        AssetPatterns { map }
    }
}

impl AssetPatterns {
    /// Replace the patterns for one kind.
    pub fn set(&mut self, kind: InstallKind, patterns: Vec<String>) {
        self.map.insert(kind, patterns);
    }

    /// Apply a `kind=glob` override as the CLI gives it (`pacman=myapp-*.zst`).
    /// Several overrides for the same kind accumulate.
    pub fn apply_override(&mut self, spec: &str, seen: &mut Vec<InstallKind>) -> Result<(), String> {
        let (k, g) = spec.split_once('=').ok_or_else(|| format!("'{spec}' should look like pacman=GLOB"))?;
        let kind = InstallKind::parse(k).ok_or_else(|| format!("'{k}' is not an install kind (pacman, deb, rpm, appimage, tarball)"))?;
        if g.is_empty() {
            return Err(format!("'{spec}' has an empty pattern"));
        }
        if !seen.contains(&kind) {
            seen.push(kind);
            self.map.insert(kind, Vec::new());
        }
        self.map.get_mut(&kind).expect("just inserted").push(g.to_string());
        Ok(())
    }

    pub fn for_kind(&self, kind: InstallKind) -> &[String] {
        self.map.get(&kind).map(|v| v.as_slice()).unwrap_or(&[])
    }
}

/// Architecture families and the spellings releases use for them.
const FAMILIES: [(&str, &[&str]); 6] = [
    ("x86_64", &["x86_64", "amd64"]),
    ("aarch64", &["aarch64", "arm64"]),
    ("arm", &["armv7", "armhf", "armv6"]),
    ("x86", &["i686", "i386"]),
    ("riscv64", &["riscv64"]),
    ("powerpc64", &["ppc64le", "ppc64el"]),
];

pub fn arch_aliases(arch: &str) -> Vec<&'static str> {
    for (canon, aliases) in FAMILIES {
        if canon == arch || aliases.contains(&arch) {
            return aliases.to_vec();
        }
    }
    Vec::new()
}

/// Case-insensitive glob with `*` and `?`.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.to_lowercase().chars().collect();
    let t: Vec<char> = text.to_lowercase().chars().collect();
    let (mut pi, mut ti, mut star, mut mark) = (0usize, 0usize, None::<usize>, 0usize);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// How well does an asset name fit the architecture? `None` = built for another one.
fn arch_score(name: &str, arch: &str) -> Option<u8> {
    let lower = name.to_lowercase();
    let mine = arch_aliases(arch);
    if mine.iter().any(|a| lower.contains(a)) {
        return Some(2);
    }
    let other = FAMILIES.iter().any(|(canon, aliases)| *canon != arch && !aliases.contains(&arch) && aliases.iter().any(|a| lower.contains(a)));
    if other { None } else { Some(1) }
}

/// Pick the asset for `kind` on `arch`: it must match a pattern; assets built
/// for another architecture are excluded, ones naming this architecture win
/// over ones that name none, ties go to the first name alphabetically.
pub fn select<'a>(release: &'a Release, kind: InstallKind, patterns: &AssetPatterns, arch: &str) -> Option<&'a Asset> {
    let aliases = arch_aliases(arch);
    let globs: Vec<String> = patterns
        .for_kind(kind)
        .iter()
        .flat_map(|p| {
            if p.contains("{arch}") {
                let mut v: Vec<String> = aliases.iter().map(|a| p.replace("{arch}", a)).collect();
                if v.is_empty() {
                    v.push(p.replace("{arch}", arch));
                }
                v
            } else {
                vec![p.clone()]
            }
        })
        .collect();
    release
        .assets
        .iter()
        .filter(|a| globs.iter().any(|g| glob_match(g, &a.name)))
        .filter_map(|a| arch_score(&a.name, arch).map(|s| (s, a)))
        .max_by(|(sa, a), (sb, b)| sa.cmp(sb).then_with(|| b.name.cmp(&a.name)))
        .map(|(_, a)| a)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(names: &[&str]) -> Release {
        Release {
            tag: "v1".into(),
            version: semver::Version::new(1, 0, 0),
            name: None,
            body: String::new(),
            prerelease: false,
            html_url: None,
            published_at: None,
            assets: names.iter().map(|n| Asset { name: n.to_string(), url: format!("https://x/{n}"), size: 1 }).collect(),
        }
    }

    const MYAPP: [&str; 12] = [
        "myapp-x86_64.pkg.tar.zst",
        "myapp-x86_64.pkg.tar.zst.sha256",
        "myapp-amd64.deb",
        "myapp-amd64.deb.sha256",
        "myapp-x86_64.rpm",
        "myapp-x86_64.rpm.sha256",
        "myapp-x86_64.AppImage",
        "myapp-x86_64.AppImage.sha256",
        "myapp-linux-x86_64.tar.gz",
        "myapp-linux-x86_64.tar.gz.sha256",
        "SHA256SUMS",
        "notes.txt",
    ];

    #[test]
    fn myapp_assets_per_kind() {
        let r = release(&MYAPP);
        let p = AssetPatterns::default();
        let want = [
            (InstallKind::Pacman, "myapp-x86_64.pkg.tar.zst"),
            (InstallKind::Deb, "myapp-amd64.deb"),
            (InstallKind::Rpm, "myapp-x86_64.rpm"),
            (InstallKind::AppImage, "myapp-x86_64.AppImage"),
            (InstallKind::Tarball, "myapp-linux-x86_64.tar.gz"),
        ];
        for (kind, name) in want {
            assert_eq!(select(&r, kind, &p, "x86_64").map(|a| a.name.as_str()), Some(name), "{kind}");
        }
    }

    #[test]
    fn architecture_is_respected() {
        let r = release(&["app-x86_64.AppImage", "app-aarch64.AppImage", "app-amd64.deb", "app-arm64.deb", "app-1.0.tar.gz"]);
        let p = AssetPatterns::default();
        assert_eq!(select(&r, InstallKind::AppImage, &p, "aarch64").unwrap().name, "app-aarch64.AppImage");
        assert_eq!(select(&r, InstallKind::AppImage, &p, "x86_64").unwrap().name, "app-x86_64.AppImage");
        assert_eq!(select(&r, InstallKind::Deb, &p, "aarch64").unwrap().name, "app-arm64.deb");
        // An arch-less asset is acceptable; an other-arch one never is.
        assert_eq!(select(&r, InstallKind::Tarball, &p, "aarch64").unwrap().name, "app-1.0.tar.gz");
        assert!(select(&release(&["app-x86_64.rpm"]), InstallKind::Rpm, &p, "aarch64").is_none());
    }

    #[test]
    fn custom_patterns_and_placeholder() {
        let r = release(&["a-x86_64.AppImage", "b-x86_64.AppImage"]);
        let mut p = AssetPatterns::default();
        let mut seen = Vec::new();
        p.apply_override("appimage=b-{arch}.AppImage", &mut seen).unwrap();
        assert_eq!(select(&r, InstallKind::AppImage, &p, "x86_64").unwrap().name, "b-x86_64.AppImage");
        assert!(p.apply_override("nonsense", &mut seen).is_err());
        assert!(p.apply_override("zip=*.zip", &mut seen).is_err());
    }

    #[test]
    fn globbing() {
        assert!(glob_match("*.AppImage", "x.appimage"));
        assert!(glob_match("a?c*", "abcdef"));
        assert!(!glob_match("*.deb", "x.deb.sha256"));
        assert!(glob_match("*", ""));
    }
}
