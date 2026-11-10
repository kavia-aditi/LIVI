// SPDX-FileCopyrightText: 2025-2026 Lasse Heitgres
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional terms: see NOTICE

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

use livi_core_client::Client;
use livi_core_proto::message::{Action, MfiFile, Radio as CoreRadio, ToCore};
use livi_ui_model::i18n::Strings;
use livi_ui_model::settings::calibration::{Pattern, set as calibrate, value_at};
use livi_ui_model::settings::color::{Hsl, from_hsl, hex, rgb, to_hsl};
use livi_ui_model::settings::custom::files::Wanted;
use livi_ui_model::settings::custom::icon::icons as icon_patch;
use livi_ui_model::settings::custom::{Watch, set_binding};
use livi_ui_model::settings::node::{CustomPage, Radio};
use livi_ui_model::settings::restart::{pending, sessions_open};
use livi_ui_model::settings::schema;
use livi_ui_model::settings::view::{
    Change, Dialog, Facts, Local, Presence, Row, RowKind as ModelRowKind, Screen, Settings,
    number_text, typed_number,
};
use serde_json::Value;
use slint::{Color, ComponentHandle, Image, Model, ModelRc, SharedString, VecModel, Weak};

use crate::files_ui::{Browser, Picked};
use crate::icon_ui::import;
use crate::icons::icon;
use crate::{
    AppWindow, KnobData, Page, PickData, RowData, RowKind, ScreenKind, SettingsState, ToolData,
};

mod capture;
mod dialog;
mod render;
mod progress;

pub use capture::*;
pub use dialog::*;
use render::*;

const SLIDE_EVERY: Duration = Duration::from_millis(100);

struct Controller {
    model: Settings,
    stack: Vec<usize>,
    strings: Strings,
    baseline: Option<Value>,
    hsl: Option<Hsl>,
    last_slide: Option<Instant>,
    rows: Vec<Row>,
    dialog: Option<Dialog>,
    dismissed: Option<String>,
    watching: Vec<Watch>,
    capturing: Option<&'static str>,
    browser: Option<Browser>,
    selected_paired: Option<String>,
    paired_inventory: Option<(Instant, Value)>,
    evidence: progress::Evidence,
    #[cfg(all(target_os = "macos", feature = "bluetooth-app-test"))]
    diagnostic: crate::bluetooth_app::Devices<crate::bluetooth_app::Native>,
}

thread_local! {
    static CONTROLLER: RefCell<Option<Controller>> = const { RefCell::new(None) };
    static LOGOS: RefCell<HashMap<String, Option<Image>>> = RefCell::new(HashMap::new());
}

fn with<R>(f: impl FnOnce(&mut Controller) -> R) -> Option<R> {
    CONTROLLER.with_borrow_mut(|c| c.as_mut().map(f))
}

fn refill<T: Clone + PartialEq + 'static>(now: &ModelRc<T>, next: Vec<T>) -> Option<ModelRc<T>> {
    let same = now
        .as_any()
        .downcast_ref::<VecModel<T>>()
        .filter(|m| m.row_count() == next.len());
    let Some(model) = same else {
        return Some(ModelRc::from(Rc::new(VecModel::from(next))));
    };
    for (i, item) in next.into_iter().enumerate() {
        if model.row_data(i).as_ref() != Some(&item) {
            model.set_row_data(i, item);
        }
    }
    None
}

pub fn depth() -> usize {
    with(|c| c.stack.len() + c.browser.as_ref().map_or(0, Browser::depth)).unwrap_or(0)
}

pub fn tr(key: &str, text: &str) -> String {
    with(|c| c.strings.t(key, text)).unwrap_or_else(|| text.to_string())
}

pub fn set_language(lang: &str) {
    with(|c| {
        if c.strings.lang() != lang {
            c.strings = Strings::new(lang);
        }
    });
}

pub fn open_path(ui: &AppWindow, client: &Client, path: &str) {
    let keys: Vec<&str> = path.split('/').filter(|k| !k.is_empty()).collect();
    with(|c| {
        if let Some(stack) = c.model.find(&keys) {
            c.stack = stack;
        }
    });
    render(ui, client);
}

pub fn reset_to_root(ui: &AppWindow, client: &Client) {
    with(|c| c.stack.clear());
    render(ui, client);
}

