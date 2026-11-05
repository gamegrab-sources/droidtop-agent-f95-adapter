# Changelog

Every green build of `main` is a release named `build-<run number>`; this file
records what each change brought.

## Unreleased

- Adapter protocol 2 (Droidtop/tracker#794): `snapshot` gives F95Checker's
  threads for the agent to publish as the context's copy, and `received`
  takes the merged copy back, writing only what the handheld changed since
  the last snapshot and only while F95Checker is closed. Needs a
  droidtop-agent with snapshot support. Two reads at once no longer share
  one temporary copy of the database.
- First version: the F95Checker reader that was droidtop-agent's built-in
  `f95checker` context, as a context adapter program (adapter protocol 1).
