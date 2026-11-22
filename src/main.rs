//! Super Simple RandR, a wlr-randr wrapper built on Rune.
//!
//! Layout: a header, the display map (each enabled output drawn where the
//! compositor puts it — press to select, drag to move, edges snap), and under
//! it the settings for the selected display: everything `wlr-randr` manages
//! (enable, mode, custom mode, orientation, scale, adaptive sync, position).
//! Changes apply immediately through one `wlr-randr` run each and are never
//! saved: on NixOS the declarative configuration owns what restarts restore.
//! All `wlr-randr` work runs on background threads and lands through an event
//! pipe that `poll` drains on the UI thread.
mod desktop;
mod randr;

use randr::{Change, Output, Transform, parse_mode_text};
use rune::edit::TextEdit;
use rune::tok::{self, Theme, type_};
use rune::{
    AppSpec, Cx, Drag, El, ImageCache, KeyEvent, Phase, ScrollSpec, SurfaceFit, TextStyle, Tx,
    Waker, col, el, oklch, row, text,
};
use rune_kit::{
    Button, ButtonVariant, ControlSize, Dialog, Divider, Kit, PaletteSeed, ScrollBar, Select,
    Switch, TextInput, root,
};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

/// The app's accent: teal #008080, in OKLCH.
fn teal() -> rune::Color {
    oklch(0.543, 0.093, 194.77)
}
/// The app id the desktop knows (Wayland app_id), matching the .desktop entry.
const APP_ID: &str = "io.github.MiPSR.SuperSimpleRandR";
const HEADER_HEIGHT: f32 = 52.0;
const MAP_HEIGHT: f32 = 176.0;
/// The smallest a display's card draws, whatever the map's scale.
const MIN_CARD_W: f32 = 52.0;
const MIN_CARD_H: f32 = 40.0;
/// Padding inside the map canvas, in points.
const MAP_PAD: f32 = 14.0;
/// Seconds between silent re-reads of the compositor's state.
const WATCH_INTERVAL: f64 = 3.0;
/// How long to wait after a change before reading the compositor's state back,
/// so the query sees what was just applied.
const SETTLE: Duration = Duration::from_millis(250);
/// The mode list's custom-mode entry.
const CUSTOM: &str = "Custom…";
/// Common scale factors; the current one joins when it is not among them.
const SCALE_STEPS: [f64; 11] = [0.5, 0.75, 1.0, 1.25, 1.5, 1.75, 2.0, 2.25, 2.5, 2.75, 3.0];

/// Which select is open. One at a time, app-wide.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Field {
    Display,
    Mode,
    Transform,
    Scale,
}

enum Panel {
    /// Turning off the only enabled display.
    ConfirmOff(String),
    Error(String),
}

/// A display drag in progress on the map.
struct DragState {
    output: String,
    start: (i32, i32),
    current: (i32, i32),
    moved: bool,
}

/// Work results from background threads, delivered to `poll` through a pipe.
enum UiEvent {
    Loaded {
        attempt: u64,
        result: Result<Vec<Output>, String>,
        silent: bool,
    },
    Applied {
        attempt: u64,
        output: String,
        change: Change,
        result: Result<(), String>,
        fresh: Result<Vec<Output>, String>,
    },
    Appearance(bool),
    Theme(bool),
}

/// A cloneable handle for background threads: an event pipe plus the app's waker.
#[derive(Clone)]
struct Pipe {
    tx: mpsc::Sender<UiEvent>,
    wake: Arc<Mutex<Option<Waker>>>,
}

impl Pipe {
    fn send(&self, event: UiEvent) {
        let woke = self.tx.send(event).is_ok();
        if woke && let Some(wake) = self.wake.lock().unwrap_or_else(|e| e.into_inner()).clone() {
            wake();
        }
    }

    fn set_waker(&self, waker: Waker) {
        *self.wake.lock().unwrap_or_else(|e| e.into_inner()) = Some(waker);
    }
}

struct State {
    icon_cache: Option<Arc<ImageCache>>,
    /// The compositor's outputs; `None` while the first query runs.
    outputs: Option<Vec<Output>>,
    selected: Option<String>,
    load_error: Option<String>,
    status: String,
    busy: Option<(String, Change)>,
    open: Option<Field>,
    dialog: Option<Panel>,
    dark: bool,
    custom_open: bool,
    custom: TextEdit,
    scale_input: TextEdit,
    pos_x: TextEdit,
    pos_y: TextEdit,
    drag: Option<DragState>,
    /// The map canvas's laid-out size, reported by `El::on_layout`.
    map_size: [f32; 2],
    /// Event pipe for background threads, and the receiving end `poll` drains.
    events: Pipe,
    event_rx: Arc<Mutex<mpsc::Receiver<UiEvent>>>,
    /// Fence for load/apply results: an event from an older generation is dropped.
    attempt: u64,
    reload_pending: bool,
    watch_due: Option<f64>,
    /// A snapshot scene installed fixture data; background loads are ignored.
    demo: bool,
}

impl Default for State {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            icon_cache: None,
            outputs: None,
            selected: None,
            load_error: None,
            status: String::new(),
            busy: None,
            open: None,
            dialog: None,
            dark: true,
            custom_open: false,
            custom: TextEdit::new(""),
            scale_input: TextEdit::new("1"),
            pos_x: TextEdit::new(""),
            pos_y: TextEdit::new(""),
            drag: None,
            map_size: [600.0, MAP_HEIGHT],
            events: Pipe {
                tx,
                wake: Arc::new(Mutex::new(None)),
            },
            event_rx: Arc::new(Mutex::new(rx)),
            attempt: 0,
            reload_pending: false,
            watch_due: None,
            demo: false,
        }
    }
}

impl State {
    fn available(&self) -> bool {
        self.busy.is_none() && self.dialog.is_none()
    }

    fn output(&self, name: &str) -> Option<&Output> {
        self.outputs.as_ref()?.iter().find(|o| o.name == name)
    }

    fn selected_output(&self) -> Option<&Output> {
        self.output(self.selected.as_deref()?)
    }

    /// How many outputs are on; for guarding the last one off.
    fn enabled_count(&self) -> usize {
        self.outputs
            .as_ref()
            .map(|o| o.iter().filter(|o| o.enabled).count())
            .unwrap_or(0)
    }
}

/// The palette the kit derives its semantic tokens from: Rune's default
/// background and text for the appearance, with the app's teal accent.
fn kit_of(s: &State) -> Kit {
    let t = theme_of(s);
    let accent = teal();
    let mut k = Kit::from_palette(PaletteSeed {
        bg: t.bg,
        fg: t.text,
        accent,
    });
    // The primary (submit) button carries the accent instead of a contrast fill.
    k.c.bg_icon_button_contrast = accent;
    k.c.submit_fg = rune::Color::WHITE;
    k.c.submit_icon = rune::Color::WHITE;
    k.c.submit_hover = rune::Color::WHITE.alpha(0.12);
    k.c.contrast_pressed = rune::Color::WHITE.alpha(0.20);
    k
}

fn theme_of(s: &State) -> Theme {
    tok::theme(s.dark)
}

fn setup(s: &mut State, waker: Waker) {
    s.icon_cache = Some(Arc::new(ImageCache::new(waker.clone())));
    s.events.set_waker(waker);
    start_load(s, false);
    let events = s.events.clone();
    std::thread::spawn(move || {
        let dark = desktop::dark();
        events.send(UiEvent::Appearance(dark));
    });
    let events = s.events.clone();
    std::thread::spawn(move || {
        desktop::watch_dark(move |dark| events.send(UiEvent::Theme(dark)));
    });
}