fn picked(wanted: Wanted, path: &std::path::Path) -> Vec<Change> {
    let mfi = |file: MfiFile| {
        vec![Change::Act(Action::ImportMfi {
            file,
            path: path.to_string_lossy().into_owned(),
        })]
    };
    match wanted {
        Wanted::Picture => match import(path) {
            Ok(pngs) => vec![Change::Config(icon_patch(pngs))],
            Err(e) => {
                println!("[ui] icon import {} failed: {e}", path.display());
                Vec::new()
            }
        },
        Wanted::Certificate => mfi(MfiFile::Certificate),
        Wanted::Identity => mfi(MfiFile::Identity),
    }
}

fn send(client: &Client, changes: Vec<Change>) {
    for change in changes {
        match change {
            Change::Config(patch) => client.act(Action::SetConfig { patch }),
            Change::DongleOn(radio) => client.act(Action::SetDongleRadio {
                radio: match radio {
                    Radio::Wifi => CoreRadio::Wifi,
                    Radio::Bt => CoreRadio::Bt,
                },
                on: true,
            }),
            Change::Act(action) => client.act(action),
        }
    }
}

fn act(
    weak: &Weak<AppWindow>,
    client: &Client,
    f: impl FnOnce(&mut Controller, &Value) -> (Vec<Change>, bool),
) {
    let Some(ui) = weak.upgrade() else { return };
    let state = client.state().clone();
    let Some((changes, _)) = with(|c| {
        let out = f(c, &state);
        if out.1 {
            c.stack.pop();
            c.hsl = None;
        }
        out
    }) else {
        return;
    };
    send(client, changes);
    render(&ui, client);
}

fn throttled(c: &mut Controller) -> bool {
    let now = Instant::now();
    if c.last_slide.is_some_and(|t| now - t < SLIDE_EVERY) {
        return false;
    }
    c.last_slide = Some(now);
    true
}

