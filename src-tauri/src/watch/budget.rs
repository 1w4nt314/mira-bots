//! Budget-regneren (trin 6d, plan6d punkt 5, A.3; research6d §3 og bilag A): en glidende
//! 60-minutters ring af starttider pr. projekt og én global, en lokal kalenderdag som
//! heltalsindeks og stille timer. Alt er rent: `now_ms` og `offset_secs` (lokal tid − UTC i
//! sekunder, fra [`crate::gh::local_offset_secs`] kaldt med `now`) injiceres.
//!
//! Tidszoner og sommertid: time-vinduet er UTC-ms og uberørt. Dagsindeks og stille timer
//! bruger offset ved `now`; på en overgangsdag (23/25 timer) nulstilles dagsloftet ved lokal
//! midnat, mens en "næste: HH:MM"-etiket for et tidspunkt efter overgangen kan være ±1 time
//! forkert (dokumenteret; genberegnes ved næste tick).

use serde::{Deserialize, Serialize};

use crate::config::{HOUR_MS, WATCH_RING_KEEP};
use crate::watch::config::in_quiet;

const DAY_SECS: i64 = 86_400;

fn local_secs(now_ms: u64, off: i64) -> i64 {
    (now_ms / 1000) as i64 + off
}

/// Den lokale kalenderdag som heltal (dage siden 1970-01-01 i lokal tid).
pub fn local_day(now_ms: u64, off: i64) -> i64 {
    local_secs(now_ms, off).div_euclid(DAY_SECS)
}

/// Minut i det lokale døgn (0–1439).
pub fn local_minute(now_ms: u64, off: i64) -> u32 {
    (local_secs(now_ms, off).rem_euclid(DAY_SECS) / 60) as u32
}

/// Ms til de stille timer `q` slutter; `None` når `now` ikke er i dem.
pub fn quiet_ends_in_ms(q: (u32, u32), now_ms: u64, off: i64) -> Option<u64> {
    let m = local_minute(now_ms, off);
    if !in_quiet(q, m) {
        return None;
    }
    // I de stille timer er slut ≠ m, så forskellen er 1–1439 minutter.
    let delta_min = (i64::from(q.1) - i64::from(m)).rem_euclid(1440) as u64;
    let secs_into_min = local_secs(now_ms, off).rem_euclid(60) as u64;
    Some(delta_min * 60_000 - secs_into_min * 1000)
}

/// UTC-ms for næste lokale midnat (med offset ved `now`).
pub fn next_local_midnight_ms(now_ms: u64, off: i64) -> u64 {
    let next = (local_day(now_ms, off) + 1) * DAY_SECS - off;
    next.max(0) as u64 * 1000
}

/// Hvorfor budgettet siger vent (rækkefølgen i [`check_project`]).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum WaitWhy {
    /// Stille timer.
    Quiet,
    /// Projektets dagsloft.
    Day,
    /// Projektets timeloft.
    Hour,
    /// Workspace-loftet for summen pr. dag.
    GlobalDay,
    /// Workspace-loftet for summen pr. time.
    GlobalHour,
    /// Et loft er 0: intet startes (ingen "næste").
    Off,
}

/// Budgettets svar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Go,
    /// `next_ms`: UTC-ms hvor den *første* årsag ophører (`None` ved [`WaitWhy::Off`]).
    Wait {
        why: WaitWhy,
        next_ms: Option<u64>,
    },
}

/// Lofterne for et start: projektets og workspace-filens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Caps {
    pub per_hour: u32,
    pub per_day: u32,
    pub ws_per_hour: u32,
    pub ws_per_day: u32,
}

impl Caps {
    /// Projektets effektive lofter `(time, dag)` = `min(projekt, workspace)` (handoff6d 2).
    pub fn effective(&self) -> (u32, u32) {
        (
            self.per_hour.min(self.ws_per_hour),
            self.per_day.min(self.ws_per_day),
        )
    }
}