fn poll(s: &mut State, now: f64, tx: &mut Tx) -> bool {
    let mut changed = false;
    loop {
        let event = s
            .event_rx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .try_recv()
            .ok();
        let Some(event) = event else { break };
        changed |= apply_event(s, event, tx);
    }
    // Periodic silent re-read of the compositor's state, for changes made
    // outside this app.
    match s.watch_due {
        None => {
            let due = now + WATCH_INTERVAL;
            s.watch_due = Some(due);
            tx.wake_at(due);
        }
        Some(due) if now >= due => {
            let next = now + WATCH_INTERVAL;
            s.watch_due = Some(next);
            tx.wake_at(next);
            if s.demo || s.outputs.is_none() {
                // A snapshot scene owns its data; nothing to watch yet.
            } else if s.available() && s.drag.is_none() && s.open.is_none() {
                changed |= start_load(s, true);
            } else {
                s.reload_pending = true;
            }
        }
        _ => {}
    }
    if s.reload_pending
        && s.available()
        && s.drag.is_none()
        && s.open.is_none()
        && s.outputs.is_some()
        && !s.demo
    {
        s.reload_pending = false;
        changed |= start_load(s, true);
    }
    changed
}

/// Every key the focused control passes on, plus every key while nothing has
/// focus. Counts as user interaction; never takes a key.
fn on_key(s: &mut State, _event: &KeyEvent, _tx: &mut Tx) -> bool {
    interact(s);
    false
}

fn apply_event(s: &mut State, event: UiEvent, tx: &mut Tx) -> bool {
    match event {
        UiEvent::Loaded {
            attempt,
            result,
            silent,
        } if attempt == s.attempt => {
            if s.demo {
                // A snapshot scene owns its fixture data; ignore real loads.
                return false;
            }
            apply_loaded(s, result, silent)
        }
        UiEvent::Loaded { .. } => false,
        UiEvent::Applied {
            attempt,
            output,
            change,
            result,
            fresh,
        } if attempt == s.attempt => {
            s.busy = None;
            let applied = match result {
                Ok(()) => {
                    s.status = format!("{output}: {}", change.describe());
                    true
                }
                Err(error) => {
                    s.status = "Failed".into();
                    s.dialog = Some(Panel::Error(error));
                    true
                }
            };
            match fresh {
                Ok(outputs) => {
                    install(s, outputs, true);
                    s.load_error = None;
                }
                Err(error) => {
                    s.status = "Change finished, but refreshing display state failed".into();
                    s.load_error = Some(error);
                }
            }
            let _ = tx;
            applied
        }
        UiEvent::Applied { .. } => false,
        UiEvent::Appearance(dark) => {
            if s.demo {
                return false;
            }
            if s.dark != dark {
                s.dark = dark;
                return true;
            }
            false
        }
        UiEvent::Theme(dark) => {
            if s.demo {
                return false;
            }
            if s.dark != dark {
                s.dark = dark;
                return true;
            }
            false
        }
    }
}

fn apply_loaded(s: &mut State, result: Result<Vec<Output>, String>, silent: bool) -> bool {
    match result {
        Ok(outputs) => {
            // Pending position typing survives a silent reload.
            install(s, outputs, !silent);
            s.load_error = None;
            if !silent {
                s.status.clear();
            }
            true
        }
        Err(error) => {
            if silent {
                s.status = "Reload failed".into();
            } else {
                s.status = "Load failed".into();
                s.load_error = Some(error);
            }
            true
        }
    }
}

/// Installs freshly read outputs, keeping the selection when it still exists.
/// `refresh_inputs` refills the position and custom-mode fields from the data.
fn install(s: &mut State, outputs: Vec<Output>, refresh_inputs: bool) {
    let previous_selection = s.selected.clone();
    let old_position = s.selected_output().and_then(|o| o.position);
    let untouched_position = old_position
        .is_some_and(|(x, y)| s.pos_x.text == x.to_string() && s.pos_y.text == y.to_string());
    if let Some(selected) = &s.selected
        && outputs.iter().any(|o| &o.name == selected)
    {
        // The selection survives.
    } else {
        s.selected = outputs
            .iter()
            .find(|o| o.enabled)
            .or_else(|| outputs.first())
            .map(|o| o.name.clone());
    }
    s.outputs = Some(outputs);
    if refresh_inputs || previous_selection != s.selected {
        rebuild_inputs(s);
    } else if untouched_position {
        if let Some((x, y)) = s.selected_output().and_then(|o| o.position) {
            s.pos_x = TextEdit::new(x.to_string());
            s.pos_y = TextEdit::new(y.to_string());
        }
    }
}

/// The settings fields track the selected display's actual state.
fn rebuild_inputs(s: &mut State) {
    let Some(output) = s.selected_output().cloned() else {
        return;
    };
    let (x, y) = output.position.unwrap_or((0, 0));
    s.pos_x = TextEdit::new(x.to_string());
    s.pos_y = TextEdit::new(y.to_string());
    s.scale_input = TextEdit::new(randr::format_scale(output.scale));
    s.custom_open = output.is_custom();
}

fn start_load(s: &mut State, silent: bool) -> bool {
    s.attempt += 1;
    let attempt = s.attempt;
    let pipe = s.events.clone();
    if !silent {
        s.status.clear();
    }
    std::thread::spawn(move || {
        let result = randr::query();
        pipe.send(UiEvent::Loaded {
            attempt,
            result,
            silent,
        });
    });
    true
}

/// Runs one `wlr-randr` change for the selected output, then reads the
/// compositor's state back once it has had a moment to settle.
fn start_apply(s: &mut State, change: &Change) -> bool {
    let Some(output) = s.selected_output() else {
        return false;
    };
    if !s.available() {
        return false;
    }
    let name = output.name.clone();
    s.attempt += 1;
    let attempt = s.attempt;
    s.busy = Some((name.clone(), change.clone()));
    s.status = format!("Applying to {name}…");
    if s.demo {
        return true;
    }
    let pipe = s.events.clone();
    let argv = randr::command(&name, change);
    let change = change.clone();
    std::thread::spawn(move || {
        let result = randr::exec(&argv[1..]).map(|_| ());
        std::thread::sleep(SETTLE);
        let fresh = randr::query();
        pipe.send(UiEvent::Applied {
            attempt,
            output: name,
            change,
            result,
            fresh,
        });
    });
    true
}

/// The next user interaction clears the status line.
fn interact(s: &mut State) {
    s.status.clear();
}

fn select_output(s: &mut State, name: &str) {
    if s.selected.as_deref() == Some(name) {
        return;
    }
    s.selected = Some(name.to_string());
    s.open = None;
    s.custom = TextEdit::new("");
    rebuild_inputs(s);
}

fn retry_load(s: &mut State) {
    interact(s);
    s.load_error = None;
    s.outputs = None;
    start_load(s, false);
}

fn toggle_field(s: &mut State, field: Field) {
    interact(s);
    s.open = (s.open != Some(field)).then_some(field);
}

fn close_field(s: &mut State, field: Field) {
    if s.open == Some(field) {
        s.open = None;
    }
}

/// The Enabled switch: off goes through a confirmation when it is the only
/// display still on.
fn toggle_enabled(s: &mut State) {
    interact(s);
    let Some(output) = s.selected_output().cloned() else {
        return;
    };
    if !s.available() {
        return;
    }
    if output.enabled {
        if s.enabled_count() <= 1 {
            s.dialog = Some(Panel::ConfirmOff(output.name.clone()));
            return;
        }
        start_apply(s, &Change::Off);
    } else {
        start_apply(s, &Change::On);
    }
}

fn choose_mode(s: &mut State, label: &str) {
    s.open = None;
    interact(s);
    let Some(output) = s.selected_output().cloned() else {
        return;
    };
    if label == CUSTOM {
        s.custom_open = true;
        return;
    }
    let Some(mode) = output.modes.iter().copied().find(|m| m.label() == label) else {
        // The list changed under a stale menu; the next reload re-renders it.
        return;
    };
    start_apply(s, &Change::Mode(mode));
}

fn choose_transform(s: &mut State, label: &str) {
    s.open = None;
    interact(s);
    let Some(transform) = Transform::from_label(label) else {
        return;
    };
    start_apply(s, &Change::Transform(transform));
}

