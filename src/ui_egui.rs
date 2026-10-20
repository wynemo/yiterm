//! Original Linux egui/eframe interface, sharing the terminal model with GPUI.
use super::*;
use eframe::egui::{self, Color32, FontId, Key, Rect, Vec2};
#[cfg(test)]
use portable_pty::CommandBuilder;

fn terminal_cell_rect(origin: egui::Pos2, cell: Vec2, column: usize, row: usize) -> Rect {
    // Compute both sides from grid coordinates. Adding cell height to the top
    // can round differently from the next row's top at fractional line heights.
    Rect::from_min_max(
        origin + Vec2::new(column as f32 * cell.x, row as f32 * cell.y),
        origin + Vec2::new((column + 1) as f32 * cell.x, (row + 1) as f32 * cell.y),
    )
}

impl TerminalSession {
    fn copy_selection_egui(&mut self, ctx: &egui::Context) {
        if let Some(text) = self
            .term
            .selection_to_string()
            .filter(|text| !text.is_empty())
        {
            ctx.copy_text(text);
            self.term.selection = None;
            self.selecting = false;
            ctx.request_repaint();
        }
    }
    fn copy_on_right_click(&mut self, response: &egui::Response, windows: bool) -> bool {
        if !windows || self.term.mode().intersects(TermMode::MOUSE_MODE) {
            return false;
        }
        let Some(_) = self
            .term
            .selection_to_string()
            .filter(|text| !text.is_empty())
        else {
            return false;
        };
        if response.secondary_clicked() {
            self.copy_selection_egui(&response.ctx);
        }
        true
    }
}
impl TerminalSession {
    fn flush_clipboard(&mut self) {
        self.flush_clipboard_with(|text| {
            let mut clipboard = arboard::Clipboard::new().context("opening host clipboard")?;
            clipboard
                .set_text(text)
                .context("writing to host clipboard")?;
            Ok(())
        });
    }

    fn show(&mut self, ctx: &egui::Context, ui: &mut egui::Ui, focused: bool) {
        let theme = ThemeMode::from_context(ctx);
        // Keep the cursor visible while typing and restart its blink on focus.
        if focused
            && ctx.input(|input| {
                input.events.iter().any(|event| {
                    matches!(
                        event,
                        egui::Event::Key { pressed: true, .. }
                            | egui::Event::Text(_)
                            | egui::Event::Paste(_)
                            | egui::Event::Ime(_)
                            | egui::Event::WindowFocused(true)
                    )
                })
            })
        {
            self.cursor_blink_started = Instant::now();
        }
        let rect = ui.available_rect_before_wrap();
        let font = FontId::monospace(self.font_settings.size);
        let cell = Vec2::new(
            ui.fonts_mut(|f| f.glyph_width(&font, 'M')),
            self.font_settings.size * TERMINAL_LINE_HEIGHT,
        );
        let size = Size {
            cols: ((rect.width() / cell.x) as usize).max(2),
            rows: ((rect.height() / cell.y) as usize).max(1),
        };
        if size.cols != self.size.cols || size.rows != self.size.rows {
            if let Err(error) = self.master.resize(PtySize {
                cols: size.cols as u16,
                rows: size.rows as u16,
                pixel_width: 0,
                pixel_height: 0,
            }) {
                tracing::warn!(%error, "PTY resize failed");
                self.status = error.to_string();
            }
            self.term.resize(size);
            self.size = size;
        }
        let hovered = ui.rect_contains_pointer(rect);
        let scroll = if hovered {
            ctx.input(|i| i.smooth_scroll_delta.y)
        } else {
            0.0
        };
        let mouse_protocol = self.mouse_modes.protocol(*self.term.mode());
        let mouse_mode = mouse_protocol.mode;
        let report_mouse = mouse_mode.intersects(TermMode::MOUSE_MODE)
            && !ctx.input(|i| i.modifiers.shift)
            && self.term.grid().display_offset() == 0;
        if !report_mouse {
            self.mouse_wheel.reset();
        }
        if scroll != 0.0 && !report_mouse {
            self.term
                .scroll_display(Scroll::Delta((scroll / cell.y).round() as i32));
        }
        let hovered_link = ctx
            .input(|input| input.pointer.hover_pos())
            .and_then(|pos| {
                rect.contains(pos)
                    .then(|| {
                        let (point, _) = selection_point(
                            rect,
                            cell,
                            size,
                            self.term.grid().display_offset(),
                            pos,
                        );
                        terminal_link_details(&self.term, point)
                    })
                    .flatten()
            });
        if let Some(link) = &hovered_link {
            ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
            let hint = if cfg!(target_os = "macos") {
                "Cmd"
            } else {
                "Ctrl"
            };
            egui::Tooltip::always_open(
                ctx.clone(),
                ui.layer_id(),
                ui.id().with("terminal_link"),
                egui::PopupAnchor::Pointer,
            )
            .show(|ui| {
                ui.label(format!("{}\n{hint}+点击打开链接", link.url));
            });
        }
        // Deliver input before shaping and submitting the existing screen. The
        // PTY can process/echo the key while we prepare this frame.
        let offset = self.term.grid().display_offset();
        let events = if focused
            && !egui::Popup::is_any_open(ctx)
            && ctx.memory(|memory| memory.top_modal_layer().is_none())
        {
            ctx.input(|i| i.events.clone())
        } else {
            Vec::new()
        };
        // Consume candidate-selection keys even if Commit clears the preedit
        // earlier in this frame. They must not execute a shell command.
        let composing = ime_owns_keyboard(&self.ime_preedit, &events);
        let paste_shortcut = events.iter().any(|event| matches!(event, egui::Event::Key { key, pressed: true, modifiers, .. } if is_paste_shortcut(*key, *modifiers)));
        if paste_shortcut {
            self.paste();
        }
        for event in events {
            match event {
                egui::Event::MouseWheel {
                    unit,
                    delta,
                    modifiers,
                } => {
                    tracing::trace!(target: "yiterm::mouse", "wheel unit={unit:?} delta={} report={report_mouse}", delta.y);
                    if !report_mouse || modifiers.shift || delta.y == 0.0 {
                        continue;
                    }
                    let Some(pos) = ctx
                        .input(|i| i.pointer.hover_pos())
                        .filter(|pos| rect.contains(*pos))
                    else {
                        self.mouse_wheel.reset();
                        continue;
                    };
                    let (units_per_step, multiplier) = match unit {
                        egui::MouseWheelUnit::Line => (1.0, 1),
                        egui::MouseWheelUnit::Point => (cell.y, 1),
                        egui::MouseWheelUnit::Page => (1.0, size.rows as i32),
                    };
                    let steps = self.mouse_wheel.add(delta.y, units_per_step) * multiplier;
                    let button = if steps > 0 { 64 } else { 65 };
                    for _ in 0..steps.unsigned_abs() {
                        if let Some(bytes) = mouse_bytes(
                            mouse_protocol,
                            rect,
                            cell,
                            size,
                            pos,
                            (button, true),
                            modifiers,
                        ) {
                            self.write_mouse(&bytes);
                        }
                    }
                }
                egui::Event::PointerButton {
                    pos,
                    button,
                    pressed,
                    modifiers,
                } => {
                    let open_link = if cfg!(target_os = "macos") {
                        modifiers.mac_cmd
                    } else {
                        modifiers.ctrl
                    };
                    if button == egui::PointerButton::Primary && open_link && rect.contains(pos) {
                        if pressed {
                            let (point, _) = selection_point(rect, cell, size, offset, pos);
                            if let Some(url) = terminal_link(&self.term, point) {
                                ctx.open_url(egui::OpenUrl::new_tab(url));
                                self.selecting = false;
                                continue;
                            }
                        } else if hovered_link.is_some() {
                            continue;
                        }
                    }
                    let tracked = self.mouse_button == Some(button);
                    if button == egui::PointerButton::Primary {
                        if pressed && !report_mouse && rect.contains(pos) {
                            let (point, side) = selection_point(rect, cell, size, offset, pos);
                            self.term.selection =
                                Some(Selection::new(SelectionType::Simple, point, side));
                            self.selecting = true;
                            ctx.request_repaint();
                        } else if !pressed && self.selecting {
                            let (point, side) = selection_point(rect, cell, size, offset, pos);
                            if let Some(selection) = &mut self.term.selection {
                                selection.update(point, side);
                            }
                            self.selecting = false;
                            ctx.request_repaint();
                            continue;
                        }
                        if self.selecting {
                            continue;
                        }
                    }
                    tracing::trace!(target: "yiterm::mouse", "button={button:?} pressed={pressed} report={report_mouse} inside={} shift={} offset={} pos={pos:?} rect={rect:?} cell={cell:?} mode={mouse_mode:?}", rect.contains(pos), modifiers.shift, self.term.grid().display_offset());
                    if report_mouse
                        && (rect.contains(pos) || (!pressed && tracked))
                        && let Some(code) = mouse_button_code(button)
                    {
                        if let Some(bytes) = mouse_bytes(
                            mouse_protocol,
                            rect,
                            cell,
                            size,
                            pos,
                            (code, pressed),
                            modifiers,
                        ) {
                            self.write_mouse(&bytes);
                        }
                        if pressed {
                            self.mouse_button = Some(button);
                        }
                    }
                    if !pressed && tracked {
                        self.mouse_button = None;
                    }
                }
                egui::Event::PointerMoved(pos) if self.selecting => {
                    let (point, side) = selection_point(rect, cell, size, offset, pos);
                    if let Some(selection) = &mut self.term.selection {
                        selection.update(point, side);
                    }
                    ctx.request_repaint();
                }
                egui::Event::PointerMoved(pos) if report_mouse => {
                    if rect.contains(pos)
                        && (mouse_mode.contains(TermMode::MOUSE_MOTION)
                            || (mouse_mode.contains(TermMode::MOUSE_DRAG)
                                && self.mouse_button.is_some()))
                    {
                        let code = self.mouse_button.and_then(mouse_button_code).unwrap_or(3) + 32;
                        if let Some(bytes) = mouse_bytes(
                            mouse_protocol,
                            rect,
                            cell,
                            size,
                            pos,
                            (code, true),
                            ctx.input(|i| i.modifiers),
                        ) {
                            self.write(&bytes);
                        }
                    }
                }
                egui::Event::Paste(_) if paste_shortcut => {}
                egui::Event::Copy => self.copy_selection_egui(ctx),
                egui::Event::Paste(_) => self.paste(),
                egui::Event::Ime(egui::ImeEvent::Preedit(text)) => {
                    self.ime_preedit = text;
                }
                egui::Event::Ime(egui::ImeEvent::Commit(text)) => {
                    self.ime_preedit.clear();
                    self.term.scroll_display(Scroll::Bottom);
                    self.write(text.as_bytes());
                }
                egui::Event::Ime(egui::ImeEvent::Disabled) | egui::Event::WindowFocused(false) => {
                    self.ime_preedit.clear();
                    self.selecting = false;
                }
                egui::Event::Text(_) if composing => {}
                egui::Event::Text(text) => {
                    self.term.scroll_display(Scroll::Bottom);
                    self.write(text.as_bytes());
                }
                egui::Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    ..
                } => {
                    if composing {
                        continue;
                    }
                    if is_copy_shortcut(key, modifiers) {
                        self.copy_selection_egui(ctx);
                        continue;
                    }
                    if is_paste_shortcut(key, modifiers) {
                        continue;
                    }
                    let app_cursor = self.term.mode().contains(TermMode::APP_CURSOR);
                    if let Some(bytes) = key_bytes(key, modifiers, app_cursor) {
                        self.term.scroll_display(Scroll::Bottom);
                        self.write(&bytes);
                    }
                }
                _ => {}
            }
        }
        let content = self.term.renderable_content();
        let selection = content.selection;
        let offset = content.display_offset;
        let cells: Vec<_> = content.display_iter.collect();
        // Paint all backgrounds first: a wide glyph extends into its spacer
        // cell, whose background would otherwise erase the glyph's right half.
        let mut backgrounds: Vec<(Rect, Color32)> = Vec::new();
        for indexed in &cells {
            let row = indexed.point.line.0 + offset as i32;
            if row < 0 {
                continue;
            }
            let bg = if selection.is_some_and(|range| range.contains(indexed.point)) {
                theme.selection()
            } else if indexed.cell.flags.contains(Flags::INVERSE) {
                color(indexed.cell.fg, true, theme)
            } else {
                color(indexed.cell.bg, false, theme)
            };
            // The central panel already paints the default background. Avoid
            // thousands of redundant quads, and merge adjacent colored cells.
            if bg == theme.background() {
                continue;
            }
            let cell_rect =
                terminal_cell_rect(rect.min, cell, indexed.point.column.0, row as usize);
            if let Some((previous, previous_color)) = backgrounds.last_mut() {
                if *previous_color == bg
                    && previous.min.y == cell_rect.min.y
                    && previous.max.x == cell_rect.min.x
                {
                    previous.max.x = cell_rect.max.x;
                    continue;
                }
            }
            backgrounds.push((cell_rect, bg));
        }
        for (background, bg) in backgrounds {
            ui.painter().rect_filled(background, 0.0, bg);
        }
        for indexed in &cells {
            let row = indexed.point.line.0 + offset as i32;
            if row < 0 {
                continue;
            }
            let pos =
                rect.min + Vec2::new(indexed.point.column.0 as f32 * cell.x, row as f32 * cell.y);
            let fg = if selection.is_some_and(|range| range.contains(indexed.point)) {
                theme.selection_foreground()
            } else if indexed.cell.flags.contains(Flags::INVERSE) {
                color(indexed.cell.bg, false, theme)
            } else {
                color(indexed.cell.fg, true, theme)
            };
            if !indexed.cell.flags.intersects(
                Flags::HIDDEN | Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER,
            ) {
                let text = terminal_cell_text(indexed.cell);
                let width = cell.x
                    * if indexed.cell.flags.contains(Flags::WIDE_CHAR) {
                        2.0
                    } else {
                        1.0
                    };
                // Blank cells still need selection/background/underline handling,
                // but have no glyph to lay out or submit to the renderer.
                if !text.is_empty() && text != " " {
                    let galley = fit_terminal_galley(
                        ui.painter().layout_no_wrap(text, font.clone(), fg),
                        width,
                    );
                    let text_pos = pos + Vec2::new(0.0, (cell.y - galley.size().y) / 2.0);
                    ui.painter().galley(text_pos, galley, fg);
                }
                if indexed.cell.flags.contains(Flags::UNDERLINE)
                    || hovered_link
                        .as_ref()
                        .is_some_and(|link| link.points.contains(&indexed.point))
                {
                    let width = if indexed.cell.flags.contains(Flags::WIDE_CHAR) {
                        2.0 * cell.x
                    } else {
                        cell.x
                    };
                    ui.painter().line_segment(
                        [
                            pos + Vec2::new(0.0, cell.y - 2.0),
                            pos + Vec2::new(width, cell.y - 2.0),
                        ],
                        (1.0, fg),
                    );
                }
            }
        }
        if focused
            && offset == 0
            && self.term.mode().contains(TermMode::SHOW_CURSOR)
            && self.cursor_settings.blinking
            && ctx.input(|input| input.focused)
        {
            let elapsed = self.cursor_blink_started.elapsed().as_millis();
            ctx.request_repaint_after(Duration::from_millis(500 - (elapsed % 500) as u64));
        }
        let blink_visible = !ctx.input(|input| input.focused)
            || !self.cursor_settings.blinking
            || self.cursor_blink_started.elapsed().as_millis() % 1000 < 500;
        if focused
            && offset == 0
            && self.term.mode().contains(TermMode::SHOW_CURSOR)
            && blink_visible
        {
            let point = content.cursor.point;
            let pos =
                rect.min + Vec2::new(point.column.0 as f32 * cell.x, point.line.0 as f32 * cell.y);
            let wide = self.term.grid()[point].flags.contains(Flags::WIDE_CHAR);
            let rect = Rect::from_min_size(
                pos,
                Vec2::new(cell.x * if wide { 2.0 } else { 1.0 }, cell.y),
            );
            let shape = match self.cursor_settings.shape {
                CursorShape::Block => rect,
                CursorShape::Underline => {
                    Rect::from_min_max(egui::pos2(rect.left(), rect.bottom() - 2.0), rect.max)
                }
                CursorShape::Beam => Rect::from_min_size(rect.min, Vec2::new(2.0, rect.height())),
            };
            ui.painter().rect_filled(shape, 0.0, theme.cursor());
            if self.cursor_settings.shape == CursorShape::Block {
                let cell_content = &self.term.grid()[point];
                if !cell_content.flags.contains(Flags::HIDDEN) {
                    let text = terminal_cell_text(cell_content);
                    let fg = theme.background();
                    let galley = fit_terminal_galley(
                        ui.painter().layout_no_wrap(text, font.clone(), fg),
                        rect.width(),
                    );
                    let text_pos = pos + Vec2::new(0.0, (cell.y - galley.size().y) / 2.0);
                    ui.painter().galley(text_pos, galley, fg);
                }
            }
        }
        let point = content.cursor.point;
        let cursor_pos =
            rect.min + Vec2::new(point.column.0 as f32 * cell.x, point.line.0 as f32 * cell.y);
        let cursor_rect = Rect::from_min_size(cursor_pos, cell);
        if focused {
            ctx.output_mut(|output| {
                output.ime = Some(egui::output::IMEOutput {
                    rect: cursor_rect,
                    cursor_rect,
                });
            });
        }
        if !self.ime_preedit.is_empty() {
            let galley = ui.painter().layout_no_wrap(
                self.ime_preedit.clone(),
                font.clone(),
                theme.foreground(),
            );
            ui.painter().rect_filled(
                Rect::from_min_size(cursor_pos, galley.size()),
                0.0,
                theme.background(),
            );
            ui.painter().galley(cursor_pos, galley, theme.foreground());
        }
    }
}

