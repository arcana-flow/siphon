# Store download presets in SQLite

Siphon stores named download presets and the selected default in its local SQLite database. The `preset` commands manage both in one store and can update them in a single transaction. A TOML file would make presets easier to edit by hand and copy between machines.
