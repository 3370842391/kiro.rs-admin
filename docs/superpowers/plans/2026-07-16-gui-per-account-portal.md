# GUI Per-Account Enterprise Portal Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Accept `{account}|{password}|{start_url}` in the GUI and use each account's AWS enterprise portal during browserless login.

**Architecture:** Carry an optional `start_url` on `AccountEntry`, resolve the effective URL at the runtime boundary with per-entry precedence, and keep GUI validation aware of whether every parsed entry supplies a portal. Extend the enterprise HTTP target parser for `ssoins-*.portal.<region>.app.aws` while preserving legacy `awsapps.com` behavior.

**Tech Stack:** Python 3.11+, tkinter/ttk, unittest, curl_cffi.

---

### Task 1: Parse and validate the third field

**Files:**
- Modify: `scripts/batch_login/models.py`
- Modify: `scripts/batch_login/input_parser.py`
- Test: `tests/batch_login/test_input_parser.py`

- [ ] Add failing tests for `{account}|{password}|{start_url}`, passwords containing `|`, rendering, and rejection of non-HTTPS or credential-bearing URLs.
- [ ] Run `python -m unittest tests.batch_login.test_input_parser -v` and confirm the new tests fail for missing third-field support.
- [ ] Add `AccountEntry.start_url`, compile the optional placeholder, greedily preserve password text, and validate the parsed URL.
- [ ] Re-run `python -m unittest tests.batch_login.test_input_parser -v` and confirm all parser tests pass.

### Task 2: Support SSO instance portal discovery

**Files:**
- Modify: `scripts/batch_login/enterprise_http.py`
- Test: `tests/batch_login/test_enterprise_http.py`

- [ ] Add a failing fixture-transport test for `ssoins-*.portal.<region>.app.aws` resolving to a validated regional signin redirect and real directory ID.
- [ ] Run `python -m unittest tests.batch_login.test_enterprise_http -v` and confirm the new test fails for unsupported portal format.
- [ ] Implement portal target parsing, regional API endpoint selection, strict redirect host/path validation, and directory discovery.
- [ ] Re-run `python -m unittest tests.batch_login.test_enterprise_http -v` and confirm all enterprise HTTP tests pass.

### Task 3: Resolve effective portal per account

**Files:**
- Modify: `scripts/batch_login/local_runner.py`
- Test: `tests/batch_login/test_local_runner.py`

- [ ] Add a failing test proving `entry.start_url` overrides the global URL and is also used as the enterprise checkpoint scope.
- [ ] Run `python -m unittest tests.batch_login.test_local_runner -v` and confirm the new test fails.
- [ ] Resolve `entry.start_url or settings.start_url` at authentication and checkpoint boundaries.
- [ ] Re-run `python -m unittest tests.batch_login.test_local_runner -v` and confirm all runner tests pass.

### Task 4: Extend GUI preview and validation

**Files:**
- Modify: `scripts/batch_login/gui_app.py`
- Modify: `scripts/batch_login/gui_controller.py`
- Test: `tests/batch_login/test_gui_controller.py`

- [ ] Add failing tests for starting with all per-entry URLs, rejecting partial missing URLs, and selecting a single common URL for GUI autofill.
- [ ] Run `python -m unittest tests.batch_login.test_gui_controller -v` and confirm the new tests fail.
- [ ] Add the three-field preset, enterprise portal preview column, common-URL autofill, multi-URL status, and conditional Start URL validation.
- [ ] Re-run `python -m unittest tests.batch_login.test_gui_controller -v` and confirm controller tests pass.

### Task 5: Extend the standalone enterprise CLI

**Files:**
- Modify: `scripts/kiro_enterprise_http_login.py`
- Test: `tests/batch_login/test_enterprise_cli.py`

- [ ] Add failing tests for per-entry precedence and a missing effective URL failure.
- [ ] Run `python -m unittest tests.batch_login.test_enterprise_cli -v` and confirm the new tests fail.
- [ ] Make `--start-url` optional and resolve the effective URL inside the per-entry loop.
- [ ] Re-run `python -m unittest tests.batch_login.test_enterprise_cli -v` and confirm all CLI tests pass.

### Task 6: Review, verify, commit, and merge locally

**Files:**
- Review all files changed in Tasks 1-5.

- [ ] Run every `tests/batch_login/test_*.py` module with `python -m unittest`.
- [ ] Run `python scripts/kiro_batch_login_gui.py --check`.
- [ ] Run `python -m compileall -q scripts` and `git diff --check`.
- [ ] Confirm `accounts.txt` was neither read nor staged, stage only task files, and create a local Chinese commit.
- [ ] Fast-forward merge into `master`, re-run the complete verification, remove the worktree, and do not push any remote.