impl PaneLayout {
    fn rectangles(
        &mut self,
        ui: &egui::Ui,
        rect: Rect,
        path: &str,
        panes: &mut Vec<(usize, Rect)>,
    ) {
        match self {
            Self::Leaf(index) => panes.push((*index, rect)),
            Self::Split {
                direction,
                ratio,
                first,
                second,
            } => {
                let horizontal = *direction == SplitDirection::Right;
                let length = if horizontal {
                    rect.width()
                } else {
                    rect.height()
                };
                let available = (length - 5.0).max(0.0);
                let minimum = 80.0_f32.min(available / 2.0);
                let mut offset = (available * *ratio).clamp(minimum, available - minimum);
                let divider = if horizontal {
                    Rect::from_min_max(
                        egui::pos2(rect.left() + offset, rect.top()),
                        egui::pos2(rect.left() + offset + 5.0, rect.bottom()),
                    )
                } else {
                    Rect::from_min_max(
                        egui::pos2(rect.left(), rect.top() + offset),
                        egui::pos2(rect.right(), rect.top() + offset + 5.0),
                    )
                };
                let response = ui
                    .interact(divider, ui.id().with(path), egui::Sense::drag())
                    .on_hover_cursor(if horizontal {
                        egui::CursorIcon::ResizeHorizontal
                    } else {
                        egui::CursorIcon::ResizeVertical
                    });
                if response.dragged() && available > 0.0 {
                    offset = (offset
                        + if horizontal {
                            response.drag_delta().x
                        } else {
                            response.drag_delta().y
                        })
                    .clamp(minimum, available - minimum);
                    *ratio = offset / available;
                }
                ui.painter().rect_filled(
                    divider,
                    0.0,
                    if response.hovered() || response.dragged() {
                        ThemeMode::from_context(ui.ctx()).accent()
                    } else {
                        ThemeMode::from_context(ui.ctx()).border()
                    },
                );
                let (first_rect, second_rect) = if horizontal {
                    (
                        Rect::from_min_max(
                            rect.min,
                            egui::pos2(rect.left() + offset, rect.bottom()),
                        ),
                        Rect::from_min_max(
                            egui::pos2(rect.left() + offset + 5.0, rect.top()),
                            rect.max,
                        ),
                    )
                } else {
                    (
                        Rect::from_min_max(rect.min, egui::pos2(rect.right(), rect.top() + offset)),
                        Rect::from_min_max(
                            egui::pos2(rect.left(), rect.top() + offset + 5.0),
                            rect.max,
                        ),
                    )
                };
                first.rectangles(ui, first_rect, &format!("{path}/0"), panes);
                second.rectangles(ui, second_rect, &format!("{path}/1"), panes);
            }
        }
    }
}

impl TerminalTab {
    fn show(&mut self, ctx: &egui::Context, ui: &mut egui::Ui) {
        let mut rectangles = Vec::new();
        self.layout.rectangles(
            ui,
            ui.available_rect_before_wrap(),
            "split",
            &mut rectangles,
        );
        let mut close = None;
        // Resolve focus before delivering this frame's keyboard and clipboard events.
        for &(index, rect) in &rectangles {
            if ctx
                .input(|input| input.pointer.primary_pressed() || input.pointer.secondary_pressed())
                && ui.rect_contains_pointer(rect)
            {
                self.select(index);
            }
            let response = ui.interact(
                rect,
                ui.id().with(("pane-menu", index)),
                egui::Sense::click(),
            );
            self.panes[index].copy_on_right_click(&response, cfg!(windows));
            let header = Rect::from_min_size(rect.min, Vec2::new(rect.width(), 24.0));
            let mut header_ui = ui.new_child(
                egui::UiBuilder::new()
                    .id_salt(("pane", index))
                    .max_rect(header),
            );
            header_ui.set_clip_rect(header.intersect(ui.clip_rect()));
            header_ui.horizontal(|ui| {
                if ui.small_button("×").on_hover_text("Close pane").clicked() {
                    close = Some(index);
                }
                ui.add(egui::Label::new(&self.panes[index].title).truncate())
                    .on_hover_text(&self.panes[index].title);
            });
        }
        for (index, rect) in rectangles {
            let terminal_rect = Rect::from_min_max(
                egui::pos2(rect.left(), (rect.top() + 24.0).min(rect.bottom())),
                rect.max,
            )
            .shrink(2.0);
            let mut pane_ui = ui.new_child(
                egui::UiBuilder::new()
                    .id_salt(("terminal", index))
                    .max_rect(terminal_rect),
            );
            pane_ui.set_clip_rect(terminal_rect.intersect(ui.clip_rect()));
            self.panes[index].show(ctx, &mut pane_ui, index == self.active);
            ui.painter().rect_stroke(
                rect.shrink(0.5),
                0.0,
                (
                    1.0,
                    if index == self.active {
                        ThemeMode::from_context(ui.ctx()).accent()
                    } else {
                        ThemeMode::from_context(ui.ctx()).border()
                    },
                ),
                egui::StrokeKind::Inside,
            );
        }
        if let Some(index) = close {
            self.close_pane(index);
        }
    }
}

// Sending even an unchanged viewport title requests another egui repaint.
fn update_window_title(ctx: &egui::Context, previous: &mut String, title: &str) {
    let title = format!("yiterm — {title}");
    if *previous != title {
        ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
        *previous = title;
    }
}

fn consume_tab_shortcuts(input: &mut egui::InputState) -> (bool, bool, bool, bool) {
    let command = if cfg!(target_os = "macos") {
        egui::Modifiers::MAC_CMD
    } else {
        egui::Modifiers::CTRL | egui::Modifiers::SHIFT
    };
    // egui allows extra Shift when matching Ctrl: consume the specific shortcut first.
    let previous = input.consume_key(egui::Modifiers::CTRL | egui::Modifiers::SHIFT, Key::Tab);
    let next = input.consume_key(egui::Modifiers::CTRL, Key::Tab);
    (
        input.consume_key(command, Key::T),
        input.consume_key(command, Key::W),
        next,
        previous,
    )
}

fn reserve_terminal_keyboard(ctx: &egui::Context) {
    if !egui::Popup::is_any_open(ctx) && ctx.memory(|memory| memory.top_modal_layer().is_none()) {
        // egui resolves Tab/arrow navigation before update. The terminal owns
        // these keys, so cancel navigation before any menu buttons are rendered.
        ctx.memory_mut(|memory| {
            memory.move_focus(egui::FocusDirection::None);
            if let Some(id) = memory.focused() {
                memory.surrender_focus(id);
            }
        });
    }
}

impl eframe::App for TerminalApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.updater.poll();
        if matches!(self.updater.state, yiterm::update::State::Restarting) {
            for tab in &mut self.tabs {
                for pane in &mut tab.panes {
                    let _ = pane.child.kill();
                }
            }
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        let profile_start = self.profile.as_ref().map(|_| Instant::now());
        ctx.set_visuals(self.theme.visuals());
        if !self.settings_open {
            reserve_terminal_keyboard(ctx);
        }
        let poll_start = self.profile.as_ref().map(|_| Instant::now());
        // Drain every PTY, including background tabs, so output and paste results
        // cannot block and terminal queries are answered while another tab is active.
        for tab in &self.tabs {
            for pane in &tab.panes {
                pane.repaint.get_or_init(|| ctx.clone());
            }
        }
        for index in (0..self.tabs.len()).rev() {
            if !self.tabs[index].poll() {
                self.close_tab(index);
            }
        }
        let profile_poll = poll_start.map(|start| start.elapsed());
        #[cfg(windows)]
        let clipboard_owner = windows_clipboard_owner(_frame).map_err(|error| format!("{error:#}"));
        for tab in &mut self.tabs {
            for pane in &mut tab.panes {
                #[cfg(windows)]
                pane.flush_clipboard(clipboard_owner.clone());
                #[cfg(not(windows))]
                pane.flush_clipboard();
            }
        }
        if self.tabs.is_empty() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        let composing = ctx.input(|input| {
            ime_owns_keyboard(&self.tabs[self.active].focused().ime_preedit, &input.events)
        });
        if !composing && !self.about_open && !self.settings_open {
            let (new, close, next, previous) = ctx.input_mut(consume_tab_shortcuts);
            if new {
                self.new_tab();
            }
            if close {
                self.close_tab(self.active);
            }
            if !self.tabs.is_empty() && (next || previous) {
                let count = self.tabs.len();
                self.select((self.active + if previous { count - 1 } else { 1 }) % count);
            }
        }
        let mut selected = None;
        let mut closed = None;
        let mut new = false;
        let mut split = None;
        let mut close_pane = false;
        egui::TopBottomPanel::top("menu").show(ctx, |ui| {
            egui::MenuBar::new().ui(ui, |ui| {
                if ui.button("Settings").clicked() {
                    self.settings_open = true;
                }
                ui.menu_button("Pane", |ui| {
                    if ui.button("Split Right").clicked() {
                        split = Some(SplitDirection::Right);
                        ui.close();
                    }
                    if ui.button("Split Down").clicked() {
                        split = Some(SplitDirection::Down);
                        ui.close();
                    }
                    if ui.button("Close Pane").clicked() {
                        close_pane = true;
                        ui.close();
                    }
                });
                ui.menu_button("Tools", |ui| {
                    if ui.button("Open Log File").clicked() {
                        if let Err(error) = logging::open_current_file(&self.log_directory) {
                            self.error = Some(format!("{error:#}"));
                        }
                        ui.close();
                    }
                });
                if ui.button("About").clicked() {
                    self.about_open = true;
                    self.about_licenses = false;
                }
            });
            egui::ScrollArea::horizontal()
                .id_salt("tabs")
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        for (index, session) in self.tabs.iter().enumerate() {
                            ui.push_id(index, |ui| {
                                if ui
                                    .add_sized(
                                        [180.0, ui.spacing().interact_size.y],
                                        egui::Button::selectable(
                                            index == self.active,
                                            format!("{}: {}", index + 1, session.focused().title),
                                        )
                                        .truncate(),
                                    )
                                    .on_hover_text(&session.focused().title)
                                    .clicked()
                                {
                                    selected = Some(index);
                                }
                                if ui.small_button("×").on_hover_text("Close tab").clicked() {
                                    closed = Some(index);
                                }
                            });
                        }
                        new = ui.button("+").on_hover_text("New tab").clicked();
                    });
                });
            if let Some(error) = &self.error {
                ui.colored_label(self.theme.palette(1), error);
            }
        });
        if let Some(index) = selected {
            self.select(index);
        }
        if let Some(index) = closed {
            self.close_tab(index);
        }
        if new {
            self.new_tab();
        }
        if self.tabs.is_empty() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        update_window_title(
            ctx,
            &mut self.window_title,
            &self.tabs[self.active].focused().title,
        );
        if let Some(direction) = split {
            self.split_pane(direction);
        }
        if close_pane {
            let tab = &mut self.tabs[self.active];
            tab.close_pane(tab.active);
        }
        if self.tabs[self.active].panes.is_empty() {
            self.close_tab(self.active);
            if self.tabs.is_empty() {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                return;
            }
        }
        if self.settings_open {
            let before = (
                self.cursor_settings,
                self.theme,
                self.font_settings.clone(),
                self.shell_settings.clone(),
            );
            let modal = show_settings(
                ctx,
                &mut self.cursor_settings,
                &mut self.theme,
                &mut self.font_settings,
                &mut self.shell_settings,
            );
            if modal.inner || modal.should_close() {
                self.settings_open = false;
            }
            if before
                != (
                    self.cursor_settings,
                    self.theme,
                    self.font_settings.clone(),
                    self.shell_settings.clone(),
                )
            {
                if before.2.family != self.font_settings.family {
                    configure_fonts_for(ctx, &self.font_settings.family);
                }
                ctx.set_visuals(self.theme.visuals());
                let result = self
                    .args
                    .config
                    .clone()
                    .map(Ok)
                    .unwrap_or_else(logging::config_path)
                    .and_then(|path| {
                        settings::save_terminal_preferences(
                            &path,
                            self.cursor_settings,
                            self.theme,
                            &self.font_settings,
                            self.browser_settings,
                            &self.shell_settings,
                        )
                    });
                if let Err(error) = result {
                    self.error = Some(format!("Cannot save settings: {error:#}"));
                }
                for tab in &mut self.tabs {
                    for pane in &mut tab.panes {
                        pane.cursor_blink_started = Instant::now();
                    }
                }
            }
        }
        for tab in &mut self.tabs {
            for pane in &mut tab.panes {
                pane.cursor_settings = self.cursor_settings;
                pane.font_settings = self.font_settings.clone();
            }
        }
        if self.about_open {
            let modal = show_about(ctx, &mut self.about_licenses, &mut self.updater);
            if modal.inner || modal.should_close() {
                self.about_open = false;
            }
        }
        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            let session = self.tabs[self.active].focused_mut();
            ui.horizontal(|ui| {
                if ui.button("Paste").clicked() {
                    session.paste();
                }
                ui.label(&session.status);
            });
        });
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(self.theme.background()))
            .show(ctx, |ui| {
                self.tabs[self.active].show(ctx, ui);
            });
        // A pane can be closed from its header during rendering.
        if self.tabs[self.active].panes.is_empty() {
            self.close_tab(self.active);
            if self.tabs.is_empty() {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
        // PTY output wakes the UI directly. Keep a low-frequency fallback for
        // child exit, asynchronous paste results and Windows clipboard IPC.
        ctx.request_repaint_after(Duration::from_millis(250));
        if let (Some(profile), Some(start), Some(poll)) =
            (&mut self.profile, profile_start, profile_poll)
        {
            profile.frame(ctx, start.elapsed(), poll, _frame.info().cpu_usage);
        }
    }
}