pub fn wire(ui: &AppWindow, client: &Client, lang: &str) {
    CONTROLLER.with_borrow_mut(|c| {
        *c = Some(Controller {
            model: Settings::new(schema()).with_facts(facts()),
            stack: Vec::new(),
            strings: Strings::new(lang),
            baseline: None,
            hsl: None,
            last_slide: None,
            rows: Vec::new(),
            dialog: None,
            dismissed: None,
            watching: Vec::new(),
            capturing: None,
            browser: None,
            selected_paired: None,
            paired_inventory: None,
            evidence: progress::Evidence::default(),
            #[cfg(all(target_os = "macos", feature = "bluetooth-app-test"))]
            diagnostic: Default::default(),
        });
    });
    let s = ui.global::<SettingsState>();
    let weak = ui.as_weak();

    s.on_back({
        let (weak, client) = (weak.clone(), client.clone());
        move || {
            act(&weak, &client, |c, _| match c.browser.as_mut() {
                Some(b) => {
                    if !b.up() {
                        c.browser = None;
                    }
                    (Vec::new(), false)
                }
                None => (Vec::new(), true),
            })
        }
    });
    s.on_press({
        let (weak, client) = (weak.clone(), client.clone());
        move |i| {
            let row = with(|c| c.rows.get(i as usize).filter(|r| r.enabled).cloned()).flatten();
            let Some(row) = row else { return };
            if let Some(a) = row.action {
                client.act(a);
            }
            if let Some(change) = row.change {
                act(&weak, &client, |_, _| (vec![change], false));
            }
            match row.local {
                Some(Local::RefreshPaired) => {
                    act(&weak, &client, |c, _| {
                        #[cfg(all(target_os = "macos", feature = "bluetooth-app-test"))]
                        c.diagnostic.revoke();
                        c.selected_paired = None;
                        c.paired_inventory = None;
                        (Vec::new(), false)
                    });
                }
                Some(Local::SelectPaired(index)) => {
                    act(&weak, &client, |c, state| {
                        #[cfg(all(target_os = "macos", feature = "bluetooth-app-test"))]
                        c.diagnostic.revoke();
                        let inventory = c.paired_inventory.as_ref()
                            .map(|(_, value)| value)
                            .unwrap_or(&state["macosPairedDevices"]);
                        c.selected_paired = inventory["devices"].get(index)
                            .and_then(|d| d["address"].as_str()).map(str::to_owned);
                        (Vec::new(), false)
                    });
                }
                #[cfg(all(target_os = "macos", feature = "bluetooth-app-test"))]
                Some(local @ (Local::BluetoothConsent | Local::BluetoothConnect | Local::BluetoothCancel)) => {
                    act(&weak, &client, |c, _| {
                        c.diagnostic.press(local, c.selected_paired.as_deref(),
                            crate::bluetooth_app::devices_allowed());
                        (Vec::new(), false)
                    });
                }
                Some(Local::Browse(wanted)) => {
                    act(&weak, &client, |c, _| {
                        c.browser = Some(Browser::open(wanted));
                        (Vec::new(), false)
                    });
                }
                Some(Local::PickEntry(n)) => {
                    act(&weak, &client, |c, _| {
                        let Some(Picked::File(wanted, path)) =
                            c.browser.as_mut().and_then(|b| b.pick(n))
                        else {
                            return (Vec::new(), false);
                        };
                        let changes = picked(wanted, &path);
                        if !changes.is_empty() {
                            c.browser = None;
                        }
                        (changes, false)
                    });
                }
                _ => {}
            }
            if row.local == Some(Local::ToProjection)
                && let Some(ui) = weak.upgrade()
            {
                ui.set_page(Page::Projection);
                ui.invoke_page_chosen(Page::Projection);
            }
        }
    });
    s.on_capture({
        let weak = weak.clone();
        move |i| {
            let Some(ui) = weak.upgrade() else { return };
            let s = ui.global::<SettingsState>();
            with(|c| {
                let Some(row) = c.rows.get(i as usize) else {
                    return;
                };
                let Some(binding) = row.binding else { return };
                c.capturing = Some(binding);
                s.set_capture_title(
                    c.strings
                        .with(
                            "settings.pressKeyFor",
                            "Press a key for “{{name}}”",
                            &[("name", &row.label)],
                        )
                        .into(),
                );
                s.set_capture_hint(
                    c.strings
                        .t("settings.pressEscToCancel", "Press Esc to cancel.")
                        .into(),
                );
                s.set_capture_open(true);
            });
        }
    });
    s.on_capture_cancel({
        let weak = weak.clone();
        move || {
            if let Some(ui) = weak.upgrade() {
                stop_capture(&ui);
            }
        }
    });
    s.on_tool({
        let client = client.clone();
        move |i, j| {
            let action = with(|c| {
                c.rows
                    .get(i as usize)
                    .and_then(|r| r.tools.get(j as usize))
                    .filter(|t| t.enabled)
                    .map(|t| t.action.clone())
            })
            .flatten();
            if let Some(a) = action {
                client.act(a);
            }
        }
    });
    s.on_dialog_press({
        let (weak, client) = (weak.clone(), client.clone());
        move |i| {
            act(&weak, &client, |c, _| {
                let Some(row) = c.dialog.as_ref().and_then(|d| d.rows.get(i as usize)) else {
                    return (Vec::new(), false);
                };
                if !row.enabled {
                    return (Vec::new(), false);
                }
                if row.local == Some(Local::CloseDialog) {
                    c.dismissed = c.dialog.as_ref().map(|d| d.key.clone());
                    return (Vec::new(), false);
                }
                (
                    row.action.clone().map(Change::Act).into_iter().collect(),
                    false,
                )
            })
        }
    });
    s.on_apply({
        let (weak, client) = (weak.clone(), client.clone());
        move || {
            client.act(Action::ApplySettings);
            let state = client.state().clone();
            with(|c| c.baseline = Some(state["config"].clone()));
            if let Some(ui) = weak.upgrade() {
                render(&ui, &client);
            }
        }
    });
    s.on_open({
        let (weak, client) = (weak.clone(), client.clone());
        move |i| {
            act(&weak, &client, |c, _| {
                let i = i as usize;
                if c.model.opens(&c.stack, i) {
                    c.stack.push(i);
                    c.hsl = None;
                }
                (Vec::new(), false)
            })
        }
    });
    s.on_toggle({
        let (weak, client) = (weak.clone(), client.clone());
        move |i| {
            act(&weak, &client, |c, state| {
                let Some(row) = c.rows.get(i as usize).cloned() else {
                    return (Vec::new(), false);
                };
                if let Some(change) = row.change {
                    let mut out = vec![change];
                    out.extend(row.action.map(Change::Act));
                    return (out, false);
                }
                (
                    c.model
                        .toggle(&c.stack, row.node, state)
                        .into_iter()
                        .collect(),
                    false,
                )
            })
        }
    });
    s.on_slide({
        let client = client.clone();
        move |i, v| {
            let change = with(|c| {
                throttled(c)
                    .then(|| c.model.level(&c.stack, Some(i as usize), v))
                    .flatten()
            })
            .flatten();
            send(&client, change.into_iter().collect());
        }
    });
    s.on_slide_done({
        let (weak, client) = (weak.clone(), client.clone());
        move |i, v| {
            act(&weak, &client, |c, _| {
                c.last_slide = None;
                (
                    c.model
                        .level(&c.stack, Some(i as usize), v)
                        .into_iter()
                        .collect(),
                    false,
                )
            })
        }
    });
    s.on_level_moved({
        let client = client.clone();
        move |v| {
            let change = with(|c| {
                throttled(c)
                    .then(|| c.model.level(&c.stack, None, v))
                    .flatten()
            })
            .flatten();
            send(&client, change.into_iter().collect());
        }
    });
    s.on_level_done({
        let (weak, client) = (weak.clone(), client.clone());
        move |v| {
            act(&weak, &client, |c, _| {
                c.last_slide = None;
                (
                    c.model.level(&c.stack, None, v).into_iter().collect(),
                    false,
                )
            })
        }
    });
    s.on_cal_moved({
        let client = client.clone();
        move |i, v| {
            let change = with(|c| {
                throttled(c)
                    .then(|| c.model.calibration_key(&c.stack, i as usize))
                    .flatten()
                    .map(|key| calibrate(key, value_at(v)))
            })
            .flatten();
            send(&client, change.into_iter().collect());
        }
    });
    s.on_cal_done({
        let (weak, client) = (weak.clone(), client.clone());
        move |i, v| {
            act(&weak, &client, |c, _| {
                c.last_slide = None;
                let key = c.model.calibration_key(&c.stack, i as usize);
                (
                    key.map(|k| calibrate(k, value_at(v))).into_iter().collect(),
                    false,
                )
            })
        }
    });
    s.on_cal_reset({
        let (weak, client) = (weak.clone(), client.clone());
        move |i| {
            act(&weak, &client, |c, _| {
                let key = c.model.calibration_key(&c.stack, i as usize);
                (key.map(|k| calibrate(k, 1.0)).into_iter().collect(), false)
            })
        }
    });
    s.on_pick({
        let (weak, client) = (weak.clone(), client.clone());
        move |i| {
            act(&weak, &client, |c, state| {
                let changes = c.model.pick(&c.stack, i as usize, state, &c.strings);
                let done = !changes.is_empty();
                (changes, done)
            })
        }
    });
    s.on_number_set({
        let (weak, client) = (weak.clone(), client.clone());
        move |v| {
            act(&weak, &client, |c, state| {
                (
                    c.model
                        .number(&c.stack, f64::from(v), state)
                        .into_iter()
                        .collect(),
                    false,
                )
            })
        }
    });
    s.on_number_commit({
        let (weak, client) = (weak.clone(), client.clone());
        move |text: SharedString| {
            if let Some(v) = typed_number(&text) {
                act(&weak, &client, |c, state| {
                    (
                        c.model.number(&c.stack, v, state).into_iter().collect(),
                        false,
                    )
                });
            }
            if let Some(ui) = weak.upgrade() {
                crate::keys_ui::text_done(&ui);
            }
        }
    });
    s.on_text_commit({
        let (weak, client) = (weak.clone(), client.clone());
        move |text: SharedString| {
            act(&weak, &client, |c, _| {
                let change = c.model.text(&c.stack, &text);
                let done = change.is_some();
                (change.into_iter().collect(), done)
            });
            if let Some(ui) = weak.upgrade() {
                crate::keys_ui::text_done(&ui);
            }
        }
    });
    s.on_color_hex_commit({
        let (weak, client) = (weak.clone(), client.clone());
        move |text: SharedString| {
            act(&weak, &client, |c, _| {
                c.hsl = None;
                (c.model.text(&c.stack, &text).into_iter().collect(), false)
            });
            if let Some(ui) = weak.upgrade() {
                crate::keys_ui::text_done(&ui);
            }
        }
    });
    s.on_color_slide({
        let weak = weak.clone();
        move |which, v| {
            let Some(ui) = weak.upgrade() else { return };
            let state = ui.global::<SettingsState>();
            let custom = state.get_color_custom();
            with(|c| {
                let mut hsl = c.hsl.unwrap_or(Hsl {
                    h: 0.0,
                    s: 0.0,
                    l: 0.0,
                });
                match which {
                    0 => hsl.h = f64::from(v).round(),
                    1 => hsl.s = f64::from(v).round(),
                    _ => hsl.l = f64::from(v).round(),
                }
                c.hsl = Some(hsl);
                show_color(&state, hsl, custom);
            });
        }
    });
    s.on_color_slide_done({
        let (weak, client) = (weak.clone(), client.clone());
        move |which, v| {
            act(&weak, &client, |c, _| {
                let mut hsl = c.hsl.unwrap_or(Hsl {
                    h: 0.0,
                    s: 0.0,
                    l: 0.0,
                });
                match which {
                    0 => hsl.h = f64::from(v).round(),
                    1 => hsl.s = f64::from(v).round(),
                    _ => hsl.l = f64::from(v).round(),
                }
                c.hsl = Some(hsl);
                (
                    c.model
                        .text(&c.stack, &hex(from_hsl(hsl)))
                        .into_iter()
                        .collect(),
                    false,
                )
            })
        }
    });
    s.on_color_reset({
        let (weak, client) = (weak.clone(), client.clone());
        move || {
            act(&weak, &client, |c, _| {
                c.hsl = None;
                (c.model.color_reset(&c.stack).into_iter().collect(), false)
            })
        }
    });
}

