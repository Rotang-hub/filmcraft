//! Fork: read Premiere Pro effect preset files (`.prfpset`, the XML a Presets bin exports).
//!
//! Each preset becomes an [`EffectPreset`]: every effect whose FilmCraft twin exists is converted
//! (parameters by name or, where Premiere leaves them unnamed, by parameter id), with its keyframes
//! and its masks. Effects FilmCraft does not have are left out.
//!
//! - **Geometry.** Premiere stores points normalized to the frame; they become pixels of a 1920×1080
//!   frame and the preset's `source_size` says so, so applying scales them to the target clip.
//! - **Keyframes.** Times are relative to the preset's anchor in point. Bézier keyframes keep their
//!   influence and their speed (as handle heights, so overshoots stay), linear and hold stay so.
//! - **Hostile input.** Sizes are capped, references are followed with a depth limit, and nothing in
//!   the file can make the reader panic.

use std::collections::HashMap;

use filmcraft_geom::Vec2;
use filmcraft_project::effect::{EffectDef, ParamDef};
use filmcraft_project::keyframe::{Interpolation, Keyframe, Param, ParamValue};
use filmcraft_project::mask::{Mask, MaskPath, MaskVertex};
use filmcraft_project::{EffectInstance, ParamKind, find_effect};
use filmcraft_time::{TICKS_PER_SECOND, Tick};

use crate::presets::{EffectPreset, KeyframeMode};

/// The frame normalized points are converted to (and the presets' `source_size`).
const FRAME: (f64, f64) = (1920.0, 1080.0);
const MAX_BYTES: usize = 256 << 20;
const MAX_PRESETS: usize = 10_000;
const MAX_KEYFRAMES: usize = 100_000;
const MAX_DEPTH: usize = 32;

/// Whether `bytes` look like a Premiere preset file.
pub fn is_premiere(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(512)];
    String::from_utf8_lossy(head).contains("<PremiereData")
}

/// The presets in a Premiere `.prfpset` file.
pub fn parse(bytes: &[u8]) -> Result<Vec<EffectPreset>, String> {
    if bytes.len() > MAX_BYTES {
        return Err("the preset file is too large".into());
    }
    let text = std::str::from_utf8(bytes).map_err(|_| "the preset file is not UTF-8 text".to_string())?;
    let opts = roxmltree::ParsingOptions { allow_dtd: false, nodes_limit: 50_000_000, ..Default::default() };
    let doc = roxmltree::Document::parse_with_options(text, opts).map_err(|e| format!("not a Premiere preset file: {e}"))?;
    let file = File::new(&doc);
    let mut out = Vec::new();
    let tree = doc.root_element().children().find(|n| n.has_tag_name("Tree") && n.attribute("ObjectID").is_some());
    let root = tree.and_then(|t| child(t, "RootBin")).and_then(|r| file.get(r));
    match root {
        Some(r) => file.walk(r, &mut Vec::new(), &mut out, 0),
        None => return Err("not a Premiere preset file (no presets bin)".into()),
    }
    if out.is_empty() {
        return Err("the file has no presets with effects FilmCraft has".into());
    }
    Ok(out)
}

/// The objects of the file by `ObjectID`.
struct File<'a, 'input> {
    objects: HashMap<&'a str, roxmltree::Node<'a, 'input>>,
    /// Bins already walked: a bin listed twice (or inside itself) is read once.
    seen: std::cell::RefCell<std::collections::HashSet<&'a str>>,
}

type Node<'a, 'input> = roxmltree::Node<'a, 'input>;

fn child<'a, 'input>(n: Node<'a, 'input>, tag: &str) -> Option<Node<'a, 'input>> {
    n.children().find(|c| c.has_tag_name(tag))
}

fn text<'a>(n: Node<'a, '_>, tag: &str) -> Option<&'a str> {
    child(n, tag).and_then(|c| c.text())
}

fn reference<'a>(n: Node<'a, '_>) -> Option<&'a str> {
    n.attribute("ObjectRef")
}

impl<'a, 'input> File<'a, 'input> {
    fn new(doc: &'a roxmltree::Document<'input>) -> Self {
        let objects = doc.root_element().children().filter_map(|n| n.attribute("ObjectID").map(|id| (id, n))).collect();
        Self { objects, seen: Default::default() }
    }

    /// The object a reference element points to.
    fn get(&self, r: Node<'a, 'input>) -> Option<Node<'a, 'input>> {
        reference(r).and_then(|id| self.objects.get(id).copied())
    }

