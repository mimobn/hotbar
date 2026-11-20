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
    focused_since: Option<std::time::Instant>,
    frames: u32,
    status: Option<String>,
}

impl App {
    /// History entries matching the search. Entries held in a slot carry its number.
    fn rows(&self) -> Vec<Row> {
        let q = self.query.to_lowercase();
        self.entries
            .iter()
            .filter(|m| q.is_empty() || m.preview.to_lowercase().contains(&q))
            .map(|m| Row {
                meta: m.clone(),
                slot: self
                    .slots
                    .iter()
                    .find(|s| s.entry.as_ref().map(|e| e.id) == Some(m.id))
                    .map(|s| s.n),
            })
            .collect()
    }

    /// The nine slots as a row of cards, always visible, empty or not.
    fn hotbar(&mut self, ui: &mut egui::Ui, action: &mut Option<Action>) {
        let gap = 6.0;
        let w = ((ui.available_width() - gap * 8.0) / 9.0).max(30.0);
        let h = self.font_size * 3.6;
        let slots = self.slots.clone();
        let ctx = ui.ctx().clone();
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = gap;
            for s in &slots {
                let (rect, resp) = ui.allocate_exact_size(egui::vec2(w, h), Sense::click());
                let filled = s.entry.is_some();
                let hovered = resp.hovered();
                let fill = match (filled, hovered) {
                    (true, true) => self.c.accent.gamma_multiply(0.35),
                    (true, false) => self.c.accent.gamma_multiply(0.18),
                    (false, true) => self.c.surface,
                    (false, false) => self.c.surface.gamma_multiply(0.6),
                };
                let painter = ui.painter_at(rect);
                painter.rect_filled(rect, self.radius * 0.6, fill);
                if filled {
                    painter.rect_stroke(
                        rect,
                        self.radius * 0.6,
                        egui::Stroke::new(1.0_f32, self.c.accent.gamma_multiply(0.6)),
                    );
                }
                let pad = 6.0;
                let num_color = if filled { self.c.accent } else { self.c.muted };
                painter.text(
                    rect.left_top() + egui::vec2(pad, pad * 0.6),
                    egui::Align2::LEFT_TOP,
                    s.n.to_string(),
                    egui::FontId::monospace(self.font_size * 0.95),
                    num_color,
                );
                let body = egui::Rect::from_min_max(
                    rect.left_top() + egui::vec2(pad, self.font_size * 1.45),
                    rect.right_bottom() - egui::vec2(pad, pad * 0.8),
                );
                let image = match &s.entry {
                    Some(m) if m.kind == Kind::Image => self.thumb(&ctx, m.id),
                    _ => None,
                };
                if let Some(tex) = image {
                    // Fit the thumbnail inside the card, keeping its shape.
                    let size = tex.size_vec2();
                    let scale = (body.width() / size.x).min(body.height() / size.y);
                    let fit = egui::Rect::from_center_size(body.center(), size * scale);
                    let uv = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
                    painter.image(tex.id(), fit, uv, Color32::WHITE);
                } else {
                    let (text, color) = match &s.entry {
                        Some(m) if m.kind == Kind::Image => ("image".to_string(), self.c.fg),
                        Some(m) => (m.preview.clone(), self.c.fg),
                        None => ("empty".to_string(), self.c.muted.gamma_multiply(0.8)),
                    };
                    let mut job = egui::text::LayoutJob::simple(
                        text,
                        egui::FontId::proportional(self.font_size * 0.72),
                        color,
                        body.width(),
                    );
                    job.wrap.max_rows = 2;
                    job.wrap.break_anywhere = true;
                    let galley = ui.fonts(|f| f.layout_job(job));
                    painter.galley(body.left_top(), galley, color);
                }
                let tip = match &s.entry {
                    Some(m) => format!("Slot {}: {}\nClick or press {} to paste", s.n, m.preview, s.n),
                    None => format!(
                        "Slot {} is empty.\nCopy with Ctrl+C then {}, or select an entry below and press Alt+{}",
                        s.n, s.n, s.n
                    ),
                };
                let resp = resp.on_hover_text(tip);
                if resp.clicked() && filled {
                    *action = Some(Action::PasteSlot(s.n));
                }
            }
        });
        if self.slots.iter().all(|s| s.entry.is_none()) {
            ui.add_space(4.0);
            ui.label(
                RichText::new("No slots yet: Ctrl+C then a digit saves a copy to that slot, or pick an entry below and press Alt+digit.")
                    .color(self.c.muted)
                    .small(),
            );
        }
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
        // The daemon does the actual paste, so the picker can simply quit.
        // (Hiding the window and asking egui to close it can leave a hidden
        // process behind, because egui stops running hidden windows.)
        let close = |_: &egui::Context, to: isize| {
            crate::system::focus_window(to);
            std::process::exit(0);
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
        // Focus can bounce while the window appears. Keep asking for it until
        // we have held it for a moment; only after that does losing focus
        // (clicking elsewhere) close the picker.
        let focused = ctx.input(|i| i.viewport().focused) == Some(true);
        let settled = self
            .focused_since
            .is_some_and(|t| t.elapsed() > std::time::Duration::from_millis(400));
        if focused {
            self.focused_since
                .get_or_insert_with(std::time::Instant::now);
        } else if settled && self.close_on_unfocus {
            std::process::exit(0);
        } else {
            self.focused_since = None;
            if self.frames < 120 {
                crate::system::force_foreground(TITLE);
                ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                ctx.request_repaint_after(std::time::Duration::from_millis(50));
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
            ui.add_space(10.0);
            self.hotbar(ui, &mut action);
            ui.add_space(10.0);
            ui.label(RichText::new("History").color(self.c.muted).small());
            ui.add_space(2.0);

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
                "Enter paste    1 to 9 paste slot    Alt+digit save to slot    Del delete    Esc close"
                    .into()
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
        focused_since: None,
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
