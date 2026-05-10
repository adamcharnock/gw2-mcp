# Test fixtures

These files are **real captured responses** from upstream services, used by the
integration tests to verify our adapters parse production-shaped data
correctly. Do **not** edit them by hand — re-capture if the upstream shape
changes.

## Re-capture commands

Run these from the repo root. They write fresh fixtures into this directory.

```bash
# Guild Wars 2 v2 API (no auth required — these endpoints are public)
curl -sSL 'https://api.guildwars2.com/v2/skills?ids=9137,5503'           > tests/fixtures/gw2_skills.json
curl -sSL 'https://api.guildwars2.com/v2/specializations?ids=42,1'       > tests/fixtures/gw2_specializations.json
curl -sSL 'https://api.guildwars2.com/v2/traits?ids=648,214'             > tests/fixtures/gw2_traits.json

# Discretize — public GitHub repo, no auth
curl -sSL 'https://api.github.com/repos/discretize/discretize-guides/git/trees/master?recursive=1' \
  > tests/fixtures/discretize_tree.json
curl -sSL 'https://raw.githubusercontent.com/discretize/discretize-guides/master/builds/guardian/power-dragonhunter/index.md' \
  > tests/fixtures/discretize_power_dragonhunter.md

# MetaBattle — public MediaWiki API, no auth
curl -sSL 'https://metabattle.com/wiki/api.php?action=query&list=categorymembers&cmtitle=Category:Meta_builds&cmlimit=10&format=json' \
  > tests/fixtures/metabattle_list.json
curl -sSL 'https://metabattle.com/wiki/api.php?action=parse&page=Build:Berserker%20-%20Power%20Berserker&prop=wikitext&format=json' \
  > tests/fixtures/metabattle_parse.json

# Snow Crows — public HTML page (always set a User-Agent that identifies you)
curl -sSL 'https://snowcrows.com/builds/raids/elementalist/celestial-alacrity-tempest-scepter-warhorn' \
  -H 'User-Agent: gw2-mcp-fixture/0.1 (+https://github.com/adamcharnock/gw2-mcp)' \
  > tests/fixtures/snowcrows_build.html
```

## Synthetic fixtures

Two fixtures are hand-written, not captured, because the corresponding
endpoints require an authenticated GW2 API key:

- `buildtabs_sample.json` — shaped exactly like `/v2/characters/:name/buildtabs?tabs=all`.
- `equipmenttabs_sample.json` — shaped exactly like `/v2/characters/:name/equipmenttabs?tabs=all`.

If you have a real key handy and want to replace these with captured data,
the curl invocation is:

```bash
curl -sSL -H "Authorization: Bearer $GW2_API_KEY" \
  'https://api.guildwars2.com/v2/characters/<NAME>/buildtabs?tabs=all'     > tests/fixtures/buildtabs_sample.json
curl -sSL -H "Authorization: Bearer $GW2_API_KEY" \
  'https://api.guildwars2.com/v2/characters/<NAME>/equipmenttabs?tabs=all' > tests/fixtures/equipmenttabs_sample.json
```

Replace `<NAME>` with a character on your account (URL-encode spaces).
