//! テーマ（ライト / ダーク / OS に合わせる）。
//!
//! - 選んだ内容は `localStorage` の [`STORAGE_KEY`] に保存する（`light` / `dark` / `system`。無い・不正なら `system`）。
//!   保存するのはテーマの設定だけ（セッショントークンや投票の内容は、これまでどおり、ブラウザに保存しない）。
//! - `<html data-theme="light|dark">` には、「決まった後の値」を設定する。`system` のときは、OS の
//!   `prefers-color-scheme` で決める。WASM の読み込みより前の設定と、OS の設定の変更への追従は、`index.html` の
//!   インラインスクリプトが行う（同じ規則。下のテストが、キーと属性名の一致と、置き場所を確認する）。
//! - 色・余白などは、`style.css` のデザイントークン。ここのテストが、そのトークンから、WCAG のコントラスト比を計算する。
//!
//! 純粋な部分（保存値の解釈・テーマの決定）と、ブラウザ API の薄い層（`load_mode` / `apply`）に分ける。

use leptos::prelude::window;

/// `localStorage` のキー。`index.html` のインラインスクリプトと同じ値にする。
pub const STORAGE_KEY: &str = "theme";
/// `<html>` に設定する属性の名前。`style.css` の `[data-theme="dark"]` と対応する。
pub const ATTRIBUTE: &str = "data-theme";
/// OS の設定を調べるメディアクエリ。
const DARK_QUERY: &str = "(prefers-color-scheme: dark)";

/// 利用者が選ぶ内容。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Light,
    Dark,
    /// OS の設定に合わせる（既定）。
    System,
}

impl Mode {
    /// 切り替えボタンの並び順。
    pub const ALL: [Mode; 3] = [Mode::Light, Mode::Dark, Mode::System];

    /// 保存する値。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Light => "light",
            Self::Dark => "dark",
            Self::System => "system",
        }
    }

    /// 保存された値を読む。不正な値は `None`。
    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|m| m.as_str() == raw)
    }

    /// 切り替えボタンの表示。
    pub fn label(self) -> &'static str {
        match self {
            Self::Light => "ライト",
            Self::Dark => "ダーク",
            Self::System => "OSに合わせる",
        }
    }
}

/// 実際に適用するテーマ。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Theme {
    Light,
    Dark,
}

impl Theme {
    /// `data-theme` の値。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Light => "light",
            Self::Dark => "dark",
        }
    }
}

/// 保存された値（無ければ `None`）から、選んだ内容を決める。無い・不正な値は `System`。
pub fn mode_from_storage(raw: Option<&str>) -> Mode {
    raw.and_then(Mode::parse).unwrap_or(Mode::System)
}

/// 選んだ内容と OS の設定から、適用するテーマを決める。OS の設定に従うのは `System` のときだけ。
pub fn resolve(mode: Mode, os_prefers_dark: bool) -> Theme {
    match mode {
        Mode::Light => Theme::Light,
        Mode::Dark => Theme::Dark,
        Mode::System if os_prefers_dark => Theme::Dark,
        Mode::System => Theme::Light,
    }
}

// ---------------------------------------------------------------------------
// ブラウザ API の薄い層。`localStorage` が使えない（プライベートモードなど）ときも、画面は壊れない。
// ---------------------------------------------------------------------------

/// 保存された選択を読む。読めなければ `System`。
pub fn load_mode() -> Mode {
    let raw = window()
        .local_storage()
        .ok()
        .flatten()
        .and_then(|storage| storage.get_item(STORAGE_KEY).ok().flatten());
    mode_from_storage(raw.as_deref())
}

