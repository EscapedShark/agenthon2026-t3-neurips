//! The unit's `card.toml` (mounted next to the scenario at `/input`), read for two decisions:
//! whether the message ledger is a required output, and the CPU budget.
//!
//! The ledger rule is the scorer's own (`qfbench2_track_simulation.limits.requires_message_ledger`):
//! an explicit boolean `[scoring.params].requires_message_ledger` decides; otherwise the ledger is
//! required unless `[task].scenario_family` is `throughput-scale`. Without a readable card the
//! ledger is always written — omitting a required output would fail the unit.
//!
//! Cards are flat files of one-line keys, so the three values are first read by a line scan that
//! accepts only that plain form. Anything else it meets (multi-line or inline values, dotted or
//! quoted keys, repeated tables, other number forms) sends the whole card to the `toml` parser,
//! which is several times slower on a cold start.

use std::path::Path;

use toml::{Table, Value};

/// What the card says about the three values it is read for.
#[derive(Clone, Debug, Default, PartialEq)]
struct Fields {
    /// `[scoring.params].requires_message_ledger`: `Some(Some(b))` for a boolean, `Some(None)` for
    /// a value of another type.
    declared_ledger: Option<Option<bool>>,
    /// `[task].scenario_family`, if it is a string.
    family: Option<String>,
    /// `[environment].cpus`, if it is an integer.
    cpus: Option<i64>,
}

pub struct Card {
    /// `None` without a readable, valid card.
    fields: Option<Fields>,
}

impl Card {
    /// The card in `dir`, if there is a readable one.
    pub fn load(dir: &Path) -> Card {
        let fields = std::fs::read_to_string(dir.join("card.toml")).ok().and_then(|text| parse(&text));
        Card { fields }
    }

    /// Whether a readable card was found.
    pub fn present(&self) -> bool {
        self.fields.is_some()
    }

    pub fn requires_message_ledger(&self) -> bool {
        let Some(card) = &self.fields else { return true };
        if let Some(Some(b)) = card.declared_ledger {
            return b;
        }
        card.family.as_deref() != Some("throughput-scale")
    }

    /// `[environment].cpus`, the container's CPU quota.
    pub fn cpus(&self) -> Option<usize> {
        let cpus = self.fields.as_ref()?.cpus?;
        usize::try_from(cpus).ok().filter(|&n| n > 0)
    }
}

fn parse(text: &str) -> Option<Fields> {
    scan(text).unwrap_or_else(|| text.parse::<Table>().ok().map(|table| from_table(&table)))
}

fn from_table(card: &Table) -> Fields {
    fn table(v: Option<&Value>) -> Option<&Table> {
        v.and_then(Value::as_table)
    }
    let declared = table(table(card.get("scoring")).and_then(|s| s.get("params"))).and_then(|p| p.get("requires_message_ledger"));
    Fields {
        declared_ledger: declared.map(Value::as_bool),
        family: table(card.get("task")).and_then(|t| t.get("scenario_family")).and_then(Value::as_str).map(str::to_string),
        cpus: table(card.get("environment")).and_then(|e| e.get("cpus")).and_then(Value::as_integer),
    }
}

