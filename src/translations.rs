//! Translating subtitles through an addon's `translate()`, as jobs the host runs. An LLM answers
//! slowly, merges and drops lines, and slips into another script, so the host keeps ids and
//! timing to itself and sends batches of lines, each with some lines around it to read; it checks
//! every answer's ids and script, splits a batch that came back wrong, and asks again only for
//! what is still missing. Each addon runs one job at a time. A finished one is kept on disk, so
//! the same lines are translated once, and nothing needs a client to stay connected.
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value as Json};

use crate::app::{self, call_ctx, log, masked, warn, App};
use crate::sandbox;
use crate::subtitles::{cached, digest, keep, language, mostly_in, script, untagged, Script};

/// How a translation is cut and retried: lines a batch, lines of context on each side, how often
/// one line is asked before it's kept as it is, and how many failed calls in a row end the job: a
/// bad key fails every call, and splitting would otherwise ask hundreds of times. More than a
/// batch's halving down to one line (7 calls for 100) and that line's tries, so a model that
/// fails only on long batches gets through.
pub struct Plan {
    pub batch: usize,
    pub context: usize,
    pub line_tries: usize,
    pub failures: usize,
}

pub const PLAN: Plan = Plan { batch: 100, context: 10, line_tries: 3, failures: 10 };
/// What one translation may be: a long film has about 2,000 lines, 100 KB of text.
const MAX_LINES: usize = 10_000;
const MAX_TEXT: usize = 1 << 20;
/// Its request, as JSON.
pub const MAX_REQUEST: u64 = 2 << 20;
/// Jobs waiting for one addon, at most.
const MAX_QUEUED: usize = 16;
/// A finished job answers by its id this long; then its result answers from disk...
const JOB_TTL: Duration = Duration::from_secs(3600);
/// ...for this long, within RESULT_CACHE.
const RESULT_TTL: Duration = Duration::from_secs(30 * 86400);
const RESULT_CACHE: u64 = 64 << 20;

/// A translation asked for: `lines` are (id, text), each text one cue's, `\n` between its lines.
pub struct Request {
    pub from: Option<String>,
    pub to: String,
    pub title: Option<String>,
    pub lines: Vec<(String, String)>,
}

/// `POST .../translate`'s body, checked.
pub fn request(body: &str) -> Result<Request, String> {
    let body: Json = serde_json::from_str(body).map_err(|e| format!("the body is not JSON: {e}"))?;
    let code = |key: &str| match &body[key] {
        Json::Null => Ok(None),
        Json::String(code) => language(code).map(Some).ok_or(format!("{key}: {code} is no language code, such as en or my")),
        _ => Err(format!("{key} must be a language code, such as en or my")),
    };
    let (from, to) = (code("from")?, code("to")?.ok_or("give to, the language to translate into, such as my or pt-BR")?);
    let title = match &body["title"] {
        Json::Null => None,
        Json::String(title) => Some(title.chars().take(300).collect()),
        _ => return Err("title must be text".into()),
    };
    let list = body["lines"].as_array().filter(|lines| !lines.is_empty()).ok_or(r#"give lines: [{"id": "1", "text": "..."}, ...]"#)?;
    if list.len() > MAX_LINES {
        return Err(format!("at most {MAX_LINES} lines a translation"));
    }
    let (mut lines, mut ids, mut size) = (Vec::with_capacity(list.len()), HashSet::new(), 0);
    for line in list {
        let id = line["id"].as_str().filter(|id| !id.is_empty() && id.len() <= 64).ok_or("each line needs an id, as text of at most 64 bytes")?;
        let text = line["text"].as_str().ok_or(format!("line {id}: text must be text"))?;
        if !ids.insert(id) {
            return Err(format!("line {id} is given twice"));
        }
        size += text.len();
        lines.push((id.to_string(), text.to_string()));
    }
    if size > MAX_TEXT {
        return Err(format!("at most {} MB of text a translation", MAX_TEXT >> 20));
    }
    Ok(Request { from, to, title, lines })
}

/// What a translation came to: each line's text, in order, and the lines kept without an answer
/// that passed, with why: `{"id", "why"}`.
pub struct Translated {
    pub texts: Vec<String>,
    pub kept: Vec<Json>,
}

/// The texts an answer gives the batch's lines, by position; None unless its ids are exactly the
/// batch's, each once with text. Ids not asked about are dropped; an id as a number is its text.
fn answered(answer: &Json, group: &[usize], lines: &[(String, String)]) -> Result<Option<HashMap<usize, String>>, String> {
    let list = answer["lines"].as_array().ok_or(r#"translate() must return {lines: [{id, text}]}"#)?;
    let wanted: HashMap<&str, usize> = group.iter().map(|&at| (lines[at].0.as_str(), at)).collect();
    let mut texts = HashMap::new();
    for line in list {
        let id = match &line["id"] {
            Json::String(id) => id.clone(),
            Json::Number(id) => id.to_string(),
            _ => continue,
        };
        let Some(&at) = wanted.get(id.as_str()) else { continue };
        let Some(text) = line["text"].as_str().filter(|text| !text.trim().is_empty() || lines[at].1.trim().is_empty()) else { return Ok(None) };
        if texts.insert(at, text.to_string()).is_some() {
            return Ok(None);
        }
    }
    Ok((texts.len() == group.len()).then_some(texts))
}

/// Each line's text once it is answered, how often it was asked on its own, and the lines kept
/// without an answer that passed.
struct Answers<'a> {
    lines: &'a [(String, String)],
    texts: Vec<Option<String>>,
    tries: Vec<usize>,
    kept: Vec<Json>,
}

impl Answers<'_> {
    /// Line `at` asked once more, or kept as `text` once it was asked `most` times.
    fn again(&mut self, at: usize, text: String, why: String, most: usize, todo: &mut Vec<Vec<usize>>) {
        self.tries[at] += 1;
        match self.tries[at] == most {
            true => {
                self.texts[at] = Some(text);
                self.kept.push(json!({"id": self.lines[at].0, "why": why}));
            }
            false => todo.push(vec![at]),
        }
    }
}