    /// The objects a list element (`<Items>`, `<Params>`…) points to, in order.
    fn list(&self, n: Node<'a, 'input>, list: &str) -> Vec<Node<'a, 'input>> {
        child(n, list).map(|l| l.children().filter(|c| c.is_element()).filter_map(|c| self.get(c)).collect()).unwrap_or_default()
    }

    /// A bin (folder) or a preset; folders are walked depth-first.
    fn walk(&self, n: Node<'a, 'input>, path: &mut Vec<String>, out: &mut Vec<EffectPreset>, depth: usize) {
        if depth > MAX_DEPTH || out.len() >= MAX_PRESETS {
            return;
        }
        let base = child(n, "TreeItemBase");
        let name = base.and_then(|b| text(b, "Name")).unwrap_or_default().trim().to_string();
        if n.has_tag_name("BinTreeItem") {
            if let Some(id) = n.attribute("ObjectID")
                && !self.seen.borrow_mut().insert(id)
            {
                return;
            }
            let named = depth > 0 && !name.is_empty();
            if named {
                path.push(name);
            }
            for item in self.list(n, "Items") {
                self.walk(item, path, out, depth + 1);
            }
            if named {
                path.pop();
            }
        } else if n.has_tag_name("TreeItem") {
            let data = base.and_then(|b| child(b, "Data")).and_then(|d| self.get(d));
            if let Some(p) = data.and_then(|d| self.preset(d, &name, path)) {
                out.push(p);
            }
        }
    }

    fn preset(&self, item: Node<'a, 'input>, name: &str, path: &[String]) -> Option<EffectPreset> {
        let filters = self.list(item, "FilterPresets");
        let first = filters.first()?;
        let anchor_in = text(*first, "AnchorInPoint").and_then(|t| t.trim().parse::<i64>().ok()).unwrap_or(0);
        let anchor_out = text(*first, "AnchorOutPoint").and_then(|t| t.trim().parse::<i64>().ok()).unwrap_or(anchor_in);
        let keyframes = match text(*first, "Type").map(str::trim) {
            Some("1") => KeyframeMode::AnchorToIn,
            Some("2") => KeyframeMode::AnchorToOut,
            _ => KeyframeMode::Scale,
        };
        let effects: Vec<EffectInstance> = filters.iter().filter_map(|f| self.filter(*f)).collect();
        if effects.is_empty() || name.is_empty() {
            return None;
        }
        Some(EffectPreset {
            name: name.to_string(),
            description: "Imported from Premiere Pro".into(),
            keyframes,
            source_duration: Tick(anchor_out.saturating_sub(anchor_in).max(0)),
            source_size: (FRAME.0 as u32, FRAME.1 as u32),
            effects,
            builtin: false,
            // `/` separates folders: one inside a name becomes a dash
            folder: path.iter().map(|p| p.replace('/', "-")).collect::<Vec<_>>().join("/"),
        })
    }

    /// One effect of a preset.
    fn filter(&self, f: Node<'a, 'input>) -> Option<EffectInstance> {
        let anchor_in = text(f, "AnchorInPoint").and_then(|t| t.trim().parse::<i64>().ok()).unwrap_or(0);
        let comp = child(f, "Component").and_then(|c| self.get(c))?;
        let match_name = text(f, "FilterMatchName").or_else(|| text(comp, "MatchName")).unwrap_or_default();
        let inner = child(comp, "Component")?;
        let params: Vec<Raw> = self.list(inner, "Params").into_iter().filter_map(|p| Raw::read(p, anchor_in)).collect();
        let mut e = convert(match_name, &params)?;
        if text(inner, "Bypass").map(str::trim) == Some("true") {
            e.enabled = false;
        }
        for sub in self.list(comp, "SubComponents") {
            if let Some(m) = self.mask(sub, anchor_in) {
                e.masks.push(m);
            }
        }
        Some(e)
    }

    fn mask(&self, comp: Node<'a, 'input>, anchor_in: i64) -> Option<Mask> {
        let inner = child(comp, "Component")?;
        if text(comp, "MatchName") != Some("AE.ADBE AEMask") {
            return None;
        }
        let params: Vec<Raw> = self.list(inner, "Params").into_iter().filter_map(|p| Raw::read(p, anchor_in)).collect();
        let path = params.iter().find(|p| p.id == 6).and_then(|p| p.blob.as_deref()).and_then(mask_path)?;
        let name = text(inner, "InstanceName").map(|n| format!("Mask ({})", n.trim())).unwrap_or_else(|| "Mask".into());
        let mut m = Mask::new(name, path);
        for p in &params {
            match p.id {
                7 => m.feather = p.param(&ParamKind::Angle),
                8 => m.opacity = p.param(&ParamKind::Angle),
                9 => m.expansion = p.param(&ParamKind::Angle),
                10 => m.inverted = p.start.trim() == "true",
                _ => {}
            }
        }
        Some(m)
    }
}

