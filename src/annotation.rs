//! Editable annotations use original image pixels, independent of window DPI and preview size.
use std::collections::VecDeque;

use eframe::egui::{Color32, Pos2, Rect, Vec2};

use crate::geometry::CropRect;

const UNDO_LIMIT: usize = 100;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Tool {
    #[default]
    Select,
    Line,
    Rectangle,
    Ellipse,
    Arrow,
    Text,
}

impl Tool {
    pub const ALL: [(Self, &'static str); 6] = [
        (Self::Select, "選取"),
        (Self::Line, "直線"),
        (Self::Rectangle, "方形"),
        (Self::Ellipse, "橢圓"),
        (Self::Arrow, "箭頭"),
        (Self::Text, "文字"),
    ];
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Style {
    pub color: Color32,
    pub width: f32,
    pub font_size: f32,
    pub filled: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Annotation {
    pub tool: Tool,
    pub start: Pos2,
    pub end: Pos2,
    pub style: Style,
    pub text: String,
}

impl Annotation {
    pub fn new(tool: Tool, start: Pos2, style: Style) -> Self {
        Self {
            tool,
            start,
            end: start,
            style,
            text: String::new(),
        }
    }

    pub fn set_end(&mut self, mut end: Pos2, square: bool, bounds: Rect) {
        end = end.clamp(bounds.min, bounds.max);
        if square && matches!(self.tool, Tool::Rectangle | Tool::Ellipse) {
            let delta = end - self.start;
            let x_limit = if delta.x < 0.0 {
                self.start.x - bounds.left()
            } else {
                bounds.right() - self.start.x
            };
            let y_limit = if delta.y < 0.0 {
                self.start.y - bounds.top()
            } else {
                bounds.bottom() - self.start.y
            };
            let side = delta.x.abs().max(delta.y.abs()).min(x_limit).min(y_limit);
            end = self.start
                + Vec2::new(
                    if delta.x < 0.0 { -side } else { side },
                    if delta.y < 0.0 { -side } else { side },
                );
        }
        self.end = end;
    }

    /// Paths are shared by hit testing and both rendering targets.
    pub fn paths(&self) -> Vec<Vec<Pos2>> {
        match self.tool {
            Tool::Line => vec![vec![self.start, self.end]],
            Tool::Arrow => {
                let delta = self.end - self.start;
                let length = delta.length();
                if length < 0.01 {
                    return Vec::new();
                }
                let direction = delta / length;
                let head = (self.style.width * 4.0).max(12.0).min(length * 0.6);
                let base = self.end - direction * head;
                let wing = Vec2::new(-direction.y, direction.x) * head * 0.45;
                vec![
                    vec![self.start, self.end],
                    vec![base + wing, self.end, base - wing],
                ]
            }
            Tool::Rectangle => {
                let r = Rect::from_two_pos(self.start, self.end);
                vec![vec![
                    r.left_top(),
                    r.right_top(),
                    r.right_bottom(),
                    r.left_bottom(),
                    r.left_top(),
                ]]
            }
            Tool::Ellipse => {
                let r = Rect::from_two_pos(self.start, self.end);
                // Sub-pixel chord error, even on large screenshots.
                let count = ((r.width().max(r.height()) * 0.5).sqrt() * 8.0)
                    .ceil()
                    .clamp(32.0, 2048.0) as usize;
                vec![
                    (0..=count)
                        .map(|i| {
                            let angle = i as f32 / count as f32 * std::f32::consts::TAU;
                            r.center()
                                + Vec2::new(
                                    angle.cos() * r.width() * 0.5,
                                    angle.sin() * r.height() * 0.5,
                                )
                        })
                        .collect(),
                ]
            }
            _ => Vec::new(),
        }
    }

    pub fn hit(&self, point: Pos2, tolerance: f32, text_size: Vec2) -> bool {
        if self.tool == Tool::Text {
            return Rect::from_min_size(self.start, text_size)
                .expand(tolerance)
                .contains(point);
        }
        if self.style.filled {
            let rect = Rect::from_two_pos(self.start, self.end);
            if self.tool == Tool::Rectangle && rect.contains(point) {
                return true;
            }
            if self.tool == Tool::Ellipse && rect.width() > 0.0 && rect.height() > 0.0 {
                let delta = (point - rect.center()) / (rect.size() * 0.5);
                if delta.length_sq() <= 1.0 {
                    return true;
                }
            }
        }
        self.paths().iter().any(|path| {
            path.windows(2).any(|pair| {
                segment_distance(point, pair[0], pair[1]) <= tolerance + self.style.width * 0.5
            })
        })
    }

    pub fn translate(&mut self, delta: Vec2) {
        self.start += delta;
        self.end += delta;
    }
}

fn segment_distance(p: Pos2, a: Pos2, b: Pos2) -> f32 {
    let ab = b - a;
    let t = ((p - a).dot(ab) / ab.length_sq().max(0.0001)).clamp(0.0, 1.0);
    p.distance(a + ab * t)
}

#[derive(Clone, Debug, PartialEq)]
pub struct Document {
    pub items: Vec<Annotation>,
    pub view: CropRect,
}

pub struct Editor {
    pub document: Document,
    saved: Document,
    undo: VecDeque<Document>,
    redo: Vec<Document>,
    pub selected: Option<usize>,
    pub tool: Tool,
    pub style: Style,
    pub draft: Option<Annotation>,
    pub moving: Option<(Pos2, Document)>,
    pub resizing: Option<ResizeDrag>,
    pub text_edit: Option<TextEdit>,
    pub version: u64,
}

pub struct ResizeDrag {
    pub handle: crate::annotation_resize::Handle,
    pub bounds: Rect,
    pub before: Document,
    pub offset: Vec2,
}

pub struct TextEdit {
    pub item: Annotation,
    pub index: Option<usize>,
    pub focus: bool,
    pub composing: bool,
}

impl Editor {
    pub fn new(width: u32, height: u32, style: Style) -> Self {
        let document = Document {
            items: Vec::new(),
            view: CropRect {
                x: 0,
                y: 0,
                width,
                height,
            },
        };
        Self {
            saved: document.clone(),
            document,
            undo: VecDeque::new(),
            redo: Vec::new(),
            selected: None,
            tool: Tool::Select,
            style,
            draft: None,
            moving: None,
            resizing: None,
            text_edit: None,
            version: 0,
        }
    }

    pub fn dirty(&self) -> bool {
        self.document != self.saved
            || self.text_edit.as_ref().is_some_and(|edit| {
                if let Some(index) = edit.index {
                    self.document.items.get(index) != Some(&edit.item)
                } else {
                    !edit.item.text.trim().is_empty()
                }
            })
            || self.draft.is_some()
    }

    pub fn saved(&mut self) {
        self.saved = self.document.clone();
    }
    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }
    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }
    pub fn repaint(&mut self) {
        self.version = self.version.wrapping_add(1);
    }

    pub fn commit(&mut self, before: Document) {
        if before != self.document {
            self.undo.push_back(before);
            if self.undo.len() > UNDO_LIMIT {
                self.undo.pop_front();
            }
            self.redo.clear();
            self.repaint();
        }
    }

    pub fn undo(&mut self) {
        self.cancel_gesture();
        if let Some(previous) = self.undo.pop_back() {
            self.redo
                .push(std::mem::replace(&mut self.document, previous));
            self.selected = None;
            self.repaint();
        }
    }

    pub fn redo(&mut self) {
        self.cancel_gesture();
        if let Some(next) = self.redo.pop() {
            self.undo
                .push_back(std::mem::replace(&mut self.document, next));
            self.selected = None;
            self.repaint();
        }
    }

    pub fn delete(&mut self) {
        self.cancel_gesture();
        if let Some(index) = self
            .selected
            .take()
            .filter(|i| *i < self.document.items.len())
        {
            let before = self.document.clone();
            self.document.items.remove(index);
            self.commit(before);
        }
    }

    pub fn finish_text(&mut self) {
        if let Some(edit) = self.text_edit.take() {
            let before = self.document.clone();
            if edit.item.text.trim().is_empty() {
                self.repaint();
                return;
            }
            if let Some(index) = edit.index {
                self.document.items[index] = edit.item;
                self.selected = Some(index);
            } else {
                self.selected = Some(self.document.items.len());
                self.document.items.push(edit.item);
            }
            self.commit(before);
            self.repaint();
        }
    }

    pub fn finish_gesture(&mut self) {
        if let Some(item) = self.draft.take() {
            if item.start.distance(item.end) >= 1.0
                && (!matches!(item.tool, Tool::Rectangle | Tool::Ellipse)
                    || (item.end.x - item.start.x)
                        .abs()
                        .min((item.end.y - item.start.y).abs())
                        >= 1.0)
            {
                let before = self.document.clone();
                self.selected = Some(self.document.items.len());
                self.document.items.push(item);
                self.commit(before);
            }
            self.repaint();
        }
        if let Some((_, before)) = self.moving.take() {
            self.commit(before);
        }
        if let Some(drag) = self.resizing.take() {
            self.commit(drag.before);
        }
    }

    pub fn cancel_gesture(&mut self) {
        if let Some((_, before)) = self.moving.take() {
            self.document = before;
        }
        if let Some(drag) = self.resizing.take() {
            self.document = drag.before;
            if let Some(index) = self.selected {
                self.style = self.document.items[index].style;
            }
        }
        self.draft = None;
        self.text_edit = None;
        self.repaint();
    }

    /// Items in paint order, including the open text editor and the shape being drawn.
    /// Borrows instead of cloning: the preview calls this on every drag frame.
    pub fn visible_items(&self) -> impl Iterator<Item = &Annotation> {
        let edit = self.text_edit.as_ref();
        let replaced = edit.and_then(|edit| edit.index.map(|index| (index, &edit.item)));
        self.document
            .items
            .iter()
            .enumerate()
            .map(move |(index, item)| match replaced {
                Some((replaced, edited)) if replaced == index => edited,
                _ => item,
            })
            .chain(
                edit.filter(|edit| edit.index.is_none())
                    .map(|edit| &edit.item),
            )
            .chain(&self.draft)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn style() -> Style {
        Style {
            color: Color32::RED,
            width: 3.0,
            font_size: 28.0,
            filled: false,
        }
    }

    #[test]
    fn history_tracks_saved_state_and_crop_without_flattening() {
        let mut editor = Editor::new(800, 600, style());
        let before = editor.document.clone();
        editor
            .document
            .items
            .push(Annotation::new(Tool::Arrow, Pos2::new(10.0, 20.0), style()));
        editor.commit(before);
        assert!(editor.dirty());
        editor.saved();
        let before = editor.document.clone();
        editor.document.view = CropRect {
            x: 50,
            y: 60,
            width: 300,
            height: 200,
        };
        editor.commit(before);
        editor.undo();
        assert!(!editor.dirty());
        assert_eq!(editor.document.items[0].start, Pos2::new(10.0, 20.0));
        editor.redo();
        assert_eq!(editor.document.view.x, 50);
        assert!(editor.dirty());
    }

    #[test]
    fn outline_hit_testing_does_not_select_empty_centers() {
        let mut item = Annotation::new(Tool::Ellipse, Pos2::ZERO, style());
        item.end = Pos2::new(100.0, 80.0);
        assert!(item.hit(Pos2::new(50.0, 0.0), 2.0, Vec2::ZERO));
        assert!(!item.hit(Pos2::new(50.0, 40.0), 2.0, Vec2::ZERO));
        item.tool = Tool::Arrow;
        item.start = item.end;
        assert!(item.paths().is_empty());
    }

    #[test]
    fn square_stays_square_at_image_edge_in_reverse_drag() {
        let mut item = Annotation::new(Tool::Rectangle, Pos2::new(80.0, 70.0), style());
        item.set_end(
            Pos2::new(10.0, 20.0),
            true,
            Rect::from_min_max(Pos2::ZERO, Pos2::new(100.0, 100.0)),
        );
        assert_eq!(
            (item.end.x - item.start.x).abs(),
            (item.end.y - item.start.y).abs()
        );
        assert_eq!(item.end, Pos2::new(10.0, 0.0));
    }

    #[test]
    fn cancelled_move_and_text_edit_preserve_document() {
        let mut editor = Editor::new(100, 100, style());
        editor
            .document
            .items
            .push(Annotation::new(Tool::Text, Pos2::ZERO, style()));
        let before = editor.document.clone();
        editor.moving = Some((Pos2::ZERO, before.clone()));
        editor.document.items[0].translate(Vec2::new(20.0, 30.0));
        editor.cancel_gesture();
        assert_eq!(editor.document, before);
        assert!(!editor.can_undo());
    }

    #[test]
    fn text_reediting_and_deletion_are_undoable_and_clear_stale_editors() {
        let mut editor = Editor::new(800, 600, style());
        let mut item = Annotation::new(Tool::Text, Pos2::new(20.0, 30.0), style());
        item.text = "原始文字".into();
        editor.document.items.push(item.clone());
        editor.saved();
        item.text = "修改文字\n第二行".into();
        editor.text_edit = Some(TextEdit {
            item: item.clone(),
            index: Some(0),
            focus: true,
            composing: false,
        });
        assert!(editor.dirty());
        editor.finish_text();
        assert_eq!(editor.document.items[0].text, "修改文字\n第二行");
        editor.undo();
        assert!(!editor.dirty());
        editor.redo();
        editor.selected = Some(0);
        editor.text_edit = Some(TextEdit {
            item,
            index: Some(0),
            focus: true,
            composing: false,
        });
        editor.delete();
        assert!(editor.text_edit.is_none());
        assert!(editor.document.items.is_empty());
        editor.undo();
        assert_eq!(editor.document.items.len(), 1);
    }
}
