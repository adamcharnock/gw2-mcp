---
name: refresh-festivals
description: Refresh data/festivals.yaml from the GW2 wiki — recompute typical MM-DD windows for each annual festival from the most recent 3-5 years of occurrences, validate the result, and stage the change.
---

# refresh-festivals

Refresh `data/festivals.yaml` from the GW2 wiki. The runtime
`get_active_festivals` MCP tool reads this file, so its accuracy
determines whether the LLM tells the user the right things about
Halloween / Wintersday / etc.

## What this skill produces

A fresh `data/festivals.yaml` with:

- `schema_version: 1` (do not change; the runtime rejects unknown versions)
- `last_updated: YYYY-MM-DD` (today's date)
- `sources: [...]` (the wiki URLs you actually consulted)
- `festivals: [...]` (one entry per festival)

Each festival entry:

```yaml
- name: "Dragon Bash"          # Display name. Match the wiki page title.
  typical_start: "06-07"       # MM-DD, year-agnostic. Median start MM-DD
                               # across the most recent 3-5 occurrences.
  typical_end: "06-28"         # MM-DD, year-agnostic. Median end MM-DD.
                               # Wintersday wraps the year boundary
                               # (e.g. 12-12 → 01-02) — that's allowed.
  wiki_url: https://wiki.guildwars2.com/wiki/Dragon_Bash
```

## Festivals to include

These six. Don't add or remove without a discussion with the maintainer:

1. **Lunar New Year** — wiki.guildwars2.com/wiki/Lunar_New_Year
2. **Super Adventure Festival** — wiki.guildwars2.com/wiki/Super_Adventure_Festival
3. **Dragon Bash** — wiki.guildwars2.com/wiki/Dragon_Bash
4. **Festival of the Four Winds** — wiki.guildwars2.com/wiki/Festival_of_the_Four_Winds
5. **Halloween** (officially "Shadow of the Mad King") — wiki.guildwars2.com/wiki/Shadow_of_the_Mad_King
6. **Wintersday** — wiki.guildwars2.com/wiki/Wintersday

## Procedure

1. **Fetch each festival's wiki page in raw form** via:
   `https://wiki.guildwars2.com/index.php?title=<PAGE>&action=raw`

   The raw wikitext exposes the prose history (e.g. "Dragon Bash 2024
   ran from June 11 to July 2") that you'll need.

2. **Extract the most recent 3-5 occurrences.** Look in the page's
   intro or a "History" section. Sometimes the dates live in a
   `{{Festival timeline}}` or similar template — read the source.

3. **Compute typical MM-DD as a median across those occurrences.**
   ArenaNet shifts dates by a few days year-to-year; the median is a
   reasonable forecast for "what's about right".

4. **Write the YAML** to `data/festivals.yaml`. Preserve
   `schema_version: 1`. Set `last_updated` to today (YYYY-MM-DD).
   Order festivals by typical_start chronologically.

5. **Validate** by running `cargo test --test festivals_schema`. If
   anything fails, fix the YAML before continuing — DO NOT commit a
   YAML the test rejects.

6. **Stage** the change with `git add data/festivals.yaml`.

7. **Stop.** Do not commit yourself; let the maintainer review.

## Year-wrap

`typical_end < typical_start` lexicographically (as MM-DD) means the
festival wraps the year boundary. Only Wintersday does this currently
(12-12 → 01-02). The runtime handles it; just write the MM-DD pair
truthfully.

## Caveat to surface in the YAML

The runtime tool flags every response as `approximate: true` and
includes a disclaimer note. That's why this file lives in-repo rather
than being fetched live — annual festival dates aren't published in
machine-readable form anywhere on ANet's site, so the "best-effort
based on prior years" framing is honest.

## When this skill should be re-run

- Whenever a festival concludes (its actual dates this year can refine
  the median).
- At minimum once every 30 days. The pre-commit hook in `lefthook.yml`
  blocks commits with a `last_updated` older than 30 days; if it
  fires, run this skill.
