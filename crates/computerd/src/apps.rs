//! Finding the program an application name stands for.

use std::path::{Path, PathBuf};

const BROWSER_NAMES: [&str; 5] = [
    "browser",
    "chromium",
    "chromium-browser",
    "chrome",
    "google-chrome",
];
const TERMINAL_NAMES: [&str; 2] = ["terminal", "xterm"];
const TERMINAL: &str = "xterm";

/// An installed application that has a `.desktop` file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesktopEntry {
    /// File name without `.desktop`.
    pub id: String,
    pub name: String,
    pub exec: String,
}

/// What an application name resolved to.
#[derive(Debug, PartialEq, Eq)]
pub enum App {
    /// The screen's own Chromium.
    Browser,
    Terminal,
    /// A program to run, with its arguments.
    Command {
        label: String,
        argv: Vec<String>,
    },
}

/// Folders that hold `.desktop` files, system ones first.
pub fn desktop_dirs(home: &Path) -> Vec<PathBuf> {
    vec![
        PathBuf::from("/usr/share/applications"),
        PathBuf::from("/usr/local/share/applications"),
        home.join(".local/share/applications"),
    ]
}

/// Reads one `.desktop` file. Entries that are hidden or are not applications give `None`.
pub fn parse_desktop(id: &str, text: &str) -> Option<DesktopEntry> {
    let mut in_entry = false;
    let (mut name, mut exec, mut kind) = (None, None, None);
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            in_entry = line == "[Desktop Entry]";
            continue;
        }
        if !in_entry {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key.trim() {
            "Name" => name = Some(value.trim().to_owned()),
            "Exec" => exec = Some(value.trim().to_owned()),
            "Type" => kind = Some(value.trim().to_owned()),
            "NoDisplay" | "Hidden" if value.trim() == "true" => return None,
            _ => {}
        }
    }
    if kind.as_deref() != Some("Application") {
        return None;
    }
    Some(DesktopEntry {
        id: id.to_owned(),
        name: name?,
        exec: exec?,
    })
}

/// Splits an `Exec=` value into arguments. Field codes become `uri`, or vanish without one.
pub fn exec_argv(exec: &str, uri: Option<&str>) -> Vec<String> {
    let mut words: Vec<String> = Vec::new();
    let mut word = String::new();
    let mut started = false;
    let mut quoted = false;
    let mut chars = exec.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                quoted = !quoted;
                started = true;
            }
            '\\' if quoted => {
                if let Some(next) = chars.next() {
                    word.push(next);
                }
            }
            ' ' | '\t' if !quoted => {
                if started {
                    words.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            _ => {
                word.push(c);
                started = true;
            }
        }
    }
    if started {
        words.push(word);
    }
    words
        .into_iter()
        .filter_map(|word| fill_field_codes(&word, uri))
        .collect()
}

/// Replaces `%u`-style codes in one argument. An argument that holds only codes is dropped
/// when nothing is left of it.
fn fill_field_codes(word: &str, uri: Option<&str>) -> Option<String> {
    let mut out = String::new();
    let mut chars = word.chars();
    let mut had_code = false;
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('%') => out.push('%'),
            Some('f' | 'F' | 'u' | 'U') => {
                had_code = true;
                out.push_str(uri.unwrap_or_default());
            }
            _ => had_code = true,
        }
    }
    if out.is_empty() && had_code {
        None
    } else {
        Some(out)
    }
}

/// Resolves an application name. Known names come first, then `.desktop` entries by file name or
/// display name, then programs found by `on_path`, then absolute paths.
pub fn resolve(
    name: &str,
    entries: &[DesktopEntry],
    uri: Option<&str>,
    on_path: impl Fn(&str) -> bool,
) -> Option<App> {
    let name = name.trim();
    let lower = name.to_lowercase();
    if BROWSER_NAMES.contains(&lower.as_str()) {
        return Some(App::Browser);
    }
    if TERMINAL_NAMES.contains(&lower.as_str()) {
        return Some(App::Terminal);
    }
    let lower = lower.strip_suffix(".desktop").unwrap_or(&lower);
    let entry = entries
        .iter()
        .find(|entry| entry.id.to_lowercase() == lower)
        .or_else(|| {
            entries
                .iter()
                .find(|entry| entry.name.to_lowercase() == lower)
        });
    if let Some(entry) = entry {
        let argv = exec_argv(&entry.exec, uri);
        if !argv.is_empty() {
            return Some(App::Command {
                label: entry.name.clone(),
                argv,
            });
        }
    }
    let runnable = if name.contains('/') {
        name.starts_with('/') && on_path(name)
    } else {
        !name.is_empty() && on_path(name)
    };
    runnable.then(|| App::Command {
        label: name.to_owned(),
        argv: std::iter::once(name.to_owned())
            .chain(uri.map(str::to_owned))
            .collect(),
    })
}

