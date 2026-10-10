//! One Terminal's Permission Prompt state (capability `agent.prompt`): the prompt published
//! in the `terminals` list, the prompt last answered or typed at, the needs-you episode, and
//! the ids. Pure: the Terminal and the prompt worker drive it under `Terminal.prompt`.
//!
//! Rules:
//! - A prompt's `id` fences answers; ids strictly increase per Terminal and are never reused,
//!   across Relaunches (the replacement continues above [`PromptCell::high_water`]) and
//!   restarts (a floor persisted with the Terminal), whatever the clock does.
//! - A prompt answered or typed at is *spent*: normal extraction never publishes it again.
//!   Only a recheck may, once the input reached the PTY and a later screen still shows it
//!   (the TUI ignored the key): then it is published again under a new id.
//! - Input during a needs-you episode (an answer, or typing, also before any prompt was
//!   listed) closes it to buttons until the next needs-you. A prompt that left the screen
//!   without input does not: a prompt recognised after it is listed under a new id.
//! - A text-only prompt (`options: []`) appears at most once per needs-you episode, after the
//!   grace period, and never once a prompt of the episode was answered, typed at or went away.

use std::time::{Duration, Instant};
use xshell_core::prompt::Found;
use xshell_protocol::msg::{PermissionPrompt, PromptOption, PROMPT_ANSWERED, PROMPT_NO_OPTION};

/// How long after an answer the recheck keeps looking for a prompt the TUI ignored.
pub(crate) const RECHECK_FOR: Duration = Duration::from_secs(60);
/// How far ahead of a new id the persisted floor is reserved (ms of the id clock), so most
/// new prompts need no state-file write.
pub(crate) const RESERVE_AHEAD: u64 = 10 * 60 * 1000;

pub(crate) struct Current {
    pub prompt: PermissionPrompt,
    /// The bytes that answer each option.
    pub keys: Vec<Vec<u8>>,
    /// `None` for a text-only prompt.
    pub fingerprint: Option<u64>,
    /// A screen revision no longer showed it: it cannot be answered, and is not listed until
    /// the worker reads the screen again.
    pub gone: bool,
    /// Its id is below the floor persisted with the Terminal. Until then it is neither
    /// listed nor answerable.
    pub confirmed: bool,
}

/// The prompt last answered or typed at.
pub(crate) struct Spent {
    pub fingerprint: Option<u64>,
    /// The Terminal's input count once the answer was queued.
    pub seq: u64,
    /// The screen revision when it was spent.
    pub rev: u64,
    pub at: Instant,
}

/// A needs-you period of the Agent Status.
pub(crate) struct Episode {
    /// The `statusAtMs` of the needs-you that started it.
    pub stamp: u64,
    pub since: Instant,
    /// Someone answered or typed during the episode (also before any prompt was listed):
    /// they are answering at the Terminal, so the episode gets no buttons.
    pub typed: bool,
    /// A recognised prompt of the episode left the screen without input: a new one may
    /// follow (with a new id), but no text-only prompt.
    pub went_away: bool,
    /// The episode had its text-only prompt.
    pub text_only: bool,
}

#[derive(Default)]
pub(crate) struct PromptCell {
    current: Option<Current>,
    spent: Option<Spent>,
    episode: Option<Episode>,
    last_id: u64,
    /// Every id is above this: the replaced run's, or the persisted one.
    floor: u64,
    /// The floor known to be in the state file: ids up to it may be listed.
    reserved: u64,
    /// A floor to persist before the current prompt may be listed.
    pending: Option<u64>,
    /// Bumped by every change a worker's read may have missed.
    gen: u64,
}

/// What a worker's read found, and when.
pub(crate) struct Read<'a> {
    pub found: Option<&'a Found>,
    /// The text-only prompt's text, if one is made.
    pub tail: &'a dyn Fn() -> String,
    /// The screen revision read.
    pub rev: u64,
    /// The recheck after an answer (or typing), not the settle after output.
    pub recheck: bool,
    /// Inputs the Terminal's writer has written, and the screen revision at its last write,
    /// read together.
    pub written: (u64, u64),
    /// Inputs queued for the Terminal so far (read after `written`).
    pub queued: u64,
    pub now: Instant,
    /// The id clock (Unix ms).
    pub now_ms: u64,
    pub grace: Duration,
    pub recheck_delay: Duration,
}

