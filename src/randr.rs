//! The wlr-randr model: the outputs a compositor reports, the state text
//! `wlr-randr` prints, and the command lines that change one property of one
//! output.
//!
//! `wlr-randr` (no options) prints every output's state as an indented block:
//!
//! ```text
//! eDP-1 "Some display"
//!   Physical size: 309x174 mm
//!   Enabled: yes
//!   Modes:
//!     2560x1600 px, 60.000000 Hz (preferred, current)
//!     2560x1600 px, 59.970001 Hz
//!   Position: 0,0
//!   Transform: normal
//!   Scale: 1.000000
//!   Adaptive Sync: disabled
//! ```
//!
//! Older versions print `NAME:` with no description, and `Position`, `Transform`,
//! `Scale` and `Adaptive Sync` only appear for enabled outputs. Both shapes parse.
//! One change is one `wlr-randr --output NAME …` run.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The compositor tool this app wraps.
pub const BINARY: &str = "wlr-randr";
/// How long a `wlr-randr` run may take before it is killed.
const TIMEOUT: Duration = Duration::from_secs(5);
/// A dragged display snaps to another's edges within this many logical pixels.
const SNAP_DISTANCE: i32 = 24;

/// An output transform, as `wlr-randr` names it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Transform {
    #[default]
    Normal,
    R90,
    R180,
    R270,
    Flipped,
    Flipped90,
    Flipped180,
    Flipped270,
}

impl Transform {
    /// The eight transforms, in `wlr-randr --transform` order.
    pub const ALL: [Transform; 8] = [
        Transform::Normal,
        Transform::R90,
        Transform::R180,
        Transform::R270,
        Transform::Flipped,
        Transform::Flipped90,
        Transform::Flipped180,
        Transform::Flipped270,
    ];

    /// The word `wlr-randr` prints and takes back.
    pub fn arg(self) -> &'static str {
        match self {
            Transform::Normal => "normal",
            Transform::R90 => "90",
            Transform::R180 => "180",
            Transform::R270 => "270",
            Transform::Flipped => "flipped",
            Transform::Flipped90 => "flipped-90",
            Transform::Flipped180 => "flipped-180",
            Transform::Flipped270 => "flipped-270",
        }
    }

    /// What the settings list shows.
    pub fn label(self) -> &'static str {
        match self {
            Transform::Normal => "Normal",
            Transform::R90 => "90° counter-clockwise",
            Transform::R180 => "180°",
            Transform::R270 => "90° clockwise",
            Transform::Flipped => "Flipped",
            Transform::Flipped90 => "Flipped + 90°",
            Transform::Flipped180 => "Flipped + 180°",
            Transform::Flipped270 => "Flipped + 270°",
        }
    }

    fn parse(word: &str) -> Option<Transform> {
        Transform::ALL.into_iter().find(|t| t.arg() == word)
    }

    /// The settings list's label back to a transform.
    pub fn from_label(label: &str) -> Option<Transform> {
        Transform::ALL.into_iter().find(|t| t.label() == label)
    }

    /// Whether the transform swaps an output's width and height.
    pub fn swaps(self) -> bool {
        matches!(
            self,
            Transform::R90 | Transform::R270 | Transform::Flipped90 | Transform::Flipped270
        )
    }
}

/// One mode an output supports.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Mode {
    pub width: i32,
    pub height: i32,
    /// Refresh rate in Hz, exactly as `wlr-randr` prints it (the compositor's
    /// milli-Hertz divided by 1000); `None` when the mode has none.
    pub refresh: Option<f64>,
    pub preferred: bool,
    pub current: bool,
}

impl Mode {
    /// What the mode list shows: `W × H @ R Hz`, with the refresh rounded
    /// enough to stay unique (modes differ by at least a milli-Hertz).
    pub fn label(self) -> String {
        match self.refresh {
            Some(hz) => format!("{} × {} @ {:.3} Hz", self.width, self.height, hz),
            None => format!("{} × {}", self.width, self.height),
        }
    }