/// The fast path: `Some(fields)` when every line is a comment, a plain `[table]` header or a
/// one-line `key = value`, and the three values have a plain form; `None` (use the full parser)
/// otherwise. Values of other keys are only checked to end on their line.
fn scan(text: &str) -> Option<Option<Fields>> {
    const WS: &[char] = &[' ', '\t'];
    let bare = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    let mut fields = Fields::default();
    // Explicit table headers and full key paths, to decline redefinitions (which the full parser
    // rejects) instead of reading past them.
    let mut tables: Vec<String> = Vec::new();
    let mut keys: Vec<String> = Vec::new();
    let mut table = String::new();
    for line in text.lines() {
        let line = line.trim_matches(WS);
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.contains("\"\"\"") || line.contains("'''") {
            return None;
        }
        if let Some(header) = line.strip_prefix('[') {
            let (name, rest) = header.split_once(']')?;
            let rest = rest.trim_matches(WS);
            if name.starts_with('[') || !(rest.is_empty() || rest.starts_with('#')) {
                return None;
            }
            let segments: Vec<&str> = name.split('.').map(|s| s.trim_matches(WS)).collect();
            if !segments.iter().all(|s| bare(s)) {
                return None;
            }
            table = segments.join(".");
            let redefines = (1..=segments.len()).any(|n| keys.contains(&segments[..n].join(".")));
            if redefines || tables.contains(&table) {
                return None;
            }
            tables.push(table.clone());
            continue;
        }
        let (key, value) = line.split_once('=')?;
        let key = key.trim_matches(WS);
        let value = value.trim_matches(WS);
        if !bare(key) || value.starts_with('{') || !ends_on_line(value) {
            return None;
        }
        let path = if table.is_empty() { key.to_string() } else { format!("{table}.{key}") };
        let prefix = format!("{path}.");
        if keys.contains(&path) || tables.iter().any(|t| *t == path || t.starts_with(&prefix)) {
            return None;
        }
        keys.push(path);
        match (table.as_str(), key) {
            ("scoring.params", "requires_message_ledger") => {
                fields.declared_ledger = Some(Some(match uncommented(value)? {
                    "true" => true,
                    "false" => false,
                    _ => return None,
                }));
            }
            ("task", "scenario_family") => {
                let inner = value.strip_prefix('"')?;
                let (family, rest) = inner.split_once('"')?;
                let rest = rest.trim_matches(WS);
                if family.contains('\\') || !(rest.is_empty() || rest.starts_with('#')) {
                    return None;
                }
                fields.family = Some(family.to_string());
            }
            ("environment", "cpus") => {
                let digits = uncommented(value)?;
                let digits = digits.strip_prefix('+').unwrap_or(digits);
                if digits.is_empty() || digits.len() > 18 || !digits.bytes().all(|b| b.is_ascii_digit()) {
                    return None;
                }
                if digits.len() > 1 && digits.starts_with('0') {
                    return None;
                }
                fields.cpus = Some(digits.parse().ok()?);
            }
            _ => {}
        }
    }
    Some(Some(fields))
}

/// A value without a trailing comment, when it contains no string.
fn uncommented(value: &str) -> Option<&str> {
    let v = value.split('#').next()?.trim_matches(&[' ', '\t'][..]);
    (!v.contains(['"', '\''])).then_some(v)
}

/// Whether a value's brackets close on its own line (outside strings and a trailing comment).
fn ends_on_line(value: &str) -> bool {
    let mut depth = 0i32;
    let mut quote = None;
    let mut escaped = false;
    for c in value.chars() {
        match quote {
            Some('"') if escaped => escaped = false,
            Some('"') if c == '\\' => escaped = true,
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None => match c {
                '"' | '\'' => quote = Some(c),
                '[' | '{' => depth += 1,
                ']' | '}' => depth -= 1,
                '#' => break,
                _ => {}
            },
        }
    }
    depth == 0 && quote.is_none()
}

/// Threads to use: the CPUs this process may run on, capped by the card's quota (4 when unknown
/// — every Track 3 card grants 4, and inside gVisor the host core count may be all that is
/// visible).
pub fn cpu_budget(card: &Card) -> usize {
    host_cpus().min(card.cpus().unwrap_or(4)).max(1)
}

