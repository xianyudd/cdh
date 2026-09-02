use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_TEMP_FILE: AtomicU64 = AtomicU64::new(0);
const MAX_TEMP_CREATE_ATTEMPTS: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LanguagePreference {
    Auto,
    ZhCn,
    En,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SettingKey {
    Language,
    Theme,
    Preview,
    Color,
    Mouse,
}

/// Which settings the environment pins, snapshotted once per frame so the
/// render path never has to consult `UiSettings` (or the process
/// environment) while drawing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct SettingLocks {
    pub(super) language: bool,
    pub(super) theme: bool,
    pub(super) preview: bool,
    pub(super) color: bool,
    pub(super) mouse: bool,
}

impl SettingLocks {
    pub(super) fn is_locked(&self, key: SettingKey) -> bool {
        match key {
            SettingKey::Language => self.language,
            SettingKey::Theme => self.theme,
            SettingKey::Preview => self.preview,
            SettingKey::Color => self.color,
            SettingKey::Mouse => self.mouse,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct UiPreferences {
    pub(super) language: LanguagePreference,
    pub(super) preview: bool,
    pub(super) color: bool,
    pub(super) mouse: bool,
    pub(super) theme: super::ThemeChoice,
}

impl Default for UiPreferences {
    fn default() -> Self {
        Self {
            language: LanguagePreference::Auto,
            preview: false,
            color: true,
            mouse: true,
            theme: super::ThemeChoice::Graphite,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct UiEnvironment {
    pub(super) language: Option<LanguagePreference>,
    pub(super) preview: Option<bool>,
    pub(super) color: Option<bool>,
    pub(super) mouse: Option<bool>,
    pub(super) theme: Option<super::ThemeChoice>,
}

impl UiEnvironment {
    pub(super) fn from_process() -> Self {
        let language = env::var("CDH_LANG").ok();
        let preview = env::var("CDH_PREVIEW").ok();
        let color = env::var("CDH_COLOR").ok();
        let mouse = env::var("CDH_MOUSE").ok();
        let theme = env::var("CDH_THEME").ok();
        Self::from_values(
            language.as_deref(),
            preview.as_deref(),
            color.as_deref(),
            mouse.as_deref(),
            theme.as_deref(),
        )
    }

    pub(super) fn from_values(
        language: Option<&str>,
        preview: Option<&str>,
        color: Option<&str>,
        mouse: Option<&str>,
        theme: Option<&str>,
    ) -> Self {
        Self {
            language: language.and_then(parse_environment_language),
            preview: preview.map(parse_boolean_environment),
            color: color.map(parse_boolean_environment),
            mouse: mouse.map(parse_boolean_environment),
            theme: theme.and_then(super::ThemeChoice::from_tag),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct UiSettings {
    path: PathBuf,
    saved: UiPreferences,
    environment: UiEnvironment,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SettingsLoad {
    pub(super) settings: UiSettings,
    pub(super) warning: Option<String>,
}

impl UiSettings {
    pub(super) fn load(path: PathBuf, environment: UiEnvironment) -> SettingsLoad {
        match fs::read_to_string(&path) {
            Ok(contents) => Self::parse(path, environment, &contents),
            Err(error) if error.kind() == io::ErrorKind::NotFound => SettingsLoad {
                settings: Self::new(path, UiPreferences::default(), environment),
                warning: None,
            },
            Err(error) => SettingsLoad {
                warning: Some(format!(
                    "failed to read TUI settings at {}: {error}",
                    path.display()
                )),
                settings: Self::new(path, UiPreferences::default(), environment),
            },
        }
    }

    fn parse(path: PathBuf, environment: UiEnvironment, contents: &str) -> SettingsLoad {
        let table = match contents.parse::<toml::Table>() {
            Ok(table) => table,
            Err(error) => {
                return SettingsLoad {
                    warning: Some(format!(
                        "failed to parse TUI settings at {}: {error}",
                        path.display()
                    )),
                    settings: Self::new(path, UiPreferences::default(), environment),
                };
            }
        };
        let mut saved = UiPreferences::default();
        let mut invalid = Vec::new();

        if let Some(value) = table.get("language") {
            match value.as_str().and_then(parse_saved_language) {
                Some(language) => saved.language = language,
                None => invalid.push(invalid_value_detail(
                    "language",
                    "a supported language",
                    value,
                )),
            }
        }
        if let Some(value) = table.get("theme") {
            match value.as_str().and_then(super::ThemeChoice::from_tag) {
                Some(theme) => saved.theme = theme,
                None => invalid.push(invalid_value_detail(
                    "theme",
                    "graphite, nord, daylight, mono, dracula, amber, or forest",
                    value,
                )),
            }
        }
        parse_boolean_field(&table, "preview", &mut saved.preview, &mut invalid);
        parse_boolean_field(&table, "color", &mut saved.color, &mut invalid);
        parse_boolean_field(&table, "mouse", &mut saved.mouse, &mut invalid);

        SettingsLoad {
            warning: (!invalid.is_empty()).then(|| {
                format!(
                    "invalid TUI setting value(s) at {}: {}",
                    path.display(),
                    invalid.join(", ")
                )
            }),
            settings: Self::new(path, saved, environment),
        }
    }

    fn new(path: PathBuf, saved: UiPreferences, environment: UiEnvironment) -> Self {
        Self {
            path,
            saved,
            environment,
        }
    }

    #[cfg(test)]
    pub(super) fn for_test(saved: UiPreferences) -> Self {
        Self::new(PathBuf::from("tui.toml"), saved, UiEnvironment::default())
    }

    #[cfg(test)]
    pub(super) fn saved(&self) -> UiPreferences {
        self.saved
    }

    pub(super) fn effective(&self) -> UiPreferences {
        UiPreferences {
            language: self.environment.language.unwrap_or(self.saved.language),
            preview: self.environment.preview.unwrap_or(self.saved.preview),
            color: self.environment.color.unwrap_or(self.saved.color),
            mouse: self.environment.mouse.unwrap_or(self.saved.mouse),
            theme: self.environment.theme.unwrap_or(self.saved.theme),
        }
    }

    pub(super) fn is_locked(&self, key: SettingKey) -> bool {
        match key {
            SettingKey::Language => self.environment.language.is_some(),
            SettingKey::Theme => self.environment.theme.is_some(),
            SettingKey::Preview => self.environment.preview.is_some(),
            SettingKey::Color => self.environment.color.is_some(),
            SettingKey::Mouse => self.environment.mouse.is_some(),
        }
    }

    /// The per-key lock bits, for callers that need the whole picture at once
    /// instead of one `is_locked` round-trip per row.
    pub(super) fn locks(&self) -> SettingLocks {
        SettingLocks {
            language: self.is_locked(SettingKey::Language),
            theme: self.is_locked(SettingKey::Theme),
            preview: self.is_locked(SettingKey::Preview),
            color: self.is_locked(SettingKey::Color),
            mouse: self.is_locked(SettingKey::Mouse),
        }
    }

    pub(super) fn candidate(&self, key: SettingKey, direction: isize) -> Option<UiPreferences> {
        if self.is_locked(key) {
            return None;
        }
        let mut candidate = self.saved;
        match key {
            SettingKey::Language => {
                let languages = [
                    LanguagePreference::Auto,
                    LanguagePreference::ZhCn,
                    LanguagePreference::En,
                ];
                let current = languages
                    .iter()
                    .position(|language| *language == candidate.language)
                    .unwrap_or(0) as isize;
                candidate.language = languages[(current + direction).rem_euclid(3) as usize];
            }
            SettingKey::Theme => candidate.theme = candidate.theme.cycle(direction),
            SettingKey::Preview => candidate.preview = !candidate.preview,
            SettingKey::Color => candidate.color = !candidate.color,
            SettingKey::Mouse => candidate.mouse = !candidate.mouse,
        }
        Some(candidate)
    }

    pub(super) fn persist(&mut self, candidate: UiPreferences) -> io::Result<()> {
        self.persist_with_replace(candidate, |source, target| fs::rename(source, target))
    }

    fn persist_with_replace<F>(&mut self, candidate: UiPreferences, replace: F) -> io::Result<()>
    where
        F: FnOnce(&Path, &Path) -> io::Result<()>,
    {
        let parent = settings_parent(&self.path);
        fs::create_dir_all(parent)?;

        let (temp_path, mut temp_file) = open_unique_temp(&self.path)?;
        let contents = serialize_preferences(candidate);
        let write_result = temp_file
            .write_all(contents.as_bytes())
            .and_then(|()| temp_file.flush())
            .and_then(|()| temp_file.sync_all());
        drop(temp_file);

        if let Err(error) = write_result {
            let _ = fs::remove_file(&temp_path);
            return Err(error);
        }
        // Atomic overwrite via std::fs::rename relies on Unix semantics. cdh runs in Unix
        // shells; Windows Terminal usage is supported through WSL, without a delete fallback.
        if let Err(error) = replace(&temp_path, &self.path) {
            let _ = fs::remove_file(&temp_path);
            return Err(error);
        }

        // The rename is already visible, so parent sync is best-effort: returning an error
        // here would report failure even though disk changed and memory must follow it.
        sync_parent_best_effort(parent);
        self.saved = candidate;
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn path(&self) -> &Path {
        &self.path
    }
}

fn settings_parent(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn unique_temp_path(path: &Path) -> PathBuf {
    let sequence = NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed);
    let file_name = path.file_name().unwrap_or_else(|| OsStr::new("tui.toml"));
    let mut temp_name = OsString::from(".");
    temp_name.push(file_name);
    temp_name.push(format!(".{}.{sequence}.tmp", std::process::id()));
    settings_parent(path).join(temp_name)
}

fn open_unique_temp(path: &Path) -> io::Result<(PathBuf, File)> {
    open_unique_temp_with(path, |temp_path| {
        OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(temp_path)
    })
}

fn open_unique_temp_with<W, F>(path: &Path, mut open: F) -> io::Result<(PathBuf, W)>
where
    F: FnMut(&Path) -> io::Result<W>,
{
    for _ in 0..MAX_TEMP_CREATE_ATTEMPTS {
        let temp_path = unique_temp_path(path);
        match open(&temp_path) {
            Ok(writer) => return Ok((temp_path, writer)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }

    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!(
            "failed to create a unique settings temp file after {MAX_TEMP_CREATE_ATTEMPTS} attempts"
        ),
    ))
}

fn sync_parent_best_effort(parent: &Path) {
    #[cfg(unix)]
    if let Ok(directory) = File::open(parent) {
        let _ = directory.sync_all();
    }

    #[cfg(not(unix))]
    let _ = parent;
}

fn serialize_preferences(preferences: UiPreferences) -> String {
    let language = match preferences.language {
        LanguagePreference::Auto => "auto",
        LanguagePreference::ZhCn => "zh-CN",
        LanguagePreference::En => "en",
    };
    format!(
        "language = \"{language}\"\ntheme = \"{}\"\npreview = {}\ncolor = {}\nmouse = {}\n",
        preferences.theme.tag(),
        preferences.preview,
        preferences.color,
        preferences.mouse
    )
}

fn parse_boolean_field(
    table: &toml::Table,
    key: &'static str,
    target: &mut bool,
    invalid: &mut Vec<String>,
) {
    if let Some(value) = table.get(key) {
        match value.as_bool() {
            Some(value) => *target = value,
            None => invalid.push(invalid_value_detail(key, "boolean", value)),
        }
    }
}

fn invalid_value_detail(key: &str, expected: &str, value: &toml::Value) -> String {
    format!(
        "{key} expected {expected}, got {} ({value})",
        value_type(value)
    )
}

fn value_type(value: &toml::Value) -> &'static str {
    match value {
        toml::Value::String(_) => "string",
        toml::Value::Integer(_) => "integer",
        toml::Value::Float(_) => "float",
        toml::Value::Boolean(_) => "boolean",
        toml::Value::Datetime(_) => "datetime",
        toml::Value::Array(_) => "array",
        toml::Value::Table(_) => "table",
    }
}

fn parse_saved_language(value: &str) -> Option<LanguagePreference> {
    if value.trim().eq_ignore_ascii_case("auto") {
        Some(LanguagePreference::Auto)
    } else {
        parse_environment_language(value)
    }
}

fn parse_environment_language(value: &str) -> Option<LanguagePreference> {
    let normalized = value.trim().to_ascii_lowercase().replace('_', "-");
    let tag = normalized.split(['.', '@']).next().unwrap_or_default();
    if tag == "c" || tag == "posix" || tag == "en" || tag.starts_with("en-") {
        Some(LanguagePreference::En)
    } else if tag == "zh" || tag.starts_with("zh-") {
        Some(LanguagePreference::ZhCn)
    } else {
        None
    }
}

fn parse_boolean_environment(value: &str) -> bool {
    value == "1" || value.eq_ignore_ascii_case("true")
}

#[cfg(test)]
mod tests {
    use super::super::ThemeChoice;
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    static NEXT_TEST_FILE: AtomicU64 = AtomicU64::new(0);

    fn test_path(name: &str) -> PathBuf {
        let sequence = NEXT_TEST_FILE.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "cdh-settings-{name}-{}-{sequence}.toml",
            std::process::id()
        ))
    }

    fn load_text(name: &str, text: &str, environment: UiEnvironment) -> SettingsLoad {
        let path = test_path(name);
        fs::write(&path, text).unwrap();
        let loaded = UiSettings::load(path.clone(), environment);
        let _ = fs::remove_file(path);
        loaded
    }

    fn test_settings_path(name: &str) -> PathBuf {
        let directory = test_path(name);
        fs::create_dir_all(&directory).unwrap();
        directory.join("tui.toml")
    }

    #[test]
    fn settings_persistence_round_trips_all_fields_with_exact_language_tokens() {
        let cases = [
            (LanguagePreference::Auto, "auto"),
            (LanguagePreference::ZhCn, "zh-CN"),
            (LanguagePreference::En, "en"),
        ];

        for (index, (language, token)) in cases.into_iter().enumerate() {
            let path = test_settings_path(&format!("round-trip-{index}"));
            let candidate = UiPreferences {
                language,
                preview: true,
                color: false,
                mouse: false,
                theme: ThemeChoice::Graphite,
            };
            let mut settings = UiSettings::load(path.clone(), UiEnvironment::default()).settings;

            settings.persist(candidate).unwrap();

            assert_eq!(
                fs::read_to_string(&path).unwrap(),
                format!("language = \"{token}\"\ntheme = \"graphite\"\npreview = true\ncolor = false\nmouse = false\n")
            );
            let reloaded = UiSettings::load(path.clone(), UiEnvironment::default());
            assert_eq!(reloaded.settings.saved(), candidate);
            assert!(reloaded.warning.is_none());
            fs::remove_dir_all(path.parent().unwrap()).unwrap();
        }
    }

    #[test]
    fn settings_persistence_round_trips_theme_tokens() {
        let cases = [
            ThemeChoice::Graphite,
            ThemeChoice::Nord,
            ThemeChoice::Daylight,
            ThemeChoice::Mono,
            ThemeChoice::Dracula,
            ThemeChoice::Amber,
            ThemeChoice::Forest,
        ];

        for (index, theme) in cases.into_iter().enumerate() {
            let path = test_settings_path(&format!("theme-round-trip-{index}"));
            let candidate = UiPreferences {
                language: LanguagePreference::En,
                preview: false,
                color: true,
                mouse: true,
                theme,
            };
            let mut settings = UiSettings::load(path.clone(), UiEnvironment::default()).settings;
            settings.persist(candidate).unwrap();
            assert_eq!(
                fs::read_to_string(&path).unwrap(),
                format!(
                    "language = \"en\"\ntheme = \"{}\"\npreview = false\ncolor = true\nmouse = true\n",
                    theme.tag()
                )
            );
            let reloaded = UiSettings::load(path.clone(), UiEnvironment::default());
            assert_eq!(reloaded.settings.saved(), candidate);
            assert!(reloaded.warning.is_none());
            fs::remove_dir_all(path.parent().unwrap()).unwrap();
        }
    }

    #[test]
    fn settings_persistence_candidate_updates_saved_and_effective_only_after_success() {
        let path = test_settings_path("candidate-success");
        let environment = UiEnvironment {
            preview: Some(false),
            ..UiEnvironment::default()
        };
        fs::write(
            &path,
            "language = \"auto\"\npreview = true\ncolor = true\nmouse = true\n",
        )
        .unwrap();
        let mut settings = UiSettings::load(path.clone(), environment).settings;
        let candidate = settings.candidate(SettingKey::Color, 1).unwrap();

        assert!(settings.saved().preview);
        assert!(!settings.effective().preview);

        settings.persist(candidate).unwrap();

        assert_eq!(settings.saved(), candidate);
        assert!(settings.saved().preview);
        assert_eq!(settings.effective().color, candidate.color);
        assert!(!settings.effective().preview);
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn settings_persistence_replacement_failure_preserves_memory_and_disk() {
        let path = test_settings_path("replacement-failure");
        let original = "language = \"auto\"\npreview = false\ncolor = true\nmouse = true\n";
        fs::write(&path, original).unwrap();
        let mut settings = UiSettings::load(path.clone(), UiEnvironment::default()).settings;
        let candidate = UiPreferences {
            language: LanguagePreference::En,
            preview: true,
            color: false,
            mouse: false,
            theme: ThemeChoice::Graphite,
        };

        let result = settings.persist_with_replace(candidate, |_, _| {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "injected replacement failure",
            ))
        });

        assert!(result.is_err());
        assert_eq!(settings.saved(), UiPreferences::default());
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        let leftovers: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.path() != path)
            .collect();
        assert!(leftovers.is_empty(), "leftover temp files: {leftovers:?}");
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn settings_persistence_retries_after_stale_temp_collision() {
        let path = test_settings_path("stale-temp-collision");
        let mut attempted_paths = Vec::new();

        let (temp_path, temp_file) = open_unique_temp_with(&path, |candidate_path| {
            attempted_paths.push(candidate_path.to_path_buf());
            if attempted_paths.len() == 1 {
                fs::write(candidate_path, "stale").unwrap();
                Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "injected stale temp collision",
                ))
            } else {
                OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .open(candidate_path)
            }
        })
        .unwrap();
        drop(temp_file);

        assert_eq!(attempted_paths.len(), 2);
        assert_ne!(attempted_paths[0], attempted_paths[1]);
        assert_eq!(temp_path, attempted_paths[1]);
        assert_eq!(fs::read_to_string(&attempted_paths[0]).unwrap(), "stale");
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn settings_persistence_bounds_stale_temp_collision_retries() {
        let path = test_path("bounded-temp-collisions");
        let mut attempts = 0;

        let error = open_unique_temp_with(&path, |_| {
            attempts += 1;
            Err::<(), _>(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "injected stale temp collision",
            ))
        })
        .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(attempts, MAX_TEMP_CREATE_ATTEMPTS);
    }

    #[test]
    fn settings_persistence_propagates_noncollision_temp_error_without_retry() {
        let path = test_path("noncollision-temp-error");
        let mut attempts = 0;

        let error = open_unique_temp_with(&path, |_| {
            attempts += 1;
            Err::<(), _>(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "injected temp creation failure",
            ))
        })
        .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(attempts, 1);
    }

    #[test]
    fn settings_persistence_uses_unique_temp_names_under_concurrent_writes() {
        let path = Arc::new(test_settings_path("concurrent-temp-names"));
        let handles: Vec<_> = (0..64)
            .map(|_| {
                let path = Arc::clone(&path);
                std::thread::spawn(move || {
                    let mut settings =
                        UiSettings::load(path.as_ref().clone(), UiEnvironment::default()).settings;
                    settings.persist(UiPreferences::default())
                })
            })
            .collect();

        for handle in handles {
            handle.join().unwrap().unwrap();
        }
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn settings_persistence_success_leaves_no_temp_files() {
        let path = test_settings_path("temp-cleanup");
        let mut settings = UiSettings::load(path.clone(), UiEnvironment::default()).settings;

        settings.persist(UiPreferences::default()).unwrap();

        let leftovers: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .filter(|name| {
                let name = name.to_string_lossy();
                name.starts_with(".tui.toml.") && name.ends_with(".tmp")
            })
            .collect();
        assert!(leftovers.is_empty(), "leftover temp files: {leftovers:?}");
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn settings_parser_missing_file_uses_defaults_without_warning() {
        let path = test_path("missing");
        let loaded = UiSettings::load(path.clone(), UiEnvironment::default());

        assert_eq!(loaded.settings.path(), path.as_path());
        assert_eq!(loaded.settings.saved(), UiPreferences::default());
        assert_eq!(loaded.settings.effective(), UiPreferences::default());
        assert!(loaded.warning.is_none());
    }

    #[test]
    fn settings_parser_valid_toml_loads_all_preferences() {
        let loaded = load_text(
            "valid",
            "language = \"zh-CN\"\ntheme = \"nord\"\npreview = true\ncolor = false\nmouse = false\n",
            UiEnvironment::default(),
        );

        assert_eq!(
            loaded.settings.saved(),
            UiPreferences {
                language: LanguagePreference::ZhCn,
                preview: true,
                color: false,
                mouse: false,
                theme: ThemeChoice::Nord,
            }
        );
        assert!(loaded.warning.is_none());
    }

    #[test]
    fn settings_parser_invalid_known_values_fall_back_independently() {
        let cases = [
            (
                "language = \"fr\"\npreview = true\ncolor = false\nmouse = false\n",
                SettingKey::Language,
            ),
            (
                "language = \"en\"\ntheme = \"solarized\"\npreview = true\ncolor = false\nmouse = false\n",
                SettingKey::Theme,
            ),
            (
                "language = \"en\"\npreview = \"yes\"\ncolor = false\nmouse = false\n",
                SettingKey::Preview,
            ),
            (
                "language = \"en\"\npreview = true\ncolor = 1\nmouse = false\n",
                SettingKey::Color,
            ),
            (
                "language = \"en\"\npreview = true\ncolor = false\nmouse = []\n",
                SettingKey::Mouse,
            ),
        ];

        for (index, (text, invalid)) in cases.into_iter().enumerate() {
            let name = format!("invalid-{index}");
            let loaded = load_text(&name, text, UiEnvironment::default());
            let saved = loaded.settings.saved();
            let warning = loaded.warning.expect("invalid known value should warn");
            assert!(warning.contains(&name));
            assert!(warning.contains(match invalid {
                SettingKey::Language => "fr",
                SettingKey::Theme => "solarized",
                SettingKey::Preview => "yes",
                SettingKey::Color => "integer",
                SettingKey::Mouse => "array",
            }));
            assert_eq!(
                saved.language,
                if invalid == SettingKey::Language {
                    LanguagePreference::Auto
                } else {
                    LanguagePreference::En
                }
            );
            // invalid theme falls back to default Graphite; other cases never set theme.
            assert_eq!(saved.theme, ThemeChoice::Graphite);
            assert_eq!(saved.preview, invalid != SettingKey::Preview);
            assert_eq!(saved.color, invalid == SettingKey::Color);
            assert_eq!(saved.mouse, invalid == SettingKey::Mouse);
        }
    }

    #[test]
    fn settings_parser_malformed_toml_returns_defaults_with_warning() {
        let loaded = load_text(
            "malformed",
            "language = [\npreview = true",
            UiEnvironment::default(),
        );

        assert_eq!(loaded.settings.saved(), UiPreferences::default());
        let warning = loaded.warning.expect("malformed TOML should warn");
        assert!(warning.contains("malformed"));
        assert!(warning.contains("parse"));
    }

    #[test]
    fn settings_parser_read_error_includes_path_and_reason() {
        let path = test_path("read-error");
        fs::create_dir(&path).unwrap();

        let loaded = UiSettings::load(path.clone(), UiEnvironment::default());
        let _ = fs::remove_dir(&path);

        let warning = loaded.warning.expect("read error should warn");
        assert!(warning.contains(path.to_string_lossy().as_ref()));
        assert!(warning.contains("read"));
    }

    #[test]
    fn settings_parser_unknown_keys_are_ignored() {
        let loaded = load_text(
            "unknown",
            "language = \"en\"\npreview = true\ncolor = false\nmouse = false\nfuture = { value = 7 }\n",
            UiEnvironment::default(),
        );

        assert_eq!(loaded.settings.saved().language, LanguagePreference::En);
        assert!(loaded.warning.is_none());
    }

    #[test]
    fn settings_parser_environment_precedence_and_locks_are_per_key() {
        let environment = UiEnvironment {
            language: Some(LanguagePreference::ZhCn),
            preview: Some(false),
            color: None,
            mouse: Some(true),
            theme: Some(ThemeChoice::Daylight),
        };
        let loaded = load_text(
            "precedence",
            "language = \"en\"\ntheme = \"nord\"\npreview = true\ncolor = false\nmouse = false\n",
            environment,
        );

        assert_eq!(
            loaded.settings.effective(),
            UiPreferences {
                language: LanguagePreference::ZhCn,
                preview: false,
                color: false,
                mouse: true,
                theme: ThemeChoice::Daylight,
            }
        );
        assert_eq!(loaded.settings.saved().theme, ThemeChoice::Nord);
        assert!(loaded.settings.is_locked(SettingKey::Language));
        assert!(loaded.settings.is_locked(SettingKey::Theme));
        assert!(loaded.settings.is_locked(SettingKey::Preview));
        assert!(!loaded.settings.is_locked(SettingKey::Color));
        assert!(loaded.settings.is_locked(SettingKey::Mouse));
        assert!(loaded.settings.candidate(SettingKey::Preview, 1).is_none());
        assert!(loaded.settings.candidate(SettingKey::Theme, 1).is_none());
        assert!(
            loaded
                .settings
                .candidate(SettingKey::Color, 1)
                .unwrap()
                .color
        );
    }

    #[test]
    fn settings_parser_invalid_cdh_lang_is_ignored_and_unlocked() {
        let environment = UiEnvironment::from_values(Some("fr-FR"), None, None, None, None);
        let loaded = load_text("invalid-language-env", "language = \"en\"\n", environment);

        assert_eq!(loaded.settings.effective().language, LanguagePreference::En);
        assert!(!loaded.settings.is_locked(SettingKey::Language));
    }

    #[test]
    fn settings_parser_invalid_cdh_theme_is_ignored_and_unlocked() {
        let environment = UiEnvironment::from_values(None, None, None, None, Some("solarized"));
        assert_eq!(environment.theme, None);
        let loaded = load_text("invalid-theme-env", "theme = \"nord\"\n", environment);
        assert_eq!(loaded.settings.effective().theme, ThemeChoice::Nord);
        assert!(!loaded.settings.is_locked(SettingKey::Theme));
        assert_eq!(
            loaded
                .settings
                .candidate(SettingKey::Theme, 1)
                .unwrap()
                .theme,
            ThemeChoice::Daylight
        );
    }

    #[test]
    fn settings_parser_existing_boolean_environment_semantics_are_preserved() {
        for (value, expected) in [
            ("1", true),
            ("true", true),
            ("TRUE", true),
            ("0", false),
            ("false", false),
            ("yes", false),
            ("", false),
        ] {
            let environment =
                UiEnvironment::from_values(None, Some(value), Some(value), Some(value), None);
            assert_eq!(environment.preview, Some(expected), "preview: {value:?}");
            assert_eq!(environment.color, Some(expected), "color: {value:?}");
            assert_eq!(environment.mouse, Some(expected), "mouse: {value:?}");
        }
    }
}
