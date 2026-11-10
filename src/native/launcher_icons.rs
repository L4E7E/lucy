//! Native icon-theme lookup, without GTK or recursive filesystem scans.
//! Declared directories may themselves be symlinks (notably Papirus-Dark/apps).
use std::{
    collections::{HashMap, HashSet},
    path::{Component, Path, PathBuf},
};

const SIZE: i32 = 48;
const EXTENSIONS: [&str; 3] = ["png", "svg", "xpm"];

struct Theme {
    name: String,
    directories: Vec<String>,
}
pub(super) struct Lookup {
    roots: Vec<PathBuf>,
    fallbacks: Vec<PathBuf>,
    themes: Vec<Theme>,
    paths: HashMap<String, Option<PathBuf>>,
}
fn list(file: &glib::KeyFile, key: &str) -> Vec<String> {
    file.string("Icon Theme", key)
        .ok()
        .into_iter()
        .flat_map(|s| {
            s.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .collect()
}
fn relative(path: &str) -> bool {
    !path.is_empty()
        && !Path::new(path).is_absolute()
        && Path::new(path)
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
}
fn theme_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-_+.".contains(&c))
}
fn distance(file: &glib::KeyFile, group: &str) -> i32 {
    let size = file.integer(group, "Size").unwrap_or(SIZE).clamp(1, 4096);
    let scale = file.integer(group, "Scale").unwrap_or(1).clamp(1, 16);
    let (low, high) = match file.string(group, "Type").ok().as_deref() {
        Some("Fixed") => (size, size),
        Some("Scalable") => (
            file.integer(group, "MinSize").unwrap_or(size),
            file.integer(group, "MaxSize").unwrap_or(size),
        ),
        _ => {
            let t = file.integer(group, "Threshold").unwrap_or(2).clamp(0, 4096);
            (size - t, size + t)
        }
    };
    let low = low.clamp(0, 8192) * scale;
    let high = high.clamp(0, 8192).max(low / scale) * scale;
    if SIZE < low {
        low - SIZE
    } else if SIZE > high {
        SIZE - high
    } else {
        0
    }
}
impl Lookup {
    pub(super) fn system(theme: &str) -> Self {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default();
        let data = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".local/share"));
        let directories = std::env::var_os("XDG_DATA_DIRS")
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "/usr/local/share:/usr/share".into());
        let mut roots = vec![data.join("icons"), home.join(".icons")];
        let mut fallbacks = Vec::new();
        for dir in std::env::split_paths(&directories).filter(|p| p.is_absolute()) {
            roots.push(dir.join("icons"));
            fallbacks.push(dir.join("pixmaps"));
        }
        fallbacks.push(PathBuf::from("/usr/share/pixmaps"));
        Self::new(theme, roots, fallbacks)
    }
    fn new(theme: &str, mut roots: Vec<PathBuf>, fallbacks: Vec<PathBuf>) -> Self {
        let mut seen = HashSet::new();
        roots.retain(|r| seen.insert(r.clone()));
        let mut lookup = Self {
            roots,
            fallbacks,
            themes: Vec::new(),
            paths: HashMap::new(),
        };
        let mut seen = HashSet::new();
        lookup.add_theme(theme, &mut seen);
        if theme != "hicolor" {
            lookup.add_theme("Adwaita", &mut seen);
        }
        lookup.add_theme("hicolor", &mut seen);
        lookup
    }
    fn add_theme(&mut self, theme: &str, seen: &mut HashSet<String>) {
        if !theme_name(theme) || seen.len() >= 32 || !seen.insert(theme.into()) {
            return;
        }
        let metadata = self.roots.iter().find_map(|root| {
            let file = glib::KeyFile::new();
            file.load_from_file(
                root.join(theme).join("index.theme"),
                glib::KeyFileFlags::NONE,
            )
            .ok()?;
            Some(file)
        });
        let Some(file) = metadata else {
            return;
        };
        let mut directories = list(&file, "Directories");
        directories.extend(list(&file, "ScaledDirectories"));
        directories.retain(|d| relative(d));
        // Stable ties retain the theme's declared order. Follow its directory links
        // via ordinary path lookup; never enumerate 16px/symbolic files at random.
        directories.sort_by_key(|d| distance(&file, d));
        directories.truncate(4096);
        self.themes.push(Theme {
            name: theme.into(),
            directories,
        });
        for parent in list(&file, "Inherits")
            .into_iter()
            .filter(|p| p != "hicolor")
        {
            self.add_theme(&parent, seen);
        }
    }
    pub(super) fn resolve(&mut self, name: &str) -> Option<PathBuf> {
        if let Some(path) = self.paths.get(name) {
            return path.clone();
        }
        let path = self.find(name);
        self.paths.insert(name.into(), path.clone());
        path
    }
    fn find(&self, name: &str) -> Option<PathBuf> {
        let path = Path::new(name);
        if path.is_absolute() {
            return path.is_file().then(|| path.to_owned());
        }
        if !theme_name(name) {
            return None;
        }
        let filenames = if path
            .extension()
            .and_then(|s| s.to_str())
            .is_some_and(|s| EXTENSIONS.contains(&s))
        {
            vec![name.to_owned()]
        } else {
            EXTENSIONS.map(|ext| format!("{name}.{ext}")).to_vec()
        };
        for theme in &self.themes {
            for dir in &theme.directories {
                for root in &self.roots {
                    for filename in &filenames {
                        let path = root.join(&theme.name).join(dir).join(filename);
                        if path.is_file() {
                            return Some(path);
                        }
                    }
                }
            }
        }
        for root in self.roots.iter().chain(&self.fallbacks) {
            for filename in &filenames {
                let path = root.join(filename);
                if path.is_file() {
                    return Some(path);
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, os::unix::fs::symlink};
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "lucy-launcher-icons.{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            fs::create_dir(&root).unwrap();
            Self(root)
        }
        fn write(&self, path: &str, text: &str) -> PathBuf {
            let p = self.0.join(path);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(&p, text).unwrap();
            p
        }
        fn lookup(&self, theme: &str) -> Lookup {
            Lookup::new(theme, vec![self.0.clone()], vec![self.0.join("pixmaps")])
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }
    const INDEX: &str = "[Icon Theme]\nDirectories=16x16/apps,48x48/apps\nInherits=hicolor\n[16x16/apps]\nSize=16\nType=Fixed\n[48x48/apps]\nSize=48\nType=Fixed\n";
    #[test]
    fn papirus_style_linked_directories_and_icon_aliases_beat_generic_fallback() {
        let f = Fixture::new();
        f.write("Dark/index.theme", INDEX);
        f.write("hicolor/index.theme", INDEX);
        f.write("hicolor/48x48/apps/example.svg", "generic");
        let expected = f.write("Base/48x48/apps/real.svg", "themed");
        symlink("real.svg", f.0.join("Base/48x48/apps/example.svg")).unwrap();
        symlink("../Base/48x48", f.0.join("Dark/48x48")).unwrap();
        let path = f.lookup("Dark").resolve("example").unwrap();
        assert!(path.starts_with(f.0.join("Dark")));
        assert_eq!(fs::canonicalize(path).unwrap(), expected);
    }
    #[test]
    fn app_sized_art_is_selected_deterministically_instead_of_first_small_icon() {
        let f = Fixture::new();
        f.write("Dark/index.theme", INDEX);
        f.write("Dark/16x16/apps/example.svg", "tiny");
        let expected = f.write("Dark/48x48/apps/example.svg", "large");
        for _ in 0..3 {
            assert_eq!(f.lookup("Dark").resolve("example"), Some(expected.clone()));
        }
    }
    #[test]
    fn inheritance_user_overrides_cycles_and_unthemed_fallback_are_bounded() {
        let f = Fixture::new();
        f.write(
            "System/Dark/index.theme",
            &INDEX.replace("Inherits=hicolor", "Inherits=Parent,hicolor"),
        );
        f.write(
            "System/Parent/index.theme",
            &INDEX.replace("Inherits=hicolor", "Inherits=Dark"),
        );
        f.write("System/hicolor/index.theme", INDEX);
        f.write("System/Dark/48x48/apps/custom.svg", "system");
        let user = f.write("User/Dark/48x48/apps/custom.svg", "override");
        let parent = f.write("System/Parent/48x48/apps/inherited.svg", "parent");
        let generic = f.write("System/hicolor/48x48/apps/unknown.svg", "fallback");
        let raw = f.write("pixmaps/legacy.png", "legacy");
        let mut lookup = Lookup::new(
            "Dark",
            vec![f.0.join("User"), f.0.join("System")],
            vec![f.0.join("pixmaps")],
        );
        assert_eq!(lookup.resolve("custom"), Some(user));
        assert_eq!(lookup.resolve("inherited"), Some(parent));
        assert_eq!(lookup.resolve("unknown"), Some(generic));
        assert_eq!(lookup.resolve("legacy.png"), Some(raw.clone()));
        assert_eq!(lookup.resolve(raw.to_str().unwrap()), Some(raw));
        assert_eq!(lookup.resolve("absent"), None);
        assert_eq!(lookup.resolve("../elsewhere"), None);
        assert_eq!(lookup.themes.len(), 3);
    }
    #[test]
    fn selected_theme_stays_first_even_if_a_parent_has_a_closer_size() {
        let f = Fixture::new();
        f.write(
            "Dark/index.theme",
            &INDEX.replace("Inherits=hicolor", "Inherits=Parent"),
        );
        f.write("Parent/index.theme", INDEX);
        let expected = f.write("Dark/16x16/apps/example.svg", "selected");
        f.write("Parent/48x48/apps/example.svg", "inherited");
        assert_eq!(f.lookup("Dark").resolve("example"), Some(expected));
    }
    #[test]
    #[ignore = "requires the installed Papirus-Dark theme and its SVG loader"]
    fn installed_papirus_application_icons_resolve_and_decode() {
        let mut lookup = Lookup::system("Papirus-Dark");
        for name in ["org.gnome.Nautilus", "firefox", "foot"] {
            let path = lookup.resolve(name).unwrap();
            assert!(
                path.starts_with("/usr/share/icons/Papirus-Dark/48x48/apps"),
                "{name}: {path:?}"
            );
            let icon = super::super::decode(&path).unwrap();
            let size = super::super::ICON_RASTER_SIZE;
            assert_eq!((icon.width, icon.height), (size, size));
            println!(
                "{name}: {} -> {} ({size}x{size} decoded)",
                path.display(),
                std::fs::canonicalize(&path).unwrap().display()
            );
        }
        let mut apps = super::super::catalog();
        let start = std::time::Instant::now();
        super::super::load_icons(&mut apps);
        for name in ["org.gnome.Nautilus", "firefox", "foot"] {
            let expected = super::super::decode(&lookup.resolve(name).unwrap()).unwrap();
            let app = apps.iter().find(|app| app.icon_name == name).unwrap();
            assert_eq!(
                app.icon.as_ref().unwrap().bytes,
                expected.bytes,
                "{name}: the actual catalog must receive Papirus pixels, not a fallback"
            );
        }
        println!(
            "catalog: {}/{} icons decoded in {:?}",
            apps.iter().filter(|app| app.icon.is_some()).count(),
            apps.len(),
            start.elapsed()
        );
    }
}
