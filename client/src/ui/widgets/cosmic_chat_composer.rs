    pub fn ui(
        &mut self,
        ui: &mut egui::Ui,
        hint: &str,
        desired_width: f32,
    ) -> ChatComposerUiResult {
        let mut result = ChatComposerUiResult::default();

        let desired_width = desired_width.max(120.0);
        let content_height = self
            .editor
            .with_buffer(|buffer| (buffer.lines.len() as f32) * LINE_HEIGHT + (PADDING_Y * 2.0));
        let height = content_height.clamp(MIN_HEIGHT, MAX_HEIGHT);

        let (rect, response) = ui.allocate_exact_size(
            egui::vec2(desired_width, height),
            egui::Sense::click_and_drag(),
        );
        let frame_rect = rect;
        ui.painter().rect_filled(frame_rect, 8.0, theme::bg_input());

        if response.clicked() {
            response.request_focus();
        }
        response.context_menu(|ui| {
            let has_selection = self
                .editor
                .copy_selection()
                .map(|text| !text.is_empty())
                .unwrap_or(false);

            if ui
                .add_enabled(has_selection, egui::Button::new("Copy"))
                .clicked()
            {
                if let Some(copied) = self.editor.copy_selection() {
                    ui.ctx().copy_text(copied);
                }
                ui.close();
            }

            if ui.button("Paste").clicked() {
                response.request_focus();
                ui.ctx()
                    .send_viewport_cmd(egui::ViewportCommand::RequestPaste);
                ui.close();
            }
        });

        let has_focus = response.has_focus();
        result.has_focus = has_focus;

        let content_rect = egui::Rect::from_min_max(
            egui::pos2(frame_rect.left() + PADDING_X, frame_rect.top() + PADDING_Y),
            egui::pos2(
                frame_rect.right() - PADDING_X,
                frame_rect.bottom() - PADDING_Y,
            ),
        );

        self.editor.with_buffer_mut(|buffer| {
            buffer.set_size(
                &mut self.font_system,
                Some(content_rect.width().max(1.0)),
                Some(content_rect.height().max(1.0)),
            );
@@ -255,50 +271,56 @@ impl ChatComposer {
                        pressed: true,
                        modifiers,
                        ..
                    } => {
                        let ctrl = modifiers.ctrl || modifiers.command;
                        let shift = modifiers.shift;
                        let action = match key {
                            egui::Key::ArrowLeft => Some(Action::Motion(if ctrl {
                                Motion::PreviousWord
                            } else {
                                Motion::Left
                            })),
                            egui::Key::ArrowRight => Some(Action::Motion(if ctrl {
                                Motion::NextWord
                            } else {
                                Motion::Right
                            })),
                            egui::Key::ArrowUp => Some(Action::Motion(Motion::Up)),
                            egui::Key::ArrowDown => Some(Action::Motion(Motion::Down)),
                            egui::Key::Home => Some(Action::Motion(Motion::Home)),
                            egui::Key::End => Some(Action::Motion(Motion::End)),
                            egui::Key::Backspace => Some(Action::Backspace),
                            egui::Key::Delete => Some(Action::Delete),
                            egui::Key::Escape => Some(Action::Escape),
                            egui::Key::A if ctrl => Some(Action::SelectAll),
                            egui::Key::C if ctrl => {
                                if let Some(copied) = self.editor.copy_selection() {
                                    ui.ctx().copy_text(copied);
                                }
                                None
                            }
                            egui::Key::Enter => {
                                if shift {
                                    Some(Action::Enter)
                                } else {
                                    result.send_requested = true;
                                    None
                                }
                            }
                            _ => None,
                        };

                        if let Some(action) = action {
                            self.editor.action(&mut self.font_system, action);
                            self.dirty = true;
                        }
                    }
                    _ => {}
                }
            }

            if !insert_text.is_empty() {
                for text in insert_text {
                    self.editor.insert_string(&text, None);
                }
                self.dirty = true;
