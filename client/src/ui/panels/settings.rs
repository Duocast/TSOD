//! Settings panel — TS3-inspired Options dialog with sidebar navigation.
//!
//! Categories: Application, Capture, Playback, Hotkeys, Chat, Downloads,
//!             Notifications, Whisper, Screen Share, Video Call, Security

use crate::settings_io;
use crate::ui::model::{
    AppSettings, AudioDeviceInfo, CaptureMode, FecMode, SettingsPage, UiIntent, UiModel,
};
use crate::ui::theme;
use crossbeam_channel::Sender;
use eframe::egui;
use std::path::PathBuf;

/// Main entry point: renders the full settings window content.
pub fn show(ui: &mut egui::Ui, model: &mut UiModel, tx_intent: &Sender<UiIntent>) {
    ui.horizontal_top(|ui: &mut egui::Ui| {
        // ── Left sidebar: category list ──
        ui.allocate_ui_with_layout(
            egui::vec2(160.0, ui.available_height()),
            egui::Layout::top_down(egui::Align::LEFT),
            |ui: &mut egui::Ui| {
                ui.add_space(4.0);
                for page in SettingsPage::ALL {
                    let selected = model.settings_page == page;
                    let text = egui::RichText::new(page.label()).size(13.0);
                    let text = if selected {
                        text.strong().color(if theme::is_light_mode() {
                            egui::Color32::from_rgb(36, 41, 47)
                        } else {
                            egui::Color32::WHITE
                        })
                    } else {
                        text.color(theme::text_dim())
                    };

                    let btn = egui::Button::new(text)
                        .fill(if selected {
                            theme::COLOR_ACCENT.linear_multiply(0.3)
                        } else {
                            egui::Color32::TRANSPARENT
                        })
                        .corner_radius(4.0)
                        .min_size(egui::vec2(150.0, 28.0));

                    if ui.add(btn).clicked() {
                        model.settings_page = page;
                    }
                }

                ui.add_space(12.0);
                ui.separator();
                ui.add_space(4.0);

                // Apply / Revert buttons
                let dirty = model.settings_dirty;
                ui.horizontal(|ui: &mut egui::Ui| {
                    let apply_btn = egui::Button::new(
                        egui::RichText::new("Apply").size(12.0).color(if dirty {
                            egui::Color32::WHITE
                        } else {
                            theme::text_muted()
                        }),
                    )
                    .fill(if dirty {
                        theme::COLOR_ACCENT
                    } else {
                        theme::muted_button_fill()
                    })
                    .corner_radius(4.0);

                    if ui.add(apply_btn).clicked() && dirty {
                        model.settings = model.settings_draft.clone();
                        model.settings_dirty = false;
                        model.sync_settings_to_runtime();
                        let _ = tx_intent
                            .send(UiIntent::ApplySettings(Box::new(model.settings.clone())));
                        let _ = tx_intent
                            .send(UiIntent::SaveSettings(Box::new(model.settings.clone())));
                        let _ = settings_io::save_settings(&model.settings);
                    }
                });

                if ui.small_button("Revert").clicked() {
                    model.settings_draft = model.settings.clone();
                    model.settings_dirty = false;
                }
            },
        );

        ui.separator();

        // ── Right side: page content ──
        ui.allocate_ui_with_layout(
            egui::vec2(ui.available_width(), ui.available_height()),
            egui::Layout::top_down(egui::Align::LEFT),
            |ui: &mut egui::Ui| {
                egui::ScrollArea::vertical()
                    .id_salt("settings_content")
                    .auto_shrink([false, false])
                    .show(ui, |ui: &mut egui::Ui| {
                        ui.set_min_width(ui.available_width().max(440.0));
                        let dirty = match model.settings_page {
                            SettingsPage::Application => {
                                page_application(ui, &mut model.settings_draft, &model.log)
                            }
                            SettingsPage::Capture => page_capture(
                                ui,
                                &mut model.settings_draft,
                                &model.input_devices,
                                model.loopback_active,
                                model.vad_level,
                                &model.mic_test_waveform,
                                tx_intent,
                            ),
                            SettingsPage::Playback => page_playback(
                                ui,
                                &mut model.settings_draft,
                                &model.output_devices,
                                &model.playback_modes,
                                model.connected,
                                tx_intent,
                            ),
                            SettingsPage::Hotkeys => page_hotkeys(ui, &mut model.settings_draft),
                            SettingsPage::Chat => page_chat(ui, &mut model.settings_draft),
                            SettingsPage::Downloads => {
                                page_downloads(ui, &mut model.settings_draft)
                            }
                            SettingsPage::Notifications => {
                                page_notifications(ui, &mut model.settings_draft)
                            }
                            SettingsPage::Whisper => page_whisper(ui, &mut model.settings_draft),
                            SettingsPage::ScreenShare => {
                                page_screen_share(ui, &mut model.settings_draft)
                            }
                            SettingsPage::VideoCall => {
                                page_video_call(ui, &mut model.settings_draft)
                            }
                            SettingsPage::Security => page_security(ui, &mut model.settings_draft),
                        };
                        if dirty {
                            model.settings_dirty = true;
                        }
                    });
            },
        );
    });
}

fn common_download_directories() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = std::env::var_os("HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("USERPROFILE").map(PathBuf::from))
    {
        dirs.push(home.join("Downloads"));
        dirs.push(home.join("Desktop"));
        dirs.push(home.join("Documents"));
        dirs.push(home);
    }
    dirs.retain(|p| p.exists());
    dirs
}