fn system_font_families(ctx: &egui::Context) -> std::sync::Arc<Vec<String>> {
    let id = egui::Id::new("system-font-families");
    if let Some(fonts) = ctx.data(|data| data.get_temp::<std::sync::Arc<Vec<String>>>(id)) {
        return fonts;
    }
    let output = std::process::Command::new("fc-list")
        .arg("--format=%{family[0]}\n")
        .output();
    let mut names = vec!["Lilex".to_owned()];
    match output {
        Ok(output) if output.status.success() => {
            names.extend(
                String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .map(str::trim)
                    .filter(|name| !name.is_empty() && !name.starts_with('.'))
                    .map(str::to_owned),
            );
        }
        _ => tracing::warn!("Fontconfig font enumeration unavailable"),
    }
    names.sort_by_key(|name| name.to_lowercase());
    names.dedup();
    let names = std::sync::Arc::new(names);
    ctx.data_mut(|data| data.insert_temp(id, names.clone()));
    names
}

fn settings_row(
    ui: &mut egui::Ui,
    title: &str,
    detail: &str,
    width: f32,
    controls: impl FnOnce(&mut egui::Ui),
) {
    ui.add_space(16.0);
    ui.horizontal(|ui| {
        let label_width = (ui.available_width() - width - 20.0).max(80.0);
        ui.allocate_ui_with_layout(
            Vec2::new(label_width, 0.0),
            egui::Layout::top_down(egui::Align::Min),
            |ui| {
                ui.label(egui::RichText::new(title).size(14.0));
                ui.add_space(4.0);
                ui.label(egui::RichText::new(detail).size(12.0).weak());
            },
        );
        ui.allocate_ui_with_layout(
            Vec2::new(width, 0.0),
            egui::Layout::left_to_right(egui::Align::Center),
            controls,
        );
    });
    ui.add_space(16.0);
}

fn show_settings(
    ctx: &egui::Context,
    settings: &mut CursorSettings,
    theme: &mut ThemeMode,
    font: &mut settings::FontSettings,
    shell: &mut settings::ShellSettings,
) -> egui::ModalResponse<bool> {
    let families = system_font_families(ctx);
    egui::Modal::new(egui::Id::new("settings"))
        .area(
            egui::Modal::default_area(egui::Id::new("settings"))
                .default_size(Vec2::new(600.0, ctx.content_rect().height() * 0.95)),
        )
        .frame(
            egui::Frame::window(&ctx.style())
                .inner_margin(24.0)
                .corner_radius(10.0),
        )
        .show(ctx, |ui| {
            ui.set_width(552.0_f32.min((ctx.content_rect().width() - 64.0).max(300.0)));
            ui.style_mut()
                .text_styles
                .insert(egui::TextStyle::Body, FontId::proportional(13.0));
            ui.spacing_mut().button_padding = Vec2::new(8.0, 5.0);
            let mut done = false;
            egui::ScrollArea::vertical()
                .auto_shrink([false, true])
                .max_height((ctx.content_rect().height() * 0.95 - 100.0).clamp(120.0, 740.0))
                .show(ui, |ui| {
                    ui.label(egui::RichText::new("Settings").size(20.0));
                    ui.add_space(20.0);
                    ui.weak("TERMINAL");
                    settings_row(
                        ui,
                        "Shell",
                        "Shell for new terminals. --shell takes priority.",
                        148.0,
                        |ui| {
                            egui::ComboBox::from_id_salt("shell-mode")
                                .selected_text(if shell.mode == settings::ShellMode::Default {
                                    "Default"
                                } else {
                                    "Program"
                                })
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(
                                        &mut shell.mode,
                                        settings::ShellMode::Default,
                                        "Default",
                                    );
                                    ui.selectable_value(
                                        &mut shell.mode,
                                        settings::ShellMode::Program,
                                        "Program",
                                    );
                                });
                        },
                    );
                    if shell.mode == settings::ShellMode::Program {
                        settings_row(
                            ui,
                            "Program",
                            "Executable name or path, without arguments.",
                            260.0,
                            |ui| {
                                ui.text_edit_singleline(&mut shell.program);
                            },
                        );
                        if shell.program.trim().is_empty() {
                            ui.colored_label(egui::Color32::RED, "Shell program cannot be empty");
                        }
                    }
                    ui.separator();
                    ui.weak("APPEARANCE");
                    settings_row(
                        ui,
                        "Theme",
                        "Choose the window and terminal appearance.",
                        148.0,
                        |ui| {
                            ui.selectable_value(theme, ThemeMode::Dark, "Dark");
                            ui.selectable_value(theme, ThemeMode::Light, "Light");
                        },
                    );
                    ui.separator();
                    settings_row(
                        ui,
                        "Font Size",
                        "Terminal text size in pixels, from 8 to 48.",
                        148.0,
                        |ui| {
                            ui.spacing_mut().item_spacing.x = 0.0;
                            if ui
                                .add_enabled(
                                    font.size > 8.0,
                                    egui::Button::new("−").min_size(Vec2::new(30.0, 28.0)),
                                )
                                .clicked()
                            {
                                font.size = (font.size - 1.0).max(8.0);
                            }
                            ui.add_sized(
                                [82.0, 28.0],
                                egui::DragValue::new(&mut font.size)
                                    .range(8.0..=48.0)
                                    .speed(1.0)
                                    .suffix(" px"),
                            );
                            if ui
                                .add_enabled(
                                    font.size < 48.0,
                                    egui::Button::new("+").min_size(Vec2::new(30.0, 28.0)),
                                )
                                .clicked()
                            {
                                font.size = (font.size + 1.0).min(48.0);
                            }
                        },
                    );
                    ui.separator();
                    settings_row(
                        ui,
                        "Font Family",
                        "Use a monospace font to keep characters aligned.",
                        220.0,
                        |ui| {
                            let button = ui.add_sized(
                                [220.0, 28.0],
                                egui::Button::new(&font.family).right_text("⏷").truncate(),
                            );
                            egui::Popup::menu(&button)
                                .width(button.rect.width())
                                .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
                                .show(|ui| {
                                    let search_id = egui::Id::new("font-search");
                                    let mut search = ctx.data_mut(|data| {
                                        data.get_temp::<String>(search_id).unwrap_or_default()
                                    });
                                    let search_changed = ui
                                        .add(
                                            egui::TextEdit::singleline(&mut search)
                                                .hint_text("Search fonts…")
                                                .desired_width(200.0),
                                        )
                                        .changed();
                                    let query = search.to_lowercase();
                                    ctx.data_mut(|data| data.insert_temp(search_id, search));
                                    ui.separator();
                                    // Keep search outside the scrolling font list.
                                    // A popup's sizing pass can collapse a scroll area to its
                                    // default 64px minimum; max_height alone won't expand it.
                                    let list_height =
                                        (ctx.content_rect().height() - 100.0).clamp(64.0, 300.0);
                                    let mut list = egui::ScrollArea::vertical()
                                        .id_salt("font-family-list")
                                        .min_scrolled_height(list_height)
                                        .max_height(list_height);
                                    if search_changed {
                                        list = list.vertical_scroll_offset(0.0);
                                    }
                                    list.show(ui, |ui| {
                                        let mut found = false;
                                        for family in families
                                            .iter()
                                            .filter(|name| name.to_lowercase().contains(&query))
                                        {
                                            found = true;
                                            if ui
                                                .selectable_value(
                                                    &mut font.family,
                                                    family.clone(),
                                                    family,
                                                )
                                                .clicked()
                                            {
                                                ctx.data_mut(|data| {
                                                    data.remove::<String>(search_id)
                                                });
                                                ui.close();
                                            }
                                        }
                                        if !found {
                                            ui.weak("No matching fonts");
                                        }
                                    });
                                });
                        },
                    );
                    let preview_height = font.size * TERMINAL_LINE_HEIGHT + 28.0;
                    egui::Frame::new()
                        .fill(theme.background())
                        .corner_radius(6.0)
                        .inner_margin(14.0)
                        .show(ui, |ui| {
                            ui.set_min_size(Vec2::new(ui.available_width(), preview_height - 28.0));
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new("Aa Bb 0123  ~/projects")
                                        .font(FontId::monospace(font.size)),
                                )
                                .truncate(),
                            );
                        });
                    ui.add_space(24.0);
                    ui.weak("CURSOR");
                    settings_row(
                        ui,
                        "Cursor Style",
                        "Choose the terminal cursor shape.",
                        220.0,
                        |ui| {
                            for (shape, label) in [
                                (CursorShape::Block, "Block"),
                                (CursorShape::Underline, "Underline"),
                                (CursorShape::Beam, "Bar"),
                            ] {
                                ui.selectable_value(&mut settings.shape, shape, label);
                            }
                        },
                    );
                    ui.separator();
                    settings_row(
                        ui,
                        "Cursor Blinking",
                        "Blink the cursor while the terminal is focused.",
                        70.0,
                        |ui| {
                            ui.toggle_value(&mut settings.blinking, "Enabled");
                        },
                    );
                });
            ui.separator();
            ui.add_space(16.0);
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new("Changes are saved automatically.")
                        .size(11.0)
                        .weak(),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    done = ui.button("Done").clicked();
                    if ui.button("Reset Defaults").clicked() {
                        *settings = CursorSettings::default();
                        *theme = ThemeMode::default();
                        *font = settings::FontSettings::default();
                        *shell = settings::ShellSettings::default();
                        ctx.data_mut(|data| data.remove::<String>(egui::Id::new("font-search")));
                    }
                });
            });
            done
        })
}

