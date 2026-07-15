# Enterprise HTTP Login Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace browser-based enterprise login with a strict pure-HTTP AWS signin state machine and durable generated-password recovery.

**Architecture:** Separate cryptographic primitives, fingerprint generation, password vault, and AWS protocol orchestration. Existing credential storage and Microsoft browser login remain unchanged.

**Tech Stack:** Python 3.11+, curl_cffi, cryptography, SQLite, Windows DPAPI, unittest.

---

### Task 1: Password vault and password generation

**Files:**
- Create: `scripts/batch_login/password_vault.py`
- Test: `tests/batch_login/test_password_vault.py`

- [ ] Write failing tests for password complexity, `prepared` read-back verification, `confirmed`/`rejected`/`uncertain` transitions, and persistence failure blocking the caller.
- [ ] Implement `GeneratedPassword`, `PasswordAttempt`, `PasswordVault.prepare()`, `mark_confirmed()`, `mark_rejected()`, and `mark_uncertain()` using SQLite WAL and `synchronous=FULL`.
- [ ] Implement Windows DPAPI protection with a test-injected protector; reject plaintext fallback on Windows.
- [ ] Run `python -m unittest tests.batch_login.test_password_vault -v` and commit.

### Task 2: JWE and AWS fingerprint primitives

**Files:**
- Create: `scripts/batch_login/aws_jwe.py`
- Create: `scripts/batch_login/aws_fingerprint.py`
- Test: `tests/batch_login/test_aws_jwe.py`
- Test: `tests/batch_login/test_aws_fingerprint.py`
- Modify: `scripts/requirements-batch-login.txt`

- [ ] Write failing deterministic vectors for base64url, RSA-OAEP-256/A256GCM compact JWE metadata, CRC32, XXTEA, dynamic `app.js` config extraction, and encrypted fingerprint prefix.
- [ ] Implement JWE with `cryptography`, keeping passwords out of repr and exceptions.
- [ ] Implement browser identity, ordered fingerprint JSON, dynamic XXTEA config extraction, and encrypted fingerprint output.
- [ ] Add `cryptography` and `curl_cffi` dependencies; run both test modules and commit.

### Task 3: Enterprise AWS HTTP state machine

**Files:**
- Create: `scripts/batch_login/enterprise_http.py`
- Test: `tests/batch_login/test_enterprise_http.py`
- Modify: `scripts/batch_login/local_auth.py`

- [ ] Write failing fixture-transport tests for portal initialization, workflow initialization, username, password, no-reset redirect, reset flow, SSO token, device association, and final OIDC token.
- [ ] Implement strict response models and stage errors without logging sensitive values.
- [ ] Integrate password vault: prepare and verify before change; confirmed/rejected/uncertain transitions based on response certainty.
- [ ] Replace `LocalEnterpriseAuth` browser dependency with the HTTP protocol while preserving `CredentialRecord` output.
- [ ] Run enterprise protocol and local-auth tests and commit.

### Task 4: GUI/runtime integration

**Files:**
- Modify: `scripts/batch_login/gui_controller.py`
- Modify: `scripts/batch_login/gui_app.py`
- Modify: `scripts/batch_login/gui_runtime.py`
- Modify: `scripts/batch_login/worker_events.py`
- Test: `tests/batch_login/test_gui_controller.py`
- Test: `tests/batch_login/test_local_runner.py`

- [ ] Write failing tests proving enterprise mode does not initialize Playwright and supplies a password-vault path.
- [ ] Add the vault path to form/run settings; replace fixed-password UI with automatic-generation status.
- [ ] Lazily initialize Playwright only for Microsoft mode and wire the enterprise HTTP protocol.
- [ ] Run GUI/runtime tests and commit.

### Task 5: Verification and operating guide

**Files:**
- Modify: `README.md`
- Modify: `scripts/kiro_batch_login_gui.py`

- [ ] Document enterprise no-browser behavior, vault path, `prepared/confirmed/rejected/uncertain`, and first-account testing procedure.
- [ ] Update dependency check for `curl_cffi` and `cryptography`.
- [ ] Run all batch-login test modules individually, `python -m compileall -q scripts`, GUI `--check`, and `git diff --check`.
- [ ] Confirm `accounts.txt` remains untouched and commit documentation.
