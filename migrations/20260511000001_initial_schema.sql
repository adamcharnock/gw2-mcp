-- Initial schema for the on-disk search index (Tier 6C).
--
-- Mirrors the original rusqlite-era `init_schema` block verbatim so that
-- existing user databases (with these tables already present) migrate
-- without touching data. SQLX records this migration as applied either
-- way; future migrations build on top.
--
-- Schema convention per kind:
--   * a parent table with typed columns + a `raw_json TEXT` payload
--   * a contentless FTS5 mirror named `<kind>_fts`
--   * INSERT/DELETE/UPDATE triggers that keep the FTS mirror in sync
--
-- FTS5 tokenizer is `unicode61 remove_diacritics 1` so "Café" matches
-- "cafe" and vice versa.

CREATE TABLE IF NOT EXISTS meta (
    key TEXT PRIMARY KEY NOT NULL,
    value TEXT NOT NULL
);

-- Skills ---------------------------------------------------------------
CREATE TABLE IF NOT EXISTS skills (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    description TEXT,
    type TEXT,
    slot TEXT,
    professions TEXT,
    weapon_type TEXT,
    chat_link TEXT,
    raw_json TEXT NOT NULL,
    fetched_at INTEGER NOT NULL,
    build INTEGER NOT NULL
);
CREATE VIRTUAL TABLE IF NOT EXISTS skills_fts USING fts5(
    name, description,
    content='skills', content_rowid='id',
    tokenize='unicode61 remove_diacritics 1'
);
CREATE TRIGGER IF NOT EXISTS skills_fts_insert AFTER INSERT ON skills BEGIN
    INSERT INTO skills_fts(rowid, name, description)
    VALUES (new.id, new.name, new.description);
END;
CREATE TRIGGER IF NOT EXISTS skills_fts_delete AFTER DELETE ON skills BEGIN
    INSERT INTO skills_fts(skills_fts, rowid, name, description)
    VALUES('delete', old.id, old.name, old.description);
END;
CREATE TRIGGER IF NOT EXISTS skills_fts_update AFTER UPDATE ON skills BEGIN
    INSERT INTO skills_fts(skills_fts, rowid, name, description)
    VALUES('delete', old.id, old.name, old.description);
    INSERT INTO skills_fts(rowid, name, description)
    VALUES (new.id, new.name, new.description);
END;

-- Traits ---------------------------------------------------------------
CREATE TABLE IF NOT EXISTS traits (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    description TEXT,
    specialization INTEGER,
    tier INTEGER,
    slot TEXT,
    raw_json TEXT NOT NULL,
    fetched_at INTEGER NOT NULL,
    build INTEGER NOT NULL
);
CREATE VIRTUAL TABLE IF NOT EXISTS traits_fts USING fts5(
    name, description,
    content='traits', content_rowid='id',
    tokenize='unicode61 remove_diacritics 1'
);
CREATE TRIGGER IF NOT EXISTS traits_fts_insert AFTER INSERT ON traits BEGIN
    INSERT INTO traits_fts(rowid, name, description)
    VALUES (new.id, new.name, new.description);
END;
CREATE TRIGGER IF NOT EXISTS traits_fts_delete AFTER DELETE ON traits BEGIN
    INSERT INTO traits_fts(traits_fts, rowid, name, description)
    VALUES('delete', old.id, old.name, old.description);
END;
CREATE TRIGGER IF NOT EXISTS traits_fts_update AFTER UPDATE ON traits BEGIN
    INSERT INTO traits_fts(traits_fts, rowid, name, description)
    VALUES('delete', old.id, old.name, old.description);
    INSERT INTO traits_fts(rowid, name, description)
    VALUES (new.id, new.name, new.description);
END;