/// En ring af starttider (stigende UTC-ms, kun den seneste time efter [`Ring::normalise`]) og
/// dagens tæller.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase", default)]
pub struct Ring {
    pub starts: Vec<u64>,
    /// [`local_day`] for `day_count`.
    pub day: i64,
    pub day_count: u32,
}

impl Ring {
    /// Smider poster ældre end en time, klemmer poster fra fremtiden til `now` (uret stillet
    /// tilbage må ikke blokere i timer) og ruller dagen: en ny (senere) dag nulstiller
    /// tælleren; går dagen *tilbage* (uret eller tidszonen), beholdes tælleren (konservativt).
    pub fn normalise(&mut self, now: u64, off: i64) {
        self.starts.retain(|t| t.saturating_add(HOUR_MS) > now);
        for t in &mut self.starts {
            if *t > now {
                *t = now;
            }
        }
        self.starts.sort_unstable();
        let d = local_day(now, off);
        if d > self.day {
            self.day_count = 0;
        }
        self.day = d;
    }

    /// Registrerer et start ved `now` (efter [`Ring::normalise`]).
    pub fn record(&mut self, now: u64) {
        self.starts.push(now);
        self.day_count = self.day_count.saturating_add(1);
    }

    /// Fjerner den seneste post `== ts` og tæller dagen ned; `false` når ingen fandtes.
    pub fn cancel(&mut self, ts: u64) -> bool {
        match self.starts.iter().rposition(|t| *t == ts) {
            Some(i) => {
                self.starts.remove(i);
                self.day_count = self.day_count.saturating_sub(1);
                true
            }
            None => false,
        }
    }

    /// Beholder højst `keep` poster (de nyeste).
    pub fn clamp_len(&mut self, keep: usize) {
        if self.starts.len() > keep {
            let drop = self.starts.len() - keep;
            self.starts.drain(..drop);
        }
    }

    /// Dagsloftet `cap` nået → næste lokale midnat.
    fn day_wait(&self, now: u64, off: i64, cap: u32) -> Option<u64> {
        (self.day_count >= cap).then(|| next_local_midnight_ms(now, off))
    }

    /// Timeloftet `cap` nået → når posten `starts[n - cap]` forlader vinduet (ikke blot den
    /// ældste: rigtigt også når loftet er sat ned under antallet i ringen).
    fn hour_wait(&self, cap: u32) -> Option<u64> {
        let n = self.starts.len();
        let cap = cap as usize;
        (n >= cap && cap > 0).then(|| self.starts[n - cap] + HOUR_MS)
    }
}

/// Om et start må ske nu for et projekt (A.3). Normaliserer begge ringe og svarer i
/// rækkefølgen stille → dag (projekt) → dag (global) → time (projekt) → time (global); den
/// første årsag giver "næste". Reserverer intet (se [`reserve`]).
pub fn check_project(
    project: &mut Ring,
    global: &mut Ring,
    now: u64,
    off: i64,
    caps: Caps,
    quiet: Option<(u32, u32)>,
) -> Verdict {
    let (per_hour, per_day) = caps.effective();
    project.normalise(now, off);
    global.normalise(now, off);
    project.clamp_len(WATCH_RING_KEEP.max(per_hour as usize));
    global.clamp_len(WATCH_RING_KEEP.max(caps.ws_per_hour as usize));
    let wait = |why, next_ms| Verdict::Wait { why, next_ms };
    if let Some(ms) = quiet.and_then(|q| quiet_ends_in_ms(q, now, off)) {
        return wait(WaitWhy::Quiet, Some(now + ms));
    }
    if per_hour == 0 || per_day == 0 || caps.ws_per_hour == 0 || caps.ws_per_day == 0 {
        return wait(WaitWhy::Off, None);
    }
    if let Some(t) = project.day_wait(now, off, per_day) {
        return wait(WaitWhy::Day, Some(t));
    }
    if let Some(t) = global.day_wait(now, off, caps.ws_per_day) {
        return wait(WaitWhy::GlobalDay, Some(t));
    }
    if let Some(t) = project.hour_wait(per_hour) {
        return wait(WaitWhy::Hour, Some(t));
    }
    if let Some(t) = global.hour_wait(caps.ws_per_hour) {
        return wait(WaitWhy::GlobalHour, Some(t));
    }
    Verdict::Go
}