fn facts() -> Facts {
    let years = match option_env!("LIVI_BUILD_DATE").and_then(|d| d.get(..4)) {
        Some(y) if y > "2025" => format!("2025–{y}"),
        _ => "2025".into(),
    };
    Facts {
        name: env!("CARGO_PKG_DESCRIPTION").into(),
        homepage: env!("CARGO_PKG_HOMEPAGE").into(),
        source: "https://github.com/f-io/LIVI".into(),
        copyright: format!("© {years} Lasse Heitgres"),
        license: env!("CARGO_PKG_LICENSE").into(),
        version: env!("CARGO_PKG_VERSION").into(),
        sha: option_env!("LIVI_BUILD_SHA").unwrap_or("dev").into(),
        run: option_env!("LIVI_BUILD_RUN").unwrap_or_default().into(),
    }
}

fn set_watch(client: &Client, w: &Watch, on: bool) {
    match w {
        Watch::LinkSpeed => client.send(&ToCore::LinkSpeed { on }),
        Watch::Act(a) if on => client.act(a.clone()),
        Watch::Act(_) => {}
    }
}

fn follow_watches(client: &Client, now: Vec<Watch>) {
    let before = with(|c| std::mem::replace(&mut c.watching, now.clone())).unwrap_or_default();
    for w in before.iter().filter(|w| !now.contains(w)) {
        set_watch(client, w, false);
    }
    for w in now.iter().filter(|w| !before.contains(w)) {
        set_watch(client, w, true);
    }
}