-- Specializations ------------------------------------------------------
CREATE TABLE IF NOT EXISTS specializations (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    profession TEXT,
    elite INTEGER NOT NULL DEFAULT 0,
    raw_json TEXT NOT NULL,
    fetched_at INTEGER NOT NULL,
    build INTEGER NOT NULL
);
CREATE VIRTUAL TABLE IF NOT EXISTS specializations_fts USING fts5(
    name,
    content='specializations', content_rowid='id',
    tokenize='unicode61 remove_diacritics 1'
);
CREATE TRIGGER IF NOT EXISTS specializations_fts_insert AFTER INSERT ON specializations BEGIN
    INSERT INTO specializations_fts(rowid, name) VALUES (new.id, new.name);
END;
CREATE TRIGGER IF NOT EXISTS specializations_fts_delete AFTER DELETE ON specializations BEGIN
    INSERT INTO specializations_fts(specializations_fts, rowid, name)
    VALUES('delete', old.id, old.name);
END;
CREATE TRIGGER IF NOT EXISTS specializations_fts_update AFTER UPDATE ON specializations BEGIN
    INSERT INTO specializations_fts(specializations_fts, rowid, name)
    VALUES('delete', old.id, old.name);
    INSERT INTO specializations_fts(rowid, name) VALUES (new.id, new.name);
END;

-- Items ----------------------------------------------------------------
CREATE TABLE IF NOT EXISTS items (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    description TEXT,
    type TEXT,
    rarity TEXT,
    level INTEGER,
    weight_class TEXT,
    chat_link TEXT,
    raw_json TEXT NOT NULL,
    fetched_at INTEGER NOT NULL,
    build INTEGER NOT NULL
);
CREATE VIRTUAL TABLE IF NOT EXISTS items_fts USING fts5(
    name, description,
    content='items', content_rowid='id',
    tokenize='unicode61 remove_diacritics 1'
);
CREATE TRIGGER IF NOT EXISTS items_fts_insert AFTER INSERT ON items BEGIN
    INSERT INTO items_fts(rowid, name, description)
    VALUES (new.id, new.name, new.description);
END;
CREATE TRIGGER IF NOT EXISTS items_fts_delete AFTER DELETE ON items BEGIN
    INSERT INTO items_fts(items_fts, rowid, name, description)
    VALUES('delete', old.id, old.name, old.description);
END;
CREATE TRIGGER IF NOT EXISTS items_fts_update AFTER UPDATE ON items BEGIN
    INSERT INTO items_fts(items_fts, rowid, name, description)
    VALUES('delete', old.id, old.name, old.description);
    INSERT INTO items_fts(rowid, name, description)
    VALUES (new.id, new.name, new.description);
END;

-- Achievements ---------------------------------------------------------
CREATE TABLE IF NOT EXISTS achievements (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    description TEXT,
    requirement TEXT,
    type TEXT,
    categories TEXT,
    repeatable INTEGER,
    points INTEGER,
    raw_json TEXT NOT NULL,
    fetched_at INTEGER NOT NULL,
    build INTEGER NOT NULL
);
CREATE VIRTUAL TABLE IF NOT EXISTS achievements_fts USING fts5(
    name, description, requirement,
    content='achievements', content_rowid='id',
    tokenize='unicode61 remove_diacritics 1'
);
CREATE TRIGGER IF NOT EXISTS achievements_fts_insert AFTER INSERT ON achievements BEGIN
    INSERT INTO achievements_fts(rowid, name, description, requirement)
    VALUES (new.id, new.name, new.description, new.requirement);
END;
CREATE TRIGGER IF NOT EXISTS achievements_fts_delete AFTER DELETE ON achievements BEGIN
    INSERT INTO achievements_fts(achievements_fts, rowid, name, description, requirement)
    VALUES('delete', old.id, old.name, old.description, old.requirement);
END;
CREATE TRIGGER IF NOT EXISTS achievements_fts_update AFTER UPDATE ON achievements BEGIN
    INSERT INTO achievements_fts(achievements_fts, rowid, name, description, requirement)
    VALUES('delete', old.id, old.name, old.description, old.requirement);
    INSERT INTO achievements_fts(rowid, name, description, requirement)
    VALUES (new.id, new.name, new.description, new.requirement);
END;
