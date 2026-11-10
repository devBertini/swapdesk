//! The icon paths of the design (SVG `d` attributes: M L H V C A Z, absolute and relative) as moves, lines and cubic
//! curves, for graphics that have no SVG-style elliptical arc (CoreGraphics). Arcs become cubics of at most 90 degrees.

/// One step of a path, in the caller's coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Seg {
    Move(f32, f32),
    Line(f32, f32),
    Cubic(f32, f32, f32, f32, f32, f32),
    Close,
}

/// Parses `d` from a 24-unit viewBox placed at `(ox, oy)` and scaled by `s`. Unknown commands end the path.
pub fn parse(d: &str, ox: f32, oy: f32, s: f32) -> Vec<Seg> {
    let mut out = Vec::new();
    let pt = |x: f32, y: f32| (ox + x * s, oy + y * s);
    let tokens = tokenize(d);
    let arity = |c: u8| match c.to_ascii_lowercase() {
        b'm' | b'l' => 2,
        b'h' | b'v' => 1,
        b'c' => 6,
        b'a' => 7,
        _ => 0,
    };
    let (mut cx, mut cy, mut sx, mut sy) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
    let mut cmd = b'M';
    let mut nums: Vec<f32> = Vec::new();
    let mut idx = 0;
    while idx < tokens.len() {
        if let (Some(c), _) = tokens[idx] {
            cmd = c;
            idx += 1;
            if matches!(c, b'z' | b'Z') {
                out.push(Seg::Close);
                cx = sx;
                cy = sy;
                continue;
            }
        }
        let n = arity(cmd);
        if n == 0 {
            break;
        }
        nums.clear();
        while nums.len() < n && idx < tokens.len() && tokens[idx].0.is_none() {
            nums.push(tokens[idx].1);
            idx += 1;
        }
        if nums.len() < n {
            break;
        }
        let rel = cmd.is_ascii_lowercase();
        let (bx, by) = if rel { (cx, cy) } else { (0.0, 0.0) };
        match cmd.to_ascii_lowercase() {
            b'm' => {
                cx = bx + nums[0];
                cy = by + nums[1];
                sx = cx;
                sy = cy;
                let (x, y) = pt(cx, cy);
                out.push(Seg::Move(x, y));
                // Further pairs after a move are implicit line-tos.
                cmd = if rel { b'l' } else { b'L' };
            }
            b'l' | b'h' | b'v' => {
                match cmd.to_ascii_lowercase() {
                    b'l' => {
                        cx = bx + nums[0];
                        cy = by + nums[1];
                    }
                    b'h' => cx = bx + nums[0],
                    _ => cy = by + nums[0],
                }
                let (x, y) = pt(cx, cy);
                out.push(Seg::Line(x, y));
            }
            b'c' => {
                let (x1, y1) = pt(bx + nums[0], by + nums[1]);
                let (x2, y2) = pt(bx + nums[2], by + nums[3]);
                cx = bx + nums[4];
                cy = by + nums[5];
                let (x, y) = pt(cx, cy);
                out.push(Seg::Cubic(x1, y1, x2, y2, x, y));
            }
            b'a' => {
                let from = pt(cx, cy);
                cx = bx + nums[5];
                cy = by + nums[6];
                let to = pt(cx, cy);
                let radii = (nums[0] * s, nums[1] * s);
                arc_to_cubics(from, to, radii, nums[2], nums[3] != 0.0, nums[4] != 0.0, &mut out);
            }
            _ => {}
        }
    }
    out
}

fn tokenize(d: &str) -> Vec<(Option<u8>, f32)> {
    let bytes = d.as_bytes();
    let mut i = 0;
    let mut tokens = Vec::new();
    while i < bytes.len() {
        let ch = bytes[i];
        if ch.is_ascii_alphabetic() {
            tokens.push((Some(ch), 0.0));
            i += 1;
        } else if ch == b'-' || ch == b'.' || ch.is_ascii_digit() {
            let start = i;
            i += 1;
            let mut seen_dot = ch == b'.';
            while i < bytes.len() {
                let c = bytes[i];
                if c.is_ascii_digit() {
                    i += 1;
                } else if c == b'.' && !seen_dot {
                    seen_dot = true;
                    i += 1;
                } else {
                    break;
                }
            }
            tokens.push((None, d[start..i].parse().unwrap_or(0.0)));
        } else {
            i += 1;
        }
    }
    tokens
}

