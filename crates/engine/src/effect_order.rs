//! Fork: reorder a clip's added effects (`effects.move`), as dragging an effect header in Effect
//! Controls does.
//!
//! Only effects added from the library move, and only among themselves: the fixed effects
//! (Motion, Opacity, Time Remapping, Volume…) and graphic layers keep their places. `to` is the
//! effect's new position among the added effects (0 = first). The selected effect stays selected.
//!
//! Kept in its own file so upstream updates rarely touch it.

use serde_json::{Value, json};

use filmcraft_project::{ClipId, EffectInstance};

use crate::commands::{CommandSpec, bad, has_seq, u64_p};
use crate::{EffectSel, Result, Session};

/// Whether `e` is an added effect (one that can be reordered).
pub fn movable(e: &EffectInstance) -> bool {
    !filmcraft_project::graphic::is_layer(e) && e.def().is_some_and(|d| !d.intrinsic)
}

fn run(s: &mut Session, p: &Value) -> Result<Value> {
    const ID: &str = "effects.move";
    let clip = ClipId(u64_p(p, "clip").ok_or_else(|| bad(ID, "need `clip`"))?);
    let index = u64_p(p, "index").ok_or_else(|| bad(ID, "need `index` (the effect's index on the clip)"))? as usize;
    let to = u64_p(p, "to").ok_or_else(|| bad(ID, "need `to` (its new place among the added effects)"))? as usize;
    let q = s.active_sequence().ok_or(crate::EngineError::NoSequence)?;
    let (_, it) = q.find_item(clip).ok_or_else(|| bad(ID, "no such clip"))?;
    let slots: Vec<usize> = (0..it.effects.len()).filter(|i| it.effects.get(*i).is_some_and(movable)).collect();
    let from = slots.iter().position(|i| *i == index).ok_or_else(|| bad(ID, "only added effects can be moved"))?;
    let to = to.min(slots.len().saturating_sub(1));
    if from == to {
        return Ok(json!({"moved": false}));
    }
    let selected = s.state.selected_effect.clone().filter(|e| e.clip == clip).and_then(|e| e.index(&it.effects));
    let new_selected = s.edit_sequence("Move Effect", move |q, _, _| {
        let (_, it) = q.find_item_mut(clip).ok_or_else(|| bad(ID, "no such clip"))?;
        // the added effects in their new order go back into the same slots
        let mut order: Vec<EffectInstance> = slots.iter().filter_map(|i| it.effects.get(*i).cloned()).collect();
        let moving = order.remove(from);
        order.insert(to, moving);
        let mut keep: Vec<usize> = slots.clone();
        let m = keep.remove(from);
        keep.insert(to, m);
        for (slot, e) in slots.iter().zip(order) {
            if let Some(dst) = it.effects.get_mut(*slot) {
                *dst = e;
            }
        }
        // the selection follows its effect: `keep` lists the old slots in their new order
        Ok(selected.map(|old| keep.iter().position(|i| *i == old).and_then(|n| slots.get(n).copied()).unwrap_or(old)))
    })?;
    if let Some(i) = new_selected
        && let Some((_, it)) = s.active_sequence().and_then(|q| q.find_item(clip))
    {
        s.state.selected_effect = EffectSel::of(clip, &it.effects, i);
    }
    Ok(json!({"moved": true, "selected": new_selected}))
}

pub fn commands() -> Vec<CommandSpec> {
    vec![CommandSpec {
        id: "effects.move",
        label: "Move Effect",
        menu: &[],
        shortcut: None,
        params: r#"{"clip":id,"index":n,"to":n}"#,
        enabled: has_seq,
        run,
        journal: true,
        named: crate::commands::NamedKeys::NONE,
    }]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(s: &Session, clip: u64) -> Vec<String> {
        let q = s.active_sequence().unwrap();
        q.find_item(ClipId(clip)).unwrap().1.effects.iter().map(|e| e.effect.clone()).collect()
    }

    #[test]
    fn added_effects_reorder_and_fixed_ones_stay() {
        let mut s = Session::default();
        s.execute("file.openDemoProject", json!({})).unwrap();
        let clip = s.active_sequence().unwrap().video_tracks[0].items[0].id.0;
        for fx in ["gaussian_blur", "tint", "mirror"] {
            s.execute("effects.apply", json!({"effect": fx, "clips": [clip]})).unwrap();
        }
        let before = names(&s, clip);
        let blur = before.iter().position(|e| e == "gaussian_blur").unwrap();
        s.execute("effects.select", json!({"clip": clip, "effect": blur})).unwrap();
        // Gaussian Blur to the end: Tint, Mirror, Gaussian Blur
        let r = s.execute("effects.move", json!({"clip": clip, "index": blur, "to": 2})).unwrap();
        assert_eq!(r["moved"], true);
        let after = names(&s, clip);
        let added: Vec<&str> = after.iter().map(String::as_str).filter(|e| ["gaussian_blur", "tint", "mirror"].contains(e)).collect();
        assert_eq!(added, ["tint", "mirror", "gaussian_blur"]);
        let fixed = |v: &[String]| v.iter().filter(|e| !["gaussian_blur", "tint", "mirror"].contains(&e.as_str())).cloned().collect::<Vec<_>>();
        assert_eq!(fixed(&after), fixed(&before), "Motion, Opacity… keep their places");
        let sel = s.state.selected_effect.clone().unwrap();
        assert_eq!(sel.effect, "gaussian_blur", "the selection follows its effect");
        // one undo step
        s.execute("edit.undo", json!({})).unwrap();
        assert_eq!(names(&s, clip), before);
    }

    #[test]
    fn fixed_effects_and_bad_requests_are_refused() {
        let mut s = Session::default();
        s.execute("file.openDemoProject", json!({})).unwrap();
        let clip = s.active_sequence().unwrap().video_tracks[0].items[0].id.0;
        s.execute("effects.apply", json!({"effect": "tint", "clips": [clip]})).unwrap();
        let motion = names(&s, clip).iter().position(|e| e == "motion").unwrap();
        for p in [
            json!({"clip": clip, "index": motion, "to": 0}),
            json!({"clip": clip, "index": 999, "to": 0}),
            json!({"clip": 999_999, "index": 0, "to": 0}),
            json!({"clip": clip}),
            json!({}),
        ] {
            assert!(s.execute("effects.move", p.clone()).is_err(), "{p}");
        }
        let tint = names(&s, clip).iter().position(|e| e == "tint").unwrap();
        let undo = s.history.undo.len();
        assert_eq!(s.execute("effects.move", json!({"clip": clip, "index": tint, "to": 99})).unwrap()["moved"], false, "already last");
        assert_eq!(s.history.undo.len(), undo);
    }
}
