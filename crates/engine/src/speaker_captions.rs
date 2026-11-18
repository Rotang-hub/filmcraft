//! Fork: automatic captions per speaker, as head-caption graphics.
//!
//! Each person speaks on their own audio track (their own recording). `captions.bySpeaker`
//! transcribes the clips of every assigned track on its own (so people talking at once are still
//! written down separately), groups each person's words into short phrases, and places every
//! phrase as a graphic made from that person's head-caption template (`<name>_머리자막.fcgt` in
//! the template folder, or a template named in the request) with the phrase as its text.
//!
//! **Talking at once.** Phrases are laid out in start order: a phrase goes on the first caption
//! track ("Speaker Captions 1", "2"…) free for its whole length. So whoever started first stays
//! on track 1 in the template's own place (the main speaker), and anyone talking over them goes on
//! the next track, raised by `spacing` pixels per track so the captions stack instead of covering
//! each other.
//!
//! **Spell check.** `captions.exportText` writes the text of those graphics (one line per
//! caption: clip id, time, speaker, text) to a file the user can have checked and corrected;
//! `captions.importText` writes the corrected text back, one undo step.
//!
//! The work splits so a host can transcribe off the UI thread: [`audio_jobs`] (needs the
//! session), [`transcribe_job`] (pure, `Send`), then `captions.placeSpeakerPhrases` with the
//! phrases. `captions.bySpeaker` runs all three in one call.
//!
//! Kept in its own file so upstream updates rarely touch it.

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::{Value, json};

use filmcraft_media::SharedSource;
use filmcraft_project::graphic_design::GraphicMeta;
use filmcraft_project::gtemplate::{ControlKind, layer_uid};
use filmcraft_project::keyframe::{Param, ParamValue};
use filmcraft_project::{ClipId, EffectInstance, Track, TrackId, TrackKind};
use filmcraft_speech::{Options, Transcriber};
use filmcraft_time::{Tick, TimeRange};

use crate::commands::{CommandSpec, bad, f64_p, has_seq, str_p, track_p, u64_p};
use crate::{EngineError, Result, Session};

/// Caption tracks are named this, numbered from 1 (track 1 holds the main speaker).
pub const TRACK_NAME: &str = "Speaker Captions";
/// File name suffix of a person's head-caption template.
pub const TEMPLATE_SUFFIX: &str = "_머리자막.fcgt";
/// Default most characters in one caption (Korean reads in short bursts).
pub const MAX_CHARS: usize = 18;
/// Longest audio read for one clip (3 hours), to keep memory bounded.
const MAX_SECONDS: f64 = 3.0 * 3600.0;
/// Pause that ends a phrase.
const PHRASE_GAP: f64 = 0.6;
/// A caption stays up this long after its last word (unless the next one starts).
const HOLD: f64 = 0.3;
/// Shortest caption.
const MIN_SECONDS: f64 = 0.6;
/// Most captions placed in one call.
const MAX_PHRASES: usize = 20_000;

/// One clip's audio to transcribe, and where its words land on the timeline.
#[derive(Clone)]
pub struct AudioJob {
    pub speaker: String,
    pub template: String,
    pub source: SharedSource,
    pub stream: usize,
    /// Source channels to mix (empty: all).
    pub channels: Vec<u16>,
    /// Media time of the clip's first sample and how much media it plays.
    pub media_in: Tick,
    pub media_len: Tick,
    /// Where the clip starts on the timeline, and its speed (> 0).
    pub seq_start: Tick,
    pub speed: f64,
}

/// A caption to place: who said it, which template, when, and what.
#[derive(Clone, Debug, PartialEq)]
pub struct Phrase {
    pub speaker: String,
    pub template: String,
    pub start: Tick,
    pub end: Tick,
    pub text: String,
}

impl Phrase {
    pub fn to_json(&self) -> Value {
        json!({"speaker": self.speaker, "template": self.template, "start": self.start.0, "end": self.end.0, "text": self.text})
    }