/// A batch split in halves, the first asked first.
fn halves(group: Vec<usize>, todo: &mut Vec<Vec<usize>>) {
    let (head, tail) = group.split_at(group.len() / 2);
    todo.push(tail.to_vec());
    todo.push(head.to_vec());
}

/// Translates `request` through `call`, which takes one batch as `translate()` does and answers
/// what it returned; `done` is told how many lines are finished after each batch. A batch whose
/// answer's ids are wrong, or whose call failed, is split in halves down to single lines; lines
/// answered in a script not the target's are asked again on their own. A line still left out, in
/// the wrong script or failing after `line_tries` is kept, untranslated or as answered, and
/// listed. A call counts as failed unless it gave a line with letters worth keeping; `failures` of
/// them in a row end the job, as does keeping most lines as they were: a translator that answers
/// every call in the wrong script or with the wrong ids would otherwise be asked thousands of
/// times, and its job saved as done.
pub fn translate(request: &Request, plan: &Plan, call: &mut dyn FnMut(&Json) -> Result<Json, String>, done: &mut dyn FnMut(usize)) -> Result<Translated, String> {
    let lines = &request.lines;
    let target: Option<Script> = script(&request.to);
    // Whether an answer is its line as given, tags aside. That passes only for a line whose letters
    // are all the target's (names, into a Latin language) or fewer than 3 (OK): a model that
    // echoes English when asked for Myanmar would otherwise pass every line.
    let echo = |text: &str, line: &str| untagged(text).trim() == untagged(line).trim();
    let outside = |line: &str| untagged(line).chars().filter(|c| c.is_alphabetic() && target.is_some_and(|(_, ranges)| !ranges.iter().any(|r| r.contains(c)))).count();
    let fits = |text: &str, line: &str| match echo(text, line) {
        true => outside(line) == 0 || untagged(line).chars().filter(|c| c.is_alphabetic()).count() < 3,
        false => target.is_none_or(|(_, ranges)| mostly_in(text, ranges)),
    };
    let mut answers = Answers { lines, texts: vec![None; lines.len()], tries: vec![0; lines.len()], kept: Vec::new() };
    // Failed calls in a row, and whether any call gave a line worth keeping.
    let (mut failures, mut worked) = (0, false);
    let letters = |text: &str| untagged(text).chars().any(char::is_alphabetic);
    let name = target.map_or("", |(name, _)| name);
    for start in (0..lines.len()).step_by(plan.batch) {
        let end = (start + plan.batch).min(lines.len());
        // Batches to ask, the next one last.
        let mut todo: Vec<Vec<usize>> = vec![(start..end).collect()];
        while let Some(group) = todo.pop() {
            let (first, last, group_len) = (group[0], group[group.len() - 1], group.len());
            let shown = |range: std::ops::Range<usize>| range.map(|at| json!({"id": lines[at].0, "text": lines[at].1})).collect::<Vec<_>>();
            let batch = json!({
                "from": request.from, "to": request.to, "title": request.title,
                "lines": group.iter().map(|&at| json!({"id": lines[at].0, "text": lines[at].1})).collect::<Vec<_>>(),
                "before": shown(first.saturating_sub(plan.context)..first), "after": shown(last + 1..(last + 1 + plan.context).min(lines.len())),
            });
            // Why the call failed, unless it gave a line worth keeping.
            let mut failed: Option<String> = None;
            match call(&batch).and_then(|answer| answered(&answer, &group, lines)) {
                Err(error) => {
                    failed = Some(app::first_line(&error));
                    match group.len() {
                        // A line it fails on, where others went through (a model refusing one line).
                        1 if worked => answers.again(first, lines[first].1.clone(), format!("translate() failed on it {} times", plan.line_tries), plan.line_tries, &mut todo),
                        1 => todo.push(group),
                        _ => halves(group, &mut todo),
                    }
                }
                Ok(None) => {
                    failed = Some("its answer's ids were not the lines asked for".into());
                    match group.len() {
                        1 => answers.again(first, lines[first].1.clone(), format!("translate() left it out {} times", plan.line_tries), plan.line_tries, &mut todo),
                        _ => halves(group, &mut todo),
                    }
                }
                // A batch answered mostly as it was given is no translation, whatever its target: none
                // of it is kept, and it is split as a failed one.
                Ok(Some(texts)) if group.len() > 1 && {
                    let worded: Vec<usize> = group.iter().copied().filter(|&at| letters(&lines[at].1)).collect();
                    worded.iter().filter(|&&at| echo(&texts[&at], &lines[at].1)).count() * 2 > worded.len()
                } => {
                    failed = Some("it answered most lines as they were given".into());
                    halves(group, &mut todo);
                }
                Ok(Some(mut texts)) => {
                    let wrong: Vec<usize> = group.iter().copied().filter(|at| !fits(&texts[at], &lines[*at].1)).collect();
                    let kept: Vec<usize> = group.iter().copied().filter(|at| !wrong.contains(at)).collect();
                    // Worth keeping: a line with letters translated, not one it gave back as it was.
                    if !kept.iter().any(|&at| letters(&lines[at].1) && !echo(&texts[&at], &lines[at].1)) {
                        failed = Some(match target {
                            Some(_) => format!("its translations were not in {name} script"),
                            None => "it gave its lines back as they were".into(),
                        });
                    }
                    for at in kept {
                        answers.texts[at] = texts.remove(&at);
                    }
                    match wrong.len() {
                        0 => {}
                        _ if group.len() == 1 => answers.again(first, texts.remove(&first).unwrap_or_default(), format!("its translation is not in {name} script"), plan.line_tries, &mut todo),
                        n if n == group.len() => halves(group, &mut todo),
                        _ => todo.push(wrong),
                    }
                }
            }
            match failed {
                None => (failures, worked) = (0, true),
                // Once lines went through, a single line is bounded by its own tries: two refused
                // lines side by side would otherwise use up the job's failures and end it.
                Some(_) if worked && group_len == 1 => {}
                Some(why) => {
                    failures += 1;
                    if failures == plan.failures {
                        return Err(format!("translate() failed {failures} times in a row, last with: {why}"));
                    }
                }
            }
        }
        if let Some(first) = answers.kept.first().filter(|_| answers.kept.len() * 2 > end) {
            return Err(format!("translate() answered too few lines usably: {} of {end} were kept as they were, line {} as {}", answers.kept.len(), first["id"], first["why"]));
        }
        done(end);
    }
    Ok(Translated { texts: answers.texts.into_iter().map(Option::unwrap_or_default).collect(), kept: answers.kept })
}

