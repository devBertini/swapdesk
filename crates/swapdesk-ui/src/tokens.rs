//! Design tokens read by both shells (C6): colours for light and dark, text styles, spacing and radii.
//!
//! Values come from the `.sd` and `.sd.dark` blocks and the inline styles of `docs/design/*.dc.html`; a test reads
//! the design file and compares.

/// An 8-bit sRGB colour with alpha (`a` is 0 to 255).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Color {
    pub const fn rgb(hex: u32) -> Color {
        Color { r: (hex >> 16) as u8, g: (hex >> 8) as u8, b: hex as u8, a: 255 }
    }

    pub const fn rgba(r: u8, g: u8, b: u8, alpha_percent: u8) -> Color {
        Color { r, g, b, a: ((alpha_percent as u16 * 255 + 50) / 100) as u8 }
    }

    pub const fn with_alpha(self, a: f32) -> Color {
        Color { a: (a * 255.0 + 0.5) as u8, ..self }
    }

    pub fn hex(self) -> u32 {
        (u32::from(self.r) << 16) | (u32::from(self.g) << 8) | u32::from(self.b)
    }

    pub fn to_f32(self) -> [f32; 4] {
        [f32::from(self.r) / 255.0, f32::from(self.g) / 255.0, f32::from(self.b) / 255.0, f32::from(self.a) / 255.0]
    }
}

/// The named colours of `.sd` and `.sd.dark`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    pub canvas: Color,
    pub surface: Color,
    pub sunken: Color,
    /// Button and segmented-control fill.
    pub control: Color,
    /// Empty slots and skeletons.
    pub placeholder: Color,
    /// A switch's track when off.
    pub track: Color,
    pub ink: Color,
    pub soft: Color,
    pub line: Color,
    pub link: Color,
    pub link_ink: Color,
    pub link_tint: Color,
    pub ok: Color,
    pub danger: Color,
    pub danger_tint: Color,
    pub remote: Color,
}

pub const LIGHT: Palette = Palette {
    canvas: Color::rgb(0xEEF0F3),
    surface: Color::rgb(0xFFFFFF),
    sunken: Color::rgb(0xF6F7F9),
    control: Color::rgb(0xE7EAEF),
    placeholder: Color::rgb(0xE2E5EA),
    track: Color::rgb(0x8C93A0),
    ink: Color::rgb(0x171A21),
    soft: Color::rgb(0x575E6B),
    line: Color::rgb(0xDFE2E8),
    link: Color::rgb(0x3346D3),
    link_ink: Color::rgb(0xFFFFFF),
    link_tint: Color::rgb(0xE8EBFC),
    ok: Color::rgb(0x1C7C54),
    danger: Color::rgb(0xBF3328),
    danger_tint: Color::rgb(0xFBE9E7),
    remote: Color::rgb(0x2A2F3A),
};

pub const DARK: Palette = Palette {
    canvas: Color::rgb(0x14161B),
    surface: Color::rgb(0x1C1F26),
    sunken: Color::rgb(0x181B21),
    control: Color::rgb(0x2A2E37),
    placeholder: Color::rgb(0x262A32),
    track: Color::rgb(0x6B7281),
    ink: Color::rgb(0xE9EBF0),
    soft: Color::rgb(0xA0A7B4),
    line: Color::rgb(0x2D323C),
    link: Color::rgb(0x8C9BFF),
    link_ink: Color::rgb(0x10142E),
    link_tint: Color::rgb(0x272D55),
    ok: Color::rgb(0x4FC28E),
    danger: Color::rgb(0xFF7A6E),
    danger_tint: Color::rgb(0x3A1F1D),
    remote: Color::rgb(0x0C0E12),
};

pub const fn palette(dark: bool) -> &'static Palette {
    if dark { &DARK } else { &LIGHT }
}