/// An arc of `sweep_deg` degrees from `start_deg` around `center` (y grows down, so positive sweeps turn clockwise on
/// screen), as a move and cubics.
pub fn arc(center: (f32, f32), radius: f32, start_deg: f32, sweep_deg: f32) -> Vec<Seg> {
    let at = |deg: f32| {
        let r = deg.to_radians();
        (center.0 + radius * r.cos(), center.1 + radius * r.sin())
    };
    let (x, y) = at(start_deg);
    let mut out = vec![Seg::Move(x, y)];
    push_center_arc(center, (radius, radius), 0.0, start_deg.to_radians(), sweep_deg.to_radians(), &mut out);
    out
}

/// SVG endpoint arc to center form (SVG 1.1 appendix F.6.5), then cubics.
#[allow(clippy::too_many_arguments)] // the arc's endpoint parameters as SVG names them
fn arc_to_cubics(
    from: (f32, f32),
    to: (f32, f32),
    radii: (f32, f32),
    rotation_deg: f32,
    large: bool,
    sweep: bool,
    out: &mut Vec<Seg>,
) {
    let (mut rx, mut ry) = (radii.0.abs(), radii.1.abs());
    if from == to {
        return;
    }
    if rx == 0.0 || ry == 0.0 {
        out.push(Seg::Line(to.0, to.1));
        return;
    }
    let phi = rotation_deg.to_radians();
    let (sin_phi, cos_phi) = phi.sin_cos();
    let (dx, dy) = ((from.0 - to.0) / 2.0, (from.1 - to.1) / 2.0);
    let x1p = cos_phi * dx + sin_phi * dy;
    let y1p = -sin_phi * dx + cos_phi * dy;
    let lambda = (x1p * x1p) / (rx * rx) + (y1p * y1p) / (ry * ry);
    if lambda > 1.0 {
        rx *= lambda.sqrt();
        ry *= lambda.sqrt();
    }
    let num = rx * rx * ry * ry - rx * rx * y1p * y1p - ry * ry * x1p * x1p;
    let den = rx * rx * y1p * y1p + ry * ry * x1p * x1p;
    let mut coef = (num / den).max(0.0).sqrt();
    if large == sweep {
        coef = -coef;
    }
    let cxp = coef * rx * y1p / ry;
    let cyp = -coef * ry * x1p / rx;
    let cx = cos_phi * cxp - sin_phi * cyp + (from.0 + to.0) / 2.0;
    let cy = sin_phi * cxp + cos_phi * cyp + (from.1 + to.1) / 2.0;
    let angle = |ux: f32, uy: f32, vx: f32, vy: f32| {
        let a = (ux * vy - uy * vx).atan2(ux * vx + uy * vy);
        if a.is_nan() { 0.0 } else { a }
    };
    let theta1 = angle(1.0, 0.0, (x1p - cxp) / rx, (y1p - cyp) / ry);
    let mut delta = angle((x1p - cxp) / rx, (y1p - cyp) / ry, (-x1p - cxp) / rx, (-y1p - cyp) / ry);
    let tau = std::f32::consts::TAU;
    if !sweep && delta > 0.0 {
        delta -= tau;
    } else if sweep && delta < 0.0 {
        delta += tau;
    }
    push_center_arc((cx, cy), (rx, ry), phi, theta1, delta, out);
    // The last point is the endpoint exactly, whatever rounding did.
    if let Some(Seg::Cubic(_, _, _, _, x, y)) = out.last_mut() {
        *x = to.0;
        *y = to.1;
    }
}

