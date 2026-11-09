use crate::api::{Client, Device, Scene};
use anyhow::Result;
use ratatui::{
    crossterm::event::{self, Event, KeyCode, KeyEventKind},
    prelude::*,
    widgets::*,
};
use serde_json::{json, Map, Value};
use std::{
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

const REFRESH_EVERY: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------
// Background worker: all HTTP happens here so the UI never blocks.
// ---------------------------------------------------------------------------

enum Cmd {
    Refresh,
    Set {
        id: String,
        attrs: Map<String, Value>,
        seq: u64,
    },
    Scene {
        id: String,
        name: String,
    },
}

enum Msg {
    /// `seq` = the last local edit the hub had seen when this data was fetched.
    Data {
        devices: Vec<Device>,
        scenes: Vec<Scene>,
        seq: u64,
    },
    Error(String),
    Info(String),
}

fn worker(client: Client, rx: mpsc::Receiver<Cmd>, tx: mpsc::Sender<Msg>) {
    let mut applied_seq = 0;
    while let Ok(first) = rx.recv() {
        let mut batch = vec![first];
        let mut refresh = false;
        let mut sets: Vec<(String, Map<String, Value>)> = Vec::new();
        let mut scenes = Vec::new();

        // Loop so that edits arriving during the settle delay get coalesced too
        // (e.g. holding down the brightness key).
        loop {
            batch.extend(rx.try_iter());
            let had_sets = !sets.is_empty();
            for cmd in batch.drain(..) {
                match cmd {
                    Cmd::Refresh => refresh = true,
                    Cmd::Set { id, attrs, seq } => {
                        applied_seq = applied_seq.max(seq);
                        refresh = true;
                        match sets.iter_mut().find(|(i, _)| *i == id) {
                            Some((_, m)) => m.extend(attrs),
                            None => sets.push((id, attrs)),
                        }
                    }
                    Cmd::Scene { id, name } => {
                        refresh = true;
                        scenes.push((id, name));
                    }
                }
            }
            if sets.is_empty() || had_sets {
                break;
            }
            thread::sleep(Duration::from_millis(60));
        }

        for (id, attrs) in sets {
            if let Err(e) = client.set_attributes(&id, &attrs) {
                let _ = tx.send(Msg::Error(format!("{e:#}")));
            }
        }
        for (id, name) in scenes {
            let _ = tx.send(match client.trigger_scene(&id) {
                Ok(()) => Msg::Info(format!("Triggered scene “{name}”")),
                Err(e) => Msg::Error(format!("{e:#}")),
            });
        }
        if refresh {
            // Give the hub a moment to settle so we don't read back stale values.
            thread::sleep(Duration::from_millis(250));
            let res = client.devices().and_then(|d| Ok((d, client.scenes()?)));
            let _ = tx.send(match res {
                Ok((devices, scenes)) => Msg::Data {
                    devices,
                    scenes,
                    seq: applied_seq,
                },
                Err(e) => Msg::Error(format!("{e:#}")),
            });
        }
    }
}

// ---------------------------------------------------------------------------
// App state
// ---------------------------------------------------------------------------

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Devices,
    Scenes,
}

struct App {
    tab: Tab,
    devices: Vec<Device>,
    scenes: Vec<Scene>,
    dev_state: TableState,
    scene_state: ListState,
    status: Option<(String, bool, Instant)>,
    tx: mpsc::Sender<Cmd>,
    seq: u64,
    loaded: bool,
    raw: bool,
    help: bool,
    quit: bool,
}

fn type_rank(t: &str) -> u8 {
    match t {
        "light" => 0,
        "outlet" => 1,
        "blinds" => 2,
        "airPurifier" => 3,
        "speaker" => 4,
        _ if t.ends_with("Sensor") => 5,
        _ => 6,
    }
}

impl App {
    fn status(&mut self, msg: impl Into<String>, err: bool) {
        self.status = Some((msg.into(), err, Instant::now()));
    }

