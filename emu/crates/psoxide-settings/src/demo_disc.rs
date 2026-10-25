//! Telling the PSoXide Demo Disc's releases apart by name.
//!
//! The disc is published as `PSoXide Demo Disc v0.48` and kept next to its
//! builds for the console: `... v0.48 Private` and `... v0.48 Private DIAG`.
//! A games folder can hold several of them at once. The shelf shows the newest
//! public release as the Demo Disc and files the rest after it, so it has to
//! know which name is which. Pure string work, shared by the desktop and the
//! tests.

use std::cmp::Ordering;

/// The name every release of the disc starts with.
pub const DEMO_DISC_NAME: &str = "PSoXide Demo Disc";

/// Which build of a release a name describes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[non_exhaustive]
pub enum Edition {
    /// The release anyone can download.
    Public,
    /// A build with the private extras, for the owner's console.
    Private,
    /// A build that prints diagnostics, for chasing a console bug.
    Diagnostic,
    /// Any other tag (a copy, a beta): never taken for the public release.
    Other,
}

/// A demo disc file name, taken apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct DemoDisc {
    /// `(major, minor)` of `vMAJOR.MINOR`, when the name has one.
    pub version: Option<(u32, u32)>,
    /// Which build this is.
    pub edition: Edition,
}

impl DemoDisc {
    /// Read a title such as `PSoXide Demo Disc v0.48 Private DIAG`. `None`
    /// when the title is not a demo disc.
    ///
    /// Anything after the version that is not a tag this module knows makes
    /// the build non-public, so an odd copy never takes the Demo Disc's place.
    #[must_use]
    pub fn parse(title: &str) -> Option<Self> {
        let rest = strip_prefix_ignore_case(title.trim(), DEMO_DISC_NAME)?;
        if !(rest.is_empty() || rest.starts_with(char::is_whitespace)) {
            return None;
        }
        let mut version = None;
        let mut edition = Edition::Public;
        for word in rest.split_whitespace() {
            if version.is_none() {
                if let Some(v) = parse_version(word) {
                    version = Some(v);
                    continue;
                }
            }
            let word = word.to_ascii_lowercase();
            edition = edition.max(match word.as_str() {
                "private" => Edition::Private,
                "diag" | "diagnostic" => Edition::Diagnostic,
                // A tag nobody knows: not the public release.
                _ => Edition::Other,
            });
        }
        Some(Self { version, edition })
    }

    /// Whether this is the public release.
    #[must_use]
    pub fn is_public(&self) -> bool {
        self.edition == Edition::Public
    }

    /// A short phrase for the shelf, such as `v0.48, private diagnostic build`.
    #[must_use]
    pub fn describe(&self) -> String {
        let build = match self.edition {
            Edition::Public => "public release",
            Edition::Private => "private build",
            Edition::Diagnostic => "private diagnostic build",
            Edition::Other => "other build",
        };
        match self.version {
            Some((major, minor)) => format!("v{major}.{minor}, {build}"),
            None => build.to_string(),
        }
    }

    /// A name short enough for a case in the grid: `Demo Disc v0.47 Private`. `None` for a
    /// build with a tag this module does not know, which keeps its full name.
    #[must_use]
    pub fn short_title(&self) -> Option<String> {
        let tag = match self.edition {
            Edition::Public => "",
            Edition::Private => " Private",
            Edition::Diagnostic => " DIAG",
            Edition::Other => return None,
        };
        Some(match self.version {
            Some((major, minor)) => format!("Demo Disc v{major}.{minor}{tag}"),
            None => format!("Demo Disc{tag}"),
        })
    }

    /// Newest first: higher versions sort before lower ones, a name without a
    /// version last, and within one version the public release before the others.
    #[must_use]
    pub fn newest_first(&self, other: &Self) -> Ordering {
        other
            .version
            .cmp(&self.version)
            .then(self.edition.cmp(&other.edition))
    }
}

/// The index of the Demo Disc among titles: the newest public release.
/// `None` when no title is a public demo disc.
#[must_use]
pub fn newest_public<'a>(titles: impl IntoIterator<Item = &'a str>) -> Option<usize> {
    titles
        .into_iter()
        .enumerate()
        .filter_map(|(i, t)| {
            DemoDisc::parse(t)
                .filter(DemoDisc::is_public)
                .map(|d| (i, d))
        })
        .min_by(|(_, a), (_, b)| a.newest_first(b))
        .map(|(i, _)| i)
}

