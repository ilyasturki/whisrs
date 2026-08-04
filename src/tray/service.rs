//! System tray implementation using ksni (StatusNotifierItem).

use ksni::menu::StandardItem;
use ksni::{Icon, MenuItem, ToolTip, TrayMethods};
use tokio::sync::{mpsc, watch};
use tracing::{debug, info, warn};

use super::NotifyFn;
use crate::service_ctl::{restart_daemon_via_systemd, RestartOutcome};
use crate::{Command, State};

/// 16x16 ARGB icon data for each state.
/// Format: each pixel is 4 bytes (ARGB, big-endian).
mod icons {
    /// Generate a simple 16x16 solid circle icon with the given ARGB color.
    pub fn circle_icon(argb: u32) -> Vec<u8> {
        let size = 16;
        let center = size as f32 / 2.0;
        let radius = 6.0;
        let mut pixels = Vec::with_capacity(size * size * 4);

        for y in 0..size {
            for x in 0..size {
                let dx = x as f32 + 0.5 - center;
                let dy = y as f32 + 0.5 - center;
                let dist = (dx * dx + dy * dy).sqrt();

                if dist <= radius {
                    pixels.extend_from_slice(&argb.to_be_bytes());
                } else if dist <= radius + 1.0 {
                    let alpha = ((radius + 1.0 - dist) * 255.0) as u8;
                    let [_, r, g, b] = argb.to_be_bytes();
                    pixels.extend_from_slice(&[alpha, r, g, b]);
                } else {
                    pixels.extend_from_slice(&[0, 0, 0, 0]);
                }
            }
        }
        pixels
    }

    pub fn idle() -> Vec<u8> {
        circle_icon(0xFF_88_88_88)
    }

    pub fn recording() -> Vec<u8> {
        circle_icon(0xFF_E0_40_40)
    }

    pub fn transcribing() -> Vec<u8> {
        circle_icon(0xFF_E0_A0_20)
    }

    /// Read-aloud: synthesizing speech (blue/purple).
    pub fn synthesizing() -> Vec<u8> {
        circle_icon(0xFF_7C_5C_FF)
    }

    /// Read-aloud: playing speech (green).
    pub fn speaking() -> Vec<u8> {
        circle_icon(0xFF_34_D3_99)
    }
}

/// Small mutable state owned by the tray service itself.
///
/// Keeping this directly on the tray object is important: `ksni::Handle::update`
/// expects the closure to mutate the tray instance so the host knows which
/// properties changed. When the state lives out-of-band, some tray hosts can
/// miss icon refreshes and leave the old color visible.
struct TrayState {
    current: State,
}

/// Human-readable word for a state, used in the title and menu status line.
fn state_word(state: State) -> &'static str {
    match state {
        State::Idle => "idle",
        State::Recording => "recording",
        State::Transcribing => "transcribing",
        State::Synthesizing => "synthesizing",
        State::Speaking => "speaking",
    }
}

/// The ksni tray implementation.
struct WhisrsTray {
    state: TrayState,
    /// Sender into the daemon's shared command dispatch loop — the same loop
    /// the hotkey listener feeds — so tray clicks drive `handle_command`
    /// exactly like hotkey presses do.
    cmd_tx: mpsc::Sender<Command>,
    /// Desktop-toast hook from the daemon (`None` when notifications are
    /// disabled), used to surface menu-callback failures the journal alone
    /// would hide — currently a failed "Restart Daemon" click.
    notify: Option<NotifyFn>,
}

impl WhisrsTray {
    /// Queue a command for the daemon without blocking.
    ///
    /// ksni invokes tray callbacks on the tray service task and warns against
    /// blocking there, so use `try_send`; if the queue is somehow full the
    /// click is dropped with a warning instead of freezing the tray.
    fn send(&self, cmd: Command) {
        if let Err(e) = self.cmd_tx.try_send(cmd) {
            warn!("tray: failed to queue command for daemon: {e}");
        }
    }
}

impl ksni::Tray for WhisrsTray {
    fn id(&self) -> String {
        "whisrs".to_string()
    }

    fn title(&self) -> String {
        format!("whisrs — {}", state_word(self.state.current))
    }

    /// Left-click on the icon: toggle recording, same as `whisrs toggle`.
    fn activate(&mut self, _x: i32, _y: i32) {
        debug!("tray activated (left-click): toggle");
        self.send(Command::Toggle { language: None });
    }

    fn icon_pixmap(&self) -> Vec<Icon> {
        let data = match self.state.current {
            State::Idle => icons::idle(),
            State::Recording => icons::recording(),
            State::Transcribing => icons::transcribing(),
            State::Synthesizing => icons::synthesizing(),
            State::Speaking => icons::speaking(),
        };
        vec![Icon {
            width: 16,
            height: 16,
            data,
        }]
    }

    fn tool_tip(&self) -> ToolTip {
        let description = match self.state.current {
            State::Idle => "Idle — ready to record",
            State::Recording => "Recording...",
            State::Transcribing => "Transcribing...",
            State::Synthesizing => "Synthesizing…",
            State::Speaking => "Reading aloud…",
        };
        ToolTip {
            title: "whisrs".to_string(),
            description: description.to_string(),
            icon_name: String::new(),
            icon_pixmap: Vec::new(),
        }
    }