fn enumerate_video_devices() -> Vec<String> {
    #[cfg(target_os = "linux")]
    {
        let mut devices = Vec::new();
        if let Ok(entries) = std::fs::read_dir("/dev") {
            for entry in entries.flatten() {
                if let Some(name) = entry.file_name().to_str() {
                    if name.starts_with("video") {
                        devices.push(format!("/dev/{name}"));
                    }
                }
            }
        }
        devices.sort();
        devices
    }
    #[cfg(not(target_os = "linux"))]
    {
        Vec::new()
    }
}

// ── Helpers ───────────────────────────────────────────────────────────

fn section(ui: &mut egui::Ui, title: &str) {
    ui.add_space(8.0);
    ui.label(
        egui::RichText::new(title)
            .size(14.0)
            .strong()
            .color(theme::text_color()),
    );
    ui.add_space(2.0);
    ui.separator();
    ui.add_space(4.0);
}

fn hint(ui: &mut egui::Ui, text: &str) {
    ui.label(egui::RichText::new(text).small().color(theme::text_muted()));
}

// ── Application ───────────────────────────────────────────────────────

fn page_application(
    ui: &mut egui::Ui,
    s: &mut AppSettings,
    log: &std::collections::VecDeque<String>,
) -> bool {
    let mut dirty = false;

    section(ui, "General");

    ui.horizontal(|ui: &mut egui::Ui| {
        ui.label("Language:");
        let langs = [
            "English", "German", "French", "Spanish", "Japanese", "Chinese",
        ];
        egui::ComboBox::from_id_salt("app_lang")
            .selected_text(&s.language)
            .width(180.0)
            .show_ui(ui, |ui: &mut egui::Ui| {
                for lang in &langs {
                    if ui
                        .selectable_value(&mut s.language, lang.to_string(), *lang)
                        .changed()
                    {
                        dirty = true;
                    }
                }
            });
    });

    ui.horizontal(|ui: &mut egui::Ui| {
        ui.label("Theme:");
        let themes = ["Dark", "Light", "OLED Black"];
        egui::ComboBox::from_id_salt("app_theme")
            .selected_text(&s.theme)
            .width(180.0)
            .show_ui(ui, |ui: &mut egui::Ui| {
                for t in &themes {
                    if ui
                        .selectable_value(&mut s.theme, t.to_string(), *t)
                        .changed()
                    {
                        dirty = true;
                    }
                }
            });
    });

    ui.horizontal(|ui: &mut egui::Ui| {
        let pct = (s.ui_scale * 100.0).round() as i32;
        ui.label(format!("UI Scale: {pct}%"));
    });
    let prev = s.ui_scale;
    ui.add(
        egui::Slider::new(&mut s.ui_scale, 0.75..=2.0)
            .step_by(0.05)
            .show_value(false),
    );
    if (s.ui_scale - prev).abs() > 0.001 {
        dirty = true;
    }

    section(ui, "Behavior");

    if ui
        .checkbox(&mut s.start_minimized, "Start minimized")
        .changed()
    {
        dirty = true;
    }
    if ui
        .checkbox(&mut s.minimize_to_tray, "Minimize to system tray")
        .changed()
    {
        dirty = true;
    }
    if ui
        .checkbox(&mut s.check_for_updates, "Check for updates on startup")
        .changed()
    {
        dirty = true;
    }

    section(ui, "Debug");

    egui::CollapsingHeader::new("Debug Log")
        .default_open(false)
        .show(ui, |ui: &mut egui::Ui| {
            egui::ScrollArea::vertical()
                .max_height(200.0)
                .stick_to_bottom(true)
                .show(ui, |ui: &mut egui::Ui| {
                    for line in log.iter() {
                        ui.label(
                            egui::RichText::new(line)
                                .small()
                                .monospace()
                                .color(egui::Color32::GRAY),
                        );
                    }
                });
        });

    dirty
}

// ── Capture ───────────────────────────────────────────────────────────