/// A Premiere parameter as stored: its static value and keyframes, still text.
struct Raw {
    id: u32,
    name: String,
    start: String,
    /// (time relative to the anchor in point, value, interpolation type, in speed, in influence,
    /// out speed, out influence)
    keys: Vec<RawKey>,
    /// Arbitrary-data parameters (mask paths): the decoded bytes.
    blob: Option<Vec<u8>>,
}

struct RawKey {
    time: i64,
    value: String,
    kind: u32,
    in_speed: f64,
    in_influence: f64,
    out_speed: f64,
    out_influence: f64,
}

impl Raw {
    fn read(p: Node<'_, '_>, anchor_in: i64) -> Option<Raw> {
        let id = text(p, "ParameterID")?.trim().parse().ok()?;
        let name = text(p, "Name").unwrap_or_default().trim().replace("&amp;", "&");
        let start = text(p, "StartKeyframe").and_then(|s| s.split(',').nth(1)).unwrap_or_default().to_string();
        let blob = text(p, "StartKeyframeValue").and_then(|b| filmcraft_project::gtemplate::base64_decode(b.trim()));
        let keys = text(p, "Keyframes").map(|k| parse_keys(k, anchor_in)).unwrap_or_default();
        Some(Raw { id, name, start, keys, blob })
    }

    /// The parameter for a FilmCraft parameter of `kind`.
    fn param(&self, kind: &ParamKind) -> Param {
        self.param_with(kind, |v| v)
    }

    /// Like [`Raw::param`], with every value passed through `f` (unit changes).
    fn param_with(&self, kind: &ParamKind, f: impl Fn(ParamValue) -> ParamValue) -> Param {
        let value = value(kind, &self.start).map(&f).unwrap_or(ParamValue::Float(0.0));
        let mut p = Param::new(value);
        p.keyframes = keyframes(kind, &self.keys, &f);
        if let Some(k) = p.keyframes.first() {
            p.value = k.value.clone();
        }
        p
    }
}

fn parse_keys(s: &str, anchor_in: i64) -> Vec<RawKey> {
    s.split(';')
        .filter(|k| !k.trim().is_empty())
        .take(MAX_KEYFRAMES)
        .filter_map(|k| {
            let f: Vec<&str> = k.split(',').collect();
            let num = |i: usize| f.get(i).and_then(|x| x.trim().parse::<f64>().ok()).filter(|x| x.is_finite()).unwrap_or(0.0);
            let time = f.first()?.trim().parse::<i64>().ok()?;
            Some(RawKey {
                time: time.saturating_sub(anchor_in),
                value: f.get(1)?.to_string(),
                kind: f.get(2).and_then(|x| x.trim().parse().ok()).unwrap_or(0),
                in_speed: num(4),
                in_influence: num(5),
                out_speed: num(6),
                out_influence: num(7),
            })
        })
        .collect()
}

/// A stored value as a FilmCraft value of `kind`.
fn value(kind: &ParamKind, s: &str) -> Option<ParamValue> {
    let s = s.trim();
    match kind {
        ParamKind::Float { .. } | ParamKind::Angle => s.parse::<f64>().ok().filter(|v| v.is_finite()).map(ParamValue::Float),
        ParamKind::Bool => Some(ParamValue::Bool(s == "true" || s == "1")),
        ParamKind::Choice(items) => {
            let i = s.parse::<u32>().ok()?;
            ((i as usize) < items.len()).then_some(ParamValue::Choice(i))
        }
        ParamKind::Color => s.parse::<u64>().ok().map(|c| ParamValue::Color(color(c))),
        ParamKind::Point => point(s).map(|(x, y)| ParamValue::Vec2(Vec2::new(x * FRAME.0, y * FRAME.1))),
        _ => None,
    }
}

/// Premiere colours: 16 bits each of alpha, red, green, blue.
fn color(c: u64) -> [f32; 4] {
    // 0xff00 is full (8-bit 255 in the high byte)
    let ch = |shift: u32| (((c >> shift) & 0xffff) as f32 / 65280.0).min(1.0);
    [ch(32), ch(16), ch(0), ch(48)]
}

