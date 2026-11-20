//! Page layouts for the settings window, mirroring Synapse 4's mouse tabs.

#![allow(non_snake_case, unsafe_op_in_unsafe_fn)]

use windows_sys::Win32::Foundation::{POINT, RECT};
use windows_sys::Win32::Graphics::Gdi::HDC;

use super::draw::theme::*;
use super::draw::*;
use super::{FontKind, Id, Kind, Layer, Page, TileArt, TileItem, Ui, Widget, display_model};
use crate::accel::Mode;
use crate::config::parse_button;
use crate::device::{CLICK_CODES, DONGLE_LED_SLOTS, DeviceSpec, DongleKind, PhysicalButton, TURBO_MAX, TURBO_MIN};

/// Card padding, the label column of a settings row, and the row heights (DESIGN.md 3.3, 4.6).
const PAD: i32 = 16;
const LABEL_W: i32 = 400;
const ROW_H: i32 = 48;
const ROW_H_SUB: i32 = 64;
/// Control heights.
const BUTTON_H: i32 = 32;
const CHIP_H: i32 = 28;
/// Game tile size (2:3 cover) and the gap of the grid.
const TILE_W: i32 = 150;
const TILE_H: i32 = 212;
const TILE_GAP: i32 = 16;

/// Builds widgets top to bottom in logical pixels, grouping them in cards.
pub struct Form {
    scale: f32,
    dc: HDC,
    body: windows_sys::Win32::Graphics::Gdi::HFONT,
    x: i32,
    w: i32,
    y: i32,
    widgets: Vec<Widget>,
    card: Option<(usize, i32)>,
    /// The divider under the last settings row, removed when its card closes.
    last_divider: Option<usize>,
}

impl Form {
    pub fn new(scale: f32, dc: HDC, fonts: &Fonts, x: i32, w: i32) -> Form {
        Form {
            scale,
            dc,
            body: fonts.body,
            x: (x as f32 / scale) as i32,
            w: (w as f32 / scale) as i32,
            y: 24,
            widgets: Vec::new(),
            card: None,
            last_divider: None,
        }
    }

    /// Right edge of the content inside a card.
    fn right(&self) -> i32 {
        self.x + self.w - PAD
    }

    /// Hairline under a settings row (dropped again if it is the card's last row).
    fn divider(&mut self, y: i32) {
        let i = self.push(Id::None, self.x + PAD, y, self.w - 2 * PAD, 1, Kind::Line);
        self.last_divider = Some(i);
    }

    fn s(&self, v: i32) -> i32 {
        (v as f32 * self.scale).round() as i32
    }

    fn measure(&self, font: windows_sys::Win32::Graphics::Gdi::HFONT, t: &str) -> i32 {
        (unsafe { text_width(self.dc, font, t) } as f32 / self.scale).ceil() as i32
    }

    fn push(&mut self, id: Id, x: i32, y: i32, w: i32, h: i32, kind: Kind) -> usize {
        self.widgets.push(Widget { id, rect: rect(self.s(x), self.s(y), self.s(w), self.s(h)), kind, fixed: false });
        self.widgets.len() - 1
    }

    fn cx(&self) -> i32 {
        self.x + PAD + LABEL_W
    }

    fn cw(&self) -> i32 {
        self.w - 2 * PAD - LABEL_W
    }

    /// Start a new card with a section title; closes the previous one.
    pub fn section(&mut self, title: &str) {
        self.section_icon(title, None);
    }

    /// Card with a title (DESIGN.md 4.5): Body strong, sentence case, no icon and no rule.
    /// The icon argument is kept for the call sites but not drawn.
    pub fn section_icon(&mut self, title: &str, _icon: Option<Icon>) {
        self.close_card();
        self.card = Some((self.widgets.len(), self.y));
        self.widgets.push(Widget { id: Id::None, rect: rect(0, 0, 0, 0), kind: Kind::Card, fixed: false });
        self.y += 12;
        self.push(
            Id::None,
            self.x + PAD,
            self.y,
            self.w - 2 * PAD,
            24,
            Kind::Text { text: title.to_string(), font: FontKind::Bold, color: TEXT, flags: DT_ROW },
        );
        self.y += 28;
    }

    /// A grid of game tiles, as many per row as fit.
    pub fn tiles(&mut self, items: Vec<TileItem>) {
        if items.is_empty() {
            return;
        }
        let cols = ((self.w - 2 * PAD + TILE_GAP) / (TILE_W + TILE_GAP)).max(1) as usize;
        let n = items.len();
        self.y += 4;
        for (k, it) in items.into_iter().enumerate() {
            let col = (k % cols) as i32;
            let x = self.x + PAD + col * (TILE_W + TILE_GAP);
            self.push(it.id, x, self.y, TILE_W, TILE_H, Kind::Tile { title: it.title, sub: it.sub, badge: it.badge, art: it.art, selected: it.selected });
            if k % cols == cols - 1 || k + 1 == n {
                self.y += TILE_H + TILE_GAP;
            }
        }
    }

    fn close_card(&mut self) {
        if let Some((i, y0)) = self.card.take() {
            if let Some(d) = self.last_divider.take() {
                self.widgets[d].rect = rect(0, 0, 0, 0);
            }
            self.y += 8;
            self.widgets[i].rect = rect(self.s(self.x), self.s(y0), self.s(self.w), self.s(self.y - y0));
            self.y += 12;
        }
    }

    pub fn finish(mut self) -> (Vec<Widget>, i32) {
        self.close_card();
        let h = self.s(self.y);
        (self.widgets, h)
    }

    /// Settings row (DESIGN.md 4.6): title in Body strong, description in Caption,
    /// a divider underneath; the caller puts the control at the right. Returns the row height.
    fn label(&mut self, title: &str, sub: &str) -> i32 {
        self.label_min(title, sub, 0)
    }

    fn label_min(&mut self, title: &str, sub: &str, min_h: i32) -> i32 {
        let h = if sub.is_empty() {
            ROW_H
        } else {
            // The description wraps inside the label column; the row grows with it.
            let lines = (self.measure(self.body, sub) * 12 / 14 / (LABEL_W - 16) + 1).clamp(1, 3);
            ROW_H_SUB + 16 * (lines - 1)
        }
        .max(min_h);
        if sub.is_empty() {
            self.push(
                Id::None,
                self.x + PAD,
                self.y,
                LABEL_W - 10,
                h,
                Kind::Text { text: title.to_string(), font: FontKind::Bold, color: TEXT, flags: DT_ROW },
            );
        } else {
            self.push(
                Id::None,
                self.x + PAD,
                self.y + 12,
                LABEL_W - 16,
                20,
                Kind::Text { text: title.to_string(), font: FontKind::Bold, color: TEXT, flags: DT_ROW },
            );
            self.push(
                Id::None,
                self.x + PAD,
                self.y + 32,
                LABEL_W - 16,
                h - 40,
                Kind::Text { text: sub.to_string(), font: FontKind::Small, color: MUTED, flags: DT_LEFT_ | DT_WORDBREAK_ | DT_NOPREFIX_ | DT_END_ELLIPSIS_ },
            );
        }
        self.divider(self.y + h - 1);
        h
    }

    pub fn note(&mut self, text: &str) {
        // Descriptions wrap at about 60 characters (DESIGN.md 3.2).
        let w = (self.w - 2 * PAD).min(560);
        let lines = (self.measure(self.body, text) / w.max(1) + 1).clamp(1, 4);
        let h = 16 * lines + 12;
        self.push(
            Id::None,
            self.x + PAD,
            self.y,
            w,
            h,
            Kind::Text { text: text.to_string(), font: FontKind::Small, color: MUTED, flags: DT_LEFT_ | DT_WORDBREAK_ | DT_NOPREFIX_ },
        );
        self.y += h;
    }

    pub fn info(&mut self, title: &str, value: &str) {
        self.push(Id::None, self.x + PAD, self.y, LABEL_W - 10, 36, Kind::Text { text: title.to_string(), font: FontKind::Body, color: MUTED, flags: DT_ROW });
        self.push(Id::None, self.cx(), self.y, self.cw(), 36, Kind::Text { text: value.to_string(), font: FontKind::Body, color: TEXT, flags: DT_ROW });
        self.divider(self.y + 35);
        self.y += 36;
    }

    /// An info row whose value starts with a clickable address; `rest` follows it as plain text.
    pub fn link(&mut self, title: &str, shown: &str, url: &'static str, rest: &str) {
        self.push(Id::None, self.x + PAD, self.y, LABEL_W - 10, 36, Kind::Text { text: title.to_string(), font: FontKind::Body, color: MUTED, flags: DT_ROW });
        let w = (self.measure(self.body, shown) + 2).min(self.cw());
        self.push(Id::Link(url), self.cx(), self.y, w, 36, Kind::Link { text: shown.to_string() });
        if !rest.is_empty() {
            let text = format!("  \u{b7}  {rest}");
            self.push(Id::None, self.cx() + w, self.y, self.cw() - w, 36, Kind::Text { text, font: FontKind::Body, color: TEXT, flags: DT_ROW });
        }
        self.divider(self.y + 35);
        self.y += 36;
    }

    pub fn toggle(&mut self, title: &str, sub: &str, on: bool, id: Id) {
        let h = self.label(title, sub);
        self.push(id, self.right() - 40, self.y + (h - 20) / 2, 40, 20, Kind::Toggle { on });
        self.y += h;
    }

    /// Chip boxes wrapped into rows of at most `max_w` logical units; returns `(scaled_rects, used_w, total_h)`.
    fn chip_rects(&self, options: &[String], max_w: i32) -> (Vec<RECT>, i32, i32) {
        let mut rects = Vec::with_capacity(options.len());
        let limit = max_w.max(60);
        let mut x = 0;
        let mut y = 0;
        let mut used_w = 0;
        for o in options {
            let w = (self.measure(self.body, o) + 20).min(limit);
            if x > 0 && x + w > limit {
                x = 0;
                y += CHIP_H + 8;
            }
            rects.push(RECT { left: self.s(x), top: self.s(y), right: self.s(x + w), bottom: self.s(y + CHIP_H) });
            used_w = used_w.max(x + w);
            x += w + 8;
        }
        let total_h = if options.is_empty() { CHIP_H } else { y + CHIP_H };
        (rects, used_w, total_h)
    }

    /// Chips right-aligned in a settings row (wrapping inside the control column when narrow).
    pub fn chips(&mut self, title: &str, sub: &str, options: &[String], selected: Option<usize>, id: Id) {
        let (rects, used_w, total_h) = self.chip_rects(options, self.cw());
        let h = self.label_min(title, sub, total_h + 16);
        let w = used_w.min(self.cw());
        self.push(id, self.right() - w, self.y + (h - total_h) / 2, w, total_h, Kind::Chips { options: options.to_vec(), selected, rects });
        self.y += h;
    }

    /// Chips spanning the whole card width, wrapping onto multiple lines so long lists never overflow.
    pub fn chips_wide(&mut self, title: &str, options: &[String], selected: Option<usize>, id: Id) {
        if !title.is_empty() {
            self.push(
                Id::None,
                self.x + PAD,
                self.y,
                self.w - 2 * PAD,
                24,
                Kind::Text { text: title.to_string(), font: FontKind::Bold, color: TEXT, flags: DT_ROW },
            );
            self.y += 28;
        }
        let avail_w = self.w - 2 * PAD;
        let (rects, _, total_h) = self.chip_rects(options, avail_w);
        self.push(id, self.x + PAD, self.y, avail_w, total_h, Kind::Chips { options: options.to_vec(), selected, rects });
        self.y += total_h + 12;
    }

    #[allow(clippy::too_many_arguments)]
    pub fn slider(&mut self, title: &str, sub: &str, min: i32, max: i32, step: i32, value: i32, fmt: fn(i32) -> String, id: Id) {
        self.slider_opt(title, sub, min, max, step, value, fmt, id, true);
    }

