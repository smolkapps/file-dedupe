# Worklog

## 2026-10-16 — Studio CI migration

Goal: run CI on the local Mac Studio with a simple hosted rollback.
Changed only runner selection; original triggers, job names and test/build steps remain.
Same-repository revisions use the dedicated serial Studio runner; public fork PRs retain free hosted Ubuntu validation.
Set repository variable `STUDIO_CI_UBUNTU_LATEST` to the JSON string `"ubuntu-latest"` to restore hosted execution.
Structural YAML comparison passed. Actual migration run is queued for local validation; final receipts and controller instructions are in `smolkapps/new-app-publisher-agent/tools/studio-ci/`.
Next: inspect the actual CI result; retain existing failures separately from runtime setup failures.