fn point(s: &str) -> Option<(f64, f64)> {
    let (x, y) = s.split_once(':')?;
    let x = x.trim().parse::<f64>().ok().filter(|v| v.is_finite())?;
    let y = y.trim().parse::<f64>().ok().filter(|v| v.is_finite())?;
    Some((x, y))
}

fn secs(t: i64) -> f64 {
    t as f64 / TICKS_PER_SECOND as f64
}

/// FilmCraft keyframes for Premiere keyframes: Linear, Hold, or Bézier with the same influences and
/// handle speeds (as [`Keyframe::out_rise`] / [`Keyframe::in_rise`], fractions of each segment's change).
fn keyframes(kind: &ParamKind, keys: &[RawKey], f: &impl Fn(ParamValue) -> ParamValue) -> Vec<Keyframe> {
    let numeric = matches!(kind, ParamKind::Float { .. } | ParamKind::Angle | ParamKind::Point);
    let hold = !numeric && !matches!(kind, ParamKind::Color);
    let mut out: Vec<(Keyframe, &RawKey)> = Vec::new();
    for k in keys {
        let Some(v) = value(kind, &k.value).map(f) else { continue };
        let mut key = Keyframe::new(Tick(k.time), v);
        key.in_influence = k.in_influence.clamp(0.01, 1.0);
        key.out_influence = k.out_influence.clamp(0.01, 1.0);
        key.interp = if hold || k.kind == 4 {
            Interpolation::Hold
        } else if numeric && k.kind != 0 {
            Interpolation::Bezier
        } else {
            Interpolation::Linear
        };
        out.push((key, k));
    }
    out.sort_by_key(|(k, _)| k.time);
    out.dedup_by_key(|(k, _)| k.time);
    for i in 1..out.len() {
        let (Some((a, ra)), Some((b, rb))) = (out.get(i - 1), out.get(i)) else { continue };
        if a.interp != Interpolation::Bezier {
            continue;
        }
        let (out_rise, in_rise) = rises(ra, rb, &a.value, &b.value);
        if let Some((a, _)) = out.get_mut(i - 1) {
            a.out_rise = out_rise;
        }
        if let Some((b, _)) = out.get_mut(i) {
            b.in_rise = in_rise;
        }
    }
    out.into_iter().map(|(k, _)| k).collect()
}

/// The handle heights of the segment `a`→`b`, as fractions of its change (`None`: flat).
fn rises(a: &RawKey, b: &RawKey, va: &ParamValue, vb: &ParamValue) -> (Option<f64>, Option<f64>) {
    let d = secs(b.time.saturating_sub(a.time));
    // scalar speeds are signed velocities; point speeds are magnitudes along the segment in
    // normalized units per second
    let (change, out_speed, in_speed) = match (va, vb) {
        (ParamValue::Float(x), ParamValue::Float(y)) => (y - x, a.out_speed, b.in_speed),
        (ParamValue::Vec2(p), ParamValue::Vec2(q)) => (((q.x - p.x) / FRAME.0).hypot((q.y - p.y) / FRAME.1), a.out_speed.abs(), b.in_speed.abs()),
        _ => return (None, None),
    };
    if change.abs() < 1e-9 || !d.is_finite() || d <= 0.0 {
        return (None, None);
    }
    // along a path the distance only grows: a point's handles stay within the segment
    let spatial = matches!(va, ParamValue::Vec2(_));
    let rise = |speed: f64, influence: f64| {
        let r = speed * influence.clamp(0.01, 1.0) * d / change;
        let r = if spatial { r.clamp(0.0, 1.0) } else { r };
        (r.is_finite() && r.abs() > 1e-9).then_some(r)
    };
    (rise(out_speed, a.out_influence), rise(in_speed, b.in_influence))
}

/// A mask path blob (`2cin`): vertices with absolute in/out handles, normalized to the frame.
fn mask_path(b: &[u8]) -> Option<MaskPath> {
    let u32_at = |o: usize| b.get(o..o + 4).and_then(|s| s.try_into().ok()).map(u32::from_le_bytes);
    let f32_at = |o: usize| b.get(o..o + 4).and_then(|s| s.try_into().ok()).map(f32::from_le_bytes).map(f64::from).filter(|v| v.is_finite());
    if b.get(0..4) != Some(b"2cin") {
        return None;
    }
    let closed = u32_at(8)? != 0;
    let count = (u32_at(12)? as usize).min(4096);
    let mut vertices = Vec::with_capacity(count);
    for i in 0..count {
        let o = 16 + i * 32 + 4;
        let pt = |k: usize| Some(Vec2::new(f32_at(o + k * 8)? * FRAME.0, f32_at(o + k * 8 + 4)? * FRAME.1));
        let (p, h_in, h_out) = (pt(0)?, pt(1)?, pt(2)?);
        vertices.push(MaskVertex { p, t_in: h_in - p, t_out: h_out - p });
    }
    (vertices.len() >= 2).then_some(MaskPath { vertices, closed })
}

