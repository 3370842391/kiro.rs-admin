# GUI Saved Settings Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add explicit save/clear configuration controls and automatically restore the last saved GUI settings, including a user-approved plaintext Admin Key.

**Architecture:** A new `gui_settings.py` module owns the versioned JSON schema, validation, default path, atomic writes, and deletion. `gui_app.py` only maps tkinter variables to/from `GuiSavedSettings`, loads before layout visibility is applied, and presents save/clear feedback without persisting account text or credential contents.

**Tech Stack:** Python 3.11+, tkinter/ttk, dataclasses, JSON, unittest.

---

### Task 1: Versioned atomic settings store

**Files:**
- Create: `scripts/batch_login/gui_settings.py`
- Create: `tests/batch_login/test_gui_settings.py`

- [ ] **Step 1: Write failing persistence and validation tests**

```python
class GuiSettingsStoreTests(unittest.TestCase):
    def test_round_trip_preserves_plaintext_admin_key_and_all_fields(self):
        settings = GuiSavedSettings(
            start_url="https://d-123.awsapps.com/start",
            rs_url="https://rs.example/admin",
            admin_key="plain-admin-key",
            use_ssh=True,
            ssh_host="host.example",
        )
        store.save(settings)
        self.assertEqual(settings, store.load())
        self.assertIn("plain-admin-key", path.read_text(encoding="utf-8"))

    def test_invalid_json_version_and_types_are_rejected(self):
        path.write_text("{broken", encoding="utf-8")
        with self.assertRaises(GuiSettingsError):
            store.load()

    def test_clear_is_idempotent(self):
        self.assertFalse(store.clear())
        store.save(GuiSavedSettings())
        self.assertTrue(store.clear())
        self.assertFalse(store.clear())
```

- [ ] **Step 2: Run tests and verify RED**

Run:

```powershell
python -m unittest tests.batch_login.test_gui_settings -v
```

Expected: failure because `batch_login.gui_settings` does not exist.

- [ ] **Step 3: Implement the settings schema and store**

```python
@dataclass(frozen=True, slots=True)
class GuiSavedSettings:
    version: int = 1
    input_template: str = "login = {account} / onetime password = {password}"
    output_template: str = "{account}----{password}"
    mode: str = "enterprise"
    start_url: str = ""
    password_vault_path: str = ""
    region: str = "us-east-1"
    headless: bool = False
    timeout_seconds: float = 180
    mfa_timeout_seconds: float = 300
    result_mode: str = "save_only"
    credential_path: str = ""
    checkpoint_path: str = ""
    resume: bool = False
    rs_url: str = ""
    admin_key: str = ""
    use_ssh: bool = False
    ssh_host: str = ""
    ssh_user: str = ""
    ssh_port: str = "22"
    identity_file: str = ""
    remote_host: str = "127.0.0.1"
    remote_port: str = "8990"
    local_port: str = ""
```

Implement `default_settings_path()`, strict `from_mapping()`, `GuiSettingsStore.load()`, atomic `save()` using a same-directory temporary file + `flush` + `os.fsync` + `os.replace`, and idempotent `clear()`.

- [ ] **Step 4: Run tests and verify GREEN**

Run:

```powershell
python -m unittest tests.batch_login.test_gui_settings -v
```

Expected: all settings-store tests pass.

### Task 2: GUI auto-load and controls

**Files:**
- Modify: `scripts/batch_login/gui_app.py`
- Modify: `tests/batch_login/test_gui_controller.py`

- [ ] **Step 1: Write failing GUI mapping tests**

```python
def test_saved_settings_are_applied_to_gui_variables(self):
    app = make_settings_app()
    app._apply_saved_settings(
        GuiSavedSettings(
            start_url="https://d-123.awsapps.com/start",
            rs_url="https://rs.example/admin",
            admin_key="plain-admin-key",
            result_mode="save_and_import",
            use_ssh=True,
        )
    )
    self.assertEqual("plain-admin-key", app.admin_key_var.get())
    self.assertEqual("https://rs.example/admin", app.rs_url_var.get())

def test_save_configuration_excludes_account_text(self):
    app = make_settings_app()
    app._save_configuration()
    saved = app.settings_store.saved
    self.assertFalse(hasattr(saved, "account_text"))
    self.assertIn("明文 Admin Key", app.status_var.get())
```

- [ ] **Step 2: Run tests and verify RED**

Run:

```powershell
python -m unittest tests.batch_login.test_gui_controller.BatchLoginAppTests -v
```

Expected: failures because GUI settings mapping and actions do not exist.

