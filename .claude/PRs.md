# PRs

Reference for opening Jira tickets and raising PRs in this repo, plus a log of
PRs raised through Claude Code.

## Workflow

1. **Jira ticket** (via `acli`, authed already):
   - Project key: `SO` (https://suioptions.atlassian.net).
   - Create under the relevant epic with `--parent`:
     - Frontend → `SO-19`
     - Quote Service → `SO-6`, Indexer → `SO-7`, Protocol → `SO-8`
     - Writer MM Bot → `SO-9`, Trader MM Bot → `SO-10`
     - Contract Deployment Tracking → `SO-11`, CI/CD → `SO-12`
     - Data Layer → `SO-32`, Token Info Service → `SO-99`, Auth Service → `SO-110`
   - Example:
     ```
     acli jira workitem create --project SO --type Task --parent SO-19 \
       --summary "..." --description "..." --json
     ```
   - List epics: `acli jira workitem search --jql 'project = SO AND issuetype = Epic'`

2. **Branch**: `SO-<num>-<short-kebab-description>` off `staging`
   (e.g. `SO-141-consume-token-info-frontend`).

3. **Commit / PR conventions** (matching existing history):
   - Base branch: `staging`.
   - PR title: `[SO-<num>] Title Case Summary`.
   - PR body: a Jira link line `[SO-<num>](https://suioptions.atlassian.net/browse/SO-<num>)`,
     then concise bullets describing the change, optional **Notes**, then the
     Claude Code footer.
   - Keep diffs surgical — don't commit regenerated artifacts
     (`package-lock.json` churn from `npm install`, `tsconfig.tsbuildinfo`).

## Log

| PR | Jira | Epic | Title |
|----|------|------|-------|
| [#131](https://github.com/ewitulsk/SuiOptions/pull/131) | [SO-141](https://suioptions.atlassian.net/browse/SO-141) | SO-19 Frontend | Consume Token-Info Logo, Name & Pyth Feeds on the Frontend |
| [#134](https://github.com/ewitulsk/SuiOptions/pull/134) | [SO-145](https://suioptions.atlassian.net/browse/SO-145) | SO-19 Frontend | Fix Hardcoded Bitcoin Logo on Earn Page; Serve TWAL/TDEEP Logos |
| [#170](https://github.com/ewitulsk/SuiOptions/pull/170) | [SO-182](https://suioptions.atlassian.net/browse/SO-182) | SO-19 Frontend | Integrate PostHog Analytics, Session Replay & Error Tracking |
