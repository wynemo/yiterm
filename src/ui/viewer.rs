use super::*;
use crate::git::{DiffLine, LineKind};
use gpui_component::{ActiveTheme, Rope, highlighter::SyntaxHighlighter};
use std::sync::Arc;

#[derive(Clone, Copy)]
enum Kind {
    Context,
    Added,
    Removed,
    Hunk,
}
struct Row {
    text: String,
    old: Option<usize>,
    new: Option<usize>,
    kind: Kind,
    offset: usize,
}
pub(super) struct ViewerData {
    id: uuid::Uuid,
    rows: Vec<Row>,
    pairs: Vec<(Option<usize>, Option<usize>)>,
    added: usize,
    removed: usize,
    columns: usize,
    syntax: SyntaxHighlighter,
    syntax_old: SyntaxHighlighter,
}
enum Source {
    File(Vec<String>),
    Diff(Vec<DiffLine>, bool),
}
impl ViewerData {
    fn build(id: uuid::Uuid, path: &std::path::Path, source: Source) -> Self {
        let mut rows = Vec::new();
        let (mut old, mut new) = (None, None);
        match source {
            Source::File(lines) => {
                rows.extend(lines.into_iter().enumerate().map(|(i, text)| Row {
                    text,
                    old: None,
                    new: Some(i + 1),
                    kind: Kind::Context,
                    offset: 0,
                }));
            }
            Source::Diff(lines, untracked) => {
                if untracked {
                    new = Some(1);
                }
                for line in lines {
                    let kind = match line.kind {
                        LineKind::Header => continue,
                        LineKind::Hunk => {
                            let mut parts = line.text.split_whitespace();
                            parts.next();
                            old = parts
                                .next()
                                .and_then(|p| p.strip_prefix('-'))
                                .and_then(|p| p.split(',').next())
                                .and_then(|p| p.parse().ok());
                            new = parts
                                .next()
                                .and_then(|p| p.strip_prefix('+'))
                                .and_then(|p| p.split(',').next())
                                .and_then(|p| p.parse().ok());
                            Kind::Hunk
                        }
                        LineKind::Addition => Kind::Added,
                        LineKind::Deletion => Kind::Removed,
                        LineKind::Context if line.text.starts_with('\\') => Kind::Hunk,
                        LineKind::Context if old.is_none() && new.is_none() => Kind::Hunk,
                        LineKind::Context => Kind::Context,
                    };
                    let (before, after) = match kind {
                        Kind::Added => (None, new),
                        Kind::Removed => (old, None),
                        Kind::Context => (old, new),
                        Kind::Hunk => (None, None),
                    };
                    let text = if !untracked && !matches!(kind, Kind::Hunk) {
                        line.text.get(1..).unwrap_or("").to_owned()
                    } else {
                        line.text
                    };
                    rows.push(Row {
                        text,
                        old: before,
                        new: after,
                        kind,
                        offset: 0,
                    });
                    if before.is_some() {
                        old = old.map(|n| n + 1);
                    }
                    if after.is_some() {
                        new = new.map(|n| n + 1);
                    }
                }
            }
        }
        let mut source = String::new();
        let mut old_source = String::new();
        let (mut added, mut removed, mut columns) = (0, 0, 0);
        for row in &mut rows {
            added += usize::from(matches!(row.kind, Kind::Added));
            removed += usize::from(matches!(row.kind, Kind::Removed));
            columns = columns.max(
                row.text
                    .chars()
                    .map(|c| if c.is_ascii() { 1 } else { 2 })
                    .sum(),
            );
            row.offset = if matches!(row.kind, Kind::Removed) {
                old_source.len()
            } else {
                source.len()
            };
            if !matches!(row.kind, Kind::Hunk) {
                if !matches!(row.kind, Kind::Removed) {
                    source.push_str(&row.text);
                    source.push('\n');
                }
                if !matches!(row.kind, Kind::Added) {
                    old_source.push_str(&row.text);
                    old_source.push('\n');
                }
            } else {
                source.push('\n');
                old_source.push('\n');
            }
        }
        let mut pairs = Vec::new();
        let mut index = 0;
        while index < rows.len() {
            match rows[index].kind {
                Kind::Context | Kind::Hunk => {
                    pairs.push((Some(index), Some(index)));
                    index += 1;
                }
                _ => {
                    let (mut left, mut right) = (Vec::new(), Vec::new());
                    while index < rows.len()
                        && matches!(rows[index].kind, Kind::Added | Kind::Removed)
                    {
                        if matches!(rows[index].kind, Kind::Removed) {
                            left.push(index);
                        } else {
                            right.push(index);
                        }
                        index += 1;
                    }
                    for i in 0..left.len().max(right.len()) {
                        pairs.push((left.get(i).copied(), right.get(i).copied()));
                    }
                }
            }
        }
        let extension = path.extension().and_then(|s| s.to_str()).unwrap_or("text");
        let language = match extension {
            "rs" => "rust",
            "py" => "python",
            "js" | "jsx" => "javascript",
            "ts" | "tsx" => "typescript",
            "md" => "markdown",
            "sh" | "zsh" | "bash" => "bash",
            other => other,
        };
        let mut syntax = SyntaxHighlighter::new(language);
        syntax.update(None, &Rope::from(source));
        let mut syntax_old = SyntaxHighlighter::new(language);
        syntax_old.update(None, &Rope::from(old_source));
        Self {
            id,
            rows,
            pairs,
            added,
            removed,
            columns,
            syntax,
            syntax_old,
        }
    }
    fn code(&self, index: usize, cx: &App) -> gpui::StyledText {
        let row = &self.rows[index];
        let syntax = if matches!(row.kind, Kind::Removed) {
            &self.syntax_old
        } else {
            &self.syntax
        };
        let styles = syntax
            .styles(
                &(row.offset..row.offset + row.text.len()),
                &cx.theme().highlight_theme,
            )
            .into_iter()
            .map(|(range, style)| ((range.start - row.offset)..(range.end - row.offset), style));
        gpui::StyledText::new(row.text.clone()).with_highlights(styles)
    }
}
impl TerminalUi {
    pub(super) fn viewer_open(&self) -> bool {
        self.app.files_settings.visible
            && match self.app.files_settings.mode {
                settings::FilesPanelMode::Git => self.app.git.selected.is_some(),
                settings::FilesPanelMode::Files => {
                    self.app.tabs[self.app.active].files.preview_path.is_some()
                }
            }
    }
    fn close_viewer(&mut self, window: &mut Window, cx: &mut ViewContext<Self>) {
        match self.app.files_settings.mode {
            settings::FilesPanelMode::Git => self.app.git.back(),
            settings::FilesPanelMode::Files => self.app.tabs[self.app.active].files.back(),
        }
        self.files_focus.focus(window);
        cx.notify();
    }
    fn viewer_number(number: Option<usize>, theme: ThemeMode) -> gpui::Div {
        div()
            .w(px(46.))
            .flex_shrink_0()
            .pr(px(10.))
            .text_right()
            .text_color(theme.palette(8))
            .child(number.map(|n| n.to_string()).unwrap_or_default())
    }
    fn viewer_row(
        data: &ViewerData,
        index: usize,
        git: bool,
        side: Option<bool>,
        width: f32,
        theme: ThemeMode,
        cx: &App,
    ) -> gpui::Div {
        let row = &data.rows[index];
        let color = match row.kind {
            Kind::Added => 2,
            Kind::Removed => 1,
            _ => 8,
        };
        let mut line = div()
            .w(px(width))
            .h(px(21.))
            .flex()
            .items_center()
            .overflow_hidden()
            .whitespace_nowrap()
            .font_family("Lilex")
            .text_size(px(12.))
            .line_height(px(21.));
        if matches!(row.kind, Kind::Hunk) {
            return line
                .bg(gpui::Hsla::from(theme.border()).opacity(0.25))
                .text_color(theme.palette(8))
                .px(px(8.))
                .child(row.text.clone());
        }
        if git {
            line = line.bg(gpui::Hsla::from(theme.palette(color)).opacity(
                if matches!(row.kind, Kind::Context) {
                    0.
                } else {
                    0.08
                },
            ));
        }
        match side {
            None if git => {
                line = line
                    .child(Self::viewer_number(row.old, theme))
                    .child(Self::viewer_number(row.new, theme));
            }
            Some(false) => {
                line = line.child(Self::viewer_number(row.old, theme));
            }
            _ => {
                line = line.child(Self::viewer_number(row.new, theme));
            }
        }
        if git {
            line = line.child(
                div()
                    .w(px(18.))
                    .flex_shrink_0()
                    .text_color(theme.palette(color))
                    .child(match row.kind {
                        Kind::Added => "+",
                        Kind::Removed => "−",
                        _ => "",
                    }),
            );
        }
        line.child(data.code(index, cx))
    }
    pub(super) fn code_viewer(
        &mut self,
        window: &mut Window,
        cx: &mut ViewContext<Self>,
    ) -> gpui::AnyElement {
        let git = self.app.files_settings.mode == settings::FilesPanelMode::Git;
        let theme = self.app.theme;
        let (path, id, notice, error) = if git {
            let change = self.app.git.selected.as_ref().unwrap();
            let (id, notice, error) = match &self.app.git.diff {
                Some(Ok(diff)) => (Some(diff.id), diff.notice.clone(), None),
                Some(Err(error)) => (None, None, Some(error.clone())),
                None => (None, None, None),
            };
            (change.path.clone(), id, notice, error)
        } else {
            let files = &self.app.tabs[self.app.active].files;
            (
                files.preview_path.clone().unwrap(),
                files.preview.as_ref().map(|p| p.id),
                files.preview.as_ref().and_then(|p| p.notice.clone()),
                files.preview_error.clone(),
            )
        };
        if let Some(id) = id
            && self.viewer_request != Some(id)
        {
            self.viewer_request = Some(id);
            self.viewer_data = None;
            self.viewer_scroll = Default::default();
            let source = if git {
                Source::Diff(
                    self.app
                        .git
                        .diff
                        .as_ref()
                        .unwrap()
                        .as_ref()
                        .unwrap()
                        .lines
                        .clone(),
                    self.app.git.selected.as_ref().unwrap().section
                        == crate::git::Section::Untracked,
                )
            } else {
                Source::File(
                    self.app.tabs[self.app.active]
                        .files
                        .preview
                        .as_ref()
                        .unwrap()
                        .lines
                        .clone(),
                )
            };
            let path = path.clone();
            let work = cx
                .background_executor()
                .spawn(async move { Arc::new(ViewerData::build(id, &path, source)) });
            cx.spawn_in(window, async move |this, cx| {
                let data = work.await;
                let _ = this.update_in(cx, |this, _, cx| {
                    if this.viewer_request == Some(data.id) {
                        this.viewer_data = Some(data);
                        cx.notify();
                    }
                });
            })
            .detach();
        }
        let data = self
            .viewer_data
            .as_ref()
            .filter(|d| Some(d.id) == id)
            .cloned();
        let title = if git {
            match self.app.git.selected.as_ref().unwrap().section {
                crate::git::Section::Staged => "Staged changes",
                crate::git::Section::Conflicted => "Conflicts",
                _ => "Changes",
            }
        } else {
            "File preview"
        };
        let mut panel = div()
            .id("code-viewer")
            .absolute()
            .inset_0()
            .occlude()
            .flex()
            .flex_col()
            .bg(self.sidebar_surface())
            .track_focus(&self.files_focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                match event.keystroke.key.as_str() {
                    "escape" => this.close_viewer(window, cx),
                    "up" | "down" | "pageup" | "pagedown" => {
                        let step = if event.keystroke.key.starts_with("page") {
                            f32::from(window.viewport_size().height) * 0.7
                        } else {
                            21.
                        };
                        let sign = if matches!(event.keystroke.key.as_str(), "up" | "pageup") {
                            1.
                        } else {
                            -1.
                        };
                        let handle = &this.viewer_scroll.0.borrow().base_handle;
                        let offset = handle.offset();
                        handle.set_offset(point(offset.x, offset.y + px(step * sign)));
                    }
                    _ => {}
                }
                cx.stop_propagation();
                cx.notify();
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    this.files_focus.focus(window);
                    cx.stop_propagation();
                }),
            )
            .child(
                div()
                    .w_full()
                    .h(px(48.))
                    .flex_shrink_0()
                    .px(px(20.))
                    .flex()
                    .items_center()
                    .justify_between()
                    .border_b_1()
                    .border_color(theme.border())
                    .child(div().font_weight(gpui::FontWeight::SEMIBOLD).child(title))
                    .child(
                        Button::new("viewer-close")
                            .icon(IconName::Close)
                            .ghost()
                            .small()
                            .tooltip("Close (Esc)")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.close_viewer(window, cx);
                                cx.stop_propagation();
                            })),
                    ),
            );
        let mut toolbar = div()
            .w_full()
            .h(px(50.))
            .flex_shrink_0()
            .px(px(20.))
            .flex()
            .items_center()
            .gap(px(8.))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .child(crate::git::shown(&path)),
            );
        if git {
            if let Some(data) = &data {
                toolbar = toolbar
                    .child(
                        div()
                            .text_color(theme.palette(2))
                            .child(format!("+{}", data.added)),
                    )
                    .child(
                        div()
                            .text_color(theme.palette(1))
                            .child(format!("−{}", data.removed)),
                    );
            }
            let mut tabs = div()
                .flex()
                .gap(px(2.))
                .p(px(3.))
                .rounded(px(7.))
                .border_1()
                .border_color(theme.border());
            for (label, split) in [("Unified", false), ("Split", true)] {
                tabs = tabs.child(
                    div()
                        .id(if split {
                            "viewer-split"
                        } else {
                            "viewer-unified"
                        })
                        .px(px(10.))
                        .h(px(24.))
                        .flex()
                        .items_center()
                        .rounded(px(4.))
                        .cursor_pointer()
                        .when(self.viewer_split == split, |d| {
                            d.bg(theme.background()).shadow_sm()
                        })
                        .child(label)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.viewer_split = split;
                            this.viewer_scroll = Default::default();
                            cx.stop_propagation();
                            cx.notify();
                        })),
                );
            }
            toolbar = toolbar.child(tabs);
        }
        panel = panel.child(toolbar);
        if let Some(error) = error {
            panel = panel.child(div().p(px(20.)).child(error));
        } else if let Some(data) = data {
            let split = git && self.viewer_split;
            let available = (f32::from(window.viewport_size().width) - 40.).max(100.);
            let viewport = if split { available * 0.5 } else { available };
            let width = (data.columns as f32 * 9. + if git { 116. } else { 50. }).max(viewport);
            let mut body = div().flex_1().min_h_0().min_w_0().mx(px(20.)).flex();
            for side in if split {
                vec![Some(false), Some(true)]
            } else {
                vec![None]
            } {
                let data = data.clone();
                let count = if split {
                    data.pairs.len()
                } else {
                    data.rows.len()
                };
                let list = gpui::uniform_list(
                    if side == Some(true) {
                        "viewer-new-lines"
                    } else {
                        "viewer-lines"
                    },
                    count,
                    cx.processor(move |_, range: Range<usize>, _, cx| {
                        range
                            .map(|i| {
                                let index = match side {
                                    None => Some(i),
                                    Some(false) => data.pairs[i].0,
                                    Some(true) => data.pairs[i].1,
                                };
                                index
                                    .map(|index| {
                                        Self::viewer_row(&data, index, git, side, width, theme, cx)
                                    })
                                    .unwrap_or_else(|| {
                                        div()
                                            .w(px(width))
                                            .h(px(21.))
                                            .bg(gpui::Hsla::from(theme.border()).opacity(0.1))
                                    })
                            })
                            .collect::<Vec<_>>()
                    }),
                )
                .w(px(width))
                .h_full()
                .track_scroll(self.viewer_scroll.clone());
                body = body.child(
                    div()
                        .id(if side == Some(true) {
                            "viewer-new-horizontal"
                        } else {
                            "viewer-horizontal"
                        })
                        .w(px(viewport))
                        .min_w_0()
                        .h_full()
                        .overflow_x_scroll()
                        .when(split && side == Some(false), |d| {
                            d.border_r_1().border_color(theme.border())
                        })
                        .child(list),
                );
            }
            panel = panel.child(body);
        } else {
            panel = panel.child(div().p(px(20.)).child("Loading…"));
        }
        panel
            .when_some(notice, |d, notice| {
                d.child(
                    div()
                        .p(px(10.))
                        .text_size(px(11.))
                        .text_color(theme.palette(8))
                        .child(notice),
                )
            })
            .into_any_element()
    }
}
