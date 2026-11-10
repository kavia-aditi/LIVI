// SPDX-FileCopyrightText: 2025-2026 Lasse Heitgres
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional terms: see NOTICE

use super::*;

fn native_status_owner(
    context: livi_aa_stack::manager::NativeContext, session: AaId,
    render: Option<livi_host_proto::native::Owner>,
) -> livi_core_proto::native_wireless_aa::NativeProjectionOwner {
    livi_core_proto::native_wireless_aa::NativeProjectionOwner {
        runtime_id: context.runtime, attempt_id: context.attempt, generation: context.generation,
        session_id: Some(session), connection_id: render.map(|o| o.connection),
        player_id: render.map(|o| o.player),
    }
}

impl Projection {
    pub(super) fn on_aa(&mut self, ev: AaEvent) {
        match ev {
            AaEvent::NativeSession { id, context } => {
                use livi_core_proto::native_wireless_aa::{NativeProjectionMilestone as M, NativeProjectionProgress as P};
                if self.native_latest.is_some_and(|latest| id <= latest) { return; }
                self.native_latest = Some(id);
                let owner = native_status_owner(context, id, None);
                self.native_sessions.insert(id, NativeReadiness {
                    context, tls: false, main_video: false, owner: None, visibility: 0,
                    decoded: BTreeMap::new(), last_frame: 0, ready: false,
                    progress: P::Milestone { owner, milestone: M::Tcp },
                    deadline: tokio::time::Instant::now() + Duration::from_secs(30),
                });
                self.hub.update(|s| s.system.native_wireless_aa =
                    livi_core_proto::native_wireless_aa::NativeWirelessAaStatus::Connecting {
                        attempt_id: context.attempt,
                    });
                self.publish();
            }
            AaEvent::NativeMilestone { id, context, milestone } => {
                use livi_aa_stack::manager::NativeMilestone;
                use livi_core_proto::native_wireless_aa::{NativeProjectionMilestone as M, NativeProjectionProgress as P};
                if let Some(entry) = self.native_sessions.get_mut(&id).filter(|entry| entry.context == context) {
                    if matches!(entry.progress, P::Failed { .. } | P::Cancelled { .. }) { return; }
                    match milestone {
                        NativeMilestone::Tls => entry.tls = true,
                        NativeMilestone::MainVideo if entry.tls => entry.main_video = true,
                        _ => return,
                    }
                    if entry.ready { return; }
                    entry.progress = P::Milestone {
                        owner: native_status_owner(context, id, entry.owner),
                        milestone: if entry.main_video { M::AaRunning } else { M::Tls },
                    };
                    self.publish();
                }
            }
            AaEvent::Spawned {
                id,
                wired,
                usb_serial,
                ..
            } => {
                self.aa_spawned.insert(id, (wired, usb_serial));
            }
            AaEvent::Connected { id } => {
                let (wired, usb_serial) =
                    self.aa_spawned.get(&id).cloned().unwrap_or((false, None));
                println!(
                    "[core] Android Auto projects over {}",
                    if wired { "usb" } else { "wifi" }
                );
                let ids = Ids {
                    usb_serial: usb_serial.filter(|s| !s.is_empty()),
                    ..Default::default()
                };
                let index = self.upsert(Link::Aa(id), transport(wired), &ids);
                self.auto_activate(index);
                if !self.native_sessions.contains_key(&id) { self.ensure_aa_links(); }
                self.aa_telemetry.hydrate(&self.telemetry_seen, &self.aa);
            }
            AaEvent::HelperConnected => self.push_wired(true),
            AaEvent::Helper(ev) => self.on_aa_helper(&ev),
            AaEvent::Device {
                id,
                name,
                model,
                instance_id,
                ip,
            } => {
                self.on_aa_device(id, name, model, instance_id, ip);
            }
            AaEvent::Status { id, status } => {
                if self.native_sessions.contains_key(&id) { return; }
                let link = Link::Aa(id);
                let ids = match self.sessions.iter().find(|s| s.link == link) {
                    Some(s) => registry_ids(&s.device),
                    None => devices::Ids {
                        ip: opt(&status.ip),
                        ..Default::default()
                    },
                };
                if status.battery_level.is_some() {
                    self.hands_free.battery_precise = true;
                }
                let status = devices::Status {
                    battery_level: status.battery_level.map(f64::from),
                    signal_strength: status.signal_strength.map(f64::from),
                    ..Default::default()
                };
                self.devices.registry.note_status(&ids, &status);
                self.publish();
            }
            AaEvent::Disconnected { id } => {
                self.aa_spawned.remove(&id);
                self.cancel_native_touches(id);
                if let Some(entry) = self.native_sessions.remove(&id) {
                    use livi_core_proto::native_wireless_aa::{NativeWirelessAaFailure as E, NativeWirelessAaStatus as S};
                    let newest = self.native_latest == Some(id);
                    self.hub.update(|s| {
                        if newest && !matches!(s.system.native_wireless_aa,
                            S::Cancelled { attempt_id } | S::Failed { attempt_id, .. }
                                if attempt_id == entry.context.attempt)
                        {
                            s.system.native_wireless_aa = S::Failed {
                                attempt_id: entry.context.attempt, code: E::TransportFailed,
                            };
                        }
                    });
                }
                self.close_by_link(Link::Aa(id));
                self.publish();
            }
            AaEvent::Audio {
                id, stream, active, ..
            } => {
                if active && self.is_active(Link::Aa(id)) {
                    self.push_level(cp_kind(AaKind::of(stream)), 0);
                }
                if self.is_active(Link::Aa(id)) {
                    self.activity(|a| a.stream(cp_kind(AaKind::of(stream)), active));
                }
            }
            AaEvent::Duck {
                id,
                level,
                duration_ms,
            } => {
                self.on_duck(Link::Aa(id), level, duration_ms);
            }
            AaEvent::HostUiRequested { id } => {
                println!("[core] Android Auto asks for the car's own UI");
                if self.is_active(Link::Aa(id)) {
                    self.host_ui();
                }
            }
            AaEvent::VideoCodec { id, cluster, .. } => {
                if !cluster && !self.native_sessions.contains_key(&id) {
                    self.video_up(Link::Aa(id));
                }
            }
            AaEvent::NowPlaying { id, now_playing } => self.aa_now_playing(id, now_playing),
            AaEvent::Navigation { id, navigation } => self.aa_navigation(id, navigation),
            AaEvent::NavigationImage { id, image } => {
                let link = Link::Aa(id);
                let Some(entry) = self.by_link(link) else {
                    return;
                };
                entry.navigation.image = (!image.is_empty()).then(|| STANDARD.encode(&image));
                if self.is_active(link) {
                    self.publish();
                }
            }
            _ => {}
        }
    }