fn strip_prefix_ignore_case<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let head = text.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &text[prefix.len()..])
}

/// `v0.48` as `(0, 48)`.
fn parse_version(word: &str) -> Option<(u32, u32)> {
    let digits = word.strip_prefix(['v', 'V'])?;
    let (major, minor) = digits.split_once('.')?;
    Some((major.parse().ok()?, minor.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(title: &str) -> DemoDisc {
        DemoDisc::parse(title).expect("a demo disc")
    }

    #[test]
    fn names_are_taken_apart() {
        assert_eq!(
            parse("PSoXide Demo Disc v0.48"),
            DemoDisc {
                version: Some((0, 48)),
                edition: Edition::Public
            }
        );
        assert_eq!(
            parse("PSoXide Demo Disc v0.47 Private").edition,
            Edition::Private
        );
        let diag = parse("PSoXide Demo Disc v0.48 Private DIAG");
        assert_eq!(diag.version, Some((0, 48)));
        assert_eq!(diag.edition, Edition::Diagnostic);
        assert_eq!(parse("PSoXide Demo Disc").version, None);
        assert!(parse("psoxide demo disc v1.2").is_public());
    }

    #[test]
    fn other_games_are_not_demo_discs() {
        assert_eq!(DemoDisc::parse("PSoXide Arcade (psoxide-arcade)"), None);
        assert_eq!(DemoDisc::parse("PSoXide Demo Discount"), None);
        assert_eq!(DemoDisc::parse("Quake (quake-psx)"), None);
        assert_eq!(DemoDisc::parse(""), None);
    }

    #[test]
    fn an_unknown_tag_is_never_the_public_release() {
        assert!(!parse("PSoXide Demo Disc v0.49 beta").is_public());
        assert_eq!(parse("PSoXide Demo Disc v0.49 beta").short_title(), None);
        assert!(!parse("PSoXide Demo Disc v0.49 (copy)").is_public());
    }

    #[test]
    fn versions_compare_as_numbers() {
        let titles = ["PSoXide Demo Disc v0.9", "PSoXide Demo Disc v0.10"];
        assert_eq!(newest_public(titles), Some(1));
    }

    #[test]
    fn the_newest_public_release_wins_over_newer_private_builds() {
        let titles = [
            "PSoXide Demo Disc v0.48 Private DIAG",
            "PSoXide Demo Disc v0.47",
            "Quake (quake-psx)",
            "PSoXide Demo Disc v0.45",
            "PSoXide Demo Disc v0.47 Private DIAG",
        ];
        assert_eq!(newest_public(titles), Some(1));
        assert_eq!(newest_public(["PSoXide Demo Disc v0.48 Private"]), None);
        assert_eq!(newest_public(["Quake"]), None);
    }

    #[test]
    fn short_titles_keep_the_version_and_the_tag() {
        let short = |t: &str| parse(t).short_title();
        assert_eq!(
            short("PSoXide Demo Disc v0.47").as_deref(),
            Some("Demo Disc v0.47")
        );
        assert_eq!(
            short("PSoXide Demo Disc v0.47 Private").as_deref(),
            Some("Demo Disc v0.47 Private")
        );
        assert_eq!(
            short("PSoXide Demo Disc v0.48 Private DIAG").as_deref(),
            Some("Demo Disc v0.48 DIAG")
        );
        assert_eq!(short("PSoXide Demo Disc").as_deref(), Some("Demo Disc"));
    }

    #[test]
    fn the_rest_sort_newest_first_public_before_private() {
        let mut rest = [
            parse("PSoXide Demo Disc v0.45"),
            parse("PSoXide Demo Disc v0.47 Private DIAG"),
            parse("PSoXide Demo Disc v0.48 Private DIAG"),
            parse("PSoXide Demo Disc v0.47 Private"),
            parse("PSoXide Demo Disc v0.47"),
        ];
        rest.sort_by(DemoDisc::newest_first);
        let described: Vec<String> = rest.iter().map(DemoDisc::describe).collect();
        assert_eq!(
            described,
            [
                "v0.48, private diagnostic build",
                "v0.47, public release",
                "v0.47, private build",
                "v0.47, private diagnostic build",
                "v0.45, public release",
            ]
        );
    }
}
