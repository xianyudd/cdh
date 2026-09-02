use ratatui::{
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Block, Clear, Paragraph},
    Frame,
};
use unicode_width::UnicodeWidthStr;

pub(super) use excludes::render as render_excludes;
#[cfg(test)]
pub(super) use excludes::{layout as excludes_layout, window_start as excludes_window_start};
#[cfg(test)]
pub(super) use help::lines as help_lines;
pub(super) use help::render as render_help;
pub(super) use settings::{render as render_settings, theme_choice_label};

mod help {
    use super::super::{Language, TextKey, Theme};
    use super::*;

    pub fn render(frame: &mut Frame, language: Language, theme: &Theme, full: Rect) {
        let lines = lines(language, theme);
        let width = 76u16.min(full.width.saturating_sub(4));
        let height = (lines.len() as u16 + 2).min(full.height);
        let area = centered(full, width, height);
        frame.render_widget(Clear, area);
        frame.render_widget(Block::default().style(theme.panel()), area);
        // Flat panel: top rule instead of a boxed border.
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "─".repeat(area.width as usize),
                theme.border(),
            ))),
            Rect::new(area.x, area.y, area.width, 1),
        );
        let inner = Rect::new(
            area.x.saturating_add(1),
            area.y.saturating_add(1),
            area.width.saturating_sub(2),
            area.height.saturating_sub(2),
        );
        frame.render_widget(Paragraph::new(lines), inner);
    }

    pub fn lines(language: Language, theme: &Theme) -> Vec<Line<'static>> {
        vec![
            Line::from(Span::styled(
                language.text(TextKey::HelpTitle),
                theme.title(),
            )),
            section(language.text(TextKey::Movement), theme),
            row("↑ / Ctrl+P", language.text(TextKey::PreviousItem), theme),
            row("↓ / Ctrl+N", language.text(TextKey::NextItem), theme),
            section(language.text(TextKey::Paging), theme),
            row("Ctrl+↑ / PgUp", language.text(TextKey::PreviousPage), theme),
            row("Ctrl+↓ / PgDn", language.text(TextKey::NextPage), theme),
            row("Home", language.text(TextKey::FirstItem), theme),
            row("End", language.text(TextKey::LastItem), theme),
            section(language.text(TextKey::Search), theme),
            row("← / →", language.text(TextKey::MoveCursor), theme),
            row(
                "Backspace",
                language.text(TextKey::DeleteBeforeCursor),
                theme,
            ),
            row("Delete", language.text(TextKey::DeleteAtCursor), theme),
            row(
                "Ctrl+U",
                language.text(TextKey::ClearSearchDescription),
                theme,
            ),
            section(language.text(TextKey::Actions), theme),
            row("Enter", language.text(TextKey::JumpToDirectory), theme),
            row("Tab", language.text(TextKey::TogglePreview), theme),
            row("Ctrl+D", language.text(TextKey::DeleteHistoryEntry), theme),
            row(
                "Ctrl+H / F5",
                language.text(TextKey::ToggleHiddenDirectories),
                theme,
            ),
            row("F1 / ? / ？", language.text(TextKey::OpenHelp), theme),
            row("F2", language.text(TextKey::OpenSettings), theme),
            row("F4", language.text(TextKey::OpenExcludes), theme),
            row("Ctrl+T / F3", language.text(TextKey::SettingTheme), theme),
            row(
                "↑↓  ←→  Enter/Space  Esc",
                language.text(TextKey::SettingsControls),
                theme,
            ),
            row("Esc", language.text(TextKey::EscapeDescription), theme),
        ]
    }

    fn section(title: &str, theme: &Theme) -> Line<'static> {
        Line::from(Span::styled(title.to_string(), theme.accent()))
    }

    fn row(key: &str, description: &str, theme: &Theme) -> Line<'static> {
        const KEY_WIDTH: usize = 21;
        let padding = KEY_WIDTH.saturating_sub(UnicodeWidthStr::width(key));
        Line::from(vec![
            Span::styled(format!("{key}{}", " ".repeat(padding)), theme.accent()),
            Span::styled(description.to_string(), theme.primary()),
        ])
    }

    fn centered(full: Rect, width: u16, height: u16) -> Rect {
        Rect::new(
            full.x + (full.width.saturating_sub(width)) / 2,
            full.y + (full.height.saturating_sub(height)) / 2,
            width,
            height,
        )
    }
}

