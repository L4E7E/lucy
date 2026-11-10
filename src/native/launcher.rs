//! Desktop-entry launcher; only successful launches contribute to local ranking.
use super::tray::Icon;
use gio::prelude::*;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};
#[path = "launcher_icons.rs"]
mod icon_theme;

#[derive(Clone)]
pub struct App {
    pub id: String,
    pub name: String,
    pub search: String,
    pub icon_name: String,
    pub icon: Option<Icon>,
}
pub const WIDTH: i32 = 600;
pub const HEIGHT: i32 = 486; // Three full rows, then half of the fourth row's icons.
pub const ROW_HEIGHT: f64 = 116.0;
pub const CELL_WIDTH: f64 = 112.0;
pub const ICON_SIZE: f64 = 48.0;
pub const ICON_RASTER_SIZE: i32 = 96; // Crisp at both 100% and 150–200% output scale.
pub const NAME_SIZE: f64 = 13.0;
pub const NAME_HEIGHT: f64 = 36.0;
pub const TILE_HEIGHT: f64 = ICON_SIZE + NAME_HEIGHT + 24.0; // Three 8px padding/gap bands.
pub const LIST_BOTTOM: f64 = 22.0; // Keep exactly half of the fourth 48px icon visible.
#[derive(Default)]
pub struct Launcher {
    apps: Vec<App>,
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
    pub fn set_apps(&mut self, apps: Vec<App>) {
        self.apps = apps;
        self.rank();
    }
    fn rank(&mut self) {
        // Case folding and frequency sorting belong to catalog/ranking changes,
        // not each hover, scroll frame, or key press.
        self.apps.sort_by_cached_key(|app| {
            (
                std::cmp::Reverse(self.usage.get(&app.id).copied().unwrap_or(0)),
                app.name.to_lowercase(),
                app.id.clone(),
            )
        });
    }
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
        self.rank();
    }
    pub fn results(&self) -> Vec<&App> {
        let query = self.query.to_lowercase();
        let words: Vec<_> = query.split_whitespace().collect();
        self.apps
            .iter()
            .filter(|a| words.iter().all(|w| a.search.contains(w)))
            .collect()
    }
    pub fn record(&mut self, id: &str) {
        let count = self.usage.entry(id.into()).or_default();
        *count = count.saturating_add(1);
        self.rank();
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
    let theme = crate::bar_backend::output(
        "gsettings",
        &["get", "org.gnome.desktop.interface", "icon-theme"],
    )
    .unwrap_or_else(|| "Adwaita".into())
    .trim_matches('\'')
    .to_owned();
    let mut lookup = icon_theme::Lookup::system(&theme);
    let mut decoded: HashMap<PathBuf, Option<Icon>> = HashMap::new();
    for app in apps {
        app.icon = lookup.resolve(&app.icon_name).and_then(|path| {
            decoded
                .entry(path.clone())
                .or_insert_with(|| decode(&path))
                .clone()
        });
    }
}
fn decode(path: &Path) -> Option<Icon> {
    if std::fs::metadata(path).ok()?.len() > 2 * 1024 * 1024 {
        return None;
    }
    let p = gdk_pixbuf::Pixbuf::from_file_at_scale(path, ICON_RASTER_SIZE, ICON_RASTER_SIZE, true)
        .ok()?;
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
        bytes: bytes.into(),
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
        l.rank();
        assert_eq!(
            l.results()
                .iter()
                .map(|a| a.name.as_str())
                .collect::<Vec<_>>(),
            vec!["Alpha", "Beta", "Zulu"]
        );
        l.usage.insert("Zulu".into(), 5);
        l.rank();
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
