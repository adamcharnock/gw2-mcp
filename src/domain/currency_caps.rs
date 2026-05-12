//! Hand-curated table of GW2 currencies that have a documented holding
//! or weekly-earn cap. Used to annotate `WalletEntry` so the LLM can
//! proactively warn "you're about to cap" without doing the math itself.
//!
//! **Why hardcoded?** The GW2 v2 API does not expose currency caps, and
//! the wiki only documents them in prose on each currency's page
//! (no machine-readable list). Per project policy this is the rare
//! case where in-repo data is justified — every entry below cites the
//! exact wiki page it was sourced from, so future re-verifies are a
//! one-WebFetch round-trip. Update this table when `ANet` changes a cap
//! (rare; happens at most a few times per year).
//!
//! Two cap dimensions are tracked because they answer different
//! questions:
//! - `holding_cap`: maximum balance the wallet can show. Astral
//!   Acclaim (cap 1300) is the canonical example — it doesn't expire,
//!   but once you hit the cap, new objective rewards are FORFEIT until
//!   you spend down. The right LLM framing is "don't let earning go to
//!   waste" — NOT "spend before it expires."
//! - `weekly_earn_cap`: maximum a player can gain per weekly reset.
//!   The LLM can answer "Magnetite Shards earn up to 800 per week from
//!   raids — plan your week accordingly". This is the more common
//!   shape, but it requires the LLM to compare against weekly delta
//!   (which the API doesn't directly expose).

use std::collections::BTreeMap;
use std::sync::OnceLock;

use crate::domain::CurrencyId;

/// One cap entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CurrencyCap {
    /// Maximum balance the wallet can hold. Players may briefly exceed
    /// this if they claim rewards while under-cap (per Astral Acclaim
    /// wiki note).
    pub holding_cap: Option<i64>,
    /// Maximum amount a player can earn per weekly reset.
    pub weekly_earn_cap: Option<i64>,
    /// The wiki URL the cap was sourced from. Surfaced to the LLM so
    /// the user can verify the claim themselves if needed.
    pub source_url: &'static str,
}

/// Threshold used to flag a balance as "at risk" of capping. 80% of
/// `holding_cap`.
pub const AT_RISK_FRACTION_NUM: i64 = 4;
pub const AT_RISK_FRACTION_DEN: i64 = 5; // 4/5 = 80%

/// Lookup a cap by currency id. `None` if the currency has no
/// documented cap.
#[must_use]
pub fn get_cap(id: CurrencyId) -> Option<&'static CurrencyCap> {
    table().get(&id)
}

/// Compute the at-risk flag: true when `holding_cap` exists and
/// `balance >= 4/5 * holding_cap`. Weekly earn caps don't drive
/// at-risk flagging — they're informational, not actionable from a
/// single point-in-time balance.
#[must_use]
pub fn is_at_risk(balance: i64, cap: &CurrencyCap) -> Option<bool> {
    let h = cap.holding_cap?;
    Some(balance * AT_RISK_FRACTION_DEN >= h * AT_RISK_FRACTION_NUM)
}

fn table() -> &'static BTreeMap<CurrencyId, CurrencyCap> {
    static TABLE: OnceLock<BTreeMap<CurrencyId, CurrencyCap>> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut m = BTreeMap::new();

        // Astral Acclaim — Wizard's Vault currency. Holding cap 1300;
        // exceedable briefly when claiming below-cap rewards.
        m.insert(
            CurrencyId::new(63).expect("valid"),
            CurrencyCap {
                holding_cap: Some(1300),
                weekly_earn_cap: None,
                source_url: "https://wiki.guildwars2.com/wiki/Astral_Acclaim",
            },
        );

        // Magnetite Shards — earned from raid encounters. Weekly cap
        // on encounter rewards (vendor purchases not capped).
        m.insert(
            CurrencyId::new(28).expect("valid"),
            CurrencyCap {
                holding_cap: None,
                weekly_earn_cap: Some(800),
                source_url: "https://wiki.guildwars2.com/wiki/Magnetite_Shard",
            },
        );

        // Gaeting Crystals — Path of Fire raid currency. Weekly cap on
        // boss kill / wipe rewards.
        m.insert(
            CurrencyId::new(39).expect("valid"),
            CurrencyCap {
                holding_cap: None,
                weekly_earn_cap: Some(150),
                source_url: "https://wiki.guildwars2.com/wiki/Gaeting_Crystal",
            },
        );

        m
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn astral_acclaim_lookup() {
        let cap = get_cap(CurrencyId::new(63).unwrap()).expect("astral acclaim has cap");
        assert_eq!(cap.holding_cap, Some(1300));
    }

    #[test]
    fn at_risk_returns_none_for_weekly_only_currency() {
        let cap = get_cap(CurrencyId::new(28).unwrap()).expect("magnetite has earn cap");
        // No holding cap → at-risk is not computable.
        assert_eq!(is_at_risk(10_000, cap), None);
    }

    #[test]
    fn at_risk_true_at_80_percent_of_holding_cap() {
        let cap = get_cap(CurrencyId::new(63).unwrap()).unwrap();
        // 80% of 1300 = 1040.
        assert_eq!(is_at_risk(1039, cap), Some(false));
        assert_eq!(is_at_risk(1040, cap), Some(true));
        assert_eq!(is_at_risk(1300, cap), Some(true));
    }

    #[test]
    fn unknown_currency_returns_none() {
        assert!(get_cap(CurrencyId::new(99999).unwrap()).is_none());
    }
}
