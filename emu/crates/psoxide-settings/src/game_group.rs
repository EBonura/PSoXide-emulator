//! Which files of a games folder are games.
//!
//! A scan finds files; the shelf shows games. A rip of a multi-track disc is a
//! cue sheet plus one `.bin` per track (`Game (Track 1).bin`, `Game (Track
//! 2).bin`, ...), a multi-disc game is several such sets (or a `.m3u`
//! playlist naming them), and none of those pieces is a game of its own. This
//! module turns the flat list of files into one [`Game`] per game, the same
//! way for the desktop scan and the browser's folder picker.
//!
//! The rules, in order:
//!
//! 1. Every file a `.cue` names is a part of that cue, and every disc a
//!    `.m3u` names is a part of that playlist. Parts are never shown.
//! 2. A cue is a game when its first (data) file is in the list. A cue whose
//!    files were deleted is not.
//! 3. A `.bin` nothing names is shown once. When several share a
//!    `(Track N)` name, only the lowest track is shown, since it holds the
//!    data.
//! 4. Games that differ only by `(Disc N)` and have at least two distinct
//!    disc numbers become one game that starts at the lowest disc.
//!
//! The functions are pure (no file system access) so the browser and the
//! tests use the same code as the desktop.

use std::collections::{BTreeMap, HashMap, HashSet};

/// One file the scan found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct FoundFile {
    /// Path with `/` separators (relative to the scanned folder, or absolute).
    pub path: String,
    /// For a `.cue` or `.m3u`: the files it names, as written inside it
    /// (relative to the sheet's folder) or already resolved to full paths.
    pub refs: Vec<String>,
}

impl FoundFile {
    /// A file with nothing listed inside it.
    #[must_use]
    pub fn plain(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            refs: Vec::new(),
        }
    }

    /// A cue sheet or playlist and the files it names.
    #[must_use]
    pub fn sheet(path: impl Into<String>, refs: Vec<String>) -> Self {
        Self {
            path: path.into(),
            refs,
        }
    }
}

/// One game to show.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Game {
    /// The file that starts the game where discs are opened through sheets
    /// (the desktop): the cue, playlist, `.ccd`, `.iso`, `.bin` or `.exe` of
    /// the first disc.
    pub entry: String,
    /// The file that holds the data track of the first disc: what the browser
    /// opens, since it reads one image file at a time. Equals [`Self::entry`]
    /// when the entry is itself an image.
    pub data: String,
    /// The name to show: the file name without extension, without the
    /// `(Track N)` tag and, for several discs, without the `(Disc N)` tag.
    pub title: String,
    /// Every disc of the game, entries in disc order (one for most games).
    pub discs: Vec<String>,
}

/// File name without folders.
fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Folder part of a path, without the trailing slash (`""` at the top).
fn dir_of(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(dir, _)| dir)
}

/// Lower-case extension, `""` when there is none.
fn ext_of(path: &str) -> String {
    let name = file_name(path);
    name.rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default()
}

/// File name without folders and without the extension.
#[must_use]
pub fn stem_of(path: &str) -> &str {
    let name = file_name(path);
    name.rsplit_once('.').map_or(name, |(s, _)| s)
}