    pub(super) fn on_native_feedback(&mut self, feedback: livi_host_proto::native::Feedback) {
        use livi_host_proto::native::{Feedback as F, Packet};
        use livi_core_proto::native_wireless_aa::{NativeProjectionFailure as E, NativeProjectionMilestone as M, NativeProjectionProgress as P};
        let Some(owner) = Packet::Feedback(feedback.clone()).owner() else { return };
        if self.plane.native_owner() != Some(owner) || !self.is_active(Link::Aa(owner.session)) { return; }
        let visible = self.front_main() == Front::Projection;
        let Some(entry) = self.native_sessions.get_mut(&owner.session).filter(|entry| {
            entry.context.runtime == owner.runtime && entry.context.attempt == owner.attempt
                && entry.context.generation == owner.generation
                && entry.owner.is_none_or(|current| current == owner)
        }) else { return };
        if matches!(entry.progress, P::Failed { .. } | P::Cancelled { .. }) { return; }
        entry.owner = Some(owner);
        let status_owner = native_status_owner(entry.context, owner.session, Some(owner));
        match feedback {
            F::Invalidated { visibility, .. } if visibility > entry.visibility => {
                entry.visibility = visibility;
                entry.ready = false;
                entry.decoded.clear();
                entry.progress = P::Invalidated { owner: status_owner, visibility_epoch: visibility };
            }
            F::Decoded { frame, ready_time, .. } if entry.note_decoded(frame, ready_time) => {
                if !entry.ready {
                    entry.progress = P::Milestone { owner: status_owner, milestone: M::DecodedBgra };
                }
            }
            F::Presented { frame, visibility, time, .. }
                if entry.presented(frame, visibility, time, visible) =>
            {
                entry.progress = P::Presented { owner: status_owner, frame_id: frame,
                    visibility_epoch: visibility, presented_time: time };
                if self.native_latest == Some(owner.session) {
                    self.hub.update(|s| s.system.native_wireless_aa =
                        livi_core_proto::native_wireless_aa::NativeWirelessAaStatus::Connected {
                            attempt_id: owner.attempt,
                        });
                }
            }
            F::Failed { .. } => {
                entry.ready = false;
                entry.decoded.clear();
                entry.progress = P::Failed { owner: status_owner, code: E::RenderFailed };
                self.aa.send(AaCmd::Close { id: owner.session });
            }
            _ => return,
        }
        if !self.native_sessions[&owner.session].ready { self.cancel_native_touches(owner.session); }
        self.publish();
    }

