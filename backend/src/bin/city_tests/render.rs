//! Software z-buffer renderer for quick PNG previews of a generated mesh
//! (orthographic, flat-shaded, coloured by triangle range like the frontend).
//! No GPU and no extra dependencies: the `image` crate writes the PNG.

use geoprint3d_backend::models::{BoundingBox, TerrainMesh};
use image::{Rgb, RgbImage};

/// Triangle ranges of a generated mesh (see `MeshData`): terrain (water
/// last), then buildings + bridges, then removable supports, then the base.
#[derive(Clone, Copy, Debug)]
pub struct Ranges {
    pub terrain: usize,
    pub water: usize,
    pub buildings: usize,
    pub supports: usize,
}

/// One camera. `az_deg` is the compass direction the camera looks FROM
/// (180 = from the south, looking north), `el_deg` its height above the
/// horizon (90 = straight down). `crop` limits the scene to a window given in
/// fractions of the terrain extent: [x0, y0, x1, y1], x east, y north.
#[derive(Clone, Debug)]
pub struct View {
    pub az_deg: f32,
    pub el_deg: f32,
    pub crop: Option<[f32; 4]>,
    /// Draw only buildings, bridges and supports (profiles of landmarks
    /// otherwise hidden behind the terrain in front of them).
    pub buildings_only: bool,
    pub width: u32,
    pub height: u32,
}

const BACKGROUND: [f32; 3] = [28.0, 32.0, 44.0];
const TERRAIN: [f32; 3] = [109.0, 163.0, 92.0];
const WATER: [f32; 3] = [94.0, 154.0, 206.0];
const BUILDING: [f32; 3] = [222.0, 205.0, 170.0];
const SUPPORT: [f32; 3] = [160.0, 160.0, 168.0];
const BASE: [f32; 3] = [78.0, 80.0, 90.0];
/// Supersampling factor (anti-aliasing by box downsampling).
const SS: u32 = 2;

/// Crop window (fractions of the bbox) around `center` (lat, lon) with a
/// half-side of `radius_m`, clamped to [0, 1].
pub fn crop_around(bbox: &BoundingBox, center: [f64; 2], radius_m: f64) -> [f32; 4] {
    let lat_span = bbox.max_lat - bbox.min_lat;
    let lon_span = bbox.max_lon - bbox.min_lon;
    let mid_lat = ((bbox.min_lat + bbox.max_lat) / 2.0).to_radians();
    let fx = (center[1] - bbox.min_lon) / lon_span;
    let fy = (center[0] - bbox.min_lat) / lat_span;
    let rx = radius_m / (lon_span * 111_320.0 * mid_lat.cos());
    let ry = radius_m / (lat_span * 111_320.0);
    let c = |v: f64| v.clamp(0.0, 1.0) as f32;
    [c(fx - rx), c(fy - ry), c(fx + rx), c(fy + ry)]
}

fn sub(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]]
}

fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn normalize(a: [f32; 3]) -> [f32; 3] {
    let n = dot(a, a).sqrt();
    if n < 1e-12 {
        [0.0, 0.0, 1.0]
    } else {
        [a[0] / n, a[1] / n, a[2] / n]
    }
}