fn choose_scale(s: &mut State, label: &str) {
    s.open = None;
    interact(s);
    let Ok(scale) = label.parse::<f64>() else {
        return;
    };
    if !scale.is_finite() || scale <= 0.0 {
        return;
    }
    start_apply(s, &Change::Scale(scale));
}

fn choose_display(s: &mut State, label: &str) {
    s.open = None;
    interact(s);
    let Some(outputs) = s.outputs.clone() else {
        return;
    };
    let Some(output) = outputs.iter().find(|o| o.label() == label) else {
        return;
    };
    select_output(s, &output.name);
}

fn preferred_pressed(s: &mut State) {
    interact(s);
    start_apply(s, &Change::Preferred);
}

fn custom_pressed(s: &mut State) {
    interact(s);
    let Some((width, height, refresh)) = parse_mode_text(&s.custom.text) else {
        s.status = "Invalid custom mode — try 1920x1080 or 1920x1080@60".into();
        return;
    };
    start_apply(
        s,
        &Change::CustomMode {
            width,
            height,
            refresh,
        },
    );
}

fn position_pressed(s: &mut State) {
    interact(s);
    let Some(output) = s.selected_output().cloned() else {
        return;
    };
    let (Ok(x), Ok(y)) = (
        s.pos_x.text.trim().parse::<i32>(),
        s.pos_y.text.trim().parse::<i32>(),
    ) else {
        s.status = "Invalid position".into();
        return;
    };
    if !output.enabled {
        s.status = "Turn the display on first".into();
        return;
    }
    start_apply(s, &Change::Position { x, y });
}

fn adaptive_sync_toggled(s: &mut State) {
    interact(s);
    let Some(output) = s.selected_output().cloned() else {
        return;
    };
    let Some(current) = output.adaptive_sync else {
        return;
    };
    start_apply(s, &Change::AdaptiveSync(!current));
}

fn confirm_off(s: &mut State) {
    interact(s);
    if let Some(Panel::ConfirmOff(name)) = s.dialog.take() {
        if s.output(&name).is_some_and(|o| o.enabled) {
            select_output(s, &name);
            start_apply(s, &Change::Off);
        }
    }
}

/// Where the compositor's logical coordinates land in the map canvas.
struct MapLayout {
    scale: f64,
    /// Logical coordinates to canvas points: `x * scale + offset_x`.
    offset_x: f64,
    offset_y: f64,
}

impl MapLayout {
    fn point(&self, x: i32, y: i32) -> (f32, f32) {
        (
            (x as f64 * self.scale + self.offset_x) as f32,
            (y as f64 * self.scale + self.offset_y) as f32,
        )
    }
}

/// Scales the enabled outputs' logical rectangles into `map` points, centered.
fn map_layout(outputs: &[Output], map: [f32; 2]) -> MapLayout {
    let rects: Vec<_> = outputs
        .iter()
        .filter(|o| o.enabled && o.position.is_some())
        .map(|o| {
            let (x, y) = o.position.unwrap_or((0, 0));
            let (w, h) = o.logical();
            (x as f64, y as f64, w, h)
        })
        .collect();
    let Some(first) = rects.first().copied() else {
        return MapLayout {
            scale: 0.0,
            offset_x: 0.0,
            offset_y: 0.0,
        };
    };
    let mut min_x = first.0;
    let mut min_y = first.1;
    let mut max_x = first.0 + first.2;
    let mut max_y = first.1 + first.3;
    for &(x, y, w, h) in &rects {
        min_x = min_x.min(x);
        min_y = min_y.min(y);
        max_x = max_x.max(x + w);
        max_y = max_y.max(y + h);
    }
    let bounds_w = (max_x - min_x).max(1.0);
    let bounds_h = (max_y - min_y).max(1.0);
    let inner_w = (map[0] - 2.0 * MAP_PAD).max(1.0) as f64;
    let inner_h = (map[1] - 2.0 * MAP_PAD).max(1.0) as f64;
    let scale = (inner_w / bounds_w).min(inner_h / bounds_h);
    MapLayout {
        scale,
        offset_x: (map[0] as f64 - bounds_w * scale) / 2.0 - min_x * scale,
        offset_y: (map[1] as f64 - bounds_h * scale) / 2.0 - min_y * scale,
    }
}

/// One display drag on the map: Down selects and anchors, Move snaps, Up
/// applies the settled position.
fn drag_monitor(s: &mut State, name: &str, d: &Drag, _tx: &mut Tx) {
    if !s.available() {
        s.drag = None;
        return;
    }
    match d.phase {
        Phase::Down => {
            select_output(s, name);
            let position = s.output(name).and_then(|o| o.position);
            if let Some(start) = position {
                s.drag = Some(DragState {
                    output: name.to_string(),
                    start,
                    current: start,
                    moved: false,
                });
            }
        }
        Phase::Move => {
            let Some(outputs) = s.outputs.clone() else {
                return;
            };
            let layout = map_layout(&outputs, s.map_size);
            if layout.scale <= 0.0 {
                return;
            }
            let (w, h) = s
                .output(name)
                .map(|o| o.logical())
                .unwrap_or((1280.0, 720.0));
            let Some(drag) = &mut s.drag else { return };
            if drag.output != name {
                return;
            }
            if d.dx.abs() + d.dy.abs() > 2.0 {
                drag.moved = true;
            }
            let x = drag.start.0 as f64 + d.dx as f64 / layout.scale;
            let y = drag.start.1 as f64 + d.dy as f64 / layout.scale;
            let (x, y) = (x.round() as i32, y.round() as i32);
            let others: Vec<_> = outputs
                .iter()
                .filter(|o| o.enabled && o.position.is_some() && o.name != name)
                .map(|o| {
                    let (ox, oy) = o.position.unwrap_or((0, 0));
                    let (ow, oh) = o.logical();
                    (ox, oy, ow as i32, oh as i32)
                })
                .collect();
            drag.current = randr::snap_position(x, y, w as i32, h as i32, &others);
        }
        Phase::Up => {
            let Some(drag) = s.drag.take() else { return };
            if drag.output == name && !d.cancelled() && drag.moved && drag.current != drag.start {
                s.selected = Some(name.to_string());
                start_apply(
                    s,
                    &Change::Position {
                        x: drag.current.0,
                        y: drag.current.1,
                    },
                );
            }
        }
    }
}

fn view(s: &State, _cx: &mut Cx<State>) -> El {
    let k = kit_of(s);
    let t = theme_of(s);
    let mut layer = Vec::new();
    let content = col()
        .key("app")
        .w_full()
        .h_full()
        .fill(t.bg)
        .child(header(s, &k))
        .child(Divider::horizontal().build(&k))
        .child(map_section(s, &k, &t))
        .child(Divider::horizontal().build(&k))
        .child(settings_area(s, &k, &t, &mut layer));
    dialog_layer(s, &k, &mut layer);
    root(content, layer).on_press(|s: &mut State| interact(s))
}

fn logo(s: &State) -> El {
    let Some(cache) = &s.icon_cache else {
        return el().size(30.0, 30.0);
    };
    let image = cache.svg(
        include_bytes!("../assets/super-simple-randr.svg").as_slice(),
        30.0,
        2.0,
    );
    el().key("logo")
        .size(30.0, 30.0)
        .pixels_fit(image.pixels(), SurfaceFit::Fit)
}

fn header(s: &State, k: &Kit) -> El {
    let t = theme_of(s);
    let title = row().gap(8.0).center_y().child(logo(s)).child(
        text("Super Simple RandR")
            .font(TextStyle::new(17.0, 600.0))
            .color(t.text),
    );
    row()
        .key("header")
        .h(HEADER_HEIGHT)
        .w_full()
        .pl(16.0)
        .pr(16.0)
        .gap(8.0)
        .center_y()
        .child(title)
        .child(el().grow(1.0))
        .child(
            text(&s.status)
                .font(k.ty.small)
                .color(k.c.text_muted)
                .truncate()
                .shrink()
                .min_w(0.0),
        )
        .child(
            Button::new("Refresh")
                .control_size(ControlSize::Small)
                .disabled(!s.available())
                .on_press(|s: &mut State| {
                    interact(s);
                    start_load(s, false);
                })
                .build(k)
                .key("refresh"),
        )
}

