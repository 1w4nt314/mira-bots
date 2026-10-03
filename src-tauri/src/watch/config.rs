//! `project.json` → `watch` og workspace-filens `watch` (trin 6d, plan6d punkt 2, A.4, C6d.1):
//! typerne, valideringen (noter, aldrig en afvist fil — som `github`) og valget af playbook.
//!
//! `serde_json` er bygget uden `preserve_order`, så et `Value::Object` har nøglerne
//! alfabetisk. `byLabel` skal matche i **filens** rækkefølge (handoff6d 4), så den læses i et
//! andet pas over teksten med [`Probe`], hvis [`Pairs`] bruger `MapAccess` direkte.

use std::fmt;

use serde::de::{MapAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::Value;

use crate::checks::GITHUB_LABEL_MAX_CHARS;
use crate::config::{
    WATCH_BY_LABEL_MAX, WATCH_MAX_AGENTS_DEFAULT, WATCH_MAX_PER_DAY_DEFAULT,
    WATCH_MAX_PER_HOUR_DEFAULT, WATCH_PER_DAY_MAX, WATCH_PER_HOUR_MAX, WATCH_WS_MAX_PER_DAY,
    WATCH_WS_MAX_PER_HOUR,
};
use crate::tickets::playbook::is_playbook_name;
use crate::tickets::prompt::one_line;
use crate::workspace::MAX_WORK_SEATS;

/// Navnet der betyder "ingen playbook" (en bar ticket uden agent; vagten parkerer emnet).
pub const NO_PLAYBOOK: &str = "task";

/// Et JSON-objekts poster i **filens** rækkefølge (research6d bilag A). Værdien er en `Value`,
/// så probe-passet aldrig fejler på typer; valideringen sker i [`parse_watch`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Pairs(pub Vec<(String, Value)>);

impl<'de> Deserialize<'de> for Pairs {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct PairsVisitor;
        impl<'de> Visitor<'de> for PairsVisitor {
            type Value = Pairs;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("et objekt")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut m: A) -> Result<Pairs, A::Error> {
                let mut v = Vec::new();
                while let Some((k, val)) = m.next_entry::<String, Value>()? {
                    v.push((k, val));
                }
                Ok(Pairs(v))
            }
        }
        d.deserialize_map(PairsVisitor)
    }
}

/// Andet pas over `project.json`: kun `watch.playbook`. Fejler aldrig (ulige former giver
/// `Default`/[`PlaybookProbe::Other`]); se [`probe_playbook`].
#[derive(Deserialize, Debug, Default)]
#[serde(default)]
pub struct Probe {
    pub watch: Option<WatchProbe>,
}

/// `watch` i probe-passet.
#[derive(Deserialize, Debug, Default)]
#[serde(default)]
pub struct WatchProbe {
    pub playbook: Option<PlaybookProbe>,
}

/// `watch.playbook` i probe-passet: et navn, et objekt (med `byLabel` i filens rækkefølge)
/// eller noget andet.
#[derive(Deserialize, Debug)]
#[serde(untagged)]
pub enum PlaybookProbe {
    Name(String),
    Rule {
        #[serde(default, rename = "byLabel")]
        by_label: Option<Pairs>,
        #[serde(default)]
        default: Option<Value>,
    },
    Other(Value),
}

/// `watch.playbook` fra filens tekst (andet pas); `None` når filen ikke har den, eller når
/// teksten ikke kan læses i dette pas (fx en dublet-nøgle) — så bruges `Value`-passets
/// rækkefølge.
pub fn probe_playbook(text: &str) -> Option<PlaybookProbe> {
    serde_json::from_str::<Probe>(text)
        .unwrap_or_default()
        .watch
        .and_then(|w| w.playbook)
}

/// Hvordan vagten vælger playbook for et emne (A.4).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum PlaybookRule {
    /// Ingen playbook valgt (udeladt, `"task"` eller ugyldig): vagten parkerer emnet.
    #[default]
    None,
    /// Altid denne playbook.
    Fixed(String),
    /// Første label (i filens rækkefølge, uden hensyn til store/små bogstaver) der matcher,
    /// ellers `default`. En værdi `"task"` betyder "ingen playbook for denne label". Et objekt
    /// er altid en regel, også uden gyldige poster (`{"default": "task"}`: kun emner med
    /// `kind:` startes; review6d W2).
    ByLabel {
        by_label: Vec<(String, String)>,
        default: Option<String>,
    },
}

