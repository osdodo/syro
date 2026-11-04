//! What a model's tokens cost: LiteLLM's community price list, fetched at most
//! once a day and kept on disk, over a small built-in table that covers the
//! models used most until the list has arrived (or when it cannot be fetched).

use std::collections::HashMap;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use serde_json::Value;

/// The list most usage tools price with; new models usually land in it within days.
pub const PRICES_URL: &str =
    "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json";

/// How old the copy on disk may get before it is fetched again.
const MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// USD per token.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Rates {
    input: f64,
    output: f64,
    read: f64,
    write: f64,
}

/// One model's rates: the base ones, and the long-context tiers that replace
/// them for a whole request once its prompt is past the tier's size.
#[derive(Debug, Clone, PartialEq)]
struct Price {
    base: Rates,
    /// `(prompt tokens above which it applies, rates)`, smallest first.
    tiers: Vec<(u64, Rates)>,
}

impl Price {
    fn cost(&self, input: u64, output: u64, read: u64, write: u64) -> Option<f64> {
        let prompt = input.checked_add(read)?.checked_add(write)?;
        let rates = self
            .tiers
            .iter()
            .rev()
            .find(|(above, _)| prompt > *above)
            .map_or(self.base, |(_, rates)| *rates);
        Some(
            input as f64 * rates.input
                + read as f64 * rates.read
                + write as f64 * rates.write
                + output as f64 * rates.output,
        )
    }
}

/// The fetched list, by model name.
#[derive(Debug, Default)]
pub struct Table(HashMap<String, Price>);

impl Table {
    /// LiteLLM's JSON, or `None` when it is not that (an error page, a cut-off
    /// download): a list with no priced model in it is not a list.
    pub fn parse(text: &str) -> Option<Table> {
        let value: Value = serde_json::from_str(text).ok()?;
        let models: HashMap<String, Price> = value
            .as_object()?
            .iter()
            .filter_map(|(name, spec)| Some((name.clone(), price(spec)?)))
            .collect();
        (!models.is_empty()).then_some(Table(models))
    }

    /// The model as a transcript names it, then without a `provider/` prefix,
    /// then without a release date (`claude-x-20260101` is priced as `claude-x`).
    fn get(&self, model: &str) -> Option<&Price> {
        let bare = model.rsplit('/').next().unwrap_or(model);
        self.0
            .get(model)
            .or_else(|| self.0.get(bare))
            .or_else(|| self.0.get(undated(bare)))
    }
}

/// One entry's rates. A cache price the list leaves out is charged as plain input.
fn price(spec: &Value) -> Option<Price> {
    let field = |name: String| {
        spec.get(&name)?
            .as_f64()
            .filter(|rate| rate.is_finite() && *rate >= 0.)
    };
    let rates = |suffix: &str, fallback: Option<Rates>| {
        let input = field(format!("input_cost_per_token{suffix}")).or(fallback.map(|r| r.input))?;
        let output =
            field(format!("output_cost_per_token{suffix}")).or(fallback.map(|r| r.output))?;
        Some(Rates {
            input,
            output,
            read: field(format!("cache_read_input_token_cost{suffix}"))
                .or(fallback.map(|r| r.read))
                .unwrap_or(input),
            write: field(format!("cache_creation_input_token_cost{suffix}"))
                .or(fallback.map(|r| r.write))
                .unwrap_or(input),
        })
    };
    let base = rates("", None)?;
    // `input_cost_per_token_above_272k_tokens`, and only that shape: the same
    // prefix also carries `_batches`/`_priority` variants, which are not ours.
    let mut tiers: Vec<(u64, Rates)> = spec
        .as_object()?
        .keys()
        .filter_map(|key| {
            let thousands: u64 = key
                .strip_prefix("input_cost_per_token_above_")?
                .strip_suffix("k_tokens")?
                .parse()
                .ok()?;
            let rates = rates(&format!("_above_{thousands}k_tokens"), Some(base))?;
            Some((thousands * 1000, rates))
        })
        .collect();
    tiers.sort_by_key(|(above, _)| *above);
    Some(Price { base, tiers })
}