    fn from_json(v: &Value) -> Option<Phrase> {
        let start = Tick(v.get("start")?.as_i64()?);
        let end = Tick(v.get("end")?.as_i64()?);
        let text = v.get("text")?.as_str()?.trim().to_string();
        (end > start && start >= Tick::ZERO && !text.is_empty()).then(|| Phrase {
            speaker: v.get("speaker").and_then(Value::as_str).unwrap_or_default().to_string(),
            template: v.get("template").and_then(Value::as_str).unwrap_or_default().to_string(),
            start,
            end,
            text,
        })
    }
}

/// The template for `name`: `explicit` when given, else `<dir>/<name>_머리자막.fcgt` when that
/// file exists, else a library template called `<name>_머리자막` or `<name> 머리자막`.
pub fn template_for(s: &Session, name: &str, explicit: Option<&str>, dir: Option<&str>) -> Result<String> {
    if let Some(t) = explicit.filter(|t| !t.trim().is_empty()) {
        return Ok(t.to_string());
    }
    if let Some(d) = dir.filter(|d| !d.trim().is_empty()) {
        let path = Path::new(d).join(format!("{name}{TEMPLATE_SUFFIX}"));
        if path.is_file() {
            return Ok(path.to_string_lossy().to_string());
        }
    }
    let stem = TEMPLATE_SUFFIX.trim_end_matches(".fcgt");
    for key in [format!("{name}{stem}"), format!("{name} {}", stem.trim_start_matches('_'))] {
        if crate::graphic_templates::find_template(s, &key).is_ok() {
            return Ok(key);
        }
    }
    Err(bad("captions.bySpeaker", format!("no head-caption template for {name} (looked for {name}{TEMPLATE_SUFFIX})")))
}

/// The speakers of a request: `[{track, name, template?}]`, with each template resolved.
pub fn speakers_p(s: &Session, p: &Value) -> Result<Vec<(TrackId, String, String)>> {
    let list = p.get("speakers").and_then(Value::as_array).ok_or_else(|| bad("captions.bySpeaker", "need `speakers`: [{track, name}]"))?;
    let dir = str_p(p, "templateDir");
    let mut out = Vec::new();
    for sp in list.iter().take(64) {
        let Some(track) = track_p(s, sp, "track", "captions.bySpeaker")? else {
            return Err(bad("captions.bySpeaker", "each speaker needs `track`"));
        };
        let name = str_p(sp, "name").map(str::trim).filter(|n| !n.is_empty()).ok_or_else(|| bad("captions.bySpeaker", "each speaker needs `name`"))?;
        let template = template_for(s, name, str_p(sp, "template"), dir)?;
        out.push((track, name.to_string(), template));
    }
    if out.is_empty() {
        return Err(bad("captions.bySpeaker", "no speakers"));
    }
    Ok(out)
}

/// The audio of every enabled clip on each speaker's track (media items only).
pub fn audio_jobs(s: &Session, speakers: &[(TrackId, String, String)]) -> Result<Vec<AudioJob>> {
    let q = s.active_sequence().ok_or(EngineError::NoSequence)?;
    let mut jobs = Vec::new();
    for (track, speaker, template) in speakers {
        let Some(t) = q.audio_tracks.iter().find(|t| t.id == *track) else {
            return Err(bad("captions.bySpeaker", "speakers' tracks must be audio tracks"));
        };
        for it in t.items.iter().filter(|i| i.enabled && !i.reverse && i.speed > 0.0 && i.speed.is_finite()) {
            if s.project.resolve_media(it.item).is_none() {
                continue;
            }
            let Some(source) = s.source(it.item) else { continue };
            jobs.push(AudioJob {
                speaker: speaker.clone(),
                template: template.clone(),
                source,
                stream: it.audio_stream,
                channels: it.source_channels.clone(),
                media_in: it.source_in,
                media_len: Tick::from_seconds_f64(it.duration.seconds() * it.speed),
                seq_start: it.start,
                speed: it.speed,
            });
        }
    }
    Ok(jobs)
}

