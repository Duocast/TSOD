use crate::ui::theme;
use cosmic_text::{
    Action, Attrs, Buffer, BufferRef, Color, Edit, Editor, FontSystem, Metrics, Motion,
    PhysicalGlyph, Renderer, Shaping, SwashCache, SwashContent,
};
use eframe::egui;

const FONT_SIZE: f32 = 16.0;
const LINE_HEIGHT: f32 = 20.0;
const PADDING_X: f32 = 10.0;
const PADDING_Y: f32 = 8.0;
const MIN_HEIGHT: f32 = 40.0;
const MAX_HEIGHT: f32 = 96.0;

#[derive(Debug, Clone, Copy)]
pub enum ComposerFormatAction {
    Bold,
    Italic,
    Underline,
    Strikethrough,
    OrderedList,
    UnorderedList,
    Quote,
    CodeBlock,
}

#[derive(Default)]
pub struct ChatComposerUiResult {
    pub send_requested: bool,
    pub has_focus: bool,
}

pub struct ChatComposer {
    font_system: FontSystem,
    swash_cache: SwashCache,
    editor: Editor<'static>,
    texture: Option<egui::TextureHandle>,
    texture_size: [usize; 2],
    dirty: bool,
}

impl ChatComposer {
    pub fn new() -> Self {
        let mut font_system = FontSystem::new();
        let metrics = Metrics::new(FONT_SIZE, LINE_HEIGHT);
        let buffer = Buffer::new(&mut font_system, metrics);
        let editor = Editor::new(BufferRef::Owned(buffer));

        Self {
            font_system,
            swash_cache: SwashCache::new(),
            editor,
            texture: None,
            texture_size: [0, 0],
            dirty: true,
        }
    }

    pub fn set_text(&mut self, text: &str) {
        self.editor.with_buffer_mut(|buffer| {
            buffer.set_text(
                &mut self.font_system,
                text,
                &Attrs::new(),
                Shaping::Advanced,
                None,
            );
        });
        self.editor
            .set_cursor(cosmic_text::Cursor::new(0, text.chars().count()));
        self.editor.set_selection(cosmic_text::Selection::None);
        self.dirty = true;
    }

    pub fn text(&self) -> String {
        self.editor.with_buffer(|buffer| {
            let mut out = String::new();
            for (idx, line) in buffer.lines.iter().enumerate() {
                if idx > 0 {
                    out.push('\n');
                }
                out.push_str(line.text());
            }
            out
        })
    }

    pub fn clear(&mut self) {
        self.set_text("");
    }

    fn select_all(&mut self) {
        let end_cursor = self.editor.with_buffer(|buffer| {
            let last_line = buffer.lines.len().saturating_sub(1);
            let last_col = buffer
                .lines
                .get(last_line)
                .map(|line| line.text().chars().count())
                .unwrap_or(0);
            cosmic_text::Cursor::new(last_line, last_col)
        });

        self.editor.set_cursor(end_cursor);
        self.editor
            .set_selection(cosmic_text::Selection::Normal(cosmic_text::Cursor::new(
                0, 0,
            )));
    }

    pub fn apply_format_action(&mut self, action: ComposerFormatAction) {
        match action {
            ComposerFormatAction::Bold => self.wrap_selection("**", "**"),
            ComposerFormatAction::Italic => self.wrap_selection("*", "*"),
            ComposerFormatAction::Underline => self.wrap_selection("<u>", "</u>"),
            ComposerFormatAction::Strikethrough => self.wrap_selection("~~", "~~"),
            ComposerFormatAction::OrderedList => self.insert_string("1. "),
            ComposerFormatAction::UnorderedList => self.insert_string("- "),
            ComposerFormatAction::Quote => self.insert_string("> "),
            ComposerFormatAction::CodeBlock => self.wrap_selection("```\n", "\n```"),
        }
        self.dirty = true;
    }

    fn insert_string(&mut self, text: &str) {
        self.editor.insert_string(text, None);
    }