- [ ] **Step 3: Implement GUI integration**

In `BatchLoginApp.__init__`, create `GuiSettingsStore`, load before layout, catch `GuiSettingsError`, then apply values after `_build_variables()` and before `_build_layout()`.

Add:

```python
def _snapshot_settings(self) -> GuiSavedSettings:
    return GuiSavedSettings(
        input_template=self.input_template_var.get(),
        output_template=self.output_template_var.get(),
        mode=self.mode_var.get(),
        start_url=self.start_url_var.get(),
        password_vault_path=self.password_vault_path_var.get(),
        region=self.region_var.get(),
        headless=self.headless_var.get(),
        timeout_seconds=float(self.timeout_var.get()),
        mfa_timeout_seconds=float(self.mfa_timeout_var.get()),
        result_mode=self.result_mode_var.get(),
        credential_path=self.credential_path_var.get(),
        checkpoint_path=self.checkpoint_path_var.get(),
        resume=self.resume_var.get(),
        rs_url=self.rs_url_var.get(),
        admin_key=self.admin_key_var.get(),
        use_ssh=self.use_ssh_var.get(),
        ssh_host=self.ssh_host_var.get(),
        ssh_user=self.ssh_user_var.get(),
        ssh_port=self.ssh_port_var.get(),
        identity_file=self.identity_file_var.get(),
        remote_host=self.remote_host_var.get(),
        remote_port=self.remote_port_var.get(),
        local_port=self.local_port_var.get(),
    )

def _apply_saved_settings(self, settings: GuiSavedSettings) -> None:
    bindings = {
        "input_template": self.input_template_var,
        "output_template": self.output_template_var,
        "mode": self.mode_var,
        "start_url": self.start_url_var,
        "password_vault_path": self.password_vault_path_var,
        "region": self.region_var,
        "headless": self.headless_var,
        "timeout_seconds": self.timeout_var,
        "mfa_timeout_seconds": self.mfa_timeout_var,
        "result_mode": self.result_mode_var,
        "credential_path": self.credential_path_var,
        "checkpoint_path": self.checkpoint_path_var,
        "resume": self.resume_var,
        "rs_url": self.rs_url_var,
        "admin_key": self.admin_key_var,
        "use_ssh": self.use_ssh_var,
        "ssh_host": self.ssh_host_var,
        "ssh_user": self.ssh_user_var,
        "ssh_port": self.ssh_port_var,
        "identity_file": self.identity_file_var,
        "remote_host": self.remote_host_var,
        "remote_port": self.remote_port_var,
        "local_port": self.local_port_var,
    }
    for name, variable in bindings.items():
        variable.set(getattr(settings, name))

def _save_configuration(self) -> None:
    path = self.settings_store.save(self._snapshot_settings())
    message = f"配置已保存到 {path}（包含明文 Admin Key）"
    self.status_var.set(message)
    self._append_log(message)

def _clear_configuration(self) -> None:
    if not messagebox.askyesno("清除配置", "删除本地保存配置？当前表单不会清空。", parent=self.root):
        return
    self.settings_store.clear()
    self.status_var.set("配置已清除，下次启动使用默认值")
    self._append_log("配置已清除，下次启动使用默认值")
```

Place `保存配置` and `清除配置` on the left side of the bottom action row beside `导入已有 JSON`. Saving updates status/log with the file path and plaintext Admin Key warning. Clearing requires `messagebox.askyesno`, deletes only the file, and keeps current form values.

- [ ] **Step 4: Run GUI tests and verify GREEN**

Run:

```powershell
python -m unittest tests.batch_login.test_gui_controller tests.batch_login.test_gui_settings -v
python scripts/kiro_batch_login_gui.py --check
```

Expected: all tests and the GUI dependency check pass.

### Task 3: Full verification and local integration

**Files:**
- Review all files changed in Tasks 1-2.

- [ ] **Step 1: Run the complete batch-login suite**

```powershell
$modules = Get-ChildItem tests/batch_login/test_*.py | ForEach-Object { 'tests.batch_login.' + $_.BaseName }
python -m unittest $modules
```

Expected: zero failures.

- [ ] **Step 2: Run static checks**

```powershell
python scripts/kiro_batch_login_gui.py --check
python -m compileall -q scripts
git diff --check
```

Expected: all commands exit 0.

- [ ] **Step 3: Commit and merge locally**

Stage only the plan, settings module, GUI integration, and their tests. Commit with a Chinese subject, fast-forward merge into local `master`, rerun the full verification on `master`, remove the worktree, and do not push any remote.