    /// Slider that can be greyed out (shown, but not adjustable).
    #[allow(clippy::too_many_arguments)]
    pub fn slider_opt(&mut self, title: &str, sub: &str, min: i32, max: i32, step: i32, value: i32, fmt: fn(i32) -> String, id: Id, enabled: bool) {
        let h = self.label(title, sub);
        self.push(
            id,
            self.cx(),
            self.y,
            self.cw(),
            h,
            Kind::Slider { min, max, step, value: value.clamp(min, max), fmt, ticks: Vec::new(), show_value: true, enabled },
        );
        self.y += h;
    }

    /// Warning line in amber with its icon, e.g. why a control above is greyed out.
    pub fn warning(&mut self, text: &str) {
        self.push(Id::None, self.x + PAD, self.y + 4, 16, 16, Kind::Icon { icon: Icon::Warning, color: WARN });
        self.push(
            Id::None,
            self.x + PAD + 24,
            self.y,
            self.w - 2 * PAD - 24,
            24,
            Kind::Text { text: text.to_string(), font: FontKind::Small, color: WARN, flags: DT_ROW },
        );
        self.y += 28;
    }

    /// Discrete slider with one tick label per step.
    pub fn ticks(&mut self, title: &str, sub: &str, labels: &[String], selected: usize, id: Id) {
        let h = self.label(title, sub).max(64);
        let n = labels.len().max(1) as i32;
        self.push(
            id,
            self.cx(),
            self.y,
            self.cw(),
            h,
            Kind::Slider {
                min: 0,
                max: n - 1,
                step: 1,
                value: (selected as i32).min(n - 1),
                fmt: |v| v.to_string(),
                ticks: labels.to_vec(),
                show_value: false,
                enabled: true,
            },
        );
        self.y += h;
    }

    /// A DPI stage row: radio (active stage), slider, and a box to type the value.
    /// With `y`, a second row for the vertical DPI follows (independent X/Y).
    pub fn stage(&mut self, i: usize, dpi: u16, y: Option<u16>, active: bool, selectable: bool, dpi_max: u16) {
        let h = ROW_H;
        let title = if selectable { format!("档位 {}", i + 1) } else { "当前灵敏度 (DPI)".to_string() };
        let title = if y.is_some() { format!("{title}  ·  X 轴") } else { title };
        let title = if active && selectable { format!("{title}  ·  当前使用") } else { title };
        if selectable {
            self.push(Id::StageActive(i), self.x + PAD, self.y, LABEL_W - 10, h, Kind::Radio { on: active, text: title });
        } else {
            self.push(Id::None, self.x + PAD, self.y, LABEL_W - 10, h, Kind::Text { text: title, font: FontKind::Bold, color: TEXT, flags: DT_ROW });
        }
        self.dpi_controls(Id::StageValue(i), dpi, h, dpi_max);
        self.y += h;
        if let Some(yd) = y {
            self.push(
                Id::None,
                self.x + PAD + 28,
                self.y,
                LABEL_W - 38,
                h,
                Kind::Text { text: "Y 轴".to_string(), font: FontKind::Body, color: MUTED, flags: DT_ROW },
            );
            self.dpi_controls(Id::StageY(i), yd, h, dpi_max);
            self.y += h;
        }
        self.divider(self.y - 1);
    }

    fn dpi_controls(&mut self, id: Id, dpi: u16, h: i32, dpi_max: u16) {
        let edit_w = 88;
        self.push(
            id,
            self.cx(),
            self.y,
            self.cw() - edit_w - 16,
            h,
            Kind::Slider {
                min: 100,
                max: dpi_max as i32,
                step: 50,
                value: dpi as i32,
                fmt: |v| v.to_string(),
                ticks: Vec::new(),
                show_value: false,
                enabled: true,
            },
        );
        self.push(id, self.right() - edit_w, self.y + (h - BUTTON_H) / 2, edit_w, BUTTON_H, Kind::Edit { text: dpi.to_string(), numeric: true });
    }

    /// Acceleration curve plot (Dynamic Sensitivity); editable curves take an id.
    pub fn curve(&mut self, points: &[(u8, f32)], editable: bool, selected: Option<usize>) {
        let h = 230;
        let samples = crate::accel::curve(points).to_vec();
        let id = if editable { Id::Curve } else { Id::None };
        self.push(id, self.x + PAD, self.y, self.w - 2 * PAD, h, Kind::Curve { points: points.to_vec(), samples, editable, selected });
        self.y += h + 8;
    }

    pub fn edit(&mut self, title: &str, sub: &str, text: &str, numeric: bool, id: Id) {
        let h = self.label(title, sub);
        let w = self.cw().min(if numeric { 88 } else { 320 });
        self.push(id, self.right() - w, self.y + (h - BUTTON_H) / 2, w, BUTTON_H, Kind::Edit { text: text.to_string(), numeric });
        self.y += h;
    }

    /// A search box with its icon, full width of the card.
    pub fn search(&mut self, text: &str, id: Id) {
        self.push(Id::None, self.x + PAD + 4, self.y + (BUTTON_H - 16) / 2, 16, 16, Kind::Icon { icon: Icon::Search, color: MUTED });
        self.push(id, self.x + PAD + 28, self.y, (self.w - 2 * PAD - 28).min(320), BUTTON_H, Kind::Edit { text: text.to_string(), numeric: false });
        self.y += BUTTON_H + 12;
    }

    pub fn buttons(&mut self, items: &[(&str, Id, bool)]) {
        let mut x = self.x + PAD;
        self.y += 4;
        for (t, id, primary) in items {
            let icon = button_icon(*id, t);
            let w = self.measure(self.body, t) + 24 + if icon.is_some() { 24 } else { 0 };
            self.push(*id, x, self.y, w, BUTTON_H, Kind::Button { text: t.to_string(), primary: *primary, enabled: *id != Id::None, icon });
            x += w + 8;
        }
        self.y += BUTTON_H + 8;
    }

    pub fn battery(&mut self) {
        self.push(Id::None, self.x + PAD, self.y, self.w - 2 * PAD, 72, Kind::Battery);
        self.divider(self.y + 79);
        self.y += 80;
    }

    pub fn gap(&mut self, h: i32) {
        self.y += h;
    }

    /// One feature line on the Roadmap page: status pill, name, and a muted note.
    pub fn status(&mut self, st: Status, title: &str, note: &str) {
        let h = 36;
        let badge_w = 72;
        self.push(Id::None, self.x + PAD, self.y + 4, 16, 16 + 12, Kind::Icon { icon: st.icon(), color: st.color() });
        self.push(Id::None, self.x + PAD + 24, self.y + (h - 22) / 2, badge_w, 22, Kind::Badge { text: st.label().to_string(), color: st.color() });
        let title_x = self.x + PAD + 24 + badge_w + 12;
        let title_w = LABEL_W - badge_w - 36;
        self.push(Id::None, title_x, self.y, title_w, h, Kind::Text { text: title.to_string(), font: FontKind::Bold, color: TEXT, flags: DT_ROW });
        let note_x = title_x + title_w + 12;
        self.push(
            Id::None,
            note_x,
            self.y,
            (self.x + self.w - PAD - note_x).max(40),
            h,
            Kind::Text { text: note.to_string(), font: FontKind::Small, color: MUTED, flags: DT_ROW },
        );
        self.divider(self.y + h - 1);
        self.y += h;
    }

    /// Bulleted roadmap item.
    pub fn bullet(&mut self, color: u32, text: &str) {
        let w = self.w - 2 * PAD - 24;
        let lines = (self.measure(self.body, text) / w.max(1) + 1).clamp(1, 4);
        let h = 20 * lines + 8;
        self.push(Id::None, self.x + PAD + 4, self.y + 4, 12, 20, Kind::Bullet { color });
        self.push(
            Id::None,
            self.x + PAD + 24,
            self.y + 4,
            w,
            h - 4,
            Kind::Text { text: text.to_string(), font: FontKind::Body, color: TEXT, flags: DT_LEFT_ | DT_WORDBREAK_ | DT_NOPREFIX_ },
        );
        self.y += h;
    }
}

/// Icon in front of a button's label, by what the button does.
fn button_icon(id: Id, label: &str) -> Option<Icon> {
    Some(match id {
        Id::GameScan | Id::Refresh => Icon::Refresh,
        Id::GameBrowse | Id::OpenConfig => Icon::Folder,
        Id::GameRemove | Id::CurveRemove => Icon::Trash,
        Id::GameButtons => Icon::Mouse,
        Id::Reimport => Icon::Sync,
        Id::Reapply => Icon::Check,
        Id::PairStart => Icon::Link,
        Id::PairCancel | Id::Unpair => Icon::Stop,
        Id::CurveAdd => Icon::Add,
        Id::None if label.starts_with("Scanning") || label.starts_with("正在扫描") => Icon::Refresh,
        _ => return None,
    })
}

/// Implementation state of a Synapse feature, for the Roadmap page.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Done,
    Partial,
    Todo,
    Never,
    /// Devices page: writes confirmed by an owner.
    Verified,
    /// Devices page: partly tested by an owner, writes need allow_unverified.
    Testing,
    /// Devices page: known from the device table only.
    ReadOnly,
}

impl Status {
    fn label(self) -> &'static str {
        match self {
            Status::Done => "已完成",
            Status::Partial => "部分支持",
            Status::Todo => "计划中",
            Status::Never => "不做",
            Status::Verified => "已验证",
            Status::Testing => "测试中",
            Status::ReadOnly => "只读",
        }
    }

    /// Colour is never the only signal (DESIGN.md 6): each state has an icon too.
    fn icon(self) -> Icon {
        match self {
            Status::Done => Icon::Check,
            Status::Partial => Icon::Warning,
            Status::Todo => Icon::Flag,
            Status::Never => Icon::Stop,
            Status::Verified => Icon::Check,
            Status::Testing => Icon::Warning,
            Status::ReadOnly => Icon::Info,
        }
    }

    fn color(self) -> u32 {
        match self {
            Status::Done => ACCENT,
            Status::Partial => WARN,
            Status::Todo => BLUE,
            Status::Never => MUTED,
            Status::Verified => ACCENT,
            Status::Testing => WARN,
            Status::ReadOnly => MUTED,
        }
    }
}

// ----------------------------------------------------------------------
// Pages
// ----------------------------------------------------------------------

pub fn build(ui: &Ui, f: &mut Form) {
    if let Some(e) = ui.config_error() {
        f.section_icon("配置文件异常", Some(Icon::Warning));
        f.note(&format!("{e}。当前已回退显示默认设置；请修复配置文件，或在“软件设置”中点击“从鼠标重新导入”。"));
    }
    match ui.page {
        Page::Performance => performance(ui, f),
        Page::Calibration => calibration(ui, f),
        Page::Power => power(ui, f),
        Page::Games => games(ui, f),
        Page::Customize => customize(ui, f),
        Page::Devices => devices(ui, f),
        Page::Roadmap => roadmap(f),
        Page::App => app(ui, f),
    }
}

fn hz_labels(rates: &[u16]) -> Vec<String> {
    rates.iter().map(|r| format!("{r}")).collect()
}

fn mode_label_zh(m: Mode) -> &'static str {
    match m {
        Mode::Classic => "经典 (Classic)",
        Mode::Natural => "自然 (Natural)",
        Mode::Jump => "跃迁 (Jump)",
        Mode::Custom => "自定义曲线",
    }
}

