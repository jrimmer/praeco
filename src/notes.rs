//! Release notes: the release body minus the install boilerplate.

/// Default marker: a line starting with this begins the install section.
pub const DEFAULT_MARKER: &str = "## Install";

/// Cut the body at the first line that starts with `marker`, drop a trailing
/// horizontal rule left behind, and normalise line endings. An empty marker
/// keeps the whole body.
pub fn strip_install_section(body: &str, marker: &str) -> String {
    let body = body.replace("\r\n", "\n");
    let mut kept: Vec<&str> = Vec::new();
    for line in body.lines() {
        if !marker.is_empty() && line.starts_with(marker) {
            break;
        }
        kept.push(line);
    }
    while let Some(last) = kept.last() {
        let t = last.trim();
        if t.is_empty() || (t.len() >= 3 && t.bytes().all(|b| b == b'-' || b == b'*' || b == b'_')) {
            kept.pop();
        } else {
            break;
        }
    }
    kept.join("\n").trim_start_matches('\n').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_install_steps() {
        let body = "### Fixed\n- a thing\n\n## Install\n\nrun pacman -U\n";
        assert_eq!(strip_install_section(body, DEFAULT_MARKER), "### Fixed\n- a thing");
    }

    #[test]
    fn keeps_body_without_marker() {
        assert_eq!(strip_install_section("just notes\n", DEFAULT_MARKER), "just notes");
    }

    #[test]
    fn custom_marker_and_rule_and_crlf() {
        let body = "Notes\r\n\r\n---\r\n## How to install\r\nsteps";
        assert_eq!(strip_install_section(body, "## How to"), "Notes");
        assert_eq!(strip_install_section(body, ""), "Notes\n\n---\n## How to install\nsteps");
    }

    #[test]
    fn marker_only_at_line_start() {
        let body = "text mentioning ## Install inline\nmore";
        assert_eq!(strip_install_section(body, DEFAULT_MARKER), body);
    }
}