/// Cubics for an elliptical arc in center form, at most 90 degrees each.
fn push_center_arc(c: (f32, f32), r: (f32, f32), phi: f32, theta1: f32, delta: f32, out: &mut Vec<Seg>) {
    let n = (delta.abs() / std::f32::consts::FRAC_PI_2).ceil().max(1.0) as usize;
    let step = delta / n as f32;
    let k = 4.0 / 3.0 * (step / 4.0).tan();
    let (sin_phi, cos_phi) = phi.sin_cos();
    let map =
        |x: f32, y: f32| (c.0 + cos_phi * r.0 * x - sin_phi * r.1 * y, c.1 + sin_phi * r.0 * x + cos_phi * r.1 * y);
    let mut t = theta1;
    for _ in 0..n {
        let (s1, c1) = t.sin_cos();
        let (s2, c2) = (t + step).sin_cos();
        let p1 = map(c1 - k * s1, s1 + k * c1);
        let p2 = map(c2 + k * s2, s2 - k * c2);
        let p3 = map(c2, s2);
        out.push(Seg::Cubic(p1.0, p1.1, p2.0, p2.1, p3.0, p3.1));
        t += step;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn last_point(segs: &[Seg]) -> (f32, f32) {
        match segs.iter().rev().find(|s| !matches!(s, Seg::Close)) {
            Some(Seg::Move(x, y) | Seg::Line(x, y) | Seg::Cubic(_, _, _, _, x, y)) => (*x, *y),
            _ => panic!("no point"),
        }
    }

    #[test]
    fn lines_moves_and_relative_commands_land_where_svg_puts_them() {
        let segs = parse("M5 12.5l4.5 4.5L19 7.5", 0.0, 0.0, 1.0);
        assert_eq!(segs, vec![Seg::Move(5.0, 12.5), Seg::Line(9.5, 17.0), Seg::Line(19.0, 7.5)]);
        let segs = parse("M4 9V4h5", 10.0, 20.0, 2.0);
        assert_eq!(segs, vec![Seg::Move(18.0, 38.0), Seg::Line(18.0, 28.0), Seg::Line(28.0, 28.0)]);
        // Implicit line-tos after a move, and a close returns to the start.
        let segs = parse("M6 6 18 18zM18 6L6 18", 0.0, 0.0, 1.0);
        assert_eq!(segs[1], Seg::Line(18.0, 18.0));
        assert_eq!(segs[2], Seg::Close);
        assert_eq!(segs[3], Seg::Move(18.0, 6.0));
    }

    #[test]
    fn an_arc_ends_at_its_endpoint_and_stays_on_its_circle() {
        // A half circle of radius 4 from (8, 12) to (16, 12), sweeping clockwise (through y = 8 with y down).
        let segs = parse("M8 12a4 4 0 0 1 8 0", 0.0, 0.0, 1.0);
        assert_eq!(last_point(&segs), (16.0, 12.0));
        let cubics: Vec<_> = segs.iter().filter(|s| matches!(s, Seg::Cubic(..))).collect();
        assert_eq!(cubics.len(), 2, "180 degrees in 90 degree pieces");
        if let Seg::Cubic(_, _, _, _, x, y) = cubics[0] {
            assert!((x - 12.0).abs() < 1e-3 && (y - 8.0).abs() < 1e-3, "the top of the circle: {x} {y}");
        }
        // The rounded rectangle icon closes back where it started.
        let segs = parse(&crate::rrect_path(5.0, 11.0, 14.0, 9.0, 2.0), 0.0, 0.0, 1.0);
        assert_eq!(segs.last(), Some(&Seg::Close));
    }

    #[test]
    fn a_center_arc_of_three_quarters_has_three_pieces() {
        let segs = arc((10.0, 10.0), 5.0, 0.0, 270.0);
        assert_eq!(segs[0], Seg::Move(15.0, 10.0));
        assert_eq!(segs.len(), 4);
        let (x, y) = last_point(&segs);
        assert!((x - 10.0).abs() < 1e-3 && (y - 5.0).abs() < 1e-3, "{x} {y}");
    }
}