    pub(super) fn cancel_native(&mut self, owner: livi_core_proto::native_wireless_aa::NativeProjectionOwner) {
        use livi_core_proto::native_wireless_aa::{NativeProjectionProgress as P, NativeWirelessAaStatus as S};
        let attempt = owner.attempt_id;
        if attempt == 0 { return; }
        let ids: Vec<_> = self.native_sessions.iter()
            .filter(|(id, entry)| entry.context.attempt == attempt
                && entry.context.runtime == owner.runtime_id && entry.context.generation == owner.generation
                && owner.session_id.is_none_or(|session| session == **id))
            .map(|(id, _)| *id).collect();
        if ids.is_empty() { return; }
        for id in ids {
            self.cancel_native_touches(id);
            let entry = self.native_sessions.get_mut(&id).unwrap();
            entry.ready = false;
            entry.decoded.clear();
            entry.progress = P::Cancelled { owner: native_status_owner(entry.context, id, entry.owner) };
            self.aa.send(AaCmd::Close { id });
        }
        if self.native_latest.and_then(|id| self.native_sessions.get(&id)).is_some_and(|entry|
            entry.context.runtime == owner.runtime_id && entry.context.attempt == attempt
                && entry.context.generation == owner.generation)
        {
            self.hub.update(|s| s.system.native_wireless_aa = S::Cancelled { attempt_id: attempt });
        }
        self.publish();
    }

    pub(super) fn expire_native(&mut self, now: tokio::time::Instant) {
        use livi_core_proto::native_wireless_aa::{NativeProjectionFailure as E, NativeProjectionProgress as P,
            NativeWirelessAaFailure as W, NativeWirelessAaStatus as S};
        let ids: Vec<_> = self.native_sessions.iter().filter(|(_, entry)| {
            entry.last_frame == 0 && now >= entry.deadline
                && !matches!(entry.progress, P::Failed { .. } | P::Cancelled { .. })
        }).map(|(id, _)| *id).collect();
        if ids.is_empty() { return; }
        for id in ids {
            self.cancel_native_touches(id);
            let entry = self.native_sessions.get_mut(&id).unwrap();
            entry.decoded.clear();
            entry.progress = P::Failed { owner: native_status_owner(entry.context, id, entry.owner), code: E::TimedOut };
            self.aa.send(AaCmd::Close { id });
            let attempt_id = entry.context.attempt;
            if self.native_latest == Some(id) {
                self.hub.update(|s| s.system.native_wireless_aa = S::Failed {
                    attempt_id, code: W::TimedOut,
                });
            }
        }
        self.publish();
    }

