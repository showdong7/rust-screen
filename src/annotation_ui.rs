use ab_glyph::FontArc;
use eframe::egui::{self, Color32, Pos2, Rect, Sense, Stroke, Vec2};

use crate::{
    annotation::{Annotation, Editor, ResizeDrag, TextEdit, Tool},
    annotation_render,
    annotation_resize::{self, Handle},
    geometry::CropRect,
};

pub fn toolbar(ui: &mut egui::Ui, editor: &mut Editor) {
    ui.horizontal_wrapped(|ui| {
        for (tool, label) in Tool::ALL {
            if ui.selectable_label(editor.tool == tool, label).clicked() {
                editor.finish_text();
                editor.cancel_gesture();
                editor.tool = tool;
                editor.selected = None;
            }
        }
        if ui
            .add_enabled(editor.can_undo(), egui::Button::new("復原"))
            .on_hover_text("Ctrl+Z")
            .clicked()
        {
            editor.undo();
        }
        if ui
            .add_enabled(editor.can_redo(), egui::Button::new("重做"))
            .on_hover_text("Ctrl+Y / Ctrl+Shift+Z")
            .clicked()
        {
            editor.redo();
        }
        if ui
            .add_enabled(editor.selected.is_some(), egui::Button::new("刪除"))
            .clicked()
        {
            editor.delete();
        }
        if editor
            .selected
            .is_some_and(|i| editor.document.items[i].tool == Tool::Text)
            && ui.button("編輯文字").clicked()
        {
            begin_text(editor);
        }
    });
    ui.horizontal_wrapped(|ui| {
        let mut changed = false;
        let shape_tool = editor
            .selected
            .map(|i| editor.document.items[i].tool)
            .unwrap_or(editor.tool);
        if matches!(shape_tool, Tool::Rectangle | Tool::Ellipse) {
            ui.label("填色");
            changed |= ui
                .selectable_value(&mut editor.style.filled, false, "空心")
                .changed();
            changed |= ui
                .selectable_value(&mut editor.style.filled, true, "實心")
                .changed();
        }
        ui.label("顏色");
        changed |= ui
            .color_edit_button_srgba(&mut editor.style.color)
            .changed();
        for color in [
            Color32::from_rgb(235, 55, 55),
            Color32::from_rgb(255, 190, 30),
            Color32::from_rgb(40, 190, 90),
            Color32::from_rgb(45, 130, 245),
            Color32::WHITE,
            Color32::BLACK,
        ] {
            let (rect, response) = ui.allocate_exact_size(Vec2::splat(20.0), Sense::click());
            ui.painter().rect_filled(rect.shrink(2.0), 3.0, color);
            ui.painter().rect_stroke(
                rect.shrink(1.0),
                3.0,
                Stroke::new(
                    if editor.style.color == color {
                        2.0_f32
                    } else {
                        1.0_f32
                    },
                    ui.visuals().text_color(),
                ),
                egui::StrokeKind::Inside,
            );
            if response.clicked() {
                editor.style.color = color;
                changed = true;
            }
        }
        ui.label("線寬");
        changed |= ui
            .add_enabled(
                !(matches!(shape_tool, Tool::Rectangle | Tool::Ellipse) && editor.style.filled),
                egui::DragValue::new(&mut editor.style.width)
                    .range(1.0..=40.0)
                    .suffix(" px"),
            )
            .changed();
        ui.label("字級");
        changed |= ui
            .add(
                egui::DragValue::new(&mut editor.style.font_size)
                    .range(8.0..=200.0)
                    .suffix(" px"),
            )
            .changed();
        if changed {
            // Only editor.style has changed so far; snapshot the document before applying
            // it, rather than cloning the whole document on every frame.
            let before = editor.document.clone();
            if let Some(edit) = &mut editor.text_edit {
                edit.item.style = editor.style;
            } else if let Some(index) = editor.selected {
                editor.document.items[index].style = editor.style;
            }
            editor.commit(before);
            editor.repaint();
        }
    });
    ui.label(egui::RichText::new("選取後拖曳控制點調整大小；拖曳物件移動，雙擊文字編輯。Shift：繪製正方形／正圓，縮放角點保持比例。Esc 取消拖曳。").small());
}