/// The display map and, under it, a chip for every display that is off.
fn map_section(s: &State, k: &Kit, t: &Theme) -> El {
    let hint = match s.drag {
        Some(_) => "Release to apply",
        None => "Press to select · drag to move",
    };
    let mut section = col()
        .key("map-section")
        .w_full()
        .pl(20.0)
        .pr(20.0)
        .pt(10.0)
        .pb(10.0)
        .gap(6.0)
        .child(
            row()
                .gap(8.0)
                .center_y()
                .child(
                    text("Displays")
                        .font(type_::SECTION)
                        .color(t.text_secondary),
                )
                .child(el().grow(1.0))
                .child(
                    text(hint)
                        .key("map-hint")
                        .font(k.ty.small)
                        .color(k.c.text_faint),
                ),
        )
        .child(map_canvas(s, k, t));
    if let Some(outputs) = &s.outputs
        && outputs.iter().any(|o| !o.enabled)
    {
        section = section.child(disabled_chips(s, k));
    }
    section
}

fn map_canvas(s: &State, k: &Kit, t: &Theme) -> El {
    let mut canvas = col()
        .key("map")
        .w_full()
        .h(MAP_HEIGHT)
        .radius(k.radius.md)
        .fill(k.c.layer_01)
        .on_layout(|s: &mut State, [_, _, w, h]| {
            let size = [w, h];
            if s.map_size != size {
                s.map_size = size;
            }
        });
    let Some(outputs) = &s.outputs else {
        return canvas.center().child(
            text(if s.load_error.is_some() {
                "Display configuration unavailable"
            } else {
                "Reading the compositor's display configuration…"
            })
            .font(k.ty.ui)
            .color(t.text_secondary),
        );
    };
    if outputs.is_empty() {
        return canvas.center().child(
            text("No displays found")
                .font(k.ty.ui)
                .color(t.text_secondary),
        );
    }
    let layout = map_layout(outputs, s.map_size);
    if layout.scale <= 0.0 {
        return canvas.center().child(
            text("Every display is off")
                .font(k.ty.ui)
                .color(t.text_secondary),
        );
    }
    for output in outputs.iter().filter(|o| o.enabled) {
        canvas = canvas.child(monitor_card(s, k, t, output, &layout));
    }
    canvas
}

/// One enabled display on the map: a card where the compositor puts it,
/// labelled with its name and mode, showing its orientation when it has one.
fn monitor_card(s: &State, k: &Kit, t: &Theme, output: &Output, layout: &MapLayout) -> El {
    let name = output.name.clone();
    let selected = s.selected.as_deref() == Some(&name);
    let dragging = s.drag.as_ref().is_some_and(|d| d.output == name);
    let (x, y) = match &s.drag {
        Some(d) if d.output == name => d.current,
        _ => output.position.unwrap_or((0, 0)),
    };
    let (lw, lh) = output.logical();
    let mut w = ((lw * layout.scale) as f32).max(MIN_CARD_W);
    let mut h = ((lh * layout.scale) as f32).max(MIN_CARD_H);
    if dragging {
        w += 4.0;
        h += 4.0;
    }
    // Enlarged cards keep their centre where the display really is.
    let (cx, cy) = layout.point(x + (lw / 2.0).round() as i32, y + (lh / 2.0).round() as i32);
    let left = cx - w / 2.0;
    let top = cy - h / 2.0;
    let accent = teal();
    let border = if selected || dragging {
        (if dragging { 2.5 } else { 2.0 }, accent)
    } else {
        (1.25, k.c.border_muted)
    };
    let fill = if selected {
        k.alpha(k.c.bg_accent, 0.16)
    } else {
        k.c.layer_02
    };
    let mut label = col().key("card").gap(1.0).center();
    if output.transform != Transform::Normal {
        label = label.child(
            text(output.transform.arg())
                .key("transform")
                .font(k.ty.small)
                .color(k.c.text_accent)
                .truncate()
                .max_w((w - 10.0).max(12.0)),
        );
    }
    label = label
        .child(
            text(&name)
                .font(k.ty.ui_medium)
                .color(t.text)
                .truncate()
                .max_w((w - 10.0).max(12.0)),
        )
        .child(
            text(
                output
                    .current_mode()
                    .map(|m| format!("{} × {}", m.width, m.height))
                    .unwrap_or_else(|| "custom mode".into()),
            )
            .key("caption")
            .font(k.ty.small)
            .color(k.c.text_muted)
            .truncate()
            .max_w((w - 10.0).max(12.0)),
        );
    let mut card = el()
        .key(format!("output-{name}"))
        .role("button")
        .label(format!("Select the {} display", output.label()))
        .focusable(true)
        .absolute(left, top)
        .size(w, h)
        .radius(k.radius.sm)
        .fill(fill)
        .border(border.0, border.1)
        .center()
        .on_press({
            let name = name.clone();
            move |s: &mut State| {
                interact(s);
                select_output(s, &name);
            }
        })
        .on_drag({
            let name = name.clone();
            move |s: &mut State, d: &mut Drag, tx: &mut Tx| drag_monitor(s, &name, d, tx)
        })
        .touch_drags_at_once()
        .child(label);
    if selected || dragging {
        card = k.interactive(card);
    }
    card
}

/// Chips for the displays that are off; pressing one selects it for editing.
fn disabled_chips(s: &State, k: &Kit) -> El {
    let Some(outputs) = &s.outputs else {
        return el();
    };
    let chips: Vec<El> = outputs
        .iter()
        .filter(|o| !o.enabled)
        .map(|o| {
            let name = o.name.clone();
            let caption = format!("{name} — off");
            k.interactive(
                el().key(format!("chip-{name}"))
                    .role("button")
                    .label(format!("Select the off display {name}"))
                    .focusable(true)
                    .h(28.0)
                    .pl(10.0)
                    .pr(10.0)
                    .center_y()
                    .radius(k.radius.sm)
                    .border(1.0, k.c.border_muted)
                    .on_press(move |s: &mut State| {
                        interact(s);
                        select_output(s, &name);
                    })
                    .child(text(caption).font(k.ty.ui_compact).color(k.c.text_muted)),
            )
        })
        .collect();
    row()
        .key("disabled-chips")
        .gap(6.0)
        .center_y()
        .child(text("Off:").font(k.ty.small).color(k.c.text_faint))
        .children(chips)
}

fn settings_area(s: &State, k: &Kit, t: &Theme, layer: &mut Vec<El>) -> El {
    let mut area = col()
        .key("settings")
        .grow(1.0)
        .min_h(0.0)
        .scroll(ScrollSpec {
            stick_bottom: false,
        });
    let mut body = col()
        .key("settings-body")
        .w_full()
        .pt(10.0)
        .pb(12.0)
        .gap(2.0);
    match (&s.outputs, &s.load_error) {
        (None, None) => {
            body = body.child(
                col().key("loading").w_full().py(24.0).center().child(
                    text("Reading the compositor's display configuration…")
                        .font(k.ty.ui)
                        .color(t.text_secondary),
                ),
            );
        }
        (None, Some(error)) => {
            body = body.child(
                col()
                    .key("load-error")
                    .w_full()
                    .py(16.0)
                    .gap(10.0)
                    .center()
                    .child(
                        text("wlr-randr could not be read")
                            .font(k.ty.ui_medium)
                            .color(k.c.danger.fg),
                    )
                    .child(text(error).wrap().font(k.ty.mono).color(k.c.text_muted))
                    .child(
                        Button::new("Retry")
                            .control_size(ControlSize::Small)
                            .on_press(|s: &mut State| retry_load(s))
                            .build(k)
                            .key("retry"),
                    ),
            );
        }
        (Some(outputs), _) => {
            if outputs.is_empty() {
                body = body.child(
                    col().key("no-outputs").w_full().py(24.0).center().child(
                        text("No displays found")
                            .font(k.ty.ui)
                            .color(t.text_secondary),
                    ),
                );
            } else {
                body = body.children(settings_rows(s, k, t, layer));
            }
        }
    }
    body = body.child(footnote(s, k));
    area = area.child(body).scroll_bar(k);
    // The selects mount their listboxes as overlays over everything.
    col().w_full().grow(1.0).min_h(0.0).px(20.0).child(area)
}

