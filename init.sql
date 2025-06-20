CREATE TABLE air_quality (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    ppm REAL NOT NULL,
    timestamp TEXT DEFAULT (datetime('now'))
);