/// `project.json` → `watch`, valideret (C6d.2). Vagten er FRA medmindre `enabled` er `true`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WatchConfig {
    pub enabled: bool,
    pub playbook: PlaybookRule,
    /// 1–[`WATCH_PER_HOUR_MAX`]; 0 = vagten starter intet (review6d N5).
    pub max_per_hour: u32,
    /// 1–[`WATCH_PER_DAY_MAX`]; 0 = vagten starter intet.
    pub max_per_day: u32,
    /// 1–[`MAX_WORK_SEATS`]: levende agenter vagten selv har startet i projektet.
    pub max_agents: usize,
    /// Stille timer som minutter i døgnet `(start, slut)`; se [`parse_quiet`].
    pub quiet: Option<(u32, u32)>,
    /// Stille timer som tekst `HH-HH` (Diagnostik/visning).
    pub quiet_text: Option<String>,
}

impl Default for WatchConfig {
    /// FRA, ingen playbook, standardlofterne, ingen stille timer.
    fn default() -> Self {
        WatchConfig {
            enabled: false,
            playbook: PlaybookRule::None,
            max_per_hour: WATCH_MAX_PER_HOUR_DEFAULT,
            max_per_day: WATCH_MAX_PER_DAY_DEFAULT,
            max_agents: WATCH_MAX_AGENTS_DEFAULT,
            quiet: None,
            quiet_text: None,
        }
    }
}

/// Workspace-filens `watch`: master-kontakt og øvre lofter (pr. projekt **og** som sum over
/// alle projekter; handoff6d 2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WorkspaceWatch {
    pub enabled: bool,
    pub max_per_hour: u32,
    pub max_per_day: u32,
}

impl Default for WorkspaceWatch {
    fn default() -> Self {
        WorkspaceWatch {
            enabled: true,
            max_per_hour: WATCH_WS_MAX_PER_HOUR,
            max_per_day: WATCH_WS_MAX_PER_DAY,
        }
    }
}

/// En værdi som kort tekst til en note (strenge uden anførselstegn).
fn shown(v: &Value) -> String {
    let s = match v {
        Value::String(s) => s.clone(),
        v => v.to_string(),
    };
    let s = one_line(&s);
    if s.chars().count() > 60 {
        format!("{}…", s.chars().take(60).collect::<String>())
    } else {
        s
    }
}

/// Et helt tal klemt til `lo..=hi` med noten "`{prefix}{key} v er sat ned/op til c ({why})`";
/// `None`/`null` giver `default`, en anden type noten "`… skal være et helt tal; d bruges`".
fn clamp_number(
    v: Option<&Value>,
    (prefix, key): (&str, &str),
    (lo, hi, default): (u64, u64, u64),
    why: &str,
    notes: &mut Vec<String>,
) -> u64 {
    match v {
        None | Some(Value::Null) => default,
        Some(n) => match n.as_u64() {
            Some(n) => {
                let c = n.clamp(lo, hi);
                if c != n {
                    let dir = if c < n { "ned" } else { "op" };
                    notes.push(format!("{prefix}{key} {n} er sat {dir} til {c} ({why})"));
                }
                c
            }
            None => {
                notes.push(format!(
                    "{prefix}{key} skal være et helt tal; {default} bruges"
                ));
                default
            }
        },
    }
}

/// Et loft (`maxPerHour`/`maxPerDay`): `0` betyder "vagten starter intet" (review6d N5,
/// fail-closed) med noten "`{prefix}{key} er 0: vagten starter intet`"; ellers
/// [`clamp_number`] med `lo` = 1.
fn cap_number(
    v: Option<&Value>,
    (prefix, key): (&str, &str),
    (hi, default): (u64, u64),
    why: &str,
    notes: &mut Vec<String>,
) -> u32 {
    if v.and_then(Value::as_u64) == Some(0) {
        notes.push(format!("{prefix}{key} er 0: vagten starter intet"));
        return 0;
    }
    clamp_number(v, (prefix, key), (1, hi, default), why, notes) as u32
}

const PJ: &str = "project.json: ";

/// Et playbook-navn fra filen: trimmet; `"task"` er lovligt men betyder ingen (`Ok(None)`);
/// et ugyldigt navn er `Err(tekst)`.
fn playbook_name(v: &Value) -> Result<Option<String>, String> {
    match v {
        Value::String(s) if s.trim() == NO_PLAYBOOK => Ok(None),
        Value::String(s) if is_playbook_name(s.trim()) => Ok(Some(s.trim().to_string())),
        v => Err(shown(v)),
    }
}

