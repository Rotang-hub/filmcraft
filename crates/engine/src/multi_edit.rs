//! Edit several clips at once (custom feature of this fork): a change made to one clip's
//! property — an effect parameter in Effect Controls, a graphics template property or a graphic
//! layer's property in Essential Graphics — is made to every selected clip that has the same
//! property, as one undo step.
//!
//! | Id | What |
//! |---|---|
//! | `edit.forSelection` | run `command` with `params` on its clip and on the other selected clips |
//!
//! How the change finds its way to the other clips:
//! - **Effect parameters** (`effects.setParam`, `effects.resetParam`, `effects.toggleEnabled`): the
//!   effect is matched by kind and by which one of that kind it is (the second Gaussian Blur of
//!   one clip is the second of the other). Mask parameters and parameters the other clip animates
//!   (it has keyframes there) are left alone, since a playhead time means something else on
//!   another clip.
//! - **Template properties** (`graphics.template.set`): clips made from a template with the same
//!   property. Text properties (a caption's words) are never copied.
//! - **Layer properties** (`graphics.set`): the layer is matched by its identity (graphics made
//!   from one template share it), else by position when the layer kinds agree. Text is never copied.
//!
//! Clips that lack the property are skipped. A drag (`merge`, with `begin` on its first change)
//! stays one undo step for all the clips together.
//!
//! Kept in its own file so upstream FilmCraft updates rarely touch it.

use filmcraft_project::graphic::{is_layer, layer_indices};
use filmcraft_project::gtemplate::{ControlKind, layer_uid};
use filmcraft_project::{ClipId, TrackItem};
use serde_json::{Value, json};

use crate::commands::{CommandSpec, bad, bool_p, has_seq, str_p};
use crate::{Result, Session};

const CMD: &str = "edit.forSelection";
/// The undo label of a change made to several clips.
pub const LABEL: &str = "Change Selected Clips";
/// The commands that can be run for the selection.
pub const COMMANDS: [&str; 5] = ["effects.setParam", "effects.resetParam", "effects.toggleEnabled", "graphics.template.set", "graphics.set"];
/// Most clips one change reaches.
const MAX_CLIPS: usize = 10_000;

fn item(s: &Session, c: ClipId) -> Option<&TrackItem> {
    s.active_sequence()?.find_item(c).map(|(_, it)| it)
}

/// `(effect id, which of that id)` for an effect reference (index or id) on `it`.
fn effect_ref(it: &TrackItem, v: &Value) -> Option<(String, usize)> {
    let i = match v {
        Value::Number(n) => usize::try_from(n.as_u64()?).ok()?,
        Value::String(id) => it.effects.iter().position(|e| &e.effect == id)?,
        _ => return None,
    };
    let e = it.effects.get(i)?;
    let nth = it.effects.iter().take(i).filter(|x| x.effect == e.effect).count();
    Some((e.effect.clone(), nth))
}

/// Index on `it` of the `nth` effect `id`.
fn effect_index(it: &TrackItem, id: &str, nth: usize) -> Option<usize> {
    it.effects.iter().enumerate().filter(|(_, e)| e.effect == id).nth(nth).map(|(i, _)| i)
}

/// Layer number (as `graphics.set` counts) on `to` of layer number `l` of `from`.
fn layer_on(from: &TrackItem, l: usize, to: &TrackItem) -> Option<usize> {
    let fi = layer_indices(&from.effects);
    let ti = layer_indices(&to.effects);
    let src = from.effects.get(*fi.get(l)?)?;
    let uid = layer_uid(src);
    if uid != 0
        && let Some(k) = ti.iter().position(|&i| to.effects.get(i).is_some_and(|e| is_layer(e) && layer_uid(e) == uid))
    {
        return Some(k);
    }
    // graphics not from one template: the same position, when it is the same kind of layer
    let same_kind = ti.get(l).and_then(|&i| to.effects.get(i)).is_some_and(|e| e.effect == src.effect);
    (fi.len() == ti.len() && same_kind).then_some(l)
}