/// The selected display's settings, one row per property `wlr-randr` manages.
fn settings_rows(s: &State, k: &Kit, t: &Theme, layer: &mut Vec<El>) -> Vec<El> {
    let Some(output) = s.selected_output().cloned() else {
        return vec![];
    };
    let lock = !s.available();
    let mut rows = Vec::new();
    // The display being edited, with its description and physical size.
    let subtitle = match output.physical {
        Some((w, h)) => format!("{} × {} mm", w, h),
        None => output.description.clone(),
    };
    let subtitle = if !output.description.is_empty() && output.physical.is_some() {
        format!("{} · {}", output.description, subtitle)
    } else {
        subtitle
    };
    rows.push(
        col()
            .key("display-title")
            .w_full()
            .pb(6.0)
            .gap(1.0)
            .child(
                row()
                    .gap(8.0)
                    .center_y()
                    .child(
                        text(output.name.clone())
                            .font(TextStyle::new(15.0, 600.0))
                            .color(t.text),
                    )
                    .child(
                        text(if output.enabled { "on" } else { "off" })
                            .key("state")
                            .font(k.ty.small)
                            .color(if output.enabled {
                                k.c.text_accent
                            } else {
                                k.c.text_faint
                            }),
                    ),
            )
            .child(
                text(subtitle)
                    .font(k.ty.small)
                    .color(k.c.text_muted)
                    .truncate()
                    .min_w(0.0)
                    .max_w(520.0),
            ),
    );

    // Which display the panel edits.
    if let Some(outputs) = &s.outputs {
        let options: Vec<String> = outputs.iter().map(|o| o.label()).collect();
        let current = output.label();
        rows.push(setting_row(
            "display",
            "Display",
            Select::new("display-select", &options)
                .current(Some(&current))
                .width(300.0)
                .label("Display")
                .open(s.open == Some(Field::Display))
                .on_toggle(|s: &mut State| toggle_field(s, Field::Display))
                .on_close(|s: &mut State| close_field(s, Field::Display))
                .on_choose(|s: &mut State, label: &str, _tx: &mut Tx| choose_display(s, label))
                .disabled(lock)
                .build(k, layer),
            k,
            t,
        ));
    }

    // Enable.
    rows.push(setting_row(
        "enabled",
        "Enabled",
        Switch::new(output.enabled)
            .label("Enabled")
            .hide_label()
            .on_toggle_tx(|s: &mut State, _tx: &mut Tx| toggle_enabled(s))
            .disabled(lock)
            .build(k),
        k,
        t,
    ));

    // Mode: the listed modes plus Custom…, and the preferred-mode shortcut.
    let mode_options: Vec<String> = output
        .modes
        .iter()
        .map(|m| m.label())
        .chain([CUSTOM.to_string()])
        .collect();
    let current = match output.current_mode() {
        Some(mode) => mode.label(),
        None if output.enabled && output.position.is_some() => CUSTOM.into(),
        None => String::new(),
    };
    let mut mode_row = row()
        .key("row-mode")
        .w_full()
        .py(6.0)
        .center_y()
        .gap(8.0)
        .child(text("Mode").font(k.ty.ui).color(t.text))
        .child(el().grow(1.0));
    if output.modes.iter().any(|m| m.preferred) {
        mode_row = mode_row.child(
            Button::new("Preferred")
                .control_size(ControlSize::Small)
                .disabled(lock || !output.enabled)
                .on_press(|s: &mut State| preferred_pressed(s))
                .build(k)
                .key("preferred"),
        );
    }
    mode_row = mode_row.child(
        Select::new("mode-select", &mode_options)
            .current((!current.is_empty()).then_some(current.as_str()))
            .width(220.0)
            .label("Mode")
            .open(s.open == Some(Field::Mode))
            .on_toggle(|s: &mut State| toggle_field(s, Field::Mode))
            .on_close(|s: &mut State| close_field(s, Field::Mode))
            .on_choose(|s: &mut State, label: &str, _tx: &mut Tx| choose_mode(s, label))
            .disabled(lock || !output.enabled)
            .build(k, layer),
    );
    rows.push(mode_row);

    // A custom mode, once asked for (or when one is already running).
    if s.custom_open {
        rows.push(
            row()
                .key("row-custom")
                .w_full()
                .py(6.0)
                .center_y()
                .gap(8.0)
                .child(text("Custom mode").font(k.ty.ui).color(t.text))
                .child(
                    text("widthxheight[@rate]")
                        .font(k.ty.small)
                        .color(k.c.text_faint),
                )
                .child(el().grow(1.0))
                .child(
                    TextInput::new(&s.custom)
                        .placeholder("1920x1080@60")
                        .width(Some(160.0))
                        .on_edit(|s: &mut State| &mut s.custom)
                        .on_change(|s: &mut State, _tx: &mut Tx| interact(s))
                        .label("Custom mode")
                        .build(k),
                )
                .child(
                    Button::new("Set")
                        .control_size(ControlSize::Small)
                        .disabled(lock || !output.enabled)
                        .on_press(|s: &mut State| custom_pressed(s))
                        .build(k)
                        .key("set-custom"),
                ),
        );
    }

    // Orientation.
    let transform_options: Vec<&str> = Transform::ALL.iter().map(|t| t.label()).collect();
    rows.push(setting_row(
        "transform",
        "Orientation",
        Select::new("transform-select", &transform_options)
            .current(Some(output.transform.label()))
            .width(220.0)
            .label("Orientation")
            .open(s.open == Some(Field::Transform))
            .on_toggle(|s: &mut State| toggle_field(s, Field::Transform))
            .on_close(|s: &mut State| close_field(s, Field::Transform))
            .on_choose(|s: &mut State, label: &str, _tx: &mut Tx| choose_transform(s, label))
            .disabled(lock || !output.enabled)
            .build(k, layer),
        k,
        t,
    ));

    // Scale: common steps, plus the current one when it is not among them.
    let mut scale_options: Vec<String> = SCALE_STEPS
        .iter()
        .map(|&v| randr::format_scale(v))
        .collect();
    let current_scale = randr::format_scale(output.scale);
    if output.scale > 0.0 && !scale_options.contains(&current_scale) {
        scale_options.push(current_scale.clone());
        scale_options.sort_by(|a, b| {
            a.parse::<f64>()
                .unwrap_or(f64::MAX)
                .total_cmp(&b.parse::<f64>().unwrap_or(f64::MAX))
        });
    }
    rows.push(setting_row(
        "scale",
        "Scale",
        Select::new("scale-select", &scale_options)
            .current(Some(&current_scale))
            .width(160.0)
            .label("Scale")
            .open(s.open == Some(Field::Scale))
            .on_toggle(|s: &mut State| toggle_field(s, Field::Scale))
            .on_close(|s: &mut State| close_field(s, Field::Scale))
            .on_choose(|s: &mut State, label: &str, _tx: &mut Tx| choose_scale(s, label))
            .disabled(lock || !output.enabled)
            .build(k, layer),
        k,
        t,
    ));
    rows.push(setting_row(
        "custom-scale",
        "Custom scale",
        row()
            .gap(8.0)
            .center_y()
            .child(
                TextInput::new(&s.scale_input)
                    .label("Custom scale factor")
                    .width(Some(100.0))
                    .disabled(lock || !output.enabled)
                    .on_edit(|s: &mut State| &mut s.scale_input)
                    .build(k),
            )
            .child(
                Button::new("Set")
                    .control_size(ControlSize::Small)
                    .disabled(lock || !output.enabled)
                    .on_press(|s: &mut State| {
                        let value = s.scale_input.text.clone();
                        match value.trim().parse::<f64>() {
                            Ok(scale) if scale.is_finite() && scale > 0.0 => {
                                choose_scale(s, value.trim())
                            }
                            _ => s.status = "Scale must be a finite positive number".into(),
                        }
                    })
                    .build(k),
            ),
        k,
        t,
    ));

    // Adaptive sync, when the compositor reports it.
    if let Some(adaptive) = output.adaptive_sync {
        rows.push(setting_row(
            "adaptive-sync",
            "Adaptive sync",
            Switch::new(adaptive)
                .label("Adaptive sync")
                .hide_label()
                .on_toggle_tx(|s: &mut State, _tx: &mut Tx| adaptive_sync_toggled(s))
                .disabled(lock || !output.enabled)
                .build(k),
            k,
            t,
        ));
    }

    // Position: the drag handles this too; the fields make it exact.
    let position_lock = lock || !output.enabled;
    rows.push(
        row()
            .key("row-position")
            .w_full()
            .py(6.0)
            .center_y()
            .gap(8.0)
            .child(text("Position").font(k.ty.ui).color(t.text))
            .child(el().grow(1.0))
            .child(
                TextInput::new(&s.pos_x)
                    .placeholder("X")
                    .width(Some(80.0))
                    .on_edit(|s: &mut State| &mut s.pos_x)
                    .on_change(|s: &mut State, _tx: &mut Tx| interact(s))
                    .label("Position X")
                    .disabled(position_lock)
                    .build(k),
            )
            .child(
                TextInput::new(&s.pos_y)
                    .placeholder("Y")
                    .width(Some(80.0))
                    .on_edit(|s: &mut State| &mut s.pos_y)
                    .on_change(|s: &mut State, _tx: &mut Tx| interact(s))
                    .label("Position Y")
                    .disabled(position_lock)
                    .build(k),
            )
            .child(
                Button::new("Set")
                    .control_size(ControlSize::Small)
                    .disabled(position_lock)
                    .on_press(|s: &mut State| position_pressed(s))
                    .build(k)
                    .key("set-position"),
            ),
    );
    rows
}