fn performance(ui: &Ui, f: &mut Form) {
    let cfg = &ui.cfg;
    f.section_icon("灵敏度 (DPI)", Some(Icon::Speed));
    let staged = cfg.dpi.stages.len() > 1;
    f.toggle("启用多档位 DPI", "可通过鼠标底部的 DPI 键在各档位之间循环切换", staged, Id::StagesEnabled);
    let xy = !cfg.dpi.y_stages.is_empty();
    f.toggle("独立设置 X / Y 轴", "分别为水平（X）与垂直（Y）移动设置不同的 DPI 灵敏度", xy, Id::XySplit);
    let y_of = |i: usize| if xy { cfg.dpi.y_stages.get(i).copied() } else { None };
    if staged {
        let counts: Vec<String> = (1..=5).map(|n| format!("{n} 档")).collect();
        f.chips("DPI 档位数量", "", &counts, Some(cfg.dpi.stages.len() - 1), Id::StageCount);
        f.gap(4);
        for (i, d) in cfg.dpi.stages.iter().enumerate() {
            f.stage(i, *d, y_of(i), cfg.dpi.active as usize == i + 1, true, ui.spec.dpi_max);
        }
        f.note(&format!(
            "点击左侧圆点切换当前生效档位。滑块范围 100 至 {} DPI（步进 50），也可在右侧输入框输入 1 DPI 精度的任意数值并按回车。",
            ui.spec.dpi_max
        ));
    } else {
        f.stage(0, cfg.dpi.stages[0], y_of(0), true, false, ui.spec.dpi_max);
        f.note(&format!("滑块范围 100 至 {} DPI（步进 50），也可在右侧输入框直接输入 1 DPI 精度的精确数值并按回车。", ui.spec.dpi_max));
    }

    if ui.spec.supports_acceleration {
        f.section_icon("动态灵敏度 (板载鼠标加速)", Some(Icon::Sliders));
        let a = &cfg.acceleration;
        f.toggle("启用动态灵敏度", "根据手部移动速度实时缩放灵敏度倍率（完全存储于鼠标板载固件）", a.enabled, Id::AccelOn);
        if a.enabled {
            let mode = a.mode().unwrap_or(Mode::Classic);
            let modes = [Mode::Classic, Mode::Natural, Mode::Jump, Mode::Custom].map(|m| mode_label_zh(m).to_string());
            f.chips("加速曲线模式", "", &modes, Some(mode as usize), Id::AccelMode);
            f.gap(4);
            if mode == Mode::Custom {
                f.curve(&a.curve, true, ui.curve_sel);
                let can_remove = a.curve.len() > 2 && ui.curve_sel.is_some_and(|i| i > 0);
                let can_add = a.curve.len() < crate::accel::MAX_CONTROL_POINTS;
                f.buttons(&[
                    ("添加控制点", if can_add { Id::CurveAdd } else { Id::None }, false),
                    ("删除选中点", if can_remove { Id::CurveRemove } else { Id::None }, false),
                    ("套用经典模板", Id::CurveTemplate(0), false),
                    ("自然模板", Id::CurveTemplate(1), false),
                    ("跃迁模板", Id::CurveTemplate(2), false),
                ]);
                f.note("直接拖动图上的控制点调整曲线：横轴为移动速度，纵轴为灵敏度倍率（0.1 至 1.5x），最后一个点之后保持平直。");
            } else {
                f.curve(&mode.template(), false, None);
                f.note("“经典 / 自然 / 跃迁”三条预设直接内置于鼠标固件中；选择“自定义曲线”可自由拖拽控制点写入板载曲线。");
            }
        }
    }
    f.section_icon("轮询回报率 (Polling Rate)", Some(Icon::Bolt));
    let rates: &[u16] = ui.spec.polling_rates;
    let sel = rates.iter().position(|r| *r == cfg.polling.rate).unwrap_or(0);
    let sub = if rates.iter().any(|r| *r > 1000) {
        "每秒向电脑汇报坐标的次数。4000 / 8000 Hz 延迟极低，但无线模式下耗电更快"
    } else {
        "每秒向电脑汇报坐标的次数"
    };
    f.ticks("回报率 (Hz)", sub, &hz_labels(rates), sel, Id::Polling);

    f.note("日常桌面推荐 500 或 1000 Hz 省电，可在“游戏配置”页面设置进入游戏时自动切换至 4000 / 8000 Hz。");

    if !ui.spec.lighting.is_empty() {
        lighting(ui, f);
    }

    if ui.spec.supports_rotation {
        f.section_icon("传感器角度微调 (Rotation)", Some(Icon::Sync));
        let on = cfg.sensor.rotation != 0;
        f.toggle("启用角度校准", "旋转传感器坐标轴以补偿斜握鼠标的发力习惯（板载生效）", on, Id::RotationOn);
        if on {
            f.slider("旋转角度", "负值逆时针补偿，正值顺时针补偿", -30, 30, 1, cfg.sensor.rotation as i32, |v| format!("{v}°"), Id::Rotation);
        }
    }
}

/// Colour chips of the Lighting section: (label, rgb).
pub const LIGHT_COLORS: [(&str, [u8; 3]); 8] = [
    ("红色", [0xff, 0x00, 0x00]),
    ("橙色", [0xff, 0x60, 0x00]),
    ("黄色", [0xff, 0xd0, 0x00]),
    ("绿色", [0x00, 0xff, 0x40]),
    ("青色", [0x00, 0xd0, 0xff]),
    ("蓝色", [0x00, 0x30, 0xff]),
    ("紫色", [0xa0, 0x00, 0xff]),
    ("白色", [0xff, 0xff, 0xff]),
];

fn lighting(ui: &Ui, f: &mut Form) {
    use crate::lighting::Effect;
    let cfg = &ui.cfg;
    f.section_icon("板载灯效", Some(Icon::Bolt));
    let kinds: Vec<String> = ["关闭", "常亮", "呼吸", "光谱循环"].iter().map(|s| s.to_string()).collect();
    for (i, (zone, _)) in ui.spec.lighting.iter().enumerate() {
        let title = match *zone {
            "logo" => "Logo 灯",
            "wheel" => "滚轮灯",
            z => z,
        };
        let e = cfg.lighting.zone(zone).and_then(|s| Effect::parse(s).ok());
        let sel = e.map(|e| match e {
            Effect::Off => 0,
            Effect::Static(_) => 1,
            Effect::Breathing(_) | Effect::BreathingRandom => 2,
            Effect::Spectrum => 3,
        });
        let sub = if e.is_none() { "未指定：保持鼠标当前灯效" } else { "" };
        f.chips(title, sub, &kinds, sel, Id::LightEffect(i));
        if let Some(e @ (Effect::Static(_) | Effect::Breathing(_) | Effect::BreathingRandom)) = e {
            let mut colors: Vec<String> = LIGHT_COLORS.iter().map(|(l, _)| l.to_string()).collect();
            if !matches!(e, Effect::Static(_)) {
                colors.push("随机颜色".into());
            }
            let csel = match e {
                Effect::BreathingRandom => Some(LIGHT_COLORS.len()),
                _ => e.color().and_then(|c| LIGHT_COLORS.iter().position(|(_, rgb)| *rgb == c)),
            };
            f.chips_wide("", &colors, csel, Id::LightColor(i));
        }
        f.gap(4);
    }
    f.slider("灯光亮度", "作用于全部灯区", 0, 100, 5, cfg.lighting.brightness.unwrap_or(100) as i32, |v| format!("{v} %"), Id::Brightness);
    f.note("直接写入鼠标板载内存。如需任意色号，可在配置文件 [lighting] 下写入 \"static:#rrggbb\"。");
}

/// "1600 DPI  ·  2000 Hz" for a profile's tile.
fn tile_summary(cfg: &crate::config::Config, g: &crate::config::GameConfig) -> String {
    let stage = match g.dpi_stage.and_then(|s| cfg.dpi.stages.get(s as usize - 1)) {
        Some(d) => format!("{d} DPI"),
        None => "保持当前 DPI".to_string(),
    };
    let polling = match g.polling {
        Some(p) => format!("{p} Hz"),
        None => format!("{} Hz", cfg.polling.game_rate),
    };
    format!("{stage}  ·  {polling}")
}

/// Launcher name shown on a tile while hovering it.
fn badge_of(source: Option<&str>) -> &'static str {
    match source {
        Some("Steam") => "Steam",
        Some("Epic Games") => "Epic Games",
        Some("GOG") => "GOG",
        Some("Xbox") => "Xbox",
        Some("EA app") => "EA app",
        Some("Battle.net") => "暴雪战网",
        Some("Riot") => "Riot",
        Some("Ubisoft Connect") => "育碧",
        Some("Rockstar") => "Rockstar",
        Some(_) => "启动器",
        None => "本地程序",
    }
}

fn games(ui: &Ui, f: &mut Form) {
    let cfg = &ui.cfg;
    let rates: &[u16] = ui.spec.polling_rates;

    f.section_icon("游戏专属配置", Some(Icon::Game));
    if cfg.games.is_empty() {
        f.note(
            "尚未添加游戏配置。点击下方已安装游戏封面（或“添加程序”），即可为该游戏独立指定 DPI 档位、回报率与按键映射；托盘程序会在游戏处于前台时自动切换。",
        );
    } else {
        f.note("点击卡片展开设置。当游戏处于前台时，托盘程序会自动将专属配置写入鼠标，退出或切回桌面时自动恢复日常设置。");
    }
    let mut tiles: Vec<TileItem> = cfg
        .games
        .iter()
        .enumerate()
        .map(|(i, g)| TileItem {
            id: Id::GameRow(i),
            title: g.label(),
            sub: tile_summary(cfg, g),
            badge: badge_of(ui.game_source(g)),
            art: ui.profile_art(g),
            selected: ui.game_open && ui.sel_game == i,
        })
        .collect();
    // The last tile adds any program (games outside a launcher).
    tiles.push(TileItem {
        id: Id::GameBrowse, title: "添加程序".to_string(), sub: "选择任意 .exe".to_string(), badge: "", art: TileArt::Add, selected: false
    });
    f.tiles(tiles);

    if let Some(g) = cfg.games.get(ui.sel_game).filter(|_| ui.game_open) {
        f.section_icon(&format!("配置详情：{}", g.label()), Some(Icon::Sliders));
        game_settings(ui, f, g, rates);
    }

    f.section_icon("已安装的游戏", Some(Icon::Folder));
    let free = ui.games_without_profile();
    let sources = ui.library_sources();
    if ui.scanning && ui.library.is_empty() {
        f.note("正在扫描本机已安装游戏...");
    } else if ui.library.is_empty() {
        f.note("未在 Steam、Epic、GOG、EA、战网、Riot、育碧、Rockstar 或 Xbox 中扫描到游戏，可点击上方“添加程序”手动选择 .exe。");
    } else {
        f.note(&format!("共发现 {} 款已安装游戏，点击任意封面即可为其创建专属配置。", ui.library.len()));
    }
    // Filter chips per launcher (DESIGN.md 4.13), with counts, then the search box.
    if !sources.is_empty() {
        let mut chips = vec![format!("全部 {}", ui.library.len())];
        chips.extend(sources.iter().map(|s| format!("{s} {}", ui.library.iter().filter(|l| l.source == *s).count())));
        let sel = ui.lib_source.and_then(|s| sources.iter().position(|x| *x == s)).map(|i| i + 1).unwrap_or(0);
        f.chips_wide("", &chips, Some(sel), Id::GameSource);
    }
    if free.len() > 8 {
        f.search(&ui.game_filter, Id::GameFilter);
    }
    let filter = ui.game_filter.trim().to_lowercase();
    let shown: Vec<TileItem> = free
        .into_iter()
        .filter(|j| filter.is_empty() || ui.library[*j].name.to_lowercase().contains(&filter))
        .filter(|j| ui.lib_source.is_none_or(|s| ui.library[*j].source == s))
        .map(|j| {
            let l = &ui.library[j];
            TileItem { id: Id::GameLib(j), title: l.name.clone(), sub: String::new(), badge: badge_of(Some(l.source)), art: TileArt::Lib(j), selected: false }
        })
        .collect();
    let none = shown.is_empty();
    f.tiles(shown);
    if none && (!filter.is_empty() || ui.lib_source.is_some()) {
        f.note("没有匹配筛选条件的游戏。");
    }
    f.buttons(&[(if ui.scanning { "正在扫描..." } else { "重新扫描游戏库" }, if ui.scanning { Id::None } else { Id::GameScan }, false)]);

    f.section_icon("游戏默认高回报率", Some(Icon::Bolt));
    let gsel = rates.iter().position(|r| *r == cfg.polling.game_rate).unwrap_or(0);
    f.ticks("游戏运行时回报率 (Hz)", "供勾选“使用游戏默认回报率”的游戏配置自动调用", &hz_labels(rates), gsel, Id::GameRate);
    if !cfg.polling.games.is_empty() {
        f.edit("额外进程列表", "逗号分隔的进程名（仅自动提升回报率）", &cfg.polling.games.join(", "), false, Id::GamesEdit);
    }
    f.note("日常桌面使用 500 / 1000 Hz 续航更久，仅当游戏处于前台时自动拉满至 4000 / 8000 Hz。");
}

