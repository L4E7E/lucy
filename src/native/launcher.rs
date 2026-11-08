//! Desktop-entry launcher; only successful launches contribute to local ranking.
use super::tray::Icon;
use gio::prelude::*;
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};

#[derive(Clone)]
pub struct App {
    pub id: String,
    pub name: String,
    pub search: String,
    pub icon_name: String,
    pub icon: Option<Icon>,
}
pub const WIDTH: i32 = 570;
pub const HEIGHT: i32 = 465; // Three full rows, then half of the fourth row's icons.
pub const ROW_HEIGHT: f64 = 110.0;
pub const CELL_WIDTH: f64 = 106.0;
#[derive(Default)]
pub struct Launcher {
    pub apps: Vec<App>,
    pub ready: bool,
    pub query: String,
    pub cursor: usize,
    pub selected_all: bool,
    pub selected: usize,
    pub keyboard_focus: bool,
    pub usage: HashMap<String, u64>,
}
fn usage_path() -> Option<PathBuf> {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .map(|p| p.join("lucy/app-usage.json"))
}
impl Launcher {
    pub fn reset_search(&mut self) {
        self.query.clear();
        self.cursor = 0;
        self.selected_all = false;
        self.selected = 0;
        self.keyboard_focus = false;
    }
    pub fn load_usage(&mut self) {
        self.usage = usage_path()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
    }
    pub fn results(&self) -> Vec<&App> {
        let query = self.query.to_lowercase();
        let words: Vec<_> = query.split_whitespace().collect();
        let mut apps: Vec<_> = self
            .apps
            .iter()
            .filter(|a| words.iter().all(|w| a.search.contains(w)))
            .collect();
        apps.sort_by(|a, b| {
            self.usage
                .get(&b.id)
                .unwrap_or(&0)
                .cmp(self.usage.get(&a.id).unwrap_or(&0))
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
                .then_with(|| a.id.cmp(&b.id))
        });
        apps
    }
    pub fn record(&mut self, id: &str) {
        let count = self.usage.entry(id.into()).or_default();
        *count = count.saturating_add(1);
        let Some(path) = usage_path() else {
            return;
        };
        let usage = self.usage.clone();
        std::thread::spawn(move || {
            let Some(parent) = path.parent() else {
                return;
            };
            if std::fs::create_dir_all(parent).is_err() {
                return;
            }
            if let Ok(bytes) = serde_json::to_vec(&usage) {
                let temp = path.with_extension("json.new");
                if std::fs::write(&temp, bytes).is_ok() {
                    let _ = std::fs::rename(temp, path);
                }
            }
        });
    }
    pub fn insert(&mut self, value: &str) {
        if self.selected_all {
            self.query.clear();
            self.cursor = 0;
            self.selected_all = false;
        }
        let value: String = value
            .chars()
            .filter(|c| !c.is_control())
            .take(256)
            .collect();
        if self.query.len() + value.len() > 4096 {
            return;
        }
        self.query.insert_str(self.cursor, &value);
        self.cursor += value.len();
        self.selected = 0;
        self.keyboard_focus = !self.query.is_empty();
    }
    pub fn backspace(&mut self) {
        if self.selected_all {
            self.query.clear();
            self.cursor = 0;
            self.selected_all = false;
        } else if self.cursor > 0 {
            let start = self.query[..self.cursor]
                .char_indices()
                .last()
                .map_or(0, |(i, _)| i);
            self.query.replace_range(start..self.cursor, "");
            self.cursor = start;
        }
        self.selected = 0;
        self.keyboard_focus = !self.query.is_empty();
    }
    pub fn delete(&mut self) {
        if self.selected_all {
            self.backspace();
        } else if let Some(c) = self.query[self.cursor..].chars().next() {
            self.query
                .replace_range(self.cursor..self.cursor + c.len_utf8(), "");
            self.selected = 0;
            self.keyboard_focus = !self.query.is_empty();
        }
    }
    pub fn move_cursor(&mut self, right: bool) {
        self.selected_all = false;
        if right {
            self.cursor += self.query[self.cursor..]
                .chars()
                .next()
                .map_or(0, char::len_utf8);
        } else {
            self.cursor = self.query[..self.cursor]
                .char_indices()
                .last()
                .map_or(0, |(i, _)| i);
        }
    }
    pub fn select(&mut self, delta: isize) {
        if !self.keyboard_focus {
            self.selected = if delta < 0 {
                self.results().len().saturating_sub(1)
            } else {
                0
            };
            self.keyboard_focus = true;
            return;
        }
        self.selected = self
            .selected
            .saturating_add_signed(delta)
            .min(self.results().len().saturating_sub(1));
    }
}
pub fn launch(id: &str) -> Result<(), String> {
    let info = gio::DesktopAppInfo::new(id).ok_or("앱을 찾을 수 없습니다.")?;
    let path = info
        .filename()
        .ok_or("앱의 desktop entry를 찾을 수 없습니다.")?;
    let unit = format!("--unit=app-lucy-{}", glib::monotonic_time());
    crate::bar_backend::output(
        "systemd-run",
        &[
            "--user",
            "--collect",
            "--quiet",
            "--property=Type=exec",
            "--property=ExitType=cgroup",
            &unit,
            "--",
            "/usr/bin/gio",
            "launch",
            &path.to_string_lossy(),
        ],
    )
    .map(|_| ())
    .ok_or_else(|| "앱 실행 요청에 실패했습니다. 사용자 systemd 상태를 확인하세요.".into())
}
pub fn catalog() -> Vec<App> {
    let mut apps: Vec<_> = gio::AppInfo::all()
        .into_iter()
        .filter(|a| a.should_show())
        .filter_map(|a| {
            let id = a.id()?.to_string();
            let name = a.display_name().to_string();
            let icon_name = a
                .icon()
                .and_then(|icon| {
                    if let Some(themed) = icon.downcast_ref::<gio::ThemedIcon>() {
                        themed.names().first().map(ToString::to_string)
                    } else {
                        icon.downcast_ref::<gio::FileIcon>()
                            .and_then(|i| i.file().path())
                            .map(|p| p.to_string_lossy().into_owned())
                    }
                })
                .unwrap_or_default();
            Some(App {
                search: format!("{name} {id} {}", a.executable().display()).to_lowercase(),
                id,
                name,
                icon_name,
                icon: None,
            })
        })
        .collect();
    apps.sort_by(|a, b| a.id.cmp(&b.id));
    apps.dedup_by(|a, b| a.id == b.id);
    apps
}
pub fn load_icons(apps: &mut [App]) {
    let names: HashSet<_> = apps
        .iter()
        .filter(|a| !a.icon_name.is_empty())
        .map(|a| a.icon_name.clone())
        .collect();
    let mut paths: HashMap<String, PathBuf> = names
        .iter()
        .filter_map(|n| {
            let p = PathBuf::from(n);
            p.is_absolute().then(|| (n.clone(), p))
        })
        .collect();
    let theme = crate::bar_backend::output(
        "gsettings",
        &["get", "org.gnome.desktop.interface", "icon-theme"],
    )
    .unwrap_or_else(|| "Adwaita".into())
    .trim_matches('\'')
    .to_owned();
    let home = std::env::var("HOME").unwrap_or_default();
    for theme in [&theme, "hicolor", "Adwaita"] {
        for base in [
            format!("{home}/.local/share/icons"),
            format!("{home}/.icons"),
            "/usr/share/icons".into(),
        ] {
            index(&PathBuf::from(base).join(theme), &names, &mut paths);
        }
    }
    index(Path::new("/usr/share/pixmaps"), &names, &mut paths);
    for app in apps {
        app.icon = paths.get(&app.icon_name).and_then(|p| decode(p));
    }
}
fn index(root: &Path, names: &HashSet<String>, paths: &mut HashMap<String, PathBuf>) {
    let mut stack = vec![(root.to_owned(), 0)];
    let mut visited = 0;
    while let Some((dir, depth)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            visited += 1;
            if visited > 60000 {
                return;
            }
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if kind.is_dir() && depth < 5 && entry.file_name() != "cursors" {
                stack.push((path, depth + 1));
            } else if path
                .extension()
                .and_then(|s| s.to_str())
                .is_some_and(|s| ["png", "svg", "xpm"].contains(&s))
            {
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    if names.contains(stem) {
                        paths.entry(stem.into()).or_insert(path);
                    }
                }
            }
        }
    }
}
fn decode(path: &Path) -> Option<Icon> {
    if std::fs::metadata(path).ok()?.len() > 2 * 1024 * 1024 {
        return None;
    }
    let p = gdk_pixbuf::Pixbuf::from_file_at_scale(path, 40, 40, true).ok()?;
    let (w, h) = (p.width(), p.height());
    let stride = p.rowstride() as usize;
    let n = p.n_channels() as usize;
    let source = p.read_pixel_bytes();
    let mut bytes = vec![0; (w * h * 4) as usize];
    for y in 0..h as usize {
        for x in 0..w as usize {
            let i = y * stride + x * n;
            let j = (y * w as usize + x) * 4;
            let alpha = if n == 4 { source[i + 3] } else { 255 };
            bytes[j] = (u16::from(source[i + 2]) * u16::from(alpha) / 255) as u8;
            bytes[j + 1] = (u16::from(source[i + 1]) * u16::from(alpha) / 255) as u8;
            bytes[j + 2] = (u16::from(source[i]) * u16::from(alpha) / 255) as u8;
            bytes[j + 3] = alpha;
        }
    }
    Some(Icon {
        bytes,
        width: w,
        height: h,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn opening_and_clearing_the_launcher_have_no_automatic_selection() {
        let mut l = Launcher::default();
        assert!(!l.keyboard_focus);
        l.select(5);
        assert!(l.keyboard_focus);
        assert_eq!(l.selected, 0);
        l.reset_search();
        assert!(!l.keyboard_focus);
        l.insert("a");
        assert!(l.keyboard_focus);
        l.backspace();
        assert!(!l.keyboard_focus);
    }
    #[test]
    fn frequency_then_name_and_search_are_deterministic() {
        let mut l = Launcher::default();
        for name in ["Zulu", "Alpha", "Beta"] {
            l.apps.push(App {
                id: name.into(),
                name: name.into(),
                search: name.to_lowercase(),
                icon_name: String::new(),
                icon: None,
            });
        }
        assert_eq!(
            l.results()
                .iter()
                .map(|a| a.name.as_str())
                .collect::<Vec<_>>(),
            vec!["Alpha", "Beta", "Zulu"]
        );
        l.usage.insert("Zulu".into(), 5);
        assert_eq!(l.results()[0].name, "Zulu");
        l.insert("alp");
        assert_eq!(l.results()[0].name, "Alpha");
    }
    #[test]
    fn unicode_search_editing_preserves_boundaries() {
        let mut l = Launcher::default();
        l.insert("한글abc");
        l.move_cursor(false);
        l.backspace();
        assert_eq!(l.query, "한글ac");
        l.selected_all = true;
        l.insert("새 검색");
        assert_eq!(l.query, "새 검색");
    }
}