/// Join a sheet's folder and a name written inside the sheet, resolving `.`,
/// `..` and backslashes. A name that is already an absolute path stays as it
/// is.
fn resolve(dir: &str, name: &str) -> String {
    let name = name.replace('\\', "/");
    let absolute = name.starts_with('/') || name.as_bytes().get(1) == Some(&b':');
    let joined = if absolute || dir.is_empty() {
        name
    } else {
        format!("{dir}/{name}")
    };
    let mut parts: Vec<&str> = Vec::new();
    for part in joined.split('/') {
        match part {
            "." => {}
            ".." if parts.last().is_some_and(|p| !p.is_empty() && *p != "..") => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    parts.join("/")
}

/// The `(Track N)` tag at the end of a name, as the track number.
/// `Game (Track 04)` is `Some(4)`; a name without the tag is `None`.
#[must_use]
pub fn track_number(stem: &str) -> Option<u32> {
    let (_, tag) = split_tag(stem, "Track")?;
    Some(tag)
}

/// The name without a trailing `(Track N)` tag.
#[must_use]
pub fn strip_track(stem: &str) -> &str {
    split_tag(stem, "Track").map_or(stem, |(base, _)| base)
}

/// `(Disc N)` anywhere in a name: its number and the name without the tag.
fn disc_tag(stem: &str) -> Option<(u32, String)> {
    let start = stem.find("(Disc ")?;
    let rest = &stem[start + "(Disc ".len()..];
    let end = rest.find(')')?;
    let number: u32 = rest[..end].trim().parse().ok()?;
    let mut base = String::with_capacity(stem.len());
    base.push_str(stem[..start].trim_end());
    let tail = rest[end + 1..].trim_start();
    if !base.is_empty() && !tail.is_empty() {
        base.push(' ');
    }
    base.push_str(tail);
    Some((number, base))
}

/// Split a name that ends in `(<word> N)` into the part before and N.
fn split_tag<'a>(stem: &'a str, word: &str) -> Option<(&'a str, u32)> {
    let stem = stem.trim_end();
    let open = stem.rfind('(')?;
    let inner = stem.get(open + 1..)?.strip_suffix(')')?;
    let number = inner.strip_prefix(word)?.trim();
    if number.is_empty() || !number.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((stem[..open].trim_end(), number.parse().ok()?))
}

/// The name to show for a disc file: its stem without a `(Track N)` tag.
#[must_use]
pub fn display_title(path: &str) -> String {
    strip_track(stem_of(path)).trim().to_string()
}

/// Extensions of files that can start a game.
fn is_game_ext(ext: &str) -> bool {
    matches!(ext, "cue" | "m3u" | "ccd" | "iso" | "bin" | "exe")
}

/// One candidate before the disc pass.
struct Candidate {
    entry: String,
    data: String,
    title: String,
}

/// The games of a list of files, in the order of their titles.
#[must_use]
pub fn group_games(files: &[FoundFile]) -> Vec<Game> {
    // Case-insensitive index: a cue sheet is often written with a different
    // case than the file on a case-sensitive disk, and macOS/Windows do not
    // care.
    let index: HashMap<String, &FoundFile> = files
        .iter()
        .map(|f| (f.path.to_ascii_lowercase(), f))
        .collect();
    let find = |path: &str| index.get(&path.to_ascii_lowercase()).copied();

    let mut hidden: HashSet<String> = HashSet::new();
    let hide = |hidden: &mut HashSet<String>, path: &str| {
        hidden.insert(path.to_ascii_lowercase());
    };

    // Resolve what every sheet names, and hide the parts.
    let mut cue_data: HashMap<String, String> = HashMap::new();
    let mut m3u_discs: HashMap<String, Vec<String>> = HashMap::new();
    for f in files {
        let ext = ext_of(&f.path);
        if ext != "cue" && ext != "m3u" {
            continue;
        }
        let dir = dir_of(&f.path);
        let named: Vec<String> = f
            .refs
            .iter()
            .map(|r| resolve(dir, r))
            .filter_map(|r| find(&r).map(|found| found.path.clone()))
            .collect();
        for part in &named {
            hide(&mut hidden, part);
        }
        if ext == "cue" {
            if let Some(first) = f
                .refs
                .first()
                .map(|r| resolve(dir, r))
                .and_then(|r| find(&r))
            {
                cue_data.insert(f.path.to_ascii_lowercase(), first.path.clone());
            }
        } else {
            m3u_discs.insert(f.path.to_ascii_lowercase(), named);
        }
    }
    // A playlist's discs are cue sheets, which in turn name bins.
    let m3u_disc_entries: HashMap<String, Vec<String>> = m3u_discs
        .iter()
        .map(|(m3u, discs)| {
            let entries: Vec<String> = discs
                .iter()
                .filter(|d| matches!(ext_of(d).as_str(), "cue" | "ccd" | "iso" | "bin"))
                .cloned()
                .collect();
            (m3u.clone(), entries)
        })
        .collect();

    // Untagged-track bins that share a base are one disc's tracks: keep the lowest.
    let mut lowest_track: BTreeMap<(String, String), (u32, String)> = BTreeMap::new();
    for f in files {
        if ext_of(&f.path) != "bin" || hidden.contains(&f.path.to_ascii_lowercase()) {
            continue;
        }
        let stem = stem_of(&f.path);
        if let Some(n) = track_number(stem) {
            let key = (
                dir_of(&f.path).to_ascii_lowercase(),
                strip_track(stem).to_ascii_lowercase(),
            );
            let slot = lowest_track
                .entry(key)
                .or_insert_with(|| (n, f.path.clone()));
            if n < slot.0 {
                *slot = (n, f.path.clone());
            }
        }
    }
    let keep_track: HashSet<String> = lowest_track
        .values()
        .map(|(_, p)| p.to_ascii_lowercase())
        .collect();

    let mut candidates: Vec<Candidate> = Vec::new();
    for f in files {
        let ext = ext_of(&f.path);
        let key = f.path.to_ascii_lowercase();
        if !is_game_ext(&ext) || hidden.contains(&key) {
            continue;
        }
        match ext.as_str() {
            "cue" => {
                let Some(data) = cue_data.get(&key) else {
                    continue;
                };
                candidates.push(Candidate {
                    entry: f.path.clone(),
                    data: data.clone(),
                    title: display_title(&f.path),
                });
            }
            "m3u" => {
                let discs = m3u_disc_entries.get(&key).cloned().unwrap_or_default();
                let Some(first) = discs.first() else {
                    continue;
                };
                let data = cue_data
                    .get(&first.to_ascii_lowercase())
                    .cloned()
                    .unwrap_or_else(|| first.clone());
                candidates.push(Candidate {
                    entry: first.clone(),
                    data,
                    title: display_title(&f.path),
                });
            }
            "bin" => {
                if track_number(stem_of(&f.path)).is_some() && !keep_track.contains(&key) {
                    continue;
                }
                candidates.push(Candidate {
                    entry: f.path.clone(),
                    data: f.path.clone(),
                    title: display_title(&f.path),
                });
            }
            "exe" => {
                // A program that ships next to its own disc image is that disc.
                let stem = stem_of(&f.path);
                let dir = dir_of(&f.path);
                let twin = ["cue", "bin", "iso", "ccd"].iter().any(|e| {
                    let name = if dir.is_empty() {
                        format!("{stem}.{e}")
                    } else {
                        format!("{dir}/{stem}.{e}")
                    };
                    find(&name).is_some()
                });
                if !twin {
                    candidates.push(Candidate {
                        entry: f.path.clone(),
                        data: f.path.clone(),
                        title: display_title(&f.path),
                    });
                }
            }
            _ => candidates.push(Candidate {
                entry: f.path.clone(),
                data: f.path.clone(),
                title: display_title(&f.path),
            }),
        }
    }

    // Playlist games already list their discs; everything else is one disc.
    let playlist: HashMap<String, Vec<String>> = files
        .iter()
        .filter(|f| ext_of(&f.path) == "m3u")
        .filter_map(|f| {
            let discs = m3u_disc_entries.get(&f.path.to_ascii_lowercase())?;
            Some((discs.first()?.to_ascii_lowercase(), discs.clone()))
        })
        .collect();

    // Group `(Disc N)` siblings.
    let mut by_base: BTreeMap<String, Vec<(u32, usize)>> = BTreeMap::new();
    for (i, c) in candidates.iter().enumerate() {
        if let Some((n, base)) = disc_tag(&c.title) {
            by_base
                .entry(base.to_ascii_lowercase())
                .or_default()
                .push((n, i));
        }
    }
    let mut merged_into: HashMap<usize, Game> = HashMap::new();
    let mut absorbed: HashSet<usize> = HashSet::new();
    for group in by_base.values_mut() {
        let distinct: HashSet<u32> = group.iter().map(|g| g.0).collect();
        if distinct.len() < 2 {
            continue;
        }
        group.sort();
        let (_, first) = group[0];
        let base = disc_tag(&candidates[first].title)
            .map(|(_, b)| b)
            .unwrap_or_default();
        let game = Game {
            entry: candidates[first].entry.clone(),
            data: candidates[first].data.clone(),
            title: base,
            discs: group
                .iter()
                .map(|(_, i)| candidates[*i].entry.clone())
                .collect(),
        };
        for (_, i) in group.iter() {
            absorbed.insert(*i);
        }
        merged_into.insert(first, game);
    }

    let mut games: Vec<Game> = Vec::new();
    for (i, c) in candidates.iter().enumerate() {
        if let Some(game) = merged_into.remove(&i) {
            games.push(game);
        } else if !absorbed.contains(&i) {
            let discs = playlist
                .get(&c.entry.to_ascii_lowercase())
                .cloned()
                .unwrap_or_else(|| vec![c.entry.clone()]);
            games.push(Game {
                entry: c.entry.clone(),
                data: c.data.clone(),
                title: c.title.clone(),
                discs,
            });
        }
    }
    games.sort_by(|a, b| {
        a.title
            .to_lowercase()
            .cmp(&b.title.to_lowercase())
            .then_with(|| a.entry.cmp(&b.entry))
    });
    games
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(paths: &[&str]) -> Vec<FoundFile> {
        paths.iter().map(|p| FoundFile::plain(*p)).collect()
    }

    fn titles(games: &[Game]) -> Vec<&str> {
        games.iter().map(|g| g.title.as_str()).collect()
    }

    /// Manny's WipEout (USA): a cue and nine track files in one folder.
    fn wipeout() -> Vec<FoundFile> {
        let mut files: Vec<FoundFile> = (1..=9)
            .map(|n| FoundFile::plain(format!("WipEout (USA)/WipEout (USA) (Track {n}).bin")))
            .collect();
        files.push(FoundFile::sheet(
            "WipEout (USA)/WipEout (USA).cue",
            (1..=9)
                .map(|n| format!("WipEout (USA) (Track {n}).bin"))
                .collect(),
        ));
        files
    }

    #[test]
    fn a_cue_and_its_tracks_are_one_game() {
        let games = group_games(&wipeout());
        assert_eq!(titles(&games), ["WipEout (USA)"]);
        assert_eq!(games[0].entry, "WipEout (USA)/WipEout (USA).cue");
        assert_eq!(games[0].data, "WipEout (USA)/WipEout (USA) (Track 1).bin");
    }

    #[test]
    fn tracks_without_a_cue_show_only_the_lowest() {
        let files = plain(&[
            "WipEout 2097 (Europe)/WipEout 2097 (Europe) (Track 02).bin",
            "WipEout 2097 (Europe)/WipEout 2097 (Europe) (Track 01).bin",
            "WipEout 2097 (Europe)/WipEout 2097 (Europe) (Track 12).bin",
        ]);
        let games = group_games(&files);
        assert_eq!(titles(&games), ["WipEout 2097 (Europe)"]);
        assert!(games[0].entry.ends_with("(Track 01).bin"));
    }

    #[test]
    fn a_lone_bin_is_shown_once_and_keeps_its_name() {
        let games = group_games(&plain(&["Quake (quake-psx).bin"]));
        assert_eq!(titles(&games), ["Quake (quake-psx)"]);
    }

    #[test]
    fn a_cue_with_deleted_bins_is_not_a_game() {
        let files = vec![FoundFile::sheet("Old.cue", vec!["Old.bin".to_string()])];
        assert!(group_games(&files).is_empty());
    }

    #[test]
    fn cue_names_resolve_case_and_folders() {
        let files = vec![
            FoundFile::plain("Games/Crash/CRASH.BIN"),
            FoundFile::sheet("Games/Crash/Crash.cue", vec!["crash.bin".to_string()]),
        ];
        let games = group_games(&files);
        assert_eq!(games.len(), 1);
        assert!(games[0].entry.ends_with("Crash.cue"));
        // A cue that names a file in the folder above, with a backslash.
        let files = vec![
            FoundFile::plain("Games/data/game.bin"),
            FoundFile::sheet(
                "Games/cues/game.cue",
                vec!["..\\data\\game.bin".to_string()],
            ),
        ];
        assert_eq!(group_games(&files).len(), 1);
    }

    #[test]
    fn already_resolved_cue_references_work() {
        let files = vec![
            FoundFile::plain("/lib/A/A (Track 1).bin"),
            FoundFile::plain("/lib/A/A (Track 2).bin"),
            FoundFile::sheet(
                "/lib/A/A.cue",
                vec![
                    "/lib/A/A (Track 1).bin".to_string(),
                    "/lib/A/A (Track 2).bin".to_string(),
                ],
            ),
        ];
        assert_eq!(group_games(&files).len(), 1);
    }

    #[test]
    fn discs_of_one_game_are_one_case() {
        let files = vec![
            FoundFile::plain("Final Fantasy IX (USA) (Disc 2)/Final Fantasy IX (USA) (Disc 2).bin"),
            FoundFile::plain("Final Fantasy IX (USA) (Disc 1)/Final Fantasy IX (USA) (Disc 1).bin"),
            FoundFile::sheet(
                "Final Fantasy IX (USA) (Disc 1)/Final Fantasy IX (USA) (Disc 1).cue",
                vec!["Final Fantasy IX (USA) (Disc 1).bin".to_string()],
            ),
            FoundFile::sheet(
                "Final Fantasy IX (USA) (Disc 2)/Final Fantasy IX (USA) (Disc 2).cue",
                vec!["Final Fantasy IX (USA) (Disc 2).bin".to_string()],
            ),
        ];
        let games = group_games(&files);
        assert_eq!(titles(&games), ["Final Fantasy IX (USA)"]);
        assert!(games[0].entry.contains("(Disc 1).cue"));
        assert_eq!(games[0].discs.len(), 2);
    }

    #[test]
    fn a_single_disc_keeps_its_disc_tag() {
        let files = plain(&["Chrono Cross (USA) (Disc 1).bin"]);
        assert_eq!(
            titles(&group_games(&files)),
            ["Chrono Cross (USA) (Disc 1)"]
        );
    }

    #[test]
    fn a_playlist_is_one_game_over_its_discs() {
        let files = vec![
            FoundFile::plain("MGS/MGS (Disc 1).bin"),
            FoundFile::plain("MGS/MGS (Disc 2).bin"),
            FoundFile::sheet("MGS/MGS (Disc 1).cue", vec!["MGS (Disc 1).bin".to_string()]),
            FoundFile::sheet("MGS/MGS (Disc 2).cue", vec!["MGS (Disc 2).bin".to_string()]),
            FoundFile::sheet(
                "MGS/Metal Gear Solid.m3u",
                vec![
                    "MGS (Disc 1).cue".to_string(),
                    "MGS (Disc 2).cue".to_string(),
                ],
            ),
        ];
        let games = group_games(&files);
        assert_eq!(titles(&games), ["Metal Gear Solid"]);
        assert_eq!(games[0].entry, "MGS/MGS (Disc 1).cue");
        assert_eq!(games[0].data, "MGS/MGS (Disc 1).bin");
        assert_eq!(games[0].discs.len(), 2);
    }

    #[test]
    fn a_program_beside_its_disc_is_hidden() {
        let files = plain(&["WipEout PSoXide.bin", "WipEout PSoXide.exe", "Other.exe"]);
        assert_eq!(titles(&group_games(&files)), ["Other", "WipEout PSoXide"]);
    }

    #[test]
    fn track_tags_parse() {
        assert_eq!(track_number("Game (Track 04)"), Some(4));
        assert_eq!(track_number("Game (Track 1)"), Some(1));
        assert_eq!(track_number("Game (USA)"), None);
        assert_eq!(strip_track("Game (USA) (Track 12)"), "Game (USA)");
        assert_eq!(
            display_title("a/Game (Europe) (Track 01).bin"),
            "Game (Europe)"
        );
    }
}