fn page_capture(
    ui: &mut egui::Ui,
    s: &mut AppSettings,
    input_devices: &[AudioDeviceInfo],
    loopback_active: bool,
    vad_level: Option<f32>,
    mic_test_waveform: &[f32],
    tx_intent: &Sender<UiIntent>,
) -> bool {
    let mut dirty = false;

    section(ui, "Capture Device");

    ui.horizontal(|ui: &mut egui::Ui| {
        ui.label("Input Device:");
        let selected_label = if s.capture_device.is_default() {
            "Default (system)".to_string()
        } else {
            input_devices
                .iter()
                .find(|d| d.key == s.capture_device)
                .map(|d| d.display_label.clone())
                .unwrap_or_else(|| format!("Missing device — {}", s.capture_device.id))
        };
        egui::ComboBox::from_id_salt("cap_device")
            .selected_text(selected_label)
            .width(300.0)
            .show_ui(ui, |ui: &mut egui::Ui| {
                if ui
                    .selectable_value(
                        &mut s.capture_device,
                        crate::ui::model::AudioDeviceId::default_input(),
                        "Default (system)",
                    )
                    .changed()
                {
                    dirty = true;
                    let _ = tx_intent.send(UiIntent::SetInputDevice(s.capture_device.clone()));
                }
                for dev in input_devices {
                    if dev.key.is_default() {
                        continue;
                    }
                    if ui
                        .selectable_value(
                            &mut s.capture_device,
                            dev.key.clone(),
                            dev.display_label.as_str(),
                        )
                        .changed()
                    {
                        dirty = true;
                        let _ = tx_intent.send(UiIntent::SetInputDevice(s.capture_device.clone()));
                    }
                }
            });
    });

    hint(
        ui,
        &format!("{} input device(s) detected", input_devices.len()),
    );

    section(ui, "Voice Activation Mode");

    for mode in CaptureMode::ALL {
        let is_current = s.capture_mode == mode;
        if ui.radio(is_current, mode.label()).clicked() && !is_current {
            s.capture_mode = mode;
            dirty = true;
        }
    }

    ui.add_space(4.0);

    match s.capture_mode {
        CaptureMode::PushToTalk => {
            ui.horizontal(|ui: &mut egui::Ui| {
                ui.label("Hotkey:");
                let prev = s.ptt_key.clone();
                ui.add(
                    egui::TextEdit::singleline(&mut s.ptt_key)
                        .desired_width(120.0)
                        .hint_text("Press a key..."),
                );
                if s.ptt_key != prev {
                    dirty = true;
                }
            });
            ui.horizontal(|ui: &mut egui::Ui| {
                ui.label("Release Delay:");
                let prev = s.ptt_delay_ms;
                ui.add(egui::Slider::new(&mut s.ptt_delay_ms, 0..=1000).suffix(" ms"));
                if s.ptt_delay_ms != prev {
                    dirty = true;
                }
            });
            hint(ui, "Audio transmits while hotkey is held. Release delay prevents clipping at end of speech.");
        }
        CaptureMode::VoiceActivation => {
            ui.horizontal(|ui: &mut egui::Ui| {
                ui.label("Sensitivity:");
                let prev = s.vad_threshold;
                ui.add(egui::Slider::new(&mut s.vad_threshold, 0.0..=1.0).step_by(0.05));
                if (s.vad_threshold - prev).abs() > 0.001 {
                    dirty = true;
                    let _ = tx_intent.send(UiIntent::SetVadThreshold(s.vad_threshold));
                }
            });
            hint(
                ui,
                "Lower = more sensitive. Higher = stricter, ignores background noise.",
            );

            // Live VAD meter with threshold marker
            if let Some(vad) = vad_level {
                ui.add_space(4.0);
                ui.horizontal(|ui: &mut egui::Ui| {
                    ui.label("Level:");
                    let bar_width = ui.available_width().min(300.0);
                    let (rect, _) =
                        ui.allocate_exact_size(egui::vec2(bar_width, 14.0), egui::Sense::hover());
                    ui.painter().rect_filled(rect, 3.0, theme::bg_dark());

                    // Threshold marker
                    let thresh_x = rect.left() + bar_width * s.vad_threshold;
                    ui.painter().vline(
                        thresh_x,
                        rect.y_range(),
                        egui::Stroke::new(2.0, theme::COLOR_DANGER),
                    );

                    // Level fill
                    let filled =
                        egui::Rect::from_min_size(rect.min, egui::vec2(bar_width * vad, 14.0));
                    let color = if vad >= s.vad_threshold {
                        theme::COLOR_ONLINE
                    } else {
                        theme::COLOR_IDLE
                    };
                    ui.painter().rect_filled(filled, 3.0, color);
                });
            }
        }
        CaptureMode::Continuous => {
            hint(
                ui,
                "Your microphone is always transmitting when connected to a voice channel.",
            );
        }
    }

    section(ui, "Input Volume");

    ui.horizontal(|ui: &mut egui::Ui| {
        let pct = (s.input_gain * 100.0).round() as i32;
        ui.label(format!("Mic Gain: {pct}%"));
    });
    let prev_gain = s.input_gain;
    ui.add(
        egui::Slider::new(&mut s.input_gain, 0.0..=2.0)
            .step_by(0.01)
            .show_value(false),
    );
    if (s.input_gain - prev_gain).abs() > 0.001 {
        dirty = true;
        let _ = tx_intent.send(UiIntent::SetInputGain(s.input_gain));
    }

    section(ui, "Signal Processing");

    let ns_prev = s.noise_suppression;
    ui.checkbox(&mut s.noise_suppression, "Noise Suppression (RNNoise)");
    if s.noise_suppression != ns_prev {
        dirty = true;
        let _ = tx_intent.send(UiIntent::SetNoiseSuppression(s.noise_suppression));
    }
    hint(
        ui,
        "Neural network noise removal. Recommended for noisy environments.",
    );

    let agc_prev = s.agc_enabled;
    ui.checkbox(&mut s.agc_enabled, "Automatic Gain Control");
    if s.agc_enabled != agc_prev {
        dirty = true;
        let _ = tx_intent.send(UiIntent::SetAgcEnabled(s.agc_enabled));
    }
    hint(ui, "Maintains consistent microphone volume automatically.");

    if s.agc_enabled {
        ui.horizontal(|ui: &mut egui::Ui| {
            ui.label("AGC Target:");
            let prev = s.agc_target_db;
            ui.add(egui::Slider::new(&mut s.agc_target_db, -30.0..=-6.0).suffix(" dBFS"));
            if (s.agc_target_db - prev).abs() > 0.1 {
                dirty = true;
                let _ = tx_intent.send(UiIntent::SetAgcTargetDb(s.agc_target_db));
            }
        });
    }

    if ui
        .checkbox(&mut s.echo_cancellation, "Echo Cancellation")
        .changed()
    {
        dirty = true;
        let _ = tx_intent.send(UiIntent::SetEchoCancellation(s.echo_cancellation));
        let _ = tx_intent.send(UiIntent::SaveSettings(Box::new(s.clone())));
    }
    hint(
        ui,
        "Removes speaker bleed-through. Useful without headphones.",
    );

    if ui
        .checkbox(&mut s.typing_attenuation, "Typing Attenuation")
        .changed()
    {
        dirty = true;
        let _ = tx_intent.send(UiIntent::SetTypingAttenuation(s.typing_attenuation));
    }
    hint(ui, "Reduces keyboard click noise while you type.");

    ui.horizontal(|ui: &mut egui::Ui| {
        ui.label("Forward Error Correction:");
        let prev = s.fec_mode;
        egui::ComboBox::from_id_salt("cap_fec_mode")
            .selected_text(s.fec_mode.label())
            .width(220.0)
            .show_ui(ui, |ui: &mut egui::Ui| {
                for mode in FecMode::ALL {
                    ui.selectable_value(&mut s.fec_mode, mode, mode.label());
                }
            });
        if s.fec_mode != prev {
            dirty = true;
            let _ = tx_intent.send(UiIntent::SetFecMode(s.fec_mode));
        }
    });

    if s.fec_mode != FecMode::Off {
        ui.horizontal(|ui: &mut egui::Ui| {
            let pct = s.fec_strength.clamp(0, 100);
            ui.label(format!("FEC Strength: {pct}%"));
        });
        let prev = s.fec_strength;
        ui.add(
            egui::Slider::new(&mut s.fec_strength, 0..=100)
                .show_value(false)
                .suffix("%"),
        );
        if s.fec_strength != prev {
            dirty = true;
            let _ = tx_intent.send(UiIntent::SetFecStrength(s.fec_strength));
        }
    }

    section(ui, "Mic Test");

    let btn_text = if loopback_active {
        "End Test"
    } else {
        "Begin Test"
    };
    let btn_color = if loopback_active {
        theme::COLOR_DANGER
    } else {
        theme::COLOR_ACCENT
    };

    if ui
        .add(
            egui::Button::new(
                egui::RichText::new(btn_text)
                    .color(egui::Color32::WHITE)
                    .strong(),
            )
            .fill(btn_color)
            .min_size(egui::vec2(160.0, 28.0))
            .corner_radius(4.0),
        )
        .clicked()
    {
        let _ = tx_intent.send(UiIntent::ToggleLoopback);
    }
    hint(
        ui,
        "Runs a live microphone test with loopback and waveform visualization.",
    );

    if loopback_active {
        if let Some(vad) = vad_level {
            let bar_width = ui.available_width().min(300.0);
            let (rect, _) =
                ui.allocate_exact_size(egui::vec2(bar_width, 10.0), egui::Sense::hover());
            ui.painter().rect_filled(rect, 3.0, theme::bg_dark());
            let filled = egui::Rect::from_min_size(rect.min, egui::vec2(bar_width * vad, 10.0));
            let color = if vad > 0.7 {
                theme::COLOR_DANGER
            } else if vad > 0.3 {
                theme::COLOR_ONLINE
            } else {
                theme::COLOR_IDLE
            };
            ui.painter().rect_filled(filled, 3.0, color);
        }

        ui.add_space(6.0);
        draw_mic_test_waveform(ui, mic_test_waveform);
        ui.label(
            egui::RichText::new("Mic test active - speak to see your waveform")
                .small()
                .color(theme::COLOR_ONLINE),
        );
    }

    dirty
}