/// The CPUs in this thread's affinity mask: one system call, where `available_parallelism` also
/// reads the cgroup files (the card's quota caps the budget anyway).
fn host_cpus() -> usize {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: `set` is a plain bit set the kernel fills in.
        let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
        if unsafe { libc::sched_getaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &mut set) } == 0 {
            let n = unsafe { libc::CPU_COUNT(&set) };
            if n > 0 {
                return n as usize;
            }
        }
    }
    std::thread::available_parallelism().map_or(1, |n| n.get())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(text: &str) -> Card {
        Card { fields: parse(text) }
    }

    #[test]
    fn ledger_rule_matches_the_scorer() {
        assert!(Card { fields: None }.requires_message_ledger());
        assert!(!card("[scoring.params]\nrequires_message_ledger = false").requires_message_ledger());
        assert!(card("[scoring.params]\nrequires_message_ledger = true\n[task]\nscenario_family = \"throughput-scale\"")
            .requires_message_ledger());
        assert!(!card("[task]\nscenario_family = \"throughput-scale\"").requires_message_ledger());
        assert!(card("[task]\nscenario_family = \"agent-mix\"").requires_message_ledger());
        // A non-boolean declaration is ignored, as `isinstance(declared, bool)` ignores it.
        assert!(!card("[scoring.params]\nrequires_message_ledger = \"no\"\n[task]\nscenario_family = \"throughput-scale\"")
            .requires_message_ledger());
        assert_eq!(card("[environment]\ncpus = 4").cpus(), Some(4));
        assert_eq!(card("[environment]\ncpus = 0").cpus(), None);
        assert!(card("not toml [").requires_message_ledger());
    }

    /// The line scan either declines or agrees with the full parser.
    #[test]
    fn scan_agrees_with_toml() {
        let check = |text: &str, fast: bool| {
            let full = text.parse::<Table>().ok().map(|t| from_table(&t));
            match scan(text) {
                Some(fields) => {
                    assert!(fast, "scanned a card it should decline:\n{text}");
                    assert_eq!(fields, full, "{text}");
                }
                None => assert!(!fast, "declined a plain card:\n{text}"),
            }
        };
        let units = Path::new(env!("CARGO_MANIFEST_DIR")).join("../track3-simulation-public/units");
        let mut n = 0;
        for unit in std::fs::read_dir(&units).into_iter().flatten().flatten() {
            if let Ok(text) = std::fs::read_to_string(unit.path().join("card.toml")) {
                check(&text, true);
                n += 1;
            }
        }
        assert!(n == 0 || n >= 71, "{n} public cards");
        let plain = "# c\n[task]\nid = \"x\" # [\nscenario_family   = \"throughput-scale\"  # c\ntags = [\"a]\", 'b[', 1]\n\
                     [ scoring . params ]\nrequires_message_ledger = true # c\n[environment]\ncpus = +8\nmemory = \"16G\"\n";
        check(plain, true);
        check("[environment]\ncpus = 4\r\n[task]\r\nscenario_family = 'throughput-scale'\r\n", false);
        for declined in [
            "[environment]\ncpus = 0x4",
            "[environment]\ncpus = 4_0",
            "[environment]\ncpus = 4.0",
            "[environment]\ncpus = 04",
            "[scoring]\nparams = { requires_message_ledger = false }",
            "[scoring]\nparams.requires_message_ledger = false",
            "[scoring.params]\nrequires_message_ledger = \"no\"",
            "[task]\nscenario_family = \"throughput\\u002dscale\"",
            "[task]\ntags = [\n\"a\",\n]\nscenario_family = \"throughput-scale\"",
            "[task]\nx = \"\"\"\n[environment]\ncpus = 8\n\"\"\"",
            "[task]\nscenario_family = \"a\"\n[task]\nscenario_family = \"throughput-scale\"",
            "[task]\nscenario_family = \"a\"\nscenario_family = \"b\"",
            "[task]\nparams = 1\n[task.params]\nx = 1",
            "[task.params.x]\ny = 1\n[task]\nparams = 1",
            "[a.b]\nx = 1\n[a]\nb = 2",
            "b = 1\n[b.c]\nx = 1",
            "[[task]]\nscenario_family = \"throughput-scale\"",
            "[\"task\"]\nscenario_family = \"throughput-scale\"",
            "[task]\n\"scenario_family\" = \"throughput-scale\"",
            "[task]\nscenario_family = \"throughput-scale\" junk",
            "not toml [",
        ] {
            check(declined, false);
        }
    }
}