/// `name-20260101` or `name-2026-01-01` → `name`.
fn undated(model: &str) -> &str {
    let digits =
        |part: &str, len: usize| part.len() == len && part.bytes().all(|b| b.is_ascii_digit());
    if let Some((head, tail)) = model.rsplit_once('-')
        && digits(tail, 8)
    {
        return head;
    }
    let parts: Vec<&str> = model.rsplitn(4, '-').collect();
    if let [day, month, year, head] = parts[..]
        && digits(day, 2)
        && digits(month, 2)
        && digits(year, 4)
    {
        return head;
    }
    model
}

static TABLE: RwLock<Option<Arc<Table>>> = RwLock::new(None);
/// Bumped whenever the table changes, so a scan knows its cached costs are stale.
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// What these tokens cost in USD: the fetched list first, the built-in table
/// second, `None` for a model neither knows.
pub fn cost(model: &str, input: u64, output: u64, read: u64, write: u64) -> Option<f64> {
    let table = TABLE.read().ok().and_then(|table| table.clone());
    table
        .as_ref()
        .and_then(|table| table.get(model))
        .and_then(|price| price.cost(input, output, read, write))
        .or_else(|| builtin(model, input, output, read, write))
}

/// Which table the costs come from; it changes when a new list is installed.
pub fn generation() -> u64 {
    GENERATION.load(Ordering::Relaxed)
}

fn install(table: Table) {
    if let Ok(mut slot) = TABLE.write() {
        *slot = Some(Arc::new(table));
        GENERATION.fetch_add(1, Ordering::Relaxed);
    }
}

/// Load the copy at `path` the first time, and fetch a fresh one when it is
/// missing or a day old. Blocking — it may download — so run it off the UI
/// thread; a failed fetch leaves the copy and the built-in table in place, and
/// the next call tries again.
pub fn refresh(path: &Path) {
    if TABLE.read().is_ok_and(|table| table.is_none())
        && let Some(table) = std::fs::read_to_string(path)
            .ok()
            .and_then(|text| Table::parse(&text))
    {
        install(table);
    }
    let fresh = std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| modified.elapsed().ok())
        .is_some_and(|age| age < MAX_AGE);
    if !fresh && let Some(table) = download(path) {
        install(table);
    }
}

/// Fetch the list to `path`, through a partial file so a cut-off download
/// never replaces a good copy.
fn download(path: &Path) -> Option<Table> {
    let partial = path.with_extension("partial");
    std::fs::create_dir_all(path.parent()?).ok()?;
    // macOS's own client, because it goes through the system proxy; `curl`
    // does not read it, and the list is on GitHub.
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut command = Command::new("/usr/bin/nscurl");
        command.arg("-o").arg(&partial).arg(PRICES_URL);
        command
    };
    #[cfg(not(target_os = "macos"))]
    let mut command = {
        let mut command = Command::new("curl");
        command
            .args(["-fsSL", "--max-time", "120", "-o"])
            .arg(&partial)
            .arg(PRICES_URL);
        command
    };
    let fetched = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    // `nscurl` exits 0 on a 404 too: the parse is what says it is the list.
    let table = fetched
        .then(|| std::fs::read_to_string(&partial).ok())
        .flatten()
        .and_then(|text| Table::parse(&text));
    if table.is_some() && std::fs::rename(&partial, path).is_ok() {
        return table;
    }
    let _ = std::fs::remove_file(&partial);
    None
}