/// One label-and-control line of the settings panel.
fn setting_row(key: &str, label: &str, control: El, k: &Kit, t: &Theme) -> El {
    row()
        .key(format!("row-{key}"))
        .w_full()
        .py(6.0)
        .center_y()
        .gap(8.0)
        .child(text(label).font(k.ty.ui).color(t.text))
        .child(el().grow(1.0))
        .child(control)
}

fn footnote(_s: &State, k: &Kit) -> El {
    col()
        .key("footnote")
        .w_full()
        .pt(10.0)
        .pb(4.0)
        .gap(6.0)
        .child(Divider::horizontal().build(k))
        .child(
            text("Session-only changes · Nothing is saved. Configure your compositor declaratively in NixOS for persistent settings.")
                .wrap()
                .font(k.ty.small)
                .color(k.c.text_faint),
        )
}

fn dialog_layer(s: &State, k: &Kit, layer: &mut Vec<El>) {
    let Some(dialog) = &s.dialog else { return };
    match dialog {
        Panel::ConfirmOff(name) => {
            Dialog::new("confirm-off", "Turn off the only display?")
                .description(name.clone())
                .fit()
                .open(true)
                .on_close(|s: &mut State| s.dialog = None)
                .child(
                    col()
                        .px(16.0)
                        .pb(16.0)
                        .gap(12.0)
                        .child(
                            text("Nothing will be shown until a display is turned on again, or the compositor restarts and your configuration takes over.")
                                .wrap()
                                .font(k.ty.small)
                                .color(k.c.text_muted),
                        )
                        .child(
                            row()
                                .justify_end()
                                .gap(8.0)
                                .child(
                                    Button::new("Cancel")
                                        .on_press(|s: &mut State| {
                                            interact(s);
                                            s.dialog = None;
                                        })
                                        .build(k)
                                        .key("confirm-off-cancel"),
                                )
                                .child(
                                    Button::new("Turn off")
                                        .variant(ButtonVariant::Danger)
                                        .on_press(|s: &mut State| confirm_off(s))
                                        .build(k)
                                        .key("confirm-off-yes"),
                                ),
                        ),
                )
                .build(k, layer);
        }
        Panel::Error(error) => {
            let error = error.clone();
            Dialog::new("error", "The change was not applied")
                .fit()
                .open(true)
                .on_close(|s: &mut State| s.dialog = None)
                .child(
                    col()
                        .px(16.0)
                        .pb(16.0)
                        .gap(12.0)
                        .child(
                            col()
                                .key("error-text")
                                .h(160.0)
                                .w_full()
                                .scroll(ScrollSpec {
                                    stick_bottom: false,
                                })
                                .child(text(error).wrap().font(k.ty.mono)),
                        )
                        .child(
                            row().justify_end().child(
                                Button::new("OK")
                                    .on_press(|s: &mut State| {
                                        interact(s);
                                        s.dialog = None;
                                    })
                                    .build(k),
                            ),
                        ),
                )
                .build(k, layer);
        }
    }
}

fn main() {
    rune::set_app_id(APP_ID);
    rune::run(AppSpec::<State> {
        min_size: Some((640.0, 520.0)),
        setup: Some(setup),
        poll: Some(poll),
        on_key: Some(on_key),
        scenes: &[
            ("fixture", |s| install_fixture(s)),
            ("fixture-light", |s| {
                install_fixture(s);
                s.dark = false;
            }),
            ("fixture-error", |s| {
                install_fixture(s);
                s.dialog = Some(Panel::Error(
                    "unknown mode: 5120x1440@120Hz\nfailed to apply configuration".into(),
                ));
            }),
        ],
        ..AppSpec::new("Super Simple RandR", (880.0, 620.0), State::default, view)
    });
}

/// A headless snapshot scene: a laptop display with an external one to its
/// right (rotated, scaled) and a TV that is off, so the map, chips, selects and
/// the footnote all show. No real `wlr-randr` run touches it: `demo` keeps
/// background loads away.
fn install_fixture(s: &mut State) {
    let laptop = Output {
        name: "eDP-1".into(),
        description: "Built-in display".into(),
        physical: Some((309, 174)),
        enabled: true,
        modes: vec![
            randr::Mode {
                width: 2560,
                height: 1600,
                refresh: Some(60.0),
                preferred: true,
                current: true,
            },
            randr::Mode {
                width: 2560,
                height: 1600,
                refresh: Some(59.97),
                preferred: false,
                current: false,
            },
            randr::Mode {
                width: 1920,
                height: 1200,
                refresh: Some(60.0),
                preferred: false,
                current: false,
            },
        ],
        position: Some((0, 0)),
        transform: Transform::Normal,
        scale: 2.0,
        adaptive_sync: Some(false),
    };
    let docked = Output {
        name: "DP-1".into(),
        description: "Dell U2720Q".into(),
        physical: Some((596, 335)),
        enabled: true,
        modes: vec![
            randr::Mode {
                width: 1920,
                height: 1080,
                refresh: Some(143.962997),
                preferred: false,
                current: true,
            },
            randr::Mode {
                width: 1920,
                height: 1080,
                refresh: Some(60.0),
                preferred: true,
                current: false,
            },
        ],
        position: Some((1280, 0)),
        transform: Transform::R90,
        scale: 1.0,
        adaptive_sync: Some(true),
    };
    let tv = Output {
        name: "HDMI-A-1".into(),
        description: "Living room TV".into(),
        physical: None,
        enabled: false,
        modes: vec![randr::Mode {
            width: 3840,
            height: 2160,
            refresh: Some(60.0),
            preferred: true,
            current: false,
        }],
        position: None,
        transform: Transform::Normal,
        scale: 1.0,
        adaptive_sync: None,
    };
    s.demo = true;
    s.dark = true;
    s.outputs = Some(vec![laptop, docked, tv]);
    s.selected = Some("eDP-1".into());
    rebuild_inputs(s);
}

