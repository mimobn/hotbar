//! The built-in picker window (egui).
//!
//! Keys:
//!   type          search
//!   Up / Down     move
//!   Enter         paste the selected entry
//!   1 to 9        paste that slot (while the search box is empty)
//!   Alt+digit     put the selected entry in that slot
//!   Delete        delete the selected entry (Shift+Delete while searching)
//!   Esc           close

use std::collections::HashMap;

use anyhow::{anyhow, Result};
use eframe::egui::{self, Color32, Key, Modifiers, RichText, Sense, TextureHandle};

use crate::config::{parse_color, Config, Theme};
use crate::content::Kind;
use crate::ipc::{self, Request, Response, SlotInfo};
use crate::store::EntryMeta;

pub const TITLE: &str = "hotbar picker";
const PASTE_DELAY_MS: u64 = 120;

fn color(s: &str, fallback: Color32) -> Color32 {
    parse_color(s)
        .map(|[r, g, b, a]| Color32::from_rgba_unmultiplied(r, g, b, a))
        .unwrap_or(fallback)
}

struct Colors {
    bg: Color32,
    surface: Color32,
    fg: Color32,
    muted: Color32,
    accent: Color32,
}

impl Colors {
    fn from(t: &Theme) -> Colors {
        Colors {
            bg: color(&t.background, Color32::from_rgb(30, 30, 46)),
            surface: color(&t.surface, Color32::from_rgb(49, 50, 68)),
            fg: color(&t.foreground, Color32::from_rgb(205, 214, 244)),
            muted: color(&t.muted, Color32::from_rgb(127, 132, 156)),
            accent: color(&t.accent, Color32::from_rgb(137, 180, 250)),
        }
    }
}

struct Row {
    meta: EntryMeta,
    slot: Option<u8>,
}

enum Action {
    PasteEntry(u64),
    PasteSlot(u8),
    Assign(u8, u64),
    Delete(u64),
    Close,
}

struct App {
    c: Colors,
    radius: f32,
    font_size: f32,
    query: String,
    slots: Vec<SlotInfo>,
    entries: Vec<EntryMeta>,
    selected: usize,
    scroll_to_selected: bool,
    thumbs: HashMap<u64, Option<TextureHandle>>,
    return_to: isize,
    close_on_unfocus: bool,
    had_focus: bool,
    frames: u32,
    status: Option<String>,
}

impl App {
    fn rows(&self) -> Vec<Row> {
        let q = self.query.to_lowercase();
        let hit = |m: &EntryMeta| q.is_empty() || m.preview.to_lowercase().contains(&q);
        let mut rows = Vec::new();
        let mut in_slot = Vec::new();
        for s in &self.slots {
            if let Some(m) = &s.entry {
                in_slot.push(m.id);
                if hit(m) {
                    rows.push(Row {
                        meta: m.clone(),
                        slot: Some(s.n),
                    });
                }
            }
        }
        for m in &self.entries {
            if !in_slot.contains(&m.id) && hit(m) {
                rows.push(Row {
                    meta: m.clone(),
                    slot: None,
                });
            }
        }
        rows
    }

    fn reload(&mut self) {
        match super::fetch() {
            Ok((s, e)) => {
                self.slots = s;
                self.entries = e;
            }
            Err(e) => self.status = Some(e.to_string()),
        }
    }