fn show_about(
    ctx: &egui::Context,
    licenses: &mut bool,
    updater: &mut yiterm::update::Controller,
) -> egui::ModalResponse<bool> {
    egui::Modal::new(egui::Id::new("about-yiterm")).show(ctx, |ui| {
        ui.set_width(360.0);
        ui.vertical_centered(|ui| {
            ui.add_space(18.0);
            let (rect, _) = ui.allocate_exact_size(Vec2::splat(72.0), egui::Sense::hover());
            ui.painter()
                .rect_filled(rect, 18.0, ThemeMode::from_context(ctx).selection());
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                ">_",
                FontId::monospace(30.0),
                ThemeMode::from_context(ui.ctx()).accent(),
            );
            ui.add_space(12.0);
            ui.label(egui::RichText::new("YiTerm").size(28.0).strong());
            ui.add_space(8.0);
            ui.label("A lightweight terminal for macOS, Windows and Linux.");
            ui.label("Tabs, split panes and image paste over SSH.");
            ui.add_space(20.0);
            ui.label(format!("Version  {}", env!("CARGO_PKG_VERSION")));
            if let Some(revision) = option_env!("YITERM_BUILD_REV") {
                let short = revision.get(..8).unwrap_or(revision);
                ui.allocate_ui_with_layout(
                    Vec2::new(ui.available_width(), ui.spacing().interact_size.y),
                    egui::Layout::left_to_right(egui::Align::Center)
                        .with_main_align(egui::Align::Center),
                    |ui| {
                        ui.label("Commit");
                        ui.hyperlink_to(
                            short,
                            format!("https://github.com/wynemo/yiterm/commit/{revision}"),
                        );
                    },
                );
            } else {
                ui.weak("Local development build");
            }
            ui.add_space(20.0);
            ui.allocate_ui_with_layout(
                Vec2::new(ui.available_width(), ui.spacing().interact_size.y),
                egui::Layout::left_to_right(egui::Align::Center)
                    .with_main_align(egui::Align::Center),
                |ui| {
                    ui.hyperlink_to(
                        "Docs",
                        "https://github.com/wynemo/yiterm/blob/main/TECHNICAL.md",
                    );
                    ui.separator();
                    ui.hyperlink_to("GitHub", "https://github.com/wynemo/yiterm");
                    ui.separator();
                    if ui.button("Licenses").clicked() {
                        *licenses = !*licenses;
                    }
                },
            );
            ui.add_space(12.0);
        });
        if let Some(label) = updater.state.action_label() {
            if ui.button(label).clicked() {
                updater.act();
            }
        }
        let message = updater.state.message();
        let has_update_message = !message.is_empty();
        if has_update_message {
            egui::ScrollArea::vertical()
                .id_salt("update-message")
                .max_height(80.0)
                .show(ui, |ui| {
                    ui.label(message);
                });
        }
        if let Some(url) = updater.state.notes_url() {
            ui.hyperlink_to("Release Notes", url);
        }
        if *licenses {
            egui::ScrollArea::vertical()
                .max_height(if has_update_message { 120.0 } else { 220.0 })
                .show(ui, |ui| {
                    ui.heading("IBM Plex Sans · SIL Open Font License 1.1");
                    ui.label(include_str!("../assets/fonts/ibm-plex-sans/OFL.txt"));
                    ui.separator();
                    ui.heading("Lilex · SIL Open Font License 1.1");
                    ui.label(include_str!("../assets/fonts/lilex/OFL.txt"));
                    ui.separator();
                    ui.heading("egui-winit · MIT / Apache-2.0");
                    ui.label(include_str!("../vendor/egui-winit/LICENSE-MIT"));
                    ui.label(include_str!("../vendor/egui-winit/LICENSE-APACHE"));
                    #[cfg(windows)]
                    {
                        ui.separator();
                        ui.heading("Microsoft ConPTY");
                        ui.label(include_str!("../licenses/conpty-LICENSE"));
                    }
                });
        }
        ui.vertical_centered(|ui| ui.button("Close").clicked())
            .inner
    })
}

fn mouse_button_code(button: egui::PointerButton) -> Option<u8> {
    match button {
        egui::PointerButton::Primary => Some(0),
        egui::PointerButton::Middle => Some(1),
        egui::PointerButton::Secondary => Some(2),
        _ => None,
    }
}

fn mouse_bytes(
    protocol: mouse::Protocol,
    rect: Rect,
    cell: Vec2,
    size: Size,
    pos: egui::Pos2,
    action: (u8, bool),
    modifiers: egui::Modifiers,
) -> Option<Vec<u8>> {
    let mode = protocol.mode;
    let (button, pressed) = action;
    let col = (((pos.x - rect.min.x) / cell.x).floor().max(0.0) as usize).min(size.cols - 1) + 1;
    let row = (((pos.y - rect.min.y) / cell.y).floor().max(0.0) as usize).min(size.rows - 1) + 1;
    let modifiers =
        u8::from(modifiers.shift) * 4 + u8::from(modifiers.alt) * 8 + u8::from(modifiers.ctrl) * 16;
    if mode.contains(TermMode::SGR_MOUSE) {
        let suffix = if pressed { 'M' } else { 'm' };
        Some(format!("\x1b[<{};{col};{row}{suffix}", button + modifiers).into_bytes())
    } else if protocol.urxvt {
        let code = if pressed { button } else { 3 };
        Some(format!("\x1b[{};{col};{row}M", 32 + code + modifiers).into_bytes())
    } else {
        // The original X10 encoding cannot represent coordinates above 223.
        if col > 223 || row > 223 {
            return None;
        }
        let code = if pressed { button } else { 3 };
        Some(vec![
            0x1b,
            b'[',
            b'M',
            32 + code + modifiers,
            32 + col as u8,
            32 + row as u8,
        ])
    }
}

fn ime_owns_keyboard(preedit: &str, events: &[egui::Event]) -> bool {
    !preedit.is_empty()
        || events.iter().any(|event| {
            matches!(event,
                egui::Event::Ime(egui::ImeEvent::Preedit(text)) if !text.is_empty()
            ) || matches!(event, egui::Event::Ime(egui::ImeEvent::Commit(_)))
        })
}

fn is_paste_shortcut(key: Key, modifiers: egui::Modifiers) -> bool {
    (key == Key::V && (modifiers.mac_cmd || modifiers.ctrl && modifiers.shift))
        || (key == Key::Insert
            && modifiers.shift
            && !modifiers.ctrl
            && !modifiers.alt
            && !modifiers.mac_cmd)
}

fn is_copy_shortcut(key: Key, modifiers: egui::Modifiers) -> bool {
    key == Key::Copy
        || (key == Key::C && (modifiers.mac_cmd || modifiers.ctrl && modifiers.shift))
        || (key == Key::Insert && modifiers.ctrl && !modifiers.shift)
}

fn selection_point(
    rect: Rect,
    cell: Vec2,
    size: Size,
    offset: usize,
    pos: egui::Pos2,
) -> (Point, Side) {
    let x = ((pos.x - rect.min.x) / cell.x).clamp(0.0, size.cols as f32);
    let column = (x.floor() as usize).min(size.cols - 1);
    let row = (((pos.y - rect.min.y) / cell.y).floor() as i32).clamp(0, size.rows as i32 - 1);
    let side = if x - (column as f32) < 0.5 {
        Side::Left
    } else {
        Side::Right
    };
    (Point::new(Line(row - offset as i32), Column(column)), side)
}

fn key_bytes(key: Key, modifiers: egui::Modifiers, app_cursor: bool) -> Option<Vec<u8>> {
    if modifiers.ctrl && !modifiers.shift {
        let name = format!("{key:?}");
        if name.len() == 1 && name.as_bytes()[0].is_ascii_uppercase() {
            return Some(vec![name.as_bytes()[0] - b'A' + 1]);
        }
    }
    let arrow = if app_cursor { "\x1bO" } else { "\x1b[" };
    let text = match key {
        Key::Enter => "\r".into(),
        Key::Tab if modifiers.shift => "\x1b[Z".into(),
        Key::Tab => "\t".into(),
        Key::Backspace => "\x7f".into(),
        Key::Escape => "\x1b".into(),
        Key::ArrowUp => format!("{arrow}A"),
        Key::ArrowDown => format!("{arrow}B"),
        Key::ArrowRight => format!("{arrow}C"),
        Key::ArrowLeft => format!("{arrow}D"),
        Key::Home => "\x1b[H".into(),
        Key::End => "\x1b[F".into(),
        Key::Insert => "\x1b[2~".into(),
        Key::Delete => "\x1b[3~".into(),
        Key::PageUp => "\x1b[5~".into(),
        Key::PageDown => "\x1b[6~".into(),
        _ => return None,
    };
    Some(text.into_bytes())
}

pub(super) fn run(app: TerminalApp) -> Result<()> {
    eframe::run_native(
        "yiterm",
        eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default()
                .with_inner_size([960.0, 640.0])
                .with_icon(eframe::icon_data::from_png_bytes(include_bytes!(
                    "../assets/icons/yiterm.png"
                ))?),
            // Terminal colors are flat; per-pixel gradient dithering wastes
            // substantial CPU when Mesa renders the window in software.
            dithering: false,
            ..Default::default()
        },
        Box::new(move |cc| {
            if profiling::enabled() {
                if let Some(gl) = &cc.gl {
                    use eframe::glow::HasContext;
                    // eframe has made this GL context current for app creation.
                    unsafe {
                        tracing::info!(target: "yiterm::profile",
                            vendor = %gl.get_parameter_string(eframe::glow::VENDOR),
                            renderer = %gl.get_parameter_string(eframe::glow::RENDERER),
                            version = %gl.get_parameter_string(eframe::glow::VERSION),
                            "OpenGL renderer");
                    }
                }
            }
            configure_fonts_for(&cc.egui_ctx, &app.font_settings.family);
            Ok(Box::new(app))
        }),
    )
    .map_err(|error| anyhow::anyhow!(error.to_string()))
}

fn fit_terminal_galley(mut galley: Arc<egui::Galley>, width: f32) -> Arc<egui::Galley> {
    if galley.size().x <= width + 0.1 {
        return galley;
    }
    let left = galley.mesh_bounds.left().min(0.0);
    let right = galley.mesh_bounds.right().max(galley.size().x);
    let scale = width / (right - left);
    let map_x = |x: f32| (x - left) * scale;
    let map_rect = |rect: &mut Rect| {
        rect.min.x = map_x(rect.min.x);
        rect.max.x = map_x(rect.max.x);
    };
    let fitted = Arc::make_mut(&mut galley);
    map_rect(&mut fitted.rect);
    map_rect(&mut fitted.mesh_bounds);
    for placed in &mut fitted.rows {
        placed.pos.x *= scale;
        let row = Arc::make_mut(&mut placed.row);
        row.size.x *= scale;
        map_rect(&mut row.visuals.mesh_bounds);
        for vertex in &mut row.visuals.mesh.vertices {
            vertex.pos.x = map_x(vertex.pos.x);
        }
        for glyph in &mut row.glyphs {
            glyph.pos.x = map_x(glyph.pos.x);
            glyph.advance_width *= scale;
        }
    }
    galley
}

// Unicode 17.0 Default_Ignorable_Code_Point, including reserved ranges.
// https://www.unicode.org/Public/17.0.0/ucd/DerivedCoreProperties.txt
// Do not replace this with a zero-width check: accents and enclosing marks are
// zero-width too, but carry visible content.
pub(super) fn is_default_ignorable(ch: char) -> bool {
    matches!(ch,
        '\u{00AD}' | '\u{034F}' | '\u{061C}'
        | '\u{115F}'..='\u{1160}'
        | '\u{17B4}'..='\u{17B5}'
        | '\u{180B}'..='\u{180F}'
        | '\u{200B}'..='\u{200F}'
        | '\u{202A}'..='\u{202E}'
        | '\u{2060}'..='\u{206F}'
        | '\u{3164}'
        | '\u{FE00}'..='\u{FE0F}'
        | '\u{FEFF}' | '\u{FFA0}'
        | '\u{FFF0}'..='\u{FFF8}'
        | '\u{1BCA0}'..='\u{1BCA3}'
        | '\u{1D173}'..='\u{1D17A}'
        | '\u{E0000}'..='\u{E0FFF}'
    )
}

fn configure_fonts(ctx: &egui::Context) {
    configure_fonts_for(ctx, "Lilex");
}

