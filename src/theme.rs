//! 统一视觉主题：配色、圆角、间距与可复用组件。
//!
//! 主窗口与远控窗口共用这里的设计令牌，避免两处各写一套颜色导致风格漂移。

use egui::{Color32, CornerRadius, Frame, Margin, RichText, Stroke, Ui, Vec2};

// ---------- 配色 ----------
/// 视口底色（最底层）
pub const BG: Color32 = Color32::from_rgb(14, 16, 21);
/// 面板底色
pub const PANEL: Color32 = Color32::from_rgb(20, 23, 29);
/// 卡片底色
pub const CARD: Color32 = Color32::from_rgb(27, 31, 39);
/// 卡片 hover / 选中底色
pub const CARD_HOVER: Color32 = Color32::from_rgb(35, 40, 50);
/// 内嵌块（输入框、代码）底色
pub const SUNKEN: Color32 = Color32::from_rgb(17, 19, 24);
/// 描边
pub const BORDER: Color32 = Color32::from_rgb(43, 49, 60);
/// 强调描边
pub const BORDER_STRONG: Color32 = Color32::from_rgb(58, 66, 80);

/// 主文字
pub const TEXT: Color32 = Color32::from_rgb(232, 236, 243);
/// 次级文字
pub const TEXT_DIM: Color32 = Color32::from_rgb(152, 162, 179);
/// 弱化文字
pub const TEXT_FAINT: Color32 = Color32::from_rgb(107, 116, 128);

/// 品牌强调色（青绿）
pub const ACCENT: Color32 = Color32::from_rgb(45, 212, 191);
/// 成功 / 在线
pub const SUCCESS: Color32 = Color32::from_rgb(62, 213, 152);
/// 警告 / 被控中
pub const WARN: Color32 = Color32::from_rgb(245, 165, 36);
/// 危险 / 断开
pub const DANGER: Color32 = Color32::from_rgb(242, 84, 91);

// ---------- 形状 ----------
pub const R_SM: CornerRadius = CornerRadius::same(6);
pub const R_MD: CornerRadius = CornerRadius::same(10);
pub const R_LG: CornerRadius = CornerRadius::same(14);
/// 胶囊圆角。CornerRadius 分量是 u8，取最大值即可（绘制时会按矩形尺寸收敛）
pub const R_PILL: CornerRadius = CornerRadius::same(255);

// ---------- 间距 ----------
pub const PAD: f32 = 12.0;

/// 把整套主题装到 egui 上（应在安装字体之后调用）。
pub fn apply(ctx: &egui::Context) {
    ctx.all_styles_mut(|style| apply_to_style(style));
}

fn apply_to_style(style: &mut egui::Style) {
    let v = &mut style.visuals;
    v.dark_mode = true;
    v.panel_fill = PANEL;
    v.window_fill = PANEL;
    v.faint_bg_color = SUNKEN;
    v.extreme_bg_color = BG;
    v.window_corner_radius = R_LG;
    v.menu_corner_radius = R_MD;
    v.window_stroke = Stroke::new(1.0, BORDER);
    v.hyperlink_color = ACCENT;

    v.widgets.noninteractive.bg_fill = CARD;
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, TEXT);
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, BORDER);
    v.widgets.noninteractive.corner_radius = R_SM;

    v.widgets.inactive.bg_fill = CARD;
    v.widgets.inactive.fg_stroke = Stroke::new(1.0, TEXT_DIM);
    v.widgets.inactive.bg_stroke = Stroke::new(1.0, BORDER);
    v.widgets.inactive.corner_radius = R_SM;

    v.widgets.hovered.bg_fill = CARD_HOVER;
    v.widgets.hovered.fg_stroke = Stroke::new(1.0, TEXT);
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, BORDER_STRONG);
    v.widgets.hovered.corner_radius = R_SM;

    v.widgets.active.bg_fill = CARD_HOVER;
    v.widgets.active.fg_stroke = Stroke::new(1.0, TEXT);
    v.widgets.active.bg_stroke = Stroke::new(1.0, ACCENT);
    v.widgets.active.corner_radius = R_SM;

    v.widgets.open = v.widgets.hovered;

    v.selection.bg_fill = Color32::from_rgba_unmultiplied(45, 212, 191, 60);
    v.selection.stroke = Stroke::new(1.0, ACCENT);

    style.spacing.item_spacing = Vec2::new(8.0, 6.0);
    style.spacing.button_padding = Vec2::new(12.0, 6.0);

    for (key, size) in [
        (egui::TextStyle::Small, 12.0),
        (egui::TextStyle::Body, 14.0),
        (egui::TextStyle::Button, 14.0),
        (egui::TextStyle::Heading, 20.0),
        (egui::TextStyle::Monospace, 13.0),
    ] {
        if let Some(f) = style.text_styles.get_mut(&key) {
            *f = egui::FontId::proportional(size);
        }
    }
}

// ---------- 组件 ----------

/// 标准卡片（带描边与内边距）。
pub fn card() -> Frame {
    Frame::new()
        .fill(CARD)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(R_MD)
        .inner_margin(Margin::same(PAD as i8))
        .outer_margin(Margin::symmetric(0, 4))
}

/// 无描边的内嵌块（放输入框等）。
pub fn sunken() -> Frame {
    Frame::new()
        .fill(SUNKEN)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(R_SM)
        .inner_margin(Margin::same(6))
}