/// Colours that do not change with the theme (the viewer toolbar, the dark cards, the placeholder screen). The toolbar
/// colours are the `--hud-*` variables, the same in both theme blocks.
pub mod fixed {
    use super::Color;
    pub const WHITE: Color = Color::rgb(0xFFFFFF);
    pub const TOOLBAR: Color = Color::rgb(0x1B1E26);
    pub const TOOLBAR_RAISE: Color = Color::rgb(0x2A2F3A);
    pub const TOOLBAR_INK: Color = Color::rgb(0xE9EBF0);
    pub const TOOLBAR_SOFT: Color = Color::rgb(0xA9B0BC);
    pub const TOOLBAR_PRESSED: Color = Color::rgb(0x2C3366);
    pub const TOOLBAR_SEP: Color = Color::rgb(0x3A404C);
    pub const TOOLBAR_LINK: Color = Color::rgb(0x8C9BFF);
    pub const TOOLBAR_DANGER: Color = Color::rgb(0xFF8A80);
    pub const REMOTE_SCREEN: Color = Color::rgb(0x46586A);
    pub const HOST_DESKTOP: Color = Color::rgb(0x3A5566);
    pub const TASKBAR: Color = Color::rgba(23, 26, 33, 90);
    pub const TASKBAR_ITEM: Color = Color::rgba(255, 255, 255, 16);
    pub const TERMINAL_BG: Color = Color::rgb(0x15181E);
    pub const TERMINAL_INK: Color = Color::rgb(0xD6DAE0);
    pub const CARD_DARK_INK: Color = Color::rgb(0xE9EBF0);
    pub const CARD_DARK_SOFT: Color = Color::rgb(0xA0A7B4);
    pub const CARD_DARK_BORDER: Color = Color::rgb(0x4A5160);
    pub const SPINNER: Color = Color::rgb(0x8C9BFF);
    pub const SHADOW_DIALOG: Color = Color::rgba(0, 0, 0, 45);
}

/// A CSS-like text style: size in px, weight, line height as a multiple of the size, tracking in em.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TextStyle {
    pub size: f32,
    pub weight: u16,
    pub line: f32,
    pub tracking: f32,
}

impl TextStyle {
    pub const fn new(size: f32, weight: u16) -> TextStyle {
        TextStyle { size, weight, line: 1.45, tracking: 0.0 }
    }
    pub const fn line(self, line: f32) -> TextStyle {
        TextStyle { line, ..self }
    }
    pub const fn tracking(self, tracking: f32) -> TextStyle {
        TextStyle { tracking, ..self }
    }
}

pub mod text {
    use super::TextStyle;
    pub const BODY: TextStyle = TextStyle::new(14.0, 400);
    pub const NAME: TextStyle = TextStyle::new(14.0, 600);
    pub const BTN: TextStyle = TextStyle::new(14.0, 500);
    pub const BTN_PRIMARY: TextStyle = TextStyle::new(14.0, 600);
    pub const SMALL: TextStyle = TextStyle::new(12.0, 400);
    pub const SMALL_BOLD: TextStyle = TextStyle::new(12.0, 600);
    pub const CAPTION: TextStyle = TextStyle::new(13.0, 400);
    pub const GROUP: TextStyle = TextStyle::new(12.0, 600);
    pub const H1: TextStyle = TextStyle::new(28.0, 600).line(1.2).tracking(-0.015);
    pub const H1_DIALOG: TextStyle = TextStyle::new(22.0, 600).line(1.25).tracking(-0.01);
    pub const H1_BOARD: TextStyle = TextStyle::new(24.0, 600).line(1.45).tracking(-0.015);
    pub const H2: TextStyle = TextStyle::new(17.0, 600).line(1.3);
    pub const H2_STATE: TextStyle = TextStyle::new(17.0, 600).line(1.3);
    pub const H2_CARD: TextStyle = TextStyle::new(18.0, 600).line(1.3);
    pub const LOGO: TextStyle = TextStyle::new(16.0, 700).tracking(-0.01);
    pub const WIRE: TextStyle = TextStyle::new(13.0, 600);
    pub const TAB: TextStyle = TextStyle::new(14.0, 500);
    pub const TAB_SELECTED: TextStyle = TextStyle::new(14.0, 600);
    pub const PAIR_CODE: TextStyle = TextStyle::new(64.0, 600).line(1.0).tracking(0.04);
    pub const TABLE_HEAD: TextStyle = TextStyle::new(12.0, 600);
    pub const BAR: TextStyle = TextStyle::new(13.0, 600);
    pub const MONO: TextStyle = TextStyle::new(13.0, 400).line(1.7);
}