/// The parameters of `cmd` for `target`, made from those for `primary`; None when the change does
/// not apply there (or must not be copied, as a caption's text).
fn retarget(s: &Session, cmd: &str, params: &Value, primary: &TrackItem, target: ClipId) -> Option<Value> {
    let to = item(s, target)?;
    let mut p = params.clone();
    p["clip"] = json!(target.0);
    match cmd {
        "effects.setParam" | "effects.resetParam" => {
            if p.get("mask").is_some_and(|m| !m.is_null()) {
                return None;
            }
            let (id, nth) = effect_ref(primary, params.get("effect").unwrap_or(&json!("motion")))?;
            let i = effect_index(to, &id, nth)?;
            let pid = str_p(params, "param")?;
            let e = to.effects.get(i)?;
            // keyframes there: the playhead's time is another moment of that clip
            if cmd == "effects.setParam" && params.get("time").is_none() && e.params.get(pid).is_some_and(|x| !x.keyframes.is_empty()) {
                return None;
            }
            if !e.params.contains_key(pid) && e.def().and_then(|d| d.param(pid)).is_none() {
                return None;
            }
            p["effect"] = json!(i);
            Some(p)
        }
        "effects.toggleEnabled" => {
            let (id, nth) = effect_ref(primary, params.get("index")?)?;
            // toggling flips: the other clips take the primary's new state, set below by the caller
            p["index"] = json!(effect_index(to, &id, nth)?);
            Some(p)
        }
        "graphics.template.set" => {
            let link = to.graphic.as_ref()?.template.as_ref()?;
            let mut out = serde_json::Map::new();
            let mut wanted: Vec<(String, Value)> = Vec::new();
            if let Some(c) = str_p(params, "control") {
                wanted.push((c.to_string(), params.get("value").cloned().unwrap_or(Value::Null)));
            }
            if let Some(vals) = params.get("values").and_then(Value::as_object) {
                wanted.extend(vals.iter().map(|(k, v)| (k.clone(), v.clone())));
            }
            for (k, v) in wanted {
                let Some(c) = link.controls.iter().find(|c| c.id == k || c.name.eq_ignore_ascii_case(&k)) else { continue };
                if c.kind == ControlKind::Text {
                    continue;
                }
                out.insert(c.id.clone(), v);
            }
            if out.is_empty() {
                return None;
            }
            Some(json!({"clip": target.0, "values": out}))
        }
        "graphics.set" => {
            let l = params.get("layer").and_then(Value::as_u64).and_then(|l| usize::try_from(l).ok())?;
            let tl = layer_on(primary, l, to)?;
            let mut props = params.get("props")?.as_object()?.clone();
            props.remove("text");
            if props.is_empty() {
                return None;
            }
            p["layer"] = json!(tl);
            p["props"] = Value::Object(props);
            Some(p)
        }
        _ => None,
    }
}

/// `edit.forSelection {command, params, clips?, merge?, begin?}`.
fn for_selection(s: &mut Session, p: &Value) -> Result<Value> {
    let cmd = str_p(p, "command").ok_or_else(|| bad(CMD, "need `command`"))?.to_string();
    if !COMMANDS.contains(&cmd.as_str()) {
        return Err(bad(CMD, format!("`{cmd}` can't be run for the selection (one of {COMMANDS:?})")));
    }
    let mut params = p.get("params").filter(|v| v.is_object()).cloned().ok_or_else(|| bad(CMD, "need `params` (an object)"))?;
    let primary = params.get("clip").and_then(Value::as_u64).map(ClipId).ok_or_else(|| bad(CMD, "`params` needs `clip`"))?;
    // a drag's flags belong to the whole change
    if let Some(o) = params.as_object_mut() {
        o.remove("merge");
        o.remove("begin");
    }
    // `merge`: true, or a key naming the gesture (a Program monitor handle that changes several
    // parameters at once); every change of one gesture folds into one undo step
    let merge_key = match p.get("merge") {
        Some(Value::String(k)) => Some(k.clone()),
        Some(Value::Bool(true)) => Some(String::new()),
        _ => None,
    };
    let merge = merge_key.is_some();
    let begin = bool_p(p, "begin").unwrap_or(false);
    let mut targets: Vec<ClipId> = match p.get("clips") {
        Some(Value::Array(a)) => a.iter().filter_map(Value::as_u64).map(ClipId).collect(),
        Some(_) => return Err(bad(CMD, "`clips` is a list of clip ids")),
        None => s.state.selection.clone(),
    };
    if targets.len() > MAX_CLIPS {
        return Err(bad(CMD, format!("at most {MAX_CLIPS} clips")));
    }
    targets.retain(|c| *c != primary);
    targets.dedup();
    let source = item(s, primary).cloned().ok_or_else(|| bad(CMD, format!("no clip {}", primary.0)))?;
    // one gesture is one key, whatever parameters it changes on the way (position and anchor)
    let key = format!("{CMD}:{cmd}:{}:{}", primary.0, merge_key.as_deref().unwrap_or(""));
    let continuing = merge && !begin && s.history.merge_key.as_deref() == Some(key.as_str()) && s.history.undo.last().is_some_and(|u| u.0 == LABEL);

    let run = |s: &mut Session| -> Result<(Value, usize)> {
        let first = s.execute(&cmd, params.clone())?;
        // a toggle flips each clip; the others take the primary's new state instead
        let enabled_now = (cmd == "effects.toggleEnabled")
            .then(|| item(s, primary).and_then(|it| it.effects.get(params.get("index")?.as_u64()? as usize).map(|e| e.enabled)))
            .flatten();
        let mut n = 1;
        for t in &targets {
            let Some(sub) = retarget(s, &cmd, &params, &source, *t) else { continue };
            if let (Some(want), Some(i)) = (enabled_now, sub.get("index").and_then(Value::as_u64)) {
                let has = item(s, *t).and_then(|it| it.effects.get(i as usize)).is_some_and(|e| e.enabled == want);
                if has {
                    n += 1;
                    continue;
                }
            }
            if s.execute(&cmd, sub).is_ok() {
                n += 1;
            }
        }
        Ok((first, n))
    };

    let (first, n) = if continuing {
        // the rest of a drag: fold into the step its first change made
        let (before, limit) = (s.history.undo.len(), s.history.limit);
        s.history.limit = usize::MAX;
        let r = run(s);
        s.history.limit = limit;
        s.history.undo.truncate(before);
        r?
    } else {
        s.edit_group(LABEL, run)?
    };
    s.history.merge_key = merge.then_some(key);
    Ok(json!({"result": first, "clips": n}))
}