fn draw_mic_test_waveform(ui: &mut egui::Ui, samples: &[f32]) {
    let width = ui.available_width().min(420.0).max(220.0);
    let height = 110.0;
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::hover());

    ui.painter().rect_filled(rect, 4.0, theme::bg_dark());

    if samples.is_empty() {
        ui.painter().text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "No input detected yet...",
            egui::FontId::proportional(12.0),
            theme::text_dim(),
        );
        return;
    }

    let mid = rect.center().y;
    ui.painter().line_segment(
        [egui::pos2(rect.left(), mid), egui::pos2(rect.right(), mid)],
        egui::Stroke::new(1.0, theme::bg_light()),
    );

    let count = samples.len().max(2);
    let step_x = rect.width() / (count as f32 - 1.0);
    let amp = rect.height() * 0.45;

    let points: Vec<egui::Pos2> = samples
        .iter()
        .enumerate()
        .map(|(i, level)| {
            let x = rect.left() + step_x * i as f32;
            let y = mid - level.clamp(-1.0, 1.0) * amp;
            egui::pos2(x, y)
        })
        .collect();

    ui.painter().add(egui::Shape::line(
        points,
        egui::Stroke::new(2.0, theme::COLOR_ACCENT),
    ));
}

// ── Playback ──────────────────────────────────────────────────────────