/// Layout numbers of the main window, in CSS pixels.
pub mod layout {
    pub const DESIGN_W: f32 = 1200.0;
    pub const DESIGN_H: f32 = 760.0;
    pub const MIN_W: f32 = 900.0;
    pub const MIN_H: f32 = 620.0;
    pub const SIDEBAR_W: f32 = 272.0;
    pub const BUTTON_H: f32 = 40.0;
    /// `--r-control`, `--r-panel`, `--r-dialog`, `--r-inner`.
    pub const BUTTON_RADIUS: f32 = 8.0;
    pub const PANEL_RADIUS: f32 = 10.0;
    pub const DIALOG_RADIUS: f32 = 12.0;
    pub const INNER_RADIUS: f32 = 4.0;
    pub const DIALOG_W: f32 = 520.0;
    pub const SWITCH_W: f32 = 38.0;
    pub const SWITCH_H: f32 = 22.0;
    pub const FIELD_H: f32 = 36.0;
    pub const VIEWER_BAR_H: f32 = 40.0;
    pub const DRAWER_W: f32 = 448.0;
    /// Pair strip geometry (Visão geral): column width, icon size, wire thickness, arrow size.
    pub const PAIR_COLUMN_W: f32 = 180.0;
    pub const PAIR_ICON: f32 = 64.0;
    pub const PAIR_WIRE: f32 = 3.0;
    pub const PAIR_ARROW_W: f32 = 16.0;
    pub const PAIR_ARROW_H: f32 = 18.0;
    pub const PAIR_COUNTDOWN_S: u16 = 120;
    /// How long "Acordando" waits for the device to appear (F-WAKE-01).
    pub const WAKE_COUNTDOWN_S: u16 = 60;
}

pub const FONT_FAMILY: &str = "Instrument Sans";
pub const FALLBACK_FONT_FAMILY: &str = "Segoe UI";

#[cfg(test)]
mod tests {
    use super::*;