fn devices_page(c: &Controller) -> bool {
    c.browser.is_none() && c.model.node(&c.stack).is_some_and(|node|
        node.children().iter().any(|child|
            matches!(child.kind, livi_ui_model::settings::node::Kind::BtDeviceList)))
}

pub fn render(ui: &AppWindow, client: &Client) {
    let mut state = client.state().clone();
    with(|c| {
        c.evidence.observe(&state);
        c.evidence.supplement(&mut state);
        state["uiSelectedPairedAddress"] = c.selected_paired.clone().into();
        #[cfg(all(target_os = "macos", feature = "bluetooth-app-test"))]
        if !devices_page(c) { c.diagnostic.revoke(); }
    });
    #[cfg(target_os = "macos")]
    let state = {
        let mut state = state;
        let devices_page = with(|c| devices_page(c)).unwrap_or(false);
        if devices_page && std::env::var_os("LIVI_UI_STATE").is_none() {
            with(|c| {
                if c.paired_inventory.as_ref().is_none_or(|(at, _)|
                    at.elapsed() >= Duration::from_secs(30))
                {
                    let loading = serde_json::json!({"loading":true});
                    c.paired_inventory = Some((Instant::now(), loading.clone()));
                    state["macosPairedDevices"] = loading;
                    let weak = ui.as_weak();
                    let client = client.clone();
                    // OS inventory requires the main thread. Run once after
                    // painting/loading, not on every core progress update.
                    slint::Timer::single_shot(Duration::from_millis(16), move || {
                        let mut snapshot = serde_json::json!({});
                        macos_inventory_for_view(&mut snapshot, true, false,
                            livi_native_bluetooth::paired::list);
                        with(|c| c.paired_inventory = Some((Instant::now(),
                            snapshot["macosPairedDevices"].clone())));
                        if let Some(ui) = weak.upgrade() {
                            if ui.get_page() == Page::Settings {
                                render(&ui, &client);
                            }
                        }
                    });
                } else if let Some((_, inventory)) = &c.paired_inventory {
                    state["macosPairedDevices"] = inventory.clone();
                }
            });
        }
        state
    };
    with(|c| render_screen(c, &ui.global::<SettingsState>(), &state));
    let now = with(|c| c.model.watches(&c.stack)).unwrap_or_default();
    follow_watches(client, now);
}

