use crate::api::{Client, Device, Room, Scene};
use anyhow::Result;
use ratatui::{
    crossterm::event::{self, Event, KeyCode, KeyEventKind},
    prelude::*,
    widgets::*,
};
use serde_json::{json, Map, Value};
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
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

/// One row in the device list.
#[derive(PartialEq, Clone, Copy)]
enum Item {
    /// Index into `App::devices`.
    Dev(usize),
    /// Index into `App::sets`.
    Set(usize),
    /// Index into `App::rooms`; a header row.
    Room(usize),
}

/// The devices in one room, in display order.
struct RoomGroup {
    /// Room id, or "none" for devices without a room.
    key: String,
    name: String,
    /// Indices into `App::devices`, never empty.
    members: Vec<usize>,
}

fn room_key(d: &Device) -> String {
    d.room
        .as_ref()
        .map_or_else(|| "none".into(), |r| r.id.clone())
}

/// Remotes, dimmers and switches: listed, but nothing to show or control.
fn is_remote(d: &Device) -> bool {
    d.device_type.ends_with("Controller") || d.device_type == "genericSwitch"
}

/// A device set ("combined light"), assembled from the members' `deviceSet` field.
struct DevSet {
    id: String,
    name: String,
    /// Indices into `App::devices`, never empty.
    members: Vec<usize>,
}

/// One adjustable parameter of a device, shown as a row in the control panel.
#[derive(PartialEq, Clone, Copy)]
enum Ctl {
    Power,
    Level,
    Temp,
    Hue,
    Sat,
    Blinds,
    Fan,
}

impl Ctl {
    fn label(self) -> &'static str {
        match self {
            Ctl::Power => "Power",
            Ctl::Level => "Brightness",
            Ctl::Temp => "Temp",
            Ctl::Hue => "Hue",
            Ctl::Sat => "Saturation",
            Ctl::Blinds => "Position",
            Ctl::Fan => "Fan",
        }
    }
}

const ALL_CTLS: [Ctl; 7] = [
    Ctl::Power,
    Ctl::Level,
    Ctl::Temp,
    Ctl::Hue,
    Ctl::Sat,
    Ctl::Blinds,
    Ctl::Fan,
];

const FAN_MODES: [&str; 5] = ["off", "auto", "low", "medium", "high"];

/// (current, low, high) color temperature in Kelvin. The hub's min/max can be
/// in either order.
fn temp_range(d: &Device) -> Option<(f64, f64, f64)> {
    let (k, a, b) = (
        d.n("colorTemperature")?,
        d.n("colorTemperatureMin")?,
        d.n("colorTemperatureMax")?,
    );
    let (lo, hi) = (a.min(b), a.max(b));
    (hi > lo).then_some((k, lo, hi))
}

fn blinds_level(d: &Device) -> f64 {
    d.n("blindsTargetLevel")
        .or(d.n("blindsCurrentLevel"))
        .unwrap_or(0.0)
}

fn fan_index(d: &Device) -> usize {
    let cur = d.s("fanMode").unwrap_or("off");
    FAN_MODES.iter().position(|m| *m == cur).unwrap_or(0)
}

/// The controls a device supports, in display order.
fn ctls(d: &Device) -> Vec<Ctl> {
    let blinds = d.device_type == "blinds";
    [
        (
            Ctl::Power,
            !blinds && d.can("isOn") && d.b("isOn").is_some(),
        ),
        (
            Ctl::Level,
            d.can("lightLevel") && d.n("lightLevel").is_some(),
        ),
        (
            Ctl::Temp,
            d.can("colorTemperature") && temp_range(d).is_some(),
        ),
        (Ctl::Hue, d.can("colorHue") && d.n("colorHue").is_some()),
        (
            Ctl::Sat,
            d.can("colorSaturation") && d.n("colorSaturation").is_some(),
        ),
        (Ctl::Blinds, blinds && d.can("blindsTargetLevel")),
        (Ctl::Fan, d.device_type == "airPurifier" && d.can("fanMode")),
    ]
    .into_iter()
    .filter_map(|(c, ok)| ok.then_some(c))
    .collect()
}

/// How long a requested color is trusted over the hub's (snapped) report.
const WANTED_TTL: Duration = Duration::from_secs(30);

/// A color the user asked for, see `App::color_of`.
struct Wanted {
    hue: f64,
    sat: f64,
    at: Instant,
}

/// Distance between two hues in degrees, accounting for wrap-around.
fn hue_dist(a: f64, b: f64) -> f64 {
    let d = (a - b).rem_euclid(360.0);
    d.min(360.0 - d)
}

struct App {
    tab: Tab,
    devices: Vec<Device>,
    sets: Vec<DevSet>,
    rooms: Vec<RoomGroup>,
    items: Vec<Item>,
    /// Keys of folded rooms. Saved in `state_path`, with `hide_remotes`.
    collapsed: HashSet<String>,
    hide_remotes: bool,
    /// Ids of outlets the user marked as lamps; they count as the room's lights.
    lamps: HashSet<String>,
    state_path: Option<PathBuf>,
    /// Ids of sets whose members are shown as separate rows.
    expanded: HashSet<String>,
    scenes: Vec<Scene>,
    dev_state: TableState,
    scene_state: ListState,
    status: Option<(String, bool, Instant)>,
    tx: mpsc::Sender<Cmd>,
    seq: u64,
    loaded: bool,
    raw: bool,
    /// Selected control row while the control panel has focus.
    focus: Option<usize>,
    /// Last requested color per device id.
    wanted: HashMap<String, Wanted>,
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
        // Secondary parts of a device (an outlet's power meter, a remote's extra
        // buttons) often have no room; give them the room of the device they belong to.
        let rooms_by_relation: HashMap<String, Room> = devices
            .iter()
            .filter_map(|d| Some((d.relation_id.clone()?, d.room.clone()?)))
            .collect();
        for d in devices.iter_mut().filter(|d| d.room.is_none()) {
            if let Some(r) = d
                .relation_id
                .as_ref()
                .and_then(|id| rooms_by_relation.get(id))
            {
                d.room = Some(r.clone());
            }
        }
        // Members of a set sort under the set's name so they stay together.
        let key = |d: &Device| {
            (
                d.room.is_none(),
                d.room_name().to_string(),
                type_rank(&d.device_type),
                d.set().map(|s| s.name.clone()).unwrap_or_else(|| d.name()),
                d.set().map(|s| s.id.clone()),
                d.name(),
            )
        };
        devices.sort_by_cached_key(key);
        let mut scenes = scenes;
        scenes.sort_by_key(|s| s.info.name.to_lowercase());