fn page_playback(
    ui: &mut egui::Ui,
    s: &mut AppSettings,
    output_devices: &[AudioDeviceInfo],
    playback_modes: &[String],
    connected: bool,
    tx_intent: &Sender<UiIntent>,
) -> bool {
    let mut dirty = false;

    section(ui, "Playback Device");

    ui.horizontal(|ui: &mut egui::Ui| {
        ui.label("Output Device:");
        let selected_label = if s.playback_device.is_default() {
            "Default (system)".to_string()
        } else {
            output_devices
                .iter()
                .find(|d| d.key == s.playback_device)
                .map(|d| d.display_label.clone())
                .unwrap_or_else(|| format!("Missing device — {}", s.playback_device.id))
        };
        egui::ComboBox::from_id_salt("play_device")
            .selected_text(selected_label)
            .width(300.0)
            .show_ui(ui, |ui: &mut egui::Ui| {
                if ui
                    .selectable_value(
                        &mut s.playback_device,
                        crate::ui::model::AudioDeviceId::default_output(),
                        "Default (system)",
                    )
                    .changed()
                {
                    dirty = true;
                    let _ = tx_intent.send(UiIntent::SetOutputDevice(s.playback_device.clone()));
                }
                for dev in output_devices {
                    if dev.key.is_default() {
                        continue;
                    }
                    if ui
                        .selectable_value(
                            &mut s.playback_device,
                            dev.key.clone(),
                            dev.display_label.as_str(),
                        )
                        .changed()
                    {
                        dirty = true;
                        let _ =
                            tx_intent.send(UiIntent::SetOutputDevice(s.playback_device.clone()));
                    }
                }
            });
    });

    hint(
        ui,
        &format!("{} output device(s) detected", output_devices.len()),
    );

    ui.horizontal(|ui: &mut egui::Ui| {
        ui.label("Playback Mode:");
        egui::ComboBox::from_id_salt("playback_mode")
            .selected_text(&s.playback_mode)
            .width(300.0)
            .show_ui(ui, |ui: &mut egui::Ui| {
                let mut changed = false;
                for mode in playback_modes {
                    if ui
                        .selectable_value(&mut s.playback_mode, mode.clone(), mode.as_str())
                        .changed()
                    {
                        changed = true;
                    }
                }
                if changed {
                    dirty = true;
                    let _ = tx_intent.send(UiIntent::SetPlaybackMode(s.playback_mode.clone()));
                }
            });
    });
    hint(
        ui,
        "Playback mode options are detected automatically for this client.",
    );

    section(ui, "Volume");

    ui.horizontal(|ui: &mut egui::Ui| {
        let pct = (s.output_gain * 100.0).round() as i32;
        ui.label(format!("Master Volume: {pct}%"));
    });
    let prev = s.output_gain;
    ui.add(
        egui::Slider::new(&mut s.output_gain, 0.0..=2.0)
            .step_by(0.01)
            .show_value(false),
    );
    if (s.output_gain - prev).abs() > 0.001 {
        dirty = true;
        let _ = tx_intent.send(UiIntent::SetOutputGain(s.output_gain));
    }

    if ui.small_button("Reset to 100%").clicked() {
        s.output_gain = 1.0;
        dirty = true;
        let _ = tx_intent.send(UiIntent::SetOutputGain(1.0));
    }

    section(ui, "Sound Processing");

    if ui
        .checkbox(
            &mut s.output_auto_level,
            "Auto-level (normalize loud/quiet users)",
        )
        .changed()
    {
        dirty = true;
        let _ = tx_intent.send(UiIntent::SetOutputAutoLevel(s.output_auto_level));
    }
    hint(
        ui,
        "Adjusts volume per-user so everyone sounds equally loud.",
    );

    if ui
        .checkbox(&mut s.mono_expansion, "Mono-to-stereo expansion")
        .changed()
    {
        dirty = true;
        let _ = tx_intent.send(UiIntent::SetMonoExpansion(s.mono_expansion));
    }
    hint(
        ui,
        "Expands mono voice audio to fill both headphone channels.",
    );

    section(ui, "Comfort Noise");

    if ui
        .checkbox(&mut s.comfort_noise, "Enable comfort noise")
        .changed()
    {
        dirty = true;
        let _ = tx_intent.send(UiIntent::SetComfortNoise(s.comfort_noise));
    }
    hint(
        ui,
        "Adds subtle background noise to prevent dead silence when no one is speaking.",
    );

    if s.comfort_noise {
        ui.horizontal(|ui: &mut egui::Ui| {
            ui.label("Level:");
            let prev = s.comfort_noise_level;
            ui.add(egui::Slider::new(&mut s.comfort_noise_level, 0.0..=0.1).step_by(0.005));
            if (s.comfort_noise_level - prev).abs() > 0.0001 {
                dirty = true;
                let _ = tx_intent.send(UiIntent::SetComfortNoiseLevel(s.comfort_noise_level));
            }
        });
    }

    section(ui, "Audio Ducking");

    if ui
        .checkbox(
            &mut s.ducking_enabled,
            "Duck other audio while receiving voice",
        )
        .changed()
    {
        dirty = true;
        let _ = tx_intent.send(UiIntent::SetDuckingEnabled(s.ducking_enabled));
    }
    hint(
        ui,
        "Lower volume of other apps (music, games) while someone speaks.",
    );

    if s.ducking_enabled {
        ui.horizontal(|ui: &mut egui::Ui| {
            ui.label("Ducking Amount:");
            let prev = s.ducking_attenuation_db;
            ui.add(egui::Slider::new(&mut s.ducking_attenuation_db, -40..=0).suffix(" dB"));
            if s.ducking_attenuation_db != prev {
                dirty = true;
                let _ = tx_intent.send(UiIntent::SetDuckingAttenuationDb(s.ducking_attenuation_db));
            }
        });
    }

    section(ui, "Connection");

    ui.horizontal(|ui: &mut egui::Ui| {
        ui.label("Status:");
        if connected {
            ui.colored_label(egui::Color32::GREEN, "Connected");
        } else {
            ui.colored_label(egui::Color32::RED, "Disconnected");
        }
    });

    dirty
}

// ── Hotkeys ───────────────────────────────────────────────────────────