/// Mono 16 kHz audio of a job (None: the clip has no readable audio).
fn job_audio(job: &AudioJob) -> Option<Vec<f32>> {
    let sr = filmcraft_speech::SAMPLE_RATE;
    let start = job.media_in.to_units_floor(sr as i64).max(0);
    let secs = job.media_len.seconds().clamp(0.0, MAX_SECONDS);
    let len = (secs * sr as f64) as usize;
    if len == 0 {
        return None;
    }
    let buf = job.source.audio_stream(job.stream, start, len, sr).ok()?;
    let picked: Vec<Vec<f32>> =
        if job.channels.is_empty() { buf.channels } else { job.channels.iter().filter_map(|c| buf.channels.get(*c as usize).cloned()).collect() };
    if picked.is_empty() {
        return None;
    }
    Some(filmcraft_speech::downmix(&picked))
}

/// Transcribe one job and group its words into phrases of at most `max_chars` characters.
/// Runs without the session (a host may call it on a worker thread).
pub fn transcribe_job(t: &dyn Transcriber, job: &AudioJob, opts: &Options, max_chars: usize, progress: &mut dyn FnMut(f32) -> bool) -> Result<Vec<Phrase>> {
    let Some(audio) = job_audio(job) else { return Ok(Vec::new()) };
    let mut tr = t.transcribe(&audio, opts, &mut |f, _| progress(f)).map_err(crate::transcript::speech_err)?;
    tr.normalize();
    let words: Vec<(Tick, Tick, String)> = tr
        .words
        .iter()
        .map(|w| {
            (
                job.seq_start + Tick::from_seconds_f64(w.start.seconds() / job.speed),
                job.seq_start + Tick::from_seconds_f64(w.end.seconds() / job.speed),
                w.text.trim().to_string(),
            )
        })
        .filter(|(_, _, t)| !t.is_empty())
        .collect();
    Ok(group(&words, max_chars)
        .into_iter()
        .map(|(start, end, text)| Phrase { speaker: job.speaker.clone(), template: job.template.clone(), start, end, text })
        .collect())
}

/// Words (timeline start, end, text) in time order → phrases (start, end, text): a phrase ends
/// at a pause, after sentence-ending punctuation, or before it would pass `max_chars`.
pub fn group(words: &[(Tick, Tick, String)], max_chars: usize) -> Vec<(Tick, Tick, String)> {
    let max_chars = max_chars.clamp(4, 200);
    let mut out: Vec<(Tick, Tick, String)> = Vec::new();
    let mut cur: Option<(Tick, Tick, String)> = None;
    for (s, e, w) in words {
        let joined = match &cur {
            Some((_, pe, text)) => {
                let gap = (*s - *pe).seconds();
                let ended = text.ends_with(['.', '?', '!', '。', '…']);
                let long = text.chars().count() + 1 + w.chars().count() > max_chars;
                !(gap >= PHRASE_GAP || ended || long)
            }
            None => false,
        };
        if joined {
            if let Some((_, pe, text)) = cur.as_mut() {
                text.push(' ');
                text.push_str(w);
                *pe = (*e).max(*pe);
            }
        } else {
            if let Some(c) = cur.take() {
                out.push(c);
            }
            cur = Some((*s, (*e).max(*s), w.clone()));
        }
    }
    out.extend(cur);
    // hold each caption a little after its last word, never into the next one
    let n = out.len();
    for i in 0..n {
        let next = out.get(i + 1).map(|p| p.0);
        if let Some(p) = out.get_mut(i) {
            let mut end = p.1 + Tick::from_seconds_f64(HOLD);
            end = end.max(p.0 + Tick::from_seconds_f64(MIN_SECONDS));
            if let Some(nx) = next {
                end = end.min(nx).max(p.1);
            }
            p.1 = end;
        }
    }
    out
}

/// A template made ready to copy: its layers for the sequence frame, its meta, and where its
/// caption text goes (layer uid, parameter).
struct Prepared {
    layers: Vec<EffectInstance>,
    meta: GraphicMeta,
    text: (u64, String),
}