    fn on_data(&mut self, mut devices: Vec<Device>, scenes: Vec<Scene>, seq: u64) {
        if seq < self.seq {
            return; // fetched before our latest edit reached the hub; a newer one is coming
        }
        devices.retain(|d| d.device_type != "gateway");
        devices.sort_by(|a, b| {
            (
                a.room.is_none(),
                a.room_name(),
                type_rank(&a.device_type),
                a.name(),
            )
                .cmp(&(
                    b.room.is_none(),
                    b.room_name(),
                    type_rank(&b.device_type),
                    b.name(),
                ))
        });
        let mut scenes = scenes;
        scenes.sort_by_key(|s| s.info.name.to_lowercase());

        // Keep the selection on the same device/scene across refreshes.
        let keep = |old: Option<usize>, old_ids: Vec<&str>, new_ids: Vec<&str>| -> Option<usize> {
            let id = old.and_then(|i| old_ids.get(i).copied());
            id.and_then(|id| new_ids.iter().position(|n| *n == id))
                .or(if new_ids.is_empty() {
                    None
                } else {
                    Some(old.unwrap_or(0).min(new_ids.len() - 1))
                })
        };
        let d_sel = keep(
            self.dev_state.selected(),
            self.devices.iter().map(|d| d.id.as_str()).collect(),
            devices.iter().map(|d| d.id.as_str()).collect(),
        );
        let s_sel = keep(
            self.scene_state.selected(),
            self.scenes.iter().map(|s| s.id.as_str()).collect(),
            scenes.iter().map(|s| s.id.as_str()).collect(),
        );
        self.devices = devices;
        self.scenes = scenes;
        self.dev_state.select(d_sel);
        self.scene_state.select(s_sel);
        self.loaded = true;
    }

    fn selected(&self) -> Option<usize> {
        self.dev_state
            .selected()
            .filter(|&i| i < self.devices.len())
    }

    /// Apply attributes optimistically and send them to the hub.
    fn set(&mut self, idx: usize, attrs: Value) {
        let Value::Object(attrs) = attrs else { return };
        let d = &mut self.devices[idx];
        d.attributes.extend(attrs.clone());
        self.seq += 1;
        let _ = self.tx.send(Cmd::Set {
            id: d.id.clone(),
            attrs,
            seq: self.seq,
        });
    }

    fn toggle(&mut self, i: usize) {
        let d = &self.devices[i];
        if d.device_type == "blinds" {
            let cur = d
                .n("blindsTargetLevel")
                .or(d.n("blindsCurrentLevel"))
                .unwrap_or(0.0);
            self.set(
                i,
                json!({ "blindsTargetLevel": if cur < 50.0 { 100 } else { 0 } }),
            );
        } else if d.device_type == "airPurifier" {
            const MODES: [&str; 5] = ["off", "auto", "low", "medium", "high"];
            let cur = d.s("fanMode").unwrap_or("off");
            let next = MODES[(MODES.iter().position(|m| *m == cur).unwrap_or(0) + 1) % MODES.len()];
            self.set(i, json!({ "fanMode": next }));
        } else if let Some(on) = d.b("isOn") {
            self.set(i, json!({ "isOn": !on }));
        } else {
            self.status("Nothing to toggle on this device", true);
        }
    }

    fn adjust(&mut self, i: usize, delta: f64) {
        let d = &self.devices[i];
        if let Some(lvl) = d.n("lightLevel") {
            let new = (lvl + delta).clamp(1.0, 100.0) as i64;
            if d.b("isOn") == Some(false) && delta > 0.0 {
                self.set(i, json!({ "isOn": true, "lightLevel": new }));
            } else {
                self.set(i, json!({ "lightLevel": new }));
            }
        } else if d.device_type == "blinds" {
            let cur = d
                .n("blindsTargetLevel")
                .or(d.n("blindsCurrentLevel"))
                .unwrap_or(0.0);
            self.set(
                i,
                json!({ "blindsTargetLevel": (cur + delta).clamp(0.0, 100.0) as i64 }),
            );
        } else {
            self.status("This device has no adjustable level", true);
        }
    }

    fn color_temp(&mut self, i: usize, dir: f64) {
        let d = &self.devices[i];
        let (Some(cur), Some(a), Some(b)) = (
            d.n("colorTemperature"),
            d.n("colorTemperatureMin"),
            d.n("colorTemperatureMax"),
        ) else {
            return self.status("This device has no color temperature", true);
        };
        let (lo, hi) = (a.min(b), a.max(b));
        let step = ((hi - lo) / 10.0).max(1.0);
        self.set(
            i,
            json!({ "colorTemperature": (cur + dir * step).clamp(lo, hi) as i64 }),
        );
    }

    fn hue(&mut self, i: usize, dir: f64) {
        let d = &self.devices[i];
        let Some(h) = d.n("colorHue") else {
            return self.status("This device has no color support", true);
        };
        let sat = d.n("colorSaturation").unwrap_or(0.0);
        let sat = if sat < 0.1 { 1.0 } else { sat };
        self.set(
            i,
            json!({ "colorHue": (h + 30.0 * dir).rem_euclid(360.0), "colorSaturation": sat }),
        );
    }