/// The settings of the open game profile.
fn game_settings(_ui: &Ui, f: &mut Form, g: &crate::config::GameConfig, rates: &[u16]) {
    let cfg = &_ui.cfg;
    let mut stages = vec!["保持日常档位".to_string()];
    stages.extend((1..=cfg.dpi.stages.len()).map(|i| format!("档位 {i} ({})", cfg.dpi.stages[i - 1])));
    f.chips_wide("游戏时使用的 DPI 档位", &stages, Some(g.dpi_stage.map(|s| s as usize).filter(|s| *s <= cfg.dpi.stages.len()).unwrap_or(0)), Id::GameStage);
    if cfg.dpi.stages.len() < 2 {
        f.note("当前仅开启了 1 个 DPI 档位。如需为游戏指定专属灵敏度，请先在“性能”页面开启多档位 DPI。");
    }
    let mut polls = vec![format!("游戏默认 ({} Hz)", cfg.polling.game_rate)];
    polls.extend(rates.iter().map(|r| format!("{r} Hz")));
    let psel = g.polling.and_then(|p| rates.iter().position(|r| *r == p)).map(|i| i + 1).unwrap_or(0);
    f.chips_wide("游戏时使用的回报率", &polls, Some(psel), Id::GamePolling);
    f.edit("显示名称", "显示在卡片与托盘提示中，按回车保存", &g.label(), false, Id::GameName);
    let exe_sub = if g.dir.is_empty() {
        "进程可执行文件名，如 cs2.exe，按回车保存"
    } else {
        "可选：留空则该安装目录下的任意进程均会自动匹配"
    };
    f.edit("可执行文件", exe_sub, &g.exe, false, Id::GameExe);
    if !g.dir.is_empty() {
        f.info("游戏目录", &g.dir);
    }
    let n = g.buttons.len();
    f.info(
        "按键映射",
        &if n == 0 {
            "与日常“标准层”保持一致".to_string()
        } else {
            format!("已单独覆盖 {n} 个按键，其余按键继承“标准层”设置")
        },
    );
    f.buttons(&[("编辑此游戏的按键布局", Id::GameButtons, true), ("删除配置", Id::GameRemove, false)]);
}

fn calibration(ui: &Ui, f: &mut Form) {
    use crate::sensor::{ASYM_LAND_MIN, ASYM_LIFT_MAX, ASYM_LIFT_MIN};
    let s = &ui.cfg.sensor;
    f.section_icon("Focus Pro 智能追踪校准", Some(Icon::Track));
    let modes = ["自定义智能追踪", "保持鼠标当前设置"].map(String::from);
    let sel = if s.tracking == "smart" { Some(0) } else { Some(1) };
    f.chips("抬起追踪模式", "与雷云 4 提供的 Focus Pro 传感器校准选项完全一致", &modes, sel, Id::TrackMode);
    if s.tracking != "smart" {
        f.note("当前保持鼠标内部已存储的抬起/着陆设置不做改动；选择“自定义智能追踪”即可在此处调节。");
        return;
    }
    f.toggle("启用非对称抬起截断 (Asymmetric Cut-off)", "分别精细设定“抬起停止追踪高度”与“放下恢复追踪高度”（共 26 级）", s.asymmetric, Id::Asym);
    if s.asymmetric {
        f.slider(
            "抬起截断高度 (Lift-off)",
            "鼠标抬离垫面达到此等级时停止追踪（2 至 26）",
            ASYM_LIFT_MIN as i32,
            ASYM_LIFT_MAX as i32,
            1,
            s.lift as i32,
            |v| format!("{v} 级"),
            Id::Lift,
        );
        f.slider(
            "着陆恢复高度 (Landing)",
            "鼠标落回垫面低于此等级时恢复追踪（须小于抬起等级）",
            ASYM_LAND_MIN as i32,
            (s.lift as i32 - 1).max(ASYM_LAND_MIN as i32),
            1,
            s.land as i32,
            |v| format!("{v} 级"),
            Id::Land,
        );
        f.note("与雷云 4 的非对称截断滑块一致（抬起 2..26，着陆 1..25，雷云默认 13 / 12）。炼狱蝰蛇 V4 Pro 支持直接从板载寄存器回读这两个等级。");
    } else {
        f.slider("对称抬起距离", "1 mm 为传感器支持的最低抬起高度", 1, 3, 1, s.lift as i32, |v| format!("{v} mm"), Id::Lift);
        f.note("智能追踪会自动适配不同材质鼠标垫表面，保持一致的抬起停止距离，设置直接存储在鼠标板载内存中。");
    }
}

fn power(ui: &Ui, f: &mut Form) {
    let cfg = &ui.cfg;
    f.section_icon("电池电量", Some(Icon::Battery));
    f.battery();
    f.slider("低电量桌面提醒", "电量低于该百分比时由托盘图标弹出气泡通知", 5, 50, 1, cfg.tray.notify_percent as i32, |v| format!("{v} %"), Id::NotifyPercent);
    f.toggle("开机静默自启到系统托盘", "登录 Windows 时无弹窗、无控制台直接后台驻留托盘（看电量、进游戏自动切高回报率）", ui.autostart, Id::Autostart);

    f.section_icon("无线休眠与节能", Some(Icon::Power));
    f.slider(
        "空闲自动休眠",
        "静止无操作达到该时长后进入省电休眠（移动即刻唤醒）",
        1,
        15,
        1,
        (cfg.power.idle_seconds / 60) as i32,
        |v| format!("{v} 分钟"),
        Id::Idle,
    );
    // Same rule as Synapse: the firmware only honours low power mode at 1000 Hz or less.
    let lpm_ok = cfg.polling.rate <= 1000;
    f.slider_opt(
        "低功耗模式阈值",
        "无线模式下电量低于该比例时自动进入低功耗节能状态",
        5,
        100,
        5,
        cfg.power.low_battery_percent as i32,
        |v| format!("{v} %"),
        Id::LowBattery,
        lpm_ok,
    );
    if !lpm_ok {
        f.warning("当前回报率为 2000 Hz 或更高时，鼠标固件将忽略低功耗模式阈值（需降至 1000 Hz 及以下生效）。");
    }

    if ui.wireless && ui.spec.dongle == DongleKind::Gen2Dome {
        f.section_icon("HyperSpeed 二代半球形无线接收器（3 颗指示灯）", Some(Icon::Wifi));
        let slot_labels: Vec<String> = DONGLE_LED_SLOTS.iter().map(|(_, _, l)| (*l).to_string()).collect();
        let slot_titles = [
            ("左指示灯", "接收器正面左侧第 1 颗 LED 显示内容（出厂默认：电量状态）"),
            ("中指示灯", "接收器正面中间第 2 颗 LED 显示内容（出厂默认：连接状态）"),
            ("右指示灯", "接收器正面右侧第 3 颗 LED 显示内容（出厂默认：回报率）"),
        ];
        let shown_leds = ui.shown_dongle_leds();
        for (idx, (title, sub)) in slot_titles.iter().enumerate() {
            let sel = shown_leds.and_then(|codes| DONGLE_LED_SLOTS.iter().position(|(c, _, _)| *c == codes[idx]));
            f.chips(title, sub, &slot_labels, sel, Id::DongleLedSlot(idx));
        }
        let shown_br = ui.shown_dongle_bright().unwrap_or(4).min(4) as i32;
        f.slider(
            "接收器指示灯亮度",
            "共 4 档亮度，拉到最左可一键熄灭全部 3 颗指示灯",
            0,
            4,
            1,
            shown_br,
            |v| if v == 0 { "关闭".into() } else { format!("{} %", v * 25) },
            Id::DongleLedBright,
        );
        f.note("直接写入二代半球形接收器固件（0x07/0x15 与 0x07/0x16），重启电脑或插到其他设备同样保持生效。");
    } else if ui.wireless && ui.spec.dongle == DongleKind::HyperPolling {
        f.section_icon("HyperPolling 无线接收器", Some(Icon::Wifi));
        let leds = ["电量状态", "回报率", "关闭"].map(String::from);
        f.chips("指示灯模式", "接收器指示灯展示的信息", &leds, Some((cfg.dongle.led as usize).clamp(1, 3) - 1), Id::DongleLed);
    }
}

pub const CATEGORIES: [&str; 9] = ["默认", "鼠标按键", "键盘按键", "DPI 切换", "多媒体", "桌面快捷", "系统与配置", "Hypershift", "禁用"];
pub const CAT_MOUSE: usize = 1;
pub const CAT_KEYBOARD: usize = 2;
pub const CAT_DPI: usize = 3;
pub const CAT_MEDIA: usize = 4;
/// Common Windows & desktop shortcuts (Synapse 4 "Windows Shortcuts"), stored onboard as `key:...`.
pub const CAT_SHORTCUT: usize = 5;
pub const CAT_SYSTEM: usize = 6;
pub const CAT_HYPERSHIFT: usize = 7;
pub const CAT_DISABLED: usize = 8;

/// One-click Windows & desktop shortcuts matching Razer Synapse 4 ("Windows 快捷方式").
/// Every entry uses canonical modifier order (`ctrl`, `shift`, `alt`, `win`) so onboard
/// hardware readback (`spec_for`) matches 1:1.
pub const SHORTCUT_PRESETS: [(&str, &str); 16] = [
    ("显示桌面", "key:win+d"),
    ("任务视图", "key:win+tab"),
    ("切换窗口", "key:alt+tab"),
    ("关闭窗口", "key:alt+f4"),
    ("虚拟桌面左移", "key:ctrl+win+left"),
    ("虚拟桌面右移", "key:ctrl+win+right"),
    ("区域截图", "key:shift+win+s"),
    ("资源管理器", "key:win+e"),
    ("锁定电脑", "key:win+l"),
    ("剪贴板历史", "key:win+v"),
    ("任务管理器", "key:ctrl+shift+esc"),
    ("浏览器后退", "key:alt+left"),
    ("浏览器前进", "key:alt+right"),
    ("复制", "key:ctrl+c"),
    ("粘贴", "key:ctrl+v"),
    ("撤销", "key:ctrl+z"),
];

/// Match a `key:...` spec against [`SHORTCUT_PRESETS`], normalizing modifier order
/// so manual edits like `key:win+shift+s` also resolve to `("区域截图", "key:shift+win+s")`.
pub fn shortcut_preset_of(spec: &str) -> Option<(&'static str, &'static str)> {
    if !spec.starts_with("key:") {
        return None;
    }
    let canon = parse_button(spec).ok().flatten().and_then(|f| crate::config::spec_for(&f));
    let target = canon.as_deref().unwrap_or(spec);
    SHORTCUT_PRESETS.iter().copied().find(|(_, s)| *s == target)
}

pub fn category_of(spec: Option<&str>) -> usize {
    match spec {
        None | Some("default") => 0,
        Some(s) if s.starts_with("mouse:") || s.starts_with("turbo:") || s == "double-click" => CAT_MOUSE,
        Some(s) if shortcut_preset_of(s).is_some() => CAT_SHORTCUT,
        Some(s) if s.starts_with("key:") || s.starts_with("turbo-key:") => CAT_KEYBOARD,
        Some(s) if s.starts_with("dpi") || s.starts_with("clutch:") => CAT_DPI,
        Some(s) if s.starts_with("profile-") => CAT_SYSTEM,
        Some(s) if s.starts_with("media:") => CAT_MEDIA,
        Some(s) if s.starts_with("system:") => CAT_SYSTEM,
        Some("hypershift") => CAT_HYPERSHIFT,
        Some("disabled") => CAT_DISABLED,
        _ => 0,
    }
}