pub(crate) fn commands() -> Vec<CommandSpec> {
    vec![CommandSpec {
        id: CMD,
        label: LABEL,
        menu: &[],
        shortcut: None,
        params: r#"{"command":"effects.setParam|effects.resetParam|effects.toggleEnabled|graphics.template.set|graphics.set","params":{…, "clip": id},"clips":[id]? (default: the selection),"merge":bool|key?,"begin":bool?}"#,
        enabled: has_seq,
        run: for_selection,
        journal: true,
    }]
}

/// Whether a change to `clip` by `cmd` should go to the whole selection.
pub fn applies(s: &Session, cmd: &str, clip: Option<u64>) -> bool {
    COMMANDS.contains(&cmd) && s.state.selection.len() > 1 && clip.is_some_and(|c| s.state.selection.contains(&ClipId(c)))
}

#[cfg(test)]
mod tests {
    use filmcraft_project::ClipId;
    use serde_json::{Value, json};

    use crate::Session;

    /// Three picture clips on V1, all selected.
    fn session() -> (Session, Vec<u64>) {
        let mut s = Session::default();
        s.execute("file.openDemoProject", json!({})).unwrap();
        s.execute("file.newSequence", json!({"name": "Many", "video": 1, "audio": 1})).unwrap();
        let item = s.project.items.values().find(|i| i.has_video() && i.as_media().is_some()).map(|i| i.id).unwrap();
        let sec = 254_016_000_000_i64;
        for k in 0..3 {
            s.execute("timeline.place", json!({"item": item.0, "time": k * 20 * sec})).unwrap();
        }
        let clips: Vec<u64> = s.active_sequence().unwrap().video_tracks[0].items.iter().map(|i| i.id.0).collect();
        s.execute("timeline.select", json!({"clips": clips, "linked": false})).unwrap();
        (s, clips)
    }

    fn param(s: &Session, clip: u64, effect: &str, p: &str) -> Value {
        let q = s.active_sequence().unwrap();
        let it = q.find_item(ClipId(clip)).unwrap().1;
        let e = it.effects.iter().find(|e| e.effect == effect).unwrap();
        match &e.params[p].value {
            filmcraft_project::ParamValue::Float(x) => json!(x),
            other => json!(format!("{other:?}")),
        }
    }

    #[test]
    fn a_parameter_changes_on_every_selected_clip_in_one_undo_step() {
        let (mut s, clips) = session();
        let undo = s.history.undo.len();
        let r = s
            .execute(
                "edit.forSelection",
                json!({"command": "effects.setParam", "params": {"clip": clips[1], "effect": "motion", "param": "scale", "value": 60.0}}),
            )
            .unwrap();
        assert_eq!(r["clips"], 3);
        for c in &clips {
            assert_eq!(param(&s, *c, "motion", "scale"), json!(60.0));
        }
        assert_eq!(s.history.undo.len(), undo + 1);
        assert_eq!(s.history.undo.last().unwrap().0, super::LABEL);
        s.execute("edit.undo", json!({})).unwrap();
        for c in &clips {
            assert_eq!(param(&s, *c, "motion", "scale"), json!(100.0));
        }
    }

    #[test]
    fn a_drag_is_one_undo_step_for_all_clips() {
        let (mut s, clips) = session();
        let undo = s.history.undo.len();
        for (i, v) in [90.0, 80.0, 70.0].iter().enumerate() {
            s.execute(
                "edit.forSelection",
                json!({"command": "effects.setParam", "params": {"clip": clips[0], "effect": "motion", "param": "scale", "value": v}, "merge": true, "begin": i == 0}),
            )
            .unwrap();
        }
        assert_eq!(s.history.undo.len(), undo + 1);
        for c in &clips {
            assert_eq!(param(&s, *c, "motion", "scale"), json!(70.0));
        }
        s.execute("edit.undo", json!({})).unwrap();
        assert_eq!(param(&s, clips[2], "motion", "scale"), json!(100.0));
    }