        // Keep the selection on the same row/scene across refreshes.
        let old_key = self.sel_item().map(|it| self.item_key(it));
        let old_scene = self
            .scene_state
            .selected()
            .and_then(|i| self.scenes.get(i))
            .map(|s| s.id.clone());
        self.devices = devices;
        self.scenes = scenes;
        self.rebuild();
        self.select_key(old_key, self.dev_state.selected());
        let s_sel = old_scene
            .and_then(|id| self.scenes.iter().position(|s| s.id == id))
            .or_else(|| {
                (!self.scenes.is_empty()).then(|| {
                    self.scene_state
                        .selected()
                        .unwrap_or(0)
                        .min(self.scenes.len() - 1)
                })
            });
        self.scene_state.select(s_sel);
        self.loaded = true;
    }

    /// Rebuild `sets`, `rooms` and the visible rows from `devices` (which must be
    /// sorted by room).
    fn rebuild(&mut self) {
        self.sets.clear();
        self.rooms.clear();
        for (i, d) in self.devices.iter().enumerate() {
            if let Some(s) = d.set() {
                match self.sets.iter_mut().find(|x| x.id == s.id) {
                    Some(x) => x.members.push(i),
                    None => self.sets.push(DevSet {
                        id: s.id.clone(),
                        name: if s.name.is_empty() {
                            "Device set".into()
                        } else {
                            s.name.clone()
                        },
                        members: vec![i],
                    }),
                }
            }
            let key = room_key(d);
            match self.rooms.iter_mut().find(|r| r.key == key) {
                Some(r) => r.members.push(i),
                None => self.rooms.push(RoomGroup {
                    key,
                    name: match &d.room {
                        Some(r) if !r.name.trim().is_empty() => r.name.trim().to_string(),
                        _ => "No room".into(),
                    },
                    members: vec![i],
                }),
            }
        }

        self.items.clear();
        for r in 0..self.rooms.len() {
            self.items.push(Item::Room(r));
            if self.collapsed.contains(&self.rooms[r].key) {
                continue;
            }
            let mut seen = HashSet::new();
            for &i in &self.rooms[r].members {
                let d = &self.devices[i];
                if self.hide_remotes && is_remote(d) {
                    continue;
                }
                match d
                    .set()
                    .and_then(|s| self.sets.iter().position(|x| x.id == s.id))
                {
                    None => self.items.push(Item::Dev(i)),
                    Some(k) if seen.insert(k) => {
                        self.items.push(Item::Set(k));
                        if self.expanded.contains(&self.sets[k].id) {
                            self.items
                                .extend(self.sets[k].members.iter().map(|&m| Item::Dev(m)));
                        }
                    }
                    Some(_) => {}
                }
            }
        }
    }

    fn item_key(&self, it: Item) -> String {
        match it {
            Item::Dev(i) => self.devices[i].id.clone(),
            Item::Set(k) => format!("set:{}", self.sets[k].id),
            Item::Room(r) => format!("room:{}", self.rooms[r].key),
        }
    }

    /// Select the row with `key`, or clamp `fallback` into range.
    fn select_key(&mut self, key: Option<String>, fallback: Option<usize>) {
        let pos =
            key.and_then(|k| (0..self.items.len()).find(|&j| self.item_key(self.items[j]) == k));
        let sel = pos.or_else(|| {
            (!self.items.is_empty()).then(|| fallback.unwrap_or(0).min(self.items.len() - 1))
        });
        self.dev_state.select(sel);
    }

    fn sel_item(&self) -> Option<Item> {
        self.dev_state
            .selected()
            .and_then(|i| self.items.get(i).copied())
    }

    /// The room (index into `rooms`) a row belongs to.
    fn room_of(&self, it: Item) -> usize {
        let dev = match it {
            Item::Room(r) => return r,
            Item::Dev(i) => i,
            Item::Set(k) => self.sets[k].members[0],
        };
        self.rooms
            .iter()
            .position(|r| r.members.contains(&dev))
            .unwrap_or(0)
    }

    /// (representative device, devices an edit applies to) for a row. A room
    /// controls its lights only, so room-wide edits never switch off an outlet.
    fn targets(&self, it: Item) -> (usize, Vec<usize>) {
        let ts = match it {
            Item::Dev(i) => return (i, vec![i]),
            Item::Set(k) => self.sets[k].members.clone(),
            Item::Room(r) => {
                let lights: Vec<usize> = self.rooms[r]
                    .members
                    .iter()
                    .copied()
                    .filter(|&j| self.is_light(j))
                    .collect();
                if lights.is_empty() {
                    return (self.rooms[r].members[0], lights);
                }
                lights
            }
        };
        let rep = ts
            .iter()
            .copied()
            .find(|&j| self.devices[j].is_reachable)
            .unwrap_or(ts[0]);
        (rep, ts)
    }

    /// Bulbs, plus outlets the user marked as lamps.
    fn is_light(&self, j: usize) -> bool {
        let d = &self.devices[j];
        d.device_type == "light" || (d.device_type == "outlet" && self.lamps.contains(&d.id))
    }

    /// `m`: mark/unmark an outlet as a lamp.
    fn toggle_lamp(&mut self, it: Item) {
        let Item::Dev(i) = it else {
            return self.status("Select an outlet to mark it as a lamp", true);
        };
        let d = &self.devices[i];
        if d.device_type != "outlet" {
            return self.status("Only outlets can be marked as lamps", true);
        }
        let (id, name) = (d.id.clone(), d.name());
        let msg = if self.lamps.remove(&id) {
            format!("{name} no longer counts as a lamp")
        } else {
            self.lamps.insert(id);
            format!("{name} now counts as a lamp in its room")
        };
        self.save_state();
        self.status(msg, false);
    }

    fn supports(&self, j: usize, ctl: Ctl) -> bool {
        ctls(&self.devices[j]).contains(&ctl)
    }

    /// Controls offered for a row: everything at least one target supports.
    fn item_ctls(&self, it: Item) -> Vec<Ctl> {
        let ts = self.targets(it).1;
        ALL_CTLS
            .into_iter()
            .filter(|&c| ts.iter().any(|&j| self.supports(j, c)))
            .collect()
    }

    /// The device whose value is shown (and edited from) for `ctl` on a row: the
    /// first reachable target supporting it. For temperature it's the one with
    /// the widest range, so a narrow bulb clamping doesn't hide the group value.
    fn shown(&self, it: Item, ctl: Ctl) -> usize {
        let (rep, ts) = self.targets(it);
        let ok: Vec<usize> = ts.into_iter().filter(|&j| self.supports(j, ctl)).collect();
        let reachable: Vec<usize> = ok
            .iter()
            .copied()
            .filter(|&j| self.devices[j].is_reachable)
            .collect();
        let pool = if reachable.is_empty() { ok } else { reachable };
        let width = |j: usize| temp_range(&self.devices[j]).map_or(0.0, |(_, lo, hi)| hi - lo);
        let pick = if ctl == Ctl::Temp {
            pool.iter()
                .copied()
                .reduce(|a, b| if width(b) > width(a) { b } else { a })
        } else {
            pool.first().copied()
        };
        pick.unwrap_or(rep)
    }

    fn apply(&mut self, ts: &[usize], attrs: Value) {
        for &t in ts {
            self.set(t, attrs.clone());
        }
    }

    /// `e`: fold/unfold a set or a room. On a device outside a set, folds its room.
    fn toggle_expand(&mut self, it: Item) {
        let set = match it {
            Item::Set(k) => Some(k),
            Item::Dev(i) => self.devices[i]
                .set()
                .and_then(|s| self.sets.iter().position(|x| x.id == s.id)),
            Item::Room(_) => None,
        };
        if let Some(k) = set {
            let id = self.sets[k].id.clone();
            if !self.expanded.remove(&id) {
                self.expanded.insert(id.clone());
            }
            self.rebuild();
            return self.select_key(Some(format!("set:{id}")), self.dev_state.selected());
        }
        let key = self.rooms[self.room_of(it)].key.clone();
        if !self.collapsed.remove(&key) {
            self.collapsed.insert(key.clone());
        }
        self.after_fold(key);
    }

    /// `E`: fold every room, or unfold them all if they're all folded already.
    fn toggle_fold_all(&mut self) {
        let key = self
            .sel_item()
            .map(|it| self.rooms[self.room_of(it)].key.clone());
        if self.rooms.iter().all(|r| self.collapsed.contains(&r.key)) {
            self.collapsed.clear();
        } else {
            self.collapsed = self.rooms.iter().map(|r| r.key.clone()).collect();
        }
        self.after_fold(key.unwrap_or_default());
    }

    fn toggle_remotes(&mut self) {
        self.hide_remotes = !self.hide_remotes;
        let key = self.sel_item().map(|it| self.item_key(it));
        self.rebuild();
        self.select_key(key, self.dev_state.selected());
        self.save_state();
        self.status(
            if self.hide_remotes {
                "Remotes and switches hidden (. to show)"
            } else {
                "Remotes and switches shown"
            },
            false,
        );
    }

    /// Rebuild after folding, keep the room header selected and remember the state.
    fn after_fold(&mut self, room_key: String) {
        self.rebuild();
        self.select_key(Some(format!("room:{room_key}")), self.dev_state.selected());
        self.save_state();
    }

    /// `{` / `}`: jump to the previous / next room header.
    fn jump_room(&mut self, dir: i64) {
        let cur = self.dev_state.selected().unwrap_or(0) as i64;
        let mut j = cur + dir;
        while j >= 0 && (j as usize) < self.items.len() {
            if matches!(self.items[j as usize], Item::Room(_)) {
                return self.dev_state.select(Some(j as usize));
            }
            j += dir;
        }
    }

    fn save_state(&self) {
        let Some(path) = &self.state_path else { return };
        let mut lines: Vec<String> = self
            .collapsed
            .iter()
            .map(|k| format!("collapsed {k}"))
            .collect();
        lines.sort();
        let mut lamps: Vec<String> = self.lamps.iter().map(|id| format!("lamp {id}")).collect();
        lamps.sort();
        lines.extend(lamps);
        if self.hide_remotes {
            lines.push("hide-remotes".into());
        }
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(path, lines.join("\n") + "\n");
    }

    fn load_state(&mut self) {
        let Some(path) = &self.state_path else { return };
        let Ok(txt) = std::fs::read_to_string(path) else {
            return;
        };
        for line in txt.lines() {
            if let Some(k) = line.strip_prefix("collapsed ") {
                self.collapsed.insert(k.to_string());
            } else if let Some(id) = line.strip_prefix("lamp ") {
                self.lamps.insert(id.to_string());
            } else if line == "hide-remotes" {
                self.hide_remotes = true;
            }
        }
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

    fn toggle(&mut self, it: Item) {
        let (i, ts) = self.targets(it);
        if ts.is_empty() {
            return self.status("No lights in this room", true);
        }
        let d = &self.devices[i];
        if d.device_type == "blinds" {
            let cur = blinds_level(d);
            self.apply(
                &ts,
                json!({ "blindsTargetLevel": if cur < 50.0 { 100 } else { 0 } }),
            );
        } else if d.device_type == "airPurifier" {
            let next = FAN_MODES[(fan_index(d) + 1) % FAN_MODES.len()];
            self.apply(&ts, json!({ "fanMode": next }));
        } else if ts.iter().any(|&j| self.supports(j, Ctl::Power)) {
            // For a group: if anything is on, turn everything off.
            let ts: Vec<usize> = ts
                .into_iter()
                .filter(|&j| self.supports(j, Ctl::Power))
                .collect();
            let any_on = ts.iter().any(|&j| self.devices[j].b("isOn") == Some(true));
            self.apply(&ts, json!({ "isOn": !any_on }));
            if let Item::Room(r) = it {
                let msg = format!(
                    "{}: lights {}",
                    self.rooms[r].name,
                    if any_on { "off" } else { "on" }
                );
                self.status(msg, false);
            }
        } else {
            self.status("Nothing to toggle on this device", true);
        }
    }

    /// The (hue, saturation) to build the next color edit on for device `i`.
    ///
    /// The hub converts hue/saturation to the bulb's own color space and back, so
    /// what it reports is snapped to colors the bulb can produce (90° can come back
    /// as 60° at full saturation) and loses ~1-2% saturation per round trip. Edits
    /// therefore build on what the user last asked for, as long as that was recent
    /// and the hub still reports something close to it (otherwise the color was
    /// probably changed elsewhere, e.g. in the IKEA app).
    fn color_of(&self, i: usize) -> (f64, f64) {
        let d = &self.devices[i];
        let h = d.n("colorHue").unwrap_or(0.0);
        let s = d.n("colorSaturation").unwrap_or(0.0).clamp(0.0, 1.0);
        match self.wanted.get(&d.id) {
            Some(w)
                if w.at.elapsed() < WANTED_TTL
                    && hue_dist(w.hue, h) < 60.0
                    && (w.sat - s).abs() < 0.15 =>
            {
                (w.hue, w.sat)
            }
            _ => (h, s),
        }
    }

    /// Set a control to an absolute value (in the control's own units) on every
    /// target that supports it. `wake` turns lights on when brightness changes, so
    /// brightening an off light works.
    fn set_ctl(&mut self, it: Item, ctl: Ctl, v: f64, wake: bool) {
        let ts: Vec<usize> = self
            .targets(it)
            .1
            .into_iter()
            .filter(|&j| self.supports(j, ctl))
            .collect();
        if ts.is_empty() {
            return;
        }
        let src = self.shown(it, ctl);
        let attrs = match ctl {
            Ctl::Power => json!({ "isOn": v > 0.5 }),
            Ctl::Level => {
                let lvl = v.round().clamp(1.0, 100.0) as i64;
                let any_off = ts.iter().any(|&j| self.devices[j].b("isOn") == Some(false));
                if wake && any_off {
                    json!({ "isOn": true, "lightLevel": lvl })
                } else {
                    json!({ "lightLevel": lvl })
                }
            }
            Ctl::Temp => {
                // The same Kelvin everywhere, clamped to what each bulb can do.
                for &t in &ts {
                    if let Some((_, lo, hi)) = temp_range(&self.devices[t]) {
                        let k = v.clamp(lo, hi).round() as i64;
                        self.set(t, json!({ "colorTemperature": k }));
                    }
                }
                return;
            }
            // The hub rejects colorSaturation unless colorHue is in the same PATCH,
            // so color edits always send both.
            Ctl::Hue | Ctl::Sat => {
                let (h, s) = self.color_of(src);
                let (hue, sat) = if ctl == Ctl::Hue {
                    // A hue change on a desaturated bulb would be invisible.
                    (v.rem_euclid(360.0).round(), if s < 0.1 { 1.0 } else { s })
                } else {
                    (h, (v.clamp(0.0, 1.0) * 100.0).round() / 100.0)
                };
                let at = Instant::now();
                for &t in &ts {
                    let w = Wanted { hue, sat, at };
                    self.wanted.insert(self.devices[t].id.clone(), w);
                }
                json!({ "colorHue": hue, "colorSaturation": sat })
            }
            Ctl::Blinds => json!({ "blindsTargetLevel": v.round().clamp(0.0, 100.0) as i64 }),
            Ctl::Fan => {
                json!({ "fanMode": FAN_MODES[(v.round() as usize).min(FAN_MODES.len() - 1)] })
            }
        };
        self.apply(&ts, attrs);
    }

    /// Move a control by `steps` fine steps (1% brightness, 1° hue, ...).
    fn nudge(&mut self, it: Item, ctl: Ctl, steps: f64) {
        let src = self.shown(it, ctl);
        let d = &self.devices[src];
        let v = match ctl {
            Ctl::Power => (steps > 0.0) as u8 as f64,
            Ctl::Level => d.n("lightLevel").unwrap_or(0.0) + steps,
            Ctl::Temp => {
                let Some((cur, lo, hi)) = temp_range(d) else {
                    return;
                };
                (cur + steps * ((hi - lo) / 100.0).max(1.0)).clamp(lo, hi)
            }
            Ctl::Hue => self.color_of(src).0 + steps,
            Ctl::Sat => self.color_of(src).1 + steps / 100.0,
            Ctl::Blinds => blinds_level(d) + steps,
            Ctl::Fan => {
                let n = FAN_MODES.len() as i64;
                (fan_index(d) as i64 + steps.signum() as i64).rem_euclid(n) as f64
            }
        };
        self.set_ctl(it, ctl, v, steps > 0.0);
    }

    /// Jump a control to a fraction (0..=1) of its range.
    fn set_frac(&mut self, it: Item, ctl: Ctl, f: f64) {
        let d = &self.devices[self.shown(it, ctl)];
        let v = match ctl {
            Ctl::Power | Ctl::Sat => f,
            Ctl::Level | Ctl::Blinds => f * 100.0,
            Ctl::Temp => {
                let Some((_, lo, hi)) = temp_range(d) else {
                    return;
                };
                lo + f * (hi - lo)
            }
            Ctl::Hue => f * 359.0,
            Ctl::Fan => f * (FAN_MODES.len() - 1) as f64,
        };
        self.set_ctl(it, ctl, v, true);
    }

    /// List-view hotkey: nudge the first of `prefer` that the row supports.
    fn hot(&mut self, it: Item, prefer: &[Ctl], steps: f64) {
        let cs = self.item_ctls(it);
        match prefer.iter().find(|c| cs.contains(c)) {
            Some(&c) => self.nudge(it, c, steps),
            None => self.status("Not supported here", true),
        }
    }

    fn open_panel(&mut self, it: Item) {
        let cs = self.item_ctls(it);
        if cs.is_empty() {
            let msg = if matches!(it, Item::Room(_)) {
                "No lights in this room"
            } else {
                "Nothing to control on this device"
            };
            return self.status(msg, true);
        }
        self.focus = Some(cs.iter().position(|&c| c == Ctl::Level).unwrap_or(0));
    }

    fn on_panel_key(&mut self, code: KeyCode) {
        let Some(it) = self.sel_item() else {
            self.focus = None;
            return;
        };
        let cs = self.item_ctls(it);
        if cs.is_empty() {
            self.focus = None;
            return;
        }
        let f = self.focus.unwrap_or(0).min(cs.len() - 1);
        self.focus = Some(f);
        let ctl = cs[f];
        match code {
            KeyCode::Esc | KeyCode::Enter | KeyCode::Backspace | KeyCode::Char('q') => {
                self.focus = None
            }
            KeyCode::Tab | KeyCode::BackTab | KeyCode::Char('2') => {
                self.focus = None;
                self.tab = Tab::Scenes;
            }
            KeyCode::Down | KeyCode::Char('j') => self.focus = Some((f + 1).min(cs.len() - 1)),
            KeyCode::Up | KeyCode::Char('k') => self.focus = Some(f.saturating_sub(1)),
            KeyCode::Home | KeyCode::Char('g') => self.focus = Some(0),
            KeyCode::End | KeyCode::Char('G') => self.focus = Some(cs.len() - 1),
            KeyCode::Right | KeyCode::Char('l' | '+' | '=') => self.nudge(it, ctl, 10.0),
            KeyCode::Left | KeyCode::Char('h' | '-') => self.nudge(it, ctl, -10.0),
            KeyCode::Char('L') => self.nudge(it, ctl, 1.0),
            KeyCode::Char('H') => self.nudge(it, ctl, -1.0),
            KeyCode::Char(c @ '0'..='9') => {
                let n = c.to_digit(10).unwrap_or(0) as f64;
                self.set_frac(it, ctl, if n == 0.0 { 1.0 } else { n / 10.0 });
            }
            KeyCode::Char(' ') => self.toggle(it),
            KeyCode::Char('i') => self.raw = !self.raw,
            KeyCode::Char('?') => self.help = true,
            KeyCode::Char('r') => {
                let _ = self.tx.send(Cmd::Refresh);
                self.status("Refreshing…", false);
            }
            _ => {}
        }
    }

    fn on_key(&mut self, code: KeyCode) {
        if self.help {
            self.help = false;
            return;
        }
        if self.focus.is_some() && self.tab == Tab::Devices {
            return self.on_panel_key(code);
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
            KeyCode::Char('{') => self.jump_room(-1),
            KeyCode::Char('}') => self.jump_room(1),
            KeyCode::Char('E') => self.toggle_fold_all(),
            KeyCode::Char('.') => self.toggle_remotes(),
            _ => {
                let Some(i) = self.sel_item() else { return };
                match code {
                    KeyCode::Char('e') => self.toggle_expand(i),
                    KeyCode::Enter => self.open_panel(i),
                    KeyCode::Char(' ') => self.toggle(i),
                    KeyCode::Right | KeyCode::Char('l' | '+' | '=') => {
                        self.hot(i, &[Ctl::Level, Ctl::Blinds], 10.0)
                    }
                    KeyCode::Left | KeyCode::Char('h' | '-') => {
                        self.hot(i, &[Ctl::Level, Ctl::Blinds], -10.0)
                    }
                    KeyCode::Char('L') => self.hot(i, &[Ctl::Level, Ctl::Blinds], 1.0),
                    KeyCode::Char('H') => self.hot(i, &[Ctl::Level, Ctl::Blinds], -1.0),
                    KeyCode::Char('[') => self.hot(i, &[Ctl::Temp], -10.0),
                    KeyCode::Char(']') => self.hot(i, &[Ctl::Temp], 10.0),
                    KeyCode::Char('c') => self.hot(i, &[Ctl::Hue], 30.0),
                    KeyCode::Char('C') => self.hot(i, &[Ctl::Hue], -30.0),
                    KeyCode::Char('a') => self.toggle(Item::Room(self.room_of(i))),
                    KeyCode::Char('m') => self.toggle_lamp(i),
                    _ => {}
                }
            }
        }
    }

    fn move_sel(&mut self, delta: i64) {
        let (len, sel) = match self.tab {
            Tab::Devices => (self.items.len(), self.dev_state.selected()),
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
        "motionSensor" | "occupancySensor" => match d.b("isDetected") {
            Some(true) => ("◆", Style::new().fg(Color::Magenta), "motion".into()),
            _ => ("◇", dim, "idle".into()),
        },
        "openCloseSensor" => match d.b("isOpen") {
            Some(true) => ("◆", Style::new().fg(Color::Magenta), "open".into()),
            _ => ("◇", dim, "closed".into()),
        },
        "electricalSensor" => match d.n("currentActivePower") {
            Some(w) => ("ϟ", Style::new().fg(Color::Yellow), format!("{w:.0} W")),
            None => ("ϟ", dim, String::new()),
        },
        "lightSensor" => match d.n("illuminance") {
            Some(lx) => ("◇", Style::new().fg(Color::Green), format!("{lx:.0} lx")),
            None => ("◇", dim, String::new()),
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

fn label(s: &str) -> Span<'static> {
    Span::styled(format!("{s:<12}"), Style::new().fg(Color::DarkGray))
}

fn raw_lines(d: &Device) -> Vec<Line<'static>> {
    let txt = serde_json::to_string_pretty(&d.attributes).unwrap_or_default();
    txt.lines().map(|l| Line::raw(l.to_string())).collect()
}

/// A `w`-cell gradient (colored by `color(0..=1)`) with a marker at `frac`.
fn gradient(w: usize, frac: f64, color: impl Fn(f64) -> Color) -> Vec<Span<'static>> {
    let last = w.saturating_sub(1).max(1) as f64;
    let pos = (frac.clamp(0.0, 1.0) * last).round() as usize;
    (0..w)
        .map(|x| {
            let c = color(x as f64 / last);
            if x == pos {
                Span::styled("┃", Style::new().fg(Color::Black).bg(c).bold())
            } else {
                Span::styled(" ", Style::new().bg(c))
            }
        })
        .collect()
}

/// Width of the slider part of a control row in a pane `inner` columns wide.
fn slider_width(inner: usize) -> usize {
    // marker (2) + label (11) + value (6)
    inner.saturating_sub(19).max(4)
}

/// One control row: marker, label, slider and value.
fn ctl_line(d: &Device, ctl: Ctl, focused: bool, w: usize) -> Line<'static> {
    let yellow = Style::new().fg(Color::Yellow);
    let dim = Style::new().fg(Color::DarkGray);
    let mut spans = vec![
        Span::styled(if focused { "▶ " } else { "  " }, yellow),
        Span::styled(
            format!("{:<11}", ctl.label()),
            if focused { yellow.bold() } else { dim },
        ),
    ];
    // Bulbs are either in color or in temperature mode; dim the other one.
    let mode = d.s("colorMode");
    let inactive = match ctl {
        Ctl::Temp => mode == Some("color"),
        Ctl::Hue | Ctl::Sat => mode == Some("temperature"),
        _ => false,
    };
    let on = d.b("isOn") != Some(false);
    let (slider, val) = match ctl {
        Ctl::Power => {
            spans.push(if on {
                Span::styled("● on", yellow)
            } else {
                Span::styled("○ off", dim)
            });
            return Line::from(spans);
        }
        Ctl::Fan => {
            spans.push(Span::raw(format!("‹ {} ›", FAN_MODES[fan_index(d)])));
            return Line::from(spans);
        }
        Ctl::Level => {
            let l = d.n("lightLevel").unwrap_or(0.0);
            let c = if on { Color::Yellow } else { Color::Gray };
            (bar(l / 100.0, w, c), format!("{l:.0}%"))
        }
        Ctl::Temp => {
            let (k, lo, hi) = temp_range(d).unwrap_or((0.0, 0.0, 1.0));
            let g = gradient(w, (k - lo) / (hi - lo), |t| {
                kelvin_color(lo + t * (hi - lo))
            });
            (g, format!("{k:.0}K"))
        }
        Ctl::Hue => {
            let h = d.n("colorHue").unwrap_or(0.0);
            (
                gradient(w, h / 360.0, |t| hsv(t * 360.0, 1.0, 1.0)),
                format!("{h:.0}°"),
            )
        }
        Ctl::Sat => {
            let h = d.n("colorHue").unwrap_or(0.0);
            let s = d.n("colorSaturation").unwrap_or(0.0);
            (
                gradient(w, s, |t| hsv(h, t, 1.0)),
                format!("{:.0}%", s * 100.0),
            )
        }
        Ctl::Blinds => {
            let l = blinds_level(d);
            (bar(l / 100.0, w, Color::Cyan), format!("{l:.0}%"))
        }
    };
    spans.extend(slider);
    spans.push(Span::styled(
        format!(" {val:>5}"),
        if inactive { dim } else { Style::new() },
    ));
    Line::from(spans)
}