/// What a commit changed and what it wants done next.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Outcome {
    /// The listed prompt changed: publish the list.
    pub changed: bool,
    /// A new prompt's id is above the persisted floor: persist this floor, then
    /// [`PromptCell::confirm`] it; until then the prompt is not listed.
    pub reserve: Option<u64>,
    /// Read again after the recheck delay.
    pub recheck_again: bool,
    /// Read again at this time (the grace period ends).
    pub grace_at: Option<Instant>,
}

impl PromptCell {
    /// A cell whose Terminal has `floor` persisted (0: none).
    pub fn new(floor: u64) -> Self {
        Self {
            floor,
            reserved: floor,
            ..Self::default()
        }
    }

    /// The prompt as listed: none while it is gone or its id is not persisted yet.
    pub fn listed(&self) -> Option<PermissionPrompt> {
        self.current
            .as_ref()
            .filter(|c| !c.gone && c.confirmed)
            .map(|c| c.prompt.clone())
    }

    /// Whether the screen must be checked on output: a recognised prompt is listed.
    pub fn watching(&self) -> Option<u64> {
        self.current
            .as_ref()
            .filter(|c| !c.gone)
            .and_then(|c| c.fingerprint)
    }

    /// Whether output should make the worker read the screen.
    pub fn interested(&self) -> bool {
        self.episode.is_some() || self.current.is_some()
    }

    pub fn gen(&self) -> u64 {
        self.gen
    }

    /// The highest id this Terminal has handed out or must stay above. (A reservation is
    /// not: ids below it are the point of making it.)
    pub fn high_water(&self) -> u64 {
        self.last_id.max(self.floor)
    }

    /// The floor to write to the state file: above every id handed out and every reservation.
    pub fn persist_floor(&self) -> Option<u64> {
        let f = self.high_water().max(self.reserved);
        Some(f.max(self.pending.unwrap_or(0))).filter(|f| *f > 0)
    }

    /// The persisted floor, `floor` (a restart).
    pub fn restored(&mut self, floor: u64) {
        self.floor = self.floor.max(floor);
        self.reserved = self.reserved.max(floor);
    }

    /// Continue the run this one replaces under the same Terminal: above its ids, and with
    /// its persisted floor (the state file entry is the same).
    pub fn inherit(&mut self, high_water: u64, reserved: u64) {
        self.floor = self.floor.max(high_water);
        self.reserved = self.reserved.max(reserved);
    }

    /// The floor of the last reservation, as persisted.
    pub fn reserved(&self) -> u64 {
        self.reserved
    }

    /// `floor` was written to the state file (`ok`), or could not be: a prompt waiting for it
    /// is listed, or dropped (fail closed). `true` when the listed prompt changed.
    pub fn confirm(&mut self, floor: u64, ok: bool) -> bool {
        let before = self.listed();
        if ok {
            self.reserved = self.reserved.max(floor);
            if self.pending.is_some_and(|p| p <= self.reserved) {
                self.pending = None;
            }
            if let Some(c) = &mut self.current {
                c.confirmed |= c.prompt.id <= self.reserved;
            }
        } else {
            self.pending = None;
            if self.current.as_ref().is_some_and(|c| !c.confirmed) {
                self.current = None;
            }
        }
        self.gen += 1;
        self.listed() != before
    }

    /// A new id: the clock, but strictly above every id before. Stays far below 2^53.
    fn next_id(&mut self, now_ms: u64) -> u64 {
        let id = now_ms.max(self.high_water() + 1);
        self.last_id = id;
        id
    }

    /// The status became needs-you (`stamp`: its `statusAtMs`): a new episode, in which a
    /// prompt that looks like the last spent one is a new prompt.
    pub fn needs_you(&mut self, stamp: u64, now: Instant) {
        if self.episode.as_ref().is_some_and(|e| e.stamp == stamp) {
            return;
        }
        self.episode = Some(Episode {
            stamp,
            since: now,
            typed: false,
            went_away: false,
            text_only: false,
        });
        self.spent = None;
        self.gen += 1;
    }