#[cfg(target_os = "macos")]
fn macos_inventory_for_view(
    state: &mut Value,
    devices_page: bool,
    fixture: bool,
    read: impl FnOnce() -> Result<Vec<livi_native_bluetooth::paired::PairedDevice>,
        livi_native_bluetooth::paired::InventoryError>,
) {
    // Fixture rendering remains strictly offline. This is a local view
    // supplement, never a core registry entry or an auto-connect input.
    if !devices_page || fixture { return; }
    state["macosPairedDevices"] = match read() {
        Ok(devices) => serde_json::json!({"devices": devices.into_iter().map(|d|
            serde_json::json!({"name": d.name, "address": d.address}))
            .collect::<Vec<_>>()}),
        Err(_) => serde_json::json!({"error": true}),
    };
}

#[cfg(all(test, target_os = "macos"))]
mod paired_inventory_tests {
    use super::*;
    use livi_native_bluetooth::paired::{InventoryError, PairedDevice};
    use serde_json::json;

    #[test]
    fn fixtures_and_other_pages_never_read_inventory() {
        for (devices_page, fixture) in [(false, false), (false, true), (true, true)] {
            let original = json!({"devices":[],"config":{"nativeWirelessAa":{"enabled":false}}});
            let mut local = original.clone();
            macos_inventory_for_view(&mut local, devices_page, fixture,
                || panic!("offline or unrelated page must not enumerate"));
            assert_eq!(local, original);
        }
    }

    #[test]
    fn devices_inventory_only_supplements_local_view_and_preserves_gates() {
        let original = json!({"devices":[],"config":{"nativeWirelessAa":{"enabled":false}}});
        let mut local = original.clone();
        macos_inventory_for_view(&mut local, true, false, || Ok(vec![PairedDevice {
            name: "Offline phone".into(), address: "02:00:00:00:00:02".into(),
        }]));
        assert_eq!(local["macosPairedDevices"]["devices"][0]["name"], "Offline phone");
        assert_eq!(local["macosPairedDevices"]["devices"][0]["address"], "02:00:00:00:00:02");
        assert_eq!(local["devices"], original["devices"]);
        assert_eq!(local["config"], original["config"]);
        assert!(original.get("macosPairedDevices").is_none());
        macos_inventory_for_view(&mut local, true, false, || Err(InventoryError::Unavailable));
        assert_eq!(local["macosPairedDevices"], json!({"error":true}));
    }
}

pub fn observe(client: &Client) {
    let state = client.state().clone();
    with(|c| c.evidence.observe(&state));
}

pub fn leave(client: &Client) {
    with(|c| c.browser = None);
    #[cfg(all(target_os = "macos", feature = "bluetooth-app-test"))]
    diagnostic_cancel();
    follow_watches(client, Vec::new());
}

#[cfg(all(target_os = "macos", feature = "bluetooth-app-test"))]
pub(crate) fn diagnostic_cancel() {
    with(|c| c.diagnostic.revoke());
}

#[cfg(all(test, target_os = "macos", feature = "bluetooth-app-test"))]
pub(crate) fn seed_offline_paired_inventory() {
    with(|c| c.paired_inventory = Some((Instant::now(), serde_json::json!({"devices":[
        {"name":"Synthetic paired phone","address":"02:01:02:03:04:05"}]}))));
}

#[cfg(all(target_os = "macos", feature = "bluetooth-app-test"))]
pub(crate) fn diagnostic_tick(ui: &AppWindow, client: &Client, cancel: bool) -> bool {
    let (changed, idle) = with(|c| {
        let changed = c.diagnostic.tick(cancel);
        (changed, c.diagnostic.idle())
    }).unwrap_or((false, true));
    if changed && ui.get_page() == Page::Settings { render(ui, client); }
    idle
}