    fn wrap_selection(&mut self, prefix: &str, suffix: &str) {
        let Some(selected) = self.editor.copy_selection() else {
            return;
        };
        if selected.is_empty() {
            return;
        }

        self.editor.delete_selection();
        self.insert_string(&format!("{prefix}{selected}{suffix}"));
    }

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
        });

        if response.clicked_by(egui::PointerButton::Primary) {
            if let Some(pos) = response.interact_pointer_pos() {
                let local = pos - content_rect.min;
                self.editor.action(
                    &mut self.font_system,
                    Action::Click {
                        x: local.x as i32,
                        y: local.y as i32,
                    },
                );
                self.dirty = true;
            }
        }

        if response.dragged_by(egui::PointerButton::Primary) {
            if let Some(pos) = ui.ctx().pointer_latest_pos() {
                let local = pos - content_rect.min;
                self.editor.action(
                    &mut self.font_system,
                    Action::Drag {
                        x: local.x as i32,
                        y: local.y as i32,
                    },
                );
                self.dirty = true;
            }
        }

        if response.double_clicked() {
            if let Some(pos) = response.interact_pointer_pos() {
                let local = pos - content_rect.min;
                self.editor.action(
                    &mut self.font_system,
                    Action::DoubleClick {
                        x: local.x as i32,
                        y: local.y as i32,
                    },
                );
                self.dirty = true;
            }
        }

        if response.triple_clicked() {
            if let Some(pos) = response.interact_pointer_pos() {
                let local = pos - content_rect.min;
                self.editor.action(
                    &mut self.font_system,
                    Action::TripleClick {
                        x: local.x as i32,
                        y: local.y as i32,
                    },
                );
                self.dirty = true;
            }
        }

        if has_focus {
            let mut insert_text = Vec::new();
            let events = ui.ctx().input(|i| i.events.clone());
            for event in events {
                match event {
                    egui::Event::Text(text) => {
                        insert_text.push(text);
                    }
                    egui::Event::Paste(text) => {
                        insert_text.push(text);
                    }
                    egui::Event::Copy => {
                        if let Some(copied) = self.editor.copy_selection() {
                            ui.ctx().copy_text(copied);
                        }
                    }
                    egui::Event::Cut => {
                        if let Some(copied) = self.editor.copy_selection() {
                            ui.ctx().copy_text(copied);
                            self.editor.delete_selection();
                            self.dirty = true;
                        }
                    }
                    egui::Event::Key {
                        key,
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
                            egui::Key::A if ctrl => {
                                self.select_all();
                                self.dirty = true;
                                None
                            }
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
            }

            if ui.input(|i| i.modifiers.command && i.key_pressed(egui::Key::C)) {
                if let Some(copied) = self.editor.copy_selection() {
                    ui.ctx().copy_text(copied);
                }
            }
            if ui.input(|i| i.modifiers.command && i.key_pressed(egui::Key::X)) {
                if let Some(copied) = self.editor.copy_selection() {
                    ui.ctx().copy_text(copied);
                    self.editor.delete_selection();
                    self.dirty = true;
                }
            }
        }

        self.editor.shape_as_needed(&mut self.font_system, true);

        let width = content_rect.width().max(1.0).round() as usize;
        let height_px = content_rect.height().max(1.0).round() as usize;

        if self.dirty || self.texture_size != [width, height_px] {
            let mut renderer = SoftwareEguiRenderer::new(
                width,
                height_px,
                &mut self.font_system,
                &mut self.swash_cache,
            );
            let text = to_cosmic_color(theme::text_color());
            let cursor = to_cosmic_color(theme::text_color());
            let selection = to_cosmic_color(theme::COLOR_ACCENT.linear_multiply(0.35));
            let selected_text = to_cosmic_color(theme::text_color());

            self.editor
                .render(&mut renderer, text, cursor, selection, selected_text);

            let image =
                egui::ColorImage::from_rgba_unmultiplied([width, height_px], &renderer.pixels);
            if let Some(texture) = &mut self.texture {
                texture.set(image, egui::TextureOptions::LINEAR);
            } else {
                self.texture = Some(ui.ctx().load_texture(
                    "chat_composer_texture",
                    image,
                    egui::TextureOptions::LINEAR,
                ));
            }
            self.texture_size = [width, height_px];
            self.dirty = false;
        }

        if let Some(texture) = &self.texture {
            ui.painter().image(
                texture.id(),
                content_rect,
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                egui::Color32::WHITE,
            );
        }

        if self.text().is_empty() && !has_focus {
            ui.painter().text(
                egui::pos2(content_rect.left() + 2.0, content_rect.center().y),
                egui::Align2::LEFT_CENTER,
                hint,
                egui::TextStyle::Body.resolve(ui.style()),
                theme::text_muted(),
            );
        }

        result
    }
}