    /// The status left needs-you: nothing is listed. The spent prompt is kept for the
    /// recheck (a key Codex ignored left it on the screen, but its status says working).
    pub fn left_needs_you(&mut self) -> bool {
        let was = self.listed().is_some();
        self.episode = None;
        self.current = None;
        self.gen += 1;
        was
    }

    /// The run ended.
    pub fn end(&mut self) -> bool {
        let was = self.left_needs_you();
        self.spent = None;
        was
    }

    /// The screen no longer shows the listed prompt (`seen`: the fingerprint it shows).
    /// `true` when that made it unanswerable.
    pub fn observe(&mut self, seen: Option<u64>) -> bool {
        match &mut self.current {
            Some(c) if !c.gone && c.fingerprint.is_some() && c.fingerprint != seen => {
                c.gone = true;
                self.gen += 1;
                true
            }
            _ => false,
        }
    }

    /// Mark the listed prompt gone (an answer found the screen changed).
    pub fn set_gone(&mut self) {
        if let Some(c) = &mut self.current {
            c.gone = true;
            self.gen += 1;
        }
    }

    /// Check an answer to prompt `id` with `option`: its keys and the prompt's fingerprint.
    pub fn check_answer(&self, id: u64, option: u32) -> Result<(Vec<u8>, u64), &'static str> {
        let c = self
            .current
            .as_ref()
            .filter(|c| c.prompt.id == id && !c.gone && c.confirmed)
            .ok_or(PROMPT_ANSWERED)?;
        let fp = c.fingerprint.ok_or(PROMPT_NO_OPTION)?;
        let keys = c.keys.get(option as usize).ok_or(PROMPT_NO_OPTION)?;
        Ok((keys.clone(), fp))
    }

    /// The listed prompt was answered or typed at, with input number `seq` queued at screen
    /// revision `rev`. `true` when a prompt was listed.
    ///
    /// Typing with no prompt listed spends the episode too: whoever types is answering it.
    pub fn spend(&mut self, seq: u64, rev: u64, now: Instant) -> bool {
        if let Some(e) = &mut self.episode {
            e.typed = true;
        }
        self.gen += 1;
        let Some(c) = self.current.take() else {
            return false;
        };
        self.spent = Some(Spent {
            fingerprint: c.fingerprint,
            seq,
            rev,
            at: now,
        });
        !c.gone
    }

    /// Make `prompt` (with a new id) current: listed at once when its id is below the
    /// persisted floor, else once a new floor is (`out.reserve`).
    fn add(&mut self, mut c: Current, out: &mut Outcome) {
        c.confirmed = c.prompt.id <= self.reserved;
        if !c.confirmed {
            let floor = c.prompt.id + RESERVE_AHEAD;
            let floor = self.pending.map_or(floor, |p| p.max(floor));
            self.pending = Some(floor);
            out.reserve = Some(floor);
        }
        self.current = Some(c);
    }

    fn publish(&mut self, f: &Found, now_ms: u64, out: &mut Outcome) {
        let id = self.next_id(now_ms);
        let c = Current {
            prompt: PermissionPrompt {
                id,
                text: f.text.clone(),
                options: f
                    .options
                    .iter()
                    .map(|o| PromptOption {
                        label: o.label.clone(),
                    })
                    .collect(),
            },
            keys: f.options.iter().map(|o| o.keys.clone()).collect(),
            fingerprint: Some(f.fingerprint),
            gone: false,
            confirmed: false,
        };
        self.add(c, out);
    }

    /// Apply a worker's read of the screen. The caller made sure nothing changed the cell
    /// since the read ([`PromptCell::gen`]) and that `r.rev` is the screen's revision now.
    pub fn commit(&mut self, r: &Read) -> Outcome {
        let before = self.listed();
        let mut out = Outcome::default();
        let needs_you = self.episode.is_some();
        // Answered or typed at in this episode: only a new needs-you reopens it. (A prompt
        // that went away by itself does not close it: the next one gets buttons.)
        let typed = self.episode.as_ref().is_some_and(|e| e.typed);
        match r.found {
            Some(f) => {
                let same = |c: &Current| c.fingerprint == Some(f.fingerprint);
                if self.current.as_ref().is_some_and(|c| !c.gone && same(c)) {
                    return out;
                }
                let spent = self
                    .spent
                    .as_ref()
                    .filter(|s| s.fingerprint == Some(f.fingerprint));
                if let Some(s) = spent {
                    // The prompt answered or typed at is still on the screen: published again
                    // only by the recheck, once every input queued so far was written and the
                    // screen changed after the last write.
                    let (written, written_rev) = r.written;
                    let ignored = written >= r.queued.max(s.seq)
                        && r.rev > s.rev.max(written_rev)
                        && r.now >= s.at + r.recheck_delay;
                    if r.recheck && ignored {
                        self.spent = None;
                        self.publish(f, r.now_ms, &mut out);
                    } else {
                        self.current = None;
                        out.recheck_again = r.recheck && r.now < s.at + RECHECK_FOR;
                    }
                } else if needs_you && !typed {
                    self.publish(f, r.now_ms, &mut out);
                } else {
                    self.current = None;
                }
            }
            None => {
                let recognised = self
                    .current
                    .as_ref()
                    .is_some_and(|c| c.fingerprint.is_some());
                if recognised {
                    self.current = None;
                    if let Some(e) = &mut self.episode {
                        e.went_away = true;
                    }
                } else if self.current.is_none() {
                    if let Some(e) = &mut self.episode {
                        if !e.typed && !e.went_away && !e.text_only {
                            let at = e.since + r.grace;
                            if r.now >= at {
                                e.text_only = true;
                                let id = self.next_id(r.now_ms);
                                let c = Current {
                                    prompt: PermissionPrompt {
                                        id,
                                        text: (r.tail)(),
                                        options: vec![],
                                    },
                                    keys: vec![],
                                    fingerprint: None,
                                    gone: false,
                                    confirmed: false,
                                };
                                self.add(c, &mut out);
                            } else {
                                out.grace_at = Some(at);
                            }
                        }
                    }
                }
                if r.recheck {
                    out.recheck_again = self.spent.as_ref().is_some_and(|s| {
                        r.now < s.at + RECHECK_FOR && r.written.0 < r.queued.max(s.seq)
                    });
                }
            }
        }
        self.gen += 1;
        out.changed = self.listed() != before;
        out
    }
}