struct Job {
    state: &'static str,
    done: usize,
    total: usize,
    /// `{lines, kept}` once done.
    result: Option<Json>,
    error: Option<String>,
    ended: Option<Instant>,
    request: Arc<Request>,
}

/// The translations asked for, by (addon, job), and each addon's queue: its first job runs.
#[derive(Default)]
pub struct Jobs(Mutex<Queues>);

#[derive(Default)]
struct Queues {
    jobs: HashMap<(String, String), Job>,
    queues: HashMap<String, VecDeque<String>>,
}

impl Jobs {
    /// The finished jobs of an addon whose settings or version changed.
    pub fn forget(&self, id: &str) {
        self.0.lock().unwrap().jobs.retain(|(addon, _), job| addon != id || job.ended.is_none());
    }
}

/// A finished translation as answered.
fn finished(job: &str, result: &Json) -> Json {
    let total = result["lines"].as_array().map_or(0, Vec::len);
    json!({"job": job, "state": "done", "done": total, "total": total, "lines": result["lines"], "kept": result["kept"]})
}

impl App {
    /// `POST /subtitles/{id}/translate`: a job for the addon's `translate()`, `202 {"job"}`; the
    /// same lines already translated, `200` with them; the same lines being translated, that job.
    pub fn translate(self: &Arc<Self>, id: &str, body: &str) -> Result<(u16, Json), (u16, String)> {
        let request = request(body).map_err(|e| (400, e))?;
        self.callable(id, "translate")?;
        let job = digest(&json!([request.from, request.to, request.lines]));
        if let Some(result) = cached(&app::dir(self, id).join("cache/translations"), &job, RESULT_TTL) {
            return Ok((200, finished(&job, &result)));
        }
        let mut queues = self.jobs.0.lock().unwrap();
        queues.jobs.retain(|_, job| job.ended.is_none_or(|at| at.elapsed() < JOB_TTL));
        let key = (id.to_string(), job.clone());
        if queues.jobs.get(&key).is_some_and(|job| job.state != "error") {
            return Ok((202, json!({"job": job})));
        }
        let queue = queues.queues.entry(id.to_string()).or_default();
        if queue.len() >= MAX_QUEUED {
            return Err((429, format!("{MAX_QUEUED} translations wait for this addon already: ask again once one is done")));
        }
        queue.push_back(job.clone());
        let first = queue.len() == 1;
        let total = request.lines.len();
        queues.jobs.insert(key, Job { state: "queued", done: 0, total, result: None, error: None, ended: None, request: Arc::new(request) });
        drop(queues);
        if first {
            let (app, id) = (self.clone(), id.to_string());
            std::thread::spawn(move || app.run_translations(&id));
        }
        Ok((202, json!({"job": job})))
    }