fn begin_text(editor: &mut Editor) {
    if let Some(index) = editor.selected {
        editor.text_edit = Some(TextEdit {
            item: editor.document.items[index].clone(),
            index: Some(index),
            focus: true,
            composing: false,
        });
        editor.repaint();
    }
}

pub fn interact(
    ui: &mut egui::Ui,
    editor: &mut Editor,
    image_rect: Rect,
    view: CropRect,
    font: &FontArc,
) {
    let response = ui.interact(
        if editor.tool == Tool::Select {
            image_rect.expand(7.0)
        } else {
            image_rect
        },
        ui.id().with("annotations"),
        Sense::click_and_drag(),
    );
    if !ui.is_enabled() {
        return;
    }
    if response.clicked() || response.drag_started() {
        response.request_focus();
    }
    // egui may end a drag on Escape. Restore the snapshot before the drag-stop
    // handler can commit it, even if the canvas just lost keyboard focus.
    let gesture_active =
        editor.resizing.is_some() || editor.moving.is_some() || editor.draft.is_some();
    if editor.text_edit.is_none()
        && ui.input(|i| i.key_pressed(egui::Key::Escape))
        && (gesture_active || !ui.ctx().wants_keyboard_input() || response.has_focus())
    {
        editor.cancel_gesture();
        editor.selected = None;
        return;
    }
    let scale = image_rect.width() / view.width as f32;
    let bounds = Rect::from_min_size(
        Pos2::new(view.x as f32, view.y as f32),
        Vec2::new(view.width as f32, view.height as f32),
    );
    let to_image = |position: Pos2| {
        (bounds.min + (position - image_rect.min) / scale).clamp(bounds.min, bounds.max)
    };
    let hit = |editor: &Editor, position: Pos2| {
        editor.document.items.iter().rposition(|item| {
            item.hit(
                position,
                6.0 / scale,
                annotation_render::bounds(item, font).size(),
            )
        })
    };
    if editor.text_edit.is_none() {
        if (response.clicked() || response.double_clicked())
            && let Some(position) = response.interact_pointer_pos()
        {
            let pointer = position;
            let position = to_image(pointer);
            if editor.tool == Tool::Select {
                if handle_at(editor, image_rect, view, font, pointer).is_none() {
                    editor.selected = if image_rect.contains(pointer) {
                        hit(editor, position)
                    } else {
                        None
                    };
                    if let Some(index) = editor.selected {
                        editor.style = editor.document.items[index].style;
                    }
                    if response.double_clicked()
                        && editor
                            .selected
                            .is_some_and(|i| editor.document.items[i].tool == Tool::Text)
                    {
                        begin_text(editor);
                    }
                }
            } else if editor.tool == Tool::Text {
                editor.selected =
                    hit(editor, position).filter(|i| editor.document.items[*i].tool == Tool::Text);
                if editor.selected.is_some() {
                    begin_text(editor);
                } else {
                    editor.text_edit = Some(TextEdit {
                        item: Annotation::new(Tool::Text, position, editor.style),
                        index: None,
                        focus: true,
                        composing: false,
                    });
                }
                editor.repaint();
            }
        }
        if response.drag_started() {
            let pointer = ui
                .input(|i| i.pointer.press_origin())
                .unwrap_or(image_rect.min);
            let position = to_image(pointer);
            if editor.tool == Tool::Select {
                if let Some((handle, source)) = handle_at(editor, image_rect, view, font, pointer) {
                    let item = &editor.document.items[editor.selected.unwrap()];
                    editor.resizing = Some(ResizeDrag {
                        handle,
                        bounds: resize_bounds(item, font),
                        before: editor.document.clone(),
                        offset: source - position,
                    });
                } else {
                    editor.selected = if image_rect.contains(pointer) {
                        hit(editor, position)
                    } else {
                        None
                    };
                    if let Some(index) = editor.selected {
                        editor.style = editor.document.items[index].style;
                        editor.moving = Some((position, editor.document.clone()));
                    }
                }
            } else if editor.tool != Tool::Text {
                editor.selected = None;
                editor.draft = Some(Annotation::new(editor.tool, position, editor.style));
            }
        }
        if (response.dragged() || response.drag_stopped())
            && let Some(position) = response.interact_pointer_pos()
        {
            let position = to_image(position);
            if let Some(draft) = &mut editor.draft {
                draft.set_end(position, ui.input(|i| i.modifiers.shift), bounds);
                editor.repaint();
            }
            if let Some((origin, before)) = &editor.moving
                && let Some(index) = editor.selected
            {
                let mut item = before.items[index].clone();
                let rect = annotation_render::bounds(&item, font);
                let mut delta = position - *origin;
                // Keep the anchor visible, including objects partly clipped by a crop.
                delta.x = delta
                    .x
                    .clamp(bounds.left() - rect.right(), bounds.right() - rect.left());
                delta.y = delta
                    .y
                    .clamp(bounds.top() - rect.bottom(), bounds.bottom() - rect.top());
                item.translate(delta);
                editor.document.items[index] = item;
                editor.repaint();
            }
            if let Some(drag) = &editor.resizing
                && let Some(index) = editor.selected
            {
                let item = annotation_resize::resize(
                    &drag.before.items[index],
                    drag.bounds,
                    drag.handle,
                    position + drag.offset,
                    ui.input(|i| i.modifiers.shift),
                );
                editor.style = item.style;
                editor.document.items[index] = item;
                editor.repaint();
            }
        }
        if response.drag_stopped() {
            if let Some(draft) = editor.draft.take() {
                if draft.start.distance(draft.end) * scale >= 3.0
                    && (!matches!(draft.tool, Tool::Rectangle | Tool::Ellipse)
                        || (draft.end.x - draft.start.x)
                            .abs()
                            .min((draft.end.y - draft.start.y).abs())
                            * scale
                            >= 1.0)
                {
                    let before = editor.document.clone();
                    editor.selected = Some(editor.document.items.len());
                    editor.document.items.push(draft);
                    editor.commit(before);
                }
                editor.repaint();
            }
            if let Some((_, before)) = editor.moving.take() {
                editor.commit(before);
            }
            if let Some(drag) = editor.resizing.take() {
                editor.commit(drag.before);
            }
        }
    }
    if (!ui.ctx().wants_keyboard_input() || response.has_focus()) && editor.text_edit.is_none() {
        // Use the key event's modifiers: a quick chord can be pressed and
        // released entirely between frames, leaving InputState.modifiers empty.
        let redo = ui.input_mut(|i| {
            i.consume_key(
                egui::Modifiers::COMMAND | egui::Modifiers::SHIFT,
                egui::Key::Z,
            ) || i.consume_key(egui::Modifiers::COMMAND, egui::Key::Y)
        });
        if redo {
            editor.redo();
        } else if ui.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::Z)) {
            editor.undo();
        } else if ui.input(|i| i.key_pressed(egui::Key::Delete)) {
            editor.delete();
        }
    }
    if response.hovered() || response.dragged() {
        let handle = editor
            .resizing
            .as_ref()
            .map(|drag| drag.handle)
            .or_else(|| {
                response.hover_pos().and_then(|pos| {
                    handle_at(editor, image_rect, view, font, pos).map(|(handle, _)| handle)
                })
            });
        ui.ctx().set_cursor_icon(match editor.tool {
            Tool::Select => handle.map(Handle::cursor).unwrap_or_else(|| {
                if editor.moving.is_some() {
                    egui::CursorIcon::Grabbing
                } else if response
                    .hover_pos()
                    .is_some_and(|pos| hit(editor, to_image(pos)).is_some())
                {
                    egui::CursorIcon::Grab
                } else {
                    egui::CursorIcon::Default
                }
            }),
            Tool::Text => egui::CursorIcon::Text,
            _ => egui::CursorIcon::Crosshair,
        });
    }
}