/// Whether a connection's `term.input` to one Terminal may answer a prompt (D5). Only writes
/// made entirely of complete, known sequences that answer nothing pass: focus reports
/// (`ESC [ I`, `ESC [ O`), cursor-position replies (`ESC [ row ; col R`) and cursor keys
/// (`ESC [ A`…`D`, `ESC O A`…`D`, `ESC [ 1 ; mod A`…`D`). Anything else may answer: text,
/// digits, Enter, a bare Esc, `^C`, a bracketed paste, an unknown sequence, a write mixing
/// them, and a sequence split across writes (the carry remembers a write that ended inside
/// one, so the next write counts as its rest).
#[derive(Debug, Default)]
pub(crate) struct InputCarry {
    split: bool,
}

impl InputCarry {
    pub fn may_answer(&mut self, data: &[u8]) -> bool {
        let after_split = std::mem::take(&mut self.split);
        let (benign, split) = classify(data);
        self.split = split;
        after_split || split || !benign
    }
}

/// `(every byte belongs to a complete non-answering sequence, the data ends inside a
/// sequence)`.
fn classify(data: &[u8]) -> (bool, bool) {
    let mut i = 0;
    while i < data.len() {
        if data[i] != 0x1b {
            return (false, false);
        }
        match benign_at(&data[i..]) {
            Seq::Benign(n) => i += n,
            Seq::Partial => return (false, true),
            Seq::Other => return (false, false),
        }
    }
    (true, false)
}

enum Seq {
    /// A non-answering sequence of this many bytes.
    Benign(usize),
    /// The data ends before the sequence does.
    Partial,
    Other,
}