    /// The map card's compact caption, refresh to two decimals at most.
    pub fn short(self) -> String {
        match self.refresh {
            Some(hz) => {
                let mut hz = format!("{hz:.2}");
                while hz.ends_with('0') {
                    hz.pop();
                }
                if hz.ends_with('.') {
                    hz.pop();
                }
                format!("{} × {} @ {hz} Hz", self.width, self.height)
            }
            None => format!("{} × {}", self.width, self.height),
        }
    }

    /// The `--mode` argument that selects this mode again: `WxH` joined with the
    /// printed refresh as `WxH@RHz`, so the compositor's milli-Hertz round-trips
    /// (`wlr-randr` rounds `R * 1000` back to what it printed).
    pub fn arg(self) -> String {
        match self.refresh {
            Some(hz) => format!("{}x{}@{hz}Hz", self.width, self.height),
            None => format!("{}x{}", self.width, self.height),
        }
    }
}

/// The custom-mode text the settings row takes (`--custom-mode`'s syntax):
/// `WxH`, `WxH@R` or `WxH@RHz`, with positive dimensions and refresh.
pub fn parse_mode_text(text: &str) -> Option<(i32, i32, Option<f64>)> {
    let text = text.trim();
    let (dimensions, refresh) = match text.split_once('@') {
        Some((d, r)) => {
            let hz = r.trim().trim_end_matches("Hz").trim().parse::<f64>().ok()?;
            if !hz.is_finite() || hz <= 0.0 {
                return None;
            }
            (d, Some(hz))
        }
        None => (text, None),
    };
    let (width, height) = dimensions.trim().split_once('x')?;
    let width: i32 = width.trim().parse().ok()?;
    let height: i32 = height.trim().parse().ok()?;
    if width <= 0 || height <= 0 {
        return None;
    }
    Some((width, height, refresh))
}

/// One output, as `wlr-randr` reports it.
#[derive(Clone, Debug)]
pub struct Output {
    pub name: String,
    pub description: String,
    /// Physical size in millimetres, when reported.
    pub physical: Option<(i32, i32)>,
    pub enabled: bool,
    pub modes: Vec<Mode>,
    /// Position in logical (already scaled) coordinates; only enabled outputs
    /// have one.
    pub position: Option<(i32, i32)>,
    pub transform: Transform,
    pub scale: f64,
    /// `None` when the compositor does not report adaptive sync at all.
    pub adaptive_sync: Option<bool>,
}

impl Output {
    /// The compositor's current mode, or `None` for a custom mode.
    pub fn current_mode(&self) -> Option<Mode> {
        self.modes.iter().copied().find(|m| m.current)
    }

    /// Whether the output runs a custom mode (`--custom-mode`): enabled with a
    /// position but none of its listed modes current.
    pub fn is_custom(&self) -> bool {
        self.enabled && self.position.is_some() && self.current_mode().is_none()
    }

    /// The output's size in logical points, transform applied then divided by
    /// the scale. A custom mode has no reported size; 1280 × 720 stands in so
    /// the map still draws something sensible.
    pub fn logical(&self) -> (f64, f64) {
        let (w, h) = match self.current_mode() {
            Some(mode) => (mode.width as f64, mode.height as f64),
            None => (1280.0, 720.0),
        };
        let (w, h) = if self.transform.swaps() {
            (h, w)
        } else {
            (w, h)
        };
        (w / self.scale, h / self.scale)
    }

    /// The map and list show the name, plus the description when there is one.
    pub fn label(&self) -> String {
        if self.description.is_empty() {
            self.name.clone()
        } else {
            format!("{} — {}", self.name, self.description)
        }
    }
}

/// One property change for one output.
#[derive(Clone, Debug, PartialEq)]
pub enum Change {
    On,
    Off,
    /// `--mode`: one of the output's listed modes.
    Mode(Mode),
    /// `--custom-mode`: a mode the compositor does not list.
    CustomMode {
        width: i32,
        height: i32,
        refresh: Option<f64>,
    },
    /// `--preferred`: whatever mode is marked preferred.
    Preferred,
    /// `--pos`: logical coordinates.
    Position {
        x: i32,
        y: i32,
    },
    Transform(Transform),
    /// `--scale`: any positive factor.
    Scale(f64),
    /// `--adaptive-sync`.
    AdaptiveSync(bool),
}