fn prepare(s: &Session, key: &str, frame: (u32, u32)) -> Result<Prepared> {
    let entry = crate::graphic_templates::find_template(s, key)?;
    let t = entry.template;
    crate::graphic_templates::register_fonts(&t);
    let text = t
        .controls
        .iter()
        .find(|c| c.kind == ControlKind::Text)
        .map(|c| (c.layer, c.param.clone()))
        .ok_or_else(|| bad("captions.bySpeaker", format!("template `{}` has no text property", t.name)))?;
    let (layers, meta) = t.instantiate(frame);
    Ok(Prepared { layers, meta, text })
}

/// Place `phrases` as graphics (see the module docs), one undo step.
pub fn place(s: &mut Session, phrases: &[Phrase], spacing: f64) -> Result<Value> {
    let q = s.active_sequence().ok_or(EngineError::NoSequence)?;
    let (w, h, rate) = (q.settings.width, q.settings.height, q.settings.frame_rate);
    let mut prepared: BTreeMap<String, Prepared> = BTreeMap::new();
    for p in phrases {
        if !prepared.contains_key(&p.template) {
            prepared.insert(p.template.clone(), prepare(s, &p.template, (w, h))?);
        }
    }
    let mut order: Vec<&Phrase> = phrases.iter().take(MAX_PHRASES).collect();
    order.sort_by_key(|p| (p.start, p.end));
    let spacing = if spacing.is_finite() { spacing.clamp(0.0, h as f64) } else { 0.0 };
    let seq_id = s.state.active_sequence.ok_or(EngineError::NoSequence)?;
    let placed = s.edit("Speaker Captions", move |pr, st| {
        let src = crate::graphics::graphic_source(pr, w, h, rate);
        let mut made: Vec<(ClipId, usize, String)> = Vec::new();
        for ph in order {
            let Some(prep) = prepared.get(&ph.template) else { continue };
            let start = rate.snap(ph.start);
            let end = rate.snap(ph.end).max(start + rate.frame_duration());
            let range = TimeRange::new(start, end - start);
            let mut ti = pr
                .make_track_item(src, TrackKind::Video, start, TimeRange::new(Tick::ZERO, end - start), rate)
                .ok_or_else(|| bad("captions.bySpeaker", "bad graphic item"))?;
            ti.name = format!("{}: {}", ph.speaker, ph.text);
            let mut layers = prep.layers.clone();
            for e in &mut layers {
                if layer_uid(e) == prep.text.0 {
                    e.params.insert(prep.text.1.clone(), Param::new(ParamValue::Text(ph.text.clone())));
                }
            }
            ti.effects.extend(layers);
            filmcraft_project::graphic::ensure_layer_uids(&mut ti.effects);
            for e in &mut ti.effects {
                filmcraft_project::resolve_auto_points(e, (w, h), (w, h));
            }
            ti.graphic = Some(Box::new(prep.meta.clone()));
            let q = pr.sequence_mut(seq_id).ok_or(EngineError::NoSequence)?;
            // the first caption track free for the whole phrase (created when there is none)
            let mut lane = 0usize;
            let idx = loop {
                let name = format!("{TRACK_NAME} {}", lane + 1);
                match q.video_tracks.iter().position(|t| t.name == name) {
                    Some(i) if q.video_tracks[i].items.iter().all(|it| !it.range().overlaps(&range)) && !q.video_tracks[i].locked => break i,
                    Some(_) => lane += 1,
                    None => {
                        let id = TrackId(pr.alloc_id());
                        let q = pr.sequence_mut(seq_id).ok_or(EngineError::NoSequence)?;
                        q.video_tracks.push(Track::new(id, TrackKind::Video, name));
                        break q.video_tracks.len() - 1;
                    }
                }
                if lane > 64 {
                    return Err(bad("captions.bySpeaker", "too many people talking at once"));
                }
            };
            // above the main speaker: raised one step per track
            if lane > 0
                && let Some(m) = ti.effects.iter_mut().find(|e| e.effect == "motion")
                && let Some(p) = m.params.get_mut("position")
                && let ParamValue::Vec2(v) = &mut p.value
            {
                v.y -= spacing * lane as f64;
            }
            let id = ti.id;
            let q = pr.sequence_mut(seq_id).ok_or(EngineError::NoSequence)?;
            let track = q.video_tracks.get_mut(idx).ok_or_else(|| bad("captions.bySpeaker", "caption track"))?;
            track.items.push(ti);
            track.sort();
            made.push((id, lane, ph.speaker.clone()));
        }
        let q = pr.sequence_mut(seq_id).ok_or(EngineError::NoSequence)?;
        q.check().map_err(EngineError::Other)?;
        st.selection = made.iter().map(|m| m.0).collect();
        Ok(made)
    })?;
    let overlapping = placed.iter().filter(|m| m.1 > 0).count();
    Ok(json!({
        "captions": placed.len(),
        "overlapping": overlapping,
        "clips": placed.iter().map(|m| json!({"clip": m.0.0, "lane": m.1, "speaker": m.2})).collect::<Vec<_>>(),
    }))
}

