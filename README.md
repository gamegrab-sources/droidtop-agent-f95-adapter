# droidtop-agent-f95-adapter

The F95 plugin's context adapter for
[droidtop-agent](https://github.com/Droidtop/droidtop-agent): it keeps
F95Checker's watched threads on your computer in step with
[droidtop-plugin-f95](https://github.com/gamegrab-sources/droidtop-plugin-f95)
on your handheld. Unofficial; part of gamegrab-sources, not of droidtop.

- Threads you watch in F95Checker show up in the plugin, and threads the
  plugin links are added to F95Checker (F95Checker's own refresh fills in
  the rest).
- Your marks travel both ways: the installed and finished versions,
  archived, rating and notes.
- F95Checker's name, version, developer, status and last update come to the
  handheld.

It reads only those columns of F95Checker's `games` table. It never reads
the `cookies` table or the passwords and tokens in its settings. It writes
only while F95Checker is closed, because F95Checker keeps its database in
memory and writes it back; while it is open, changes wait for the next sync.
Before writing, it keeps one copy of the database beside it as
`db.sqlite3.droidtop-agent-backup`.

## Use

Download the program for your system from the latest release, then:

```
droidtop-agent contexts add <path to droidtop-agent-f95-adapter>
droidtop-agent contexts          # shows the f95checker context and its record count
```

`F95CHECKER_DB` points it at a database other than F95Checker's usual one
(`%APPDATA%\f95checker`, `~/.config/f95checker`,
`~/Library/Application Support/f95checker`).

## How droidtop-agent runs it

Adapter protocol 1 (droidtop-agent's docs/DESIGN.md section 8): one run per
sync, JSON over standard input and output.

```
droidtop-agent-f95-adapter describe   # {"protocol":1,"id":"f95checker","description":...}
droidtop-agent-f95-adapter pull       # {"records":{"<thread id>":{"name":...,"installed":...}}}
droidtop-agent-f95-adapter push       # reads {"changes":[...]}, prints {"deferred":null|"why"}
```

## Build

```
cargo build --release
```

CI builds it for Windows, Linux and macOS on every push, and each green build
of `main` is a release.

## Licence

GPL-3.0-only.