/// Split a turbo assignment into its plain form and the repeat rate:
/// `turbo:left:10` -> (`mouse:left`, Some(10)); `key:c` -> (`key:c`, None).
pub fn turbo_split(spec: &str) -> (String, Option<u8>) {
    let rate = |rest: &str| rest.rsplit_once(':').map(|(base, n)| (base.to_string(), n.parse::<u8>().ok()));
    if let Some((base, n)) = spec.strip_prefix("turbo:").and_then(rate) {
        return (format!("mouse:{base}"), n);
    }
    if let Some((base, n)) = spec.strip_prefix("turbo-key:").and_then(rate) {
        return (format!("key:{base}"), n);
    }
    (spec.to_string(), None)
}

/// Inverse of [`turbo_split`]: add a repeat rate to a mouse or key assignment.
pub fn with_turbo(base: &str, rate: Option<u8>) -> String {
    let Some(n) = rate else { return base.to_string() };
    let n = n.clamp(TURBO_MIN, TURBO_MAX);
    if let Some(b) = base.strip_prefix("mouse:") {
        format!("turbo:{b}:{n}")
    } else if let Some(k) = base.strip_prefix("key:") {
        format!("turbo-key:{k}:{n}")
    } else {
        base.to_string()
    }
}

/// (label, spec) choices inside a category.
pub fn options_for(cat: usize, stages: usize) -> Vec<(String, String)> {
    match cat {
        CAT_MOUSE => {
            let mut v: Vec<(String, String)> = CLICK_CODES.iter().map(|(n, _)| (pretty_button(n), format!("mouse:{n}"))).collect();
            v.push(("左键双击".to_string(), "double-click".to_string()));
            v
        }
        CAT_SHORTCUT => SHORTCUT_PRESETS.iter().map(|(l, s)| (l.to_string(), s.to_string())).collect(),
        CAT_SYSTEM => {
            let mut v: Vec<(String, String)> =
                [("系统睡眠", "sleep"), ("系统关机/电源", "power"), ("系统唤醒", "wake")].iter().map(|(l, s)| (l.to_string(), format!("system:{s}"))).collect();
            v.push(("配置文件向上循环".to_string(), "profile-cycle-up".to_string()));
            v.push(("配置文件向下循环".to_string(), "profile-cycle-down".to_string()));
            v
        }
        CAT_DPI => {
            let mut v = vec![
                ("DPI 循环切换".to_string(), "dpi-cycle".to_string()),
                ("DPI 升高一档".to_string(), "dpi-up".to_string()),
                ("DPI 降低一档".to_string(), "dpi-down".to_string()),
            ];
            for i in 1..=stages.clamp(1, 5) {
                v.push((format!("锁定档位 {i}"), format!("dpi-stage:{i}")));
            }
            v.push(("狙击键 (按住 400 DPI)".to_string(), "clutch:400".to_string()));
            v.push(("狙击键 (按住 800 DPI)".to_string(), "clutch:800".to_string()));
            v
        }
        CAT_MEDIA => [
            ("播放 / 暂停", "play-pause"),
            ("静音", "mute"),
            ("音量 +", "vol-up"),
            ("音量 -", "vol-down"),
            ("下一曲", "next"),
            ("上一曲", "prev"),
            ("停止播放", "stop"),
        ]
        .iter()
        .map(|(l, s)| (l.to_string(), format!("media:{s}")))
        .collect(),
        _ => Vec::new(),
    }
}