impl Change {
    /// The arguments after `--output NAME`.
    pub fn args(&self) -> Vec<String> {
        match self {
            Change::On => vec!["--on".into()],
            Change::Off => vec!["--off".into()],
            Change::Mode(mode) => vec!["--mode".into(), mode.arg()],
            Change::CustomMode {
                width,
                height,
                refresh,
            } => {
                let mode = match refresh {
                    Some(hz) => format!("{width}x{height}@{hz}Hz"),
                    None => format!("{width}x{height}"),
                };
                vec!["--custom-mode".into(), mode]
            }
            Change::Preferred => vec!["--preferred".into()],
            Change::Position { x, y } => vec!["--pos".into(), format!("{x},{y}")],
            Change::Transform(t) => vec!["--transform".into(), t.arg().into()],
            Change::Scale(scale) => vec!["--scale".into(), format_scale(*scale)],
            Change::AdaptiveSync(on) => vec![
                "--adaptive-sync".into(),
                if *on {
                    "enabled".into()
                } else {
                    "disabled".into()
                },
            ],
        }
    }

    /// What the header status says was applied.
    pub fn describe(&self) -> String {
        match self {
            Change::On => "turned on".into(),
            Change::Off => "turned off".into(),
            Change::Mode(mode) => mode.short(),
            Change::CustomMode {
                width,
                height,
                refresh,
            } => match refresh {
                Some(hz) => format!("custom {width} × {height} @ {hz} Hz"),
                None => format!("custom {width} × {height}"),
            },
            Change::Preferred => "preferred mode".into(),
            Change::Position { x, y } => format!("moved to {x},{y}"),
            Change::Transform(t) => t.label().to_lowercase(),
            Change::Scale(scale) => format!("scale {}", format_scale(*scale)),
            Change::AdaptiveSync(true) => "adaptive sync on".into(),
            Change::AdaptiveSync(false) => "adaptive sync off".into(),
        }
    }
}

/// A scale as `wlr-randr` takes it and the list shows it: the shortest exact
/// decimal ("1", "1.25", "2").
pub fn format_scale(scale: f64) -> String {
    let mut text = scale.to_string();
    if !text.contains('.') {
        return text;
    }
    while text.ends_with('0') {
        text.pop();
    }
    if text.ends_with('.') {
        text.pop();
    }
    text
}

/// The full `wlr-randr` argv for changing `output` by `change`.
pub fn command(output: &str, change: &Change) -> Vec<String> {
    let mut args = vec![BINARY.into(), "--output".into(), output.into()];
    args.extend(change.args());
    args
}

