//! Small icons, drawn with lines so they need no font coverage.

use egui::{Color32, Painter, Pos2, Rect, Shape, Stroke, Vec2, pos2};

/// A clock: the history icon.
pub(crate) fn clock(painter: &Painter, center: Pos2, color: Color32) {
    let stroke = Stroke::new(1.5, color);
    painter.circle_stroke(center, 6.0, stroke);
    painter.line_segment([center + Vec2::new(0.0, -3.5), center], stroke);
    painter.line_segment([center, center + Vec2::new(2.3, 1.6)], stroke);
}

/// A plus.
pub(crate) fn plus(painter: &Painter, center: Pos2, half: f32, color: Color32) {
    let stroke = Stroke::new(1.5, color);
    painter.line_segment(
        [center - Vec2::new(half, 0.0), center + Vec2::new(half, 0.0)],
        stroke,
    );
    painter.line_segment(
        [center - Vec2::new(0.0, half), center + Vec2::new(0.0, half)],
        stroke,
    );
}

/// A cross, for close.
pub(crate) fn cross(painter: &Painter, center: Pos2, half: f32, color: Color32) {
    let stroke = Stroke::new(1.4, color);
    painter.line_segment(
        [
            center + Vec2::new(-half, -half),
            center + Vec2::new(half, half),
        ],
        stroke,
    );
    painter.line_segment(
        [
            center + Vec2::new(-half, half),
            center + Vec2::new(half, -half),
        ],
        stroke,
    );
}

/// A magnifier.
pub(crate) fn search(painter: &Painter, center: Pos2, color: Color32) {
    let stroke = Stroke::new(1.5, color);
    painter.circle_stroke(center + Vec2::new(-1.0, -1.0), 4.5, stroke);
    painter.line_segment(
        [center + Vec2::new(2.3, 2.3), center + Vec2::new(6.0, 6.0)],
        stroke,
    );
}

/// A window with its left third marked: the sidebar toggle.
pub(crate) fn sidebar(painter: &Painter, center: Pos2, color: Color32) {
    let outer = Rect::from_center_size(center, Vec2::new(15.0, 12.0));
    painter.rect_stroke(
        outer,
        2.0,
        Stroke::new(1.4, color),
        egui::StrokeKind::Inside,
    );
    let x = outer.min.x + 5.5;
    painter.line_segment(
        [pos2(x, outer.min.y), pos2(x, outer.max.y)],
        Stroke::new(1.4, color),
    );
}

/// A small filled square: stop.
pub(crate) fn stop(painter: &Painter, center: Pos2, half: f32, color: Color32) {
    painter.rect_filled(
        egui::Rect::from_center_size(center, Vec2::splat(half * 2.0)),
        1.5,
        color,
    );
}

/// A filled triangle: the twisty. `open` points down, otherwise right.
pub(crate) fn twisty(painter: &Painter, center: Pos2, open: bool, color: Color32) {
    let points = if open {
        vec![
            center + Vec2::new(-3.5, -2.0),
            center + Vec2::new(3.5, -2.0),
            center + Vec2::new(0.0, 3.0),
        ]
    } else {
        vec![
            center + Vec2::new(-2.0, -3.5),
            center + Vec2::new(-2.0, 3.5),
            center + Vec2::new(3.0, 0.0),
        ]
    };
    painter.add(Shape::convex_polygon(points, color, Stroke::NONE));
}