fn pretty_combo(combo: &str) -> String {
    combo
        .split('+')
        .map(|p| {
            let p = p.trim();
            if p.len() == 1 {
                p.to_uppercase()
            } else {
                let mut c = p.chars();
                c.next().map(|x| x.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
            }
        })
        .collect::<Vec<_>>()
        .join(" + ")
}

fn pretty_button(name: &str) -> String {
    match name {
        "left" => "左键",
        "right" => "右键",
        "middle" => "中键 (滚轮按下)",
        "back" => "后退侧键",
        "forward" => "前进侧键",
        "scroll-up" => "滚轮上滚",
        "scroll-down" => "滚轮下滚",
        "dpi" => "底部电源 / DPI 键",
        "dpi-up" => "DPI + 键",
        "dpi-down" => "DPI - 键",
        "tilt-left" => "滚轮左倾",
        "tilt-right" => "滚轮右倾",
        "clutch" => "灵敏度离合键 (狙击键)",
        "profile" => "配置文件切换键",
        "scroll-mode" => "滚轮模式切换键",
        other => other,
    }
    .to_string()
}

/// What a button does out of the box on the mouse whose buttons these are: a click is
/// itself, anything else is named after the function the table has for it.
fn default_label(own: &str, buttons: &[PhysicalButton]) -> String {
    let Some(b) = buttons.iter().find(|b| b.name == own).filter(|_| crate::device::button_code(own).is_none()) else {
        return pretty_button(own);
    };
    match (crate::config::spec_for(&b.default), b.default.0) {
        (Some(spec), _) => pretty_spec(Some(&spec), own, buttons),
        // Functions the config has no value for (the Basilisk V3's scroll-mode button and tilts).
        (None, [0x12, ..]) => "滚轮模式切换".into(),
        (None, [0x0e, _, 0x68, ..]) => "向左滚动".into(),
        (None, [0x0e, _, 0x69, ..]) => "向右滚动".into(),
        (None, _) => pretty_button(own),
    }
}

/// Human description of an assignment for the button list.
pub fn pretty_spec(spec: Option<&str>, own: &str, buttons: &[PhysicalButton]) -> String {
    if let Some(s) = spec {
        let (base, rate) = turbo_split(s);
        if let Some(n) = rate {
            return format!("{} · 连发 {n} 次/秒", pretty_spec(Some(&base), own, buttons));
        }
    }
    match spec {
        None | Some("default") => format!("{} (默认)", default_label(own, buttons)),
        Some("disabled") => "已禁用".into(),
        Some("hypershift") => "Hypershift 组合层键".into(),
        Some("double-click") => "左键双击".into(),
        Some("system:sleep") => "系统睡眠".into(),
        Some("system:power") => "系统关机/电源".into(),
        Some("system:wake") => "系统唤醒".into(),
        Some("dpi-cycle") => "DPI 循环切换".into(),
        Some("dpi-up") => "DPI 升高一档".into(),
        Some("dpi-down") => "DPI 降低一档".into(),
        Some(s) if s.starts_with("dpi-stage:") => format!("锁定 DPI 档位 {}", &s[10..]),
        Some(s) if s.starts_with("clutch:") => format!("灵敏度离合 {} DPI", s[7..].replace(':', " x ")),
        Some("profile-next") => "下一板载配置".into(),
        Some("profile-prev") => "上一板载配置".into(),
        Some("profile-cycle-up") => "配置文件向上循环".into(),
        Some("profile-cycle-down") => "配置文件向下循环".into(),
        Some(s) if s.starts_with("mouse:") => pretty_button(&s[6..]),
        Some(s) if s.starts_with("media:") => options_for(CAT_MEDIA, 1).iter().find(|(_, v)| v == s).map(|(l, _)| l.clone()).unwrap_or_else(|| s.to_string()),
        Some("key:") => "键盘按键：请输入键名".into(),
        Some(s) if shortcut_preset_of(s).is_some() => {
            let (label, canon) = shortcut_preset_of(s).unwrap();
            format!("快捷：{label} ({})", pretty_combo(&canon[4..]))
        }
        Some(s) if s.starts_with("key:") => format!("键盘：{}", pretty_combo(&s[4..])),
        Some(s) => s.to_string(),
    }
}

fn customize(ui: &Ui, f: &mut Form) {
    let cfg = &ui.cfg;
    f.section_icon("按键映射 (8 个可编程按键)", Some(Icon::Mouse));
    let mut layers = vec!["标准层".to_string(), "Hypershift 第二层".to_string()];
    layers.extend(cfg.games.iter().map(|g| {
        let l = g.label();
        if l.chars().count() > 20 { format!("{}...", l.chars().take(18).collect::<String>().trim_end()) } else { l }
    }));
    let layer_sel = match ui.layer {
        Layer::Normal => 0,
        Layer::Hypershift => 1,
        Layer::Game(i) => 2 + i.min(cfg.games.len().saturating_sub(1)),
    };
    f.chips_wide("编辑按键层", &layers, Some(layer_sel), Id::Layer);
    f.gap(2);

    // Left: list of the physical buttons. Right: the mouse with clickable areas.
    let list_w = 300;
    let row_h = 50;
    let y0 = f.y;
    let hs = ui.layer == Layer::Hypershift;
    let layer_buttons = ui.layer_buttons();
    let buttons = ui.buttons();
    for (i, b) in buttons.iter().enumerate() {
        let name = b.name;
        let key = if hs { format!("hypershift.{name}") } else { name.to_string() };
        let mut spec = layer_buttons.get(&key).map(String::as_str);
        let mut inherited = false;
        if let Layer::Game(_) = ui.layer {
            if spec.is_none() {
                spec = cfg.buttons.get(&key).map(String::as_str);
                inherited = spec.is_some();
            }
        }
        let mut sub = pretty_spec(spec, name, buttons);
        if inherited {
            sub.push_str(" · 继承自标准层");
        }
        f.push(Id::SelectButton(i), f.x + PAD, f.y, list_w, row_h - 4, Kind::Row { title: pretty_button(name), sub, selected: ui.sel().name == name });
        f.y += row_h;
    }
    let art_h = (row_h * buttons.len() as i32 - 4).min(row_h * 8 - 4);
    let art_x = f.x + PAD + list_w + 24;
    let art_w = f.w - 2 * PAD - list_w - 24;
    let regions = mouse_regions(f, art_x, y0, art_w, art_h, buttons);
    let sel = buttons.iter().position(|b| b.name == ui.sel().name).unwrap_or(0);
    f.push(Id::SelectButton(sel), art_x, y0, art_w, art_h, Kind::MouseArt { highlight: sel, regions });
    if hs && !cfg.buttons.values().any(|v| v == "hypershift") {
        f.gap(6);
        f.note("提示：标准层中尚未将任何按键指定为“Hypershift”，请先在标准层将一个按键设为 Hypershift，按住它时才能触发本层按键。");
    }
    if let Layer::Game(i) = ui.layer {
        f.gap(6);
        let exe = cfg.games.get(i).map(|g| g.label()).unwrap_or_else(|| "?".into());
        f.note(&format!("当前编辑《{exe}》前台运行时的专属按键：保持为“默认”的按键将继续沿用标准层设置，退出游戏后自动恢复。"));
    }

    let name = ui.sel().name;
    let key = ui.button_key();
    let spec = layer_buttons.get(&key).map(String::as_str);
    let cat = if ui.key_mode.as_deref() == Some(key.as_str()) { CAT_KEYBOARD } else { category_of(spec) };
    let (base, rate) = spec.map(turbo_split).unwrap_or((String::new(), None));
    let layer_title = match ui.layer {
        Layer::Normal => String::new(),
        Layer::Hypershift => "（Hypershift 第二层）".to_string(),
        Layer::Game(i) => format!("（{}）", cfg.games.get(i).map(|g| g.label()).unwrap_or_default()),
    };
    f.section_icon(&format!("分配功能：{}{layer_title}", pretty_button(name)), Some(Icon::Edit));
    let cats = CATEGORIES.map(String::from);
    f.chips_wide("功能类别", &cats, Some(cat), Id::Category(0));
    match cat {
        CAT_MOUSE | CAT_DPI | CAT_MEDIA | CAT_SHORTCUT | CAT_SYSTEM => {
            let opts = options_for(cat, cfg.dpi.stages.len());
            let labels: Vec<String> = opts.iter().map(|(l, _)| l.clone()).collect();
            let canon_base = if cat == CAT_SHORTCUT { shortcut_preset_of(&base).map(|(_, s)| s).unwrap_or(base.as_str()) } else { base.as_str() };
            let sel = opts.iter().position(|(_, s)| s == canon_base);
            f.chips_wide("", &labels, sel, Id::Option(0));
            if cat == CAT_MOUSE && base.starts_with("mouse:") {
                turbo_controls(f, rate);
            }
            if cat == CAT_SHORTCUT {
                f.note("直接写入鼠标板载内存（硬件级发送系统组合键）：无需保持后台软件运行，彻底退出程序或插到其他电脑同样永久生效。");
            }
            if cat == CAT_SYSTEM {
                f.note("睡眠、关机与唤醒会发送标准系统电源键指令，具体行为由 Windows 系统电源选项决定。");
            }
        }
        CAT_KEYBOARD => {
            let cur = base.strip_prefix("key:").unwrap_or("");
            f.edit("键盘按键或组合键", "输入后按 Enter 回车键保存", cur, false, Id::KeyEdit);
            if base.starts_with("key:") {
                turbo_controls(f, rate);
            }
            f.note("支持键名：a-z、0-9、f1-f24、enter、esc、tab、space、backspace、delete、home、end、pageup、pagedown、up、down、left、right 及标点键。修饰键支持 ctrl、shift、alt、win，例如 ctrl+shift+c 或 win+d。");
        }
        CAT_HYPERSHIFT => f.note("按住此键不放时激活 Hypershift 第二层：其他所有按键将临时切换为第二层功能。"),
        CAT_DISABLED => f.note("禁用此物理按键（按下无任何响应；底部电源键长按 3 秒仍可正常开关机）。"),
        _ => f.note("恢复该按键的出厂默认功能。所有按键映射、组合键、多媒体键与 DPI 切换均直写鼠标板载内存，无需后台软件即可在任何电脑上生效。"),
    }
}

/// Turbo toggle and rate slider under a mouse or keyboard assignment.
fn turbo_controls(f: &mut Form, rate: Option<u8>) {
    f.toggle("启用连发 (Turbo)", "按住该按键时自动高频重复触发", rate.is_some(), Id::Turbo);
    if let Some(n) = rate {
        f.slider("每秒触发次数", "", TURBO_MIN as i32, TURBO_MAX as i32, 1, n as i32, |v| format!("{v} 次/秒"), Id::TurboRate);
    }
}

fn app(ui: &Ui, f: &mut Form) {
    let cfg = &ui.cfg;
    f.section_icon("系统托盘与开机静默自启", Some(Icon::Program));
    f.toggle(
        "开机静默自启到系统托盘",
        "登录 Windows 时无弹窗、无控制台直接后台驻留托盘（状态栏电量图标与游戏自动切配置；若移动了 exe 位置会自动修复路径）",
        ui.autostart,
        Id::Autostart,
    );
    f.slider(
        "电量刷新间隔",
        "使用鼠标期间托盘读取电量的频率（鼠标休眠时绝不唤醒耗电）",
        10,
        300,
        10,
        cfg.tray.poll_seconds as i32,
        |v| format!("{v} 秒"),
        Id::PollSeconds,
    );
    f.gap(4);
    f.buttons(&[
        ("打开配置文件", Id::OpenConfig, false),
        ("从鼠标重新导入", Id::Reimport, false),
        ("重新写入全部设置", Id::Reapply, false),
        ("立即刷新状态", Id::Refresh, false),
    ]);
    f.note("“从鼠标重新导入”会读取鼠标当前板载状态并更新本地 config.toml；“重新写入全部设置”会将本地配置完整同步至鼠标。");

    f.section_icon("板载配置文件槽位", Some(Icon::Layers));
    let (stored, max) = ui.profiles.unwrap_or((1, 1));
    let slots: Vec<String> = (1..=max.clamp(1, 5)).map(|n| format!("槽位 {n}")).collect();
    f.chips("当前使用的板载配置", "按键映射与动态灵敏度均写入该板载槽位", &slots, Some((cfg.profile.active as usize).clamp(1, slots.len()) - 1), Id::Profile);
    if max <= 1 {
        f.note("炼狱蝰蛇 V4 Pro 硬件内置 1 个板载配置槽位（无需手动切换），多配置切换可通过“游戏配置”页面实现自动无感切换。");
    } else {
        f.note(&format!("该鼠标支持 {max} 个板载槽位（当前已存 {stored} 个）。"));
    }

    if ui.wireless && ui.spec.pairing {
        f.section_icon("接收器无线配对", Some(Icon::Link));
        match &ui.paired {
            Some(list) if !list.is_empty() => {
                for d in list {
                    let state = match d.status {
                        0 => "未连接",
                        1 => "已连接",
                        _ => "状态未知",
                    };
                    f.info("已配对设备", &format!("产品 ID 0x{:04x}  ·  {state}", d.product_id));
                }
            }
            Some(_) => f.info("已配对设备", "未报告"),
            None => f.info("已配对设备", "-"),
        }
        if ui.pairing_since.is_some() {
            f.note("接收器正在配对模式中：请将鼠标靠近接收器并拨动电源开关重启一次。");
            f.buttons(&[("取消配对", Id::PairCancel, true)]);
        } else {
            f.buttons(&[("配对新设备", Id::PairStart, true), (if ui.confirm_unpair { "确认取消配对" } else { "取消配对此鼠标" }, Id::Unpair, false)]);
        }
        if let Some(on) = ui.auto_pair {
            f.toggle("自动配对", "允许接收器自动配对兼容鼠标", on, Id::AutoPair);
        }
        f.note("注意：取消配对会让接收器忘记当前鼠标，仅在需要将鼠标迁移到另一个接收器时使用。");
    }

    f.section_icon("当前硬件信息", Some(Icon::Info));
    let model = display_model(&ui.spec.name);
    let link = if ui.wireless { "通过无线接收器连接" } else { "USB 有线直连" };
    let trust = if ui.spec.verified {
        "  ·  已完整适配验证"
    } else if ui.cfg.device.allow_unverified.allows(ui.spec.product_id) {
        "  ·  测试写入模式"
    } else {
        "  ·  只读保护模式"
    };
    if ui.spec.is_placeholder() {
        f.info("连接设备", "尚未打开鼠标");
    } else {
        f.info("连接设备", &format!("{model}（{link}，PID 0x{:04x}）{trust}", ui.spec.product_id));
    }
    f.info("鼠标固件版本", &ui.fw.map(|(a, b)| format!("v{a}.{b}")).unwrap_or_else(|| "-".into()));
    if ui.wireless {
        f.info("接收器固件版本", &ui.dongle_fw.map(|(a, b)| format!("v{a}.{b}")).unwrap_or_else(|| "-".into()));
    }
    let masked_serial = ui.serial.as_deref().map(crate::probe::mask_serial_text);
    f.info("鼠标序列号", masked_serial.as_deref().unwrap_or("-"));
    f.info("本地配置文件", &redact_user_path(&crate::config_path()));
    f.info(
        "当前连接状态",
        if ui.pending {
            "有待同步的更改（等待鼠标唤醒）"
        } else if ui.asleep {
            "无线休眠中"
        } else if ui.battery.is_some() {
            "在线已连接"
        } else {
            "未连接"
        },
    );
    f.info("软件名称", "Freenapse-DeathAdder V4 Pro");
    f.info("软件版本", &format!("v{}", crate::VERSION));
    f.info("故障诊断", "启动时加 --log 参数可在配置文件目录生成 hid.log 记录每次 USB 通信报文");

    f.section_icon("DeathAdder V4 Pro 专属适配特性", Some(Icon::Check));
    f.bullet(ACCENT, "完整适配 HyperSpeed 二代半球形接收器协议：支持固件/序列号读取、左/中/右 3 颗指示灯独立映射与亮度调节。");
    f.bullet(ACCENT, "精准匹配 Focus Pro 45K 二代传感器非对称抬起截断回读格式、45,000 DPI 上限与有线/无线双 8000 Hz 回报率。");
    f.bullet(ACCENT, "右手人体工学俯视轮廓图示 + 底部 DPI/电源键专属键位交互，全中文界面与托盘菜单。");

    f.section_icon("关于 Freenapse-DeathAdder V4 Pro", Some(Icon::Help));
    f.note("在原版 freenapse 0.5.2 基础上增加适配炼狱蝰蛇 V4 Pro 并作出性能与安全优化，纯原生 Win32 + Rust 编写，单文件零后台服务，核心设置直写鼠标板载内存。");
    f.link("本项目仓库", "github.com/TRAAACER/Freenapse-DeathAdder-V4-Pro", "https://github.com/TRAAACER/Freenapse-DeathAdder-V4-Pro", "");
    f.link("上游原版仓库", "github.com/MatthewSaintBull/freenapse", "https://github.com/MatthewSaintBull/freenapse", "");
    f.info("开源协议", "MIT License · 独立开源项目，与 Razer Inc. 无关联");
    f.note("按 Esc 键可随时关闭此设置窗口，系统托盘图标将继续在后台静默运行。");
}

/// Replace the leading `%USERPROFILE%` directory (e.g. `C:\Users\Alice`) with `%USERPROFILE%`
/// so screenshots of the Settings page never leak the Windows account name.
fn redact_user_path(path: &std::path::Path) -> String {
    let raw = path.to_string_lossy();
    if let Some(home) = std::env::var_os("USERPROFILE").and_then(|v| {
        let s = v.to_string_lossy().into_owned();
        if s.is_empty() { None } else { Some(s) }
    }) && raw.len() >= home.len()
        && raw.is_char_boundary(home.len())
        && raw[..home.len()].eq_ignore_ascii_case(&home)
    {
        return format!("%USERPROFILE%{}", &raw[home.len()..]);
    }
    raw.into_owned()
}

/// Rows an owner has partly tested (reads and some writes), short of verified.
const TESTING: &[u16] = &[0x00B6];

/// How far freenapse goes with one row of the device table.
fn device_status(d: &DeviceSpec) -> Status {
    if d.verified {
        Status::Verified
    } else if TESTING.contains(&d.product_id) {
        Status::Testing
    } else {
        Status::ReadOnly
    }
}

/// One line per model and status: wired and wireless rows of the same mouse
/// share a line when they have the same status. (model, rows)
fn device_models(st: Status) -> Vec<(String, Vec<&'static DeviceSpec>)> {
    let mut out: Vec<(String, Vec<&'static DeviceSpec>)> = Vec::new();
    for d in crate::device::DEVICES.iter().filter(|d| device_status(d) == st) {
        let model = display_model(&d.name);
        match out.iter_mut().find(|(m, _)| *m == model) {
            Some((_, rows)) => rows.push(d),
            None => out.push((model, vec![d])),
        }
    }
    out
}

/// "0x00BE 有线, 0x00BF 无线  ·  45000 DPI  ·  125-8000 Hz  ·  8 键"
fn device_note(rows: &[&DeviceSpec]) -> String {
    let ids: Vec<String> = rows.iter().map(|d| format!("0x{:04X} {}", d.product_id, if d.wireless { "无线" } else { "有线" })).collect();
    let dpi = rows.iter().map(|d| d.dpi_max).max().unwrap_or(0);
    let hz = rows.iter().flat_map(|d| d.polling_rates.iter().copied()).max().unwrap_or(1000);
    let mut parts = vec![ids.join(", "), format!("{dpi} DPI"), format!("125-{hz} Hz")];
    if device_status(rows[0]) != Status::ReadOnly {
        parts.push(format!("{} 键", rows[0].buttons.len()));
    }
    if rows.iter().any(|d| d.dongle == DongleKind::Gen2Dome) {
        parts.push("三灯半球接收器".into());
    }
    if !rows[0].lighting.is_empty() {
        parts.push("RGB 灯效".into());
    }
    if !rows.iter().any(|d| d.battery) {
        parts.push("有线无电池".into());
    }
    parts.join("  \u{b7}  ")
}

/// The device table: verified, being tested, and known but read-only.
fn devices(ui: &Ui, f: &mut Form) {
    let here = |rows: &[&DeviceSpec]| ui.battery.is_some() && rows.iter().any(|d| d.product_id == ui.spec.product_id);

    f.section_icon("当前已连接设备", Some(Icon::Mouse));
    if ui.battery.is_some() {
        let model = display_model(&ui.spec.name);
        let allowed = ui.cfg.device.allow_unverified.allows(ui.spec.product_id);
        let state = match (ui.spec.verified, allowed) {
            (true, _) => "已验证适配，全部设置可直接写入板载内存",
            (false, true) => "未验证型号：已手动开启测试写入权限",
            (false, false) => "未验证型号：当前处于只读保护模式",
        };
        f.info("鼠标型号", &format!("{model}（PID 0x{:04x}）  \u{b7}  {state}", ui.spec.product_id));
    } else {
        f.info("鼠标型号", "当前未检测到连接");
    }
    // The owner's opt-in, per product id ([device] allow_unverified in the config file).
    if !ui.spec.verified && !ui.spec.is_placeholder() {
        let allowed = ui.cfg.device.allow_unverified.allows(ui.spec.product_id);
        let sub = if allowed {
            "已开启：窗口、托盘与命令行中的修改将直接写入此鼠标"
        } else {
            "已关闭：仅读取状态，开启后可对此型号测试写入"
        };
        f.toggle("允许向此鼠标写入设置", sub, allowed, Id::AllowUnverified);
        f.note("该权限仅针对当前连接的产品 ID 生效，其他未验证型号仍保持只读安全状态。");
    }

    let groups: [(&str, Icon, Status, &str); 3] = [
        ("已完整验证", Icon::Check, Status::Verified, "已完成真机逐项写入与回读校验，开箱即用。"),
        ("测试中", Icon::Warning, Status::Testing, "已完成部分功能验证，需在上方手动开启写入开关。"),
        ("已知型号（默认只读）", Icon::Info, Status::ReadOnly, "已内置传感器上限与回报率参数，默认只读保护，开启上方开关后可测试写入。"),
    ];
    for (title, icon, st, text) in groups {
        let rows = device_models(st);
        if rows.is_empty() {
            continue;
        }
        f.section_icon(title, Some(icon));
        f.note(text);
        for (model, specs) in &rows {
            let mut note = device_note(specs);
            if here(specs) {
                note.push_str("  \u{b7}  当前连接");
            }
            f.status(st, model, &note);
        }
    }

    f.section_icon("其他雷蛇鼠标", Some(Icon::Help));
    f.note("不在上表中的雷蛇鼠标将以只读安全模式打开。运行 `freenapse probe` 可扫描其全部寄存器与按键编码。");
}

/// Feature status and release roadmap.
fn roadmap(f: &mut Form) {
    use Status::*;
    f.section_icon("功能支持对照表", Some(Icon::Flag));
    f.note("当前定制版对比 Razer Synapse 4（雷云 4）的各项板载与软件功能支持情况。");

    f.section("性能 (Performance)");
    f.status(Done, "最多 5 档 DPI 与当前档位切换", "支持 100 至 45000 DPI，1 DPI 步进直写板载");
    f.status(Done, "X / Y 轴独立灵敏度", "水平与垂直方向可分别指定不同 DPI");
    f.status(Done, "125 .. 8000 Hz 原生无线/有线回报率", "支持炼狱蝰蛇 V4 Pro 原生双 8K 回报率");
    f.status(Done, "游戏前台自动高回报率", "平时低回报率省电，进游戏自动切 4000 / 8000 Hz");
    f.status(Done, "传感器角度校准 (Rotation)", "-30° 至 +30° 握姿角度微调");
    f.status(Done, "动态灵敏度 (Dynamic Sensitivity)", "经典 / 自然 / 跃迁预设及自定义样条曲线编辑器");
    f.status(Never, "灵敏度匹配器", "纯软件测算工具，直接输入目标 DPI 即可");

    f.section("传感器校准 (Calibration)");
    f.status(Done, "Focus Pro 45K 智能追踪 (1 / 2 / 3 mm)", "对称抬起截断距离板载存储");
    f.status(Done, "非对称抬起截断 (2..26 / 1..25)", "支持炼狱蝰蛇 V4 Pro 非对称抬起与着陆等级双向读写");

    f.section("电源与接收器 (Power)");
    f.status(Done, "实时电量百分比与充电状态", "托盘图标动态绘制电量、窗口大电池卡片");
    f.status(Done, "空闲休眠时长与低功耗阈值", "1..15 分钟自动休眠，5..100% 低电量节能");
    f.status(Done, "HyperSpeed 二代半球形接收器 3 颗指示灯", "左 / 中 / 右三灯独立映射（电量/连接/回报率/DPI/关闭）及 4 档亮度");
    f.status(Done, "HyperPolling 单灯接收器模式", "兼容一代 HyperPolling 接收器");
    f.status(Partial, "接收器无线重新配对", "一代 HyperPolling 支持完整配对；二代半球形接收器对码暂需官方配对工具");

    f.section("按键自定义 (Customize)");
    f.status(Done, "全按键映射（标准层 + Hypershift 层）", "左/右/中/前进/后退/滚轮上下/底部 DPI 键全部板载重映射");
    f.status(Done, "桌面快捷预设（显示桌面 / 截图 / 锁屏等）", "硬件级直写系统组合键，无需后台软件永久生效");
    f.status(Done, "双击、连发 (Turbo)、多媒体与系统电源键", "1..50 次/秒硬件连发、音量静音、睡眠关机");

    f.section("软件体验");
    f.status(Done, "单文件免安装、零后台服务", "仅约 1 MB，内存占用约 10 MB，远离 1.5 GB 电子垃圾");
    f.status(Done, "Steam / Epic / 战网 / Xbox 游戏库自动扫描", "自动提取本地游戏封面生成配置卡片");
    f.status(Done, "有线 USB-C 模式 (PID 0x00BE) 协议适配", "共享 v1.20 鼠标固件指令（0x1f），插线充电时无缝读写板载");
    f.status(Todo, "二代半球形接收器工具内重新对码", "后续研究 0x00BF 专用无线对码序列，免官方工具更换接收器");
    f.status(Never, "雷蛇账号登录、云同步与后台遥测", "完全本地离线运行，保护隐私");
}

// ----------------------------------------------------------------------
// Mouse illustration (top view, DeathAdder V4 Pro ergonomic contour)
// ----------------------------------------------------------------------

/// Geometry of the drawing in logical units relative to the art box.
struct MouseGeom {
    cx: i32,
    top: i32,
    w: i32,
    h: i32,
}

fn geom(x: i32, y: i32, w: i32, h: i32, bottom_dpi: bool) -> MouseGeom {
    let reserve = if bottom_dpi { 44 } else { 20 };
    let mh = (h - reserve).min(290);
    let mw = (mh * 62 / 100).min(w - 160);
    let top_y = y + ((h - reserve - mh).max(8)) / 2;
    MouseGeom { cx: x + w / 2 + 12, top: top_y, w: mw, h: mh }
}

/// True when the mouse has the 8-button layout with its DPI/power button physically underneath
/// (DeathAdder V4 Pro, Viper V3 Pro), rather than behind the scroll wheel (Basilisk V3).
fn has_bottom_dpi(buttons: &[crate::device::PhysicalButton]) -> bool {
    buttons.iter().any(|b| b.name == "dpi") && !buttons.iter().any(|b| b.name == "scroll-mode" || b.name == "tilt-left")
}

/// Right-handed ergonomic contour (DeathAdder V4 Pro top-down silhouette),
/// expressed in (dx_pct_of_half_w, y_pct_of_h) around `g.cx` and `g.top`.
fn ergo_contour(g: &MouseGeom, dx: impl Fn(i32) -> i32, dy: impl Fn(i32) -> i32) -> Vec<POINT> {
    // Asymmetric flared front-left trigger, sculpted left thumb waist, ergonomic right palm swell.
    const PTS: &[(i32, i32)] = &[
        (-12, 0),
        (-36, 2),
        (-72, 7),
        (-96, 15),
        (-104, 24),
        (-100, 34),
        (-92, 44),
        (-89, 54),
        (-94, 66),
        (-100, 77),
        (-96, 87),
        (-80, 94),
        (-52, 99),
        (-18, 100),
        (18, 100),
        (54, 98),
        (82, 92),
        (98, 83),
        (105, 71),
        (104, 58),
        (98, 45),
        (95, 33),
        (95, 21),
        (88, 12),
        (66, 6),
        (34, 2),
        (6, 0),
    ];
    let hw = g.w / 2;
    PTS.iter().map(|(px, py)| POINT { x: dx(g.cx + hw * px / 100), y: dy(g.top + g.h * py / 100) }).collect()
}

/// Clickable regions (button index → scaled rect), computed with the same geometry as the drawing.
fn mouse_regions(f: &Form, x: i32, y: i32, w: i32, h: i32, buttons: &[crate::device::PhysicalButton]) -> Vec<(usize, RECT)> {
    let bottom_dpi = has_bottom_dpi(buttons);
    let g = geom(x, y, w, h, bottom_dpi);
    let s = |v: i32| f.s(v);
    let l = g.cx - g.w / 2;
    let split = g.top + g.h * 42 / 100;
    let wheel_w = g.w / 6;
    let wheel_h = g.h / 7;
    let wheel_y = g.top + g.h * 12 / 100;
    let r = |x: i32, y: i32, w: i32, h: i32| RECT { left: s(x), top: s(y), right: s(x + w), bottom: s(y + h) };
    let mut v: Vec<(usize, RECT)> = extra_shapes(&g, buttons)
        .into_iter()
        .filter_map(|(name, (ex, ey, ew, eh))| buttons.iter().position(|b| b.name == name).map(|i| (i, r(ex, ey, ew, eh))))
        .collect();
    v.extend([
        (0, r(l - 4, g.top, g.w / 2 + 4, split - g.top)),
        (1, r(g.cx, g.top, g.w / 2 + 4, split - g.top)),
        (2, r(g.cx - wheel_w / 2, wheel_y, wheel_w, wheel_h)),
        (5, r(g.cx - wheel_w / 2 - 4, wheel_y - 18, wheel_w + 8, 18)),
        (6, r(g.cx - wheel_w / 2 - 4, wheel_y + wheel_h, wheel_w + 8, 18)),
        (3, r(l - 12, g.top + g.h * 48 / 100, 22, g.h * 13 / 100)),
        (4, r(l - 12, g.top + g.h * 33 / 100, 22, g.h * 13 / 100)),
    ]);
    v
}

type Area = (i32, i32, i32, i32);

fn extra_shapes(g: &MouseGeom, buttons: &[crate::device::PhysicalButton]) -> Vec<(&'static str, Area)> {
    let bottom_dpi = has_bottom_dpi(buttons);
    let dpi_at = if buttons.iter().any(|b| b.name == "scroll-mode") { 37 } else { 32 };
    let l = g.cx - g.w / 2;
    let wheel_w = g.w / 6;
    let wheel_h = g.h / 7;
    let wheel_y = g.top + g.h * 12 / 100;
    let dpi_w = wheel_w - 6;
    let dpi_h = g.h * 4 / 100;
    let dpi_rect = if bottom_dpi {
        let bw = (g.w * 42 / 100).max(56);
        (g.cx - bw / 2, g.top + g.h + 10, bw, 22)
    } else {
        (g.cx - dpi_w / 2, g.top + g.h * dpi_at / 100, dpi_w, dpi_h)
    };
    vec![
        ("tilt-left", (g.cx - wheel_w / 2 - 14, wheel_y + wheel_h / 4, 10, wheel_h / 2)),
        ("tilt-right", (g.cx + wheel_w / 2 + 4, wheel_y + wheel_h / 4, 10, wheel_h / 2)),
        ("scroll-mode", (g.cx - dpi_w / 2, g.top + g.h * 32 / 100, dpi_w, dpi_h)),
        ("dpi", dpi_rect),
        ("dpi-up", (g.cx - dpi_w / 2, g.top + g.h * 32 / 100, dpi_w, dpi_h)),
        ("dpi-down", (g.cx - dpi_w / 2, g.top + g.h * 37 / 100, dpi_w, dpi_h)),
        ("clutch", (l - 12, g.top + g.h * 62 / 100, 16, g.h * 8 / 100)),
    ]
}

pub unsafe fn draw_mouse(ui: &Ui, dc: HDC, r: &RECT, highlight: usize, regions: &[(usize, RECT)]) {
    let s = |v: i32| ui.s(v);
    let scale = ui.scale;
    let bottom_dpi = has_bottom_dpi(ui.buttons());
    let ergo = ui.spec.is_ergonomic();
    // Work in device pixels: regions already are; rebuild the geometry from the rect.
    let (x, y, w, h) = ((r.left as f32 / scale) as i32, 0, ((r.right - r.left) as f32 / scale) as i32, ((r.bottom - r.top) as f32 / scale) as i32);
    let g = geom(x, y, w, h, bottom_dpi);
    let oy = r.top; // device offset for the top of the art box
    let dx = |v: i32| s(v);
    let dy = |v: i32| s(v) + oy;

    let l = g.cx - g.w / 2;
    let rt = g.cx + g.w / 2;
    let split_l = g.top + g.h * 43 / 100;
    let split_r = if ergo { g.top + g.h * 45 / 100 } else { split_l };
    let body = RECT { left: dx(l - 6), top: dy(g.top), right: dx(rt + 6), bottom: dy(g.top + g.h) };
    let hl = |i: usize| if highlight == i { (ACCENT_HI, ACCENT) } else { (rgb(0x46, 0x47, 0x4b), rgb(0x34, 0x35, 0x38)) };

    let btn = |i: usize| -> RECT {
        if i == 0 {
            RECT { left: dx(l - 8), top: dy(g.top), right: dx(g.cx), bottom: dy(split_l) }
        } else {
            RECT { left: dx(g.cx), top: dy(g.top), right: dx(rt + 8), bottom: dy(split_r) }
        }
    };

    if ergo {
        let contour = ergo_contour(&g, dx, dy);
        // Subtle drop shadow under the ergonomic shell.
        let sh_pts: Vec<POINT> = contour.iter().map(|p| POINT { x: p.x, y: p.y + s(4) }).collect();
        clip_polygon(dc, &sh_pts, || alpha_rect(dc, &offset(&body, 0, s(4)), BLACK, 55));
        clip_polygon(dc, &contour, || {
            vgradient(dc, &body, rgb(0x3a, 0x3b, 0x3e), rgb(0x23, 0x24, 0x26));
            for i in 0..2 {
                let (top, bot) = hl(i);
                vgradient(dc, &btn(i), top, bot);
            }
            reflection(dc, &body, 32);
        });
        // Split keyplate seams of the DeathAdder V4 Pro (independent left/right plates).
        polyline(dc, &[POINT { x: dx(g.cx), y: dy(g.top + 3) }, POINT { x: dx(g.cx), y: dy(split_r) }], BG, s(2));
        polyline(
            dc,
            &[
                POINT { x: dx(l - 4), y: dy(split_l - 2) },
                POINT { x: dx(g.cx - g.w / 7), y: dy(split_l + 2) },
                POINT { x: dx(g.cx), y: dy(split_l + 3) },
                POINT { x: dx(g.cx + g.w / 7), y: dy(split_r + 1) },
                POINT { x: dx(rt + 4), y: dy(split_r - 2) },
            ],
            BG,
            s(2),
        );
        polygon(dc, &contour, None, Some((rgb(0x5c, 0x5d, 0x61), s(2))));
    } else {
        let sym_body = RECT { left: dx(l), top: dy(g.top), right: dx(rt), bottom: dy(g.top + g.h) };
        shadow(dc, &sym_body, s(g.w / 2), 4);
        clip_round(dc, &sym_body, s(g.w / 2), || vgradient(dc, &sym_body, rgb(0x3a, 0x3b, 0x3e), rgb(0x24, 0x25, 0x27)));
        let rgn = windows_sys::Win32::Graphics::Gdi::CreateRoundRectRgn(sym_body.left, sym_body.top, sym_body.right + 1, sym_body.bottom + 1, s(g.w), s(g.w));
        windows_sys::Win32::Graphics::Gdi::SelectClipRgn(dc, rgn);
        for i in 0..2 {
            let (top, bot) = hl(i);
            vgradient(dc, &btn(i), top, bot);
        }
        reflection(dc, &sym_body, 34);
        windows_sys::Win32::Graphics::Gdi::SelectClipRgn(dc, 0);
        windows_sys::Win32::Graphics::Gdi::DeleteObject(rgn);
        round(dc, &sym_body, s(g.w / 2), None, Some((rgb(0x5a, 0x5b, 0x5e), s(2))));
        polyline(dc, &[POINT { x: dx(g.cx), y: dy(g.top + 6) }, POINT { x: dx(g.cx), y: dy(split_l) }], BG, s(2));
        polyline(dc, &[POINT { x: dx(l + 2), y: dy(split_l) }, POINT { x: dx(rt - 2), y: dy(split_l) }], BG, s(2));
    }

    // Optical scroll wheel with subtle tactile ribs and scroll up/down arrows.
    let wheel_w = g.w / 6;
    let wheel_h = g.h / 7;
    let wheel_y = g.top + g.h * 12 / 100;
    let wheel = RECT { left: dx(g.cx - wheel_w / 2), top: dy(wheel_y), right: dx(g.cx + wheel_w / 2), bottom: dy(wheel_y + wheel_h) };
    let (wt, wb) = hl(2);
    clip_round(dc, &wheel, s(wheel_w / 2), || {
        vgradient(dc, &wheel, wt, wb);
        let rib_col = if highlight == 2 { ON_ACCENT } else { rgb(0x25, 0x26, 0x28) };
        for step in 1..5 {
            let ry = wheel.top + (wheel.bottom - wheel.top) * step / 5;
            fill(dc, &rect(wheel.left + s(3), ry, wheel.right - wheel.left - s(6), s(1)), rib_col);
        }
    });
    round(dc, &wheel, s(wheel_w / 2), None, Some((BG, s(2))));
    let arrow = |up: bool, col: u32| {
        let cx = dx(g.cx);
        let (y0, y1) = if up { (dy(wheel_y - 6), dy(wheel_y - 14)) } else { (dy(wheel_y + wheel_h + 6), dy(wheel_y + wheel_h + 14)) };
        polygon(dc, &[POINT { x: cx - s(6), y: y0 }, POINT { x: cx + s(6), y: y0 }, POINT { x: cx, y: y1 }], Some(col), None);
    };
    arrow(true, if highlight == 5 { ACCENT } else { rgb(0x66, 0x66, 0x66) });
    arrow(false, if highlight == 6 { ACCENT } else { rgb(0x66, 0x66, 0x66) });

    // Large sculpted side buttons (left thumb groove): forward above back.
    for (i, frac) in [(4usize, 33), (3usize, 48)] {
        let inset_x = if ergo { 4 } else { 0 };
        let sb = RECT { left: dx(l - 9 + inset_x), top: dy(g.top + g.h * frac / 100), right: dx(l + 6 + inset_x), bottom: dy(g.top + g.h * (frac + 13) / 100) };
        let (st, sbt) = hl(i);
        clip_round(dc, &sb, s(4), || vgradient(dc, &sb, st, sbt));
        round(dc, &sb, s(4), None, Some((BG, s(2))));
    }

    // Extra buttons (either behind wheel on Basilisk, or bottom Power/DPI pill below mouse on DeathAdder V4 Pro).
    for (name, (ex, ey, ew, eh)) in extra_shapes(&g, ui.buttons()) {
        let Some(i) = ui.buttons().iter().position(|b| b.name == name) else { continue };
        let rr = RECT { left: dx(ex), top: dy(ey), right: dx(ex + ew), bottom: dy(ey + eh) };
        let (t, b) = hl(i);
        let rad = if name == "dpi" && bottom_dpi { s(11) } else { s(3) };
        clip_round(dc, &rr, rad, || vgradient(dc, &rr, t, b));
        round(dc, &rr, rad, None, Some((if highlight == i { ACCENT_HI } else { LINE }, s(1))));
        if name == "dpi" && bottom_dpi {
            let fg = if highlight == i { ON_ACCENT } else { TEXT };
            text(dc, &rr, "底部 DPI 键", ui.fonts().small, fg, DT_MID);
        }
    }

    // Callout labels with leader lines.
    let label = |i: usize, text_: &str, anchor: POINT, left_side: bool| {
        let col = if highlight == i { ACCENT } else { MUTED };
        let tw = text_width(dc, ui.fonts().small, text_);
        let lx = if left_side { anchor.x - s(24) - tw } else { anchor.x + s(24) };
        let end = POINT { x: if left_side { anchor.x - s(20) } else { anchor.x + s(20) }, y: anchor.y };
        polyline(dc, &[anchor, end], col, s(1));
        circle(dc, anchor.x, anchor.y, s(3), Some(col), None);
        text(dc, &rect(lx, anchor.y - s(9), tw + s(4), s(18)), text_, ui.fonts().small, col, DT_ROW);
    };
    let inset_x = if ergo { 4 } else { 0 };
    label(0, "左键", POINT { x: dx(l + 2), y: dy(g.top + g.h * 23 / 100) }, true);
    label(4, "前进侧键", POINT { x: dx(l - 4 + inset_x), y: dy(g.top + g.h * 39 / 100) }, true);
    label(3, "后退侧键", POINT { x: dx(l - 4 + inset_x), y: dy(g.top + g.h * 54 / 100) }, true);
    label(5, "滚轮上滚", POINT { x: dx(g.cx + 6), y: dy(wheel_y - 10) }, false);
    label(2, "中键", POINT { x: dx(g.cx + wheel_w / 2), y: dy(wheel_y + wheel_h / 2) }, false);
    label(6, "滚轮下滚", POINT { x: dx(g.cx + 6), y: dy(wheel_y + wheel_h + 10) }, false);
    label(1, "右键", POINT { x: dx(rt - 2), y: dy(g.top + g.h * 36 / 100) }, false);
    let _ = regions;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::{BASILISK_V2_BUTTONS, BASILISK_V3_BUTTONS, BUTTONS};

    #[test]
    fn a_default_is_named_after_the_mouse_that_has_the_button() {
        let label = |own: &str, buttons: &[PhysicalButton]| pretty_spec(None, own, buttons);
        assert_eq!(label("left", BUTTONS), "左键 (默认)");
        assert_eq!(label("dpi", BUTTONS), "DPI 循环切换 (默认)");
        // The Basilisk V3 (issue 5): functions without a config value still get one name.
        assert_eq!(label("dpi", BASILISK_V3_BUTTONS), "DPI 循环切换 (默认)");
        assert_eq!(label("scroll-mode", BASILISK_V3_BUTTONS), "滚轮模式切换 (默认)");
        assert_eq!(label("tilt-left", BASILISK_V3_BUTTONS), "向左滚动 (默认)");
        assert_eq!(label("tilt-right", BASILISK_V3_BUTTONS), "向右滚动 (默认)");
        assert_eq!(label("clutch", BASILISK_V3_BUTTONS), "灵敏度离合 400 DPI (默认)");
        assert_eq!(label("clutch", BASILISK_V2_BUTTONS), "灵敏度离合 788 DPI (默认)");
        assert_eq!(pretty_spec(Some("disabled"), "dpi", BASILISK_V3_BUTTONS), "已禁用");
    }

    #[test]
    fn shortcut_presets_round_trip_onboard() {
        for (label, spec) in SHORTCUT_PRESETS {
            let f = parse_button(spec).expect(spec).expect(spec);
            assert_eq!(crate::config::spec_for(&f).as_deref(), Some(spec), "canonical spec mismatch for {label}");
            assert_eq!(category_of(Some(spec)), CAT_SHORTCUT, "{label}");
        }
        assert_eq!(category_of(Some("key:win+shift+s")), CAT_SHORTCUT);
        assert_eq!(pretty_spec(Some("key:win+d"), "forward", BUTTONS), "快捷：显示桌面 (Win + D)");
        assert_eq!(category_of(Some("key:f5")), CAT_KEYBOARD);
    }
}