/// Recognition options: `language` (`None` or `"auto"`: detect; default Korean). One person per
/// track, so no voice clustering.
pub fn options(language: Option<&str>) -> Options {
    let language = match language {
        Some(l) if l == "auto" || l.is_empty() => None,
        Some(l) => Some(l.to_string()),
        None => Some("ko".to_string()),
    };
    Options { language, diarize: false, max_speakers: 1 }
}

/// Transcribe `jobs` in turn; `progress(job index, job count, fraction of that job)` returning
/// false cancels. Runs without the session (for a worker thread).
pub fn transcribe_jobs(
    t: &dyn Transcriber,
    jobs: &[AudioJob],
    language: Option<&str>,
    max_chars: usize,
    progress: &mut dyn FnMut(usize, usize, f32) -> bool,
) -> Result<Vec<Phrase>> {
    let opts = options(language);
    let mut out = Vec::new();
    for (i, job) in jobs.iter().enumerate() {
        if !progress(i, jobs.len(), 0.0) {
            return Err(EngineError::Other("cancelled".into()));
        }
        out.extend(transcribe_job(t, job, &opts, max_chars, &mut |f| progress(i, jobs.len(), f))?);
    }
    Ok(out)
}

/// Load the catalogue speech model `id` from the models folder (without a session: for a worker
/// thread).
pub fn load_model(id: &str) -> Result<std::sync::Arc<dyn Transcriber>> {
    if filmcraft_speech::models::find(id).is_none() {
        return Err(crate::transcript::speech_err(filmcraft_speech::SpeechError::UnknownModel(id.into())));
    }
    if !filmcraft_speech::available() {
        return Err(EngineError::Other(crate::transcript::NO_SPEECH.into()));
    }
    let dir = crate::transcript::models_dir().ok_or_else(|| EngineError::Other("no data directory for speech models".into()))?;
    filmcraft_speech::load(&dir, id).map_err(crate::transcript::speech_err)
}

/// The speech models: (id, name, download size in bytes, installed).
pub fn models() -> Vec<(&'static str, &'static str, u64, bool)> {
    let dir = crate::transcript::models_dir();
    filmcraft_speech::models::catalogue()
        .iter()
        .map(|m| (m.id, m.name, m.size(), dir.as_ref().is_some_and(|d| filmcraft_speech::models::installed(d, m))))
        .collect()
}

/// Whether this build can recognise speech and download models.
pub fn speech_available() -> bool {
    filmcraft_speech::available() && cfg!(feature = "speech-download")
}