/// `watch.playbook` (A.4); noter for alt der ignoreres.
fn parse_playbook(
    v: Option<&Value>,
    probe: Option<&PlaybookProbe>,
    notes: &mut Vec<String>,
) -> PlaybookRule {
    let v = match v {
        None | Some(Value::Null) => return PlaybookRule::None,
        Some(v) => v,
    };
    match v {
        Value::String(_) => match playbook_name(v) {
            Ok(Some(name)) => PlaybookRule::Fixed(name),
            Ok(None) => PlaybookRule::None,
            Err(x) => {
                notes.push(format!("{PJ}watch.playbook «{x}» er ikke et gyldigt navn"));
                PlaybookRule::None
            }
        },
        Value::Object(obj) => {
            // Filens rækkefølge fra probe-passet; ellers (probe ulæselig) `Value`-passets.
            let entries: Option<Vec<(String, Value)>> = match (probe, obj.get("byLabel")) {
                (_, None | Some(Value::Null)) => Some(Vec::new()),
                (
                    Some(PlaybookProbe::Rule {
                        by_label: Some(p), ..
                    }),
                    Some(Value::Object(_)),
                ) => Some(p.0.clone()),
                (_, Some(Value::Object(m))) => {
                    Some(m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
                }
                (_, Some(_)) => None,
            };
            let mut by_label = Vec::new();
            match entries {
                None => notes.push(format!(
                    "{PJ}watch.playbook.byLabel skal være et objekt; den ignoreres"
                )),
                Some(entries) => {
                    if entries.len() > WATCH_BY_LABEL_MAX {
                        notes.push(format!(
                            "{PJ}watch.playbook.byLabel har højst {WATCH_BY_LABEL_MAX} poster; resten ignoreres"
                        ));
                    }
                    for (k, val) in entries.into_iter().take(WATCH_BY_LABEL_MAX) {
                        let key = k.trim();
                        let why = if key.is_empty() {
                            Some("tom nøgle")
                        } else if key.chars().count() > GITHUB_LABEL_MAX_CHARS {
                            Some("for lang nøgle")
                        } else if !matches!(&val, Value::String(s) if s.trim() == NO_PLAYBOOK || is_playbook_name(s.trim()))
                        {
                            Some("ugyldigt playbook-navn")
                        } else {
                            None
                        };
                        match why {
                            Some(why) => notes.push(format!(
                                "{PJ}watch.playbook.byLabel.«{}» ignoreres: {why}",
                                shown(&Value::String(k.clone()))
                            )),
                            None => by_label.push((
                                key.to_string(),
                                val.as_str().unwrap_or_default().trim().to_string(),
                            )),
                        }
                    }
                }
            }
            let default = match obj.get("default") {
                None | Some(Value::Null) => None,
                Some(d) => match playbook_name(d) {
                    Ok(d) => d,
                    Err(x) => {
                        notes.push(format!(
                            "{PJ}watch.playbook.default «{x}» er ikke et gyldigt navn"
                        ));
                        None
                    }
                },
            };
            // Review6d W2: et objekt er en regel, også tomt — så kan "kun emnets `kind:`"
            // udtrykkes (`{"default": "task"}`), mens udeladt/`"task"` betyder ingen regel.
            PlaybookRule::ByLabel { by_label, default }
        }
        _ => {
            notes.push(format!(
                "{PJ}watch.playbook skal være et navn eller et objekt med byLabel/default; ingen playbook valgt"
            ));
            PlaybookRule::None
        }
    }
}

/// `project.json` → `watch` (plan6d punkt 2): `v` er `Value`-passets `watch`, `probe` andet pas'
/// `watch.playbook` ([`probe_playbook`]). Et ikke-objekt giver en note og `None`; alt andet
/// ugyldigt giver en note og en sikker værdi (`enabled` → FRA, tal klemmes, stille timer og
/// playbook ignoreres). Filen afvises aldrig.
pub fn parse_watch(
    v: &Value,
    probe: Option<&PlaybookProbe>,
    notes: &mut Vec<String>,
) -> Option<WatchConfig> {
    let Value::Object(w) = v else {
        notes.push(format!("{PJ}watch ignoreres: skal være et objekt"));
        return None;
    };
    let mut cfg = WatchConfig::default();
    match w.get("enabled") {
        None | Some(Value::Null) => {}
        Some(Value::Bool(b)) => cfg.enabled = *b,
        Some(_) => notes.push(format!(
            "{PJ}watch.enabled skal være true/false; vagten er fra"
        )),
    }
    cfg.playbook = parse_playbook(w.get("playbook"), probe, notes);
    cfg.max_per_hour = cap_number(
        w.get("maxPerHour"),
        (PJ, "watch.maxPerHour"),
        (
            u64::from(WATCH_PER_HOUR_MAX),
            u64::from(WATCH_MAX_PER_HOUR_DEFAULT),
        ),
        &format!("1–{WATCH_PER_HOUR_MAX}"),
        notes,
    );
    cfg.max_per_day = cap_number(
        w.get("maxPerDay"),
        (PJ, "watch.maxPerDay"),
        (
            u64::from(WATCH_PER_DAY_MAX),
            u64::from(WATCH_MAX_PER_DAY_DEFAULT),
        ),
        &format!("1–{WATCH_PER_DAY_MAX}"),
        notes,
    );
    cfg.max_agents = clamp_number(
        w.get("maxAgents"),
        (PJ, "watch.maxAgents"),
        (1, MAX_WORK_SEATS as u64, WATCH_MAX_AGENTS_DEFAULT as u64),
        &format!("1–{MAX_WORK_SEATS}"),
        notes,
    ) as usize;
    match w.get("quietHours") {
        None | Some(Value::Null) => {}
        Some(q) => match q.as_str().and_then(parse_quiet) {
            Some(p) => {
                cfg.quiet = Some(p);
                cfg.quiet_text = Some(quiet_text(p));
            }
            None => notes.push(format!(
                "{PJ}watch.quietHours «{}» ignoreres (formen HH-HH, fx 23-07)",
                shown(q)
            )),
        },
    }
    Some(cfg)
}

/// Workspace-filens `watch` (plan6d punkt 4): noter uden `project.json`-præfiks som de andre
/// workspace-noter; forkert form er en note, aldrig en afvist fil.
pub fn parse_workspace_watch(v: &Value, notes: &mut Vec<String>) -> WorkspaceWatch {
    let mut ws = WorkspaceWatch::default();
    let Value::Object(w) = v else {
        // Fejler lukket: `"watch": false` (eller andet der ikke er et objekt) slukker vagten.
        ws.enabled = false;
        notes.push("watch skal være et objekt; vagten er fra".to_string());
        return ws;
    };
    match w.get("enabled") {
        None | Some(Value::Null) => {}
        Some(Value::Bool(b)) => ws.enabled = *b,
        // Review6d N5: master-kontakten fejler lukket (`"enabled": "false"` tænder den ikke).
        Some(_) => {
            ws.enabled = false;
            notes.push("watch.enabled skal være true/false; vagten er fra".to_string());
        }
    }
    ws.max_per_hour = cap_number(
        w.get("maxPerHour"),
        ("", "watch.maxPerHour"),
        (
            u64::from(WATCH_PER_HOUR_MAX),
            u64::from(WATCH_WS_MAX_PER_HOUR),
        ),
        "vagtens loft",
        notes,
    );
    ws.max_per_day = cap_number(
        w.get("maxPerDay"),
        ("", "watch.maxPerDay"),
        (
            u64::from(WATCH_PER_DAY_MAX),
            u64::from(WATCH_WS_MAX_PER_DAY),
        ),
        "vagtens loft",
        notes,
    );
    ws
}

/// Playbooken for et emne (A.4): uden regel (`watch.playbook` udeladt, `"task"` eller
/// ugyldig) ingen playbook — heller ikke for et emne med `kind:` (handoff6d 5, review6d W2).
/// Med en regel vinder mappe-emnets `ticket_kind` (`"task"` tæller som ingen); ellers
/// `byLabel` i filens rækkefølge, uden hensyn til store/små bogstaver og omgivende mellemrum,
/// første match (en værdi `"task"` giver `None`), ellers `default`. `None`: ingen playbook
/// (vagten parkerer emnet). Om navnet findes i workspace tjekker kalderen.
pub fn pick_playbook<'a>(
    rule: &'a PlaybookRule,
    ticket_kind: Option<&'a str>,
    labels: &[String],
) -> Option<&'a str> {
    if *rule == PlaybookRule::None {
        return None;
    }
    if let Some(k) = ticket_kind.map(str::trim) {
        if !k.is_empty() && k != NO_PLAYBOOK {
            return Some(k);
        }
    }
    match rule {
        PlaybookRule::None => None,
        PlaybookRule::Fixed(name) => Some(name.as_str()),
        PlaybookRule::ByLabel { by_label, default } => {
            let labels: Vec<String> = labels.iter().map(|l| l.trim().to_lowercase()).collect();
            for (key, playbook) in by_label {
                let key = key.trim().to_lowercase();
                if labels.contains(&key) {
                    return (playbook != NO_PLAYBOOK).then_some(playbook.as_str());
                }
            }
            default.as_deref()
        }
    }
}