/// 分组小标题。用 TEXT_DIM 而非 TEXT_FAINT：12px 小字配最暗灰在
/// 深色底上对比度不足，实测发虚。
pub fn section_title(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text).size(12.0).color(TEXT_DIM).strong());
}

/// 生成半透明底色。
///
/// 必须用 `from_rgba_unmultiplied`：egui 的 Color32 内部存的是**已预乘 alpha**
/// 的分量。若把未预乘的 r/g/b 传给 `from_rgba_premultiplied`，低 alpha 的背景
/// 会被渲染成高亮饱和色块，同色文字直接糊在背景上看不清。
pub fn tint(color: Color32, alpha: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), alpha)
}

/// 带柔和光晕的状态点（在线指示）。
pub fn dot_glow(ui: &mut Ui, color: Color32, radius: f32) {
    let (rect, _) =
        ui.allocate_exact_size(Vec2::splat(radius * 4.0), egui::Sense::hover());
    let c = rect.center();
    ui.painter()
        .circle_filled(c, radius * 2.0, tint(color, 45));
    ui.painter().circle_filled(c, radius, color);
}

/// 胶囊标签：淡色底 + 同色文字（底色与文字都取自 `color`）。
pub fn pill(ui: &mut Ui, text: impl Into<String>, color: Color32) {
    let text: String = text.into();
    let galley = ui.painter().layout_no_wrap(
        text,
        egui::FontId::proportional(12.0),
        color,
    );
    let size = galley.size() + Vec2::new(16.0, 8.0);
    let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
    ui.painter().rect_filled(
        rect,
        R_PILL,
        tint(color, 42),
    );
    ui.painter().galley(
        egui::pos2(rect.min.x + 8.0, rect.center().y - galley.size().y / 2.0),
        galley,
        color,
    );
}

/// 主操作按钮（实心强调色）。
pub fn primary_button(text: &str) -> egui::Button<'static> {
    egui::Button::new(RichText::new(text).color(Color32::from_rgb(10, 24, 22)).strong())
        .fill(ACCENT)
        .corner_radius(R_SM)
        .min_size(Vec2::new(72.0, 28.0))
}

/// 次级按钮（描边式）。
pub fn ghost_button(text: &str) -> egui::Button<'static> {
    egui::Button::new(RichText::new(text).color(TEXT_DIM))
        .fill(CARD)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(R_SM)
        .min_size(Vec2::new(64.0, 28.0))
}

/// 危险按钮。
pub fn danger_button(text: &str) -> egui::Button<'static> {
    egui::Button::new(RichText::new(text).color(DANGER))
        .fill(Color32::from_rgba_unmultiplied(242, 84, 91, 34))
        .stroke(Stroke::new(1.0, Color32::from_rgba_unmultiplied(242, 84, 91, 150)))
        .corner_radius(R_SM)
        .min_size(Vec2::new(64.0, 28.0))
}

/// 小号图标按钮（工具栏用）。
pub fn tool_button(text: &str, enabled: bool) -> egui::Button<'static> {
    egui::Button::new(RichText::new(text).size(13.0))
        .fill(CARD)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(R_SM)
        .min_size(Vec2::new(0.0, 30.0))
        .sense(if enabled {
            egui::Sense::click()
        } else {
            egui::Sense::hover()
        })
}

/// 滑动开关（egui 内置只有复选框，这里做一个带缓动的服务开关）。
pub fn toggle(ui: &mut Ui, id: egui::Id, on: &mut bool) -> egui::Response {
    let size = Vec2::new(44.0, 24.0);
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
    if resp.clicked() {
        *on = !*on;
    }
    let t = ui.ctx().animate_bool(id, *on);
    let bg = if *on { tint(ACCENT, 210) } else { BORDER_STRONG };
    ui.painter().rect_filled(rect, R_PILL, bg);
    let r = size.y / 2.0 - 3.0;
    let x = rect.left() + 3.0 + r + t * (rect.width() - 6.0 - r * 2.0);
    ui.painter()
        .circle_filled(egui::pos2(x, rect.center().y), r, Color32::WHITE);
    resp
}

/// 空状态占位（图标 + 标题 + 说明）。
pub fn empty_state(ui: &mut Ui, icon: &str, title: &str, subtitle: &str) {
    ui.vertical_centered(|ui| {
        ui.add_space(28.0);
        ui.label(RichText::new(icon).size(34.0).color(TEXT_FAINT));
        ui.add_space(8.0);
        ui.label(RichText::new(title).size(14.0).color(TEXT_DIM).strong());
        ui.add_space(2.0);
        ui.label(
            RichText::new(subtitle)
                .size(12.0)
                .color(TEXT_FAINT),
        );
        ui.add_space(28.0);
    });
}

/// 带加载动画的等待提示。
pub fn spinner_text(ui: &mut Ui, text: &str) {
    ui.vertical_centered(|ui| {
        ui.add(egui::Spinner::new().size(22.0).color(ACCENT));
        ui.add_space(10.0);
        ui.label(RichText::new(text).size(13.0).color(TEXT_DIM));
    });
}

/// 细分隔线（比 ui.separator() 更淡）。
pub fn hairline(ui: &mut Ui) {
    let rect = ui.available_rect_before_wrap();
    let y = rect.top();
    ui.painter().hline(rect.x_range(), y, Stroke::new(1.0, BORDER));
    ui.add_space(6.0);
}