// ---------------------------------------------------------------- effects

/// The FilmCraft effect for a Premiere effect with these parameters.
fn convert(match_name: &str, params: &[Raw]) -> Option<EffectInstance> {
    let short = match_name.trim().strip_prefix("AE.").unwrap_or(match_name.trim());
    match short {
        "ADBE Geometry2" => by_id("transform", params, TRANSFORM),
        "ADBE Lumetri" => by_id("lumetri", params, LUMETRI),
        "ADBE Paint Bucket" => paint_bucket(params),
        "ADBE Opacity" => by_name("opacity", params),
        "ADBE Drop Shadow" => by_name("drop_shadow", params).map(|mut e| {
            // Premiere's Drop Shadow opacity is 0–255
            if let Some(p) = params.iter().find(|p| p.name == "Opacity") {
                e.params.insert("opacity".into(), p.param_with(&ParamKind::Angle, |v| scale(v, 100.0 / 255.0)));
            }
            e
        }),
        "ADBE Noise2" => by_name("noise", params).map(|mut e| {
            if let Some(p) = params.iter().find(|p| p.name == "Noise Type") {
                e.params.insert("color".into(), p.param(&ParamKind::Bool));
            }
            e
        }),
        "ADBE Gaussian Blur 2" => by_name("gaussian_blur", params),
        "ADBE Mirror" => by_name("mirror", params),
        "ADBE Tint" => by_name("tint", params),
        "ADBE Basic 3D" => by_name("basic_3d", params),
        "ADBE Strobe" => by_name("strobe", params),
        "ADBE Roughen Edges" => by_name("roughen_edges", params).map(|mut e| {
            // FilmCraft's Spiky only reaches outward, so on a full-frame clip it shows nothing;
            // Premiere's cuts into the picture like Roughen does
            if let Some(p) = e.params.get_mut("edge_type")
                && p.value == ParamValue::Choice(3)
            {
                p.value = ParamValue::Choice(0);
            }
            e
        }),
        "ADBE Wave Warp" => by_name("wave_warp", params),
        "ADBE Mosaic" => by_name("mosaic", params),
        "Mettle SkyBox Digital Glitch" => by_name("vr_digital_glitch", params).map(|mut e| {
            // FilmCraft's glitch shifts every band at full amplitude; Mettle's is far subtler
            if let Some(a) = e.params.get_mut("amplitude") {
                a.map_values(|v| scale(v, 0.1));
            }
            e
        }),
        _ => by_display_name(short, params),
    }
}

fn scale(v: ParamValue, k: f64) -> ParamValue {
    match v {
        ParamValue::Float(x) => ParamValue::Float(x * k),
        other => other,
    }
}

fn def_param<'d>(def: &'d EffectDef, id: &str) -> Option<&'d ParamDef> {
    def.params.iter().find(|p| p.id == id)
}

/// Parameters matched by their Premiere display name (each FilmCraft parameter at most once).
fn by_name(effect: &str, params: &[Raw]) -> Option<EffectInstance> {
    let def = find_effect(effect)?;
    let mut e = def.instance();
    let mut used: Vec<&str> = Vec::new();
    // Premiere numbers its blend modes its own way: those stay at Normal
    for p in params.iter().filter(|p| !p.name.is_empty() && !matches!(p.name.as_str(), "Blend Mode" | "Blending Mode")) {
        let Some(d) = def.params.iter().find(|d| d.label.eq_ignore_ascii_case(&p.name) && !used.contains(&d.id)) else { continue };
        if value(&d.kind, &p.start).is_none() && p.keys.is_empty() {
            continue;
        }
        used.push(d.id);
        e.params.insert(d.id.to_string(), p.param(&d.kind));
    }
    Some(e)
}

/// An effect FilmCraft has under the same display name (Premiere's match names are `ADBE <name>`).
fn by_display_name(short: &str, params: &[Raw]) -> Option<EffectInstance> {
    let name = short.strip_prefix("ADBE ").unwrap_or(short);
    let def = filmcraft_project::effect::find_effect_by_name(name)?;
    by_name(def.id, params)
}