    fn thumb(&mut self, ctx: &egui::Context, id: u64) -> Option<TextureHandle> {
        if let Some(t) = self.thumbs.get(&id) {
            return t.clone();
        }
        let tex = (|| {
            let Ok(Response::Data { data, .. }) = ipc::send(&Request::Get { id }) else {
                return None;
            };
            let bytes = ipc::decode(&data).ok()?;
            let img = image::load_from_memory(&bytes)
                .ok()?
                .thumbnail(160, 48)
                .to_rgba8();
            let (w, h) = img.dimensions();
            let ci =
                egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], img.as_raw());
            Some(ctx.load_texture(format!("thumb-{id}"), ci, egui::TextureOptions::LINEAR))
        })();
        self.thumbs.insert(id, tex.clone());
        tex
    }

    fn apply(&mut self, ctx: &egui::Context, a: Action) {
        let close = |ctx: &egui::Context, to: isize| {
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            crate::system::focus_window(to);
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        };
        match a {
            Action::PasteEntry(id) => {
                let r = ipc::send(&Request::PasteEntry {
                    id,
                    delay_ms: PASTE_DELAY_MS,
                });
                if let Err(e) = r.and_then(|r| r.into_result()) {
                    self.status = Some(e.to_string());
                    return;
                }
                close(ctx, self.return_to);
            }
            Action::PasteSlot(n) => {
                let r = ipc::send(&Request::PasteSlot {
                    n,
                    delay_ms: PASTE_DELAY_MS,
                });
                if let Err(e) = r.and_then(|r| r.into_result()) {
                    self.status = Some(e.to_string());
                    return;
                }
                close(ctx, self.return_to);
            }
            Action::Assign(n, id) => {
                let r = ipc::send(&Request::SetSlotEntry { n, id }).and_then(|r| r.into_result());
                self.status = Some(match r {
                    Ok(_) => format!("pinned to slot {n}"),
                    Err(e) => e.to_string(),
                });
                self.reload();
            }
            Action::Delete(id) => {
                let _ = ipc::send(&Request::Delete { id });
                self.thumbs.remove(&id);
                self.reload();
            }
            Action::Close => close(ctx, self.return_to),
        }
    }

    fn keys(&mut self, ctx: &egui::Context, rows: &[Row]) -> Option<Action> {
        let digits = [
            Key::Num1,
            Key::Num2,
            Key::Num3,
            Key::Num4,
            Key::Num5,
            Key::Num6,
            Key::Num7,
            Key::Num8,
            Key::Num9,
        ];
        ctx.input_mut(|i| {
            if i.consume_key(Modifiers::NONE, Key::Escape) {
                return Some(Action::Close);
            }
            if i.consume_key(Modifiers::NONE, Key::ArrowDown) && !rows.is_empty() {
                self.selected = (self.selected + 1).min(rows.len() - 1);
                self.scroll_to_selected = true;
            }
            if i.consume_key(Modifiers::NONE, Key::ArrowUp) {
                self.selected = self.selected.saturating_sub(1);
                self.scroll_to_selected = true;
            }
            for (idx, k) in digits.iter().enumerate() {
                let n = idx as u8 + 1;
                if i.consume_key(Modifiers::ALT, *k) {
                    if let Some(r) = rows.get(self.selected) {
                        return Some(Action::Assign(n, r.meta.id));
                    }
                }
                if self.query.is_empty() && i.consume_key(Modifiers::NONE, *k) {
                    return Some(Action::PasteSlot(n));
                }
            }
            if i.consume_key(Modifiers::NONE, Key::Enter) {
                if let Some(r) = rows.get(self.selected) {
                    return Some(Action::PasteEntry(r.meta.id));
                }
            }
            let del = (self.query.is_empty() && i.consume_key(Modifiers::NONE, Key::Delete))
                || i.consume_key(Modifiers::SHIFT, Key::Delete);
            if del {
                if let Some(r) = rows.get(self.selected) {
                    return Some(Action::Delete(r.meta.id));
                }
            }
            None
        })
    }
}

impl eframe::App for App {
    fn clear_color(&self, _: &egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 0.0]
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.frames += 1;
        if self.frames < 4 {
            crate::system::force_foreground(TITLE);
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        }
        if self.close_on_unfocus {
            match ctx.input(|i| i.viewport().focused) {
                Some(true) => self.had_focus = true,
                Some(false) if self.had_focus => {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    return;
                }
                _ => {}
            }
        }

        let mut rows = self.rows();
        if self.selected >= rows.len() {
            self.selected = rows.len().saturating_sub(1);
        }
        if let Some(a) = self.keys(ctx, &rows) {
            self.apply(ctx, a);
            rows = self.rows();
            if self.selected >= rows.len() {
                self.selected = rows.len().saturating_sub(1);
            }
        }