/// Runs `wlr-randr` with `args` and returns its stdout, bounded by `TIMEOUT`.
/// `Err` carries stderr (or what went wrong) for the error dialog.
pub fn exec(args: &[String]) -> Result<String, String> {
    let mut child = Command::new(BINARY)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("{BINARY} could not run: {e}"))?;
    // Drain both pipes while the process runs: mode lists can exceed a pipe's
    // capacity, so waiting for exit before reading can deadlock.
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let drain = |mut pipe: Box<dyn Read + Send>| {
        std::thread::spawn(move || {
            let mut text = String::new();
            let _ = pipe.read_to_string(&mut text);
            text
        })
    };
    let out = drain(Box::new(stdout));
    let err = drain(Box::new(stderr));
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let stdout = out.join().unwrap_or_default();
                let stderr = err.join().unwrap_or_default();
                return if status.success() {
                    Ok(stdout)
                } else {
                    let detail = stderr.trim();
                    Err(if detail.is_empty() {
                        format!("{BINARY} exited with {status}")
                    } else {
                        detail.into()
                    })
                };
            }
            Ok(None) => {}
            Err(e) => return Err(format!("{BINARY} could not run: {e}")),
        }
        if started.elapsed() > TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!(
                "{BINARY} did not answer within {} seconds",
                TIMEOUT.as_secs()
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Reads the current output configuration.
pub fn query() -> Result<Vec<Output>, String> {
    parse(&exec(&[])?)
}

/// Parses the state text `wlr-randr` prints. Unknown lines are skipped, so a
/// newer `wlr-randr` that prints extra properties still parses.
pub fn parse(text: &str) -> Result<Vec<Output>, String> {
    let mut outputs: Vec<Output> = Vec::new();
    let mut in_modes = false;
    for line in text.lines() {
        if line.is_empty() {
            continue;
        }
        if !line.starts_with(' ') {
            in_modes = false;
            outputs.push(parse_head(line)?);
            continue;
        }
        let Some(output) = outputs.last_mut() else {
            return Err(format!("indented line before any output: {line:?}"));
        };
        let trimmed = line.trim();
        if let Some(mode) = in_modes.then(|| parse_mode_line(trimmed)).flatten() {
            output.modes.push(mode);
            continue;
        }
        in_modes = trimmed == "Modes:";
        if in_modes {
            continue;
        }
        let Some((key, value)) = split_property(trimmed) else {
            continue;
        };
        match key {
            "Enabled" => output.enabled = value == "yes",
            "Position" => {
                output.position =
                    Some(parse_pair(value).ok_or_else(|| format!("invalid position: {value:?}"))?);
            }
            "Transform" => {
                output.transform = Transform::parse(value)
                    .ok_or_else(|| format!("invalid transform: {value:?}"))?;
            }
            "Scale" => {
                output.scale = value
                    .parse()
                    .map_err(|_| format!("invalid scale: {value:?}"))?;
                if !output.scale.is_finite() || output.scale <= 0.0 {
                    return Err(format!("invalid scale: {value:?}"));
                }
            }
            "Adaptive Sync" => {
                output.adaptive_sync = Some(match value {
                    "enabled" => true,
                    "disabled" => false,
                    other => return Err(format!("invalid adaptive sync: {other:?}")),
                });
            }
            "Physical size" => {
                let (w, h) = value
                    .trim_end_matches(" mm")
                    .split_once('x')
                    .and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)))
                    .ok_or_else(|| format!("invalid physical size: {value:?}"))?;
                output.physical = Some((w, h));
            }
            _ => {}
        }
    }
    Ok(outputs)
}

/// The head line: `NAME "Description"` (current `wlr-randr`) or `NAME:`.
fn parse_head(line: &str) -> Result<Output, String> {
    let line = line.trim_end();
    let (name, description) = match (line.find('"'), line.rfind('"')) {
        (Some(start), Some(end)) if start < end => {
            let name = line[..start].trim().trim_end_matches(':');
            let description = unquote(&line[start..=end]);
            (name, description)
        }
        _ => (line.trim_end_matches(':').trim(), String::new()),
    };
    if name.is_empty() {
        return Err(format!("output without a name: {line:?}"));
    }
    Ok(Output {
        name: name.into(),
        description,
        physical: None,
        enabled: false,
        modes: Vec::new(),
        position: None,
        transform: Transform::Normal,
        scale: 1.0,
        adaptive_sync: None,
    })
}