/// `"HH-HH"` → minutter i døgnet `(start, slut)` (`"23-07"` → `(1380, 420)`). Start 0–23, slut
/// 0–24 (24 = midnat); `a == b` og `0-24` er ugyldige (hele døgnet stille er ikke en
/// stille-time-regel). Wrap over midnat tilladt.
pub fn parse_quiet(s: &str) -> Option<(u32, u32)> {
    let (a, b) = s.trim().split_once('-')?;
    let (a, b) = (a.trim(), b.trim());
    let digits = |x: &str| !x.is_empty() && x.len() <= 2 && x.bytes().all(|c| c.is_ascii_digit());
    if !digits(a) || !digits(b) {
        return None;
    }
    let (a, b): (u32, u32) = (a.parse().ok()?, b.parse().ok()?);
    if a > 23 || b > 24 || a == b || (a == 0 && b == 24) {
        return None;
    }
    Some((a * 60, (b % 24) * 60))
}

/// Er minut-i-døgnet `minute` inden for de stille timer `q` (start medregnet, slut ikke)?
pub fn in_quiet(q: (u32, u32), minute: u32) -> bool {
    let (s, e) = q;
    if s < e {
        minute >= s && minute < e
    } else {
        minute >= s || minute < e
    }
}

/// `(1380, 420)` → `"23-07"`.
pub fn quiet_text(q: (u32, u32)) -> String {
    format!("{:02}-{:02}", q.0 / 60, q.1 / 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    fn parse_text(text: &str) -> (Option<WatchConfig>, Vec<String>) {
        let v: Value = serde_json::from_str(text).unwrap();
        let mut notes = Vec::new();
        let probe = probe_playbook(text);
        let cfg = match v.get("watch") {
            None | Some(Value::Null) => None,
            Some(w) => parse_watch(w, probe.as_ref(), &mut notes),
        };
        (cfg, notes)
    }

    fn rule(pairs: &[(&str, &str)], default: Option<&str>) -> PlaybookRule {
        PlaybookRule::ByLabel {
            by_label: pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            default: default.map(str::to_string),
        }
    }

    #[test]
    fn by_label_keeps_file_order_not_alphabetical() {
        let text =
            r#"{"watch":{"playbook":{"byLabel":{"zeta":"bug","Bug":"feature","alpha":"docs"}}}}"#;
        let (cfg, notes) = parse_text(text);
        assert!(notes.is_empty(), "{notes:?}");
        assert_eq!(
            cfg.unwrap().playbook,
            rule(
                &[("zeta", "bug"), ("Bug", "feature"), ("alpha", "docs")],
                None
            )
        );
        // `Value` sorterer nøglerne (serde_json uden preserve_order) — derfor probe-passet.
        let v: Value = serde_json::from_str(r#"{"zeta":1,"Bug":2,"alpha":3}"#).unwrap();
        let keys: Vec<_> = v.as_object().unwrap().keys().cloned().collect();
        assert_eq!(keys, ["Bug", "alpha", "zeta"]);
        // Refuter-casen: label `zeta` og `alpha` på samme emne → første i filen (bug).
        let (cfg, _) =
            parse_text(r#"{"watch":{"playbook":{"byLabel":{"zeta":"bug","alpha":"feature"}}}}"#);
        let cfg = cfg.unwrap();
        assert_eq!(
            pick_playbook(&cfg.playbook, None, &labels(&["alpha", "zeta"])),
            Some("bug")
        );
    }

    #[test]
    fn pick_is_case_insensitive_first_match_then_default() {
        let r = rule(&[("bug", "bug"), ("Enhancement", "feature")], Some("docs"));
        assert_eq!(
            pick_playbook(&r, None, &labels(&["ENHANCEMENT", "bug"])),
            Some("bug")
        );
        assert_eq!(
            pick_playbook(&r, None, &labels(&[" enhancement "])),
            Some("feature")
        );
        assert_eq!(pick_playbook(&r, None, &labels(&["docs"])), Some("docs"));
        assert_eq!(pick_playbook(&r, None, &[]), Some("docs"));
        let r = rule(&[("bug", "bug")], None);
        assert_eq!(pick_playbook(&r, None, &labels(&["docs"])), None);
        // En label der peger på "task" betyder: ingen playbook (default bruges ikke).
        let r = rule(&[("question", "task"), ("bug", "bug")], Some("bug"));
        assert_eq!(pick_playbook(&r, None, &labels(&["Question", "bug"])), None);
        assert_eq!(
            pick_playbook(&PlaybookRule::None, None, &labels(&["bug"])),
            None
        );
        assert_eq!(
            pick_playbook(&PlaybookRule::Fixed("bug".into()), None, &[]),
            Some("bug")
        );
    }

    #[test]
    fn ticket_kind_wins_over_labels_and_task_means_none() {
        let r = rule(&[("bug", "bug")], Some("feature"));
        assert_eq!(
            pick_playbook(&r, Some("docs"), &labels(&["bug"])),
            Some("docs")
        );
        // Review6d W2: uden regel starter heller ikke et emne med `kind:`.
        assert_eq!(pick_playbook(&PlaybookRule::None, Some("bug"), &[]), None);
        // En tom regel (`{"default": "task"}`) lader `kind:` vinde, andre emner parkeres.
        let only_kind = rule(&[], None);
        assert_eq!(pick_playbook(&only_kind, Some("bug"), &[]), Some("bug"));
        assert_eq!(pick_playbook(&only_kind, None, &labels(&["bug"])), None);
        // ticket_kind "task" tæller som ingen: reglen afgør.
        assert_eq!(
            pick_playbook(&r, Some("task"), &labels(&["bug"])),
            Some("bug")
        );
        assert_eq!(pick_playbook(&r, Some(" "), &[]), Some("feature"));
        assert_eq!(pick_playbook(&PlaybookRule::None, Some("task"), &[]), None);
        // "task" i filen: lovligt, men ingen playbook og ingen note.
        let (cfg, notes) = parse_text(r#"{"watch":{"enabled":true,"playbook":"task"}}"#);
        assert_eq!(
            (cfg.unwrap().playbook, notes.len()),
            (PlaybookRule::None, 0)
        );
        // Et objekt er en regel, også tomt (kollapses ikke til `None`).
        for text in [
            r#"{"watch":{"playbook":{"byLabel":{},"default":"task"}}}"#,
            r#"{"watch":{"playbook":{"default":"task"}}}"#,
            r#"{"watch":{"playbook":{}}}"#,
        ] {
            let (cfg, notes) = parse_text(text);
            assert_eq!((cfg.unwrap().playbook, notes.len()), (rule(&[], None), 0));
        }
        let (cfg, _) = parse_text(r#"{"watch":{"enabled":true}}"#);
        assert_eq!(cfg.unwrap().playbook, PlaybookRule::None);
        let (cfg, _) = parse_text(r#"{"watch":{"playbook":" bug "}}"#);
        assert_eq!(cfg.unwrap().playbook, PlaybookRule::Fixed("bug".into()));
    }

    #[test]
    fn watch_absent_gives_none_without_notes() {
        for text in [r#"{}"#, r#"{"watch": null}"#, r#"{"checks": []}"#] {
            let (cfg, notes) = parse_text(text);
            assert_eq!(cfg, None, "{text}");
            assert!(notes.is_empty(), "{text}: {notes:?}");
        }
        // Et tomt objekt: vagten er FRA, standardværdier, ingen noter.
        let (cfg, notes) = parse_text(r#"{"watch": {}}"#);
        assert_eq!(cfg, Some(WatchConfig::default()));
        assert!(notes.is_empty());
        let full = r#"{"watch":{"enabled":true,"playbook":{"byLabel":{"bug":"bug","enhancement":"feature"},"default":"task"},"maxPerHour":3,"maxPerDay":10,"maxAgents":2,"quietHours":"23-07"}}"#;
        let (cfg, notes) = parse_text(full);
        assert!(notes.is_empty(), "{notes:?}");
        assert_eq!(
            cfg.unwrap(),
            WatchConfig {
                enabled: true,
                playbook: rule(&[("bug", "bug"), ("enhancement", "feature")], None),
                max_per_hour: 3,
                max_per_day: 10,
                max_agents: 2,
                quiet: Some((1380, 420)),
                quiet_text: Some("23-07".into()),
            }
        );
    }

    #[test]
    fn watch_invalid_fields_are_notes_not_errors() {
        let cases: &[(&str, &[&str])] = &[
            (r#"5"#, &["project.json: watch ignoreres: skal være et objekt"]),
            (
                r#"{"enabled": "ja"}"#,
                &["project.json: watch.enabled skal være true/false; vagten er fra"],
            ),
            (
                r#"{"maxPerHour": 0}"#,
                &["project.json: watch.maxPerHour er 0: vagten starter intet"],
            ),
            (
                r#"{"maxPerDay": 0}"#,
                &["project.json: watch.maxPerDay er 0: vagten starter intet"],
            ),
            (
                r#"{"maxPerHour": 999}"#,
                &["project.json: watch.maxPerHour 999 er sat ned til 60 (1–60)"],
            ),
            (
                r#"{"maxPerDay": 501}"#,
                &["project.json: watch.maxPerDay 501 er sat ned til 500 (1–500)"],
            ),
            (
                r#"{"maxPerHour": "3"}"#,
                &["project.json: watch.maxPerHour skal være et helt tal; 3 bruges"],
            ),
            (
                r#"{"maxAgents": 9}"#,
                &["project.json: watch.maxAgents 9 er sat ned til 5 (1–5)"],
            ),
            (
                r#"{"quietHours": "7-7"}"#,
                &["project.json: watch.quietHours «7-7» ignoreres (formen HH-HH, fx 23-07)"],
            ),
            (
                r#"{"quietHours": "0-24"}"#,
                &["project.json: watch.quietHours «0-24» ignoreres (formen HH-HH, fx 23-07)"],
            ),
            (
                r#"{"quietHours": "25-3"}"#,
                &["project.json: watch.quietHours «25-3» ignoreres (formen HH-HH, fx 23-07)"],
            ),
            (
                r#"{"quietHours": 23}"#,
                &["project.json: watch.quietHours «23» ignoreres (formen HH-HH, fx 23-07)"],
            ),
            (r#"{"quietHours": "23-07"}"#, &[]),
            (
                r#"{"playbook": 5}"#,
                &["project.json: watch.playbook skal være et navn eller et objekt med byLabel/default; ingen playbook valgt"],
            ),
            (
                r#"{"playbook": "Bug!"}"#,
                &["project.json: watch.playbook «Bug!» er ikke et gyldigt navn"],
            ),
            (
                r#"{"playbook": {"byLabel": {"Bug": "bug", "x": "nope!", "": "bug"}, "default": "task"}}"#,
                &[
                    "project.json: watch.playbook.byLabel.«x» ignoreres: ugyldigt playbook-navn",
                    "project.json: watch.playbook.byLabel.«» ignoreres: tom nøgle",
                ],
            ),
            (
                r#"{"playbook": {"byLabel": ["bug"], "default": "bug"}}"#,
                &["project.json: watch.playbook.byLabel skal være et objekt; den ignoreres"],
            ),
            (
                r#"{"playbook": {"default": 7}}"#,
                &["project.json: watch.playbook.default «7» er ikke et gyldigt navn"],
            ),
        ];
        for (watch, want) in cases {
            let text = format!(r#"{{"watch": {watch}}}"#);
            let (_, notes) = parse_text(&text);
            assert_eq!(notes, *want, "{watch}");
        }
        // Værdierne efter noterne.
        let (cfg, _) = parse_text(r#"{"watch": 5}"#);
        assert_eq!(cfg, None);
        let (cfg, _) = parse_text(
            r#"{"watch": {"enabled": "ja", "maxPerHour": 999, "maxAgents": 9, "quietHours": "7-7"}}"#,
        );
        let cfg = cfg.unwrap();
        assert!(!cfg.enabled);
        assert_eq!((cfg.max_per_hour, cfg.max_agents, cfg.quiet), (60, 5, None));
        // Review6d N5: et loft på 0 klemmes ikke op; det betyder "starter intet".
        let (cfg, _) = parse_text(r#"{"watch": {"maxPerHour": 0, "maxPerDay": 0}}"#);
        let cfg = cfg.unwrap();
        assert_eq!((cfg.max_per_hour, cfg.max_per_day), (0, 0));
        let (cfg, _) = parse_text(
            r#"{"watch": {"enabled": true, "playbook": {"byLabel": {"Bug": "bug", "x": "nope!", "": "bug"}, "default": "task"}}}"#,
        );
        let cfg = cfg.unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.playbook, rule(&[("Bug", "bug")], None));
        // For lang nøgle og højst 20 poster.
        let long = "l".repeat(51);
        let (_, notes) = parse_text(&format!(
            r#"{{"watch": {{"playbook": {{"byLabel": {{"{long}": "bug"}}}}}}}}"#
        ));
        assert_eq!(
            notes,
            [format!(
                "project.json: watch.playbook.byLabel.«{long}» ignoreres: for lang nøgle"
            )]
        );
        let many: Vec<String> = (0..25).map(|i| format!(r#""l{i:02}": "bug""#)).collect();
        let (cfg, notes) = parse_text(&format!(
            r#"{{"watch": {{"playbook": {{"byLabel": {{{}}}}}}}}}"#,
            many.join(",")
        ));
        assert_eq!(
            notes,
            ["project.json: watch.playbook.byLabel har højst 20 poster; resten ignoreres"]
        );
        match cfg.unwrap().playbook {
            PlaybookRule::ByLabel { by_label, .. } => {
                assert_eq!(by_label.len(), 20);
                assert_eq!(by_label[0].0, "l00");
                assert_eq!(by_label[19].0, "l19");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn probe_never_fails_on_odd_shapes() {
        for text in [
            r#"{"watch": 5}"#,
            r#"{"watch": []}"#,
            r#"{"watch": {"playbook": 5}}"#,
            r#"{"watch": {"playbook": null}}"#,
            r#"{"watch": {"playbook": [1, 2]}}"#,
            r#"{"watch": {"playbook": {"byLabel": 5}}}"#,
            r#"{"watch": {"playbook": {"byLabel": {"a": 5, "b": {"x": 1}}}}}"#,
            r#"{"watch": {"playbook": {"byLabel": {"a": "bug"}, "default": [1]}}}"#,
            r#"{"watch": {"playbook": {"byLabel": null, "default": null}}}"#,
            r#"{"watch": {"enabled": "ja", "playbook": "bug", "extra": {"deep": [1]}}}"#,
            r#"{"watch": {"playbook": "x"}, "watch2": 1}"#,
            r#"[1, 2]"#,
            r#"{"#,
        ] {
            // Fejler aldrig: probe-passet giver en værdi eller `None`.
            let _ = probe_playbook(text);
            if let Ok(v) = serde_json::from_str::<Value>(text) {
                if let Some(w) = v.get("watch") {
                    let mut notes = Vec::new();
                    let _ = parse_watch(w, probe_playbook(text).as_ref(), &mut notes);
                }
            }
        }
        // byLabel med ikke-strenge: posterne droppes med note; strenge beholdes i rækkefølge.
        let (cfg, notes) = parse_text(
            r#"{"watch": {"playbook": {"byLabel": {"z": "bug", "a": 5, "b": "feature"}}}}"#,
        );
        assert_eq!(
            notes,
            ["project.json: watch.playbook.byLabel.«a» ignoreres: ugyldigt playbook-navn"]
        );
        assert_eq!(
            cfg.unwrap().playbook,
            rule(&[("z", "bug"), ("b", "feature")], None)
        );
        assert!(matches!(
            probe_playbook(r#"{"watch": {"playbook": "bug"}}"#),
            Some(PlaybookProbe::Name(n)) if n == "bug"
        ));
        assert!(matches!(
            probe_playbook(r#"{"watch": {"playbook": {"byLabel": 5}}}"#),
            Some(PlaybookProbe::Other(_))
        ));
        // Dublet-nøgle på topniveau: probe ulæselig → `Value`-passets (alfabetiske) rækkefølge.
        let text = r#"{"watch": {"playbook": {"byLabel": {"b": "bug", "a": "feature"}}}, "watch": {"playbook": {"byLabel": {"b": "bug", "a": "feature"}}}}"#;
        assert!(probe_playbook(text).is_none());
        let (cfg, _) = parse_text(text);
        assert_eq!(
            cfg.unwrap().playbook,
            rule(&[("a", "feature"), ("b", "bug")], None)
        );
    }

    #[test]
    fn workspace_watch_parses_with_notes() {
        let mut notes = Vec::new();
        let ws = parse_workspace_watch(
            &serde_json::json!({"enabled": false, "maxPerHour": 90, "maxPerDay": 0}),
            &mut notes,
        );
        assert_eq!(
            ws,
            WorkspaceWatch {
                enabled: false,
                max_per_hour: 60,
                max_per_day: 0
            }
        );
        assert_eq!(
            notes,
            [
                "watch.maxPerHour 90 er sat ned til 60 (vagtens loft)",
                "watch.maxPerDay er 0: vagten starter intet"
            ]
        );
        // Review6d N5: en master-kontakt af forkert type slukker vagten (fail-closed).
        for bad in [
            serde_json::json!("false"),
            serde_json::json!("true"),
            serde_json::json!(1),
        ] {
            let mut notes = Vec::new();
            let ws = parse_workspace_watch(&serde_json::json!({ "enabled": bad }), &mut notes);
            assert!(!ws.enabled, "{bad}");
            assert_eq!(notes, ["watch.enabled skal være true/false; vagten er fra"]);
        }
    }

    #[test]
    fn quiet_text_round_trips() {
        assert_eq!(quiet_text((1380, 420)), "23-07");
        assert_eq!(quiet_text(parse_quiet(" 7 - 9 ").unwrap()), "07-09");
        assert_eq!(parse_quiet("+5-3"), None);
        assert_eq!(parse_quiet("005-3"), None);
    }
}