    fn room_toggle(&mut self, i: usize) {
        let Some(room) = self.devices[i].room.as_ref().map(|r| r.id.clone()) else {
            return self.status("Device isn't in a room", true);
        };
        let idxs: Vec<usize> = (0..self.devices.len())
            .filter(|&j| {
                let d = &self.devices[j];
                d.device_type == "light"
                    && d.b("isOn").is_some()
                    && d.room.as_ref().map(|r| &r.id) == Some(&room)
            })
            .collect();
        let any_on = idxs
            .iter()
            .any(|&j| self.devices[j].b("isOn") == Some(true));
        for j in idxs {
            self.set(j, json!({ "isOn": !any_on }));
        }
        let name = self.devices[i].room_name().to_string();
        self.status(
            format!("{name}: lights {}", if any_on { "off" } else { "on" }),
            false,
        );
    }

    fn on_key(&mut self, code: KeyCode) {
        if self.help {
            self.help = false;
            return;
        }
        match code {
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Char('?') => self.help = true,
            KeyCode::Tab | KeyCode::BackTab => {
                self.tab = if self.tab == Tab::Devices {
                    Tab::Scenes
                } else {
                    Tab::Devices
                }
            }
            KeyCode::Char('1') => self.tab = Tab::Devices,
            KeyCode::Char('2') => self.tab = Tab::Scenes,
            KeyCode::Char('r') => {
                let _ = self.tx.send(Cmd::Refresh);
                self.status("Refreshing…", false);
            }
            KeyCode::Char('i') => self.raw = !self.raw,
            KeyCode::Down | KeyCode::Char('j') => self.move_sel(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_sel(-1),
            KeyCode::PageDown => self.move_sel(10),
            KeyCode::PageUp => self.move_sel(-10),
            KeyCode::Home | KeyCode::Char('g') => self.move_sel(-10_000),
            KeyCode::End | KeyCode::Char('G') => self.move_sel(10_000),
            _ if self.tab == Tab::Scenes => {
                if matches!(code, KeyCode::Enter | KeyCode::Char(' ')) {
                    if let Some(s) = self.scene_state.selected().and_then(|i| self.scenes.get(i)) {
                        let _ = self.tx.send(Cmd::Scene {
                            id: s.id.clone(),
                            name: s.info.name.clone(),
                        });
                    }
                }
            }
            _ => {
                let Some(i) = self.selected() else { return };
                match code {
                    KeyCode::Enter | KeyCode::Char(' ') => self.toggle(i),
                    KeyCode::Right | KeyCode::Char('l' | '+' | '=') => self.adjust(i, 10.0),
                    KeyCode::Left | KeyCode::Char('h' | '-') => self.adjust(i, -10.0),
                    KeyCode::Char('L') => self.adjust(i, 1.0),
                    KeyCode::Char('H') => self.adjust(i, -1.0),
                    KeyCode::Char('[') => self.color_temp(i, -1.0),
                    KeyCode::Char(']') => self.color_temp(i, 1.0),
                    KeyCode::Char('c') => self.hue(i, 1.0),
                    KeyCode::Char('C') => self.hue(i, -1.0),
                    KeyCode::Char('a') => self.room_toggle(i),
                    _ => {}
                }
            }
        }
    }

    fn move_sel(&mut self, delta: i64) {
        let (len, sel) = match self.tab {
            Tab::Devices => (self.devices.len(), self.dev_state.selected()),
            Tab::Scenes => (self.scenes.len(), self.scene_state.selected()),
        };
        if len == 0 {
            return;
        }
        let new = Some((sel.unwrap_or(0) as i64 + delta).clamp(0, len as i64 - 1) as usize);
        match self.tab {
            Tab::Devices => self.dev_state.select(new),
            Tab::Scenes => self.scene_state.select(new),
        }
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

fn hsv(h: f64, s: f64, v: f64) -> Color {
    let c = v * s;
    let x = c * (1.0 - ((h / 60.0) % 2.0 - 1.0).abs());
    let (r, g, b) = match (h / 60.0) as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = v - c;
    let f = |u: f64| ((u + m) * 255.0) as u8;
    Color::Rgb(f(r), f(g), f(b))
}

/// Approximate display color for a color temperature in Kelvin.
fn kelvin_color(k: f64) -> Color {
    let t = ((k - 2200.0) / (6500.0 - 2200.0)).clamp(0.0, 1.0);
    let lerp = |a: f64, b: f64| (a + (b - a) * t) as u8;
    Color::Rgb(lerp(255.0, 210.0), lerp(160.0, 225.0), lerp(60.0, 255.0))
}

fn bar(frac: f64, width: usize, color: Color) -> Vec<Span<'static>> {
    let filled = ((frac.clamp(0.0, 1.0) * width as f64).round()) as usize;
    vec![
        Span::styled("█".repeat(filled), Style::new().fg(color)),
        Span::styled("░".repeat(width - filled), Style::new().fg(Color::DarkGray)),
    ]
}

/// (icon, icon style, short value text) for the device list.
fn summary(d: &Device) -> (&'static str, Style, String) {
    let dim = Style::new().fg(Color::DarkGray);
    if !d.is_reachable {
        return ("✗", Style::new().fg(Color::Red), "unreachable".into());
    }
    let on_off = |on: bool, txt: String| {
        if on {
            ("●", Style::new().fg(Color::Yellow), txt)
        } else {
            ("○", dim, "off".into())
        }
    };
    match d.device_type.as_str() {
        "light" => {
            let on = d.b("isOn").unwrap_or(false);
            let lvl = d
                .n("lightLevel")
                .map(|l| format!("{l:.0}%"))
                .unwrap_or_else(|| "on".into());
            on_off(on, lvl)
        }
        "outlet" => on_off(d.b("isOn").unwrap_or(false), "on".into()),
        "blinds" => {
            let cur = d.n("blindsCurrentLevel").unwrap_or(0.0);
            let tgt = d.n("blindsTargetLevel").unwrap_or(cur);
            let txt = if (cur - tgt).abs() > 0.5 {
                format!("{cur:.0}→{tgt:.0}%")
            } else {
                format!("{cur:.0}% closed")
            };
            ("▤", Style::new().fg(Color::Cyan), txt)
        }
        "airPurifier" => {
            let mode = d.s("fanMode").unwrap_or("?").to_string();
            on_off(mode != "off", format!("fan {mode}"))
        }
        "environmentSensor" => {
            let mut parts = vec![];
            if let Some(t) = d.n("currentTemperature") {
                parts.push(format!("{t:.1}°C"));
            }
            if let Some(h) = d.n("currentRH") {
                parts.push(format!("{h:.0}%"));
            }
            ("◇", Style::new().fg(Color::Green), parts.join(" "))
        }
        "motionSensor" => match d.b("isDetected") {
            Some(true) => ("◆", Style::new().fg(Color::Magenta), "motion".into()),
            _ => ("◇", dim, "idle".into()),
        },
        "openCloseSensor" => match d.b("isOpen") {
            Some(true) => ("◆", Style::new().fg(Color::Magenta), "open".into()),
            _ => ("◇", dim, "closed".into()),
        },
        "waterSensor" => match d.b("waterLeakDetected") {
            Some(true) => ("!", Style::new().fg(Color::Red).bold(), "LEAK".into()),
            _ => ("◇", dim, "dry".into()),
        },
        _ => {
            if let Some(on) = d.b("isOn") {
                on_off(on, "on".into())
            } else if let Some(b) = d.n("batteryPercentage") {
                ("·", dim, format!("bat {b:.0}%"))
            } else {
                ("·", dim, String::new())
            }
        }
    }
}

fn details(d: &Device, raw: bool) -> Vec<Line<'static>> {
    let label = |s: &str| Span::styled(format!("{s:<12}"), Style::new().fg(Color::DarkGray));
    let mut lines = Vec::new();
    if raw {
        let txt = serde_json::to_string_pretty(&d.attributes).unwrap_or_default();
        return txt.lines().map(|l| Line::raw(l.to_string())).collect();
    }
    let mut kv = |k: &str, v: String| lines.push(Line::from(vec![label(k), Span::raw(v)]));
    kv("Type", d.device_type.clone());
    kv("Room", d.room_name().to_string());
    if let Some(m) = d.s("model") {
        kv("Model", m.to_string());
    }
    if let Some(f) = d.s("firmwareVersion") {
        kv("Firmware", f.to_string());
    }
    kv(
        "Reachable",
        if d.is_reachable {
            "yes".into()
        } else {
            "no".into()
        },
    );
    if let Some(b) = d.n("batteryPercentage") {
        kv("Battery", format!("{b:.0}%"));
    }
    if let Some(on) = d.b("isOn") {
        kv("Power", if on { "on".into() } else { "off".into() });
    }
    if let Some(t) = d.n("currentTemperature") {
        kv("Temperature", format!("{t:.1} °C"));
    }
    if let Some(h) = d.n("currentRH") {
        kv("Humidity", format!("{h:.0} %"));
    }
    if let Some(p) = d.n("currentPM25") {
        kv("PM2.5", format!("{p:.0} µg/m³"));
    }
    if let Some(m) = d.s("fanMode") {
        kv("Fan mode", m.to_string());
    }
    lines.push(Line::raw(""));

    const W: usize = 20;
    if let Some(l) = d.n("lightLevel") {
        let mut spans = vec![label("Brightness")];
        spans.extend(bar(l / 100.0, W, Color::Yellow));
        spans.push(Span::raw(format!(" {l:.0}%")));
        lines.push(Line::from(spans));
    }
    if let (Some(k), Some(a), Some(b)) = (
        d.n("colorTemperature"),
        d.n("colorTemperatureMin"),
        d.n("colorTemperatureMax"),
    ) {
        let (lo, hi) = (a.min(b), a.max(b));
        let mut spans = vec![label("Color temp")];
        spans.extend(bar((k - lo) / (hi - lo).max(1.0), W, kelvin_color(k)));
        spans.push(Span::raw(format!(" {k:.0}K")));
        lines.push(Line::from(spans));
    }
    if let Some(h) = d.n("colorHue") {
        let s = d.n("colorSaturation").unwrap_or(0.0);
        lines.push(Line::from(vec![
            label("Color"),
            Span::styled(
                "█".repeat(W),
                Style::new().fg(hsv(h, s.clamp(0.0, 1.0), 1.0)),
            ),
            Span::raw(format!(" {h:.0}° / {:.0}%", s * 100.0)),
        ]));
    }
    if d.device_type == "blinds" {
        let cur = d.n("blindsCurrentLevel").unwrap_or(0.0);
        let mut spans = vec![label("Closed")];
        spans.extend(bar(cur / 100.0, W, Color::Cyan));
        spans.push(Span::raw(format!(" {cur:.0}%")));
        lines.push(Line::from(spans));
    }
    lines
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect::new(
        area.x + (area.width - w) / 2,
        area.y + (area.height - h) / 2,
        w,
        h,
    )
}

const HELP: &[(&str, &str)] = &[
    ("j/k ↑/↓", "move"),
    ("Tab 1 2", "switch Devices / Scenes"),
    ("Space Enter", "toggle / trigger scene"),
    ("h/l ←/→ -/+", "brightness or blinds ±10"),
    ("H/L", "brightness ±1"),
    ("[ ]", "warmer / cooler"),
    ("c C", "cycle hue"),
    ("a", "toggle all lights in room"),
    ("i", "show raw attributes"),
    ("r", "refresh"),
    ("q Esc", "quit"),
];

fn draw(f: &mut Frame, app: &mut App) {
    let [top, body, foot] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(f.area());

    let tabs = Tabs::new([" Devices ", " Scenes "])
        .select(if app.tab == Tab::Devices { 0 } else { 1 })
        .highlight_style(Style::new().fg(Color::Black).bg(Color::Yellow).bold())
        .divider("");
    let [tabs_area, title_area] =
        Layout::horizontal([Constraint::Min(0), Constraint::Length(12)]).areas(top);
    f.render_widget(tabs, tabs_area);
    f.render_widget(
        Line::styled("DIRIGERA ", Style::new().fg(Color::DarkGray)).right_aligned(),
        title_area,
    );

    if !app.loaded {
        let msg = app
            .status
            .as_ref()
            .filter(|s| s.1)
            .map(|s| s.0.clone())
            .unwrap_or_else(|| "Connecting to hub…".into());
        f.render_widget(
            Paragraph::new(msg).centered().wrap(Wrap { trim: true }),
            centered(body, 60, 3),
        );
    } else if app.tab == Tab::Devices {
        let [list_area, detail_area] =
            Layout::horizontal([Constraint::Percentage(58), Constraint::Percentage(42)])
                .areas(body);
        let rows = app.devices.iter().map(|d| {
            let (icon, istyle, val) = summary(d);
            let row = Row::new(vec![
                Cell::from(Span::styled(icon, istyle)),
                Cell::from(d.name()),
                Cell::from(Span::styled(
                    d.room_name().to_string(),
                    Style::new().fg(Color::DarkGray),
                )),
                Cell::from(Line::from(val).right_aligned()),
            ]);
            if d.is_reachable {
                row
            } else {
                row.style(Style::new().fg(Color::DarkGray))
            }
        });
        let table = Table::new(
            rows,
            [
                Constraint::Length(1),
                Constraint::Fill(2),
                Constraint::Fill(1),
                Constraint::Length(12),
            ],
        )
        .block(Block::bordered().title(format!(" Devices ({}) ", app.devices.len())))
        .row_highlight_style(Style::new().bg(Color::Rgb(50, 50, 60)).bold())
        .highlight_symbol("▶ ");
        f.render_stateful_widget(table, list_area, &mut app.dev_state);

        if let Some(d) = app.selected().map(|i| &app.devices[i]) {
            let title = format!(" {} {}", d.name(), if app.raw { "(raw) " } else { "" });
            f.render_widget(
                Paragraph::new(details(d, app.raw))
                    .block(Block::bordered().title(title))
                    .wrap(Wrap { trim: false }),
                detail_area,
            );
        } else {
            f.render_widget(Block::bordered(), detail_area);
        }
    } else {
        let items: Vec<ListItem> = app
            .scenes
            .iter()
            .map(|s| ListItem::new(s.info.name.clone()))
            .collect();
        let list = List::new(items)
            .block(Block::bordered().title(format!(
                " Scenes ({}) — Enter to trigger ",
                app.scenes.len()
            )))
            .highlight_style(Style::new().bg(Color::Rgb(50, 50, 60)).bold())
            .highlight_symbol("▶ ");
        f.render_stateful_widget(list, body, &mut app.scene_state);
    }

    // Footer: recent status message, otherwise key hints.
    let footer = match &app.status {
        Some((msg, err, t)) if t.elapsed() < Duration::from_secs(5) => Line::styled(
            format!(" {msg}"),
            Style::new().fg(if *err { Color::Red } else { Color::Green }),
        ),
        _ => Line::styled(
            " space toggle · h/l level · [ ] temp · c hue · a room · tab scenes · ? help · q quit",
            Style::new().fg(Color::DarkGray),
        ),
    };
    f.render_widget(footer, foot);

    if app.help {
        let lines: Vec<Line> = HELP
            .iter()
            .map(|(k, v)| {
                Line::from(vec![
                    Span::styled(format!(" {k:<14}"), Style::new().fg(Color::Yellow)),
                    Span::raw(*v),
                ])
            })
            .collect();
        let area = centered(f.area(), 46, HELP.len() as u16 + 2);
        f.render_widget(Clear, area);
        f.render_widget(
            Paragraph::new(lines).block(Block::bordered().title(" Keys ")),
            area,
        );
    }
}

// ---------------------------------------------------------------------------

pub fn run(client: Client) -> Result<()> {
    let (cmd_tx, cmd_rx) = mpsc::channel();
    let (msg_tx, msg_rx) = mpsc::channel();
    thread::spawn(move || worker(client, cmd_rx, msg_tx));
    cmd_tx.send(Cmd::Refresh)?;

    let mut app = App {
        tab: Tab::Devices,
        devices: vec![],
        scenes: vec![],
        dev_state: TableState::default(),
        scene_state: ListState::default(),
        status: None,
        tx: cmd_tx,
        seq: 0,
        loaded: false,
        raw: false,
        help: false,
        quit: false,
    };

    let mut terminal = ratatui::init();
    let mut last_refresh = Instant::now();
    let res = (|| -> Result<()> {
        while !app.quit {
            for msg in msg_rx.try_iter() {
                match msg {
                    Msg::Data {
                        devices,
                        scenes,
                        seq,
                    } => app.on_data(devices, scenes, seq),
                    Msg::Error(e) => app.status(e, true),
                    Msg::Info(i) => app.status(i, false),
                }
            }
            terminal.draw(|f| draw(f, &mut app))?;
            if event::poll(Duration::from_millis(100))? {
                if let Event::Key(k) = event::read()? {
                    if k.kind == KeyEventKind::Press {
                        app.on_key(k.code);
                    }
                }
            }
            if last_refresh.elapsed() >= REFRESH_EVERY {
                last_refresh = Instant::now();
                let _ = app.tx.send(Cmd::Refresh);
            }
        }
        Ok(())
    })();
    ratatui::restore();
    res
}