/// The sequence at the start of `s` (which starts with ESC).
fn benign_at(s: &[u8]) -> Seq {
    let arrow = |b: u8| matches!(b, b'A'..=b'D');
    match s.get(1) {
        None => Seq::Partial,
        Some(b'O') => match s.get(2) {
            None => Seq::Partial,
            Some(&b) if arrow(b) => Seq::Benign(3),
            Some(_) => Seq::Other,
        },
        Some(b'[') => {
            // ESC [ params final: digits and `;` only.
            let mut j = 2;
            while j < s.len() && (s[j].is_ascii_digit() || s[j] == b';') {
                j += 1;
            }
            let Some(&fin) = s.get(j) else {
                return Seq::Partial;
            };
            let params = &s[2..j];
            let n = j + 1;
            let num = |p: &[u8]| !p.is_empty() && p.len() <= 5 && p.iter().all(u8::is_ascii_digit);
            let mut parts = params.split(|b| *b == b';');
            let (a, b, more) = (parts.next(), parts.next(), parts.next());
            let ok = match fin {
                b'I' | b'O' => params.is_empty(),
                f if arrow(f) => {
                    params.is_empty() || (more.is_none() && a == Some(b"1") && b.is_some_and(num))
                }
                b'R' => more.is_none() && a.is_some_and(num) && b.is_some_and(num),
                _ => false,
            };
            if ok {
                Seq::Benign(n)
            } else {
                Seq::Other
            }
        }
        Some(_) => Seq::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use xshell_core::prompt::FoundOption;

    fn found(fp: u64, n: usize) -> Found {
        Found {
            text: format!("Do you want to proceed? ({fp})"),
            options: (1..=n)
                .map(|i| FoundOption {
                    label: format!("option {i}"),
                    keys: vec![b'0' + i as u8],
                })
                .collect(),
            fingerprint: fp,
        }
    }

    /// Commit, and persist any reservation it asks for.
    fn apply(p: &mut PromptCell, r: &Read) -> Outcome {
        let mut o = p.commit(r);
        if let Some(f) = o.reserve {
            o.changed |= p.confirm(f, true);
        }
        o
    }

    struct Clock {
        t0: Instant,
        ms: u64,
    }

    fn read<'a>(c: &Clock, after: Duration, found: Option<&'a Found>, rev: u64) -> Read<'a> {
        Read {
            found,
            tail: &|| "the screen's tail".to_string(),
            rev,
            recheck: false,
            written: (u64::MAX, 0),
            queued: 0,
            now: c.t0 + after,
            now_ms: c.ms,
            grace: Duration::from_secs(1),
            recheck_delay: Duration::from_millis(1500),
        }
    }

    fn clock() -> Clock {
        Clock {
            t0: Instant::now(),
            ms: 1_000_000,
        }
    }

    /// A cell in a needs-you episode listing `f`.
    fn listing(c: &Clock, f: &Found) -> (PromptCell, u64) {
        let mut p = PromptCell::new(0);
        p.needs_you(1, c.t0);
        let o = apply(&mut p, &read(c, Duration::ZERO, Some(f), 1));
        assert!(o.changed && o.reserve.is_some());
        let id = p.listed().unwrap().id;
        (p, id)
    }

    #[test]
    fn answer_needs_current_id_and_option() {
        let c = clock();
        let f = found(7, 3);
        let (mut p, id) = listing(&c, &f);
        assert_eq!(p.check_answer(id, 1), Ok((b"2".to_vec(), 7)));
        assert_eq!(p.check_answer(id + 1, 1), Err(PROMPT_ANSWERED));
        assert_eq!(p.check_answer(id - 1, 1), Err(PROMPT_ANSWERED));
        assert_eq!(p.check_answer(id, 3), Err(PROMPT_NO_OPTION));
        assert_eq!(p.check_answer(id, u32::MAX), Err(PROMPT_NO_OPTION));
        // Gone from the screen: no longer answerable, not listed.
        assert!(p.observe(None));
        assert_eq!(p.check_answer(id, 1), Err(PROMPT_ANSWERED));
        assert_eq!(p.listed(), None);
        // A text-only prompt has no options.
        let mut t = PromptCell::new(0);
        t.needs_you(1, c.t0);
        let o = apply(&mut t, &read(&c, Duration::from_secs(2), None, 1));
        assert!(o.changed);
        let tid = t.listed().unwrap().id;
        assert_eq!(t.listed().unwrap().text, "the screen's tail");
        assert_eq!(t.check_answer(tid, 0), Err(PROMPT_NO_OPTION));
    }

    #[test]
    fn answer_spends_once() {
        let c = clock();
        let f = found(7, 3);
        let (mut p, id) = listing(&c, &f);
        assert!(p.check_answer(id, 0).is_ok());
        assert!(p.spend(1, 5, c.t0));
        assert_eq!(p.listed(), None);
        assert_eq!(p.check_answer(id, 0), Err(PROMPT_ANSWERED));
        assert!(!p.spend(2, 5, c.t0));
        // The screen still shows it: the settle never brings it back...
        for rev in [5, 6, 7] {
            let o = apply(&mut p, &read(&c, Duration::from_secs(5), Some(&f), rev));
            assert_eq!(o, Outcome::default());
            assert_eq!(p.listed(), None);
        }
        // ...and the recheck only once the key was written and the screen changed after it.
        let mut r = read(&c, Duration::from_secs(5), Some(&f), 6);
        r.recheck = true;
        r.written = (0, 0);
        assert!(apply(&mut p, &r).recheck_again, "not written yet");
        r.written = (1, 6);
        assert!(apply(&mut p, &r).recheck_again, "no screen after the write");
        r.now = c.t0 + Duration::from_millis(1000);
        r.rev = 7;
        assert!(apply(&mut p, &r).recheck_again, "before the recheck delay");
        assert_eq!(p.listed(), None);
        r.now = c.t0 + Duration::from_secs(2);
        let o = apply(&mut p, &r);
        assert!(o.changed, "published again, within the reservation");
        let again = p.listed().unwrap().id;
        assert!(again > id);
        assert_eq!(p.check_answer(id, 0), Err(PROMPT_ANSWERED));
        assert!(p.check_answer(again, 0).is_ok());
        // A recheck that finds the screen moved on stops.
        assert!(p.spend(2, 7, c.t0));
        let mut r = read(&c, Duration::from_secs(5), None, 8);
        r.recheck = true;
        r.written = (2, 7);
        assert_eq!(apply(&mut p, &r), Outcome::default());
    }

    #[test]
    fn ids_strictly_increase_across_republish_and_runs() {
        let mut c = clock();
        let (a, b) = (found(1, 2), found(2, 2));
        let (mut p, first) = listing(&c, &a);
        // The clock stands still, then goes back: ids still increase.
        let mut ids = vec![first];
        for (fp, ms) in [(&b, 1_000_000), (&a, 999_000), (&b, 10)] {
            c.ms = ms;
            apply(&mut p, &read(&c, Duration::ZERO, Some(fp), 2));
            ids.push(p.listed().unwrap().id);
        }
        assert!(ids.windows(2).all(|w| w[1] > w[0]), "{ids:?}");
        // An unchanged prompt keeps its id.
        apply(&mut p, &read(&c, Duration::ZERO, Some(&b), 3));
        assert_eq!(p.listed().unwrap().id, *ids.last().unwrap());
        // A replacement run, and a restart from a persisted floor with the clock set back.
        let hw = p.high_water();
        for mut next in [PromptCell::new(0), PromptCell::new(hw)] {
            next.inherit(hw, p.reserved());
            next.needs_you(9, c.t0);
            c.ms = 5;
            apply(&mut next, &read(&c, Duration::ZERO, Some(&a), 1));
            assert!(next.listed().unwrap().id > hw);
        }
        // Ids stay exact as JavaScript numbers.
        assert!(hw < 1 << 53);
    }

    #[test]
    fn text_only_once_per_episode_and_never_after_spent() {
        let c = clock();
        let mut p = PromptCell::new(0);
        p.needs_you(1, c.t0);
        // Before the grace: nothing, a read is asked for at its end.
        let o = apply(&mut p, &read(&c, Duration::from_millis(500), None, 1));
        assert_eq!(o.grace_at, Some(c.t0 + Duration::from_secs(1)));
        assert_eq!(p.listed(), None);
        let o = apply(&mut p, &read(&c, Duration::from_secs(1), None, 1));
        assert!(o.changed);
        assert_eq!(p.listed().unwrap().options, vec![]);
        // Typed at (the user answered in the Terminal View): gone, and never again in this
        // episode.
        assert!(p.spend(1, 2, c.t0));
        assert!(!apply(&mut p, &read(&c, Duration::from_secs(9), None, 3)).changed);
        assert_eq!(p.listed(), None);
        // A new episode may have one.
        p.left_needs_you();
        p.needs_you(2, c.t0);
        assert!(apply(&mut p, &read(&c, Duration::from_secs(2), None, 4)).changed);
        // A recognised prompt that went away spends the episode: no text-only after it.
        let f = found(3, 2);
        p.left_needs_you();
        p.needs_you(3, c.t0);
        apply(&mut p, &read(&c, Duration::ZERO, Some(&f), 5));
        assert!(p.listed().is_some());
        assert!(apply(&mut p, &read(&c, Duration::from_secs(5), None, 6)).changed);
        assert_eq!(p.listed(), None);
        assert!(!apply(&mut p, &read(&c, Duration::from_secs(9), None, 7)).changed);
        assert_eq!(p.listed(), None);
        // The same needs-you again is the same episode.
        p.needs_you(3, c.t0);
        assert!(!apply(&mut p, &read(&c, Duration::from_secs(9), None, 8)).changed);
    }

    #[test]
    fn spent_prompt_is_new_in_a_new_episode_and_gone_comes_back_renamed() {
        let c = clock();
        let f = found(4, 2);
        let (mut p, id) = listing(&c, &f);
        // Gone at a revision, then shown again: the same looking prompt, a new id.
        assert!(p.observe(Some(5)));
        assert!(!p.observe(None), "already gone");
        let o = apply(&mut p, &read(&c, Duration::ZERO, Some(&f), 3));
        assert!(o.changed);
        let id2 = p.listed().unwrap().id;
        assert!(id2 > id);
        // Answered; the next needs-you shows the same command again: a new prompt.
        p.spend(1, 3, c.t0);
        p.left_needs_you();
        assert!(!apply(&mut p, &read(&c, Duration::ZERO, Some(&f), 4)).changed);
        p.needs_you(2, c.t0);
        assert!(apply(&mut p, &read(&c, Duration::ZERO, Some(&f), 5)).changed);
        assert!(p.listed().unwrap().id > id2);
        // Leaving needs-you unlists it; the run's end forgets everything.
        assert!(p.left_needs_you());
        assert_eq!(p.listed(), None);
        assert!(!p.end());
    }

    #[test]
    fn may_answer_classification() {
        let one = |d: &[u8]| InputCarry::default().may_answer(d);
        for d in [
            &b"1"[..],
            b"\r",
            b"y",
            b"hello",
            b"\x1b",
            b"\x03",
            b"\x7f",
            b"\x1b[I\r",
            b"\x1b[A1",
            b"\x1b[200~123\x1b[201~",
            b"\x1b[200~ls\r\x1b[201~",
            b"\x1b[3~",
            b"\x1b[Z",
            b"\x1bb",
            b"\x1b[12;40",
            b"\x1b[2;3;4R",
            b"\x1b[;R",
            b"\x1b[5A",
        ] {
            assert!(one(d), "{:?} may answer", String::from_utf8_lossy(d));
        }
        for d in [
            &b"\x1b[I"[..],
            b"\x1b[O",
            b"\x1b[12;40R",
            b"\x1b[A",
            b"\x1bOB",
            b"\x1b[1;5C",
            b"\x1b[I\x1b[O\x1b[D",
            b"",
        ] {
            assert!(!one(d), "{:?} answers nothing", String::from_utf8_lossy(d));
        }
        // Split across writes: both halves count.
        let mut c = InputCarry::default();
        assert!(c.may_answer(b"\x1b["));
        assert!(c.may_answer(b"I"));
        assert!(!c.may_answer(b"\x1b[I"), "the carry is used once");
        assert!(c.may_answer(b"\x1b[I\x1b[12;4"));
        assert!(c.may_answer(b"\x1b[O"), "the rest of a split sequence");
        assert!(!c.may_answer(b"\x1b[O"));
    }

    #[test]
    fn typing_before_publication_spends_the_episode() {
        let c = clock();
        let f = found(5, 2);
        let mut p = PromptCell::new(0);
        p.needs_you(1, c.t0);
        // The Desktop types before the worker read the screen.
        assert!(!p.spend(1, 0, c.t0));
        for (after, found) in [(0, Some(&f)), (5, None), (5, Some(&f))] {
            let o = apply(&mut p, &read(&c, Duration::from_secs(after), found, 2));
            assert_eq!(o, Outcome::default());
            assert_eq!(p.listed(), None);
        }
        // Only a new needs-you reopens it.
        p.left_needs_you();
        p.needs_you(2, c.t0);
        assert!(apply(&mut p, &read(&c, Duration::ZERO, Some(&f), 3)).changed);
    }

    #[test]
    fn recheck_waits_for_every_queued_input() {
        let c = clock();
        let f = found(7, 2);
        let (mut p, id) = listing(&c, &f);
        assert!(p.spend(1, 5, c.t0));
        let mut r = read(&c, Duration::from_secs(5), Some(&f), 9);
        r.recheck = true;
        // More input was queued after the answer and is not written yet.
        r.queued = 3;
        r.written = (2, 6);
        assert!(apply(&mut p, &r).recheck_again);
        assert_eq!(p.listed(), None);
        // All of it written, but the screen is from before the last write.
        r.written = (3, 9);
        assert!(apply(&mut p, &r).recheck_again);
        assert_eq!(p.listed(), None);
        r.rev = 10;
        let o = apply(&mut p, &r);
        assert!(o.changed);
        assert!(p.listed().unwrap().id > id);
    }

    #[test]
    fn new_ids_are_listed_only_once_persisted() {
        let c = clock();
        let f = found(7, 2);
        let mut p = PromptCell::new(0);
        p.needs_you(1, c.t0);
        let o = p.commit(&read(&c, Duration::ZERO, Some(&f), 1));
        let floor = o.reserve.expect("a reservation");
        assert!(!o.changed);
        assert_eq!(p.listed(), None);
        let id = c.ms;
        assert!(floor > id);
        assert_eq!(p.check_answer(id, 0), Err(PROMPT_ANSWERED));
        assert_eq!(p.persist_floor(), Some(floor));
        // The state file could not be written: the prompt is dropped.
        assert!(!p.confirm(floor, false));
        assert_eq!(p.listed(), None);
        assert_eq!(p.check_answer(id, 0), Err(PROMPT_ANSWERED));
        // The next read tries again with a new id; persisted, it is listed.
        let o = p.commit(&read(&c, Duration::ZERO, Some(&f), 2));
        let floor = o.reserve.expect("a reservation");
        assert!(p.confirm(floor, true));
        let listed = p.listed().unwrap();
        assert!(listed.id > id);
        assert!(p.check_answer(listed.id, 0).is_ok());
        // Ids below the persisted floor need no new write.
        p.left_needs_you();
        p.needs_you(2, c.t0);
        let o = p.commit(&read(&c, Duration::ZERO, Some(&found(8, 2)), 3));
        assert_eq!(o.reserve, None);
        assert!(o.changed);
        // A restart from the floor stays above everything handed out.
        assert!(PromptCell::new(floor).high_water() >= p.listed().unwrap().id);
    }

    #[test]
    fn prompt_after_one_that_went_away_is_listed() {
        let c = clock();
        let (a, b) = (found(3, 2), found(4, 2));
        for next in [&a, &b] {
            let (mut p, id) = listing(&c, &a);
            // The worker read the blank screen between them: A is unlisted.
            assert!(apply(&mut p, &read(&c, Duration::from_secs(5), None, 2)).changed);
            assert_eq!(p.listed(), None);
            // B (or an identical-looking A) appears: listed with a new id.
            assert!(apply(&mut p, &read(&c, Duration::from_secs(5), Some(next), 3)).changed);
            assert!(p.listed().unwrap().id > id);
            // Still no text-only prompt once it goes too.
            apply(&mut p, &read(&c, Duration::from_secs(6), None, 4));
            assert!(!apply(&mut p, &read(&c, Duration::from_secs(9), None, 5)).changed);
            assert_eq!(p.listed(), None);
        }
    }
}