fn page_hotkeys(ui: &mut egui::Ui, s: &mut AppSettings) -> bool {
    let mut dirty = false;

    section(ui, "Keyboard Shortcuts");

    hint(
        ui,
        "Configure keyboard shortcuts for common actions. Changes take effect after Apply.",
    );

    ui.add_space(8.0);

    // Table header
    egui::Grid::new("hotkey_grid")
        .num_columns(3)
        .spacing([20.0, 6.0])
        .striped(true)
        .show(ui, |ui: &mut egui::Ui| {
            ui.label(egui::RichText::new("Action").strong().size(12.0));
            ui.label(egui::RichText::new("Shortcut").strong().size(12.0));
            ui.label(egui::RichText::new("On").strong().size(12.0));
            ui.end_row();

            for binding in s.hotkeys.iter_mut() {
                ui.label(
                    egui::RichText::new(binding.action.label())
                        .size(12.0)
                        .color(theme::text_color()),
                );

                let prev_key = binding.key.clone();
                ui.add(
                    egui::TextEdit::singleline(&mut binding.key)
                        .desired_width(140.0)
                        .hint_text("Key combo"),
                );
                if binding.key != prev_key {
                    dirty = true;
                }

                let prev_enabled = binding.enabled;
                ui.checkbox(&mut binding.enabled, "");
                if binding.enabled != prev_enabled {
                    dirty = true;
                }

                ui.end_row();
            }
        });

    ui.add_space(12.0);

    if ui.small_button("Reset to Defaults").clicked() {
        s.hotkeys = crate::ui::model::default_hotkeys();
        dirty = true;
    }

    dirty
}

// ── Chat ──────────────────────────────────────────────────────────────

fn page_chat(ui: &mut egui::Ui, s: &mut AppSettings) -> bool {
    let mut dirty = false;

    section(ui, "Chat Display");

    if ui
        .checkbox(&mut s.chat_show_timestamps, "Show timestamps")
        .changed()
    {
        dirty = true;
    }
    if ui
        .checkbox(&mut s.chat_show_join_leave, "Show join/leave messages")
        .changed()
    {
        dirty = true;
    }

    ui.horizontal(|ui: &mut egui::Ui| {
        ui.label("Font Size:");
        let prev = s.chat_font_size;
        ui.add(
            egui::Slider::new(&mut s.chat_font_size, 10.0..=20.0)
                .step_by(0.5)
                .suffix(" px"),
        );
        if (s.chat_font_size - prev).abs() > 0.1 {
            dirty = true;
        }
    });

    ui.horizontal(|ui: &mut egui::Ui| {
        ui.label("Max Chat Lines:");
        let prev = s.chat_max_lines;
        ui.add(egui::Slider::new(&mut s.chat_max_lines, 100..=5000).step_by(100.0));
        if s.chat_max_lines != prev {
            dirty = true;
        }
    });

    section(ui, "Chat Logging");

    if ui
        .checkbox(&mut s.chat_log_to_file, "Log chat messages to file")
        .changed()
    {
        dirty = true;
    }

    if s.chat_log_to_file {
        ui.horizontal(|ui: &mut egui::Ui| {
            ui.label("Log Directory:");
            let prev = s.chat_log_directory.clone();
            ui.add(
                egui::TextEdit::singleline(&mut s.chat_log_directory)
                    .desired_width(250.0)
                    .hint_text("/path/to/logs"),
            );
            if s.chat_log_directory != prev {
                dirty = true;
            }
        });
        hint(
            ui,
            "Chat messages saved as daily text files in this directory.",
        );
    }

    section(ui, "Media Sharing");

    hint(ui, "Drag and drop files into the chat window to share. Images and videos show inline previews.");

    ui.add_space(4.0);
    ui.label(
        egui::RichText::new("Supported formats:")
            .small()
            .strong()
            .color(theme::text_color()),
    );
    ui.label(
        egui::RichText::new("  Images: PNG, JPEG, GIF, WebP")
            .small()
            .color(theme::text_dim()),
    );
    ui.label(
        egui::RichText::new("  Videos: MP4, WebM")
            .small()
            .color(theme::text_dim()),
    );
    ui.label(
        egui::RichText::new("  Files: Any (up to configured max size)")
            .small()
            .color(theme::text_dim()),
    );

    dirty
}

// ── Downloads ─────────────────────────────────────────────────────────

fn page_downloads(ui: &mut egui::Ui, s: &mut AppSettings) -> bool {
    let mut dirty = false;
    let common_dirs = common_download_directories();

    section(ui, "File Downloads");

    ui.horizontal(|ui: &mut egui::Ui| {
        ui.label("Download Directory:");
        egui::ComboBox::from_id_salt("download_directory")
            .selected_text(if s.download_directory.is_empty() {
                "(default: OS Downloads folder)"
            } else {
                &s.download_directory
            })
            .width(360.0)
            .show_ui(ui, |ui: &mut egui::Ui| {
                if ui
                    .selectable_value(
                        &mut s.download_directory,
                        String::new(),
                        "(default: OS Downloads folder)",
                    )
                    .changed()
                {
                    dirty = true;
                }
                for path in &common_dirs {
                    let path_text = path.to_string_lossy().to_string();
                    if ui
                        .selectable_value(&mut s.download_directory, path_text.clone(), path_text)
                        .changed()
                    {
                        dirty = true;
                    }
                }
            });
    });
    hint(
        ui,
        "Choose a common folder, or leave as default to use your OS Downloads directory.",
    );

    ui.horizontal(|ui: &mut egui::Ui| {
        ui.label("Max Download Size:");
        let prev = s.max_download_size_mb;
        ui.add(egui::Slider::new(&mut s.max_download_size_mb, 1..=1000).suffix(" MB"));
        if s.max_download_size_mb != prev {
            dirty = true;
        }
    });

    section(ui, "Auto-Download");

    if ui
        .checkbox(
            &mut s.auto_download_images,
            "Auto-download images (for inline preview)",
        )
        .changed()
    {
        dirty = true;
    }
    if ui
        .checkbox(&mut s.auto_download_files, "Auto-download all files")
        .changed()
    {
        dirty = true;
    }
    hint(
        ui,
        "Warning: auto-download for all files may use significant bandwidth and disk space.",
    );

    dirty
}