    /// The body of the first `selector{...}` block of the design's stylesheet.
    fn block<'a>(css: &'a str, selector: &str) -> &'a str {
        let start =
            css.find(&format!("{selector}{{")).unwrap_or_else(|| panic!("{selector} missing")) + selector.len() + 1;
        &css[start..start + css[start..].find('}').unwrap()]
    }

    fn var(block: &str, name: &str) -> u32 {
        let key = format!("--{name}:#");
        let at = block.find(&key).unwrap_or_else(|| panic!("--{name} missing")) + key.len();
        u32::from_str_radix(&block[at..at + 6], 16).unwrap()
    }

    fn check(p: &Palette, block: &str) {
        let expect = [
            ("canvas", p.canvas),
            ("surface", p.surface),
            ("sunken", p.sunken),
            ("control", p.control),
            ("placeholder", p.placeholder),
            ("track", p.track),
            ("ink", p.ink),
            ("soft", p.soft),
            ("line", p.line),
            ("link", p.link),
            ("link-ink", p.link_ink),
            ("link-tint", p.link_tint),
            ("ok", p.ok),
            ("danger", p.danger),
            ("danger-tint", p.danger_tint),
            ("remote", p.remote),
        ];
        for (name, color) in expect {
            assert_eq!(color.hex(), var(block, name), "--{name}");
        }
    }

    /// Every board with theme blocks (the two dark previews embed the others and carry none).
    const BOARDS: [&str; 19] = [
        include_str!("../../../docs/design/Ajustes.dc.html"),
        include_str!("../../../docs/design/Arquivos.dc.html"),
        include_str!("../../../docs/design/Atividade.dc.html"),
        include_str!("../../../docs/design/Bandeja.dc.html"),
        include_str!("../../../docs/design/Banner.dc.html"),
        include_str!("../../../docs/design/Chat.dc.html"),
        include_str!("../../../docs/design/Estados.dc.html"),
        include_str!("../../../docs/design/EstadosDispositivo.dc.html"),
        include_str!("../../../docs/design/EstadosPareamento.dc.html"),
        include_str!("../../../docs/design/Inversao.dc.html"),
        include_str!("../../../docs/design/Main.dc.html"),
        include_str!("../../../docs/design/MainMac.dc.html"),
        include_str!("../../../docs/design/Pareamento.dc.html"),
        include_str!("../../../docs/design/PedidoEntrada.dc.html"),
        include_str!("../../../docs/design/Permissoes.dc.html"),
        include_str!("../../../docs/design/PrimeiraVez.dc.html"),
        include_str!("../../../docs/design/Terminal.dc.html"),
        include_str!("../../../docs/design/Visualizador.dc.html"),
        include_str!("../../../docs/design/VisualizadorTelaCheia.dc.html"),
    ];

    fn px(block: &str, name: &str) -> f32 {
        let key = format!("--{name}:");
        let at = block.find(&key).unwrap_or_else(|| panic!("--{name} missing")) + key.len();
        let end = at + block[at..].find("px").unwrap_or_else(|| panic!("--{name} is not in px"));
        block[at..end].parse().unwrap()
    }

    #[test]
    fn palettes_match_the_design_files() {
        for file in BOARDS {
            check(&LIGHT, block(file, ".sd"));
            check(&DARK, block(file, ".sd.dark"));
            for theme in [block(file, ".sd"), block(file, ".sd.dark")] {
                for (name, color) in [
                    ("hud", fixed::TOOLBAR),
                    ("hud-raise", fixed::TOOLBAR_RAISE),
                    ("hud-ink", fixed::TOOLBAR_INK),
                    ("hud-soft", fixed::TOOLBAR_SOFT),
                    ("hud-line", fixed::TOOLBAR_SEP),
                    ("hud-on", fixed::TOOLBAR_PRESSED),
                    ("hud-link", fixed::TOOLBAR_LINK),
                    ("hud-danger", fixed::TOOLBAR_DANGER),
                    ("term-ink", fixed::TERMINAL_INK),
                ] {
                    assert_eq!(color.hex(), var(theme, name), "--{name}");
                }
            }
            let light = block(file, ".sd");
            assert_eq!(fixed::TERMINAL_BG.hex(), var(light, "term"), "--term (light)");
            for (name, value) in [
                ("r-control", layout::BUTTON_RADIUS),
                ("r-panel", layout::PANEL_RADIUS),
                ("r-dialog", layout::DIALOG_RADIUS),
                ("r-inner", layout::INNER_RADIUS),
            ] {
                assert_eq!(px(light, name), value, "--{name}");
            }
        }
    }

    #[test]
    fn text_styles_match_the_design_css() {
        let main = include_str!("../../../docs/design/Main.dc.html");
        assert!(main.contains("h1{margin:0;font-size:28px;font-weight:600;letter-spacing:-0.015em;line-height:1.2}"));
        assert_eq!((text::H1.size, text::H1.weight, text::H1.line, text::H1.tracking), (28.0, 600, 1.2, -0.015));
        assert!(main.contains("h2{margin:0;font-size:17px;font-weight:600;line-height:1.3}"));
        assert_eq!((text::H2.size, text::H2.weight, text::H2.line), (17.0, 600, 1.3));
        assert!(main.contains("font-size:14px;line-height:1.45"));
        assert_eq!((text::BODY.size, text::BODY.line), (14.0, 1.45));
        assert!(main.contains(".sd .btn{height:40px;padding:0 16px;border-radius:var(--r-control)"));
        assert_eq!(layout::BUTTON_H, 40.0);
        assert!(main.contains("width:272px"));
        assert_eq!(layout::SIDEBAR_W, 272.0);
        assert!(main.contains(".sd .switch{width:38px;height:22px;border-radius:11px"));
        assert!(main.contains("Instrument Sans"));
        let pair = include_str!("../../../docs/design/Pareamento.dc.html");
        assert!(pair.contains("font-size: 64px; font-weight: 600; letter-spacing: 0.04em"));
        assert_eq!((text::PAIR_CODE.size, text::PAIR_CODE.tracking), (64.0, 0.04));
    }

    #[test]
    fn fixed_colours_match_the_design() {
        let viewer = include_str!("../../../docs/design/Visualizador.dc.html");
        assert!(viewer.contains("#46586A"));
        assert_eq!(fixed::REMOTE_SCREEN.hex(), 0x46586A);
    }

    #[test]
    fn alpha_helpers() {
        assert_eq!(Color::rgba(0, 0, 0, 100).a, 255);
        assert_eq!(Color::rgb(0x112233).with_alpha(0.5).a, 128);
    }
}