/// [`check_project`] og, ved `Go`, en reservation ved `now` i begge ringe (før start; plan6d
/// C6d.3). Annulleres med [`cancel`] hvis startet ikke gav en ticket.
pub fn reserve(
    project: &mut Ring,
    global: &mut Ring,
    now: u64,
    off: i64,
    caps: Caps,
    quiet: Option<(u32, u32)>,
) -> Verdict {
    let v = check_project(project, global, now, off, caps, quiet);
    if v == Verdict::Go {
        project.record(now);
        global.record(now);
    }
    v
}

/// Annullerer en reservation fra [`reserve`] (`ts` = dens `now`) i begge ringe.
pub fn cancel(project: &mut Ring, global: &mut Ring, ts: u64) -> bool {
    let p = project.cancel(ts);
    let g = global.cancel(ts);
    p || g
}

/// Konservativ start (korrupt `watch-state.json`): ringen fyldes op til `cap` poster i vinduet
/// ved `now`, så timeloftet først er frit en time senere. Dagens tæller røres ikke.
pub fn conservative_fill(ring: &mut Ring, cap: u32, now: u64) {
    let present = ring
        .starts
        .iter()
        .filter(|t| t.saturating_add(HOUR_MS) > now)
        .count();
    let missing = (cap as usize).saturating_sub(present);
    ring.starts.extend(std::iter::repeat_n(now, missing));
    ring.starts.sort_unstable();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::watch::config::parse_quiet;

    const CET: i64 = 3600;
    const CEST: i64 = 7200;
    /// 2026-10-03 22:30:00 UTC (00:30 CEST den 4.).
    const T: u64 = 1_791_066_600_000;
    const MIN: u64 = 60_000;

    fn caps(h: u32, d: u32) -> Caps {
        Caps {
            per_hour: h,
            per_day: d,
            ws_per_hour: 60,
            ws_per_day: 500,
        }
    }

    fn wait(why: WaitWhy, next: u64) -> Verdict {
        Verdict::Wait {
            why,
            next_ms: Some(next),
        }
    }

    #[test]
    fn sliding_hour_window_and_next_free() {
        let (mut p, mut g) = (Ring::default(), Ring::default());
        for i in 0..3 {
            assert_eq!(
                reserve(&mut p, &mut g, T + i * MIN, CEST, caps(3, 10), None),
                Verdict::Go
            );
        }
        // Det 4. inden for timen venter til den ældste forlader vinduet.
        assert_eq!(
            check_project(&mut p, &mut g, T + 10 * MIN, CEST, caps(3, 10), None),
            wait(WaitWhy::Hour, T + HOUR_MS)
        );
        // Lige før: stadig vent; 61 minutter efter det første: én plads fri.
        assert_eq!(
            check_project(&mut p, &mut g, T + HOUR_MS - 1, CEST, caps(3, 10), None),
            wait(WaitWhy::Hour, T + HOUR_MS)
        );
        assert_eq!(
            check_project(&mut p, &mut g, T + 61 * MIN, CEST, caps(3, 10), None),
            Verdict::Go
        );
        // Posten T + 1 min forlod vinduet præcis ved T + 61 min (vinduet er halvåbent).
        assert_eq!(p.starts, [T + 2 * MIN]);
        assert_eq!(p.day_count, 3);
    }

    #[test]
    fn lowered_cap_waits_for_the_right_entry() {
        let (mut p, mut g) = (Ring::default(), Ring::default());
        for i in 0..4 {
            p.starts.push(T + i * 1000);
            p.day_count += 1;
        }
        p.day = local_day(T + 5000, CEST);
        // Loftet er nu 2 med 4 i ringen: 3 skal ud → starts[2].
        assert_eq!(
            check_project(&mut p, &mut g, T + 5000, CEST, caps(2, 99), None),
            wait(WaitWhy::Hour, T + 2000 + HOUR_MS)
        );
    }

    #[test]
    fn day_rolls_at_local_midnight_and_next_is_midnight() {
        let (mut p, mut g) = (Ring::default(), Ring::default());
        // 2026-10-03 21:00 UTC = 23:00 CEST; lokal midnat = 22:00 UTC.
        let t = 1_791_061_200_000u64;
        for i in 0..2 {
            assert_eq!(
                reserve(&mut p, &mut g, t + i, CEST, caps(5, 2), None),
                Verdict::Go
            );
        }
        let mid = t + HOUR_MS;
        assert_eq!(next_local_midnight_ms(t, CEST), mid);
        assert_eq!(
            check_project(&mut p, &mut g, t + 5 * MIN, CEST, caps(5, 2), None),
            wait(WaitWhy::Day, mid)
        );
        // Efter midnat nulstilles dagen; ringens poster tæller stadig for timeloftet, så
        // længe de er under en time gamle.
        assert_eq!(
            check_project(&mut p, &mut g, mid - MIN, CEST, caps(5, 2), None),
            wait(WaitWhy::Day, mid)
        );
        assert_eq!(
            check_project(&mut p, &mut g, mid - 1, CEST, caps(2, 9), None),
            wait(WaitWhy::Hour, t + HOUR_MS)
        );
        assert_eq!(
            check_project(&mut p, &mut g, mid + 1000, CEST, caps(5, 2), None),
            Verdict::Go
        );
        assert_eq!((p.day_count, p.starts.len()), (0, 0));
        // Samme UTC-tid med et andet offset er en anden lokal dag (kalenderdag via offset).
        assert_eq!(local_day(t, CEST), local_day(t, 0));
        assert_eq!(local_day(t + HOUR_MS, CEST), local_day(t, CEST) + 1);
        assert_eq!(local_day(t + HOUR_MS, 0), local_day(t, 0));
        // Negativt offset (vest for UTC): midnat lokalt = 05:00 UTC ved -5 t.
        let utc_midnight = 1_791_072_000_000u64; // 2026-10-04 00:00 UTC
        assert_eq!(
            next_local_midnight_ms(utc_midnight, -5 * 3600),
            utc_midnight + 5 * HOUR_MS
        );
    }

    #[test]
    fn dst_fall_back_day_has_25_hours_and_offset_change_is_handled_per_call() {
        // 2026-10-25 01:00 UTC: CEST (+2) → CET (+1). Lokal 02:30 sker to gange.
        let switch = 1_792_890_000_000u64;
        let before = switch - 30 * MIN; // 00:30 UTC = 02:30 CEST
        let after = switch + 30 * MIN; // 01:30 UTC = 02:30 CET
        assert_eq!(local_minute(before, CEST), 150);
        assert_eq!(local_minute(after, CET), 150);
        assert_eq!(local_day(before, CEST), local_day(after, CET));
        let q = parse_quiet("23-07").unwrap();
        assert!(in_quiet(q, local_minute(before, CEST)) && in_quiet(q, local_minute(after, CET)));
        // Dagen har 25 timer: lokal midnat før overgangen (22:00 UTC den 24.) og efter
        // (23:00 UTC den 25.) ligger 25 timer fra hinanden.
        let start_of_day = next_local_midnight_ms(before - 3 * HOUR_MS, CEST);
        let end_of_day = next_local_midnight_ms(after, CET);
        assert_eq!(end_of_day - start_of_day, 25 * HOUR_MS);
        // Dagstælleren ruller ikke ved overgangen (samme lokale dag) men ved midnat.
        let (mut p, mut g) = (Ring::default(), Ring::default());
        assert_eq!(
            reserve(&mut p, &mut g, before, CEST, caps(5, 1), None),
            Verdict::Go
        );
        assert_eq!(
            check_project(&mut p, &mut g, after, CET, caps(5, 1), None),
            wait(WaitWhy::Day, end_of_day)
        );
        assert_eq!(
            check_project(&mut p, &mut g, end_of_day, CET, caps(5, 1), None),
            Verdict::Go
        );
        // Etiketten for de stille timers slut bruger offset ved `now`: 02:30 CET → 07:00 CET.
        assert_eq!(
            quiet_ends_in_ms(q, after, CET),
            Some(4 * HOUR_MS + 30 * MIN)
        );
    }

    #[test]
    fn clock_going_backwards_does_not_block_for_hours() {
        let (mut p, mut g) = (Ring::default(), Ring::default());
        // "Fremtidige" poster (uret er stillet 5 timer tilbage).
        p.starts = vec![T + 5 * HOUR_MS, T + 5 * HOUR_MS];
        p.day = local_day(T, CEST);
        p.day_count = 2;
        assert_eq!(
            check_project(&mut p, &mut g, T, CEST, caps(2, 99), None),
            wait(WaitWhy::Hour, T + HOUR_MS)
        );
        assert_eq!(p.starts, [T, T]);
        // Går dagen tilbage, beholdes tælleren (konservativt) — og frigives ved næste dag.
        let mut r = Ring {
            starts: Vec::new(),
            day: local_day(T, CEST) + 1,
            day_count: 4,
        };
        r.normalise(T, CEST);
        assert_eq!((r.day, r.day_count), (local_day(T, CEST), 4));
        r.normalise(T + 24 * HOUR_MS, CEST);
        assert_eq!(r.day_count, 0);
    }

    #[test]
    fn quiet_parse_and_wrap() {
        assert_eq!(parse_quiet("23-07"), Some((1380, 420)));
        assert_eq!(parse_quiet("23-7"), Some((1380, 420)));
        assert_eq!(parse_quiet("07-07"), None);
        assert_eq!(parse_quiet("7-7"), None);
        assert_eq!(parse_quiet("0-24"), None);
        assert_eq!(parse_quiet("25-3"), None);
        assert_eq!(parse_quiet("3-25"), None);
        assert_eq!(parse_quiet("12-24"), Some((720, 0)));
        assert_eq!(parse_quiet("x"), None);
        assert_eq!(parse_quiet(""), None);
        assert_eq!(parse_quiet("-3"), None);
        let q = (1380, 420);
        assert!(in_quiet(q, 23 * 60) && in_quiet(q, 0) && in_quiet(q, 6 * 60 + 59));
        assert!(!in_quiet(q, 7 * 60) && !in_quiet(q, 22 * 60 + 59));
        let day = (7 * 60, 23 * 60);
        assert!(in_quiet(day, 8 * 60) && !in_quiet(day, 23 * 60) && !in_quiet(day, 6 * 60));
        // 12-24: fra middag til midnat.
        let pm = parse_quiet("12-24").unwrap();
        assert!(in_quiet(pm, 23 * 60 + 59) && !in_quiet(pm, 0) && !in_quiet(pm, 11 * 60));
    }

    #[test]
    fn quiet_end_label() {
        let q = (1380, 420);
        // 00:30 lokalt → slutter 07:00 = om 6,5 t.
        assert_eq!(
            quiet_ends_in_ms(q, T, CEST),
            Some(6 * HOUR_MS + HOUR_MS / 2)
        );
        // 21:30 lokalt: ikke stille.
        assert_eq!(quiet_ends_in_ms(q, T - 3 * HOUR_MS, CEST), None);
        // Sekunder inde i minuttet trækkes fra: 00:30:15 → 06:29:45 tilbage.
        assert_eq!(
            quiet_ends_in_ms(q, T + 15_000, CEST),
            Some(6 * HOUR_MS + HOUR_MS / 2 - 15_000)
        );
        // Verdict'et bærer sluttidspunktet.
        let (mut p, mut g) = (Ring::default(), Ring::default());
        assert_eq!(
            check_project(&mut p, &mut g, T, CEST, caps(3, 10), Some(q)),
            wait(WaitWhy::Quiet, T + 6 * HOUR_MS + HOUR_MS / 2)
        );
    }

    #[test]
    fn global_ring_blocks_second_project_and_caps_are_min() {
        let c = Caps {
            per_hour: 3,
            per_day: 10,
            ws_per_hour: 2,
            ws_per_day: 20,
        };
        assert_eq!(c.effective(), (2, 10));
        let (mut a, mut b, mut g) = (Ring::default(), Ring::default(), Ring::default());
        assert_eq!(reserve(&mut a, &mut g, T, CEST, c, None), Verdict::Go);
        assert_eq!(reserve(&mut a, &mut g, T + MIN, CEST, c, None), Verdict::Go);
        // Projekt a: loftet er min(3, 2) = 2.
        assert_eq!(
            check_project(&mut a, &mut g, T + 2 * MIN, CEST, c, None),
            wait(WaitWhy::Hour, T + HOUR_MS)
        );
        // Projekt b har ingen starter, men summen er nået.
        assert_eq!(
            check_project(&mut b, &mut g, T + 2 * MIN, CEST, c, None),
            wait(WaitWhy::GlobalHour, T + HOUR_MS)
        );
        // Dagens sum: workspace-dagsloft 2.
        let c = Caps {
            ws_per_hour: 60,
            ws_per_day: 2,
            ..c
        };
        assert_eq!(c.effective(), (3, 2));
        assert_eq!(
            check_project(&mut b, &mut g, T + 2 * MIN, CEST, c, None),
            wait(WaitWhy::GlobalDay, next_local_midnight_ms(T, CEST))
        );
    }

    #[test]
    fn reserve_then_cancel_restores_both_rings() {
        let (mut p, mut g) = (Ring::default(), Ring::default());
        assert_eq!(
            reserve(&mut p, &mut g, T, CEST, caps(3, 10), None),
            Verdict::Go
        );
        let (p0, g0) = (p.clone(), g.clone());
        assert_eq!(
            reserve(&mut p, &mut g, T + MIN, CEST, caps(3, 10), None),
            Verdict::Go
        );
        assert_eq!((p.day_count, g.day_count), (2, 2));
        assert!(cancel(&mut p, &mut g, T + MIN));
        assert_eq!((p, g), (p0.clone(), g0.clone()));
        // Ukendt tidsstempel: intet ændres.
        let (mut p, mut g) = (p0.clone(), g0.clone());
        assert!(!cancel(&mut p, &mut g, T + 7));
        assert_eq!((p, g), (p0, g0));
        // En afvist reservation registrerer intet.
        let (mut p, mut g) = (Ring::default(), Ring::default());
        assert_eq!(
            reserve(&mut p, &mut g, T, CEST, caps(3, 10), Some((1380, 420))),
            wait(WaitWhy::Quiet, T + 6 * HOUR_MS + HOUR_MS / 2)
        );
        assert!(p.starts.is_empty() && g.starts.is_empty() && p.day_count == 0);
    }

    #[test]
    fn conservative_fill_waits_an_hour() {
        let (mut p, mut g) = (Ring::default(), Ring::default());
        conservative_fill(&mut p, 3, T);
        assert_eq!(p.starts, [T, T, T]);
        assert_eq!(p.day_count, 0);
        assert_eq!(
            check_project(&mut p, &mut g, T + 30 * MIN, CEST, caps(3, 10), None),
            wait(WaitWhy::Hour, T + HOUR_MS)
        );
        assert_eq!(
            check_project(&mut p, &mut g, T + HOUR_MS, CEST, caps(3, 10), None),
            Verdict::Go
        );
        // Allerede fyldt: intet tilføjes.
        let mut r = Ring::default();
        conservative_fill(&mut r, 2, T);
        conservative_fill(&mut r, 2, T);
        assert_eq!(r.starts.len(), 2);
    }

    #[test]
    fn order_quiet_before_day_before_hour() {
        let (mut p, mut g) = (Ring::default(), Ring::default());
        for i in 0..3 {
            assert_eq!(
                reserve(&mut p, &mut g, T + i, CEST, caps(3, 3), None),
                Verdict::Go
            );
        }
        // Både dag og time er nået; stille timer vinder, så dag, så time.
        let q = Some((1380, 420));
        assert!(matches!(
            check_project(&mut p, &mut g, T + 10, CEST, caps(3, 3), q),
            Verdict::Wait {
                why: WaitWhy::Quiet,
                ..
            }
        ));
        assert_eq!(
            check_project(&mut p, &mut g, T + 10, CEST, caps(3, 3), None),
            wait(WaitWhy::Day, next_local_midnight_ms(T, CEST))
        );
        assert_eq!(
            check_project(&mut p, &mut g, T + 10, CEST, caps(3, 10), None),
            wait(WaitWhy::Hour, T + HOUR_MS)
        );
        // Projektets dag før den globale dag; projektets time før den globale time.
        let c = Caps {
            per_hour: 3,
            per_day: 3,
            ws_per_hour: 3,
            ws_per_day: 3,
        };
        assert!(matches!(
            check_project(&mut p, &mut g, T + 10, CEST, c, None),
            Verdict::Wait {
                why: WaitWhy::Day,
                ..
            }
        ));
        let c = Caps {
            per_hour: 3,
            per_day: 10,
            ws_per_hour: 3,
            ws_per_day: 10,
        };
        assert!(matches!(
            check_project(&mut p, &mut g, T + 10, CEST, c, None),
            Verdict::Wait {
                why: WaitWhy::Hour,
                ..
            }
        ));
    }

    #[test]
    fn zero_cap_is_off() {
        let (mut p, mut g) = (Ring::default(), Ring::default());
        for c in [caps(0, 10), caps(3, 0)] {
            assert_eq!(
                check_project(&mut p, &mut g, T, CEST, c, None),
                Verdict::Wait {
                    why: WaitWhy::Off,
                    next_ms: None
                }
            );
        }
        let c = Caps {
            ws_per_hour: 0,
            ..caps(3, 10)
        };
        assert_eq!(
            reserve(&mut p, &mut g, T, CEST, c, None),
            Verdict::Wait {
                why: WaitWhy::Off,
                next_ms: None
            }
        );
        assert!(p.starts.is_empty());
        // Stille timer går stadig forud for "off".
        assert!(matches!(
            check_project(&mut p, &mut g, T, CEST, caps(0, 0), Some((1380, 420))),
            Verdict::Wait {
                why: WaitWhy::Quiet,
                ..
            }
        ));
    }

    #[test]
    fn wait_why_is_camel_case() {
        let v: Vec<String> = [
            WaitWhy::Quiet,
            WaitWhy::Day,
            WaitWhy::Hour,
            WaitWhy::GlobalDay,
            WaitWhy::GlobalHour,
            WaitWhy::Off,
        ]
        .iter()
        .map(|w| serde_json::to_string(w).unwrap())
        .collect();
        assert_eq!(
            v,
            [
                "\"quiet\"",
                "\"day\"",
                "\"hour\"",
                "\"globalDay\"",
                "\"globalHour\"",
                "\"off\""
            ]
        );
        let r = Ring {
            starts: vec![1],
            day: 2,
            day_count: 3,
        };
        assert_eq!(
            serde_json::to_string(&r).unwrap(),
            r#"{"starts":[1],"day":2,"dayCount":3}"#
        );
    }
}