#[cfg(test)]
mod tests {
    use super::*;
    use randr::{Mode, parse};
    use rune::{Env, Runtime};

    /// The wlr-randr sample the parser tests use, as fixture state.
    fn fixture_state() -> State {
        let outputs = parse(SAMPLE).unwrap();
        let mut s = State::default();
        s.demo = true;
        s.outputs = Some(outputs);
        s.selected = Some("DP-1".into());
        rebuild_inputs(&mut s);
        s
    }

    const SAMPLE: &str = "\
eDP-1 \"Built-in display\"
  Physical size: 309x174 mm
  Enabled: yes
  Modes:
    2560x1600 px, 60.000000 Hz (preferred, current)
  Position: 0,0
  Transform: normal
  Scale: 2.000000
  Adaptive Sync: disabled
DP-1 \"Dell D24\"
  Enabled: yes
  Modes:
    1920x1080 px, 143.962997 Hz (current)
    1920x1080 px, 60.000000 Hz (preferred)
  Position: 1280,0
  Transform: 90
  Scale: 1.000000
  Adaptive Sync: enabled
HDMI-A-1 \"Living room TV\"
  Enabled: no
  Modes:
    3840x2160 px, 60.000000 Hz (preferred)
";

    fn runtime(state: State) -> Runtime<State> {
        Runtime::new(state, view, (880.0, 620.0), Env::new(true, 1.0))
    }

    #[test]
    fn install_keeps_the_selection_and_falls_back_to_an_enabled_output() {
        let mut s = fixture_state();
        assert_eq!(s.selected.as_deref(), Some("DP-1"));
        // The selected display disappearing falls back to the first enabled.
        let fresh: Vec<_> = s
            .outputs
            .clone()
            .unwrap()
            .into_iter()
            .filter(|o| o.name != "DP-1")
            .collect();
        install(&mut s, fresh, true);
        assert_eq!(s.selected.as_deref(), Some("eDP-1"));
        assert_eq!(
            s.pos_x.text, "0",
            "the position fields follow the selection"
        );
        // A stale selection by name is kept when the output still exists.
        let fresh = s.outputs.clone().unwrap();
        install(&mut s, fresh, false);
        assert_eq!(s.selected.as_deref(), Some("eDP-1"));
    }

    #[test]
    fn external_positions_refresh_without_overwriting_pending_typing() {
        let mut s = fixture_state();
        let mut fresh = s.outputs.clone().unwrap();
        fresh[1].position = Some((-1080, 50));
        apply_loaded(&mut s, Ok(fresh.clone()), true);
        assert_eq!(s.pos_x.text, "-1080");
        s.pos_x = TextEdit::new("123");
        fresh[1].position = Some((-1080, 70));
        apply_loaded(&mut s, Ok(fresh), true);
        assert_eq!(
            s.pos_x.text, "123",
            "pending input survives a silent refresh"
        );
    }

    #[test]
    fn layout_supports_minimum_and_wide_windows() {
        for width in [640.0, 1280.0, 2400.0] {
            let mut rt = Runtime::new(fixture_state(), view, (width, 760.0), Env::new(true, 1.0));
            rt.frame(0.0);
            rt.frame(0.1);
            assert!(rt.find("map").is_some());
            assert!(rt.find("row-position").is_some());
            assert!((rt.state.map_size[0] - (width - 40.0)).abs() < 1.0);
        }
    }

    #[test]
    fn choosing_a_mode_applies_it_and_custom_only_reveals_the_row() {
        let mut s = fixture_state();
        choose_mode(&mut s, "Custom…");
        assert!(s.custom_open);
        assert!(s.busy.is_none(), "Custom… runs nothing");
        let mode = s
            .selected_output()
            .unwrap()
            .modes
            .iter()
            .copied()
            .find(|m| m.preferred)
            .unwrap();
        choose_mode(&mut s, &mode.label());
        assert_eq!(
            s.busy,
            Some(("DP-1".into(), Change::Mode(mode))),
            "choosing a listed mode starts a wlr-randr run"
        );
        // A label nothing matches runs nothing.
        s.busy = None;
        choose_mode(&mut s, "4096 × 2160 @ 120.000 Hz");
        assert!(s.busy.is_none());
    }

    #[test]
    fn every_choose_handler_builds_the_change_it_should() {
        let mut s = fixture_state();
        choose_transform(&mut s, "Flipped + 90°");
        assert_eq!(
            s.busy,
            Some(("DP-1".into(), Change::Transform(Transform::Flipped90)))
        );
        s.busy = None;
        choose_scale(&mut s, "1.25");
        assert_eq!(s.busy, Some(("DP-1".into(), Change::Scale(1.25))));
        s.busy = None;
        adaptive_sync_toggled(&mut s);
        assert_eq!(s.busy, Some(("DP-1".into(), Change::AdaptiveSync(false))));
        s.busy = None;
        preferred_pressed(&mut s);
        assert_eq!(s.busy, Some(("DP-1".into(), Change::Preferred)));
    }

    #[test]
    fn non_finite_scales_and_refresh_rates_never_apply() {
        let mut s = fixture_state();
        for value in ["NaN", "inf", "-1", "0", "not a scale"] {
            choose_scale(&mut s, value);
            assert!(s.busy.is_none());
        }
        for value in ["1920x1080@NaN", "1920x1080@inf", "1920x1080@-60"] {
            s.custom = TextEdit::new(value);
            custom_pressed(&mut s);
            assert!(s.busy.is_none());
        }
    }

    #[test]
    fn position_and_custom_mode_inputs_validate_before_applying() {
        let mut s = fixture_state();
        s.pos_x = TextEdit::new("1280");
        s.pos_y = TextEdit::new("-256");
        position_pressed(&mut s);
        assert_eq!(
            s.busy,
            Some(("DP-1".into(), Change::Position { x: 1280, y: -256 }))
        );
        s.busy = None;
        s.pos_x = TextEdit::new("left");
        position_pressed(&mut s);
        assert!(s.busy.is_none());
        assert_eq!(s.status, "Invalid position");
        s.custom = TextEdit::new("2048x1152@75");
        custom_pressed(&mut s);
        assert_eq!(
            s.busy,
            Some((
                "DP-1".into(),
                Change::CustomMode {
                    width: 2048,
                    height: 1152,
                    refresh: Some(75.0)
                }
            ))
        );
        s.busy = None;
        s.custom = TextEdit::new("2048x");
        custom_pressed(&mut s);
        assert!(s.busy.is_none());
        assert!(s.status.contains("Invalid custom mode"));
    }

    #[test]
    fn turning_off_the_last_display_needs_confirmation() {
        let mut s = fixture_state();
        // Two displays are on: off applies at once.
        toggle_enabled(&mut s);
        assert_eq!(s.busy, Some(("DP-1".into(), Change::Off)));
        assert!(s.dialog.is_none());
        s.busy = None;
        // With one display left, the switch asks first.
        let mut outputs = s.outputs.clone().unwrap();
        outputs[0].enabled = false;
        outputs[0].position = None;
        install(&mut s, outputs, true);
        toggle_enabled(&mut s);
        assert!(matches!(&s.dialog, Some(Panel::ConfirmOff(name)) if name == "DP-1"));
        assert!(s.busy.is_none());
        // Confirming is the actual wlr-randr run.
        confirm_off(&mut s);
        assert_eq!(s.busy, Some(("DP-1".into(), Change::Off)));
    }