/// 選択を保存して、`<html data-theme>` に反映する。
pub fn apply(mode: Mode) {
    let window = window();
    // 保存できなくても（容量・プライベートモード）、今の画面には反映する。
    if let Ok(Some(storage)) = window.local_storage() {
        let _ = storage.set_item(STORAGE_KEY, mode.as_str());
    }
    let os_prefers_dark = window
        .match_media(DARK_QUERY)
        .ok()
        .flatten()
        .is_some_and(|query| query.matches());
    let theme = resolve(mode, os_prefers_dark);
    if let Some(root) = window.document().and_then(|d| d.document_element()) {
        let _ = root.set_attribute(ATTRIBUTE, theme.as_str());
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    const CSS: &str = include_str!("../style.css");
    const INDEX_HTML: &str = include_str!("../index.html");

    // --- 純粋なロジック ---

    #[test]
    fn stored_values_are_parsed_and_anything_else_means_system() {
        assert_eq!(mode_from_storage(Some("light")), Mode::Light);
        assert_eq!(mode_from_storage(Some("dark")), Mode::Dark);
        assert_eq!(mode_from_storage(Some("system")), Mode::System);
        for bad in [None, Some(""), Some("Dark"), Some("auto"), Some("light ")] {
            assert_eq!(mode_from_storage(bad), Mode::System, "{bad:?}");
        }
        // 保存する値は、読み戻せる。
        for mode in Mode::ALL {
            assert_eq!(Mode::parse(mode.as_str()), Some(mode));
        }
    }

    #[test]
    fn only_system_follows_the_os_setting() {
        assert_eq!(resolve(Mode::Light, true), Theme::Light);
        assert_eq!(resolve(Mode::Dark, false), Theme::Dark);
        assert_eq!(resolve(Mode::System, true), Theme::Dark);
        assert_eq!(resolve(Mode::System, false), Theme::Light);
        assert_eq!(Theme::Dark.as_str(), "dark");
        assert_eq!(Theme::Light.as_str(), "light");
    }

    #[test]
    fn the_switch_offers_light_dark_and_follow_the_os() {
        let labels: Vec<&str> = Mode::ALL.into_iter().map(Mode::label).collect();
        assert_eq!(labels, ["ライト", "ダーク", "OSに合わせる"]);
    }

    // --- index.html のインラインスクリプト（node が無いので実行はせず、規則の一致と置き場所を確認する）---

    #[test]
    fn the_inline_script_in_the_head_uses_the_same_key_and_attribute_before_the_stylesheet() {
        let script = INDEX_HTML
            .find("<script>")
            .expect("インラインスクリプトがない");
        let head_end = INDEX_HTML.find("</head>").expect("</head>");
        let stylesheet = INDEX_HTML
            .find("rel=\"css\"")
            .expect("スタイルシートのリンク");
        let wasm = INDEX_HTML.find("rel=\"rust\"").expect("WASM のリンク");
        assert!(script < head_end, "スクリプトは <head> の中に置く");
        assert!(
            script < stylesheet && script < wasm,
            "スタイルシートと WASM より前に置く"
        );
        let body = &INDEX_HTML[script
            ..INDEX_HTML[script..]
                .find("</script>")
                .map_or(head_end, |i| script + i)];
        assert!(
            body.contains(&format!("var KEY = \"{STORAGE_KEY}\";")),
            "{body}"
        );
        assert!(
            body.contains(&format!("var ATTR = \"{ATTRIBUTE}\";")),
            "{body}"
        );
        // 3 つの値と、OS の設定の問い合わせ・変更への追従。
        for needle in [
            "\"light\"",
            "\"dark\"",
            "\"system\"",
            DARK_QUERY,
            "\"change\"",
        ] {
            assert!(body.contains(needle), "{needle}");
        }
        // localStorage が使えない環境でも壊れない。
        assert!(body.contains("try {") && body.contains("catch"));
    }

    // --- CSS のデザイントークン ---

    /// コメントを除く。
    fn strip_comments(css: &str) -> String {
        let mut out = String::with_capacity(css.len());
        let mut rest = css;
        while let Some(start) = rest.find("/*") {
            out.push_str(&rest[..start]);
            match rest[start..].find("*/") {
                Some(end) => rest = &rest[start + end + 2..],
                None => return out,
            }
        }
        out.push_str(rest);
        out
    }

    /// `selector { … }` の中身（ネストのないブロック）。ちょうど 1 つだけあること。
    fn block(css: &str, selector: &str) -> String {
        let open = format!("{selector} {{");
        let starts: Vec<usize> = css.match_indices(&open).map(|(i, _)| i).collect();
        assert_eq!(
            starts.len(),
            1,
            "{selector} のブロックが 1 つではありません"
        );
        let body = &css[starts[0] + open.len()..];
        body[..body.find('}').expect("閉じ括弧")].to_string()
    }

    fn declarations(block: &str) -> BTreeMap<String, String> {
        block
            .split(';')
            .filter_map(|d| {
                let (name, value) = d.split_once(':')?;
                Some((name.trim().to_string(), value.trim().to_string()))
            })
            .filter(|(name, _)| name.starts_with("--"))
            .collect()
    }

    fn tokens() -> (BTreeMap<String, String>, BTreeMap<String, String>) {
        let css = strip_comments(CSS);
        (
            declarations(&block(&css, ":root")),
            declarations(&block(&css, "[data-theme=\"dark\"]")),
        )
    }

    fn color_names(map: &BTreeMap<String, String>) -> Vec<&String> {
        map.keys()
            .filter(|k| k.starts_with("--color-") && *k != "--color-scheme")
            .collect()
    }

    #[test]
    fn the_root_defines_colors_spacing_radius_and_font_size_tokens() {
        let (root, _) = tokens();
        for prefix in ["--color-", "--space-", "--radius-", "--font-size-"] {
            assert!(
                root.keys().any(|k| k.starts_with(prefix)),
                "{prefix} のトークンがありません"
            );
        }
        for name in [
            "--color-bg",
            "--color-surface",
            "--color-surface-alt",
            "--color-text",
            "--color-text-muted",
            "--color-border",
            "--color-border-strong",
            "--color-accent",
            "--color-on-accent",
            "--color-danger",
            "--color-ok",
            "--color-focus",
        ] {
            assert!(root.contains_key(name), "{name}");
        }
    }

    #[test]
    fn the_dark_theme_overrides_only_color_tokens_and_all_of_them() {
        let (root, dark) = tokens();
        assert!(
            dark.keys().all(|k| k.starts_with("--color-")),
            "ダークで、色以外のトークンを上書きしています: {dark:?}"
        );
        let root_colors: Vec<_> = color_names(&root);
        let dark_colors: Vec<_> = color_names(&dark);
        assert_eq!(
            root_colors, dark_colors,
            "ダークが、ライトと同じ色のトークンをすべて定義していません"
        );
        // color-scheme も、トークンで切り替える（フォームの部品・スクロールバーが追従する）。
        assert_eq!(
            root.get("--color-scheme").map(String::as_str),
            Some("light")
        );
        assert_eq!(dark.get("--color-scheme").map(String::as_str), Some("dark"));
    }

    type Rgb = (u8, u8, u8);

    fn parse_hex(value: &str) -> Option<Rgb> {
        let hex = value.strip_prefix('#')?;
        let expanded: String = match hex.len() {
            3 => hex.chars().flat_map(|c| [c, c]).collect(),
            6 => hex.to_string(),
            _ => return None,
        };
        let byte = |i: usize| u8::from_str_radix(&expanded[i..i + 2], 16).ok();
        Some((byte(0)?, byte(2)?, byte(4)?))
    }

    /// WCAG 2.x の相対輝度。
    fn luminance((r, g, b): Rgb) -> f64 {
        let channel = |c: u8| {
            let c = f64::from(c) / 255.0;
            if c <= 0.03928 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(r) + 0.7152 * channel(g) + 0.0722 * channel(b)
    }

    /// WCAG 2.x のコントラスト比（1〜21）。
    fn contrast(a: Rgb, b: Rgb) -> f64 {
        let (la, lb) = (luminance(a), luminance(b));
        let (light, dark) = if la >= lb { (la, lb) } else { (lb, la) };
        (light + 0.05) / (dark + 0.05)
    }

    fn color(map: &BTreeMap<String, String>, name: &str) -> Rgb {
        let value = map
            .get(name)
            .unwrap_or_else(|| panic!("{name} がありません"));
        parse_hex(value).unwrap_or_else(|| {
            panic!("{name}: {value} は 16 進数の色（#rgb / #rrggbb）ではありません")
        })
    }

    #[test]
    fn the_contrast_formula_matches_the_wcag_reference_values() {
        assert!((contrast((0, 0, 0), (255, 255, 255)) - 21.0).abs() < 1e-9);
        assert!((contrast((255, 255, 255), (255, 255, 255)) - 1.0).abs() < 1e-9);
        // #777777 と白は、約 4.48（AA に届かない有名な例）。#767676 は約 4.54（届く）。
        assert!((contrast((0x77, 0x77, 0x77), (255, 255, 255)) - 4.48).abs() < 0.01);
        assert!((contrast((0x76, 0x76, 0x76), (255, 255, 255)) - 4.54).abs() < 0.01);
        assert_eq!(parse_hex("#fff"), Some((255, 255, 255)));
        assert_eq!(parse_hex("#1d4ed8"), Some((0x1d, 0x4e, 0xd8)));
        assert_eq!(parse_hex("white"), None);
        assert_eq!(parse_hex("#12345"), None);
    }

    #[test]
    fn color_tokens_are_plain_hex_values() {
        let (root, dark) = tokens();
        for (theme, map) in [("light", &root), ("dark", &dark)] {
            for name in color_names(map) {
                assert!(
                    parse_hex(&map[name]).is_some(),
                    "{theme}: {name} = {} は 16 進数の色ではありません",
                    map[name]
                );
            }
        }
    }

    /// 文字色と、その上に置く背景色（両方のテーマで、通常の文字の AA = 4.5:1 以上）。
    const TEXT_ON: &[(&str, &[&str])] = &[
        (
            "--color-text",
            &["--color-bg", "--color-surface", "--color-surface-alt"],
        ),
        (
            "--color-text-muted",
            &["--color-bg", "--color-surface", "--color-surface-alt"],
        ),
        (
            "--color-accent",
            &["--color-bg", "--color-surface", "--color-surface-alt"],
        ),
        (
            "--color-danger",
            &["--color-bg", "--color-surface", "--color-surface-alt"],
        ),
        (
            "--color-ok",
            &["--color-bg", "--color-surface", "--color-surface-alt"],
        ),
        // ボタン: 主なボタンの文字（アクセント色の背景の上）。
        ("--color-on-accent", &["--color-accent"]),
    ];

    /// 枠線・フォーカスと、その隣の背景色（文字ではない部品の境界: WCAG 1.4.11 の 3:1 以上）。
    const BOUNDARY_ON: &[(&str, &[&str])] = &[
        (
            "--color-border-strong",
            &["--color-bg", "--color-surface", "--color-surface-alt"],
        ),
        (
            "--color-focus",
            &["--color-bg", "--color-surface", "--color-surface-alt"],
        ),
    ];

    fn assert_pairs(pairs: &[(&str, &[&str])], minimum: f64) {
        let (root, dark) = tokens();
        for (theme, map) in [("light", &root), ("dark", &dark)] {
            for (fg, backgrounds) in pairs {
                for bg in *backgrounds {
                    let ratio = contrast(color(map, fg), color(map, bg));
                    assert!(
                        ratio >= minimum,
                        "{theme}: {fg}（{}）/ {bg}（{}）のコントラスト比が {ratio:.2} で、{minimum}:1 に届きません",
                        map[*fg],
                        map[*bg]
                    );
                }
            }
        }
    }

    #[test]
    fn text_colors_meet_wcag_aa_4_5_to_1_in_both_themes() {
        assert_pairs(TEXT_ON, 4.5);
    }

    #[test]
    fn borders_and_focus_meet_3_to_1_in_both_themes() {
        assert_pairs(BOUNDARY_ON, 3.0);
    }

    #[test]
    fn the_light_theme_is_white_based() {
        let (root, _) = tokens();
        assert_eq!(color(&root, "--color-bg"), (255, 255, 255));
        assert_eq!(color(&root, "--color-surface"), (255, 255, 255));
    }

    #[test]
    fn no_color_literals_outside_the_token_blocks() {
        let css = strip_comments(CSS);
        let mut rest = css.replacen(&format!(":root {{{}}}", block(&css, ":root")), "", 1);
        rest = rest.replacen(
            &format!(
                "[data-theme=\"dark\"] {{{}}}",
                block(&css, "[data-theme=\"dark\"]")
            ),
            "",
            1,
        );
        assert!(
            !rest.contains("--color-bg:") && !rest.contains(":root"),
            "トークンのブロックを取り除けていません"
        );
        for (i, line) in rest.lines().enumerate() {
            let lower = line.to_lowercase();
            assert!(
                !lower.contains('#')
                    && !lower.contains("rgb(")
                    && !lower.contains("rgba(")
                    && !lower.contains("hsl(")
                    && !lower.contains("hsla(")
                    && !lower.contains("oklch(")
                    && !lower.contains("color-mix("),
                "トークン以外の場所に色が直接書かれています（{}行目）: {line}",
                i + 1
            );
            // 色名（transparent・white など）。プロパティの値の中の単語として探す。
            for word in NAMED_COLORS {
                let found = lower
                    .split(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
                    .any(|w| w == *word);
                assert!(
                    !found,
                    "色名 {word} が直接書かれています（{}行目）: {line}",
                    i + 1
                );
            }
        }
    }

    /// よく使われる CSS の色名（トークン以外に書かれていないことの確認用）。
    pub(crate) const NAMED_COLORS: &[&str] = &[
        "white",
        "black",
        "red",
        "green",
        "blue",
        "gray",
        "grey",
        "silver",
        "yellow",
        "orange",
        "purple",
        "pink",
        "brown",
        "cyan",
        "magenta",
        "navy",
        "teal",
        "maroon",
        "olive",
        "lime",
        "aqua",
        "fuchsia",
        "transparent",
        "currentcolor",
    ];
}