fn resize_bounds(item: &Annotation, font: &FontArc) -> Rect {
    if item.tool == Tool::Text {
        annotation_render::bounds(item, font)
    } else {
        Rect::from_two_pos(item.start, item.end)
    }
}

fn control_points(
    editor: &Editor,
    image_rect: Rect,
    view: CropRect,
    font: &FontArc,
) -> Vec<(Handle, Pos2, Pos2)> {
    if editor.tool != Tool::Select || editor.text_edit.is_some() {
        return Vec::new();
    }
    let Some(index) = editor.selected else {
        return Vec::new();
    };
    let item = &editor.document.items[index];
    let scale = image_rect.width() / view.width as f32;
    let origin = Pos2::new(view.x as f32, view.y as f32);
    annotation_resize::handles(item, resize_bounds(item, font))
        .into_iter()
        .filter_map(|(handle, source)| {
            let screen = image_rect.min + (source - origin) * scale;
            image_rect
                .expand(0.01)
                .contains(screen)
                .then_some((handle, source, screen))
        })
        .collect()
}

fn handle_at(
    editor: &Editor,
    image_rect: Rect,
    view: CropRect,
    font: &FontArc,
    pointer: Pos2,
) -> Option<(Handle, Pos2)> {
    control_points(editor, image_rect, view, font)
        .into_iter()
        .filter(|(_, _, screen)| screen.distance(pointer) <= 7.0)
        .min_by(|a, b| {
            a.2.distance_sq(pointer)
                .total_cmp(&b.2.distance_sq(pointer))
        })
        .map(|(handle, source, _)| (handle, source))
}