// ── Notifications ─────────────────────────────────────────────────────

fn page_notifications(ui: &mut egui::Ui, s: &mut AppSettings) -> bool {
    let mut dirty = false;

    section(ui, "Event Notifications");

    if ui
        .checkbox(&mut s.notify_user_joined, "User joined your channel")
        .changed()
    {
        dirty = true;
    }
    if ui
        .checkbox(&mut s.notify_user_left, "User left your channel")
        .changed()
    {
        dirty = true;
    }
    if ui.checkbox(&mut s.notify_poke, "Poke received").changed() {
        dirty = true;
    }
    if ui
        .checkbox(&mut s.notify_chat_message, "Chat message received")
        .changed()
    {
        dirty = true;
    }

    section(ui, "Sound");

    ui.horizontal(|ui: &mut egui::Ui| {
        ui.label("Sound Pack:");
        let packs = ["Default", "Minimal", "Classic", "Silent"];
        egui::ComboBox::from_id_salt("sound_pack")
            .selected_text(&s.sound_pack)
            .width(180.0)
            .show_ui(ui, |ui: &mut egui::Ui| {
                for pack in &packs {
                    if ui
                        .selectable_value(&mut s.sound_pack, pack.to_string(), *pack)
                        .changed()
                    {
                        dirty = true;
                    }
                }
            });
    });

    ui.horizontal(|ui: &mut egui::Ui| {
        let pct = (s.notification_volume * 100.0).round() as i32;
        ui.label(format!("Volume: {pct}%"));
    });
    let prev = s.notification_volume;
    ui.add(
        egui::Slider::new(&mut s.notification_volume, 0.0..=1.0)
            .step_by(0.05)
            .show_value(false),
    );
    if (s.notification_volume - prev).abs() > 0.001 {
        dirty = true;
    }

    dirty
}

// ── Whisper ───────────────────────────────────────────────────────────

fn page_whisper(ui: &mut egui::Ui, s: &mut AppSettings) -> bool {
    let mut dirty = false;

    section(ui, "Whisper Permissions");

    let prev = s.whisper_allow_all;
    ui.radio_value(
        &mut s.whisper_allow_all,
        true,
        "Allow whispers from everyone",
    );
    ui.radio_value(
        &mut s.whisper_allow_all,
        false,
        "Only allow whispers from specific users",
    );
    if s.whisper_allow_all != prev {
        dirty = true;
    }

    if !s.whisper_allow_all {
        ui.add_space(4.0);
        hint(ui, "Allowed users:");

        let mut to_remove = None;
        for (i, user) in s.whisper_allowed_users.iter().enumerate() {
            ui.horizontal(|ui: &mut egui::Ui| {
                ui.label(egui::RichText::new(user).monospace());
                if ui.small_button("x").clicked() {
                    to_remove = Some(i);
                }
            });
        }
        if let Some(i) = to_remove {
            s.whisper_allowed_users.remove(i);
            dirty = true;
        }

        if ui.small_button("+ Add user").clicked() {
            s.whisper_allowed_users.push(String::new());
            dirty = true;
        }
    }

    section(ui, "Whisper Notifications");

    if ui
        .checkbox(
            &mut s.whisper_notify,
            "Show notification when receiving a whisper",
        )
        .changed()
    {
        dirty = true;
    }

    hint(
        ui,
        "Whisper sends a private voice message directly to a user, bypassing the channel.",
    );

    dirty
}

// ── Screen Share (modern) ─────────────────────────────────────────────

fn page_screen_share(ui: &mut egui::Ui, s: &mut AppSettings) -> bool {
    let mut dirty = false;

    section(ui, "Screen Sharing");

    hint(
        ui,
        "Uses QUIC DATAGRAM for low-latency delivery with simulcast layers and adaptive bitrate.",
    );

    ui.add_space(4.0);

    ui.horizontal(|ui: &mut egui::Ui| {
        ui.label("Frame Rate:");
        let prev = s.screen_share_fps;
        ui.add(egui::Slider::new(&mut s.screen_share_fps, 5..=60).suffix(" fps"));
        if s.screen_share_fps != prev {
            dirty = true;
        }
    });

    ui.horizontal(|ui: &mut egui::Ui| {
        ui.label("Max Bitrate:");
        let prev = s.screen_share_max_bitrate_kbps;
        ui.add(
            egui::Slider::new(&mut s.screen_share_max_bitrate_kbps, 500..=10000).suffix(" kbps"),
        );
        if s.screen_share_max_bitrate_kbps != prev {
            dirty = true;
        }
    });
    hint(ui, "Higher bitrate = better quality. Adaptive bitrate reduces quality if network is constrained.");

    ui.horizontal(|ui: &mut egui::Ui| {
        ui.label("Codec:");
        let codecs = ["H264", "VP9", "AV1"];
        egui::ComboBox::from_id_salt("ss_codec")
            .selected_text(&s.screen_share_codec)
            .width(120.0)
            .show_ui(ui, |ui: &mut egui::Ui| {
                for c in &codecs {
                    if ui
                        .selectable_value(&mut s.screen_share_codec, c.to_string(), *c)
                        .changed()
                    {
                        dirty = true;
                    }
                }
            });
    });

    if ui
        .checkbox(
            &mut s.screen_share_capture_audio,
            "Capture system audio with screen share",
        )
        .changed()
    {
        dirty = true;
    }

    section(ui, "Simulcast & Adaptive Bitrate");

    hint(
        ui,
        "TSOD transmits multiple resolution layers simultaneously. Receivers automatically \
              select the best layer based on available bandwidth.",
    );

    ui.add_space(4.0);
    ui.label(
        egui::RichText::new("Layers:")
            .small()
            .strong()
            .color(theme::text_color()),
    );
    ui.label(
        egui::RichText::new("  High: Full resolution at configured bitrate")
            .small()
            .color(theme::text_dim()),
    );
    ui.label(
        egui::RichText::new("  Medium: 1/2 resolution at 1/2 bitrate")
            .small()
            .color(theme::text_dim()),
    );
    ui.label(
        egui::RichText::new("  Low: 1/4 resolution at 1/4 bitrate (thumbnail)")
            .small()
            .color(theme::text_dim()),
    );

    dirty
}

