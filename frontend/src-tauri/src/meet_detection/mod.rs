pub mod commands;
pub mod window_scan;

use crate::audio::recording_commands::{self, RecordingArgs};
use crate::database::repositories::setting::SettingsRepository;
use crate::state::AppState;
use log::{debug, info, warn};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager, Runtime};

const POLL_INTERVAL: Duration = Duration::from_secs(5);
/// Consecutive "no Meet window" polls to wait through before auto-stopping, so a brief tab
/// switch or window-manager hiccup doesn't cut a call short. ~20s at a 5s poll interval.
const STOP_DEBOUNCE_POLLS: u32 = 4;
/// After a failed auto-start (e.g. no audio device available), wait this long before retrying
/// rather than hammering it every poll for as long as the call stays open.
const AUTO_START_RETRY_COOLDOWN: Duration = Duration::from_secs(60);
/// How often to re-log the missing-permission warning — otherwise it fires every single 5s
/// poll for as long as the toggle is on and permission isn't granted, drowning out other logs.
const PERMISSION_WARNING_INTERVAL: Duration = Duration::from_secs(300);

/// Spawns the background poller that watches for an active Google Meet call and auto
/// starts/stops Meetily's existing recording pipeline. The loop always runs (one settings
/// read + one window scan every 5s) but only acts while `auto_detect_meet_enabled` is on,
/// so flipping the Settings toggle takes effect without an app restart.
pub fn spawn_meet_detection_task<R: Runtime>(app: AppHandle<R>) {
    tauri::async_runtime::spawn(async move {
        let mut consecutive_absent: u32 = 0;
        let mut last_start_failure: Option<Instant> = None;
        let mut last_permission_warning: Option<Instant> = None;
        let mut permission_requested = false;
        let mut auto_started = false;
        let mut suppress_restart_until_absent = false;

        loop {
            tokio::time::sleep(POLL_INTERVAL).await;

            // AppState isn't registered until the DB is initialized, which on first launch is
            // deferred until the user finishes onboarding (see database/setup.rs) — so unlike
            // most command handlers, this loop can legitimately run before it exists.
            let Some(state) = app.try_state::<AppState>() else {
                continue;
            };
            let pool = state.db_manager.pool().clone();

            let enabled = match SettingsRepository::get_auto_detect_meet_enabled(&pool).await {
                Ok(v) => v,
                Err(e) => {
                    warn!("Failed to read auto-detect-meet setting: {}", e);
                    continue;
                }
            };

            let currently_recording = recording_commands::is_recording().await;

            if !enabled {
                consecutive_absent = 0;
                if !currently_recording {
                    auto_started = false;
                }
                continue;
            }

            if !crate::audio::permissions::check_window_title_read_permission() {
                if !permission_requested {
                    permission_requested = true;
                    let granted = tokio::task::spawn_blocking(
                        crate::audio::permissions::request_window_title_read_permission,
                    )
                    .await
                    .unwrap_or(false);
                    if granted {
                        info!("[meet-detect] Screen Recording permission granted");
                    }
                }

                let should_warn = last_permission_warning
                    .is_none_or(|t| t.elapsed() >= PERMISSION_WARNING_INTERVAL);
                if should_warn {
                    warn!(
                        "[meet-detect] auto-detect is on but Screen Recording permission isn't \
                         granted — window titles are invisible, so detection can never fire. \
                         Grant it in System Settings > Privacy & Security > Screen Recording."
                    );
                    last_permission_warning = Some(Instant::now());
                }
                continue;
            }

            let meet_active = tokio::task::spawn_blocking(window_scan::scan_for_active_meet_call)
                .await
                .unwrap_or(false);
            debug!("[meet-detect] poll: meet_active={}", meet_active);

            // A detector-owned recording that is no longer active was stopped manually or by an
            // external path. Do not immediately restart it while the same Meet window remains.
            if auto_started && !currently_recording {
                auto_started = false;
                suppress_restart_until_absent = true;
            }

            if meet_active {
                consecutive_absent = 0;
                if currently_recording && !auto_started {
                    suppress_restart_until_absent = true;
                }
                let cooling_down =
                    last_start_failure.is_some_and(|t| t.elapsed() < AUTO_START_RETRY_COOLDOWN);
                if !currently_recording && !suppress_restart_until_absent && !cooling_down {
                    info!("Detected an active Google Meet call — auto-starting recording");
                    match recording_commands::start_recording_with_meeting_name(app.clone(), None)
                        .await
                    {
                        Ok(()) => {
                            auto_started = true;
                            last_start_failure = None;
                        }
                        Err(e) => {
                            warn!("Auto-start recording failed: {}", e);
                            last_start_failure = Some(Instant::now());
                        }
                    }
                }
                continue;
            }

            suppress_restart_until_absent = false;
            if currently_recording && auto_started {
                consecutive_absent += 1;
                if consecutive_absent >= STOP_DEBOUNCE_POLLS {
                    info!("Google Meet call no longer detected — auto-stopping recording");
                    consecutive_absent = 0;
                    if let Err(e) = recording_commands::stop_recording(
                        app.clone(),
                        RecordingArgs {
                            save_path: String::new(),
                        },
                    )
                    .await
                    {
                        warn!("Auto-stop recording failed: {}", e);
                    } else {
                        auto_started = false;
                        // Unlike the main-window Stop button, detector-driven stops have no
                        // React click handler to run the save pipeline. Mirror the tray stop
                        // path so the frontend persists the transcript and calendar metadata.
                        if let Err(e) = app.emit("recording-stop-complete", true) {
                            warn!("Failed to trigger post-recording save: {}", e);
                        }
                    }
                }
            } else {
                consecutive_absent = 0;
                if !currently_recording {
                    auto_started = false;
                }
            }
        }
    });
}