pub fn render(mesh: &TerrainMesh, ranges: Ranges, view: &View) -> RgbImage {
    let tris = &mesh.triangles;
    let v = &mesh.vertices;

    // Terrain extent: the reference frame of crop fractions.
    let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
    for t in &tris[..ranges.terrain.min(tris.len())] {
        for &i in t {
            x0 = x0.min(v[i][0]);
            y0 = y0.min(v[i][1]);
            x1 = x1.max(v[i][0]);
            y1 = y1.max(v[i][1]);
        }
    }
    let window = view.crop.map(|c| {
        [x0 + c[0] * (x1 - x0), y0 + c[1] * (y1 - y0), x0 + c[2] * (x1 - x0), y0 + c[3] * (y1 - y0)]
    });

    let colour_of = |k: usize| -> [f32; 3] {
        let water_start = ranges.terrain - ranges.water;
        if k < water_start {
            TERRAIN
        } else if k < ranges.terrain {
            WATER
        } else if k < ranges.terrain + ranges.buildings {
            BUILDING
        } else if k < ranges.terrain + ranges.buildings + ranges.supports {
            SUPPORT
        } else {
            BASE
        }
    };

    // Camera basis (x east, y north, z up).
    let (az, el) = (view.az_deg.to_radians(), view.el_deg.to_radians());
    let to_cam = [az.sin() * el.cos(), az.cos() * el.cos(), el.sin()];
    let fwd = [-to_cam[0], -to_cam[1], -to_cam[2]];
    let right = [-az.cos(), az.sin(), 0.0];
    let up = cross(right, fwd);
    // Key light from the north-west, high; plus a little headlight.
    let light = normalize([-0.45, 0.35, 0.82]);

    // Select triangles lying entirely inside the crop window (plus a small
    // margin) and project. Testing only the centroid let the long cap
    // triangles of buildings cut by the window poke out as needles.
    let base_start = ranges.terrain + ranges.buildings + ranges.supports;
    let window = window.map(|w| {
        let (mx, my) = (0.06 * (w[2] - w[0]), 0.06 * (w[3] - w[1]));
        [w[0] - mx, w[1] - my, w[2] + mx, w[3] + my]
    });
    let mut selected: Vec<usize> = Vec::with_capacity(tris.len());
    for (k, t) in tris.iter().enumerate() {
        if view.buildings_only && (k < ranges.terrain || k >= base_start) {
            continue;
        }
        if let Some(w) = window {
            if k >= base_start {
                continue; // base walls/bottom only make sense in full views
            }
            let outside = t.iter().any(|&i| v[i][0] < w[0] || v[i][0] > w[2] || v[i][1] < w[1] || v[i][1] > w[3]);
            if outside {
                continue;
            }
        }
        selected.push(k);
    }
    let project = |p: [f32; 3]| -> [f32; 3] { [dot(p, right), dot(p, up), dot(p, fwd)] };
    let (mut sx0, mut sy0, mut sx1, mut sy1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
    for &k in &selected {
        for &i in &tris[k] {
            let s = project(v[i]);
            sx0 = sx0.min(s[0]);
            sy0 = sy0.min(s[1]);
            sx1 = sx1.max(s[0]);
            sy1 = sy1.max(s[1]);
        }
    }

    let (w, h) = (view.width * SS, view.height * SS);
    let margin = 0.03;
    let scale = ((w as f32 * (1.0 - 2.0 * margin)) / (sx1 - sx0).max(1e-6))
        .min((h as f32 * (1.0 - 2.0 * margin)) / (sy1 - sy0).max(1e-6));
    let off_x = (w as f32 - (sx1 - sx0) * scale) / 2.0;
    let off_y = (h as f32 - (sy1 - sy0) * scale) / 2.0;
    let to_px = |s: [f32; 3]| -> [f32; 3] {
        [off_x + (s[0] - sx0) * scale, off_y + (sy1 - s[1]) * scale, s[2]]
    };

    let mut depth = vec![f32::INFINITY; (w * h) as usize];
    let mut colour = vec![BACKGROUND; (w * h) as usize];
    for &k in &selected {
        let t = tris[k];
        let (a, b, c) = (v[t[0]], v[t[1]], v[t[2]]);
        let mut n = normalize(cross(sub(b, a), sub(c, a)));
        if dot(n, to_cam) < 0.0 {
            n = [-n[0], -n[1], -n[2]]; // two-sided lighting
        }
        let shade = 0.32 + 0.53 * dot(n, light).max(0.0) + 0.15 * dot(n, to_cam).max(0.0);
        let base = colour_of(k);
        let rgb = [base[0] * shade, base[1] * shade, base[2] * shade];

        let (pa, pb, pc) = (to_px(project(a)), to_px(project(b)), to_px(project(c)));
        let area = (pb[0] - pa[0]) * (pc[1] - pa[1]) - (pb[1] - pa[1]) * (pc[0] - pa[0]);
        if area.abs() < 1e-9 {
            continue;
        }
        let min_x = pa[0].min(pb[0]).min(pc[0]).floor().max(0.0) as i64;
        let max_x = pa[0].max(pb[0]).max(pc[0]).ceil().min(w as f32 - 1.0) as i64;
        let min_y = pa[1].min(pb[1]).min(pc[1]).floor().max(0.0) as i64;
        let max_y = pa[1].max(pb[1]).max(pc[1]).ceil().min(h as f32 - 1.0) as i64;
        for py in min_y..=max_y {
            for px in min_x..=max_x {
                let (x, y) = (px as f32 + 0.5, py as f32 + 0.5);
                let w0 = ((pb[0] - x) * (pc[1] - y) - (pb[1] - y) * (pc[0] - x)) / area;
                let w1 = ((pc[0] - x) * (pa[1] - y) - (pc[1] - y) * (pa[0] - x)) / area;
                let w2 = 1.0 - w0 - w1;
                if w0 < 0.0 || w1 < 0.0 || w2 < 0.0 {
                    continue;
                }
                let z = w0 * pa[2] + w1 * pb[2] + w2 * pc[2];
                let idx = (py as u32 * w + px as u32) as usize;
                if z < depth[idx] {
                    depth[idx] = z;
                    colour[idx] = rgb;
                }
            }
        }
    }

    // Box-filter down to the output size.
    let mut img = RgbImage::new(view.width, view.height);
    for oy in 0..view.height {
        for ox in 0..view.width {
            let mut acc = [0.0f32; 3];
            for dy in 0..SS {
                for dx in 0..SS {
                    let c = colour[((oy * SS + dy) * w + ox * SS + dx) as usize];
                    acc[0] += c[0];
                    acc[1] += c[1];
                    acc[2] += c[2];
                }
            }
            let n = (SS * SS) as f32;
            let px = |c: f32| (c / n).round().clamp(0.0, 255.0) as u8;
            img.put_pixel(ox, oy, Rgb([px(acc[0]), px(acc[1]), px(acc[2])]));
        }
    }
    img
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crop_is_centred_on_the_landmark_and_clamped() {
        let bbox = BoundingBox { min_lat: 41.0, min_lon: 12.0, max_lat: 41.01, max_lon: 12.02 };
        let c = crop_around(&bbox, [41.005, 12.01], 100.0);
        assert!(((c[0] + c[2]) / 2.0 - 0.5).abs() < 1e-6);
        assert!(((c[1] + c[3]) / 2.0 - 0.5).abs() < 1e-6);
        // 100 m over a 1113 m tall bbox ≈ 0.09 half-height.
        assert!((c[3] - 0.5 - 0.0898).abs() < 1e-3);
        let edge = crop_around(&bbox, [41.0, 12.0], 500.0);
        assert_eq!((edge[0], edge[1]), (0.0, 0.0));
    }

    #[test]
    fn renders_a_single_triangle_in_building_colour() {
        let mesh = TerrainMesh {
            vertices: vec![[0.0, 0.0, 0.0], [10.0, 0.0, 0.0], [0.0, 10.0, 0.0], [3.0, 3.0, 1.0], [8.0, 3.0, 1.0], [3.0, 8.0, 1.0]],
            triangles: vec![[0, 1, 2], [3, 4, 5]],
        };
        let ranges = Ranges { terrain: 1, water: 0, buildings: 1, supports: 0 };
        let view = View { az_deg: 180.0, el_deg: 90.0, crop: None, buildings_only: false, width: 40, height: 40 };
        let img = render(&mesh, ranges, &view);
        let centre = img.get_pixel(20, 20);
        assert!(centre[0] > centre[1], "building triangle (tan) drawn over the terrain (green)");
    }
}