/// The command that starts a terminal.
pub fn terminal_argv() -> Vec<String> {
    vec![TERMINAL.to_owned()]
}

/// Whether `name` is an executable file in a folder of `path_var`, or an executable path itself.
pub fn executable_exists(name: &str, path_var: Option<&std::ffi::OsStr>) -> bool {
    let is_file = |path: &Path| path.is_file();
    if name.contains('/') {
        return is_file(Path::new(name));
    }
    path_var
        .into_iter()
        .flat_map(std::env::split_paths)
        .any(|dir| is_file(&dir.join(name)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, name: &str, exec: &str) -> DesktopEntry {
        DesktopEntry {
            id: id.to_owned(),
            name: name.to_owned(),
            exec: exec.to_owned(),
        }
    }

    #[test]
    fn desktop_files_give_applications_and_skip_hidden_and_other_entries() {
        let text = "[Desktop Entry]\nType=Application\nName=Image Viewer\nName[de]=Bildbetrachter\nExec=eog %U\n\n[Desktop Action new]\nName=New\nExec=eog --new\n";
        assert_eq!(
            parse_desktop("eog", text),
            Some(entry("eog", "Image Viewer", "eog %U"))
        );
        let hidden = "[Desktop Entry]\nType=Application\nName=X\nExec=x\nNoDisplay=true\n";
        assert_eq!(parse_desktop("x", hidden), None);
        let link = "[Desktop Entry]\nType=Link\nName=X\nURL=http://a\n";
        assert_eq!(parse_desktop("x", link), None);
    }

    #[test]
    fn exec_lines_split_with_quotes_and_field_codes() {
        assert_eq!(
            exec_argv(
                r#"env "A B=1" /opt/app/run --open=%u %i"#,
                Some("file:///a")
            ),
            ["env", "A B=1", "/opt/app/run", "--open=file:///a"]
        );
        assert_eq!(exec_argv("eog %U", None), ["eog"]);
        assert_eq!(exec_argv("eog %U", Some("x.png")), ["eog", "x.png"]);
        assert_eq!(exec_argv("tool 100%%", None), ["tool", "100%"]);
    }

    #[test]
    fn names_resolve_to_known_apps_then_desktop_entries_then_programs() {
        let entries = [
            entry("chromium", "Chromium", "/usr/bin/chromium %U"),
            entry("eog", "Image Viewer", "eog %U"),
        ];
        let on_path = |name: &str| name == "htop" || name == "/opt/tool";
        let at = |name| resolve(name, &entries, None, on_path);
        assert_eq!(at("Browser"), Some(App::Browser));
        assert_eq!(at("chromium"), Some(App::Browser));
        assert_eq!(at(" XTerm "), Some(App::Terminal));
        assert_eq!(
            at("eog.desktop"),
            Some(App::Command {
                label: "Image Viewer".to_owned(),
                argv: vec!["eog".to_owned()]
            })
        );
        assert_eq!(
            at("image viewer"),
            Some(App::Command {
                label: "Image Viewer".to_owned(),
                argv: vec!["eog".to_owned()]
            })
        );
        assert_eq!(
            resolve("htop", &entries, Some("a"), on_path),
            Some(App::Command {
                label: "htop".to_owned(),
                argv: vec!["htop".to_owned(), "a".to_owned()]
            })
        );
        assert!(at("/opt/tool").is_some());
        assert_eq!(at("tool"), None);
        assert_eq!(at("nope"), None);
        assert_eq!(at("relative/htop"), None);
    }
}