/// Download speech model `id` (`progress(done, total)` returning false cancels).
pub fn download_model(id: &str, progress: &mut dyn FnMut(u64, u64) -> bool) -> Result<()> {
    let m = filmcraft_speech::models::find(id).ok_or_else(|| crate::transcript::speech_err(filmcraft_speech::SpeechError::UnknownModel(id.into())))?;
    let dir = crate::transcript::models_dir().ok_or_else(|| EngineError::Other("no data directory for speech models".into()))?;
    #[cfg(feature = "speech-download")]
    {
        filmcraft_speech::models::download(&dir, m, &mut |done, total, _| progress(done, total)).map_err(crate::transcript::speech_err)
    }
    #[cfg(not(feature = "speech-download"))]
    {
        let _ = (m, dir, progress);
        Err(EngineError::Other("model downloads are not available in this build".into()))
    }
}

/// Default raise per extra speaker: a caption line and a gap, scaled to the frame.
pub fn default_spacing(s: &Session) -> f64 {
    s.active_sequence().map_or(110.0, |q| 110.0 * q.settings.height as f64 / 1080.0)
}

fn by_speaker(s: &mut Session, p: &Value) -> Result<Value> {
    let speakers = speakers_p(s, p)?;
    let jobs = audio_jobs(s, &speakers)?;
    let t = crate::transcript::transcriber(s, p)?;
    let max_chars = u64_p(p, "maxChars").map_or(MAX_CHARS, |n| n as usize);
    let phrases = transcribe_jobs(&*t, &jobs, str_p(p, "language"), max_chars, &mut |_, _, _| true)?;
    if phrases.is_empty() {
        return Err(bad("captions.bySpeaker", "no speech found on the speakers' tracks"));
    }
    let spacing = f64_p(p, "spacing").unwrap_or_else(|| default_spacing(s));
    place(s, &phrases, spacing)
}

fn place_phrases(s: &mut Session, p: &Value) -> Result<Value> {
    let list = p.get("phrases").and_then(Value::as_array).ok_or_else(|| bad("captions.placeSpeakerPhrases", "need `phrases`"))?;
    let phrases: Vec<Phrase> = list.iter().take(MAX_PHRASES).filter_map(Phrase::from_json).collect();
    if phrases.is_empty() {
        return Err(bad("captions.placeSpeakerPhrases", "no phrases"));
    }
    let spacing = f64_p(p, "spacing").unwrap_or_else(|| default_spacing(s));
    place(s, &phrases, spacing)
}

// ---------------------------------------------------------------- spell check round trip

/// The text property of a template graphic: (effect index, parameter).
fn text_slot(it: &filmcraft_project::TrackItem) -> Option<(usize, String)> {
    let link = it.graphic.as_ref()?.template.as_ref()?;
    let c = link.controls.iter().find(|c| c.kind == ControlKind::Text)?;
    let ei = it.effects.iter().position(|e| filmcraft_project::graphic::is_layer(e) && layer_uid(e) == c.layer)?;
    Some((ei, c.param.clone()))
}

fn text_of(it: &filmcraft_project::TrackItem) -> Option<String> {
    let (ei, param) = text_slot(it)?;
    match &it.effects.get(ei)?.params.get(&param)?.value {
        ParamValue::Text(t) => Some(t.clone()),
        _ => None,
    }
}

fn timecode(t: Tick) -> String {
    let ms = (t.seconds() * 1000.0).round().max(0.0) as u64;
    format!("{:02}:{:02}:{:02}.{:03}", ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000)
}

/// One caption per line: `clip id <TAB> time <TAB> speaker <TAB> text`, in time order. Only the
/// text column is read back.
fn export_text(s: &mut Session, p: &Value) -> Result<Value> {
    let path = str_p(p, "path").ok_or_else(|| bad("captions.exportText", "need `path`"))?;
    let q = s.active_sequence().ok_or(EngineError::NoSequence)?;
    let mut rows: Vec<(Tick, u64, String, String)> = Vec::new();
    for t in &q.video_tracks {
        for it in &t.items {
            if let Some(text) = text_of(it) {
                let speaker = it.name.split_once(": ").map_or("", |(a, _)| a).to_string();
                rows.push((it.start, it.id.0, speaker, text.replace(['\t', '\r'], " ").replace('\n', "\\n")));
            }
        }
    }
    rows.sort();
    if rows.is_empty() {
        return Err(bad("captions.exportText", "no template captions in this sequence"));
    }
    let mut out = String::from("# FilmCraft captions: id\ttime\tspeaker\ttext — edit only the text column\n");
    for (t, id, sp, text) in &rows {
        out.push_str(&format!("{id}\t{}\t{sp}\t{text}\n", timecode(*t)));
    }
    std::fs::write(path, out).map_err(|e| EngineError::Other(format!("{path}: {e}")))?;
    Ok(json!({"path": path, "captions": rows.len()}))
}