        let frame = egui::Frame::none()
            .fill(self.c.bg)
            .rounding(self.radius)
            .stroke(egui::Stroke::new(1.0_f32, self.c.surface))
            .inner_margin(14.0);
        let mut action = None;
        egui::CentralPanel::default().frame(frame).show(ctx, |ui| {
            let before = self.query.clone();
            let edit = ui.add(
                egui::TextEdit::singleline(&mut self.query)
                    .hint_text("Search, or press 1 to 9 to paste a slot")
                    .desired_width(f32::INFINITY)
                    .margin(egui::vec2(10.0, 8.0)),
            );
            edit.request_focus();
            if self.query != before {
                self.selected = 0;
            }
            ui.add_space(8.0);

            let footer_h = self.font_size * 1.6;
            let list_h = (ui.available_height() - footer_h).max(40.0);
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .max_height(list_h)
                .show(ui, |ui| {
                    if rows.is_empty() {
                        ui.add_space(20.0);
                        ui.vertical_centered(|ui| {
                            ui.label(
                                RichText::new("Nothing here yet. Copy something.")
                                    .color(self.c.muted),
                            );
                        });
                    }
                    for (i, row) in rows.iter().enumerate() {
                        let selected = i == self.selected;
                        let fill = if selected {
                            self.c.accent.gamma_multiply(0.25)
                        } else {
                            Color32::TRANSPARENT
                        };
                        let thumb = if row.meta.kind == Kind::Image {
                            self.thumb(ctx, row.meta.id)
                        } else {
                            None
                        };
                        let inner = egui::Frame::none()
                            .fill(fill)
                            .rounding(self.radius * 0.6)
                            .inner_margin(egui::Margin::symmetric(10.0, 6.0))
                            .show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                ui.horizontal(|ui| {
                                    let badge = match row.slot {
                                        Some(n) => RichText::new(format!("{n}"))
                                            .color(self.c.accent)
                                            .strong(),
                                        None => RichText::new(" ").color(self.c.muted),
                                    };
                                    ui.add_sized(
                                        [self.font_size, self.font_size * 1.2],
                                        egui::Label::new(badge.monospace()),
                                    );
                                    if let Some(t) = &thumb {
                                        let s = t.size_vec2();
                                        ui.image((t.id(), s));
                                    }
                                    let text_color = if selected {
                                        self.c.fg
                                    } else {
                                        self.c.fg.gamma_multiply(0.85)
                                    };
                                    ui.add(
                                        egui::Label::new(
                                            RichText::new(&row.meta.preview).color(text_color),
                                        )
                                        .truncate(),
                                    );
                                });
                            });
                        let resp = inner.response.interact(Sense::click());
                        if resp.clicked() {
                            action = Some(Action::PasteEntry(row.meta.id));
                        }
                        if resp.hovered() && ui.input(|i| i.pointer.delta() != egui::Vec2::ZERO) {
                            self.selected = i;
                        }
                        if selected && self.scroll_to_selected {
                            resp.scroll_to_me(None);
                        }
                    }
                });
            self.scroll_to_selected = false;

            ui.add_space(4.0);
            let hint = self.status.clone().unwrap_or_else(|| {
                "Enter paste    Alt+digit pin to slot    Del delete    Esc close".into()
            });
            ui.label(RichText::new(hint).color(self.c.muted).small());
        });
        if let Some(a) = action {
            self.apply(ctx, a);
        }
    }
}

pub fn run(return_to: isize) -> Result<()> {
    let cfg = Config::load().unwrap_or_default();
    let theme = cfg.theme.resolved();
    let (slots, entries) = super::fetch()?;
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(TITLE)
            .with_app_id("hotbar")
            .with_inner_size([cfg.picker.width, cfg.picker.height])
            .with_decorations(false)
            .with_resizable(false)
            .with_always_on_top()
            .with_transparent(true)
            .with_taskbar(false),
        centered: true,
        ..Default::default()
    };
    let font_size = theme.font_size;
    let app = App {
        c: Colors::from(&theme),
        radius: theme.radius,
        font_size,
        query: String::new(),
        slots,
        entries,
        selected: 0,
        scroll_to_selected: false,
        thumbs: HashMap::new(),
        return_to,
        close_on_unfocus: cfg.picker.close_on_unfocus,
        had_focus: false,
        frames: 0,
        status: None,
    };
    eframe::run_native(
        TITLE,
        options,
        Box::new(move |cc| {
            let ctx = &cc.egui_ctx;
            ctx.set_visuals(egui::Visuals::dark());
            let (fg, surface, accent) = (app.c.fg, app.c.surface, app.c.accent);
            ctx.style_mut(|s| {
                for (style, scale) in [
                    (egui::TextStyle::Body, 1.0),
                    (egui::TextStyle::Button, 1.0),
                    (egui::TextStyle::Monospace, 1.0),
                    (egui::TextStyle::Small, 0.8),
                    (egui::TextStyle::Heading, 1.3),
                ] {
                    if let Some(f) = s.text_styles.get_mut(&style) {
                        f.size = font_size * scale;
                    }
                }
                s.visuals.override_text_color = Some(fg);
                s.visuals.extreme_bg_color = surface;
                s.visuals.selection.bg_fill = accent.gamma_multiply(0.5);
                s.visuals.widgets.inactive.rounding = egui::Rounding::same(8.0);
                s.visuals.widgets.hovered.rounding = egui::Rounding::same(8.0);
                s.visuals.widgets.active.rounding = egui::Rounding::same(8.0);
            });
            Ok(Box::new(app))
        }),
    )
    .map_err(|e| anyhow!("picker window failed: {e}"))
}