mod settings {
    use super::super::settings::LanguagePreference;
    use super::super::{FrameView, Language, SettingKey, TextKey, Theme, ThemeChoice};
    use super::*;
    use unicode_width::UnicodeWidthStr;

    pub fn render(frame: &mut Frame, view: &FrameView, theme: &Theme, full: Rect, selected: usize) {
        let width = 72u16.min(full.width.saturating_sub(2));
        let height = 10u16.min(full.height);
        if width < 2 || height < 2 {
            return;
        }

        let area = centered(full, width, height);
        frame.render_widget(Clear, area);
        frame.render_widget(Block::default().style(theme.panel()), area);
        // Flat panel: no box border — title + dim rule keep hierarchy without a frame.
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "─".repeat(area.width as usize),
                theme.border(),
            ))),
            Rect::new(area.x, area.y, area.width, 1),
        );
        let inner = Rect::new(
            area.x.saturating_add(1),
            area.y.saturating_add(1),
            area.width.saturating_sub(2),
            area.height.saturating_sub(2),
        );
        if inner.is_empty() {
            return;
        }

        render_line(
            frame,
            inner,
            0,
            view.language.text(TextKey::SettingsTitle),
            theme.title(),
        );

        let row_start = u16::from(inner.height >= 7);
        for (index, key) in [
            SettingKey::Language,
            SettingKey::Theme,
            SettingKey::Preview,
            SettingKey::Color,
            SettingKey::Mouse,
        ]
        .into_iter()
        .enumerate()
        {
            let offset = row_start + 1 + index as u16;
            if offset >= inner.height {
                break;
            }
            let style = if index == selected.min(4) {
                theme.selected()
            } else {
                theme.primary()
            };
            let text = row_text(view, key, inner.width as usize);
            render_line(frame, inner, offset, &text, style);
        }

        if inner.height > 1 {
            let footer = super::super::trim_end(
                view.language.text(TextKey::SettingsFooter),
                inner.width as usize,
            );
            render_line(frame, inner, inner.height - 1, &footer, theme.dim());
        }
    }

    pub fn theme_choice_label(language: Language, choice: ThemeChoice) -> &'static str {
        language.text(match choice {
            ThemeChoice::Graphite => TextKey::ThemeGraphite,
            ThemeChoice::Nord => TextKey::ThemeNord,
            ThemeChoice::Daylight => TextKey::ThemeDaylight,
            ThemeChoice::Mono => TextKey::ThemeMono,
            ThemeChoice::Dracula => TextKey::ThemeDracula,
            ThemeChoice::Amber => TextKey::ThemeAmber,
            ThemeChoice::Forest => TextKey::ThemeForest,
        })
    }

    fn row_text(view: &FrameView, key: SettingKey, width: usize) -> String {
        let (label, value) = match key {
            SettingKey::Language => (
                view.language.text(TextKey::SettingLanguage),
                match view.prefs.language {
                    LanguagePreference::Auto => view.language.text(TextKey::LanguageAuto),
                    LanguagePreference::ZhCn => {
                        view.language.text(TextKey::LanguageSimplifiedChinese)
                    }
                    LanguagePreference::En => view.language.text(TextKey::LanguageEnglish),
                },
            ),
            SettingKey::Theme => (
                view.language.text(TextKey::SettingTheme),
                theme_choice_label(view.language, view.prefs.theme),
            ),
            SettingKey::Preview => (
                view.language.text(TextKey::SettingPreviewStartup),
                boolean_text(view.language, view.prefs.preview),
            ),
            SettingKey::Color => (
                view.language.text(TextKey::SettingColor),
                boolean_text(view.language, view.prefs.color),
            ),
            SettingKey::Mouse => (
                view.language.text(TextKey::SettingMouseCapture),
                boolean_text(view.language, view.prefs.mouse),
            ),
        };
        let marker = view
            .locked
            .is_locked(key)
            .then(|| view.language.text(TextKey::EnvironmentControlled));
        let right = marker
            .map(|marker| format!("{value} · {marker}"))
            .unwrap_or_else(|| value.to_string());
        let occupied = UnicodeWidthStr::width(label) + UnicodeWidthStr::width(right.as_str());
        if occupied < width {
            format!("{label}{}{right}", " ".repeat(width - occupied))
        } else {
            super::super::trim_end(&format!("{label}  {right}"), width)
        }
    }

    fn boolean_text(language: Language, value: bool) -> &'static str {
        language.text(if value {
            TextKey::SettingOn
        } else {
            TextKey::SettingOff
        })
    }

    fn render_line(frame: &mut Frame, inner: Rect, offset: u16, text: &str, style: Style) {
        if offset >= inner.height {
            return;
        }
        let area = Rect::new(inner.x, inner.y + offset, inner.width, 1);
        frame.render_widget(
            Paragraph::new(super::super::trim_end(text, inner.width as usize)).style(style),
            area,
        );
    }

    fn centered(full: Rect, width: u16, height: u16) -> Rect {
        Rect::new(
            full.x + (full.width.saturating_sub(width)) / 2,
            full.y + (full.height.saturating_sub(height)) / 2,
            width,
            height,
        )
    }
}