    pub(super) fn invalidate_native(&mut self) {
        let ids: Vec<_> = self.native_sessions.keys().copied().collect();
        for id in ids {
            self.cancel_native_touches(id);
            if let Some(entry) = self.native_sessions.get_mut(&id) {
                entry.ready = false;
                entry.decoded.clear();
                entry.progress = livi_core_proto::native_wireless_aa::NativeProjectionProgress::Failed {
                    owner: native_status_owner(entry.context, id, entry.owner),
                    code: livi_core_proto::native_wireless_aa::NativeProjectionFailure::RenderFailed,
                };
                self.aa.send(AaCmd::Close { id });
            }
        }
        self.publish();
    }

    pub(super) fn on_aa_helper(&mut self, ev: &Value) {
        let text = |key: &str| {
            ev.get(key)
                .and_then(Value::as_str)
                .filter(|v| !v.is_empty())
        };
        let up = ev.get("up").and_then(Value::as_bool) == Some(true);
        match text("event") {
            Some("sco") => self.call_audio(up),
            Some("hfp") => self.hands_free.on_link(up, text("mac")),
            Some("phone-battery") => {
                let pct = ev.get("pct").and_then(Value::as_f64);
                let Some(mac) = text("mac") else { return };
                println!("[core] HFP battery {mac}: ~{}%", pct.unwrap_or(0.0));
                if self.hands_free.battery_precise || pct.is_none() {
                    return;
                }
                let ids = devices::Ids {
                    bt_mac: Some(mac.to_string()),
                    ..Default::default()
                };
                let status = devices::Status {
                    battery_level: pct,
                    ..Default::default()
                };
                self.devices.registry.note_status(&ids, &status);
                self.publish();
            }
            Some("aa-device") => {
                let Some(instance) = text("instanceId") else {
                    return;
                };
                if let Some(mac) = text("btMac") {
                    self.hands_free
                        .bt_by_instance
                        .insert(instance.into(), mac.into());
                    self.keep_link(mac);
                }
                if let Some(serial) = text("usbSerial") {
                    self.hands_free
                        .serial_by_instance
                        .insert(instance.into(), serial.into());
                }
            }
            _ => {}
        }
    }

    pub(super) fn on_aa_device(
        &mut self,
        id: AaId,
        name: String,
        model: String,
        instance: String,
        ip: String,
    ) {
        if self.native_sessions.contains_key(&id) {
            // Remote metadata is not selected-phone authentication and must
            // never merge into wired/CarPlay identities or keep an HFP link.
            return;
        }
        let link = Link::Aa(id);
        let wired = match self.sessions.iter().find(|s| s.link == link) {
            Some(s) => s.transport == Transport::Usb,
            None => self.aa_spawned.get(&id).is_some_and(|(wired, _)| *wired),
        };
        let instance_id = opt(&instance);
        let lookup =
            |by: &HashMap<String, String>| instance_id.as_ref().and_then(|i| by.get(i)).cloned();
        let bt_mac = if wired {
            None
        } else {
            lookup(&self.hands_free.bt_by_instance)
        };
        let usb_serial = self
            .aa_spawned
            .get(&id)
            .and_then(|(_, serial)| serial.clone())
            .filter(|s| !s.is_empty())
            .or_else(|| lookup(&self.hands_free.serial_by_instance));
        let ids = Ids {
            bt_mac,
            usb_serial,
            instance_id,
            ip: opt(&ip),
            ..Default::default()
        };
        let seen = Seen {
            ids: registry_ids(&ids),
            name: opt(&name),
            model: opt(&model),
            protocol: Some(Protocol::Androidauto),
            wired,
        };
        self.devices.registry.note_device(&seen);
        let index = self.upsert(link, transport(wired), &ids);
        self.auto_activate(index);
    }

    pub(super) fn ensure_aa_links(&mut self) {
        let known: Vec<String> = self.hands_free.bt_by_instance.values().cloned().collect();
        if known.is_empty() {
            let _ = self.phone_io.send(Io::FindPhones);
            return;
        }
        for mac in known {
            self.keep_link(&mac);
        }
    }

