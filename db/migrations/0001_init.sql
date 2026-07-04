-- PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS users (
    username      TEXT PRIMARY KEY,
    password_hash TEXT NOT NULL,
    created_at    INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS trips (
    id         INTEGER PRIMARY KEY,
    username   TEXT NOT NULL REFERENCES users(username) ON DELETE CASCADE,
    started_at INTEGER NOT NULL,
    ended_at   INTEGER
);

CREATE INDEX IF NOT EXISTS idx_trips_user_started
    ON trips(username, started_at);