pub fn selection(ui: &egui::Ui, editor: &Editor, image_rect: Rect, view: CropRect, font: &FontArc) {
    if let Some(index) = editor.selected {
        let bounds = annotation_render::bounds(&editor.document.items[index], font);
        let scale = image_rect.width() / view.width as f32;
        let offset = Vec2::new(view.x as f32, view.y as f32);
        let rect = Rect::from_min_max(
            image_rect.min + (bounds.min.to_vec2() - offset) * scale,
            image_rect.min + (bounds.max.to_vec2() - offset) * scale,
        );
        ui.painter().with_clip_rect(image_rect).rect_stroke(
            rect.expand(4.0),
            2.0,
            Stroke::new(1.0_f32, Color32::from_rgb(0, 180, 220)),
            egui::StrokeKind::Outside,
        );
        let painter = ui.painter().with_clip_rect(image_rect.expand(5.0));
        for (_, _, screen) in control_points(editor, image_rect, view, font) {
            let rect = Rect::from_center_size(screen, Vec2::splat(8.0));
            painter.rect_filled(rect, 1.0, Color32::WHITE);
            painter.rect_stroke(
                rect,
                1.0,
                Stroke::new(1.0_f32, Color32::from_rgb(0, 140, 190)),
                egui::StrokeKind::Inside,
            );
        }
    }
}