fn configure_fonts_for(ctx: &egui::Context, family: &str) {
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "IBM Plex Sans".into(),
        std::sync::Arc::new(egui::FontData::from_static(include_bytes!(
            "../assets/fonts/ibm-plex-sans/IBMPlexSans-Regular.ttf"
        ))),
    );
    fonts
        .families
        .entry(egui::FontFamily::Proportional)
        .or_default()
        .insert(0, "IBM Plex Sans".into());
    fonts.font_data.insert(
        "Lilex".into(),
        std::sync::Arc::new(egui::FontData::from_static(include_bytes!(
            "../assets/fonts/lilex/Lilex-Regular.ttf"
        ))),
    );
    fonts
        .families
        .entry(egui::FontFamily::Monospace)
        .or_default()
        .insert(0, "Lilex".into());
    #[cfg(windows)]
    {
        let windows = std::env::var_os("WINDIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from(r"C:\Windows"));
        // Append fallback fonts so ASCII and box drawing keep Lilex's metrics.
        // Remote macOS apps use symbols such as ⌃/⌥/⌘ for keyboard shortcuts;
        // the built-in and CJK fonts do not cover all of these glyphs.
        // FontData uses face zero, including for Windows TTC collections.
        for filename in ["seguisym.ttf", "msyh.ttc", "simhei.ttf", "simsun.ttc"] {
            let Ok(data) = std::fs::read(windows.join("Fonts").join(filename)) else {
                continue;
            };
            fonts.font_data.insert(
                filename.into(),
                std::sync::Arc::new(egui::FontData::from_owned(data)),
            );
            for family in [egui::FontFamily::Monospace, egui::FontFamily::Proportional] {
                fonts
                    .families
                    .entry(family)
                    .or_default()
                    .push(filename.into());
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        for path in [
            "/System/Library/Fonts/PingFang.ttc",
            "/System/Library/Fonts/STHeiti Light.ttc",
        ] {
            let Ok(data) = std::fs::read(path) else {
                continue;
            };
            fonts.font_data.insert(
                path.into(),
                std::sync::Arc::new(egui::FontData::from_owned(data)),
            );
            for family in [egui::FontFamily::Monospace, egui::FontFamily::Proportional] {
                fonts.families.entry(family).or_default().push(path.into());
            }
        }
    }
    #[cfg(target_os = "linux")]
    {
        // egui does not inherit Fontconfig fallback from the desktop terminal.
        // Preserve the selected TTC face instead of assuming collection index 0.
        let mut loaded = std::collections::HashSet::new();
        for pattern in [
            "monospace:lang=zh-cn",
            "monospace:lang=zh-tw",
            "monospace:lang=ja",
            "monospace:lang=ko",
            "sans-serif:charset=2318",
        ] {
            let Ok(output) = std::process::Command::new("fc-match")
                .args(["--format=%{file}\n%{index}\n", pattern])
                .output()
            else {
                tracing::warn!(
                    "fc-match unavailable; Linux system font fallback could not be loaded"
                );
                break;
            };
            if !output.status.success() {
                continue;
            }
            let result = String::from_utf8_lossy(&output.stdout);
            let mut lines = result.lines();
            let Some(path) = lines.next().filter(|path| !path.is_empty()) else {
                continue;
            };
            let Some(index) = lines.next().and_then(|index| index.parse::<u32>().ok()) else {
                continue;
            };
            if !loaded.insert((path.to_owned(), index)) {
                continue;
            }
            let Ok(data) = std::fs::read(path) else {
                continue;
            };
            let name = format!("linux:{path}:{index}");
            let mut data = egui::FontData::from_owned(data);
            data.index = index;
            fonts
                .font_data
                .insert(name.clone(), std::sync::Arc::new(data));
            for family in [egui::FontFamily::Monospace, egui::FontFamily::Proportional] {
                fonts.families.entry(family).or_default().push(name.clone());
            }
            tracing::info!(path, index, "Loaded Linux fallback font");
        }
    }
    if family != "Lilex" && !family.trim().is_empty() {
        if let Ok(output) = std::process::Command::new("fc-match")
            .args(["--format=%{file}\n%{index}\n", family])
            .output()
        {
            let result = String::from_utf8_lossy(&output.stdout);
            let mut lines = result.lines();
            if let (Some(path), Some(index)) = (
                lines.next(),
                lines.next().and_then(|v| v.parse::<u32>().ok()),
            ) {
                if let Ok(bytes) = std::fs::read(path) {
                    let mut data = egui::FontData::from_owned(bytes);
                    data.index = index;
                    fonts
                        .font_data
                        .insert("selected-terminal".into(), std::sync::Arc::new(data));
                    fonts
                        .families
                        .entry(egui::FontFamily::Monospace)
                        .or_default()
                        .insert(0, "selected-terminal".into());
                }
            }
        }
    }
    ctx.set_fonts(fonts);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ansi_white_text_is_readable_in_light_theme_without_changing_backgrounds() {
        for (named, index) in [(NamedColor::White, 7), (NamedColor::BrightWhite, 15)] {
            for value in [Color::Named(named), Color::Indexed(index)] {
                assert_eq!(
                    color(value, true, ThemeMode::Light),
                    ThemeMode::Light.foreground()
                );
                assert_eq!(
                    color(value, false, ThemeMode::Light),
                    ThemeMode::Light.palette(index as usize)
                );
                assert_eq!(
                    color(value, true, ThemeMode::Dark),
                    ThemeMode::Dark.palette(index as usize)
                );
            }
        }
    }

    #[test]
    fn theme_colors_preserve_explicit_terminal_colors() {
        use alacritty_terminal::vte::ansi::Rgb;
        let rgb = Color::Spec(Rgb {
            r: 12,
            g: 34,
            b: 56,
        });
        for theme in [ThemeMode::Dark, ThemeMode::Light] {
            assert_eq!(
                color(Color::Named(NamedColor::Background), false, theme),
                theme.background()
            );
            assert_eq!(
                color(Color::Named(NamedColor::Foreground), true, theme),
                theme.foreground()
            );
            assert_eq!(color(rgb, true, theme), Color32::from_rgb(12, 34, 56));
            for index in 16..=255 {
                assert_eq!(
                    color(Color::Indexed(index), true, theme),
                    color(Color::Indexed(index), true, ThemeMode::Dark)
                );
            }
        }
        assert_ne!(
            color(Color::Indexed(1), true, ThemeMode::Dark),
            color(Color::Indexed(1), true, ThemeMode::Light)
        );
    }

    #[test]
    fn settings_reset_restores_all_defaults() {
        let ctx = egui::Context::default();
        let mut cursor = CursorSettings {
            shape: CursorShape::Beam,
            blinking: false,
        };
        let mut theme = ThemeMode::Light;
        let mut font = settings::FontSettings {
            family: "Test Mono".into(),
            size: 32.0,
        };
        let mut button = egui::Pos2::ZERO;
        for frame in 0..4 {
            let events = if frame == 3 {
                vec![
                    egui::Event::PointerMoved(button),
                    egui::Event::PointerButton {
                        pos: button,
                        button: egui::PointerButton::Primary,
                        pressed: true,
                        modifiers: Default::default(),
                    },
                    egui::Event::PointerButton {
                        pos: button,
                        button: egui::PointerButton::Primary,
                        pressed: false,
                        modifiers: Default::default(),
                    },
                ]
            } else {
                vec![]
            };
            let output = ctx.run(
                egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(
                        egui::Pos2::ZERO,
                        Vec2::new(710.0, 900.0),
                    )),
                    events,
                    ..Default::default()
                },
                |ctx| {
                    show_settings(
                        ctx,
                        &mut cursor,
                        &mut theme,
                        &mut font,
                        &mut settings::ShellSettings::default(),
                    );
                },
            );
            if frame == 2 {
                button = output
                    .shapes
                    .iter()
                    .find_map(|shape| {
                        if let egui::Shape::Text(t) = &shape.shape {
                            (t.galley.text() == "Reset Defaults")
                                .then(|| Rect::from_min_size(t.pos, t.galley.size()).center())
                        } else {
                            None
                        }
                    })
                    .expect("Reset Defaults button is visible");
            }
        }
        assert_eq!(cursor, CursorSettings::default());
        assert_eq!(theme, ThemeMode::default());
        assert_eq!(font, settings::FontSettings::default());
    }

    #[test]
    fn settings_font_picker_searches_and_selects_installed_font() {
        let ctx = egui::Context::default();
        ctx.data_mut(|data| {
            data.insert_temp(
                egui::Id::new("system-font-families"),
                std::sync::Arc::new(
                    ["Lilex".to_owned(), "Test Mono".to_owned()]
                        .into_iter()
                        .chain((0..80).map(|i| format!("Font {i:03}")))
                        .collect::<Vec<_>>(),
                ),
            )
        });
        let mut cursor = CursorSettings::default();
        let mut theme = ThemeMode::Dark;
        let mut font = settings::FontSettings::default();
        let mut render = |events: Vec<egui::Event>| {
            ctx.run(
                egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(
                        egui::Pos2::ZERO,
                        Vec2::new(710.0, 900.0),
                    )),
                    events,
                    ..Default::default()
                },
                |ctx| {
                    show_settings(
                        ctx,
                        &mut cursor,
                        &mut theme,
                        &mut font,
                        &mut settings::ShellSettings::default(),
                    );
                },
            )
        };
        let text_center = |output: &egui::FullOutput, text: &str| {
            output
                .shapes
                .iter()
                .find_map(|shape| {
                    if let egui::Shape::Text(t) = &shape.shape {
                        (t.galley.text() == text)
                            .then(|| Rect::from_min_size(t.pos, t.galley.size()).center())
                    } else {
                        None
                    }
                })
                .unwrap_or_else(|| panic!("Missing text: {text}"))
        };
        let click = |pos| {
            vec![
                egui::Event::PointerMoved(pos),
                egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: Default::default(),
                },
                egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: Default::default(),
                },
            ]
        };
        render(vec![]);
        let output = render(vec![]);
        render(click(text_center(&output, "Lilex")));
        let output = render(vec![]);
        let search_center = text_center(&output, "Search fonts…");
        let visible_fonts = output
            .shapes
            .iter()
            .filter(|shape| {
                if let egui::Shape::Text(t) = &shape.shape {
                    t.galley.text().starts_with("Font ")
                        && shape.clip_rect.contains(t.pos + t.galley.size() / 2.0)
                } else {
                    false
                }
            })
            .count();
        assert!(
            visible_fonts >= 8,
            "font popup collapsed to {visible_fonts} rows"
        );
        let list_center = text_center(&output, "Test Mono");
        render(vec![
            egui::Event::PointerMoved(list_center),
            egui::Event::MouseWheel {
                unit: egui::MouseWheelUnit::Point,
                delta: Vec2::new(0.0, -1000.0),
                modifiers: Default::default(),
            },
        ]);
        for _ in 0..20 {
            render(vec![]);
        }
        let output = render(vec![]);
        assert_eq!(text_center(&output, "Search fonts…"), search_center);
        assert!(output.shapes.iter().any(|shape| {
            if let egui::Shape::Text(t) = &shape.shape {
                t.galley.text() == "Search fonts…" && shape.clip_rect.contains(search_center)
            } else {
                false
            }
        }));
        assert!(
            !output.shapes.iter().any(|shape| {
                if let egui::Shape::Text(t) = &shape.shape {
                    t.galley.text() == "Test Mono"
                        && shape.clip_rect.contains(t.pos + t.galley.size() / 2.0)
                } else {
                    false
                }
            }),
            "the font list actually scrolled"
        );
        render(click(text_center(&output, "Search fonts…")));
        render(vec![egui::Event::Text("Test".into())]);
        let output = render(vec![]);
        render(click(text_center(&output, "Test Mono")));
        assert_eq!(font.family, "Test Mono");
    }

    #[test]
    fn settings_dialog_scrolls_in_small_window_and_keeps_done_visible() {
        let ctx = egui::Context::default();
        let screen = Rect::from_min_size(egui::Pos2::ZERO, Vec2::new(420.0, 500.0));
        let mut cursor = CursorSettings::default();
        let mut theme = ThemeMode::Dark;
        let mut font = settings::FontSettings::default();
        for frame in 0..3 {
            let mut dialog = Rect::NOTHING;
            let output = ctx.run(
                egui::RawInput {
                    screen_rect: Some(screen),
                    ..Default::default()
                },
                |ctx| {
                    dialog = show_settings(
                        ctx,
                        &mut cursor,
                        &mut theme,
                        &mut font,
                        &mut settings::ShellSettings::default(),
                    )
                    .response
                    .rect;
                },
            );
            if frame == 0 {
                continue;
            }
            assert!(screen.contains_rect(dialog));
            let done = output
                .shapes
                .iter()
                .find_map(|shape| {
                    if let egui::Shape::Text(t) = &shape.shape {
                        (t.galley.text() == "Done")
                            .then(|| Rect::from_min_size(t.pos, t.galley.size()))
                    } else {
                        None
                    }
                })
                .expect("Done stays visible outside scroll area");
            assert!(dialog.contains_rect(done));
        }
    }

    #[test]
    fn settings_dialog_keeps_controls_inside_window() {
        for mut theme in [ThemeMode::Dark, ThemeMode::Light] {
            let ctx = egui::Context::default();
            ctx.set_visuals(theme.visuals());
            let mut settings = CursorSettings::default();
            for frame in 0..3 {
                let screen = Rect::from_min_size(egui::Pos2::ZERO, Vec2::new(710.0, 900.0));
                let mut dialog = Rect::NOTHING;
                let output = ctx.run(
                    egui::RawInput {
                        screen_rect: Some(screen),
                        ..Default::default()
                    },
                    |ctx| {
                        dialog = show_settings(
                            ctx,
                            &mut settings,
                            &mut theme,
                            &mut settings::FontSettings::default(),
                            &mut settings::ShellSettings::default(),
                        )
                        .response
                        .rect;
                    },
                );
                if frame == 0 {
                    continue;
                }
                assert!(screen.contains_rect(dialog));
                for text in [
                    "Settings",
                    "Theme",
                    "Dark",
                    "Light",
                    "Cursor Style",
                    "Cursor Blinking",
                    "Done",
                ] {
                    let bounds = output
                        .shapes
                        .iter()
                        .find_map(|shape| match &shape.shape {
                            egui::Shape::Text(shape) if shape.galley.text() == text => {
                                Some(Rect::from_min_size(shape.pos, shape.galley.size()))
                            }
                            _ => None,
                        })
                        .unwrap_or_else(|| panic!("Missing settings text: {text}"));
                    assert!(
                        dialog.contains_rect(bounds),
                        "{text}: {bounds:?} outside {dialog:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn about_dialog_keeps_content_inside_a_compact_window() {
        for size in [Vec2::new(710.0, 650.0), Vec2::new(1280.0, 900.0)] {
            for licenses in [false, true] {
                let ctx = egui::Context::default();
                let mut licenses = licenses;
                for frame in 0..3 {
                    let mut dialog = Rect::NOTHING;
                    let output = ctx.run(
                        egui::RawInput {
                            screen_rect: Some(Rect::from_min_size(egui::Pos2::ZERO, size)),
                            ..Default::default()
                        },
                        |ctx| {
                            dialog = show_about(
                                ctx,
                                &mut licenses,
                                &mut yiterm::update::Controller::default(),
                            )
                            .response
                            .rect;
                        },
                    );
                    assert!(dialog.width() < 420.0, "About width: {dialog:?}");
                    assert!(dialog.height() < 610.0, "About height: {dialog:?}");
                    if frame == 0 {
                        continue; // egui's first area pass only measures the dialog.
                    }
                    for text in [
                        "YiTerm",
                        concat!("Version  ", env!("CARGO_PKG_VERSION")),
                        "Docs",
                        "GitHub",
                        "Licenses",
                        "Close",
                    ] {
                        let shape = output
                            .shapes
                            .iter()
                            .find_map(|shape| match &shape.shape {
                                egui::Shape::Text(shape) if shape.galley.text() == text => {
                                    Some(shape)
                                }
                                _ => None,
                            })
                            .unwrap_or_else(|| panic!("Missing About text: {text}"));
                        let bounds = Rect::from_min_size(shape.pos, shape.galley.size());
                        assert!(
                            dialog.expand(1.0).contains_rect(bounds),
                            "{text} outside About: {bounds:?}, {dialog:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn selection_coordinates_include_scrollback_and_clamp_outside_drag() {
        let rect = Rect::from_min_size(egui::pos2(10.0, 20.0), Vec2::new(100.0, 60.0));
        let cell = Vec2::new(10.0, 20.0);
        let size = Size { cols: 10, rows: 3 };
        assert_eq!(
            selection_point(rect, cell, size, 5, egui::pos2(11.0, 21.0)),
            (Point::new(Line(-5), Column(0)), Side::Left)
        );
        assert_eq!(
            selection_point(rect, cell, size, 5, egui::pos2(500.0, 500.0)),
            (Point::new(Line(-3), Column(9)), Side::Right)
        );
        assert!(!is_copy_shortcut(Key::C, egui::Modifiers::CTRL));
        assert_eq!(
            key_bytes(Key::C, egui::Modifiers::CTRL, false),
            Some(vec![3])
        );
        assert!(is_copy_shortcut(Key::C, egui::Modifiers::MAC_CMD));
        assert!(is_copy_shortcut(
            Key::C,
            egui::Modifiers::CTRL | egui::Modifiers::SHIFT
        ));
    }

    #[cfg(unix)]
    #[test]
    fn osc52_copy_request_reaches_the_host_clipboard() {
        let args = Args {
            ssh: None,
            config: None,
            log_filter: None,
            shell: Some("/bin/sh".into()),
        };
        let mut session = TerminalSession::new(&args, None).unwrap();
        session
            .parser
            .advance(&mut session.term, b"\x1b]52;c;SGVsbG8=\x07");
        assert!(session.poll());

        let mut copied = None;
        session.flush_clipboard_with(|text| {
            copied = Some(text);
            Ok(())
        });
        assert_eq!(copied.as_deref(), Some("Hello"));
        assert_eq!(session.status, "Copied 5 chars to host clipboard");

        session.pending_clipboard = Some("again".into());
        session.flush_clipboard_with(|_| Err(anyhow::anyhow!("clipboard unavailable")));
        assert_eq!(
            session.status,
            "Host clipboard write failed: clipboard unavailable"
        );
    }

    #[cfg(unix)]
    #[test]
    fn windows_right_click_copies_selection_instead_of_opening_menu() {
        let args = Args {
            ssh: None,
            config: None,
            log_filter: None,
            shell: Some("/bin/sh".into()),
        };
        let mut session = TerminalSession::new(&args, None).unwrap();
        session.parser.advance(&mut session.term, b"hello");
        for (windows, selected) in [(true, true), (true, false), (false, true)] {
            session.term.selection = if selected {
                let mut selection = Selection::new(
                    SelectionType::Simple,
                    Point::new(Line(0), Column(0)),
                    Side::Left,
                );
                selection.update(Point::new(Line(0), Column(4)), Side::Right);
                Some(selection)
            } else {
                None
            };
            let ctx = egui::Context::default();
            let mut copies = 0;
            let pos = egui::pos2(40.0, 40.0);
            for pressed in [None, Some(true), Some(false)] {
                let mut events = vec![egui::Event::PointerMoved(pos)];
                if let Some(pressed) = pressed {
                    events.push(egui::Event::PointerButton {
                        pos,
                        button: egui::PointerButton::Secondary,
                        pressed,
                        modifiers: egui::Modifiers::NONE,
                    });
                }
                let output = ctx.run(
                    egui::RawInput {
                        events,
                        ..Default::default()
                    },
                    |ctx| {
                        egui::CentralPanel::default().show(ctx, |ui| {
                            let response = ui.interact(
                                Rect::from_min_size(egui::Pos2::ZERO, Vec2::splat(100.0)),
                                ui.id().with("pane"),
                                egui::Sense::click(),
                            );
                            if !session.copy_on_right_click(&response, windows) {
                                response.context_menu(|ui| {
                                    ui.label("Split Right");
                                });
                            }
                        });
                    },
                );
                copies += output.platform_output.commands.iter().filter(|command| matches!(command, egui::OutputCommand::CopyText(text) if text == "hello")).count();
            }
            let copies_selection = windows && selected;
            assert_eq!(copies, usize::from(copies_selection));
            assert_eq!(egui::Popup::is_any_open(&ctx), !copies_selection);
            assert_eq!(
                session.term.selection.is_some(),
                selected && !copies_selection
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn right_click_reaches_mouse_reporting_apps() {
        let args = Args {
            ssh: None,
            config: None,
            log_filter: None,
            shell: Some("/bin/sh".into()),
        };
        for (modes, expected) in [
            (
                b"\x1b[?1000h\x1b[?1006h".as_slice(),
                b"\x1b[<2;1;1M\x1b[<2;1;1m".as_slice(),
            ),
            (
                b"\x1b[?1000h".as_slice(),
                b"\x1b[M\x22!!\x1b[M\x23!!".as_slice(),
            ),
            (
                b"\x1b[?1000h\x1b[?1015h".as_slice(),
                b"\x1b[34;1;1M\x1b[35;1;1M".as_slice(),
            ),
            (b"".as_slice(), b"".as_slice()),
        ] {
            let mut session = TerminalSession::new(&args, None).unwrap();
            let output = tempfile::NamedTempFile::new().unwrap();
            session.writer = Box::new(output.reopen().unwrap());
            session.parser.advance(&mut session.term, b"hello");
            session.parser.advance(&mut session.term, modes);
            session.mouse_modes.observe(modes);
            let ctx = egui::Context::default();
            let pos = egui::pos2(10.0, 10.0);
            for pressed in [None, Some(true), Some(false)] {
                let mut events = vec![egui::Event::PointerMoved(pos)];
                if let Some(pressed) = pressed {
                    events.push(egui::Event::PointerButton {
                        pos,
                        button: egui::PointerButton::Secondary,
                        pressed,
                        modifiers: egui::Modifiers::NONE,
                    });
                }
                let _ = ctx.run(
                    egui::RawInput {
                        events,
                        ..Default::default()
                    },
                    |ctx| {
                        egui::CentralPanel::default().show(ctx, |ui| {
                            let response = ui.interact(
                                ui.available_rect_before_wrap(),
                                ui.id().with("copy"),
                                egui::Sense::click(),
                            );
                            if !modes.is_empty() {
                                let mut selection = Selection::new(
                                    SelectionType::Simple,
                                    Point::new(Line(0), Column(0)),
                                    Side::Left,
                                );
                                selection.update(Point::new(Line(0), Column(4)), Side::Right);
                                session.term.selection = Some(selection);
                                assert_eq!(
                                    session.term.selection_to_string().as_deref(),
                                    Some("hello")
                                );
                                assert!(!session.copy_on_right_click(&response, true));
                            }
                            session.show(ctx, ui, true);
                        });
                    },
                );
            }
            assert_eq!(std::fs::read(output.path()).unwrap(), expected);
            assert!(session.mouse_button.is_none());
        }
    }

    #[cfg(unix)]
    #[test]
    fn dragging_terminal_text_highlights_and_copies_without_mouse_reports() {
        let args = Args {
            ssh: None,
            config: None,
            log_filter: None,
            shell: Some("/bin/sh".into()),
        };
        let mut session = TerminalSession::new(&args, None).unwrap();
        session.parser.advance(&mut session.term, b"hello world");
        let ctx = egui::Context::default();
        configure_fonts(&ctx);
        let mut origin = egui::Pos2::ZERO;
        let mut cell = Vec2::ZERO;
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                origin = ui.available_rect_before_wrap().min;
                cell = Vec2::new(
                    ui.fonts_mut(|fonts| {
                        fonts.glyph_width(&FontId::monospace(TERMINAL_FONT_SIZE), 'M')
                    }),
                    TERMINAL_FONT_SIZE * TERMINAL_LINE_HEIGHT,
                );
                session.show(ctx, ui, true);
            });
        });
        let start = origin + Vec2::new(cell.x * 0.1, cell.y * 0.5);
        let end = origin + Vec2::new(cell.x * 4.9, cell.y * 0.5);
        for events in [
            vec![egui::Event::PointerButton {
                pos: start,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            }],
            vec![
                egui::Event::PointerMoved(end),
                egui::Event::PointerButton {
                    pos: end,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        ] {
            let _ = ctx.run(
                egui::RawInput {
                    events,
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| session.show(ctx, ui, true));
                },
            );
        }
        assert!(!session.selecting);
        assert_eq!(session.term.selection_to_string().as_deref(), Some("hello"));
        let selected = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| session.show(ctx, ui, true));
        });
        assert!(selected.shapes.iter().any(|shape| matches!(&shape.shape, egui::Shape::Rect(rect) if rect.fill == Color32::from_rgb(65, 90, 125))));
        let output = ctx.run(
            egui::RawInput {
                events: vec![egui::Event::Copy],
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| session.show(ctx, ui, true));
            },
        );
        assert!(output.platform_output.commands.iter().any(
            |command| matches!(command, egui::OutputCommand::CopyText(text) if text == "hello")
        ));
        assert!(session.term.selection.is_none());
        let cleared = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| session.show(ctx, ui, true));
        });
        assert!(!cleared.shapes.iter().any(|shape| matches!(&shape.shape, egui::Shape::Rect(rect) if rect.fill == Color32::from_rgb(65, 90, 125))));
        // Copy clears the highlight in the same frame now that input is handled first.
        assert!(!output.shapes.iter().any(|shape| matches!(&shape.shape, egui::Shape::Rect(rect) if rect.fill == Color32::from_rgb(65, 90, 125))));
        session.writer = Box::new(Vec::<u8>::new());
        session.parser.advance(&mut session.term, b"\x1b[?1000h");
        for modifiers in [egui::Modifiers::NONE, egui::Modifiers::SHIFT] {
            session.term.selection = None;
            for events in [
                vec![egui::Event::PointerButton {
                    pos: start,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers,
                }],
                vec![
                    egui::Event::PointerMoved(end),
                    egui::Event::PointerButton {
                        pos: end,
                        button: egui::PointerButton::Primary,
                        pressed: false,
                        modifiers,
                    },
                ],
            ] {
                let _ = ctx.run(
                    egui::RawInput {
                        events,
                        modifiers,
                        ..Default::default()
                    },
                    |ctx| {
                        egui::CentralPanel::default().show(ctx, |ui| session.show(ctx, ui, true));
                    },
                );
            }
            if modifiers.shift {
                assert_eq!(session.term.selection_to_string().as_deref(), Some("hello"));
            } else {
                assert!(
                    session.term.selection.is_none(),
                    "Application mouse mode must receive ordinary drags"
                );
            }
        }
    }

    #[test]
    fn terminal_background_cells_share_pixel_edges() {
        use egui::emath::GuiRounding;

        let cell = Vec2::new(9.6, TERMINAL_FONT_SIZE * TERMINAL_LINE_HEIGHT);
        for scale in [1.0, 1.25, 1.5, 2.0] {
            for origin in [egui::pos2(2.0, 26.5), egui::pos2(7.25, 63.0)] {
                for row in 0..100 {
                    for column in 0..100 {
                        let rect =
                            terminal_cell_rect(origin, cell, column, row).round_to_pixels(scale);
                        let below = terminal_cell_rect(origin, cell, column, row + 1)
                            .round_to_pixels(scale);
                        let right = terminal_cell_rect(origin, cell, column + 1, row)
                            .round_to_pixels(scale);
                        assert_eq!(rect.max.y, below.min.y, "background gap at row {row}");
                        assert_eq!(rect.max.x, right.min.x, "background gap at column {column}");
                    }
                }
            }
        }
    }

    #[test]
    fn terminal_unicode_display_keeps_visible_marks() {
        let invisible = [
            '\u{00AD}',
            '\u{034F}',
            '\u{061C}',
            '\u{115F}',
            '\u{1160}',
            '\u{17B4}',
            '\u{17B5}',
            '\u{180B}',
            '\u{180E}',
            '\u{180F}',
            '\u{200B}',
            '\u{200C}',
            '\u{200D}',
            '\u{200E}',
            '\u{200F}',
            '\u{202A}',
            '\u{202E}',
            '\u{2060}',
            '\u{2065}',
            '\u{2069}',
            '\u{206F}',
            '\u{3164}',
            '\u{FE00}',
            '\u{FE0E}',
            '\u{FE0F}',
            '\u{FEFF}',
            '\u{FFA0}',
            '\u{FFF0}',
            '\u{FFF8}',
            '\u{1BCA0}',
            '\u{1BCA3}',
            '\u{1D173}',
            '\u{1D17A}',
            '\u{E0001}',
            '\u{E0067}',
            '\u{E007F}',
            '\u{E0100}',
            '\u{E01EF}',
            '\u{E0FFF}',
        ];
        for ch in invisible {
            let mut cell = alacritty_terminal::term::cell::Cell::default();
            cell.c = '⚠';
            cell.push_zerowidth(ch);
            assert_eq!(terminal_cell_text(&cell), "⚠", "{ch:?}");
            assert_eq!(cell.zerowidth().unwrap(), &[ch]);
            cell.c = ch;
            assert!(terminal_cell_text(&cell).is_empty(), "{ch:?}");
        }
        for (base, mark) in [
            ('e', '\u{0301}'),
            ('a', '\u{0308}'),
            ('1', '\u{20E3}'),
            ('क', '\u{093F}'),
        ] {
            let mut cell = alacritty_terminal::term::cell::Cell::default();
            cell.c = base;
            cell.push_zerowidth(mark);
            assert_eq!(terminal_cell_text(&cell), format!("{base}{mark}"));
        }
        for ch in [
            ' ', '\u{00A0}', '\u{2007}', '\u{202F}', '中', '─', '⚠', '❤', '😀', '\u{FFFD}',
            '\u{E000}',
        ] {
            assert!(!is_default_ignorable(ch), "{ch:?}");
        }
    }

    #[test]
    fn terminal_unicode_keeps_grid_positions_and_original_selection() {
        for (input, display, columns) in [
            ("⚠", "⚠", 1),
            ("⚠\u{FE0F}", "⚠", 2),
            ("❤\u{FE0F}", "❤", 2),
            ("❤\u{FE0E}", "❤", 1),
            ("✓\u{FE0F}", "✓", 1),
            ("中\u{E0100}", "中", 2),
            ("ᠠ\u{180B}", "ᠠ", 1),
            ("a\u{034F}\u{0301}", "a\u{0301}", 1),
            ("a\u{200D}\u{2060}\u{FEFF}", "a", 1),
            ("🏴\u{E0067}\u{E0062}\u{E007F}", "🏴", 2),
            ("1\u{FE0F}\u{20E3}", "1\u{20E3}", 2),
        ] {
            let (tx, _rx) = mpsc::channel();
            let mut term = Term::new(Config::default(), &Size { cols: 20, rows: 2 }, Listener(tx));
            let mut parser: Processor = Processor::new();
            parser.advance(&mut term, format!("{input}X").as_bytes());
            let first = &term.grid()[Point::new(Line(0), Column(0))];
            assert_eq!(terminal_cell_text(first), display, "{input:?}");
            assert_eq!(first.flags.contains(Flags::WIDE_CHAR), columns == 2);
            assert_eq!(term.grid()[Point::new(Line(0), Column(columns))].c, 'X');
            assert_eq!(term.grid().cursor.point.column.0, columns + 1);
            term.selection = Some(Selection::new(
                SelectionType::Lines,
                Point::new(Line(0), Column(0)),
                Side::Left,
            ));
            assert_eq!(
                term.selection_to_string().unwrap().trim_end(),
                format!("{input}X")
            );
        }
    }

    #[test]
    fn terminal_unicode_pi_footer_survives_streaming_and_cursor_updates() {
        let prefix = "· Mimo cache 43/44·2.06M/2.25M 91.5% ";
        let footer = format!("{prefix}⚠️ compat");
        for split in 0..=footer.len() {
            let (tx, _rx) = mpsc::channel();
            let mut term = Term::new(Config::default(), &Size { cols: 80, rows: 3 }, Listener(tx));
            let mut parser: Processor = Processor::new();
            parser.advance(&mut term, &footer.as_bytes()[..split]);
            parser.advance(&mut term, &footer.as_bytes()[split..]);
            let warning_column = prefix.chars().count();
            let warning = &term.grid()[Point::new(Line(0), Column(warning_column))];
            assert_eq!(warning.c, '⚠');
            assert!(warning.flags.contains(Flags::WIDE_CHAR), "split {split}");
            assert!(
                term.grid()[Point::new(Line(0), Column(warning_column + 1))]
                    .flags
                    .contains(Flags::WIDE_CHAR_SPACER)
            );
            assert_eq!(term.grid().cursor.point.column.0, warning_column + 9);
            // Pi's next absolute cursor update must land after the same two
            // columns the terminal consumed, without leaving stale text.
            parser.advance(
                &mut term,
                format!("\x1b[1;{}Hready\x1b[K", warning_column + 4).as_bytes(),
            );
            for (i, ch) in "ready".chars().enumerate() {
                assert_eq!(
                    term.grid()[Point::new(Line(0), Column(warning_column + 3 + i))].c,
                    ch
                );
            }
            term.selection = Some(Selection::new(
                SelectionType::Lines,
                Point::new(Line(0), Column(0)),
                Side::Left,
            ));
            assert_eq!(
                term.selection_to_string().unwrap().trim_end(),
                format!("{prefix}⚠️ ready")
            );
        }
    }

    #[test]
    fn terminal_unicode_emoji_handles_row_edges_scrolling_insert_and_overwrite() {
        for (input, row, col) in [
            ("abcd⚠️X", 0, 4),
            ("abcde⚠️X", 1, 0),
            ("\r\nabcde⚠️X", 1, 0),
            ("abcdef\x1b[1;2H\x1b[4h⚠️", 0, 1),
            ("ab中XY\x1b[1;2H⚠️", 0, 1),
            ("⚠️\u{FE0F}", 0, 0),
        ] {
            let (tx, _rx) = mpsc::channel();
            let mut term = Term::new(Config::default(), &Size { cols: 6, rows: 2 }, Listener(tx));
            let mut parser: Processor = Processor::new();
            // Split every UTF-8 byte to exercise decoding and late selectors.
            for byte in input.as_bytes() {
                parser.advance(&mut term, std::slice::from_ref(byte));
            }
            let base = &term.grid()[Point::new(Line(row), Column(col))];
            assert_eq!(base.c, '⚠', "{input:?}");
            assert!(base.flags.contains(Flags::WIDE_CHAR), "{input:?}");
            let spacer = &term.grid()[Point::new(Line(row), Column(col + 1))];
            assert_eq!(spacer.c, ' ');
            assert!(spacer.flags.contains(Flags::WIDE_CHAR_SPACER));
            assert!(spacer.zerowidth().unwrap_or_default().is_empty());
            if input.contains("abcde⚠") {
                let placeholder = &term.grid()[Point::new(Line(0), Column(5))];
                assert_eq!(placeholder.c, ' ');
                assert!(
                    placeholder
                        .flags
                        .contains(Flags::LEADING_WIDE_CHAR_SPACER | Flags::WRAPLINE)
                );
            }
            if input.contains("[4h") {
                assert_eq!(term.grid()[Point::new(Line(0), Column(3))].c, 'b');
            }
            if input.contains('中') {
                assert!(
                    !term.grid()[Point::new(Line(0), Column(3))]
                        .flags
                        .contains(Flags::WIDE_CHAR_SPACER)
                );
            }
        }
        let (tx, _rx) = mpsc::channel();
        let mut term = Term::new(Config::default(), &Size { cols: 6, rows: 2 }, Listener(tx));
        let mut parser: Processor = Processor::new();
        parser.advance(&mut term, "\x1b[?7labcde⚠️".as_bytes());
        assert_eq!(term.grid().cursor.point, Point::new(Line(0), Column(5)));
        assert!(
            !term.grid()[Point::new(Line(0), Column(5))]
                .flags
                .contains(Flags::WIDE_CHAR)
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires Linux CI CJK fonts"]
    fn terminal_unicode_linux_system_cjk_fallback() {
        let ctx = egui::Context::default();
        configure_fonts(&ctx);
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            ctx.fonts_mut(|fonts| {
                for family in [egui::FontFamily::Monospace, egui::FontFamily::Proportional] {
                    let font = FontId::new(TERMINAL_FONT_SIZE, family);
                    assert!(fonts.has_glyphs(&font, "中文截图剪贴板你好世界"));
                }
            });
        });
    }

    #[test]
    fn terminal_unicode_format_characters_do_not_add_rendered_width() {
        for scale in [1.0, 1.25, 1.5, 2.0] {
            let ctx = egui::Context::default();
            configure_fonts(&ctx);
            ctx.set_pixels_per_point(scale);
            let _ = ctx.run(egui::RawInput::default(), |ctx| {
                ctx.fonts_mut(|fonts| {
                    for base in ['⚠', '❤', '✓', '中', 'ᠠ', '🏴'] {
                        let mut cell = alacritty_terminal::term::cell::Cell::default();
                        cell.c = base;
                        cell.push_zerowidth('\u{FE0F}');
                        cell.push_zerowidth('\u{034F}');
                        cell.push_zerowidth('\u{E007F}');
                        let font = FontId::monospace(TERMINAL_FONT_SIZE);
                        let expected =
                            fonts.layout_no_wrap(base.to_string(), font.clone(), Color32::WHITE);
                        let actual =
                            fonts.layout_no_wrap(terminal_cell_text(&cell), font, Color32::WHITE);
                        assert_eq!(actual.size(), expected.size(), "{base} at {scale}");
                    }
                });
            });
        }
    }

    #[test]
    fn terminal_unicode_fallback_symbols_fit_columns_without_changing_height_or_cache() {
        for scale in [1.0, 1.25, 1.5, 2.0] {
            let ctx = egui::Context::default();
            configure_fonts(&ctx);
            ctx.set_pixels_per_point(scale);
            let _ = ctx.run(egui::RawInput::default(), |ctx| {
                ctx.fonts_mut(|fonts| {
                    let font = FontId::monospace(TERMINAL_FONT_SIZE);
                    let column = fonts.glyph_width(&font, 'M');
                    for (text, columns) in [
                        ("⚠", 1.0),
                        ("❤", 1.0),
                        ("☀", 1.0),
                        ("中", 2.0),
                        ("😀", 2.0),
                        ("M", 1.0),
                        ("─", 1.0),
                    ] {
                        let original =
                            fonts.layout_no_wrap(text.into(), font.clone(), Color32::WHITE);
                        let original_bounds = original.mesh_bounds;
                        let original_size = original.size();
                        let width = column * columns;
                        let fitted = fit_terminal_galley(original.clone(), width);
                        assert!(fitted.size().x <= width + 0.1, "{text} at {scale}");
                        assert_eq!(fitted.size().y, original_size.y);
                        assert_eq!(fitted.mesh_bounds.height(), original_bounds.height());
                        assert_eq!(original.mesh_bounds, original_bounds);
                        assert_eq!(original.size(), original_size);
                        if original_size.x > width + 0.1 {
                            assert!(fitted.mesh_bounds.left() >= -0.01);
                            assert!(fitted.mesh_bounds.right() <= width + 0.01);
                            for row in &fitted.rows {
                                for vertex in &row.visuals.mesh.vertices {
                                    assert!(vertex.pos.x >= -0.01 && vertex.pos.x <= width + 0.01);
                                }
                            }
                        } else {
                            assert!(Arc::ptr_eq(&original, &fitted));
                        }
                    }
                });
            });
        }
    }

    #[test]
    fn embedded_terminal_font_keeps_box_drawing_on_the_cell_grid() {
        for scale in [1.0, 1.25, 1.5, 2.0] {
            let ctx = egui::Context::default();
            configure_fonts(&ctx);
            ctx.set_pixels_per_point(scale);
            let _ = ctx.run(egui::RawInput::default(), |ctx| {
                ctx.fonts_mut(|fonts| {
                    let font = FontId::monospace(TERMINAL_FONT_SIZE);
                    let width = fonts.glyph_width(&font, 'M');
                    // Lilex has a 600/1000 em advance. Check the resolved font,
                    // not just registration: falling back to Hack changes this.
                    assert!((width - 9.6).abs() < 0.01);
                    for ch in "─│┌┐└┘├┤┬┴┼".chars() {
                        assert!((fonts.glyph_width(&font, ch) - width).abs() < 0.01);
                    }
                    let height = fonts.row_height(&font);
                    assert!(
                        (height - TERMINAL_FONT_SIZE * TERMINAL_LINE_HEIGHT).abs() < 0.1,
                        "Lilex line box {height} does not match terminal rows at scale {scale}"
                    );
                });
            });
        }
    }

    #[test]
    fn popup_clicks_do_not_activate_the_pane_underneath() {
        let ctx = egui::Context::default();
        let pos = egui::pos2(100.0, 100.0);
        for pressed in [false, true] {
            let mut events = vec![egui::Event::PointerMoved(pos)];
            if pressed {
                events.push(egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                });
            }
            let _ = ctx.run(
                egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(
                        egui::Pos2::ZERO,
                        Vec2::new(800.0, 600.0),
                    )),
                    events,
                    ..Default::default()
                },
                |ctx| {
                    egui::Area::new(egui::Id::new("pane-menu"))
                        .order(egui::Order::Foreground)
                        .fixed_pos(egui::pos2(50.0, 50.0))
                        .show(ctx, |ui| {
                            ui.allocate_space(Vec2::splat(200.0));
                        });
                    egui::CentralPanel::default().show(ctx, |ui| {
                        let rect = ui.available_rect_before_wrap();
                        assert!(rect.contains(pos));
                        if pressed {
                            assert!(ctx.input(|input| input.pointer.primary_pressed()));
                            assert!(
                                !ui.rect_contains_pointer(rect),
                                "Menu click reached underlying pane"
                            );
                        }
                    });
                },
            );
        }
    }

    #[test]
    fn nested_splits_have_disjoint_rectangles_and_collapse_after_close() {
        let mut layout = PaneLayout::Leaf(0);
        layout.split(0, 1, SplitDirection::Right);
        layout.split(1, 2, SplitDirection::Down);
        let ctx = egui::Context::default();
        let mut rectangles = Vec::new();
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                layout.rectangles(
                    ui,
                    Rect::from_min_size(egui::Pos2::ZERO, Vec2::new(805.0, 605.0)),
                    "test",
                    &mut rectangles,
                );
            });
        });
        assert_eq!(rectangles.len(), 3);
        assert_eq!(rectangles[0].1.width(), 400.0);
        assert_eq!(rectangles[1].1.height(), 300.0);
        assert_eq!(rectangles[2].1.height(), 300.0);
        assert!(rectangles[0].1.right() < rectangles[1].1.left());
        assert!(rectangles[1].1.bottom() < rectangles[2].1.top());
        let layout = layout.remove(1).unwrap();
        assert_eq!(
            layout,
            PaneLayout::Split {
                direction: SplitDirection::Right,
                ratio: 0.5,
                first: Box::new(PaneLayout::Leaf(0)),
                second: Box::new(PaneLayout::Leaf(1)),
            }
        );
        assert_eq!(layout.remove(0), Some(PaneLayout::Leaf(0)));
        assert_eq!(PaneLayout::Leaf(0).remove(0), None);
    }

    #[cfg(unix)]
    #[test]
    fn split_panes_route_keyboard_only_to_clicked_pane_and_preserve_survivors() {
        let args = Args {
            ssh: None,
            config: None,
            log_filter: None,
            shell: Some("/bin/sh".into()),
        };
        let mut tab = TerminalTab::new(TerminalSession::new(&args, None).unwrap());
        tab.split(
            TerminalSession::new(&args, None).unwrap(),
            SplitDirection::Right,
        );
        tab.split(
            TerminalSession::new(&args, None).unwrap(),
            SplitDirection::Down,
        );
        assert_eq!(tab.active, 2);
        let ctx = egui::Context::default();
        let pos = egui::pos2(100.0, 100.0);
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(
                egui::Pos2::ZERO,
                Vec2::new(805.0, 605.0),
            )),
            events: vec![
                egui::Event::PointerMoved(pos),
                egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
                egui::Event::Text("printf '\\033]0;focused-pane\\007'".into()),
                egui::Event::Key {
                    key: Key::Enter,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
            ..Default::default()
        };
        let _ = ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| tab.show(ctx, ui));
        });
        assert_eq!(tab.active, 0);
        assert!(tab.panes[0].size.cols < 80);
        assert!(tab.panes[1].size.rows < tab.panes[0].size.rows);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            assert!(tab.poll());
            if tab.panes[0].title == "focused-pane" {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "Focused pane did not receive keyboard input"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_ne!(tab.panes[1].title, "focused-pane");
        assert_ne!(tab.panes[2].title, "focused-pane");
        // Exiting one shell removes only its pane and collapses its parent split.
        tab.panes[1].write(b"exit\n");
        loop {
            assert!(tab.poll());
            if tab.panes.len() == 2 {
                break;
            }
            assert!(Instant::now() < deadline, "Exited pane was not removed");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(tab.active, 0);
        assert_eq!(tab.panes[0].title, "focused-pane");
        tab.close_pane(0);
        assert_eq!(tab.layout, PaneLayout::Leaf(0));
        assert!(tab.focused_mut().child.try_wait().unwrap().is_none());
    }

    #[test]
    fn closing_tabs_preserves_selection_or_selects_a_neighbor() {
        assert_eq!(active_after_close(2, 0, 3), 1);
        assert_eq!(active_after_close(1, 3, 3), 1);
        assert_eq!(active_after_close(1, 1, 3), 1);
        assert_eq!(active_after_close(3, 3, 3), 2);
        assert_eq!(active_after_close(0, 0, 0), 0);
    }

    #[test]
    fn tab_shortcuts_are_consumed_without_reversing_previous_tab() {
        for (modifiers, expected) in [
            (egui::Modifiers::CTRL, (false, false, true, false)),
            (
                egui::Modifiers::CTRL | egui::Modifiers::SHIFT,
                (false, false, false, true),
            ),
            (egui::Modifiers::NONE, (false, false, false, false)),
        ] {
            let mut input = egui::InputState::default();
            input.events.push(egui::Event::Key {
                key: Key::Tab,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers,
            });
            assert_eq!(consume_tab_shortcuts(&mut input), expected);
            assert_eq!(
                input.events.len(),
                usize::from(modifiers == egui::Modifiers::NONE)
            );
        }
    }

    #[test]
    fn terminal_keys_do_not_move_focus_to_menu_buttons() {
        let ctx = egui::Context::default();
        for (key, modifiers, bytes) in [
            (Key::Tab, egui::Modifiers::NONE, "\t"),
            (Key::Tab, egui::Modifiers::SHIFT, "\x1b[Z"),
            (Key::ArrowDown, egui::Modifiers::NONE, "\x1b[B"),
        ] {
            let _ = ctx.run(
                egui::RawInput {
                    events: vec![egui::Event::Key {
                        key,
                        physical_key: None,
                        pressed: true,
                        repeat: false,
                        modifiers,
                    }],
                    ..Default::default()
                },
                |ctx| {
                    reserve_terminal_keyboard(ctx);
                    egui::TopBottomPanel::top("menu").show(ctx, |ui| {
                        ui.menu_button("Pane", |_| {});
                        let _ = ui.button("+");
                    });
                    assert_eq!(ctx.memory(|memory| memory.focused()), None);
                    assert!(ctx.input(|input| input.key_pressed(key)));
                    assert_eq!(
                        key_bytes(key, modifiers, false),
                        Some(bytes.as_bytes().to_vec())
                    );
                },
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn background_sessions_process_output_and_survive_another_tab_closing() {
        let args = Args {
            ssh: None,
            config: None,
            log_filter: None,
            shell: Some("/bin/sh".into()),
        };
        let first = TerminalSession::new(&args, None).unwrap();
        let second = TerminalSession::new(&args, None).unwrap();
        let mut app = TerminalApp {
            profile: None,
            window_title: String::new(),
            tabs: vec![TerminalTab::new(first), TerminalTab::new(second)],
            active: 0,
            args,
            log_directory: std::env::temp_dir(),
            warning: None,
            error: None,
            about_open: false,
            about_licenses: false,
            updater: yiterm::update::Controller::default(),
            settings_open: false,
            theme: ThemeMode::Dark,
            cursor_settings: CursorSettings::default(),
            font_settings: settings::FontSettings::default(),
            browser_settings: settings::BrowserSettings::default(),
            shell_settings: settings::ShellSettings::default(),
        };
        // Set different titles through each real PTY; the background session
        // must parse output too, without changing the foreground session title.
        app.tabs[0]
            .focused_mut()
            .write(b"printf '\\033]0;first-session\\007'\n");
        app.tabs[1]
            .focused_mut()
            .write(b"printf '\\033]0;background-session\\007'\n");
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            assert!(app.tabs[0].poll());
            assert!(app.tabs[1].poll());
            if app.tabs[0].focused().title == "first-session"
                && app.tabs[1].focused().title == "background-session"
            {
                break;
            }
            assert!(Instant::now() < deadline, "PTY titles were not processed");
            std::thread::sleep(Duration::from_millis(10));
        }
        app.close_tab(0);
        assert_eq!(app.active, 0);
        assert_eq!(app.tabs[0].focused().title, "background-session");
        assert!(
            app.tabs[0]
                .focused_mut()
                .child
                .try_wait()
                .unwrap()
                .is_none()
        );
        app.new_tab();
        assert_eq!(app.tabs.len(), 2);
        assert_eq!(app.active, 1);
        app.args.shell = Some("/yiterm-nonexistent-shell".into());
        app.new_tab();
        assert_eq!(app.tabs.len(), 2);
        assert_eq!(app.active, 1);
        assert!(app.error.is_some());
    }

    #[test]
    fn mouse_reports_use_one_based_cells_and_preserve_release_button() {
        let rect = Rect::from_min_size(egui::pos2(10.0, 20.0), Vec2::new(800.0, 500.0));
        let cell = Vec2::new(10.0, 20.0);
        let size = Size { cols: 80, rows: 25 };
        let encode = |mode: TermMode, pos, button, pressed| {
            mouse_bytes(
                mode.into(),
                rect,
                cell,
                size,
                pos,
                (button, pressed),
                egui::Modifiers::NONE,
            )
        };
        assert_eq!(
            encode(TermMode::SGR_MOUSE, egui::pos2(25.0, 20.0), 0, true),
            Some(b"\x1b[<0;2;1M".to_vec())
        );
        assert_eq!(
            encode(TermMode::SGR_MOUSE, egui::pos2(25.0, 20.0), 0, false),
            Some(b"\x1b[<0;2;1m".to_vec())
        );
        assert_eq!(
            encode(TermMode::empty(), rect.min, 0, true),
            Some(vec![27, 91, 77, 32, 33, 33])
        );
        assert_eq!(
            encode(TermMode::empty(), rect.min, 0, false),
            Some(vec![27, 91, 77, 35, 33, 33])
        );
        assert_eq!(
            encode(TermMode::SGR_MOUSE, rect.max, 64, true),
            Some(b"\x1b[<64;80;25M".to_vec())
        );
    }
    #[test]
    fn urxvt_reports_preserve_large_coordinates_and_event_types() {
        let protocol = mouse::Protocol {
            mode: TermMode::MOUSE_DRAG,
            urxvt: true,
        };
        let rect = Rect::from_min_size(egui::Pos2::ZERO, Vec2::new(4000.0, 500.0));
        let encode = |button, pressed| {
            mouse_bytes(
                protocol,
                rect,
                Vec2::new(10.0, 20.0),
                Size {
                    cols: 400,
                    rows: 25,
                },
                egui::pos2(2995.0, 10.0),
                (button, pressed),
                egui::Modifiers::NONE,
            )
            .unwrap()
        };
        assert_eq!(encode(0, true), b"\x1b[32;300;1M");
        assert_eq!(encode(0, false), b"\x1b[35;300;1M");
        assert_eq!(encode(32, true), b"\x1b[64;300;1M");
        assert_eq!(encode(64, true), b"\x1b[96;300;1M");
        assert_eq!(encode(65, true), b"\x1b[97;300;1M");
    }
    #[test]
    fn ime_reserves_composition_and_commit_keys() {
        assert!(!ime_owns_keyboard("", &[egui::Event::Text("a".into())]));
        assert!(ime_owns_keyboard("ni", &[]));
        assert!(ime_owns_keyboard(
            "",
            &[egui::Event::Ime(egui::ImeEvent::Preedit("ni".into()))]
        ));
        assert!(ime_owns_keyboard(
            "",
            &[
                egui::Event::Ime(egui::ImeEvent::Preedit(String::new())),
                egui::Event::Ime(egui::ImeEvent::Commit("你".into())),
            ]
        ));
        // Windows may leave IME enabled between compositions: ordinary keys
        // still belong to the shell when no preedit or commit is present.
        assert!(!ime_owns_keyboard(
            "",
            &[egui::Event::Ime(egui::ImeEvent::Enabled)]
        ));
    }
    #[test]
    fn terminal_links_exclude_following_prose() {
        let url = "https://platform.worldquantbrain.com/alpha/xA1MlEam";
        for suffix in [
            ") (TO 14.73%",
            ")(TO 14.73%",
            ")（TO 14.73%",
            "）作为备选",
            "].",
        ] {
            let (tx, _rx) = mpsc::channel();
            let mut term = Term::new(Config::default(), &Size { cols: 20, rows: 8 }, Listener(tx));
            let mut parser: Processor = Processor::new();
            parser.advance(&mut term, format!("({url}{suffix}").as_bytes());
            let link = terminal_link_details(&term, Point::new(Line(0), Column(5))).unwrap();
            assert_eq!(link.url, url, "suffix: {suffix}");
            assert_eq!(link.points.len(), url.len());
            let end = url.len() + 1;
            assert!(
                terminal_link(&term, Point::new(Line((end / 20) as i32), Column(end % 20)))
                    .is_none()
            );
        }
        assert_eq!(
            link_text("https://example.com/a(b)?x=[1] rest"),
            "https://example.com/a(b)?x=[1]"
        );
        assert_eq!(
            link_text("https://example.com/a(b))."),
            "https://example.com/a(b)"
        );
    }

    #[test]
    fn terminal_links_handle_wrap_punctuation_and_osc8() {
        let (tx, _rx) = mpsc::channel();
        let mut term = Term::new(Config::default(), &Size { cols: 20, rows: 6 }, Listener(tx));
        let mut parser: Processor = Processor::new();
        parser.advance(&mut term, b"(https://example.com/a(b)).");
        assert_eq!(
            terminal_link(&term, Point::new(Line(1), Column(1))).as_deref(),
            Some("https://example.com/a(b)")
        );
        let link = terminal_link_details(&term, Point::new(Line(1), Column(1))).unwrap();
        assert_eq!(link.points.first(), Some(&Point::new(Line(0), Column(1))));
        assert_eq!(link.points.last(), Some(&Point::new(Line(1), Column(4))));
        assert_eq!(link.points.len(), "https://example.com/a(b)".len());
        assert!(terminal_link(&term, Point::new(Line(0), Column(0))).is_none());
        assert!(terminal_link(&term, Point::new(Line(1), Column(5))).is_none());
        parser.advance(
            &mut term,
            b"\r\n\x1b]8;;https://example.org\x1b\\label\x1b]8;;\x1b\\",
        );
        assert_eq!(
            terminal_link(&term, Point::new(Line(2), Column(0))).as_deref(),
            Some("https://example.org")
        );
        let link = terminal_link_details(&term, Point::new(Line(2), Column(2))).unwrap();
        assert_eq!(link.points.len(), 5);
        assert_eq!(link.points.first(), Some(&Point::new(Line(2), Column(0))));
        assert_eq!(link.points.last(), Some(&Point::new(Line(2), Column(4))));
        assert!(browser_url("javascript:alert(1)").is_none());
        assert!(browser_url("file:///tmp/test").is_none());
    }

    #[test]
    fn terminal_parses_output_and_answers_queries() {
        let (tx, rx) = mpsc::channel();
        let mut term = Term::new(
            Config::default(),
            &Size { cols: 80, rows: 24 },
            Listener(tx),
        );
        let mut parser: Processor = Processor::new();
        parser.advance(&mut term, b"hello\x1b[6n");
        assert_eq!(
            term.grid()[alacritty_terminal::index::Line(0)][alacritty_terminal::index::Column(0)].c,
            'h'
        );
        assert!(
            rx.try_iter()
                .any(|e| matches!(e, Event::PtyWrite(text) if text == "\x1b[1;6R"))
        );
    }
    #[test]
    fn real_pty_runs_shell_and_receives_output() {
        let pair = portable_pty::native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut command = if cfg!(windows) {
            let mut command = CommandBuilder::new("powershell.exe");
            command.args(["-NoProfile", "-Command", "Write-Output yiterm-pty-ok"]);
            command
        } else {
            let mut command = CommandBuilder::new("/bin/sh");
            command.args(["-c", "printf yiterm-pty-ok"]);
            command
        };
        command.env("TERM", "xterm-256color");
        let mut child = pair.slave.spawn_command(command).unwrap();
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut output = Vec::new();
            let mut buffer = [0; 1024];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => output.extend_from_slice(&buffer[..n]),
                }
                // ConPTY may keep its output pipe open after the child exits.
                if String::from_utf8_lossy(&output).contains("yiterm-pty-ok") {
                    break;
                }
            }
            let _ = tx.send(output);
        });
        let output = match rx.recv_timeout(Duration::from_secs(15)) {
            Ok(output) => output,
            Err(error) => {
                let _ = child.kill();
                panic!("PTY output timed out: {error}");
            }
        };
        assert!(child.wait().unwrap().success());
        assert!(String::from_utf8_lossy(&output).contains("yiterm-pty-ok"));
    }
    #[test]
    fn keyboard_supports_control_and_application_cursor() {
        assert_eq!(
            key_bytes(Key::C, egui::Modifiers::CTRL, false),
            Some(vec![3])
        );
        assert_eq!(
            key_bytes(Key::ArrowUp, egui::Modifiers::NONE, true),
            Some(b"\x1bOA".to_vec())
        );
    }

    #[test]
    fn paste_shortcuts_include_shift_insert() {
        assert!(is_paste_shortcut(Key::Insert, egui::Modifiers::SHIFT));
        assert!(is_paste_shortcut(
            Key::V,
            egui::Modifiers::CTRL | egui::Modifiers::SHIFT
        ));
        assert!(is_paste_shortcut(Key::V, egui::Modifiers::MAC_CMD));
        assert!(!is_paste_shortcut(Key::Insert, egui::Modifiers::NONE));
        assert!(!is_paste_shortcut(Key::Insert, egui::Modifiers::CTRL));
        assert!(!is_paste_shortcut(
            Key::Insert,
            egui::Modifiers::CTRL | egui::Modifiers::SHIFT
        ));
        assert!(!is_paste_shortcut(Key::V, egui::Modifiers::CTRL));
        assert_eq!(
            key_bytes(Key::Insert, egui::Modifiers::NONE, false),
            Some(b"\x1b[2~".to_vec())
        );
    }
}

#[cfg(test)]
mod repaint_tests {
    use super::*;

    #[test]
    fn unchanged_title_does_not_enqueue_another_viewport_command() {
        let ctx = egui::Context::default();
        let mut previous = String::new();
        for (title, expected) in [
            ("Terminal", 1),
            ("Terminal", 0),
            ("remote", 1),
            ("remote", 0),
        ] {
            let output = ctx.run(Default::default(), |ctx| {
                update_window_title(ctx, &mut previous, title);
            });
            let commands = &output.viewport_output[&egui::ViewportId::ROOT].commands;
            assert_eq!(
                commands
                    .iter()
                    .filter(|cmd| matches!(cmd, egui::ViewportCommand::Title(_)))
                    .count(),
                expected
            );
        }
        assert_eq!(previous, "yiterm — remote");
    }
}