/// A folder: a tab on a body, or, `open`, the front flap tilted down.
pub(crate) fn folder(painter: &Painter, center: Pos2, open: bool, color: Color32) {
    let stroke = Stroke::new(1.3, color);
    let left = center.x - 7.5;
    let right = center.x + 7.5;
    let top = center.y - 5.5;
    let bottom = center.y + 5.5;
    // The back, with its tab.
    let mut back = vec![
        pos2(left, bottom),
        pos2(left, top),
        pos2(left + 5.0, top),
        pos2(left + 6.5, top + 2.0),
        pos2(right, top + 2.0),
    ];
    if open {
        back.push(pos2(right, top + 4.0));
        painter.add(Shape::line(back, stroke));
        // The front, leaning out over the bottom.
        let front = vec![
            pos2(left, bottom),
            pos2(left + 2.5, top + 4.5),
            pos2(right + 1.5, top + 4.5),
            pos2(right - 1.0, bottom),
        ];
        painter.add(Shape::closed_line(front, stroke));
    } else {
        back.push(pos2(right, bottom));
        painter.add(Shape::closed_line(back, stroke));
        painter.line_segment([pos2(left, top + 4.0), pos2(right, top + 4.0)], stroke);
    }
}

/// Two overlapping sheets: copy.
pub(crate) fn copy(painter: &Painter, center: Pos2, color: Color32, fill: Color32) {
    let stroke = Stroke::new(1.3, color);
    let back = Rect::from_min_size(center + Vec2::new(-4.5, -4.5), Vec2::new(7.0, 8.0));
    let front = back.translate(Vec2::new(2.5, 2.5));
    painter.rect_stroke(back, 1.5, stroke, egui::StrokeKind::Middle);
    painter.rect_filled(front, 1.5, fill);
    painter.rect_stroke(front, 1.5, stroke, egui::StrokeKind::Middle);
}

/// A check mark.
pub(crate) fn check(painter: &Painter, center: Pos2, color: Color32) {
    let stroke = Stroke::new(1.8, color);
    let a = center + Vec2::new(-3.5, 0.5);
    let b = center + Vec2::new(-1.0, 3.0);
    let c = center + Vec2::new(4.0, -3.5);
    painter.line_segment([a, b], stroke);
    painter.line_segment([b, c], stroke);
}

/// A robot head, for "generate a title with a utility session".
pub(crate) fn robot(painter: &Painter, center: Pos2, color: Color32) {
    let stroke = Stroke::new(1.3, color);
    let head = Rect::from_center_size(center + Vec2::new(0.0, 1.0), Vec2::new(11.0, 8.0));
    painter.rect_stroke(head, 2.0, stroke, egui::StrokeKind::Middle);
    painter.line_segment(
        [center + Vec2::new(0.0, -3.0), center + Vec2::new(0.0, -5.0)],
        stroke,
    );
    painter.circle_filled(center + Vec2::new(0.0, -5.5), 1.0, color);
    painter.circle_filled(center + Vec2::new(-2.2, 0.5), 1.0, color);
    painter.circle_filled(center + Vec2::new(2.2, 0.5), 1.0, color);
    painter.line_segment(
        [center + Vec2::new(-2.0, 3.0), center + Vec2::new(2.0, 3.0)],
        stroke,
    );
}

/// A gear: a ring with eight teeth.
/// A lightning bolt: the project's operations.
pub(crate) fn bolt(painter: &Painter, center: Pos2, color: Color32) {
    let points = [
        center + Vec2::new(1.5, -7.0),
        center + Vec2::new(-4.0, 1.0),
        center + Vec2::new(-0.2, 1.0),
        center + Vec2::new(-1.5, 7.0),
        center + Vec2::new(4.0, -1.0),
        center + Vec2::new(0.2, -1.0),
    ];
    painter.add(egui::Shape::closed_line(
        points.to_vec(),
        Stroke::new(1.3, color),
    ));
}

pub(crate) fn gear(painter: &Painter, center: Pos2, color: Color32) {
    let stroke = Stroke::new(1.4, color);
    painter.circle_stroke(center, 3.0, stroke);
    for step in 0..8 {
        let angle = step as f32 * std::f32::consts::FRAC_PI_4;
        let dir = Vec2::new(angle.cos(), angle.sin());
        painter.line_segment([center + dir * 5.0, center + dir * 7.0], stroke);
    }
    painter.circle_stroke(center, 5.0, Stroke::new(1.0, color));
}
