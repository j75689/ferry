# Contributing

## Commit messages

Commits follow [Conventional Commits 1.0.0](https://www.conventionalcommits.org/en/v1.0.0/):

```
<type>[optional scope]: <description>

[optional body]

[optional footer(s)]
```

* **type**: `feat` (new feature), `fix` (bug fix), `docs`, `style`, `refactor`,
  `perf`, `test`, `build`, `ci`, `chore`, or `revert`.
* **scope** (optional): the part of the project touched, such as `proxy`,
  `cli`, `desktop`, `ui`, or `ci`.
* **description**: imperative mood, lower case, no trailing period, about 72
  characters at most.
* **body**: what changed and why, wrapped at 72 columns.
* **breaking changes**: add `!` after the type or scope, and/or a
  `BREAKING CHANGE: <description>` footer.

Examples:

```
feat(proxy): resume streams that break mid-answer
fix(ui): keep unsaved edits when the status refreshes
ci: bundle installers only on manual runs and v* tags
feat(cli)!: rename the config env var to FERRY_CONFIG
```

## Code

* Code, comments, identifiers, UI text, and docs are written in English.
* Before pushing: `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings`,
  and `cargo test`.