    /// `GET /subtitles/{id}/translate/{job}`.
    pub fn translation(&self, id: &str, job: &str) -> Result<Json, (u16, String)> {
        if !self.addons.lock().unwrap().contains_key(id) {
            return Err((404, "no such addon".into()));
        }
        let gone = || (404, "no such translation: it ended over an hour ago, or never began".to_string());
        if job.len() != 32 || !job.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(gone());
        }
        if let Some(j) = self.jobs.0.lock().unwrap().jobs.get(&(id.to_string(), job.to_string())) {
            return Ok(match (&j.result, &j.error) {
                (Some(result), _) => finished(job, result),
                (_, Some(error)) => json!({"job": job, "state": j.state, "done": j.done, "total": j.total, "error": error}),
                _ => json!({"job": job, "state": j.state, "done": j.done, "total": j.total}),
            });
        }
        cached(&app::dir(self, id).join("cache/translations"), job, RESULT_TTL).map(|result| finished(job, &result)).ok_or_else(gone)
    }

    /// Runs the addon's queued translations one after another, until none is left.
    fn run_translations(self: &Arc<Self>, id: &str) {
        loop {
            let (job, request) = {
                let mut queues = self.jobs.0.lock().unwrap();
                let Some(job) = queues.queues.get(id).and_then(|queue| queue.front().cloned()) else {
                    queues.queues.remove(id);
                    return;
                };
                let entry = queues.jobs.get_mut(&(id.to_string(), job.clone())).unwrap();
                entry.state = "running";
                (job, entry.request.clone())
            };
            let key = (id.to_string(), job.clone());
            let (started, mut calls) = (Instant::now(), 0);
            let progress = |done: usize| {
                if let Some(entry) = self.jobs.0.lock().unwrap().jobs.get_mut(&key) {
                    entry.done = done;
                }
            };
            // A panic would leave the queue with no worker, every later job waiting for good.
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                translate(&request, &PLAN, &mut |batch| {
                    calls += 1;
                    self.call_translate(id, batch)
                }, &mut { progress })
            }))
            .unwrap_or_else(|_| Err("rex-addons itself failed while translating: its log says where".into()));
            let (lines, from, to) = (request.lines.len(), request.from.as_deref().unwrap_or("its language"), &request.to);
            let result = result.map(|translated| {
                let lines: Vec<Json> = request.lines.iter().zip(translated.texts).map(|((id, _), text)| json!({"id": id, "text": text})).collect();
                json!({"lines": lines, "kept": translated.kept})
            });
            match &result {
                Ok(result) => {
                    let kept = result["kept"].as_array().map_or(0, Vec::len);
                    log(id, &format!("translated {lines} lines from {from} to {to} in {:.1} s, {calls} calls, {kept} kept as they were", started.elapsed().as_secs_f64()));
                    // Not for an addon removed meanwhile: its folder is gone.
                    if self.addons.lock().unwrap().contains_key(id) {
                        keep(&app::dir(self, id).join("cache/translations"), &job, result, RESULT_TTL, RESULT_CACHE);
                    }
                }
                Err(error) => warn(id, &format!("translating {lines} lines from {from} to {to} failed after {calls} calls: {error}")),
            }
            let mut queues = self.jobs.0.lock().unwrap();
            if let Some(entry) = queues.jobs.get_mut(&key) {
                entry.ended = Some(Instant::now());
                match result {
                    Ok(result) => (entry.state, entry.done, entry.result) = ("done", lines, Some(result)),
                    Err(error) => (entry.state, entry.error) = ("error", Some(error)),
                }
            }
            // Whether any is left is decided with the lock held: a job asked for after this
            // finds no queue and starts a worker of its own, never a second one beside this.
            if let Some(queue) = queues.queues.get_mut(id) {
                queue.pop_front();
                if queue.is_empty() {
                    queues.queues.remove(id);
                    return;
                }
            }
        }
    }

    /// One batch through the addon's `translate(batch, ctx)`, looked up again for each batch, so
    /// one disabled, removed or waiting for an update ends the job.
    fn call_translate(self: &Arc<Self>, id: &str, batch: &Json) -> Result<Json, String> {
        let (manifest, source, config, storage) = self.callable(id, "translate").map_err(|(_, error)| error)?;
        let env = self.env(id, &manifest, &config, &source, storage, true);
        sandbox::call(&env, "translate", &[batch.clone(), call_ctx(&manifest, &config)], sandbox::TRANSLATE).map_err(|error| {
            let error = masked(id, &error);
            warn(id, &format!("translate() failed: {error}"));
            error
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::Hosts;
    use crate::probe::Prober;
    use crate::sandbox::{Env, Limits, Storage};

    const QUICK: Limits = Limits { memory: 16 << 20, cpu: Duration::from_secs(2), wall: Duration::from_secs(10) };
    const SMALL: Plan = Plan { batch: 4, context: 2, line_tries: 3, failures: 6 };

    fn ask(to: &str, texts: &[&str]) -> Request {
        Request { from: Some("en".into()), to: to.into(), title: Some("His Girl Friday".into()), lines: texts.iter().enumerate().map(|(n, t)| ((n + 1).to_string(), t.to_string())).collect() }
    }

    /// `request` translated through `source`'s translate() in the real sandbox: what it came to,
    /// each batch sent, and the progress told.
    fn run(source: &str, request: &Request) -> (Result<Translated, String>, Vec<Json>, Vec<usize>) {
        let env = Env {
            addon: "translator", source, hosts: Hosts::default(), storage: Arc::new(Storage::open("/dev/null".into())),
            prober: Arc::new(Prober::new(None, "127.0.0.1:9".parse().unwrap())), calls: None,
        };
        let (mut sent, mut progress) = (Vec::new(), Vec::new());
        let result = translate(request, &SMALL, &mut |batch| {
            sent.push(batch.clone());
            sandbox::call(&env, "translate", &[batch.clone(), json!({})], QUICK)
        }, &mut |done| progress.push(done));
        (result, sent, progress)
    }

    fn ids(batch: &Json, key: &str) -> Vec<String> {
        batch[key].as_array().unwrap().iter().map(|line| line["id"].as_str().unwrap().to_string()).collect()
    }

    #[test]
    fn batches_go_with_their_context_and_lines_keep_their_breaks() {
        let source = "export default { translate(b) { return { lines: b.lines.map((l) => ({ id: l.id, text: `[${b.to}] ${l.text}` })) }; } };";
        let texts = ["Walter!", "Hildy,\nwait.", "3", "4", "5", "6", "7", "8", "9", "10"];
        let (result, sent, progress) = run(source, &ask("de", &texts));
        let result = result.unwrap();
        assert_eq!(result.texts[1], "[de] Hildy,\nwait.");
        assert_eq!(result.texts, texts.iter().map(|t| format!("[de] {t}")).collect::<Vec<_>>());
        assert!(result.kept.is_empty());
        assert_eq!(sent.iter().map(|b| ids(b, "lines")).collect::<Vec<_>>(), [vec!["1", "2", "3", "4"], vec!["5", "6", "7", "8"], vec!["9", "10"]]);
        assert_eq!((ids(&sent[1], "before"), ids(&sent[1], "after")), (vec!["3".to_string(), "4".into()], vec!["9".to_string(), "10".into()]));
        assert_eq!((&sent[0]["from"], &sent[0]["to"], &sent[0]["title"], ids(&sent[0], "before").len()), (&json!("en"), &json!("de"), &json!("His Girl Friday"), 0));
        assert_eq!(progress, [4, 8, 10]);
    }

    /// An answer that merges line 3 into line 2 and leaves 3 out, with an id never asked about: the
    /// batch is split until each part comes back whole, so line 2 never keeps 3's text.
    #[test]
    fn a_line_merged_or_left_out_splits_its_batch_until_every_id_comes_back() {
        let source = "export default { translate(b) {
            const out = b.lines.map((l) => ({ id: l.id, text: 'de ' + l.text }));
            const three = out.findIndex((l) => l.id === '3');
            if (three > 0 && b.lines.length > 1) { out[three - 1].text += ' ' + out[three].text; out.splice(three, 1); }
            return { lines: [...out, { id: 'note', text: 'translated by a model' }] };
        } };";
        let (result, sent, _) = run(source, &ask("de", &["one", "two", "three", "four", "five"]));
        let result = result.unwrap();
        assert_eq!(result.texts, ["de one", "de two", "de three", "de four", "de five"]);
        assert_eq!(sent.iter().map(|b| ids(b, "lines")).collect::<Vec<_>>(), [vec!["1", "2", "3", "4"], vec!["1", "2"], vec!["3", "4"], vec!["5"]]);
    }

    /// Asked for Myanmar, an answer mostly in Latin letters is asked again on its own; a short
    /// line answered as given (OK) passes; one that stays Latin is kept as answered, and listed.
    #[test]
    fn an_answer_in_the_wrong_script_is_asked_again() {
        let source = "export default { translate(b) { return { lines: b.lines.map((l) => ({ id: l.id,
            text: l.text === 'OK' ? 'OK' : l.text === 'stubborn' ? 'still in Latin' : b.lines.length > 1 ? 'in Latin ' + l.text : 'မြန်မာ' })) }; } };";
        let (result, sent, _) = run(source, &ask("my", &["hello", "OK", "stubborn", "bye"]));
        let result = result.unwrap();
        assert_eq!(result.texts, ["မြန်မာ", "OK", "still in Latin", "မြန်မာ"]);
        assert_eq!(result.kept, [json!({"id": "3", "why": "its translation is not in Myanmar script"})]);
        // All but OK came back in Latin: those asked again together, then in halves, then alone.
        assert_eq!(sent.iter().map(|b| ids(b, "lines")).collect::<Vec<_>>(),
            [vec!["1", "2", "3", "4"], vec!["1", "3", "4"], vec!["1"], vec!["3", "4"], vec!["3"], vec!["3"], vec!["3"], vec!["4"]]);
    }

    /// A translator that always throws, as one with a bad key does: the job ends after
    /// `failures` calls, with the addon's message, not after every split and retry.
    #[test]
    fn a_translator_that_always_fails_ends_the_job_soon() {
        let source = "export default { translate() { throw new Error('the API key is not valid\\nmore detail'); } };";
        let (result, sent, progress) = run(source, &ask("de", &["one", "two", "three", "four", "five"]));
        assert_eq!(result.err().unwrap(), "translate() failed 6 times in a row, last with: the API key is not valid");
        assert_eq!(sent.len(), SMALL.failures);
        assert!(progress.is_empty());
        // One that answers nothing in the shape asked counts the same.
        let (result, _, _) = run("export default { translate() { return 'Hallo'; } };", &ask("de", &["one"]));
        assert_eq!(result.err().unwrap(), "translate() failed 6 times in a row, last with: translate() must return {lines: [{id, text}]}");
    }

    /// At PLAN's own sizes, with `call` standing in for the addon: one that fails on batches over
    /// 30 lines gets through, a line it always fails on is kept as it is, and one that always
    /// fails ends at `failures` calls.
    #[test]
    fn plan_sizes_get_a_long_film_through_a_picky_translator() {
        let texts: Vec<String> = (1..=250).map(|n| format!("line {n}")).collect();
        let request = ask("de", &texts.iter().map(String::as_str).collect::<Vec<_>>());
        let echo = |batch: &Json| -> Result<Json, String> {
            let lines = batch["lines"].as_array().unwrap();
            if lines.len() > 30 {
                return Err("the model's reply was cut short".into());
            }
            if lines.iter().any(|line| line["id"] == "57") {
                return Err("the model refused this one".into());
            }
            Ok(json!({"lines": lines.iter().map(|l| json!({"id": l["id"], "text": format!("de {}", l["text"].as_str().unwrap())})).collect::<Vec<_>>()}))
        };
        let (mut calls, mut progress) = (0, Vec::new());
        let done = translate(&request, &PLAN, &mut |batch| {
            calls += 1;
            echo(batch)
        }, &mut |n| progress.push(n)).unwrap();
        assert_eq!(done.texts[0], "de line 1");
        assert_eq!(done.texts[56], "line 57");
        assert!(done.texts.iter().enumerate().all(|(n, text)| n == 56 || *text == format!("de line {}", n + 1)));
        assert_eq!(done.kept, [json!({"id": "57", "why": "translate() failed on it 3 times"})]);
        assert_eq!(progress, [100, 200, 250]);
        assert!(calls < 60, "{calls} calls");
        let mut calls = 0;
        let error = translate(&request, &PLAN, &mut |_| {
            calls += 1;
            Err("the API key is not valid".to_string())
        }, &mut |_| {}).err().unwrap();
        assert_eq!((error.as_str(), calls), ("translate() failed 10 times in a row, last with: the API key is not valid", PLAN.failures));
    }

    #[test]
    fn a_request_is_checked() {
        let line = |id: &str| json!({"id": id, "text": "Hi"});
        let ok = request(&json!({"from": "EN", "to": "pt-br", "title": "x", "lines": [line("1"), {"id": "2", "text": "a\nb"}]}).to_string()).unwrap();
        assert_eq!((ok.from.as_deref(), ok.to.as_str(), ok.lines[1].1.as_str()), (Some("en"), "pt-BR", "a\nb"));
        for (body, error) in [
            (json!({"lines": [line("1")]}), "give to, the language to translate into, such as my or pt-BR"),
            (json!({"to": "Burmese", "lines": [line("1")]}), "to: Burmese is no language code, such as en or my"),
            (json!({"to": "my", "lines": []}), r#"give lines: [{"id": "1", "text": "..."}, ...]"#),
            (json!({"to": "my", "lines": [line("1"), line("1")]}), "line 1 is given twice"),
            (json!({"to": "my", "lines": [{"id": 1, "text": "Hi"}]}), "each line needs an id, as text of at most 64 bytes"),
            (json!({"to": "my", "lines": [{"id": "1"}]}), "line 1: text must be text"),
            (json!({"to": "my", "lines": (0..MAX_LINES + 1).map(|n| line(&n.to_string())).collect::<Vec<_>>()}), "at most 10000 lines a translation"),
            (json!({"to": "my", "lines": [{"id": "1", "text": "x".repeat(MAX_TEXT + 1)}]}), "at most 1 MB of text a translation"),
        ] {
            assert_eq!(request(&body.to_string()).err().unwrap(), error);
        }
    }

    /// A film of `n` lines at PLAN's sizes through a stand-in translator that throws on any batch
    /// holding a `refused` line and translates the rest: what it came to, and how many calls.
    fn picky(n: usize, refused: &[&str]) -> (Result<Translated, String>, usize) {
        let texts: Vec<String> = (1..=n).map(|n| format!("line {n}")).collect();
        let request = ask("de", &texts.iter().map(String::as_str).collect::<Vec<_>>());
        let mut calls = 0;
        let result = translate(&request, &PLAN, &mut |batch| {
            calls += 1;
            let lines = batch["lines"].as_array().unwrap();
            if lines.iter().any(|line| refused.contains(&line["id"].as_str().unwrap())) {
                return Err("the model refused this one".into());
            }
            Ok(json!({"lines": lines.iter().map(|l| json!({"id": l["id"], "text": format!("de {}", l["text"].as_str().unwrap())})).collect::<Vec<_>>()}))
        }, &mut |_| {});
        (result, calls)
    }

    /// Two refused lines side by side in a long film (the review's 101 and 102, then 150 and 151):
    /// once lines went through, a single line's tries bound it, so each is kept and the job goes on.
    #[test]
    fn refused_lines_side_by_side_are_kept_and_the_job_goes_on() {
        for refused in [["101", "102"], ["150", "151"]] {
            let (result, calls) = picky(2000, &refused);
            let done = result.unwrap_or_else(|error| panic!("{refused:?}: {error}"));
            let kept: Vec<&str> = done.kept.iter().map(|k| k["id"].as_str().unwrap()).collect();
            assert_eq!(kept, refused, "{calls} calls");
            assert_eq!(done.texts[0], "de line 1");
        }
    }

    /// `n` lines asked for in Myanmar through a stand-in translator: `answer(id, text)` is each
    /// line's translation. What it came to, and how many calls and lines it was sent.
    fn burmese(n: usize, answer: impl Fn(&Json, &str) -> Json) -> (Result<Translated, String>, usize, usize) {
        let texts: Vec<String> = (1..=n).map(|n| format!("<font color=\"#ffff00\">line {n}</font>")).collect();
        let request = ask("my", &texts.iter().map(String::as_str).collect::<Vec<_>>());
        let (mut calls, mut sent) = (0, 0);
        let result = translate(&request, &PLAN, &mut |batch| {
            calls += 1;
            let lines = batch["lines"].as_array().unwrap();
            sent += lines.len();
            Ok(json!({"lines": lines.iter().map(|l| json!({"id": answer(&l["id"], l["text"].as_str().unwrap()), "text": answer(&Json::Null, l["text"].as_str().unwrap())})).collect::<Vec<_>>()}))
        }, &mut |_| {});
        (result, calls, sent)
    }

    /// Answers that parse but are no use: ids as numbers (taken, as the same ids), Myanmar inside
    /// the line's own tags (taken: tags are no letters of any script), and a model that can't
    /// write Myanmar at all (the job ends within a few calls, not after thousands, and nothing
    /// is kept as done).
    #[test]
    fn answers_that_parse_but_are_no_use_end_the_job() {
        // Ids as numbers; the line's tags kept around Myanmar text.
        let (result, calls, _) = burmese(250, |id, text| match id {
            Json::Null => json!(text.replace("line", "\u{1019}\u{103c}\u{1014}\u{103a}\u{1019}\u{102c}")),
            id => json!(id.as_str().unwrap().parse::<u64>().unwrap()),
        });
        let done = result.unwrap();
        assert!(done.kept.is_empty(), "{:?}", &done.kept[..3.min(done.kept.len())]);
        assert_eq!(done.texts[0], "<font color=\"#ffff00\">\u{1019}\u{103c}\u{1014}\u{103a}\u{1019}\u{102c} 1</font>");
        assert_eq!(calls, 3);
        // In Latin, always: a model that can't write the target script.
        let (result, calls, sent) = burmese(2000, |id, text| match id {
            Json::Null => json!(format!("in Latin: {text}")),
            id => id.clone(),
        });
        assert!(result.is_err() && calls <= PLAN.failures + 2, "{calls} calls, {sent} lines sent: {:?}", result.map(|d| d.kept.len()));
        // Myanmar for one line of five: most lines kept as they were fails the job, saving nothing.
        let (result, _, _) = burmese(5, |id, text| match id {
            Json::Null if text.trim_end_matches("</font>").ends_with(['0', '3', '6', '9']) => json!("\u{1019}\u{103c}\u{1014}\u{103a}\u{1019}\u{102c}"),
            Json::Null => json!(format!("in Latin: {text}")),
            id => id.clone(),
        });
        let error = result.err().unwrap();
        assert!(error.starts_with("translate() answered too few lines usably: "), "{error}");
    }

    /// A model that answers each line as it was given: no translation, so the job fails within a
    /// few calls, into Myanmar or into German, rather than be saved as done for 30 days.
    #[test]
    fn a_translator_that_echoes_its_lines_fails() {
        for to in ["my", "de"] {
            let texts: Vec<String> = (1..=250).map(|n| format!("{{\\an8}}Line number {n}")).collect();
            let mut calls = 0;
            let result = translate(&ask(to, &texts.iter().map(String::as_str).collect::<Vec<_>>()), &PLAN, &mut |batch| {
                calls += 1;
                Ok(json!({"lines": batch["lines"]}))
            }, &mut |_| {});
            assert!(result.is_err() && calls <= PLAN.failures + 2, "{to}: {calls} calls: {:?}", result.map(|d| d.kept.len()));
        }
    }
}
