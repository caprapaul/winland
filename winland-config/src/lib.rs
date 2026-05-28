use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use winland_core::{
    LayoutConfig, LayoutKind, MonitorId, TextMatcher, WindowRule, WindowRuleAction,
    WindowRuleMatch, WorkspaceId,
};

pub const DEFAULT_FILE_NAME: &str = "winland.toml";
pub const SUPPORTED_LAYOUTS: &[&str] = &[
    "master-stack",
    "dwindle",
    "vertical-stack",
    "horizontal-stack",
];

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub general: GeneralConfig,
    pub hotkeys: HotkeysConfig,
    pub layout: LayoutSection,
    pub workspaces: WorkspacesConfig,
    pub behavior: BehaviorConfig,
    #[serde(rename = "window_rules")]
    pub window_rules: Vec<WindowRuleConfig>,
}

impl Config {
    pub fn validate(&self) -> Result<(), ConfigError> {
        let mut errors = Vec::new();

        validate_general(&self.general, &mut errors);
        validate_layout(&self.layout, &mut errors);
        validate_workspaces(&self.workspaces, &mut errors);
        validate_hotkeys(&self.hotkeys, self.workspaces.count, &mut errors);
        validate_window_rules(
            &self.window_rules,
            self.workspaces.count,
            &self.layout,
            &mut errors,
        );

        if errors.is_empty() {
            Ok(())
        } else {
            Err(ConfigError::Validation(ValidationErrors(errors)))
        }
    }

    pub fn layout_config(&self) -> LayoutConfig {
        LayoutConfig {
            kind: LayoutKind::from_name(&self.layout.default).unwrap_or_default(),
            gap: self.layout.gap as i32,
            border: self.layout.border as i32,
            master_ratio_percent: self.layout.master_ratio_percent,
            smart_split: self.layout.smart_split,
            preserve_split: self.layout.preserve_split,
        }
        .normalized()
    }

    pub fn layout_config_for_monitor(
        &self,
        monitor: MonitorId,
        is_primary: bool,
        workspace: WorkspaceId,
    ) -> LayoutConfig {
        let mut layout = self
            .layout
            .per_workspace
            .get(&workspace.0.to_string())
            .map(|override_config| merge_layout_override(self.layout_config(), override_config))
            .unwrap_or_else(|| self.layout_config());

        if is_primary && let Some(override_config) = self.layout.per_monitor.get("primary") {
            layout = merge_layout_override(self.layout_config(), override_config);
        }

        self.layout
            .per_monitor
            .get(&monitor.to_string())
            .map(|override_config| merge_layout_override(self.layout_config(), override_config))
            .unwrap_or(layout)
            .normalized()
    }

    pub fn workspace_count(&self) -> u16 {
        self.workspaces.count
    }

    pub fn window_rules(&self) -> Result<Vec<WindowRule>, ConfigError> {
        self.window_rules
            .iter()
            .map(WindowRuleConfig::to_core_rule)
            .collect()
    }
}

