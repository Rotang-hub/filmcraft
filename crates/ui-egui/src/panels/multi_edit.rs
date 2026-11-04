//! Fork feature: a change made in Effect Controls, the Program monitor's transform handles or
//! Essential Graphics goes to every selected clip that has the same property
//! (`edit.forSelection`, `crates/engine/src/multi_edit.rs`). With one clip selected, or a clip
//! outside the selection, the command runs as it always did.

use filmcraft_engine::{EngineError, Session};
use serde_json::{Value, json};

/// Run `cmd` with `p`, for the whole selection when that applies.
pub fn execute(session: &mut Session, cmd: &str, p: Value) -> Result<Value, EngineError> {
    let clip = p.get("clip").and_then(Value::as_u64);
    if !filmcraft_engine::multi_edit::applies(session, cmd, clip) {
        return session.execute(cmd, p);
    }
    // a drag's flags (a key or `true`) say the change continues one undo step
    let merge = match p.get("merge") {
        Some(Value::String(k)) => json!(k),
        Some(Value::Bool(true)) => json!(true),
        _ => json!(false),
    };
    let begin = p.get("begin").and_then(Value::as_bool).unwrap_or(false);
    let r = session.execute("edit.forSelection", json!({"command": cmd, "params": p, "merge": merge, "begin": begin}))?;
    Ok(r["result"].clone())
}