mod excludes {
    use super::super::{FrameView, TextKey, Theme};
    use super::*;

    pub fn render(frame: &mut Frame, view: &FrameView, theme: &Theme, full: Rect, selected: usize) {
        let width = 72u16.min(full.width.saturating_sub(2));
        let height = 14u16.min(full.height);
        if width < 2 || height < 2 {
            return;
        }

        let area = centered(full, width, height);
        frame.render_widget(Clear, area);
        frame.render_widget(Block::default().style(theme.panel()), area);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "─".repeat(area.width as usize),
                theme.border(),
            ))),
            Rect::new(area.x, area.y, area.width, 1),
        );
        let inner = Rect::new(
            area.x.saturating_add(1),
            area.y.saturating_add(1),
            area.width.saturating_sub(2),
            area.height.saturating_sub(2),
        );
        if inner.is_empty() {
            return;
        }

        render_line(
            frame,
            inner,
            0,
            view.language.text(TextKey::ExcludesTitle),
            theme.title(),
        );

        let roots = view.exclude_roots;
        if roots.is_empty() {
            render_line(
                frame,
                inner,
                2,
                view.language.text(TextKey::ExcludesEmpty),
                theme.dim(),
            );
        } else {
            // Scroll the window with the cursor: the list is unbounded in principle
            // and a fixed top would strand entries below the panel with no way to
            // reach them.
            let rows = visible_rows(inner.height);
            let selected = selected.min(roots.len() - 1);
            let start = window_start(roots.len(), rows, selected);
            for (offset, root) in roots[start..].iter().take(rows).enumerate() {
                let index = start + offset;
                let style = if index == selected {
                    theme.selected()
                } else {
                    theme.primary()
                };
                let text = format!(
                    " {}",
                    super::super::PathDisplay::from_path(root, view.home).text
                );
                render_line(
                    frame,
                    inner,
                    2 + offset as u16,
                    &super::super::trim_middle(&text, inner.width as usize),
                    style,
                );
            }
        }

        if layout(inner.height).1 {
            let footer = super::super::trim_end(
                view.language.text(TextKey::ExcludesFooter),
                inner.width as usize,
            );
            render_line(frame, inner, inner.height - 1, &footer, theme.dim());
        }
    }

    pub fn layout(height: u16) -> (usize, bool) {
        if height >= 4 {
            (height.saturating_sub(3) as usize, true)
        } else {
            (height.saturating_sub(2) as usize, false)
        }
    }

    pub(super) fn visible_rows(height: u16) -> usize {
        layout(height).0
    }

    pub fn window_start(len: usize, rows: usize, selected: usize) -> usize {
        if len <= rows {
            return 0;
        }
        selected
            .saturating_sub(rows.saturating_sub(1))
            .min(len - rows)
    }

    fn render_line(frame: &mut Frame, inner: Rect, offset: u16, text: &str, style: Style) {
        if offset >= inner.height {
            return;
        }
        let area = Rect::new(inner.x, inner.y + offset, inner.width, 1);
        frame.render_widget(
            Paragraph::new(super::super::trim_end(text, inner.width as usize)).style(style),
            area,
        );
    }

    fn centered(full: Rect, width: u16, height: u16) -> Rect {
        Rect::new(
            full.x + (full.width.saturating_sub(width)) / 2,
            full.y + (full.height.saturating_sub(height)) / 2,
            width,
            height,
        )
    }
}