/// Parameters matched by Premiere parameter id: (id, FilmCraft parameter, conversion).
type IdMap = &'static [(u32, &'static str, Conv)];

#[derive(Clone, Copy)]
enum Conv {
    Same,
    Not,
}

const TRANSFORM: IdMap = &[
    (1, "anchor", Conv::Same),
    (2, "position", Conv::Same),
    (11, "uniform_scale", Conv::Same),
    (3, "scale_height", Conv::Same),
    (4, "scale_width", Conv::Same),
    (5, "skew", Conv::Same),
    (6, "skew_axis", Conv::Same),
    (7, "rotation", Conv::Same),
    (8, "opacity", Conv::Same),
    // "Use Composition's Shutter Angle"
    (9, "shutter_override", Conv::Not),
    (10, "shutter_angle", Conv::Same),
];

const LUMETRI: IdMap = &[
    (7, "temperature", Conv::Same),
    (8, "tint", Conv::Same),
    (20, "saturation", Conv::Same),
    (11, "exposure", Conv::Same),
    (12, "contrast", Conv::Same),
    (13, "highlights", Conv::Same),
    (14, "shadows", Conv::Same),
    (15, "whites", Conv::Same),
    (16, "blacks", Conv::Same),
    (26, "look_intensity", Conv::Same),
    (28, "faded_film", Conv::Same),
    (29, "sharpen", Conv::Same),
    (30, "vibrance", Conv::Same),
    (31, "creative_sat", Conv::Same),
    (51, "vignette_amount", Conv::Same),
    (52, "vignette_midpoint", Conv::Same),
    (53, "vignette_roundness", Conv::Same),
    (54, "vignette_feather", Conv::Same),
];

fn by_id(effect: &str, params: &[Raw], map: IdMap) -> Option<EffectInstance> {
    let def = find_effect(effect)?;
    let mut e = def.instance();
    for (pid, id, conv) in map {
        let (Some(p), Some(d)) = (params.iter().find(|p| p.id == *pid), def_param(def, id)) else { continue };
        if value(&d.kind, &p.start).is_none() && p.keys.is_empty() {
            continue;
        }
        let prm = match conv {
            Conv::Same => p.param(&d.kind),
            Conv::Not => p.param_with(&d.kind, |v| match v {
                ParamValue::Bool(b) => ParamValue::Bool(!b),
                other => other,
            }),
        };
        e.params.insert(id.to_string(), prm);
    }
    Some(e)
}