/// The text between the first and last quote, with `\"` and `\\` unescaped.
fn unquote(quoted: &str) -> String {
    let inner = &quoted[1..quoted.len() - 1];
    let mut out = String::new();
    let mut escaped = false;
    for ch in inner.chars() {
        if escaped {
            match ch {
                'n' => out.push('\n'),
                't' => out.push('\t'),
                other => out.push(other),
            }
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else {
            out.push(ch);
        }
    }
    out
}

fn split_property(line: &str) -> Option<(&str, &str)> {
    let (key, value) = line.split_once(':')?;
    Some((key.trim(), value.trim()))
}

/// `X,Y` as `wlr-randr` prints positions and physical sizes.
fn parse_pair(text: &str) -> Option<(i32, i32)> {
    let (x, y) = text.split_once(',')?;
    Some((x.trim().parse().ok()?, y.trim().parse().ok()?))
}

/// A mode line: current `wlr-randr` prints `WxH px[, R Hz][ (preferred[,
/// current])]`; older versions printed `WxH@RHz`. Both parse.
fn parse_mode_line(line: &str) -> Option<Mode> {
    let (base, flags) = match line.rfind('(') {
        Some(at) if line.ends_with(')') => (&line[..at], &line[at + 1..line.len() - 1]),
        _ => (line, ""),
    };
    let base = base.trim();
    let (dimensions, refresh) = if let Some((before, after)) = base.split_once(" px") {
        (before, parse_refresh(after))
    } else if let Some((before, after)) = base.split_once('@') {
        (before, parse_refresh(after))
    } else {
        (base, None)
    };
    let (width, height) = dimensions.trim().split_once('x')?;
    let (width, height): (i32, i32) = (width.trim().parse().ok()?, height.trim().parse().ok()?);
    Some(Mode {
        width,
        height,
        refresh,
        preferred: flags.contains("preferred"),
        current: flags.contains("current"),
    })
}

/// `", 60.000000 Hz"` / `"59.970001Hz"` / `"60"` → `Some(60.0)`; empty → `None`.
fn parse_refresh(text: &str) -> Option<f64> {
    let text = text.trim().trim_start_matches(',').trim();
    if text.is_empty() {
        return None;
    }
    let text = text.trim().trim_end_matches("Hz").trim();
    text.parse().ok()
}

/// Where a display dragged to `x, y` settles: the position itself, or the
/// nearest edge alignment with `others` (each `x, y, width, height`), or the
/// origin, within `SNAP_DISTANCE` logical pixels, per axis.
pub fn snap_position(
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    others: &[(i32, i32, i32, i32)],
) -> (i32, i32) {
    let mut x_candidates = vec![0];
    let mut y_candidates = vec![0];
    for &(ox, oy, ow, oh) in others {
        x_candidates.extend([
            ox,
            ox.saturating_add(ow).saturating_sub(w),
            ox.saturating_add(ow),
            ox.saturating_sub(w),
        ]);
        y_candidates.extend([
            oy,
            oy.saturating_add(oh).saturating_sub(h),
            oy.saturating_add(oh),
            oy.saturating_sub(h),
        ]);
    }
    let snap = |wanted: i32, candidates: &[i32]| {
        candidates
            .iter()
            .filter(|&&c| (i64::from(c) - i64::from(wanted)).abs() <= i64::from(SNAP_DISTANCE))
            .min_by_key(|&&c| (i64::from(c) - i64::from(wanted)).abs())
            .copied()
            .unwrap_or(wanted)
    };
    (snap(x, &x_candidates), snap(y, &y_candidates))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
eDP-1 \"Built-in display\"
  Physical size: 309x174 mm
  Enabled: yes
  Modes:
    2560x1600 px, 60.000000 Hz (preferred, current)
    2560x1600 px, 59.970001 Hz
    1920x1200 px, 60.000000 Hz
  Position: 0,0
  Transform: normal
  Scale: 1.000000
  Adaptive Sync: disabled
DP-1 \"Dell D24\"
  Make: DELL
  Model: D24
  Serial: ABC-123
  Enabled: yes
  Modes:
    1920x1080 px, 143.962997 Hz (current)
    1920x1080 px, 60.000000 Hz (preferred)
  Position: 2560,0
  Transform: 90
  Scale: 1.000000
  Adaptive Sync: enabled
HDMI-A-1 \"Living room TV\"
  Enabled: no
  Modes:
    3840x2160 px, 60.000000 Hz (preferred)
";

    #[test]
    fn parses_current_wlr_randr_output() {
        let outputs = parse(SAMPLE).unwrap();
        assert_eq!(outputs.len(), 3);
        let edp = &outputs[0];
        assert_eq!(edp.name, "eDP-1");
        assert_eq!(edp.description, "Built-in display");
        assert_eq!(edp.physical, Some((309, 174)));
        assert!(edp.enabled);
        assert_eq!(edp.modes.len(), 3);
        assert_eq!(
            edp.modes[0],
            Mode {
                width: 2560,
                height: 1600,
                refresh: Some(60.0),
                preferred: true,
                current: true
            }
        );
        assert_eq!(edp.position, Some((0, 0)));
        assert_eq!(edp.transform, Transform::Normal);
        assert_eq!(edp.scale, 1.0);
        assert_eq!(edp.adaptive_sync, Some(false));

        let dp = &outputs[1];
        assert_eq!(dp.name, "DP-1");
        // `Make`, `Model` and `Serial` are skipped as unknown properties.
        assert_eq!(dp.position, Some((2560, 0)));
        assert_eq!(dp.transform, Transform::R90);
        assert_eq!(dp.adaptive_sync, Some(true));
        assert_eq!(dp.current_mode().unwrap().refresh, Some(143.962997));

        let hdmi = &outputs[2];
        assert!(!hdmi.enabled);
        assert_eq!(hdmi.position, None);
        assert_eq!(hdmi.adaptive_sync, None);
        assert!(hdmi.modes[0].preferred && !hdmi.modes[0].current);
    }

    #[test]
    fn parses_older_wlr_randr_without_descriptions() {
        let outputs = parse("NOOP-1:\n  Enabled: yes\n  Modes:\n    1920x1080 px (current)\n  Position: 0,0\n  Transform: normal\n  Scale: 1.000000\n").unwrap();
        assert_eq!(outputs.len(), 1);
        let output = &outputs[0];
        assert_eq!(output.name, "NOOP-1");
        assert_eq!(output.description, "");
        assert_eq!(
            output.modes[0],
            Mode {
                width: 1920,
                height: 1080,
                refresh: None,
                preferred: false,
                current: true
            }
        );
    }

    #[test]
    fn parses_the_oldest_at_sign_mode_lines() {
        let outputs = parse("NOOP-1:\n  Modes:\n    1920x1080@60Hz (current)\n    1024x768@75Hz\n  Position: 0,0\n  Transform: normal\n  Scale: 1.000000\n  Enabled: yes\n").unwrap();
        let output = &outputs[0];
        assert_eq!(output.modes.len(), 2);
        assert_eq!(output.modes[0].refresh, Some(60.0));
        assert!(output.modes[0].current && !output.modes[1].current);
        assert!(output.enabled, "Enabled last, after Modes, still lands");
    }

    #[test]
    fn empty_text_parses_to_no_outputs() {
        assert!(parse("").unwrap().is_empty());
    }

    #[test]
    fn a_current_mode_round_trips_through_the_command_line() {
        let outputs = parse(SAMPLE).unwrap();
        let current = outputs[1].current_mode().unwrap();
        assert_eq!(current.arg(), "1920x1080@143.962997Hz");
        // The line wlr-randr would print for that argument parses back to it.
        let reparsed = parse_mode_line(&format!(
            "{} px, {:.6} Hz (current)",
            format!("{}x{}", current.width, current.height),
            current.refresh.unwrap()
        ))
        .unwrap();
        assert_eq!(reparsed.width, current.width);
        assert_eq!(reparsed.height, current.height);
        assert_eq!(reparsed.refresh, current.refresh);
        assert!(reparsed.current);
    }

    #[test]
    fn mode_labels_stay_unique_and_short_caps_refresh() {
        let outputs = parse(SAMPLE).unwrap();
        let labels: Vec<String> = outputs[0].modes.iter().map(|m| m.label()).collect();
        assert_eq!(labels.len(), 3);
        let unique: std::collections::HashSet<_> = labels.iter().collect();
        assert_eq!(unique.len(), 3, "labels must distinguish modes");
        assert_eq!(outputs[0].modes[0].short(), "2560 × 1600 @ 60 Hz");
        assert_eq!(outputs[0].modes[1].short(), "2560 × 1600 @ 59.97 Hz");
    }

    #[test]
    fn custom_mode_text_parses_wlr_randr_syntax() {
        assert_eq!(parse_mode_text("1920x1080"), Some((1920, 1080, None)));
        assert_eq!(
            parse_mode_text(" 1920x1080@60 "),
            Some((1920, 1080, Some(60.0)))
        );
        assert_eq!(
            parse_mode_text("800x600@75Hz"),
            Some((800, 600, Some(75.0)))
        );
        assert_eq!(
            parse_mode_text("1920x1080@59.94Hz"),
            Some((1920, 1080, Some(59.94)))
        );
        assert_eq!(parse_mode_text("1024"), None);
        assert_eq!(parse_mode_text("ax1080"), None);
        assert_eq!(parse_mode_text("-100x50"), None);
        assert_eq!(parse_mode_text("100x0"), None);
        assert_eq!(parse_mode_text("100x100@0"), None);
        assert_eq!(parse_mode_text("100x100@abc"), None);
    }

    #[test]
    fn commands_cover_every_change() {
        let mode = Mode {
            width: 1920,
            height: 1080,
            refresh: Some(59.939999),
            preferred: false,
            current: true,
        };
        let cases = [
            (Change::On, vec!["--on"]),
            (Change::Off, vec!["--off"]),
            (Change::Mode(mode), vec!["--mode", "1920x1080@59.939999Hz"]),
            (
                Change::CustomMode {
                    width: 1024,
                    height: 768,
                    refresh: Some(75.0),
                },
                vec!["--custom-mode", "1024x768@75Hz"],
            ),
            (
                Change::CustomMode {
                    width: 800,
                    height: 600,
                    refresh: None,
                },
                vec!["--custom-mode", "800x600"],
            ),
            (Change::Preferred, vec!["--preferred"]),
            (
                Change::Position { x: -1920, y: 1080 },
                vec!["--pos", "-1920,1080"],
            ),
            (
                Change::Transform(Transform::Flipped270),
                vec!["--transform", "flipped-270"],
            ),
            (Change::Scale(1.25), vec!["--scale", "1.25"]),
            (
                Change::AdaptiveSync(true),
                vec!["--adaptive-sync", "enabled"],
            ),
            (
                Change::AdaptiveSync(false),
                vec!["--adaptive-sync", "disabled"],
            ),
        ];
        for (change, tail) in cases {
            assert_eq!(command("DP-1", &change)[1..3], ["--output", "DP-1"]);
            assert_eq!(&command("DP-1", &change)[3..], tail);
        }
    }

    #[test]
    fn scales_format_short_and_parse_back() {
        assert_eq!(format_scale(1.0), "1");
        assert_eq!(format_scale(1.25), "1.25");
        assert_eq!(format_scale(2.0), "2");
        assert_eq!(format_scale(1.333), "1.333");
        for scale in [0.5, 1.0, 1.25, 1.333, 2.0, 3.0] {
            assert_eq!(
                format_scale(scale).parse::<f64>().unwrap(),
                scale,
                "a formatted scale must parse back exactly"
            );
        }
    }

    #[test]
    fn logical_size_applies_transform_and_scale() {
        let mut output = parse(SAMPLE).unwrap()[0].clone();
        output.transform = Transform::R90;
        output.scale = 2.0;
        let (w, h) = output.logical();
        assert_eq!((w, h), (800.0, 1280.0), "2560x1600 rotated 90° at 2×");
    }

    #[test]
    fn a_custom_mode_has_no_listed_current_mode() {
        let outputs = parse(SAMPLE).unwrap();
        assert!(!outputs[1].is_custom());
        let mut output = outputs[1].clone();
        for mode in &mut output.modes {
            mode.current = false;
        }
        assert!(
            output.is_custom(),
            "enabled with a position but no current mode"
        );
    }

    #[test]
    fn snapping_pulls_edges_together_and_aligns_tops() {
        let others = [(1920, 0, 2560, 1440)];
        // Dropped just below and left of the other display: edges pull together.
        assert_eq!(snap_position(1900, 1450, 1920, 1080, &others), (1920, 1440));
        // Top alignment with slack either side of the threshold.
        assert_eq!(snap_position(2620, 6, 1920, 1080, &others), (2620, 0));
        assert_eq!(snap_position(2620, 40, 1920, 1080, &others), (2620, 40));
        // Left of the other display, touching its left edge.
        assert_eq!(snap_position(10, 0, 1920, 1080, &others), (0, 0));
        // The origin snaps too.
        assert_eq!(snap_position(20, 0, 1920, 1080, &[]), (0, 0));
        // No candidate within reach stays put.
        assert_eq!(
            snap_position(10000, 10000, 1920, 1080, &others),
            (10000, 10000)
        );
    }

    #[test]
    fn transform_labels_round_trip() {
        for t in Transform::ALL {
            assert_eq!(Transform::from_label(t.label()), Some(t));
            assert_eq!(Transform::parse(t.arg()), Some(t));
        }
    }
}
