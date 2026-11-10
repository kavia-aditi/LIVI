// SPDX-FileCopyrightText: 2025-2026 Lasse Heitgres
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional terms: see NOTICE

//! Presentation-only simulation. Never represents adapter or projection state.
pub const NOTICE: &str = "Simulation — no Bluetooth connection established";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum State {
    #[default]
    Disconnected,
    Connecting,
    Connected,
    ConnectionLost,
    Reconnecting,
}

impl State {
    pub fn label(self) -> &'static str {
        match self {
            Self::Disconnected => "Disconnected",
            Self::Connecting => "Connecting",
            Self::Connected => "Connected",
            Self::ConnectionLost => "Connection lost",
            Self::Reconnecting => "Reconnecting",
        }
    }

    pub fn pending(self) -> bool {
        matches!(self, Self::Connecting | Self::Reconnecting)
    }
}

#[derive(Clone, Copy, Debug)]
pub enum Action {
    Open,
    Back,
    Advance,
    Disconnect,
    LoseConnection,
    ServiceLost,
    ServiceRecovered,
    Reset,
}

#[derive(Debug)]
pub struct Demo {
    pub opened: bool,
    pub service_available: bool,
    pub state: State,
}

impl Default for Demo {
    fn default() -> Self {
        Self {
            opened: false,
            service_available: true,
            state: State::Disconnected,
        }
    }
}

impl Demo {
    pub fn apply(&mut self, action: Action) {
        match action {
            Action::Open => self.opened = true,
            Action::Back => {
                self.opened = false;
                self.state = State::Disconnected;
            }
            Action::ServiceLost => {
                self.service_available = false;
                self.state = match self.state {
                    State::Disconnected => State::Disconnected,
                    _ => State::ConnectionLost,
                };
            }
            Action::ServiceRecovered => {
                self.service_available = true;
                self.state = State::Disconnected;
            }
            Action::Reset => *self = Self::default(),
            Action::Advance if self.opened && self.service_available => {
                self.state = match self.state {
                    State::Disconnected => State::Connecting,
                    State::Connecting | State::Reconnecting => State::Connected,
                    State::ConnectionLost => State::Reconnecting,
                    State::Connected => State::Connected,
                };
            }
            Action::Disconnect if self.opened => self.state = State::Disconnected,
            Action::LoseConnection if self.opened && self.state == State::Connected => {
                self.state = State::ConnectionLost;
            }
            _ => {}
        }
    }

    pub fn advance_label(&self) -> &'static str {
        match self.state {
            State::Connecting | State::Reconnecting => "Complete simulated attempt",
            State::ConnectionLost => "Simulate reconnect",
            _ => "Simulate connect",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_disconnect_loss_and_reconnect() {
        let mut demo = Demo::default();
        demo.apply(Action::Open);
        for _ in 0..100 {
            for (action, expected) in [
                (Action::Advance, State::Connecting),
                (Action::Advance, State::Connected),
                (Action::LoseConnection, State::ConnectionLost),
                (Action::Advance, State::Reconnecting),
                (Action::Advance, State::Connected),
                (Action::Disconnect, State::Disconnected),
            ] {
                demo.apply(action);
                assert_eq!(demo.state, expected);
            }
        }
    }

    #[test]
    fn service_loss_cancels_each_state_and_recovery_starts_disconnected() {
        for state in [
            State::Disconnected,
            State::Connecting,
            State::Connected,
            State::ConnectionLost,
            State::Reconnecting,
        ] {
            let mut demo = Demo {
                opened: true,
                service_available: true,
                state,
            };
            demo.apply(Action::ServiceLost);
            assert!(!demo.service_available);
            assert!(!demo.state.pending());
            let unavailable = demo.state;
            demo.apply(Action::Advance);
            assert_eq!(demo.state, unavailable);
            demo.apply(Action::ServiceRecovered);
            assert_eq!(demo.state, State::Disconnected);
            demo.apply(Action::Advance);
            assert_eq!(demo.state, State::Connecting);
        }
    }

    #[test]
    fn leaving_cleans_pending_attempts_and_blocks_late_completion() {
        for state in [State::Connecting, State::Reconnecting, State::Connected] {
            let mut demo = Demo {
                opened: true,
                service_available: true,
                state,
            };
            demo.apply(Action::Back);
            assert!(!demo.opened);
            assert_eq!(demo.state, State::Disconnected);
            demo.apply(Action::Advance);
            assert_eq!(demo.state, State::Disconnected);
            demo.apply(Action::Open);
            assert_eq!(demo.state, State::Disconnected);
        }
    }

    #[test]
    fn opening_never_connects_and_reset_clears_demo_state() {
        let mut demo = Demo::default();
        demo.apply(Action::Advance);
        assert_eq!(demo.state, State::Disconnected);
        demo.apply(Action::Open);
        assert_eq!(demo.state, State::Disconnected);
        demo.apply(Action::ServiceLost);
        demo.apply(Action::Reset);
        assert!(!demo.opened);
        assert!(demo.service_available);
        assert_eq!(demo.state, State::Disconnected);
    }
}
