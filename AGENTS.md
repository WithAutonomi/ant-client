# AGENTS.md

Instructions for any coding agent working in this repo (Claude Code, Codex, OpenCode,
pi and others). Claude Code loads this file through `CLAUDE.md`.

## Changelog

A PR that changes what `ant` or `ant-core` users see adds an entry under
`## [Unreleased]` in `CHANGELOG.md` (Keep a Changelog headings: `Added`,
`Changed`, `Fixed`, `Security`, `Internal`; end the entry with the PR number).
Never write a versioned `## [X.Y.Z]` heading in a feature PR: the release train
creates it when it cuts rc.1, by running `scripts/rotate-changelog.sh X.Y.Z` on
the rc branch. The release workflow ships the `## [X.Y.Z]` section as the notes
of every `ant-cli-vX.Y.Z*` tag, and falls back to GitHub's generated PR list
when the section is missing (V2-1107).