/// Paint Bucket: its Stroke option (an outline around the alpha edge) is FilmCraft's Stroke effect;
/// a plain fill stays a Paint Bucket.
fn paint_bucket(params: &[Raw]) -> Option<EffectInstance> {
    let get = |id: u32| params.iter().find(|p| p.id == id);
    let stroke = get(8).is_some_and(|p| p.start.trim() == "4");
    if !stroke {
        return by_name("paint_bucket", params);
    }
    let def = find_effect("stroke")?;
    let mut e = def.instance();
    let mut set = |pid: u32, id: &str| {
        if let (Some(p), Some(d)) = (get(pid), def_param(def, id)) {
            e.params.insert(id.to_string(), p.param(&d.kind));
        }
    };
    set(1, "color");
    set(5, "width");
    set(10, "opacity");
    e.params.insert("position".into(), Param::new(ParamValue::Choice(1)));
    Some(e)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<PremiereData Version="3">
	<Tree ObjectRef="1"/>
	<Tree ObjectID="1"><RootBin ObjectRef="2"/></Tree>
	<BinTreeItem ObjectID="2"><Items><Item Index="0" ObjectRef="3"/></Items><TreeItemBase><Name>Root</Name></TreeItemBase></BinTreeItem>
	<BinTreeItem ObjectID="3"><Items><Item Index="0" ObjectRef="4"/></Items><TreeItemBase><Name>Slides</Name></TreeItemBase></BinTreeItem>
	<TreeItem ObjectID="4"><TreeItemBase><Name>Slide In</Name><Data ObjectRef="5"/></TreeItemBase></TreeItem>
	<FilterPresetItem ObjectID="5"><FilterPresets><FilterPreset Index="0" ObjectRef="6"/><FilterPreset Index="1" ObjectRef="20"/></FilterPresets></FilterPresetItem>
	<FilterPreset ObjectID="6"><FilterMatchName>AE.ADBE Geometry2</FilterMatchName><Component ObjectRef="7"/><AnchorInPoint>0</AnchorInPoint><AnchorOutPoint>254016000000</AnchorOutPoint><Type>1</Type></FilterPreset>
	<VideoFilterComponent ObjectID="7"><Component><Bypass>false</Bypass><DisplayName>Transform</DisplayName><Params><Param Index="0" ObjectRef="8"/><Param Index="1" ObjectRef="9"/><Param Index="2" ObjectRef="10"/></Params></Component><MatchName>AE.ADBE Geometry2</MatchName></VideoFilterComponent>
	<PointComponentParam ObjectID="8"><Name>Position</Name><Keyframes>0,1.5:0.5,5,0,0,0.33333,10,0.2,5,4,0,0,0,0;254016000000,0.5:0.5,5,0,0,1,0,0.33333,5,4,0,0,0,0;</Keyframes><ParameterID>2</ParameterID><StartKeyframe>-91445760000000000,0.5:0.5,0,0,0,0,0,0,5,4,0,0,0,0</StartKeyframe></PointComponentParam>
	<VideoComponentParam ObjectID="9"><Name>Rotation</Name><Keyframes></Keyframes><ParameterID>7</ParameterID><StartKeyframe>-91445760000000000,15.,0,0,0,0,0,0</StartKeyframe></VideoComponentParam>
	<VideoComponentParam ObjectID="10"><Name></Name><Keyframes></Keyframes><ParameterID>9</ParameterID><StartKeyframe>-91445760000000000,false,0,0,0,0,0,0</StartKeyframe></VideoComponentParam>
	<FilterPreset ObjectID="20"><FilterMatchName>AE.ADBE Tint</FilterMatchName><Component ObjectRef="21"/><AnchorInPoint>0</AnchorInPoint><AnchorOutPoint>254016000000</AnchorOutPoint><Type>1</Type></FilterPreset>
	<VideoFilterComponent ObjectID="21"><Component><DisplayName>Tint</DisplayName><Params><Param Index="0" ObjectRef="22"/><Param Index="1" ObjectRef="23"/></Params></Component><MatchName>AE.ADBE Tint</MatchName><SubComponents><SubComponent Index="0" ObjectRef="30"/></SubComponents></VideoFilterComponent>
	<VideoComponentParam ObjectID="22"><Name>Map Black To</Name><ParameterID>1</ParameterID><StartKeyframe>-91445760000000000,18374686479671623680,0,0,0,0,0,0</StartKeyframe></VideoComponentParam>
	<VideoComponentParam ObjectID="23"><Name>Amount to Tint</Name><ParameterID>3</ParameterID><StartKeyframe>-91445760000000000,50.,0,0,0,0,0,0</StartKeyframe></VideoComponentParam>
	<VideoFilterComponent ObjectID="30"><Component><DisplayName>Mask</DisplayName><InstanceName>1</InstanceName><Params><Param Index="0" ObjectRef="31"/><Param Index="1" ObjectRef="32"/><Param Index="2" ObjectRef="33"/></Params></Component><MatchName>AE.ADBE AEMask</MatchName></VideoFilterComponent>
	<ArbVideoComponentParam ObjectID="31"><Name>Mask Path</Name><ParameterID>6</ParameterID><StartKeyframeValue Encoding="base64">MmNpbgIAAAABAAAABAAAAAEAAAAiP/4+q/aqPY3Vbz6a9qo9v0lCP7z2qj0BAAAAAQAAAGG8eD9ZNvo+Yrx4Pwqhhz5hvHg/1GU2PwEAAAABAAAAHj/+PnzXZD+8SUI/ftdkP4TVbz5612Q/AQAAAAEAAADeV7A8Sjb6PsxXsDzMZTY/8FewPPughz4BAAAA</StartKeyframeValue></ArbVideoComponentParam>
	<VideoComponentParam ObjectID="32"><Name>Mask Feather</Name><ParameterID>7</ParameterID><StartKeyframe>-91445760000000000,385.,0,0,0,0,0,0</StartKeyframe></VideoComponentParam>
	<VideoComponentParam ObjectID="33"><Name></Name><ParameterID>10</ParameterID><StartKeyframe>-91445760000000000,true,0,0,0,0,0,0</StartKeyframe></VideoComponentParam>
</PremiereData>"#;

    fn f(e: &EffectInstance, id: &str) -> Option<ParamValue> {
        e.params.get(id).map(|p| p.value.clone())
    }

    #[test]
    fn reads_effects_keyframes_and_masks() {
        assert!(is_premiere(SAMPLE.as_bytes()));
        let presets = parse(SAMPLE.as_bytes()).unwrap_or_default();
        assert_eq!(presets.len(), 1);
        let p = &presets[0];
        assert_eq!(p.name, "Slide In");
        assert_eq!(p.folder, "Slides");
        assert_eq!(p.keyframes, KeyframeMode::AnchorToIn);
        assert_eq!(p.source_duration, Tick(TICKS_PER_SECOND));
        let t = &p.effects[0];
        assert_eq!(t.effect, "transform");
        assert_eq!(f(t, "rotation"), Some(ParamValue::Float(15.0)));
        // "Use Composition's Shutter Angle" off = the effect's own shutter angle
        assert_eq!(f(t, "shutter_override"), Some(ParamValue::Bool(true)));
        let pos = &t.params["position"];
        // normalized points become 1920×1080 pixels; the Bézier speed becomes the handle's height
        assert_eq!(pos.keyframes.len(), 2);
        assert_eq!(pos.keyframes[0].value, ParamValue::Vec2(Vec2::new(2880.0, 540.0)));
        assert_eq!(pos.keyframes[1].value, ParamValue::Vec2(Vec2::new(960.0, 540.0)));
        assert_eq!(pos.keyframes[1].time, Tick(TICKS_PER_SECOND));
        assert_eq!(pos.keyframes[0].interp, Interpolation::Bezier);
        // a point's handle stays within its segment (Premiere does not overshoot along a path)
        assert_eq!(pos.keyframes[0].out_rise, Some(1.0));
        let tint = &p.effects[1];
        assert_eq!(f(tint, "black"), Some(ParamValue::Color([0.0, 0.0, 0.0, 1.0])));
        assert_eq!(f(tint, "amount"), Some(ParamValue::Float(50.0)));
        let m = &tint.masks[0];
        assert!(m.inverted);
        assert_eq!(m.feather.value, ParamValue::Float(385.0));
        let ParamValue::Path(path) = &m.path.value else { panic!("a path") };
        assert_eq!(path.vertices.len(), 4);
        assert!((path.vertices[0].p.x - 0.4966 * 1920.0).abs() < 1.0);
    }

    #[test]
    fn a_fast_start_settles_without_passing_the_end() {
        // out speed 10 normalized/s at 20 % influence, flat into the end with 100 % influence
        let presets = parse(SAMPLE.as_bytes()).unwrap_or_default();
        let pos = &presets[0].effects[0].params["position"];
        let x = |s: f64| pos.vec2_at(Tick((s * TICKS_PER_SECOND as f64) as i64)).x;
        assert!(x(0.1) < 2880.0 - 960.0, "most of the way after 0.1 s: {}", x(0.1));
        assert!((1..10).map(|i| x(i as f64 / 10.0)).all(|v| v >= 960.0 - 1e-6), "never passes the end");
        assert!((x(1.0) - 960.0).abs() < 1e-6);
    }

    #[test]
    fn a_scalar_handle_with_speed_overshoots() {
        // Keyframe::out_rise: a fast start (twice the change) carries a value past its end
        let mut a = Keyframe::new(Tick(0), ParamValue::Float(0.0));
        a.interp = Interpolation::Bezier;
        a.out_rise = Some(2.0);
        let mut b = Keyframe::new(Tick(TICKS_PER_SECOND), ParamValue::Float(100.0));
        b.interp = Interpolation::Bezier;
        b.in_influence = 1.0;
        let p = Param { value: ParamValue::Float(0.0), keyframes: vec![a.clone(), b.clone()] };
        let peak = (1..10).map(|i| p.f64_at(Tick(TICKS_PER_SECOND * i / 10))).fold(f64::MIN, f64::max);
        assert!(peak > 100.0, "{peak}");
        // no rise: the plain ease stays within the change
        a.out_rise = None;
        let p = Param { value: ParamValue::Float(0.0), keyframes: vec![a, b] };
        assert!((1..10).map(|i| p.f64_at(Tick(TICKS_PER_SECOND * i / 10))).all(|v| (0.0..=100.0).contains(&v)));
    }

    #[test]
    fn hostile_files_do_not_panic() {
        for bad in [
            "",
            "<PremiereData>",
            "<PremiereData><Tree ObjectRef=\"1\"/><Tree ObjectID=\"1\"><RootBin ObjectRef=\"1\"/></Tree></PremiereData>",
            &SAMPLE.replace("0.5:0.5", "x:y"),
            &SAMPLE.replace("254016000000", "99999999999999999999"),
        ] {
            let _ = parse(bad.as_bytes());
        }
        assert!(mask_path(b"2cin\x02\0\0\0\x01\0\0\0\xff\xff\xff\xff").is_none());
    }
}