    /// Right-click menu.
    ///
    /// Deliberately holds no recording controls: whisrs is driven by hotkeys,
    /// and toggle/cancel each already have a hotkey, a CLI command, and (for
    /// toggle) the left-click above. A menu item for them would be a fourth
    /// way to do the same thing. What is left is what has no other trigger.
    fn menu(&self) -> Vec<MenuItem<Self>> {
        let state = self.state.current;
        vec![
            // Non-interactive status line: version + current state.
            MenuItem::Standard(StandardItem {
                label: format!(
                    "whisrs v{} — {}",
                    env!("CARGO_PKG_VERSION"),
                    state_word(state)
                ),
                enabled: false,
                ..Default::default()
            }),
            MenuItem::Separator,
            MenuItem::Standard(StandardItem {
                label: "Restart Daemon".to_string(),
                activate: Box::new(|tray: &mut Self| {
                    restart_daemon(tray.notify);
                }),
                ..Default::default()
            }),
            MenuItem::Standard(StandardItem {
                label: "Quit".to_string(),
                activate: Box::new(|_tray: &mut Self| {
                    quit_daemon();
                }),
                ..Default::default()
            }),
        ]
    }
}

/// Restart the daemon through systemd, from the tray menu.
///
/// This cannot go through the daemon's own command loop: a successful restart
/// kills this very process before it could reply. Runs on its own thread
/// because ksni menu callbacks must not block and `systemctl` is a subprocess
/// round-trip.
///
/// Failure raises a desktop toast (when `notify` is set): without one, a
/// click on a non-systemd setup would silently do nothing, since journal
/// warnings are invisible from the tray.
fn restart_daemon(notify: Option<NotifyFn>) {
    info!("tray: restart requested");
    std::thread::spawn(move || match restart_daemon_via_systemd() {
        // When this daemon runs under the unit, systemd kills it mid-restart,
        // so this line is normally never reached.
        RestartOutcome::Restarted => info!("tray: daemon restarted via systemd"),
        RestartOutcome::NoSystemdUnit => {
            warn!("tray: no whisrs.service user unit loaded — restart the daemon manually");
            if let Some(notify) = notify {
                notify(
                    "whisrs",
                    "Restart from the tray needs the whisrs.service systemd unit. \
                     Restart the daemon manually.",
                );
            }
        }
        RestartOutcome::Failed => {
            warn!("tray: `systemctl --user restart whisrs.service` failed");
            if let Some(notify) = notify {
                notify(
                    "whisrs",
                    "Daemon restart failed: `systemctl --user restart whisrs.service` \
                     returned an error.",
                );
            }
        }
    });
}

/// Quit from the tray menu, mirroring the daemon's SIGINT handler:
/// remove the IPC socket, then exit cleanly.
fn quit_daemon() {
    info!("tray: quit requested, shutting down");
    let _ = std::fs::remove_file(crate::socket_path());
    std::process::exit(0);
}

/// Maximum number of attempts to connect to the SNI tray host.
const TRAY_MAX_RETRIES: u32 = 10;

/// Initial retry delay (doubles each attempt, capped at 10 s).
const TRAY_INITIAL_DELAY: std::time::Duration = std::time::Duration::from_secs(1);

/// Spawn the system tray indicator.
///
/// Runs in the background and updates the icon whenever the daemon state changes.
/// Retries with exponential backoff if the SNI host isn't available yet (common
/// on boot when the daemon starts before the desktop environment is fully ready).
pub async fn spawn_tray(
    mut state_rx: watch::Receiver<State>,
    cmd_tx: mpsc::Sender<Command>,
    notify: Option<NotifyFn>,
) {
    // Retry spawning the tray with exponential backoff.
    let mut delay = TRAY_INITIAL_DELAY;
    let mut handle = None;

    for attempt in 1..=TRAY_MAX_RETRIES {
        let tray = WhisrsTray {
            state: TrayState {
                current: *state_rx.borrow(),
            },
            cmd_tx: cmd_tx.clone(),
            notify,
        };

        match tray.spawn().await {
            Ok(h) => {
                info!("system tray started (attempt {attempt})");
                handle = Some(h);
                break;
            }
            Err(e) => {
                if attempt == TRAY_MAX_RETRIES {
                    warn!(
                        "failed to start system tray after {TRAY_MAX_RETRIES} attempts: {e} — continuing without tray"
                    );
                    return;
                }
                info!(
                    "tray host not available (attempt {attempt}/{TRAY_MAX_RETRIES}): {e} — retrying in {delay:?}"
                );
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(std::time::Duration::from_secs(10));
            }
        }
    }

    let handle = handle.expect("handle must be set after successful spawn");

    // Watch for state changes and update the tray.
    tokio::spawn(async move {
        while state_rx.changed().await.is_ok() {
            let new_state = *state_rx.borrow();
            debug!("tray state update: {new_state:?}");
            // Mutate the tray object itself so ksni emits the corresponding
            // D-Bus property changes for title, tooltip, and icon pixmap.
            handle
                .update(|tray| {
                    tray.state.current = new_state;
                })
                .await;
        }
    });
}
