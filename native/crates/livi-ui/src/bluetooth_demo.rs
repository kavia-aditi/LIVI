// SPDX-FileCopyrightText: 2025-2026 Lasse Heitgres
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional terms: see NOTICE

//! Isolated native demonstration: no Client, core process, helper or transport.
use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use livi_ui_model::bluetooth_demo::{Action, Demo, NOTICE, State};
use slint::{ComponentHandle, Timer, TimerMode};

use crate::BluetoothDemoWindow;

fn action(id: i32) -> Option<Action> {
    Some(match id {
        0 => Action::Open,
        1 => Action::Back,
        2 => Action::Advance,
        3 => Action::Disconnect,
        4 => Action::LoseConnection,
        5 => Action::ServiceLost,
        6 => Action::ServiceRecovered,
        7 => Action::Reset,
        _ => return None,
    })
}

fn render(ui: &BluetoothDemoWindow, demo: &Demo) {
    ui.set_opened(demo.opened);
    ui.set_service_available(demo.service_available);
    ui.set_connected(demo.state == State::Connected);
    ui.set_state_label(demo.state.label().into());
    ui.set_advance_label(demo.advance_label().into());
}

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    let capture = if let Some(i) = args.iter().position(|a| a == "--capture-dir") {
        let path = PathBuf::from(args.get(i + 1).ok_or("--capture-dir requires a path")?);
        std::fs::create_dir_all(&path)?;
        Some(path)
    } else {
        None
    };
    let ui = BluetoothDemoWindow::new()?;
    ui.set_notice(NOTICE.into());
    let demo = Rc::new(RefCell::new(Demo::default()));
    render(&ui, &demo.borrow());
    ui.on_action({
        let weak = ui.as_weak();
        let demo = demo.clone();
        move |id| {
            let (Some(ui), Some(action)) = (weak.upgrade(), action(id)) else {
                return;
            };
            let mut demo = demo.borrow_mut();
            demo.apply(action);
            render(&ui, &demo);
            println!(
                "[bluetooth-demo] action={action:?} opened={} service={} state={:?}",
                demo.opened, demo.service_available, demo.state
            );
        }
    });

    // The scripted run invokes the same callback as the on-screen buttons.
    // Every capture occurs after a render delay, before the next transition.
    let timer = Timer::default();
    let error = Rc::new(RefCell::new(None::<String>));
    let automated = capture.is_some();
    let completed = Rc::new(Cell::new(false));
    if let Some(dir) = capture {
        use State::*;
        let steps = [
            (0, Disconnected, true, true, "disconnected"),
            (2, Connecting, true, true, "connecting"),
            (2, Connected, true, true, "connected-simulated"),
            (4, ConnectionLost, true, true, "connection-lost"),
            (2, Reconnecting, true, true, "reconnecting"),
            (2, Connected, true, true, "reconnected-simulated"),
            (5, ConnectionLost, true, false, "service-unavailable"),
            (2, ConnectionLost, true, false, "unavailable-blocks-connect"),
            (6, Disconnected, true, true, "service-recovered"),
            (2, Connecting, true, true, "recovery-connecting"),
            (1, Disconnected, false, true, "navigation-cleanup"),
            (2, Disconnected, false, true, "late-completion-blocked"),
            (0, Disconnected, true, true, "navigation-return"),
            (2, Connecting, true, true, "repeat-connecting"),
            (2, Connected, true, true, "repeat-connected"),
            (3, Disconnected, true, true, "explicit-disconnect"),
            (2, Connecting, true, true, "pending-service-loss-start"),
            (5, ConnectionLost, true, false, "pending-service-loss"),
            (6, Disconnected, true, true, "second-recovery"),
            (1, Disconnected, false, true, "final-cleanup"),
        ];
        let index = Rc::new(Cell::new(0usize));
        timer.start(TimerMode::Repeated, Duration::from_millis(800), {
            let weak = ui.as_weak();
            let demo = demo.clone();
            let error = error.clone();
            let completed = completed.clone();
            move || {
                let Some(ui) = weak.upgrade() else { return };
                let i = index.get();
                if i == steps.len() {
                    completed.set(true);
                    println!(
                        "[bluetooth-demo] PASS: {} checked UI steps; exiting",
                        steps.len()
                    );
                    let _ = slint::quit_event_loop();
                    return;
                }
                let (id, state, opened, service, label) = steps[i];
                ui.invoke_action(id);
                let actual = demo.borrow();
                if (actual.state, actual.opened, actual.service_available)
                    != (state, opened, service)
                    || ui.get_state_label().as_str() != state.label()
                    || ui.get_opened() != opened
                    || ui.get_service_available() != service
                {
                    *error.borrow_mut() = Some(format!("UI step {i} ({label}) failed: {actual:?}"));
                    let _ = slint::quit_event_loop();
                    return;
                }
                let path = dir.join(format!("{i:02}-{label}.png"));
                let weak = weak.clone();
                let error = error.clone();
                Timer::single_shot(Duration::from_millis(250), move || {
                    let result = (|| -> Result<(), Box<dyn std::error::Error>> {
                        let ui = weak.upgrade().ok_or("demo window closed before capture")?;
                        let buf = ui.window().take_snapshot()?;
                        let img = image::RgbaImage::from_raw(
                            buf.width(),
                            buf.height(),
                            buf.as_bytes().to_vec(),
                        )
                        .ok_or("invalid screenshot buffer")?;
                        img.save(&path)?;
                        println!("[bluetooth-demo] screenshot {}", path.display());
                        Ok(())
                    })();
                    if let Err(e) = result {
                        *error.borrow_mut() = Some(format!("capture failed: {e}"));
                        let _ = slint::quit_event_loop();
                    }
                });
                index.set(i + 1);
            }
        });
    }
    println!("[bluetooth-demo] {NOTICE}; core/helper startup bypassed");
    ui.show()?;
    let result = slint::run_event_loop_until_quit();
    timer.stop();
    demo.borrow_mut().apply(Action::Back);
    ui.hide()?;
    println!("[bluetooth-demo] cleanup: timer stopped, view closed, simulated attempt cancelled");
    result?;
    if let Some(error) = error.borrow_mut().take() {
        return Err(error.into());
    }
    if automated && !completed.get() {
        return Err("automated demo closed before all steps completed".into());
    }
    Ok(())
}