pub fn text_window(ctx: &egui::Context, editor: &mut Editor) {
    let mut apply = false;
    let mut cancel = false;
    let mut repaint = false;
    if let Some(edit) = &mut editor.text_edit {
        let was_composing = edit.composing;
        let ime_event = ctx.input(|input| {
            let mut any = false;
            for event in &input.events {
                if let egui::Event::Ime(event) = event {
                    any = true;
                    match event {
                        egui::ImeEvent::Preedit(text) => edit.composing = !text.is_empty(),
                        egui::ImeEvent::Commit(_) | egui::ImeEvent::Disabled => {
                            edit.composing = false
                        }
                        egui::ImeEvent::Enabled => {}
                    }
                }
            }
            any
        });
        let mut open = true;
        egui::Window::new("編輯標註文字")
            .id(egui::Id::new("annotation_text"))
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_width(320.0)
            .show(ctx, |ui| {
                ui.label("輸入文字；Enter 換行，完成後按「套用」。");
                let response = ui.add(
                    egui::TextEdit::multiline(&mut edit.item.text)
                        .desired_width(f32::INFINITY)
                        .desired_rows(4)
                        .char_limit(4096),
                );
                if edit.focus {
                    response.request_focus();
                    edit.focus = false;
                }
                repaint |= response.changed();
                ui.horizontal(|ui| {
                    apply = ui
                        .add_enabled(!edit.item.text.trim().is_empty(), egui::Button::new("套用"))
                        .clicked();
                    cancel = ui.button("取消").clicked();
                });
                // TextEdit/IME owns Enter and Escape while composing.
                if ui.input(|i| i.key_pressed(egui::Key::Escape))
                    && !was_composing
                    && !edit.composing
                    && !ime_event
                {
                    cancel = true;
                }
            });
        cancel |= !open;
    }
    if apply {
        editor.finish_text();
    }
    if cancel {
        editor.text_edit = None;
        editor.repaint();
    }
    if repaint {
        editor.repaint();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::annotation::Style;

    fn frame(ctx: &egui::Context, editor: &mut Editor, font: &FontArc, events: Vec<egui::Event>) {
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(600.0, 400.0))),
            modifiers: events
                .iter()
                .rev()
                .find_map(|event| {
                    if let egui::Event::Key { modifiers, .. } = event {
                        Some(*modifiers)
                    } else {
                        None
                    }
                })
                .unwrap_or_default(),
            events,
            ..Default::default()
        };
        let _ = ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                interact(
                    ui,
                    editor,
                    Rect::from_min_size(Pos2::new(20.0, 50.0), Vec2::new(400.0, 200.0)),
                    CropRect {
                        x: 100,
                        y: 200,
                        width: 800,
                        height: 400,
                    },
                    font,
                );
            });
        });
    }

    fn button(pos: Pos2, pressed: bool) -> egui::Event {
        egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        }
    }

    #[test]
    fn mouse_drags_create_all_shapes_in_original_coordinates_after_crop() {
        let font = annotation_render::load_font();
        for tool in [Tool::Line, Tool::Rectangle, Tool::Ellipse, Tool::Arrow] {
            let ctx = egui::Context::default();
            let mut editor = Editor::new(
                1200,
                800,
                Style {
                    color: Color32::RED,
                    width: 3.0,
                    font_size: 32.0,
                    filled: false,
                },
            );
            editor.tool = tool;
            let start = Pos2::new(60.0, 80.0);
            let end = Pos2::new(180.0, 160.0);
            frame(&ctx, &mut editor, &font, vec![]);
            frame(
                &ctx,
                &mut editor,
                &font,
                vec![egui::Event::PointerMoved(start), button(start, true)],
            );
            frame(
                &ctx,
                &mut editor,
                &font,
                vec![egui::Event::PointerMoved(end)],
            );
            frame(&ctx, &mut editor, &font, vec![button(end, false)]);
            assert_eq!(editor.document.items.len(), 1, "{tool:?}");
            let item = &editor.document.items[0];
            assert_eq!(item.start, Pos2::new(180.0, 260.0), "{tool:?}");
            assert_eq!(item.end, Pos2::new(420.0, 420.0), "{tool:?}");
            let rendered = annotation_render::overlay(
                &editor.document.items,
                &font,
                CropRect {
                    x: 100,
                    y: 200,
                    width: 800,
                    height: 400,
                },
                400,
                200,
            )
            .unwrap();
            assert!(rendered.pixels().any(|p| p[3] > 0), "{tool:?}");
            editor.undo();
            assert!(editor.document.items.is_empty());
            editor.redo();
            assert_eq!(editor.document.items.len(), 1);
        }
    }

    #[test]
    fn select_drag_moves_topmost_object_and_undo_restores_it() {
        let font = annotation_render::load_font();
        let ctx = egui::Context::default();
        let mut editor = Editor::new(
            1200,
            800,
            Style {
                color: Color32::RED,
                width: 3.0,
                font_size: 32.0,
                filled: false,
            },
        );
        let mut item = Annotation::new(Tool::Line, Pos2::new(180.0, 260.0), editor.style);
        item.end = Pos2::new(420.0, 420.0);
        editor.document.items = vec![item.clone(), item];
        let before = editor.document.clone();
        let start = Pos2::new(60.0, 80.0);
        let end = Pos2::new(90.0, 100.0);
        frame(&ctx, &mut editor, &font, vec![]);
        frame(
            &ctx,
            &mut editor,
            &font,
            vec![egui::Event::PointerMoved(start), button(start, true)],
        );
        frame(
            &ctx,
            &mut editor,
            &font,
            vec![egui::Event::PointerMoved(end)],
        );
        frame(&ctx, &mut editor, &font, vec![button(end, false)]);
        assert_eq!(editor.selected, Some(1));
        assert_eq!(editor.document.items[0], before.items[0]);
        assert_eq!(editor.document.items[1].start, Pos2::new(240.0, 300.0));
        editor.undo();
        assert_eq!(editor.document, before);
    }

    #[test]
    fn selected_shape_handle_wins_over_an_overlapping_solid_object_and_history_is_one_step() {
        let font = annotation_render::load_font();
        let ctx = egui::Context::default();
        let mut editor = Editor::new(
            1200,
            800,
            Style {
                color: Color32::RED,
                width: 3.0,
                font_size: 32.0,
                filled: false,
            },
        );
        let mut item = Annotation::new(Tool::Rectangle, Pos2::new(180.0, 260.0), editor.style);
        item.end = Pos2::new(420.0, 420.0);
        let mut covering = item.clone();
        covering.start = Pos2::new(380.0, 380.0);
        covering.end = Pos2::new(580.0, 500.0);
        covering.style.filled = true;
        editor.document.items = vec![item, covering];
        editor.selected = Some(0);
        editor.saved();
        let before = editor.document.clone();
        let start = Pos2::new(180.0, 160.0);
        let end = Pos2::new(230.0, 180.0);
        frame(&ctx, &mut editor, &font, vec![]);
        frame(
            &ctx,
            &mut editor,
            &font,
            vec![egui::Event::PointerMoved(start), button(start, true)],
        );
        frame(
            &ctx,
            &mut editor,
            &font,
            vec![egui::Event::PointerMoved(Pos2::new(200.0, 170.0))],
        );
        frame(
            &ctx,
            &mut editor,
            &font,
            vec![egui::Event::PointerMoved(end)],
        );
        frame(&ctx, &mut editor, &font, vec![button(end, false)]);
        assert_eq!(editor.selected, Some(0));
        assert_eq!(editor.document.items[0].start, Pos2::new(180.0, 260.0));
        assert_eq!(editor.document.items[0].end, Pos2::new(520.0, 460.0));
        assert_eq!(editor.document.items[1], before.items[1]);
        assert!(editor.dirty());
        editor.undo();
        assert_eq!(editor.document, before);
        assert!(!editor.can_undo());
        assert!(!editor.dirty());
        editor.redo();
        assert_eq!(editor.document.items[0].end, Pos2::new(520.0, 460.0));
    }

    #[test]
    fn escape_during_text_resize_restores_font_position_and_style_without_history_entry() {
        let font = annotation_render::load_font();
        let ctx = egui::Context::default();
        let mut editor = Editor::new(
            1200,
            800,
            Style {
                color: Color32::RED,
                width: 3.0,
                font_size: 32.0,
                filled: false,
            },
        );
        let mut item = Annotation::new(Tool::Text, Pos2::new(180.0, 260.0), editor.style);
        item.text = "文字\nTest".into();
        let bounds = annotation_render::bounds(&item, &font);
        let start = Pos2::new(20.0, 50.0) + (bounds.max - Pos2::new(100.0, 200.0)) * 0.5;
        editor.document.items.push(item);
        editor.selected = Some(0);
        editor.saved();
        let before = editor.document.clone();
        let end = start + Vec2::new(30.0, 20.0);
        frame(&ctx, &mut editor, &font, vec![]);
        frame(
            &ctx,
            &mut editor,
            &font,
            vec![egui::Event::PointerMoved(start), button(start, true)],
        );
        frame(
            &ctx,
            &mut editor,
            &font,
            vec![egui::Event::PointerMoved(end)],
        );
        assert!(editor.document.items[0].style.font_size > 32.0);
        frame(
            &ctx,
            &mut editor,
            &font,
            vec![egui::Event::Key {
                key: egui::Key::Escape,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
        );
        frame(&ctx, &mut editor, &font, vec![button(end, false)]);
        assert_eq!(editor.document, before);
        assert_eq!(editor.style.font_size, 32.0);
        assert!(!editor.can_undo());
        assert!(!editor.dirty());
        assert!(editor.resizing.is_none());
    }

    #[test]
    fn handle_hit_area_is_constant_in_screen_points_and_hidden_crop_handles_are_ignored() {
        let font = annotation_render::load_font();
        let mut editor = Editor::new(
            1200,
            800,
            Style {
                color: Color32::RED,
                width: 3.0,
                font_size: 32.0,
                filled: false,
            },
        );
        let mut item = Annotation::new(Tool::Ellipse, Pos2::new(180.0, 260.0), editor.style);
        item.end = Pos2::new(420.0, 420.0);
        editor.document.items.push(item);
        editor.selected = Some(0);
        let view = CropRect {
            x: 100,
            y: 200,
            width: 800,
            height: 400,
        };
        for scale in [0.25, 0.5, 1.0, 2.0] {
            let rect = Rect::from_min_size(Pos2::new(20.0, 50.0), Vec2::new(800.0, 400.0) * scale);
            let point = rect.min + Vec2::new(320.0, 220.0) * scale;
            assert_eq!(
                handle_at(&editor, rect, view, &font, point + Vec2::new(6.0, 0.0))
                    .unwrap()
                    .0,
                Handle::BottomRight
            );
            assert!(handle_at(&editor, rect, view, &font, point + Vec2::new(8.0, 0.0)).is_none());
        }
        let cropped = CropRect {
            x: 250,
            y: 300,
            width: 200,
            height: 200,
        };
        let points = control_points(
            &editor,
            Rect::from_min_size(Pos2::ZERO, Vec2::new(200.0, 200.0)),
            cropped,
            &font,
        );
        assert!(
            points
                .iter()
                .all(|(handle, _, _)| *handle != Handle::TopLeft)
        );
        assert!(
            points
                .iter()
                .any(|(handle, _, _)| *handle == Handle::BottomRight)
        );
    }

    #[test]
    fn canvas_takes_keyboard_focus_and_shortcuts_undo_and_redo_a_resize() {
        let font = annotation_render::load_font();
        let ctx = egui::Context::default();
        let mut editor = Editor::new(
            1200,
            800,
            Style {
                color: Color32::RED,
                width: 3.0,
                font_size: 32.0,
                filled: false,
            },
        );
        let mut item = Annotation::new(Tool::Arrow, Pos2::new(180.0, 260.0), editor.style);
        item.end = Pos2::new(420.0, 420.0);
        editor.document.items.push(item);
        editor.selected = Some(0);
        let before = editor.document.clone();
        let start = Pos2::new(180.0, 160.0);
        let end = Pos2::new(230.0, 180.0);
        frame(&ctx, &mut editor, &font, vec![]);
        ctx.memory_mut(|memory| memory.request_focus(egui::Id::new("toolbar")));
        frame(
            &ctx,
            &mut editor,
            &font,
            vec![egui::Event::PointerMoved(start), button(start, true)],
        );
        frame(
            &ctx,
            &mut editor,
            &font,
            vec![egui::Event::PointerMoved(end)],
        );
        frame(&ctx, &mut editor, &font, vec![button(end, false)]);
        assert_eq!(editor.document.items[0].end, Pos2::new(520.0, 460.0));
        let modifiers = egui::Modifiers {
            ctrl: true,
            command: true,
            ..Default::default()
        };
        frame(
            &ctx,
            &mut editor,
            &font,
            vec![
                egui::Event::Key {
                    key: egui::Key::Z,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers,
                },
                egui::Event::Key {
                    key: egui::Key::Z,
                    physical_key: None,
                    pressed: false,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );
        assert_eq!(editor.document, before);
        frame(
            &ctx,
            &mut editor,
            &font,
            vec![
                egui::Event::Key {
                    key: egui::Key::Y,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers,
                },
                egui::Event::Key {
                    key: egui::Key::Y,
                    physical_key: None,
                    pressed: false,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );
        assert_eq!(editor.document.items[0].end, Pos2::new(520.0, 460.0));
    }
}