    #[test]
    fn effects_are_matched_by_kind_and_order_and_others_are_skipped() {
        let (mut s, clips) = session();
        // only the first two clips have a Gaussian Blur
        s.execute("effects.apply", json!({"clips": [clips[0], clips[1]], "effect": "gaussian_blur"})).unwrap();
        let idx = {
            let q = s.active_sequence().unwrap();
            q.find_item(ClipId(clips[0])).unwrap().1.effects.iter().position(|e| e.effect == "gaussian_blur").unwrap()
        };
        let r = s
            .execute(
                "edit.forSelection",
                json!({"command": "effects.setParam", "params": {"clip": clips[0], "effect": idx, "param": "blurriness", "value": 33.0}}),
            )
            .unwrap();
        assert_eq!(r["clips"], 2);
        assert_eq!(param(&s, clips[1], "gaussian_blur", "blurriness"), json!(33.0));
        // toggling turns every clip's blur off, whatever each was
        s.execute("effects.toggleEnabled", json!({"clip": clips[1], "index": idx})).unwrap();
        s.execute("edit.forSelection", json!({"command": "effects.toggleEnabled", "params": {"clip": clips[0], "index": idx}})).unwrap();
        let q = s.active_sequence().unwrap();
        for c in [clips[0], clips[1]] {
            assert!(!q.find_item(ClipId(c)).unwrap().1.effects[idx].enabled, "clip {c}");
        }
    }

    #[test]
    fn template_text_is_never_copied_but_style_is() {
        let mut s = Session::default();
        s.execute("file.newSequence", json!({"name": "T", "video": 1, "audio": 1})).unwrap();
        let sec = 254_016_000_000_i64;
        let a = s.execute("graphics.template.apply", json!({"template": "builtin:lower-third-slab", "time": 0})).unwrap()["clip"].as_u64().unwrap();
        let b = s.execute("graphics.template.apply", json!({"template": "builtin:lower-third-slab", "time": 10 * sec})).unwrap()["clip"].as_u64().unwrap();
        s.execute("graphics.template.set", json!({"clip": b, "control": "name", "value": "Other name"})).unwrap();
        s.execute("timeline.select", json!({"clips": [a, b]})).unwrap();
        s.execute("edit.forSelection", json!({"command": "graphics.template.set", "params": {"clip": a, "control": "name", "value": "First"}})).unwrap();
        s.execute("edit.forSelection", json!({"command": "graphics.template.set", "params": {"clip": a, "control": "text_color", "value": "#ff0000"}}))
            .unwrap();
        s.execute("edit.forSelection", json!({"command": "graphics.set", "params": {"clip": a, "layer": 2, "props": {"size": 80.0, "text": "nope"}}})).unwrap();
        let ctl = |s: &mut Session, c: u64| s.execute("graphics.template.controls", json!({"clip": c})).unwrap();
        let (ca, cb) = (ctl(&mut s, a), ctl(&mut s, b));
        let get = |v: &Value, id: &str| v["controls"].as_array().unwrap().iter().find(|c| c["id"] == id).map(|c| c["value"].clone()).unwrap();
        assert_eq!(get(&ca, "name"), json!("nope"), "the clip edited takes everything, its text too");
        assert_eq!(get(&cb, "name"), json!("Other name"), "text stays each caption's own");
        assert_eq!(get(&ca, "text_color"), get(&cb, "text_color"), "the colour went to both");
    }

    #[test]
    fn hostile_parameters_are_errors_not_panics() {
        let (mut s, clips) = session();
        let before = s.project.clone();
        for p in [
            json!({}),
            json!({"command": "file.close"}),
            json!({"command": "effects.setParam"}),
            json!({"command": "effects.setParam", "params": []}),
            json!({"command": "effects.setParam", "params": {"effect": "motion"}}),
            json!({"command": "effects.setParam", "params": {"clip": 999_999, "effect": "motion", "param": "scale", "value": 5}}),
            json!({"command": "effects.setParam", "params": {"clip": clips[0], "effect": "motion", "param": "scale", "value": 5}, "clips": "all"}),
            json!({"command": "effects.setParam", "params": {"clip": clips[0], "effect": 9999, "param": "scale", "value": 5}}),
        ] {
            assert!(s.execute("edit.forSelection", p.clone()).is_err(), "{p}");
        }
        assert_eq!(*s.project, *before);
    }
}