// ── Video Call (modern) ───────────────────────────────────────────────

fn page_video_call(ui: &mut egui::Ui, s: &mut AppSettings) -> bool {
    let mut dirty = false;
    let devices = enumerate_video_devices();

    section(ui, "Camera");

    ui.horizontal(|ui: &mut egui::Ui| {
        ui.label("Video Device:");
        egui::ComboBox::from_id_salt("vid_device")
            .selected_text(&s.video_device)
            .width(300.0)
            .show_ui(ui, |ui: &mut egui::Ui| {
                if ui
                    .selectable_value(
                        &mut s.video_device,
                        "(system default)".to_string(),
                        "(system default)",
                    )
                    .changed()
                {
                    dirty = true;
                }
                for d in &devices {
                    if ui
                        .selectable_value(&mut s.video_device, d.clone(), d)
                        .changed()
                    {
                        dirty = true;
                    }
                }
            });
    });
    hint(ui, &format!("{} camera device(s) detected", devices.len()));

    section(ui, "Video Quality");

    ui.horizontal(|ui: &mut egui::Ui| {
        ui.label("Resolution:");
        let resolutions = ["480p", "720p", "1080p"];
        egui::ComboBox::from_id_salt("vid_res")
            .selected_text(&s.video_resolution)
            .width(120.0)
            .show_ui(ui, |ui: &mut egui::Ui| {
                for r in &resolutions {
                    if ui
                        .selectable_value(&mut s.video_resolution, r.to_string(), *r)
                        .changed()
                    {
                        dirty = true;
                    }
                }
            });
    });

    ui.horizontal(|ui: &mut egui::Ui| {
        ui.label("Frame Rate:");
        let prev = s.video_fps;
        ui.add(egui::Slider::new(&mut s.video_fps, 15..=60).suffix(" fps"));
        if s.video_fps != prev {
            dirty = true;
        }
    });

    ui.horizontal(|ui: &mut egui::Ui| {
        ui.label("Max Bitrate:");
        let prev = s.video_max_bitrate_kbps;
        ui.add(egui::Slider::new(&mut s.video_max_bitrate_kbps, 200..=5000).suffix(" kbps"));
        if s.video_max_bitrate_kbps != prev {
            dirty = true;
        }
    });
    hint(
        ui,
        "Video calls use QUIC DATAGRAM for 1:1 and small group calls with adaptive bitrate.",
    );

    section(ui, "Video Transport");

    hint(
        ui,
        "TSOD uses QUIC DATAGRAM (not WebRTC) for all video transport. \
              Lower latency and better congestion control than traditional approaches.",
    );

    dirty
}

// ── Security ──────────────────────────────────────────────────────────

fn page_security(ui: &mut egui::Ui, s: &mut AppSettings) -> bool {
    let mut dirty = false;

    section(ui, "Identity");
    hint(
        ui,
        "Nickname is configured from the Connections window and used when connecting/joining channels.",
    );

    section(ui, "Connection");

    if ui
        .checkbox(
            &mut s.auto_connect,
            "Auto-connect to last server on startup",
        )
        .changed()
    {
        dirty = true;
    }
    if ui
        .checkbox(&mut s.auto_reconnect, "Auto-reconnect on disconnect")
        .changed()
    {
        dirty = true;
    }

    if s.auto_reconnect {
        ui.horizontal(|ui: &mut egui::Ui| {
            ui.label("Reconnect Delay:");
            let prev = s.reconnect_delay_sec;
            ui.add(egui::Slider::new(&mut s.reconnect_delay_sec, 1..=60).suffix(" sec"));
            if s.reconnect_delay_sec != prev {
                dirty = true;
            }
        });
    }

    section(ui, "Encryption");

    hint(
        ui,
        "All voice and control data encrypted in transit using QUIC/TLS 1.3.",
    );

    ui.add_space(4.0);
    ui.label(
        egui::RichText::new("TLS Status:")
            .small()
            .strong()
            .color(theme::text_color()),
    );
    ui.label(
        egui::RichText::new("  Transport: TLS 1.3 via QUIC (always on)")
            .small()
            .color(theme::COLOR_ONLINE),
    );
    ui.label(
        egui::RichText::new("  Voice E2EE: Available (per-channel)")
            .small()
            .color(theme::text_dim()),
    );
    ui.label(
        egui::RichText::new("  Video E2EE: Available (per-channel)")
            .small()
            .color(theme::text_dim()),
    );

    section(ui, "Certificate");

    hint(
        ui,
        "Server TLS certificate validated via CA cert (--ca-cert-pem) or \
              certificate pinning (VP_TLS_PIN_SHA256_HEX). Dev mode accepts all certificates.",
    );

    dirty
}