impl Default for ChatComposer {
    fn default() -> Self {
        Self::new()
    }
}

fn to_cosmic_color(color: egui::Color32) -> Color {
    Color::rgba(color.r(), color.g(), color.b(), color.a())
}

struct SoftwareEguiRenderer {
    width: usize,
    height: usize,
    pixels: Vec<u8>,
    font_system: *mut FontSystem,
    swash_cache: *mut SwashCache,
}

impl SoftwareEguiRenderer {
    fn new(
        width: usize,
        height: usize,
        font_system: &mut FontSystem,
        swash_cache: &mut SwashCache,
    ) -> Self {
        Self {
            width,
            height,
            pixels: vec![0; width * height * 4],
            font_system,
            swash_cache,
        }
    }

    fn blend_pixel(&mut self, x: i32, y: i32, src: [u8; 4]) {
        if x < 0 || y < 0 || x >= self.width as i32 || y >= self.height as i32 {
            return;
        }
        let idx = (y as usize * self.width + x as usize) * 4;
        let dst = &mut self.pixels[idx..idx + 4];

        let src_a = src[3] as f32 / 255.0;
        let dst_a = dst[3] as f32 / 255.0;
        let out_a = src_a + dst_a * (1.0 - src_a);
        if out_a <= f32::EPSILON {
            return;
        }

        for i in 0..3 {
            let src_c = src[i] as f32 / 255.0;
            let dst_c = dst[i] as f32 / 255.0;
            let out_c = (src_c * src_a + dst_c * dst_a * (1.0 - src_a)) / out_a;
            dst[i] = (out_c * 255.0).clamp(0.0, 255.0) as u8;
        }
        dst[3] = (out_a * 255.0).clamp(0.0, 255.0) as u8;
    }
}

impl Renderer for SoftwareEguiRenderer {
    fn rectangle(&mut self, x: i32, y: i32, width: u32, height: u32, color: Color) {
        let color = [color.r(), color.g(), color.b(), color.a()];
        let x0 = x;
        let y0 = y;
        let x1 = x.saturating_add(width as i32);
        let y1 = y.saturating_add(height as i32);

        for py in y0..y1 {
            for px in x0..x1 {
                self.blend_pixel(px, py, color);
            }
        }
    }

    fn glyph(&mut self, glyph: PhysicalGlyph, color: Color) {
        let Some(image) = (unsafe { &mut *self.swash_cache })
            .get_image(unsafe { &mut *self.font_system }, glyph.cache_key)
        else {
            return;
        };

        let x = glyph.x + image.placement.left;
        let y = glyph.y - image.placement.top;
        let w = image.placement.width as usize;
        let h = image.placement.height as usize;
        let data: &[u8] = image.data.as_ref();
        let content = image.content;

        match content {
            SwashContent::Mask => {
                for gy in 0..h {
                    for gx in 0..w {
                        let alpha = data[gy * w + gx];
                        if alpha == 0 {
                            continue;
                        }
                        self.blend_pixel(
                            x + gx as i32,
                            y + gy as i32,
                            [
                                color.r(),
                                color.g(),
                                color.b(),
                                ((alpha as u16 * color.a() as u16) / 255) as u8,
                            ],
                        );
                    }
                }
            }
            SwashContent::SubpixelMask => {
                for gy in 0..h {
                    for gx in 0..w {
                        let idx = (gy * w + gx) * 4;
                        let rgba = [
                            ((data[idx] as u16 * color.r() as u16) / 255) as u8,
                            ((data[idx + 1] as u16 * color.g() as u16) / 255) as u8,
                            ((data[idx + 2] as u16 * color.b() as u16) / 255) as u8,
                            ((data[idx + 3] as u16 * color.a() as u16) / 255) as u8,
                        ];
                        self.blend_pixel(x + gx as i32, y + gy as i32, rgba);
                    }
                }
            }
            SwashContent::Color => {
                for gy in 0..h {
                    for gx in 0..w {
                        let idx = (gy * w + gx) * 4;
                        self.blend_pixel(
                            x + gx as i32,
                            y + gy as i32,
                            [data[idx], data[idx + 1], data[idx + 2], data[idx + 3]],
                        );
                    }
                }
            }
        }
    }
}
