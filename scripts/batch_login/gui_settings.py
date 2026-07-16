from __future__ import annotations

import json
import os
from dataclasses import asdict, dataclass, fields
from pathlib import Path
from uuid import uuid4


SETTINGS_VERSION = 1


class GuiSettingsError(RuntimeError):
    """The saved GUI configuration could not be read or written safely."""


@dataclass(frozen=True, slots=True)
class GuiSavedSettings:
    version: int = SETTINGS_VERSION
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
    oidc_export_mode: str = "merged"
    oidc_export_directory: str = ""

    @classmethod
    def from_mapping(cls, value: object) -> GuiSavedSettings:
        if not isinstance(value, dict):
            raise GuiSettingsError("GUI 配置必须是 JSON 对象")
        version = value.get("version")
        if type(version) is not int or version != SETTINGS_VERSION:
            raise GuiSettingsError("GUI 配置版本不受支持")

        defaults = cls()
        string_fields = {
            "input_template",
            "output_template",
            "mode",
            "start_url",
            "password_vault_path",
            "region",
            "result_mode",
            "credential_path",
            "checkpoint_path",
            "rs_url",
            "admin_key",
            "ssh_host",
            "ssh_user",
            "ssh_port",
            "identity_file",
            "remote_host",
            "remote_port",
            "local_port",
            "oidc_export_mode",
            "oidc_export_directory",
        }
        bool_fields = {"headless", "resume", "use_ssh"}
        number_fields = {"timeout_seconds", "mfa_timeout_seconds"}
        cleaned: dict[str, object] = {"version": SETTINGS_VERSION}
        for item in fields(cls):
            name = item.name
            if name == "version":
                continue
            candidate = value.get(name, getattr(defaults, name))
            if name in string_fields:
                if not isinstance(candidate, str):
                    raise GuiSettingsError(f"GUI 配置字段 {name} 类型无效")
            elif name in bool_fields:
                if type(candidate) is not bool:
                    raise GuiSettingsError(f"GUI 配置字段 {name} 类型无效")
            elif name in number_fields:
                if isinstance(candidate, bool) or not isinstance(candidate, (int, float)):
                    raise GuiSettingsError(f"GUI 配置字段 {name} 类型无效")
                if float(candidate) <= 0:
                    raise GuiSettingsError(f"GUI 配置字段 {name} 必须大于 0")
                candidate = float(candidate)
            cleaned[name] = candidate

        if cleaned["mode"] not in {"enterprise", "microsoft"}:
            raise GuiSettingsError("GUI 配置登录模式无效")
        if cleaned["result_mode"] not in {"save_only", "save_and_import"}:
            raise GuiSettingsError("GUI 配置结果方式无效")
        if cleaned["oidc_export_mode"] not in {
            "merged",
            "per_account",
            "both",
        }:
            raise GuiSettingsError("GUI 配置 OIDC 导出方式无效")
        return cls(**cleaned)

    def as_json(self) -> dict[str, object]:
        return asdict(self)


def default_settings_path() -> Path:
    local_app_data = os.environ.get("LOCALAPPDATA")
    base = Path(local_app_data) if local_app_data else Path.home() / ".local" / "share"
    return base / "KiroBatchLogin" / "settings.json"


class GuiSettingsStore:
    def __init__(self, path: Path | None = None):
        self.path = Path(path) if path is not None else default_settings_path()

    def load(self) -> GuiSavedSettings | None:
        if not self.path.exists():
            return None
        try:
            raw = self.path.read_text(encoding="utf-8")
            value = json.loads(raw)
            return GuiSavedSettings.from_mapping(value)
        except GuiSettingsError:
            raise
        except (OSError, UnicodeError, json.JSONDecodeError) as error:
            raise GuiSettingsError("无法读取 GUI 配置") from error

    def save(self, settings: GuiSavedSettings) -> Path:
        if not isinstance(settings, GuiSavedSettings):
            raise GuiSettingsError("GUI 配置对象无效")
        self.path.parent.mkdir(parents=True, exist_ok=True)
        temporary = self.path.with_name(self.path.name + ".tmp-" + uuid4().hex)
        try:
            with temporary.open("w", encoding="utf-8", newline="\n") as handle:
                json.dump(
                    settings.as_json(),
                    handle,
                    ensure_ascii=False,
                    indent=2,
                )
                handle.write("\n")
                handle.flush()
                os.fsync(handle.fileno())
            os.replace(temporary, self.path)
        except OSError as error:
            raise GuiSettingsError("无法保存 GUI 配置") from error
        finally:
            try:
                temporary.unlink(missing_ok=True)
            except OSError:
                pass
        return self.path

    def clear(self) -> bool:
        try:
            if not self.path.exists():
                return False
            self.path.unlink()
            return True
        except OSError as error:
            raise GuiSettingsError("无法清除 GUI 配置") from error