/// OpenAI Standard USD per 1M tokens, checked 2026-10-16 (gpt-6-astra 2026-11-04):
/// https://help.openai.com/en/articles/20001415-chatgpt-rate-card-enterprise-token-based-pricing
/// https://developers.openai.com/api/docs/pricing
/// Cache-write rate (1.25x input): https://developers.openai.com/api/docs/guides/prompt-caching
/// shortcut: a dated Standard-rate snapshot, only for before the fetched list
/// arrives; Fast/Ultrafast, regional premiums and tool fees are not estimated.
fn builtin(model: &str, input: u64, output: u64, read: u64, write: u64) -> Option<f64> {
    let (input_rate, read_rate, output_rate) = match model {
        "gpt-6-astra" => (10., 1., 50.),
        "gpt-6.1-sol" => (2., 0.1, 10.),
        "gpt-6-sol" => (2., 0.2, 10.),
        // Current promotion, published as available at least through 2026-11-21.
        "gpt-5.6-sol" => (4., 0.4, 20.),
        _ => return None,
    };
    let prompt = input.checked_add(read)?.checked_add(write)?;
    // Long-context prices apply to the whole request, not only the excess.
    let (input_multiplier, output_multiplier) = if prompt > 272_000 {
        (2., 1.5)
    } else {
        (1., 1.)
    };
    Some(
        ((input as f64 * input_rate + read as f64 * read_rate + write as f64 * input_rate * 1.25)
            * input_multiplier
            + output as f64 * output_rate * output_multiplier)
            / 1_000_000.,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape LiteLLM writes, trimmed to what is read plus a variant that is not.
    const SAMPLE: &str = r#"{
        "sample_spec": {"input_cost_per_token": "string"},
        "gpt-6-astra": {
            "input_cost_per_token": 1e-05,
            "output_cost_per_token": 5e-05,
            "cache_read_input_token_cost": 1e-06,
            "cache_creation_input_token_cost": 1.25e-05,
            "input_cost_per_token_above_272k_tokens": 2e-05,
            "output_cost_per_token_above_272k_tokens": 7.5e-05,
            "cache_read_input_token_cost_above_272k_tokens": 2e-06,
            "cache_creation_input_token_cost_above_272k_tokens": 2.5e-05,
            "input_cost_per_token_above_272k_tokens_priority": 4e-05
        },
        "claude-x": {"input_cost_per_token": 3e-06, "output_cost_per_token": 1.5e-05},
        "image-model": {"output_cost_per_image": 0.04}
    }"#;

    #[test]
    fn the_list_prices_like_the_official_rates() {
        let table = Table::parse(SAMPLE).unwrap();
        // Unpriced and malformed entries are left out, not guessed at.
        assert_eq!(table.0.len(), 2);
        let astra = table.get("gpt-6-astra").unwrap();
        for (input, output, read, write) in
            [(100_000, 1_000, 80_000, 10_000), (300_000, 2_000, 0, 0)]
        {
            let listed = astra.cost(input, output, read, write).unwrap();
            let official = builtin("gpt-6-astra", input, output, read, write).unwrap();
            assert!((listed - official).abs() < 1e-9, "{listed} vs {official}");
        }
        // A missing cache price is plain input.
        let claude = table.get("claude-x").unwrap();
        assert!((claude.cost(0, 0, 1_000_000, 1_000_000).unwrap() - 6.).abs() < 1e-9);
    }

    #[test]
    fn a_model_is_found_by_prefix_and_release_date() {
        let table = Table::parse(SAMPLE).unwrap();
        for name in [
            "gpt-6-astra",
            "openai/gpt-6-astra",
            "gpt-6-astra-20260903",
            "gpt-6-astra-2026-09-03",
        ] {
            assert!(table.get(name).is_some(), "{name}");
        }
        assert!(table.get("gpt-6-astra-law").is_none());
        assert_eq!(undated("gpt-6-sol"), "gpt-6-sol");
    }

    #[test]
    fn what_is_not_the_list_is_refused() {
        assert!(Table::parse("404: Not Found").is_none());
        assert!(Table::parse("{}").is_none());
        assert!(Table::parse(r#"{"x": {"output_cost_per_image": 1}}"#).is_none());
    }
}