    pub(super) fn on_back(&mut self, back: Back) {
        let Back::Phones(phones) = back;
        if let [mac] = phones.as_slice() {
            let unnamed: Vec<Option<String>> = self
                .sessions
                .iter()
                .filter(|s| matches!(s.link, Link::Aa(id) if !self.native_sessions.contains_key(&id))
                    && s.device.bt_mac.is_none())
                .map(|s| s.device.instance_id.clone())
                .collect();
            for instance in unnamed {
                if let Some(instance) = &instance {
                    self.hands_free
                        .bt_by_instance
                        .insert(instance.clone(), mac.clone());
                }
                let ids = devices::Ids {
                    bt_mac: Some(mac.clone()),
                    instance_id: instance,
                    ..Default::default()
                };
                let seen = Seen {
                    ids,
                    protocol: Some(Protocol::Androidauto),
                    ..Default::default()
                };
                self.devices.registry.note_device(&seen);
            }
            self.publish();
        }
        for mac in phones {
            self.keep_link(&mac);
        }
    }

    pub(super) fn keep_link(&mut self, mac: &str) {
        if self.hands_free.keep(mac) {
            self.check_link(&mac.to_lowercase());
        }
    }

    pub(super) fn keep_links(&mut self) {
        for mac in self.hands_free.kept() {
            self.check_link(&mac);
        }
    }

    pub(super) fn check_link(&mut self, mac: &str) {
        let wanted = self
            .hands_free
            .bt_by_instance
            .values()
            .any(|m| m.eq_ignore_ascii_case(mac))
            || self.sessions.iter().any(|s| matches!(s.link, Link::Aa(id)
                if !self.native_sessions.contains_key(&id)));
        if let Some(io) = self
            .hands_free
            .check(mac, wanted, tokio::time::Instant::now())
        {
            let _ = self.phone_io.send(io);
        }
    }

    pub(super) fn aa_now_playing(&mut self, id: AaId, from: AaNowPlaying) {
        let link = Link::Aa(id);
        let Some(entry) = self.by_link(link) else {
            return;
        };
        let np = &mut entry.now_playing;
        for (field, value) in [
            (&mut np.title, from.title),
            (&mut np.artist, from.artist),
            (&mut np.album, from.album),
            (&mut np.app, from.app),
        ] {
            if value.is_some() {
                *field = value;
            }
        }
        if let Some(ms) = from.duration_ms {
            np.duration_ms = Some(ms as f64);
        }
        if let Some(ms) = from.elapsed_ms {
            np.elapsed_ms = Some(ms as f64);
        }
        if from.playing.is_some() {
            np.playing = from.playing;
        }
        if let Some(art) = from.artwork.filter(|a| !a.is_empty()) {
            np.artwork = Some(STANDARD.encode(art));
        }
        if self.is_active(link) {
            self.publish();
        }
    }

    pub(super) fn aa_navigation(&mut self, id: AaId, from: AaNavigation) {
        let link = Link::Aa(id);
        let Some(entry) = self.by_link(link) else {
            return;
        };
        let image = entry.navigation.image.take();
        entry.navigation = if from.active == Some(false) {
            Navigation {
                active: Some(false),
                ..Default::default()
            }
        } else {
            Navigation {
                active: from.active,
                road_name: from.road_name,
                destination_name: from.destination_name,
                time_to_destination: from.time_to_destination.map(f64::from),
                distance_to_destination: from.distance_to_destination.map(f64::from),
                remain_distance: from.remain_distance.map(f64::from),
                maneuver_type: from.maneuver_type.map(|m| m as u32),
                turn_side: from.turn_side.map(|s| s as u32),
                turn_angle: from.turn_angle.and_then(|a| i32::try_from(a).ok()),
                eta_text: from.eta,
                app_name: from.app_name,
                image,
                ..Default::default()
            }
        };
        if self.is_active(link) {
            self.publish();
        }
    }
}