/// Read the corrected file; set every caption whose text changed, one undo step.
fn import_text(s: &mut Session, p: &Value) -> Result<Value> {
    let path = str_p(p, "path").ok_or_else(|| bad("captions.importText", "need `path`"))?;
    let meta = std::fs::metadata(path).map_err(|e| EngineError::Other(format!("{path}: {e}")))?;
    if meta.len() > 64 << 20 {
        return Err(bad("captions.importText", "the file is too large"));
    }
    let raw = std::fs::read(path).map_err(|e| EngineError::Other(format!("{path}: {e}")))?;
    let text = String::from_utf8_lossy(&raw);
    let mut wanted: BTreeMap<u64, String> = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim_start_matches('\u{feff}');
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let mut cols = line.splitn(4, '\t');
        let (Some(id), Some(_), Some(_), Some(t)) = (cols.next(), cols.next(), cols.next(), cols.next()) else { continue };
        if let Ok(id) = id.trim().parse::<u64>() {
            wanted.insert(id, t.trim().replace("\\n", "\n"));
        }
    }
    if wanted.is_empty() {
        return Err(bad("captions.importText", "no caption lines in the file (id, time, speaker, text separated by tabs)"));
    }
    let (changed, missing) = s.edit_sequence("Import Caption Text", move |q, _, _| {
        let mut changed = 0usize;
        let mut found = 0usize;
        for t in q.video_tracks.iter_mut() {
            for it in t.items.iter_mut() {
                let Some(new) = wanted.get(&it.id.0) else { continue };
                found += 1;
                let Some((ei, param)) = text_slot(it) else { continue };
                let Some(e) = it.effects.get_mut(ei) else { continue };
                let same = matches!(e.params.get(&param).map(|p| &p.value), Some(ParamValue::Text(t)) if t == new);
                if !same && !new.is_empty() {
                    e.params.insert(param, Param::new(ParamValue::Text(new.clone())));
                    if let Some((sp, _)) = it.name.split_once(": ") {
                        it.name = format!("{sp}: {new}");
                    }
                    changed += 1;
                }
            }
        }
        Ok((changed, wanted.len().saturating_sub(found)))
    })?;
    Ok(json!({"changed": changed, "notFound": missing}))
}

type Run = fn(&mut Session, &Value) -> Result<Value>;

fn spec(id: &'static str, label: &'static str, params: &'static str, run: Run) -> CommandSpec {
    CommandSpec { id, label, menu: &[], shortcut: None, params, enabled: has_seq, run, journal: true, named: crate::commands::NamedKeys::NONE }
}

pub fn commands() -> Vec<CommandSpec> {
    vec![
        spec(
            "captions.bySpeaker",
            "Speaker Captions…",
            r#"{"speakers":[{"track":"A4","name":"라스","template":path|name?}],"templateDir":path?,"model":"whisper-small"?,"language":"ko"|"auto"?,"maxChars":n?,"spacing":px?}"#,
            by_speaker,
        ),
        spec(
            "captions.placeSpeakerPhrases",
            "Place Speaker Captions",
            r#"{"phrases":[{"speaker":str,"template":path|name,"start":tick,"end":tick,"text":str}],"spacing":px?}"#,
            place_phrases,
        ),
        spec("captions.exportText", "Export Caption Text…", r#"{"path":str}"#, export_text),
        spec("captions.importText", "Import Caption Text…", r#"{"path":str}"#, import_text),
    ]
}

#[cfg(test)]
#[path = "speaker_captions_tests.rs"]
mod tests;