/// The control rows for a device; `focus` marks the selected one.
/// The control rows for a list row; `focus` marks the selected one. For groups,
/// each row shows the value of the member that `App::shown` picks.
fn controls(app: &App, it: Item, focus: Option<usize>, w: usize) -> Vec<Line<'static>> {
    let cs = app.item_ctls(it);
    let f = focus.map(|f| f.min(cs.len().saturating_sub(1)));
    cs.iter()
        .enumerate()
        .map(|(j, &c)| ctl_line(&app.devices[app.shown(it, c)], c, f == Some(j), w))
        .collect()
}

/// `ctl_lines`, then a blank line if there were any.
fn with_controls(ctl_lines: Vec<Line<'static>>) -> Vec<Line<'static>> {
    let mut lines = ctl_lines;
    if !lines.is_empty() {
        lines.push(Line::raw(""));
    }
    lines
}

/// One compact line per device: icon, name, short value.
fn member_line(d: &Device) -> Line<'static> {
    let (icon, istyle, val) = summary(d);
    Line::from(vec![
        Span::raw("  "),
        Span::styled(icon, istyle),
        Span::raw(format!(" {} ", d.name())),
        Span::styled(val, Style::new().fg(Color::DarkGray)),
    ])
}

fn details(d: &Device, raw: bool, ctl_lines: Vec<Line<'static>>) -> Vec<Line<'static>> {
    if raw {
        return raw_lines(d);
    }
    let mut lines = with_controls(ctl_lines);
    let cs = ctls(d);
    let mut kv = |k: &str, v: String| lines.push(Line::from(vec![label(k), Span::raw(v)]));
    kv("Type", d.device_type.clone());
    kv("Room", d.room_name().trim().to_string());
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
    if let (Some(m), true) = (
        d.s("colorMode"),
        cs.contains(&Ctl::Temp) && cs.contains(&Ctl::Hue),
    ) {
        kv("Mode", m.to_string());
    }
    if let Some(b) = d.n("batteryPercentage") {
        kv("Battery", format!("{b:.0}%"));
    }
    if let (Some(on), false) = (d.b("isOn"), cs.contains(&Ctl::Power)) {
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
    if let Some(w) = d.n("currentActivePower") {
        kv("Power use", format!("{w:.1} W"));
    }
    if let Some(v) = d.n("currentVoltage") {
        kv("Voltage", format!("{v:.0} V"));
    }
    if let Some(e) = d.n("energyConsumedAtLastReset") {
        kv("Energy", format!("{e:.2} kWh"));
    }
    if let Some(lx) = d.n("illuminance") {
        kv("Light", format!("{lx:.0} lx"));
    }
    lines
}

/// Details for a device set: its controls, then its members.
fn set_details(
    s: &DevSet,
    devices: &[Device],
    rep: usize,
    raw: bool,
    ctl_lines: Vec<Line<'static>>,
) -> Vec<Line<'static>> {
    let r = &devices[rep];
    if raw {
        let dim = Style::new().fg(Color::DarkGray);
        let mut lines = vec![Line::styled(format!("// member: {}", r.name()), dim)];
        lines.extend(raw_lines(r));
        return lines;
    }
    let mut lines = with_controls(ctl_lines);
    lines.extend([
        Line::from(vec![
            label("Type"),
            Span::raw(format!("{} set", r.device_type)),
        ]),
        Line::from(vec![
            label("Room"),
            Span::raw(r.room_name().trim().to_string()),
        ]),
        Line::from(vec![
            label("Members"),
            Span::raw(s.members.len().to_string()),
        ]),
    ]);
    lines.extend(s.members.iter().map(|&j| member_line(&devices[j])));
    lines
}

/// Details for a room: controls for all its lights, then the lights.
fn room_details(app: &App, r: usize, ctl_lines: Vec<Line<'static>>) -> Vec<Line<'static>> {
    let lights = app.targets(Item::Room(r)).1;
    let mut lines = with_controls(ctl_lines);
    if lights.is_empty() {
        lines.push(Line::styled(
            "No lights in this room",
            Style::new().fg(Color::DarkGray),
        ));
        return lines;
    }
    let on = lights
        .iter()
        .filter(|&&j| app.devices[j].b("isOn") == Some(true))
        .count();
    lines.push(Line::from(vec![
        label("Lights"),
        Span::raw(format!("{on} of {} on", lights.len())),
    ]));
    lines.extend(lights.iter().map(|&j| member_line(&app.devices[j])));
    lines
}

/// (name cell, value text, dimmed) for one row of the device list.
fn list_row(app: &App, it: Item) -> (Line<'static>, Line<'static>, bool) {
    let dim = Style::new().fg(Color::DarkGray);
    let on = |j: &usize| app.devices[*j].b("isOn") == Some(true);
    match it {
        Item::Room(r) => {
            let room = &app.rooms[r];
            let arrow = if app.collapsed.contains(&room.key) {
                "▸ "
            } else {
                "▾ "
            };
            let lights = app.targets(it).1;
            let n_on = lights.iter().filter(|j| on(j)).count();
            let val = if lights.is_empty() {
                Line::styled(format!("{} dev", room.members.len()), dim)
            } else if n_on == 0 {
                Line::styled(format!("0/{} on", lights.len()), dim)
            } else {
                Line::styled(
                    format!("{n_on}/{} on", lights.len()),
                    Style::new().fg(Color::Yellow),
                )
            };
            let name = Line::from(vec![
                Span::styled(arrow, Style::new().fg(Color::Cyan)),
                Span::styled(room.name.clone(), Style::new().fg(Color::Cyan).bold()),
            ]);
            (name, val.right_aligned(), false)
        }
        Item::Set(k) => {
            let s = &app.sets[k];
            let n = s.members.len();
            let n_on = s.members.iter().filter(|j| on(j)).count();
            // Show an "on" member if there is one, so the icon reflects it.
            let shown = s
                .members
                .iter()
                .copied()
                .find(|j| on(j))
                .unwrap_or(app.targets(it).0);
            let (icon, istyle, sval) = summary(&app.devices[shown]);
            let arrow = if app.expanded.contains(&s.id) {
                "▾"
            } else {
                "▸"
            };
            let val = if n_on > 0 && n_on < n {
                format!("{n_on}/{n} on")
            } else {
                sval
            };
            let name = Line::from(vec![
                Span::raw("  "),
                Span::styled(icon, istyle),
                Span::raw(format!(" {arrow} {} ({n})", s.name)),
            ]);
            let reachable = s.members.iter().any(|&j| app.devices[j].is_reachable);
            (name, Line::raw(val).right_aligned(), !reachable)
        }
        Item::Dev(i) => {
            let d = &app.devices[i];
            // Expanded set members get a tree prefix.
            let prefix = match d.set().and_then(|s| app.sets.iter().find(|x| x.id == s.id)) {
                Some(s) if s.members.last() == Some(&i) => " └ ",
                Some(_) => " ├ ",
                None => "",
            };
            let (icon, istyle, val) = summary(d);
            let name = Line::from(vec![
                Span::raw("  "),
                Span::styled(icon, istyle),
                Span::raw(format!(" {prefix}{}", d.name())),
            ]);
            let dimmed = !d.is_reachable || is_remote(d);
            (name, Line::raw(val).right_aligned(), dimmed)
        }
    }
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
    ("Space", "toggle / trigger scene"),
    ("Enter", "open controls / trigger scene"),
    ("h/l H/L", "brightness or blinds ±10 / ±1"),
    ("[ ]", "warmer / cooler"),
    ("c C", "cycle hue"),
    ("a", "toggle all lights in room"),
    ("m", "mark outlet as a lamp"),
    ("{ }", "previous / next room"),
    ("e E", "fold room or set / all rooms"),
    (".", "hide / show remotes"),
    ("i", "show raw attributes"),
    ("r", "refresh"),
    ("q Esc", "quit"),
    ("In controls:", ""),
    ("j/k", "choose a control"),
    ("h/l H/L", "adjust ±10 / ±1"),
    ("1-9 0", "set to 10%-90% / 100%"),
    ("Esc q Enter", "back to list"),
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
        // The control panel gets more room while it has focus.
        let focused = app.focus.is_some();
        let split = if focused { 50 } else { 58 };
        let [list_area, detail_area] = Layout::horizontal([
            Constraint::Percentage(split),
            Constraint::Percentage(100 - split),
        ])
        .areas(body);
        let w = slider_width(detail_area.width.saturating_sub(2) as usize);
        let rows = app.items.iter().map(|&it| {
            let (name, val, dimmed) = list_row(app, it);
            let row = Row::new(vec![Cell::from(name), Cell::from(val)]);
            if dimmed {
                row.style(Style::new().fg(Color::DarkGray))
            } else {
                row
            }
        });
        let title = if app.hide_remotes {
            format!(" Devices ({}, remotes hidden) ", app.devices.len())
        } else {
            format!(" Devices ({}) ", app.devices.len())
        };
        let table = Table::new(rows, [Constraint::Fill(1), Constraint::Length(12)])
            .block(Block::bordered().title(title).border_style(if focused {
                Style::new().fg(Color::DarkGray)
            } else {
                Style::new()
            }))
            .row_highlight_style(Style::new().bg(Color::Rgb(50, 50, 60)).bold())
            .highlight_symbol("▶ ");
        f.render_stateful_widget(table, list_area, &mut app.dev_state);

        let pane = app.sel_item().map(|it| {
            let ctl_lines = controls(app, it, app.focus, w);
            match it {
                Item::Dev(i) => {
                    let d = &app.devices[i];
                    let mut lines = details(d, app.raw, ctl_lines);
                    if d.device_type == "outlet" && !app.raw {
                        let lamp = app.lamps.contains(&d.id);
                        lines.push(Line::from(vec![
                            label("Lamp"),
                            Span::raw(if lamp {
                                "yes (m to unmark)"
                            } else {
                                "no (m to mark)"
                            }),
                        ]));
                    }
                    (d.name(), lines)
                }
                Item::Set(k) => {
                    let s = &app.sets[k];
                    let rep = app.targets(it).0;
                    let lines = set_details(s, &app.devices, rep, app.raw, ctl_lines);
                    (s.name.clone(), lines)
                }
                Item::Room(r) => (app.rooms[r].name.clone(), room_details(app, r, ctl_lines)),
            }
        });
        if let Some((name, lines)) = pane {
            let title = format!(" {name} {}", if app.raw { "(raw) " } else { "" });
            f.render_widget(
                Paragraph::new(lines)
                    .block(Block::bordered().title(title).border_style(if focused {
                        Style::new().fg(Color::Yellow)
                    } else {
                        Style::new()
                    }))
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
        _ if app.focus.is_some() && app.tab == Tab::Devices => Line::styled(
            " j/k choose · h/l adjust · H/L fine · 1-9 0 set · space power · esc back",
            Style::new().fg(Color::DarkGray),
        ),
        _ => Line::styled(
            " enter controls · space toggle · h/l level · e fold · { } room · ? help · q quit",
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
        sets: vec![],
        rooms: vec![],
        items: vec![],
        collapsed: HashSet::new(),
        hide_remotes: false,
        lamps: HashSet::new(),
        state_path: dirs::config_dir().map(|p| p.join("dirigera-tui").join("state")),
        expanded: HashSet::new(),
        scenes: vec![],
        dev_state: TableState::default(),
        scene_state: ListState::default(),
        status: None,
        tx: cmd_tx,
        seq: 0,
        loaded: false,
        raw: false,
        focus: None,
        wanted: HashMap::new(),
        help: false,
        quit: false,
    };
    app.load_state();

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
