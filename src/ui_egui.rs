//! Original Linux egui/eframe interface, sharing the terminal model with GPUI.
use super::*;
use eframe::egui::{self, Color32, FontId, Key, Rect, Vec2};
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
        let rect = ui.available_rect_before_wrap();
        // Remember popups before showing the menu: its closing click/Esc must
        // not become terminal input after the popup has disappeared.
        let popup_was_open = egui::Popup::is_any_open(ctx);
        let report_mouse = self.term.mode().intersects(TermMode::MOUSE_MODE)
            && !ctx.input(|i| i.modifiers.shift)
            && self.term.grid().display_offset() == 0;
        let response = ui.interact(rect, ui.id().with("context-menu"), egui::Sense::click());
        if response.secondary_clicked() && !report_mouse {
            tracing::debug!(target: "yiterm::mouse",
                session_id = %self.log_id, shift = ctx.input(|i| i.modifiers.shift),
                mode = ?self.term.mode(), display_offset = self.term.grid().display_offset(),
                action = "open_context_menu", "Terminal context menu requested"
            );
        }
        let popup = if report_mouse {
            // Keep an existing Shift+right-click menu open after Shift is
            // released, without treating application mouse clicks as triggers.
            egui::Popup::menu(&response)
                .open_memory(None)
                .at_pointer_fixed()
        } else {
            egui::Popup::context_menu(&response)
        };
        let menu_shown = popup
            .width(120.0)
            .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
            .show(|ui| {
                self.selecting = false;
                self.mouse_button = None;
                let can_copy = self
                    .term
                    .selection_to_string()
                    .is_some_and(|s| !s.is_empty());
                if ui
                    .add_enabled(can_copy, egui::Button::new("Copy"))
                    .clicked()
                {
                    self.copy_selection_egui(ctx);
                    ui.close();
                }
                if ui
                    .add_enabled(!self.paste_busy, egui::Button::new("Paste"))
                    .clicked()
                {
                    self.paste();
                    ui.close();
                }
            })
            .is_some();
        let popup_blocks_input = popup_was_open || menu_shown || egui::Popup::is_any_open(ctx);
        // The active pane keeps its visual selection behind dialogs, but must
        // yield both keyboard events and IME output to the focused text editor.
        // In particular, don't overwrite TextEdit's IME cursor later in the frame.
        let focused = focused
            && !ctx.wants_keyboard_input()
            && !popup_blocks_input
            && ctx.memory(|memory| memory.top_modal_layer().is_none());
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
        let scroll = if hovered && !popup_blocks_input {
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
        let events = if focused {
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
                    if pressed && rect.contains(pos) {
                        tracing::debug!(target: "yiterm::mouse",
                            session_id = %self.log_id, ?button, shift = modifiers.shift,
                            mode = ?self.term.mode(), display_offset = offset,
                            has_selection = self.term.selection.as_ref().is_some_and(|s| !s.is_empty()),
                            action = if report_mouse { "forward_to_application" }
                                else if button == egui::PointerButton::Primary { "start_selection" }
                                else if button == egui::PointerButton::Secondary { "context_menu_on_release" }
                                else { "ignore" },
                            "Terminal mouse down"
                        );
                    }
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
            } else {
                terminal_colors(indexed.cell, theme).1
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
            } else {
                terminal_colors(indexed.cell, theme).0
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
            if !egui::Popup::is_any_open(ctx)
                && ctx.input(|input| {
                    input.pointer.primary_pressed() || input.pointer.secondary_pressed()
                })
                && ui.rect_contains_pointer(rect)
            {
                self.select(index);
            }
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
    fn raw_input_hook(&mut self, ctx: &egui::Context, input: &mut egui::RawInput) {
        // terminal-input leaves clipboard shortcuts as keys for the PTY. Restore
        // ordinary text editing while settings or a searchable menu owns input.
        if !self.settings_open && !ctx.wants_keyboard_input() {
            return;
        }
        for event in &mut input.events {
            let egui::Event::Key {
                key,
                pressed: true,
                modifiers,
                ..
            } = event
            else {
                continue;
            };
            if !modifiers.command || modifiers.alt {
                continue;
            }
            match key {
                Key::C => *event = egui::Event::Copy,
                Key::X => *event = egui::Event::Cut,
                Key::V => {
                    if let Ok(text) = arboard::Clipboard::new().and_then(|mut c| c.get_text()) {
                        *event = egui::Event::Paste(text.replace("\r\n", "\n"));
                    }
                }
                _ => {}
            }
        }
    }

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
                    ctx.data_mut(|data| {
                        data.insert_temp(
                            egui::Id::new("shell-choices"),
                            settings::available_shell_programs(),
                        );
                        data.remove::<String>(egui::Id::new("shell-search"));
                        data.remove::<String>(egui::Id::new("shell-program-draft"));
                        data.remove::<ShellFilePicker>(egui::Id::new("shell-file-picker"));
                    });
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
                // Done/outside clicks can move focus after TextEdit was rendered.
                // Commit its draft here as well, before removing the dialog.
                if self.shell_settings.mode == settings::ShellMode::Program
                    && let Some(draft) = ctx.data_mut(|data| {
                        data.get_temp::<String>(egui::Id::new("shell-program-draft"))
                    })
                    && !draft.trim().is_empty()
                    && !draft.contains('\0')
                {
                    self.shell_settings.program = draft;
                }
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

#[derive(Clone, Default)]
struct ShellFilePicker {
    pending: bool,
    result: Option<std::result::Result<Option<String>, String>>,
    error: Option<String>,
}

fn pick_shell_file(directory: &std::path::Path) -> Result<Option<String>> {
    use std::os::unix::fs::PermissionsExt;
    use std::process::{Command, Stdio};

    let kde = std::env::var("XDG_CURRENT_DESKTOP")
        .unwrap_or_default()
        .split(':')
        .any(|desktop| desktop.eq_ignore_ascii_case("KDE"));
    let programs = if kde {
        ["kdialog", "zenity"]
    } else {
        ["zenity", "kdialog"]
    };
    for program in programs {
        let mut command = Command::new(program);
        command.stdin(Stdio::null());
        if program == "zenity" {
            command.args(["--file-selection", "--title=Select shell executable"]);
            command.arg(format!("--filename={}/", directory.display()));
        } else {
            command.args(["--title", "Select shell executable", "--getopenfilename"]);
            command.arg(directory);
        }
        let output = match command.output() {
            Ok(output) => output,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("Opening {program} file dialog"));
            }
        };
        if output.status.code() == Some(1) {
            return Ok(None);
        }
        anyhow::ensure!(
            output.status.success(),
            "Cannot open {program} file dialog: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
        let mut path =
            String::from_utf8(output.stdout).context("Selected path is not valid UTF-8")?;
        // Remove only the dialog's line terminator, preserving filename whitespace.
        if path.ends_with('\n') {
            path.pop();
        }
        anyhow::ensure!(!path.is_empty(), "No file was selected");
        let metadata = std::fs::metadata(&path).context("Reading selected file")?;
        anyhow::ensure!(
            metadata.is_file() && metadata.permissions().mode() & 0o111 != 0,
            "Select an executable file."
        );
        return Ok(Some(path));
    }
    anyhow::bail!("Install zenity or kdialog to open the system file chooser.")
}

fn show_shell_settings(ui: &mut egui::Ui, shell: &mut settings::ShellSettings) {
    let ctx = ui.ctx().clone();
    let choices_id = egui::Id::new("shell-choices");
    let choices = ctx.data_mut(|data| {
        data.get_temp::<Vec<String>>(choices_id).unwrap_or_else(|| {
            let choices = settings::available_shell_programs();
            data.insert_temp(choices_id, choices.clone());
            choices
        })
    });
    let draft_id = egui::Id::new("shell-program-draft");
    let picker_id = egui::Id::new("shell-file-picker");
    if let Some(mut picker) = ctx.data_mut(|data| data.get_temp::<ShellFilePicker>(picker_id)) {
        if let Some(result) = picker.result.take() {
            match result {
                Ok(Some(path)) => {
                    shell.mode = settings::ShellMode::Program;
                    shell.program = path;
                    ctx.data_mut(|data| data.insert_temp(draft_id, shell.program.clone()));
                }
                Ok(None) => {}
                Err(error) => picker.error = Some(error),
            }
            ctx.data_mut(|data| data.insert_temp(picker_id, picker.clone()));
        }
    }
    settings_row(
        ui,
        "Shell",
        "Shell for new terminals. --shell takes priority.",
        300.0,
        |ui| {
            let label = if shell.mode == settings::ShellMode::Default {
                "Default"
            } else {
                &shell.program
            };
            let button = ui.add_sized(
                [210.0, 28.0],
                egui::Button::new(label).right_text("⏷").truncate(),
            );
            button.clone().on_hover_text(label);
            egui::Popup::menu(&button)
                .width(300.0)
                .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
                .show(|ui| {
                    let search_id = egui::Id::new("shell-search");
                    let mut search =
                        ctx.data_mut(|data| data.get_temp::<String>(search_id).unwrap_or_default());
                    ui.add(
                        egui::TextEdit::singleline(&mut search)
                            .id(egui::Id::new("shell-search-input"))
                            .hint_text("Search shells…")
                            .desired_width(f32::INFINITY),
                    );
                    let query = search.to_lowercase();
                    ctx.data_mut(|data| data.insert_temp(search_id, search));
                    ui.separator();
                    if ui
                        .selectable_label(shell.mode == settings::ShellMode::Default, "Default")
                        .clicked()
                    {
                        shell.mode = settings::ShellMode::Default;
                        ui.close();
                    }
                    if ui
                        .selectable_label(false, "Program (enter path…)")
                        .clicked()
                    {
                        shell.mode = settings::ShellMode::Program;
                        ctx.data_mut(|data| data.insert_temp(draft_id, shell.program.clone()));
                        ui.close();
                    }
                    egui::ScrollArea::vertical()
                        .id_salt("shell-list")
                        .min_scrolled_height(120.0)
                        .max_height(240.0)
                        .show(ui, |ui| {
                            let mut found = false;
                            for program in
                                choices.iter().filter(|p| p.to_lowercase().contains(&query))
                            {
                                found = true;
                                if ui
                                    .selectable_label(
                                        shell.mode == settings::ShellMode::Program
                                            && shell.program == *program,
                                        program,
                                    )
                                    .clicked()
                                {
                                    shell.mode = settings::ShellMode::Program;
                                    shell.program = program.clone();
                                    ctx.data_mut(|data| {
                                        data.insert_temp(draft_id, program.clone())
                                    });
                                    ui.close();
                                }
                            }
                            if !found {
                                ui.weak("No matching shells. Enter a path or use Browse…");
                            }
                        });
                });
            let pending = ctx.data_mut(|data| {
                data.get_temp::<ShellFilePicker>(picker_id)
                    .is_some_and(|picker| picker.pending)
            });
            if ui
                .add_enabled(!pending, egui::Button::new("Browse…"))
                .clicked()
            {
                let directory = std::path::Path::new(&shell.program)
                    .parent()
                    .filter(|p| p.is_dir())
                    .unwrap_or_else(|| std::path::Path::new("/bin"))
                    .to_path_buf();
                ctx.data_mut(|data| {
                    data.insert_temp(
                        picker_id,
                        ShellFilePicker {
                            pending: true,
                            ..Default::default()
                        },
                    )
                });
                let worker_ctx = ctx.clone();
                let worker = std::thread::Builder::new()
                    .name("shell-file-dialog".into())
                    .spawn(move || {
                        let result =
                            pick_shell_file(&directory).map_err(|error| format!("{error:#}"));
                        worker_ctx.data_mut(|data| {
                            data.insert_temp(
                                picker_id,
                                ShellFilePicker {
                                    result: Some(result),
                                    ..Default::default()
                                },
                            )
                        });
                        worker_ctx.request_repaint();
                    });
                if let Err(error) = worker {
                    ctx.data_mut(|data| {
                        data.insert_temp(
                            picker_id,
                            ShellFilePicker {
                                error: Some(format!("Cannot open file dialog: {error}")),
                                ..Default::default()
                            },
                        )
                    });
                }
            }
        },
    );
    if let Some(error) = ctx.data_mut(|data| {
        data.get_temp::<ShellFilePicker>(picker_id)
            .and_then(|picker| picker.error)
    }) {
        ui.colored_label(Color32::RED, error);
    }
    if shell.mode == settings::ShellMode::Program {
        let mut draft = ctx.data_mut(|data| {
            data.get_temp::<String>(draft_id)
                .unwrap_or_else(|| shell.program.clone())
        });
        let mut finished_editing = false;
        settings_row(
            ui,
            "Program",
            "Executable name or path, without arguments.",
            300.0,
            |ui| {
                let response = ui.add(
                    egui::TextEdit::singleline(&mut draft)
                        .id(egui::Id::new("shell-program-input"))
                        .desired_width(290.0),
                );
                finished_editing = response.lost_focus();
            },
        );
        // Keep invalid intermediate input visible without overwriting the last
        // valid setting or sending save failures to the terminal behind the modal.
        if draft.trim().is_empty() {
            ui.colored_label(Color32::RED, "Shell program cannot be empty");
        } else if draft.contains('\0') {
            ui.colored_label(Color32::RED, "Shell program contains a null character");
        } else if finished_editing {
            shell.program = draft.clone();
        }
        ctx.data_mut(|data| data.insert_temp(draft_id, draft));
    }
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
                    show_shell_settings(ui, shell);
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
                                                .id(egui::Id::new("font-search-input"))
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
                        ctx.data_mut(|data| {
                            data.remove::<String>(egui::Id::new("font-search"));
                            data.remove::<String>(egui::Id::new("shell-program-draft"));
                            data.remove::<ShellFilePicker>(egui::Id::new("shell-file-picker"));
                        });
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