    #[test]
    fn a_drag_selects_moves_snaps_and_applies_on_release() {
        let mut rt = runtime(fixture_state());
        rt.frame(0.0);
        rt.frame(0.1);
        let (x, y) = rt.find("output-eDP-1").unwrap();
        rt.pointer_down(x, y);
        rt.frame(0.01);
        assert_eq!(rt.state.selected.as_deref(), Some("eDP-1"));
        let scale = map_layout(rt.state.outputs.as_ref().unwrap(), rt.state.map_size).scale;
        let dx = (300.0 * scale) as f32;
        rt.pointer_move(x + dx, y);
        rt.frame(0.01);
        assert!(rt.state.drag.as_ref().unwrap().moved);
        rt.pointer_up(x + dx, y);
        rt.frame(0.01);
        assert!(matches!(rt.state.busy, Some((_, Change::Position { .. }))));
    }

    #[test]
    fn the_fixture_scene_lays_out_map_cards_and_settings() {
        let mut rt = runtime(fixture_state());
        rt.frame(0.0);
        assert!(
            rt.find("output-eDP-1").is_some(),
            "the laptop is on the map"
        );
        assert!(
            rt.find("output-DP-1").is_some(),
            "the docked display is on the map"
        );
        assert!(rt.find("output-HDMI-A-1").is_none(), "the off TV is not");
        assert!(rt.find("chip-HDMI-A-1").is_some(), "the off TV is a chip");
        assert!(rt.find("row-enabled").is_some());
        assert!(rt.find("row-mode").is_some());
        assert!(rt.find("row-transform").is_some());
        assert!(rt.find("row-scale").is_some());
        assert!(
            rt.find("row-adaptive-sync").is_some(),
            "DP-1 reports adaptive sync"
        );
        assert!(rt.find("row-position").is_some());
        assert!(
            rt.find("row-custom").is_none(),
            "no custom row while a listed mode is current"
        );
        // The disabled display selected: its position row is disabled.
        rt.update(|s: &mut State, _| select_output(s, "HDMI-A-1"));
        rt.frame(1.0 / 60.0);
        assert!(rt.find("chip-HDMI-A-1").is_some());
        assert!(rt.find("row-position").is_some());
    }

    #[test]
    fn custom_modes_and_disabled_outputs_shape_the_panel() {
        let mut s = fixture_state();
        // A custom mode: the row shows and the mode select says so.
        let mut outputs = s.outputs.clone().unwrap();
        outputs[1].modes.clear();
        install(&mut s, outputs, true);
        assert!(
            s.custom_open,
            "an output running a custom mode shows the row"
        );
        let mut rt = runtime(s);
        rt.frame(0.0);
        assert!(rt.find("row-custom").is_some());
        // Every display off: the map says so and the chips list them all.
        let mut s = fixture_state();
        let mut outputs = s.outputs.clone().unwrap();
        for o in &mut outputs {
            o.enabled = false;
            o.position = None;
        }
        install(&mut s, outputs, true);
        let mut rt = runtime(s);
        rt.frame(0.0);
        assert!(rt.find("chip-eDP-1").is_some());
        assert!(rt.find("chip-DP-1").is_some());
        assert!(rt.find("chip-HDMI-A-1").is_some());
    }

    #[test]
    fn map_layout_scales_and_centers_the_enabled_outputs() {
        let outputs = parse(SAMPLE).unwrap();
        // eDP-1: 1280×800 logical at (0,0); DP-1: 1920×1080 rotated at
        // (1280,0) → 1080×1920. Bounds: 2360 × 1920.
        let layout = map_layout(&outputs, [600.0, MAP_HEIGHT]);
        assert!((layout.scale - (MAP_HEIGHT as f64 - 28.0) / 1920.0).abs() < 1e-9);
        let (x, _) = layout.point(0, 0);
        let (right, _) = layout.point(2360, 0);
        assert!(((right - x) as f64 - 2360.0 * layout.scale).abs() < 1e-4);
        // The whole arrangement is centered horizontally.
        let margin = x - 0.0;
        assert!((margin as f64 - (600.0 - 2360.0 * layout.scale) / 2.0).abs() < 1e-4);
    }

    #[test]
    fn the_watcher_only_silently_reloads_loaded_non_demo_state() {
        let mut s = fixture_state();
        s.demo = false;
        let mut rt = runtime(s);
        rt.update(|s: &mut State, tx| {
            s.watch_due = Some(90.0);
            poll(s, 100.0, tx);
        });
        assert_eq!(rt.state.attempt, 1, "a silent reload started");
        // While busy, the due check defers instead of stacking reloads.
        rt.update(|s: &mut State, tx| {
            s.busy = Some(("eDP-1".into(), Change::On));
            s.watch_due = Some(200.0);
            poll(s, 300.0, tx);
            assert!(s.reload_pending, "the reload waits for the busy change");
        });
        rt.update(|s: &mut State, tx| {
            s.busy = None;
            poll(s, 301.0, tx);
        });
        assert_eq!(rt.state.attempt, 2, "the deferred reload ran once free");
        // A demo scene never reloads.
        let mut rt = runtime(fixture_state());
        rt.update(|s: &mut State, tx| {
            s.watch_due = Some(90.0);
            poll(s, 100.0, tx);
        });
        assert_eq!(rt.state.attempt, 0);
    }

    #[test]
    fn apply_results_fence_by_attempt_and_refresh_the_inputs() {
        let mut s = fixture_state();
        s.demo = false;
        s.attempt = 7;
        s.busy = Some(("DP-1".into(), Change::Scale(1.25)));
        s.pos_x = TextEdit::new("999");
        let fresh = parse(SAMPLE).unwrap();
        let mut rt = runtime(s);
        rt.update(|s: &mut State, tx| {
            // A stale result is dropped outright.
            apply_event(
                s,
                UiEvent::Applied {
                    attempt: 6,
                    output: "DP-1".into(),
                    change: Change::Scale(1.25),
                    result: Ok(()),
                    fresh: Ok(fresh.clone()),
                },
                tx,
            );
        });
        assert!(rt.state.busy.is_some(), "a stale event changes nothing");
        rt.update(|s: &mut State, tx| {
            apply_event(
                s,
                UiEvent::Applied {
                    attempt: 7,
                    output: "DP-1".into(),
                    change: Change::Scale(1.25),
                    result: Ok(()),
                    fresh: Ok(fresh.clone()),
                },
                tx,
            );
        });
        assert!(rt.state.busy.is_none());
        assert_eq!(
            rt.state.pos_x.text, "1280",
            "the position fields refresh from the read-back"
        );
        assert_eq!(rt.state.status, "DP-1: scale 1.25");
        // A failed run keeps the fresh state and reports through the dialog.
        rt.update(|s: &mut State, tx| {
            s.attempt = 8;
            s.busy = Some((
                "DP-1".into(),
                Change::Mode(Mode {
                    width: 4096,
                    height: 2160,
                    refresh: Some(120.0),
                    preferred: false,
                    current: false,
                }),
            ));
            apply_event(
                s,
                UiEvent::Applied {
                    attempt: 8,
                    output: "DP-1".into(),
                    change: Change::Mode(Mode {
                        width: 4096,
                        height: 2160,
                        refresh: Some(120.0),
                        preferred: false,
                        current: false,
                    }),
                    result: Err("unknown mode: 4096x2160@120Hz".into()),
                    fresh: Ok(fresh),
                },
                tx,
            );
        });
        assert!(matches!(rt.state.dialog, Some(Panel::Error(_))));
        assert_eq!(rt.state.status, "Failed");
    }

    #[test]
    fn load_failures_show_the_error_and_retry_starts_a_fresh_query() {
        let mut rt = runtime(State::default());
        rt.update(|s: &mut State, tx| {
            apply_event(
                s,
                UiEvent::Loaded {
                    attempt: 0,
                    result: Err("failed to connect to display".into()),
                    silent: false,
                },
                tx,
            );
        });
        assert!(rt.state.load_error.is_some());
        rt.frame(0.0);
        assert!(rt.find("retry").is_some(), "the load error offers a retry");
        rt.update(|s: &mut State, _| retry_load(s));
        assert!(rt.state.load_error.is_none());
        assert_eq!(rt.state.attempt, 1, "retry started a new query");
    }
}