fn merge_layout_override(base: LayoutConfig, override_config: &LayoutOverride) -> LayoutConfig {
    LayoutConfig {
        kind: override_config
            .layout
            .as_deref()
            .and_then(LayoutKind::from_name)
            .unwrap_or(base.kind),
        gap: override_config.gap.map(i32::from).unwrap_or(base.gap),
        border: override_config.border.map(i32::from).unwrap_or(base.border),
        master_ratio_percent: override_config
            .master_ratio_percent
            .unwrap_or(base.master_ratio_percent),
        smart_split: override_config.smart_split.unwrap_or(base.smart_split),
        preserve_split: override_config
            .preserve_split
            .unwrap_or(base.preserve_split),
    }
    .normalized()
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GeneralConfig {
    pub log_level: String,
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            log_level: "info".to_owned(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HotkeysConfig {
    pub mode: HotkeyMode,
    pub panic_hotkey: String,
    pub override_latency_budget_micros: u64,
    pub bypass: HotkeyBypassConfig,
    pub bindings: Vec<HotkeyBindingConfig>,
}

impl Default for HotkeysConfig {
    fn default() -> Self {
        Self {
            mode: HotkeyMode::AdvancedInterception,
            panic_hotkey: "Ctrl+Alt+Shift+P".to_owned(),
            override_latency_budget_micros: 250,
            bypass: HotkeyBypassConfig::default(),
            bindings: default_hotkey_bindings(),
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum HotkeyMode {
    #[default]
    Normal,
    AdvancedInterception,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct HotkeyBypassConfig {
    pub fullscreen: bool,
    pub class: Vec<TextMatcherConfig>,
    pub executable_path: Vec<TextMatcherConfig>,
    pub process_name: Vec<TextMatcherConfig>,
}

impl Default for HotkeyBypassConfig {
    fn default() -> Self {
        Self {
            fullscreen: true,
            class: Vec::new(),
            executable_path: Vec::new(),
            process_name: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HotkeyBindingConfig {
    pub keys: String,
    pub command: Option<String>,
    pub launch: Option<String>,
    #[serde(default)]
    pub override_app: bool,
}

impl HotkeyBindingConfig {
    pub fn chord(&self) -> Result<HotkeyChord, ConfigError> {
        parse_hotkey_chord(&self.keys)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LayoutSection {
    pub default: String,
    pub gap: u16,
    pub border: u16,
    pub master_ratio_percent: u8,
    pub smart_split: bool,
    pub preserve_split: bool,
    pub per_monitor: BTreeMap<String, LayoutOverride>,
    pub per_workspace: BTreeMap<String, LayoutOverride>,
}

impl Default for LayoutSection {
    fn default() -> Self {
        Self {
            default: LayoutKind::MasterStack.name().to_owned(),
            gap: 0,
            border: 0,
            master_ratio_percent: 50,
            smart_split: false,
            preserve_split: false,
            per_monitor: BTreeMap::new(),
            per_workspace: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct LayoutOverride {
    pub layout: Option<String>,
    pub gap: Option<u16>,
    pub border: Option<u16>,
    pub master_ratio_percent: Option<u8>,
    pub smart_split: Option<bool>,
    pub preserve_split: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WorkspacesConfig {
    pub count: u16,
    pub names: Vec<String>,
    pub initial_monitor: BTreeMap<String, String>,
    pub startup: WorkspaceStartup,
}

impl Default for WorkspacesConfig {
    fn default() -> Self {
        Self {
            count: 9,
            names: Vec::new(),
            initial_monitor: BTreeMap::new(),
            startup: WorkspaceStartup::KeepCurrent,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum WorkspaceStartup {
    #[default]
    KeepCurrent,
    First,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BehaviorConfig {
    pub startup_retile: bool,
    pub dynamic_retile: bool,
    pub drag_to_float: bool,
    pub retile_on_drag_end: bool,
    pub overflow_focus_policy: OverflowFocusPolicy,
    pub focus_follows_mouse: bool,
    pub restore_previous_placement: bool,
    pub manage_minimized_windows: bool,
    pub avoid_fullscreen_windows: bool,
}

impl Default for BehaviorConfig {
    fn default() -> Self {
        Self {
            startup_retile: true,
            dynamic_retile: true,
            drag_to_float: true,
            retile_on_drag_end: true,
            overflow_focus_policy: OverflowFocusPolicy::TileFocused,
            focus_follows_mouse: false,
            restore_previous_placement: true,
            manage_minimized_windows: false,
            avoid_fullscreen_windows: true,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum OverflowFocusPolicy {
    #[default]
    TileFocused,
    FloatFocused,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WindowRuleConfig {
    pub name: Option<String>,
    #[serde(rename = "match")]
    pub matcher: WindowRuleMatchConfig,
    pub action: WindowRuleActionConfig,
}

impl WindowRuleConfig {
    pub fn to_core_rule(&self) -> Result<WindowRule, ConfigError> {
        Ok(WindowRule {
            name: self
                .name
                .clone()
                .unwrap_or_else(|| "unnamed window rule".to_owned()),
            matcher: self.matcher.to_core_match()?,
            action: self.action.to_core_action(),
        })
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct WindowRuleMatchConfig {
    pub class: Option<TextMatcherConfig>,
    pub title: Option<TextMatcherConfig>,
    pub executable_path: Option<TextMatcherConfig>,
    pub process_name: Option<TextMatcherConfig>,
}

impl WindowRuleMatchConfig {
    fn to_core_match(&self) -> Result<WindowRuleMatch, ConfigError> {
        Ok(WindowRuleMatch {
            class_name: self
                .class
                .as_ref()
                .map(TextMatcherConfig::to_core)
                .transpose()?,
            title: self
                .title
                .as_ref()
                .map(TextMatcherConfig::to_core)
                .transpose()?,
            executable_path: self
                .executable_path
                .as_ref()
                .map(TextMatcherConfig::to_core)
                .transpose()?,
            process_name: self
                .process_name
                .as_ref()
                .map(TextMatcherConfig::to_core)
                .transpose()?,
        })
    }

    fn is_empty(&self) -> bool {
        self.class.is_none()
            && self.title.is_none()
            && self.executable_path.is_none()
            && self.process_name.is_none()
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct WindowRuleActionConfig {
    pub manage: Option<bool>,
    pub float: Option<bool>,
    pub workspace: Option<u16>,
    pub always_on_workspace: Option<bool>,
    pub layout: Option<String>,
}

impl WindowRuleActionConfig {
    fn to_core_action(&self) -> WindowRuleAction {
        WindowRuleAction {
            manage: self.manage,
            float: self.float,
            target_workspace: self.workspace.map(WorkspaceId),
            always_on_workspace: self.always_on_workspace,
            layout: self.layout.clone(),
        }
    }

    fn is_empty(&self) -> bool {
        self.manage.is_none()
            && self.float.is_none()
            && self.workspace.is_none()
            && self.always_on_workspace.is_none()
            && self.layout.is_none()
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum TextMatcherConfig {
    Exact(String),
    Detailed(TextMatcherFields),
}

impl TextMatcherConfig {
    pub fn to_core(&self) -> Result<TextMatcher, ConfigError> {
        match self {
            Self::Exact(value) => Ok(TextMatcher::Exact(value.clone())),
            Self::Detailed(fields) => fields.to_core(),
        }
    }

    fn validate(&self, context: String, errors: &mut Vec<String>) {
        match self {
            Self::Exact(value) if value.trim().is_empty() => {
                errors.push(format!("{context} must not be empty"));
            }
            Self::Exact(_) => {}
            Self::Detailed(fields) => fields.validate(context, errors),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Default, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct TextMatcherFields {
    pub exact: Option<String>,
    pub contains: Option<String>,
    pub prefix: Option<String>,
    pub suffix: Option<String>,
}

impl TextMatcherFields {
    fn to_core(&self) -> Result<TextMatcher, ConfigError> {
        let mut values = [
            self.exact
                .as_ref()
                .map(|value| TextMatcher::Exact(value.clone())),
            self.contains
                .as_ref()
                .map(|value| TextMatcher::Contains(value.clone())),
            self.prefix
                .as_ref()
                .map(|value| TextMatcher::Prefix(value.clone())),
            self.suffix
                .as_ref()
                .map(|value| TextMatcher::Suffix(value.clone())),
        ]
        .into_iter()
        .flatten();

        let Some(matcher) = values.next() else {
            return Err(ConfigError::Validation(ValidationErrors(vec![
                "text matcher must set exactly one of exact, contains, prefix, or suffix"
                    .to_owned(),
            ])));
        };

        if values.next().is_some() {
            return Err(ConfigError::Validation(ValidationErrors(vec![
                "text matcher must set exactly one of exact, contains, prefix, or suffix"
                    .to_owned(),
            ])));
        }

        Ok(matcher)
    }

    fn validate(&self, context: String, errors: &mut Vec<String>) {
        let values = [
            self.exact.as_deref(),
            self.contains.as_deref(),
            self.prefix.as_deref(),
            self.suffix.as_deref(),
        ];
        let set_count = values.iter().filter(|value| value.is_some()).count();
        if set_count != 1 {
            errors.push(format!(
                "{context} must set exactly one of exact, contains, prefix, or suffix"
            ));
        }

        for value in values.into_iter().flatten() {
            if value.trim().is_empty() {
                errors.push(format!("{context} must not contain an empty matcher"));
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct HotkeyChord {
    pub modifiers: BTreeSet<HotkeyModifier>,
    pub key: HotkeyKey,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum HotkeyModifier {
    Alt,
    Control,
    Shift,
    Super,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum HotkeyKey {
    Character(char),
    ArrowLeft,
    ArrowDown,
    ArrowUp,
    ArrowRight,
    Escape,
    Space,
}

#[derive(Debug, Clone)]
pub struct LoadedConfig {
    pub config: Config,
    pub path: Option<PathBuf>,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read config file {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse config TOML")]
    Parse(#[from] toml::de::Error),
    #[error("invalid config:\n{0}")]
    Validation(ValidationErrors),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationErrors(pub Vec<String>);

impl std::fmt::Display for ValidationErrors {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (index, error) in self.0.iter().enumerate() {
            if index > 0 {
                writeln!(f)?;
            }
            write!(f, "- {error}")?;
        }
        Ok(())
    }
}

pub fn parse_toml(input: &str) -> Result<Config, ConfigError> {
    let config: Config = toml::from_str(input)?;
    config.validate()?;
    Ok(config)
}

pub fn load_or_default(explicit_path: Option<&Path>) -> Result<LoadedConfig, ConfigError> {
    if let Some(path) = explicit_path {
        return load_path(path);
    }

    if let Some(path) = discover_config_path() {
        load_path(&path)
    } else {
        Ok(LoadedConfig {
            config: Config::default(),
            path: None,
        })
    }
}

pub fn load_path(path: &Path) -> Result<LoadedConfig, ConfigError> {
    let input = fs::read_to_string(path).map_err(|source| ConfigError::Read {
        path: path.to_owned(),
        source,
    })?;
    let config = parse_toml(&input)?;
    Ok(LoadedConfig {
        config,
        path: Some(path.to_owned()),
    })
}

pub fn discover_config_path() -> Option<PathBuf> {
    candidate_config_paths()
        .into_iter()
        .find(|path| path.is_file())
}

pub fn candidate_config_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();

    if let Some(path) = env::var_os("WINLAND_CONFIG") {
        paths.push(PathBuf::from(path));
    }

    if let Some(appdata) = env::var_os("APPDATA") {
        paths.push(
            PathBuf::from(appdata)
                .join("winland")
                .join(DEFAULT_FILE_NAME),
        );
    }

    if let Some(profile) = env::var_os("USERPROFILE") {
        paths.push(
            PathBuf::from(profile)
                .join(".config")
                .join("winland")
                .join(DEFAULT_FILE_NAME),
        );
    }

    if let Ok(current_exe) = env::current_exe()
        && let Some(exe_dir) = current_exe.parent()
    {
        paths.push(exe_dir.join(DEFAULT_FILE_NAME));
    }

    if let Ok(current_dir) = env::current_dir() {
        paths.push(current_dir.join(DEFAULT_FILE_NAME));
    }

    paths
}

pub fn parse_hotkey_chord(input: &str) -> Result<HotkeyChord, ConfigError> {
    let mut parts: Vec<_> = input
        .split('+')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect();
    if parts.is_empty() {
        return Err(ConfigError::Validation(ValidationErrors(vec![
            "hotkey must include a key".to_owned(),
        ])));
    }

    let key_part = parts.pop().expect("parts is not empty");
    let mut modifiers = BTreeSet::new();
    for modifier in parts {
        let parsed = match modifier.to_ascii_lowercase().as_str() {
            "alt" => HotkeyModifier::Alt,
            "ctrl" | "control" => HotkeyModifier::Control,
            "shift" => HotkeyModifier::Shift,
            "win" | "super" | "windows" => HotkeyModifier::Super,
            _ => {
                return Err(ConfigError::Validation(ValidationErrors(vec![format!(
                    "unsupported hotkey modifier '{modifier}'"
                )])));
            }
        };

        if !modifiers.insert(parsed) {
            return Err(ConfigError::Validation(ValidationErrors(vec![format!(
                "duplicate hotkey modifier '{modifier}'"
            )])));
        }
    }

    let key = parse_hotkey_key(key_part)?;
    Ok(HotkeyChord { modifiers, key })
}

fn parse_hotkey_key(input: &str) -> Result<HotkeyKey, ConfigError> {
    match input.to_ascii_lowercase().as_str() {
        "left" | "arrowleft" | "leftarrow" => Ok(HotkeyKey::ArrowLeft),
        "down" | "arrowdown" | "downarrow" => Ok(HotkeyKey::ArrowDown),
        "up" | "arrowup" | "uparrow" => Ok(HotkeyKey::ArrowUp),
        "right" | "arrowright" | "rightarrow" => Ok(HotkeyKey::ArrowRight),
        "esc" | "escape" => Ok(HotkeyKey::Escape),
        "space" => Ok(HotkeyKey::Space),
        _ => {
            let mut chars = input.chars();
            let Some(ch) = chars.next() else {
                return Err(ConfigError::Validation(ValidationErrors(vec![
                    "hotkey key must not be empty".to_owned(),
                ])));
            };
            if chars.next().is_some() || !ch.is_ascii_alphanumeric() {
                return Err(ConfigError::Validation(ValidationErrors(vec![format!(
                    "unsupported hotkey key '{input}'"
                )])));
            }

            Ok(HotkeyKey::Character(ch.to_ascii_uppercase()))
        }
    }
}

fn validate_general(general: &GeneralConfig, errors: &mut Vec<String>) {
    const LEVELS: &[&str] = &["trace", "debug", "info", "warn", "error", "off"];
    if !LEVELS.contains(&general.log_level.as_str()) {
        errors.push(format!(
            "general.log_level must be one of {}",
            LEVELS.join(", ")
        ));
    }
}

fn validate_layout(layout: &LayoutSection, errors: &mut Vec<String>) {
    validate_layout_name("layout.default", &layout.default, errors);
    validate_gap("layout.gap", layout.gap, errors);
    validate_gap("layout.border", layout.border, errors);
    validate_ratio(
        "layout.master_ratio_percent",
        layout.master_ratio_percent,
        errors,
    );
    validate_dwindle_flags(
        "layout",
        &layout.default,
        layout.smart_split,
        layout.preserve_split,
        errors,
    );

    for (monitor, override_config) in &layout.per_monitor {
        validate_layout_override(
            &format!("layout.per_monitor.{monitor}"),
            &layout.default,
            override_config,
            errors,
        );
    }

    for (workspace, override_config) in &layout.per_workspace {
        validate_workspace_key(
            &format!("layout.per_workspace.{workspace}"),
            workspace,
            errors,
        );
        validate_layout_override(
            &format!("layout.per_workspace.{workspace}"),
            &layout.default,
            override_config,
            errors,
        );
    }
}

fn validate_layout_override(
    context: &str,
    default_layout: &str,
    value: &LayoutOverride,
    errors: &mut Vec<String>,
) {
    if let Some(layout) = &value.layout {
        validate_layout_name(&format!("{context}.layout"), layout, errors);
    }
    if let Some(gap) = value.gap {
        validate_gap(&format!("{context}.gap"), gap, errors);
    }
    if let Some(border) = value.border {
        validate_gap(&format!("{context}.border"), border, errors);
    }
    if let Some(ratio) = value.master_ratio_percent {
        validate_ratio(&format!("{context}.master_ratio_percent"), ratio, errors);
    }
    let effective_layout = value.layout.as_deref().unwrap_or(default_layout);
    validate_dwindle_flags(
        context,
        effective_layout,
        value.smart_split.unwrap_or(false),
        value.preserve_split.unwrap_or(false),
        errors,
    );
}

fn validate_layout_name(context: &str, layout: &str, errors: &mut Vec<String>) {
    if !SUPPORTED_LAYOUTS.contains(&layout) {
        errors.push(format!(
            "{context} must be one of {}",
            SUPPORTED_LAYOUTS.join(", ")
        ));
    }
}

fn validate_dwindle_flags(
    context: &str,
    layout: &str,
    smart_split: bool,
    preserve_split: bool,
    errors: &mut Vec<String>,
) {
    if layout != LayoutKind::Dwindle.name() && (smart_split || preserve_split) {
        errors.push(format!(
            "{context}.smart_split and {context}.preserve_split only apply to dwindle layout"
        ));
    }
}

fn validate_gap(context: &str, value: u16, errors: &mut Vec<String>) {
    if value > 256 {
        errors.push(format!("{context} must be <= 256"));
    }
}

fn validate_ratio(context: &str, value: u8, errors: &mut Vec<String>) {
    if !(LayoutConfig::MIN_MASTER_RATIO_PERCENT..=LayoutConfig::MAX_MASTER_RATIO_PERCENT)
        .contains(&value)
    {
        errors.push(format!(
            "{context} must be between {} and {}",
            LayoutConfig::MIN_MASTER_RATIO_PERCENT,
            LayoutConfig::MAX_MASTER_RATIO_PERCENT
        ));
    }
}

fn validate_workspaces(workspaces: &WorkspacesConfig, errors: &mut Vec<String>) {
    if !(1..=32).contains(&workspaces.count) {
        errors.push("workspaces.count must be between 1 and 32".to_owned());
    }

    if workspaces.names.len() > workspaces.count as usize {
        errors
            .push("workspaces.names cannot contain more entries than workspaces.count".to_owned());
    }

    let mut seen = BTreeSet::new();
    for (index, name) in workspaces.names.iter().enumerate() {
        if name.trim().is_empty() {
            errors.push(format!("workspaces.names[{index}] must not be empty"));
        }
        if !seen.insert(name.to_ascii_lowercase()) {
            errors.push(format!(
                "workspaces.names[{index}] duplicates an earlier name"
            ));
        }
    }

    for (workspace, monitor) in &workspaces.initial_monitor {
        validate_workspace_key(
            &format!("workspaces.initial_monitor.{workspace}"),
            workspace,
            errors,
        );
        if monitor.trim().is_empty() {
            errors.push(format!(
                "workspaces.initial_monitor.{workspace} must not be empty"
            ));
        }
    }
}

fn validate_hotkeys(hotkeys: &HotkeysConfig, workspace_count: u16, errors: &mut Vec<String>) {
    match parse_hotkey_chord(&hotkeys.panic_hotkey) {
        Ok(chord) => {
            if is_protected_hotkey_chord(&chord) {
                errors.push(
                    "hotkeys.panic_hotkey must not be a protected Windows shortcut".to_owned(),
                );
            }
        }
        Err(ConfigError::Validation(validation)) => {
            for error in validation.0 {
                errors.push(format!("hotkeys.panic_hotkey: {error}"));
            }
        }
        Err(error) => errors.push(format!("hotkeys.panic_hotkey: {error}")),
    }

    if hotkeys.override_latency_budget_micros == 0 {
        errors.push("hotkeys.override_latency_budget_micros must be greater than 0".to_owned());
    }

    validate_hotkey_bypass(&hotkeys.bypass, errors);

    let mut chords = BTreeSet::new();
    for (index, binding) in hotkeys.bindings.iter().enumerate() {
        match binding.chord() {
            Ok(chord) => {
                if !chords.insert(chord.clone()) {
                    errors.push(format!(
                        "hotkeys.bindings[{index}].keys duplicates an earlier binding"
                    ));
                }
                if hotkeys.mode == HotkeyMode::AdvancedInterception
                    && binding.override_app
                    && is_protected_hotkey_chord(&chord)
                {
                    errors.push(format!(
                        "hotkeys.bindings[{index}].keys is a protected Windows shortcut and cannot be overridden"
                    ));
                }
            }
            Err(ConfigError::Validation(validation)) => {
                for error in validation.0 {
                    errors.push(format!("hotkeys.bindings[{index}].keys: {error}"));
                }
            }
            Err(error) => errors.push(format!("hotkeys.bindings[{index}].keys: {error}")),
        }

        validate_hotkey_binding_action(binding, index, workspace_count, errors);
    }
}

fn validate_hotkey_binding_action(
    binding: &HotkeyBindingConfig,
    index: usize,
    workspace_count: u16,
    errors: &mut Vec<String>,
) {
    let action_count =
        usize::from(binding.command.is_some()) + usize::from(binding.launch.is_some());
    if action_count != 1 {
        errors.push(format!(
            "hotkeys.bindings[{index}] must set exactly one of command or launch"
        ));
    }

    if let Some(command) = &binding.command {
        validate_command(
            &format!("hotkeys.bindings[{index}].command"),
            command,
            workspace_count,
            errors,
        );
    }

    if let Some(launch) = &binding.launch
        && launch.trim().is_empty()
    {
        errors.push(format!(
            "hotkeys.bindings[{index}].launch must not be empty"
        ));
    }
}

fn validate_hotkey_bypass(bypass: &HotkeyBypassConfig, errors: &mut Vec<String>) {
    for (index, matcher) in bypass.class.iter().enumerate() {
        matcher.validate(format!("hotkeys.bypass.class[{index}]"), errors);
    }
    for (index, matcher) in bypass.executable_path.iter().enumerate() {
        matcher.validate(format!("hotkeys.bypass.executable_path[{index}]"), errors);
    }
    for (index, matcher) in bypass.process_name.iter().enumerate() {
        matcher.validate(format!("hotkeys.bypass.process_name[{index}]"), errors);
    }
}

fn is_protected_hotkey_chord(chord: &HotkeyChord) -> bool {
    let has_super = chord.modifiers.contains(&HotkeyModifier::Super);
    let has_control = chord.modifiers.contains(&HotkeyModifier::Control);
    let has_alt = chord.modifiers.contains(&HotkeyModifier::Alt);

    matches!(chord.key, HotkeyKey::Character('L')) && has_super
        || matches!(chord.key, HotkeyKey::Escape) && has_control && has_alt
}

fn validate_command(context: &str, command: &str, workspace_count: u16, errors: &mut Vec<String>) {
    if is_supported_static_command(command) {
        return;
    }

    if command_workspace_suffix(command, "switch-workspace-")
        .or_else(|| command_workspace_suffix(command, "move-to-workspace-"))
        .is_some_and(|workspace| (1..=workspace_count).contains(&workspace))
    {
        return;
    }

    errors.push(format!("{context} uses unsupported command '{command}'"));
}

fn is_supported_static_command(command: &str) -> bool {
    matches!(
        command,
        "focus-left"
            | "focus-down"
            | "focus-up"
            | "focus-right"
            | "swap-left"
            | "swap-down"
            | "swap-up"
            | "swap-right"
            | "retile"
            | "toggle-float"
            | "reload"
            | "quit"
    )
}

fn command_workspace_suffix(command: &str, prefix: &str) -> Option<u16> {
    command.strip_prefix(prefix)?.parse().ok()
}

fn validate_window_rules(
    rules: &[WindowRuleConfig],
    workspace_count: u16,
    layout: &LayoutSection,
    errors: &mut Vec<String>,
) {
    for (index, rule) in rules.iter().enumerate() {
        let context = format!("window_rules[{index}]");
        if rule
            .name
            .as_deref()
            .is_some_and(|name| name.trim().is_empty())
        {
            errors.push(format!("{context}.name must not be empty"));
        }
        if rule.matcher.is_empty() {
            errors.push(format!("{context}.match must contain at least one matcher"));
        }
        validate_rule_match(&context, &rule.matcher, errors);
        if rule.action.is_empty() {
            errors.push(format!("{context}.action must contain at least one action"));
        }
        if rule
            .action
            .workspace
            .is_some_and(|workspace| !(1..=workspace_count).contains(&workspace))
        {
            errors.push(format!(
                "{context}.action.workspace must be between 1 and {workspace_count}"
            ));
        }
        if let Some(rule_layout) = &rule.action.layout
            && rule_layout != &layout.default
        {
            errors.push(format!(
                "{context}.action.layout must be '{}' until more layouts exist",
                layout.default
            ));
        }
    }
}

fn validate_rule_match(context: &str, matcher: &WindowRuleMatchConfig, errors: &mut Vec<String>) {
    if let Some(value) = &matcher.class {
        value.validate(format!("{context}.match.class"), errors);
    }
    if let Some(value) = &matcher.title {
        value.validate(format!("{context}.match.title"), errors);
    }
    if let Some(value) = &matcher.executable_path {
        value.validate(format!("{context}.match.executable_path"), errors);
    }
    if let Some(value) = &matcher.process_name {
        value.validate(format!("{context}.match.process_name"), errors);
    }
}

fn validate_workspace_key(context: &str, value: &str, errors: &mut Vec<String>) {
    match value.parse::<u16>() {
        Ok(workspace) if workspace > 0 => {}
        _ => errors.push(format!("{context} must be a positive workspace number")),
    }
}

fn default_hotkey_bindings() -> Vec<HotkeyBindingConfig> {
    let mut bindings = vec![HotkeyBindingConfig {
        keys: "Win+T".to_owned(),
        command: None,
        launch: Some("wt.exe".to_owned()),
        override_app: true,
    }];

    bindings.extend(
        [
            ("Win+Left", "focus-left"),
            ("Win+Down", "focus-down"),
            ("Win+Up", "focus-up"),
            ("Win+Right", "focus-right"),
            ("Win+Shift+Left", "swap-left"),
            ("Win+Shift+Down", "swap-down"),
            ("Win+Shift+Up", "swap-up"),
            ("Win+Shift+Right", "swap-right"),
            ("Win+R", "retile"),
            ("Win+F", "toggle-float"),
            ("Win+C", "reload"),
            ("Win+Q", "quit"),
            ("Win+1", "switch-workspace-1"),
            ("Win+2", "switch-workspace-2"),
            ("Win+3", "switch-workspace-3"),
            ("Win+4", "switch-workspace-4"),
            ("Win+5", "switch-workspace-5"),
            ("Win+6", "switch-workspace-6"),
            ("Win+7", "switch-workspace-7"),
            ("Win+8", "switch-workspace-8"),
            ("Win+9", "switch-workspace-9"),
            ("Win+Shift+1", "move-to-workspace-1"),
            ("Win+Shift+2", "move-to-workspace-2"),
            ("Win+Shift+3", "move-to-workspace-3"),
            ("Win+Shift+4", "move-to-workspace-4"),
            ("Win+Shift+5", "move-to-workspace-5"),
            ("Win+Shift+6", "move-to-workspace-6"),
            ("Win+Shift+7", "move-to-workspace-7"),
            ("Win+Shift+8", "move-to-workspace-8"),
            ("Win+Shift+9", "move-to-workspace-9"),
        ]
        .into_iter()
        .map(|(keys, command)| HotkeyBindingConfig {
            keys: keys.to_owned(),
            command: Some(command.to_owned()),
            launch: None,
            override_app: true,
        }),
    );

    bindings
}

#[cfg(test)]
mod tests {
    use super::*;
    use winland_core::{Rect, WindowHandle, WindowInfo, WindowStyles, evaluate_window_rules};

    #[test]
    fn defaults_are_valid_and_cover_phase_seven_sections() {
        let config = Config::default();

        config.validate().unwrap();

        assert_eq!(config.general.log_level, "info");
        assert_eq!(config.workspace_count(), 9);
        assert_eq!(config.layout_config().master_ratio_percent, 50);
        assert_eq!(config.hotkeys.mode, HotkeyMode::AdvancedInterception);
        assert_eq!(config.hotkeys.panic_hotkey, "Ctrl+Alt+Shift+P");
        assert_eq!(config.hotkeys.override_latency_budget_micros, 250);
        assert!(config.hotkeys.bypass.fullscreen);
        assert_eq!(config.hotkeys.bindings.len(), 31);
        assert_eq!(config.hotkeys.bindings[0].keys, "Win+T");
        assert_eq!(config.hotkeys.bindings[0].launch.as_deref(), Some("wt.exe"));
        assert!(config.hotkeys.bindings[0].override_app);
        assert_eq!(config.hotkeys.bindings[1].keys, "Win+Left");
        assert_eq!(
            config.hotkeys.bindings[1].command.as_deref(),
            Some("focus-left")
        );
        assert!(config.hotkeys.bindings[1].override_app);
        assert!(config.behavior.startup_retile);
        assert!(config.behavior.dynamic_retile);
        assert!(config.behavior.drag_to_float);
        assert!(config.behavior.retile_on_drag_end);
        assert_eq!(
            config.behavior.overflow_focus_policy,
            OverflowFocusPolicy::TileFocused
        );
        assert!(config.window_rules.is_empty());
    }

    #[test]
    fn parses_toml_for_hotkeys_layout_workspaces_behavior_and_rules() {
        let config = parse_toml(
            r#"
            [general]
            log_level = "debug"

            [hotkeys]
            mode = "advanced-interception"
            panic_hotkey = "Ctrl+Alt+Shift+P"
            override_latency_budget_micros = 300
            bypass = { fullscreen = true, process_name = ["game.exe"] }
            bindings = [
              { keys = "Ctrl+Alt+R", command = "retile", override_app = true },
              { keys = "Win+T", launch = "wt.exe" },
              { keys = "Ctrl+Alt+1", command = "switch-workspace-1" },
            ]

            [layout]
            default = "dwindle"
            gap = 8
            border = 1
            master_ratio_percent = 60
            smart_split = true

            [workspaces]
            count = 2
            names = ["main", "chat"]
            initial_monitor = { "1" = "primary" }

            [behavior]
            startup_retile = true
            dynamic_retile = false
            drag_to_float = true
            retile_on_drag_end = false
            overflow_focus_policy = "float-focused"

            [[window_rules]]
            name = "float settings"
            [window_rules.match]
            title = { contains = "Settings" }
            process_name = "SystemSettings.exe"
            [window_rules.action]
            float = true
            workspace = 2
            always_on_workspace = true
            "#,
        )
        .unwrap();

        assert_eq!(config.general.log_level, "debug");
        assert_eq!(config.hotkeys.mode, HotkeyMode::AdvancedInterception);
        assert!(config.hotkeys.bindings[0].override_app);
        assert_eq!(config.hotkeys.bindings[1].launch.as_deref(), Some("wt.exe"));
        assert_eq!(config.hotkeys.override_latency_budget_micros, 300);
        assert_eq!(config.hotkeys.bypass.process_name.len(), 1);
        assert_eq!(config.layout_config().kind, LayoutKind::Dwindle);
        assert_eq!(config.layout_config().gap, 8);
        assert!(config.layout_config().smart_split);
        assert!(config.layout_config().preserve_split);
        assert_eq!(config.workspace_count(), 2);
        assert!(config.behavior.startup_retile);
        assert!(!config.behavior.dynamic_retile);
        assert!(config.behavior.drag_to_float);
        assert!(!config.behavior.retile_on_drag_end);
        assert_eq!(
            config.behavior.overflow_focus_policy,
            OverflowFocusPolicy::FloatFocused
        );
        assert_eq!(config.window_rules().unwrap().len(), 1);
    }

    #[test]
    fn validation_reports_multiple_errors() {
        let error = parse_toml(
            r#"
            [general]
            log_level = "chatty"

            [layout]
            master_ratio_percent = 95

            [workspaces]
            count = 1

            [hotkeys]
            bindings = [
              { keys = "Ctrl+Ctrl+R", command = "retile" },
              { keys = "Ctrl+Alt+2", command = "switch-workspace-2" },
            ]

            [[window_rules]]
            [window_rules.match]
            title = { exact = "" }
            [window_rules.action]
            workspace = 3
            "#,
        )
        .unwrap_err();

        let ConfigError::Validation(errors) = error else {
            panic!("expected validation errors");
        };
        let output = errors.to_string();

        assert!(output.contains("general.log_level"));
        assert!(output.contains("layout.master_ratio_percent"));
        assert!(output.contains("duplicate hotkey modifier"));
        assert!(output.contains("switch-workspace-2"));
        assert!(output.contains("window_rules[0].action.workspace"));
    }

    #[test]
    fn hotkey_chords_are_normalized_for_duplicate_detection() {
        assert_eq!(
            parse_hotkey_chord("control + alt + h").unwrap(),
            parse_hotkey_chord("Ctrl+Alt+H").unwrap()
        );
    }

    #[test]
    fn override_mode_rejects_protected_suppressed_shortcuts() {
        let error = parse_toml(
            r#"
            [hotkeys]
            mode = "advanced-interception"
            panic_hotkey = "Ctrl+Alt+Shift+P"
            bindings = [
              { keys = "Win+L", command = "retile", override_app = true },
            ]
            "#,
        )
        .unwrap_err();

        let ConfigError::Validation(errors) = error else {
            panic!("expected validation errors");
        };

        assert!(
            errors
                .to_string()
                .contains("protected Windows shortcut and cannot be overridden")
        );
    }

    #[test]
    fn hotkey_binding_must_choose_command_or_launch() {
        let error = parse_toml(
            r#"
            [hotkeys]
            bindings = [
              { keys = "Win+T", command = "retile", launch = "wt.exe" },
              { keys = "Win+Y" },
              { keys = "Win+U", launch = "" },
            ]
            "#,
        )
        .unwrap_err();

        let ConfigError::Validation(errors) = error else {
            panic!("expected validation errors");
        };
        let output = errors.to_string();

        assert!(output.contains("must set exactly one of command or launch"));
        assert!(output.contains("hotkeys.bindings[2].launch must not be empty"));
    }

    #[test]
    fn config_rules_convert_to_core_rule_evaluation() {
        let config = parse_toml(
            r#"
            [[window_rules]]
            name = "ignore splash"
            [window_rules.match]
            class = { suffix = "Splash" }
            [window_rules.action]
            manage = false

            [[window_rules]]
            name = "float setup"
            [window_rules.match]
            title = { contains = "Setup" }
            process_name = "installer.exe"
            [window_rules.action]
            manage = true
            float = true
            workspace = 2
            "#,
        )
        .unwrap();
        let rules = config.window_rules().unwrap();
        let decision = evaluate_window_rules(&window(), &rules);

        assert_eq!(decision.manage, Some(true));
        assert_eq!(decision.float, Some(true));
        assert_eq!(decision.target_workspace, Some(WorkspaceId(2)));
        assert_eq!(decision.matched_rules, vec!["float setup"]);
    }

    fn window() -> WindowInfo {
        WindowInfo {
            handle: WindowHandle(1),
            title: "Setup Wizard".to_owned(),
            class_name: "InstallerMain".to_owned(),
            process_id: 42,
            executable_path: Some(r"C:\Temp\installer.exe".to_owned()),
            is_visible: true,
            is_minimized: false,
            is_dwm_cloaked: false,
            has_owner: false,
            is_tool_window: false,
            styles: WindowStyles {
                style: 0,
                extended_style: 0,
            },
            size_constraints: winland_core::WindowSizeConstraints::NONE,
            rect: Rect::from_size(0, 0, 100, 100),
        }
    }
}
