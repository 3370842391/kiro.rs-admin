from __future__ import annotations

import argparse
import shutil
import sys


def dependency_errors() -> list[str]:
    errors: list[str] = []
    try:
        import tkinter  # noqa: F401
    except ImportError:
        errors.append("当前 Python 未安装 Tkinter")
    try:
        import httpx  # noqa: F401
        import playwright  # noqa: F401
        import cryptography  # noqa: F401
        import curl_cffi  # noqa: F401
    except ImportError:
        errors.append("请安装 scripts/requirements-batch-login.txt")
    if shutil.which("ssh") is None:
        errors.append("未找到系统 OpenSSH；仅保存 JSON 和直接 RS 模式仍可使用")
    return errors


def _is_fatal(message: str) -> bool:
    return "Tkinter" in message or "requirements" in message


def main(argv=None, *, checker=dependency_errors) -> int:
    parser = argparse.ArgumentParser(description="Kiro 登录账号桌面助手")
    parser.add_argument("--check", action="store_true", help="只检查运行依赖")
    args = parser.parse_args(argv)
    errors = checker()
    if args.check:
        for error in errors:
            print(error, file=sys.stderr)
        return 1 if any(_is_fatal(error) for error in errors) else 0
    fatal = [error for error in errors if _is_fatal(error)]
    if fatal:
        raise SystemExit("；".join(fatal))

    import tkinter as tk

    from batch_login.account_login_coordinator import AccountLoginCoordinator
    from batch_login.account_manager_service import AccountManagerService
    from batch_login.account_repository import (
        AccountRepository,
        default_account_db_path,
    )
    from batch_login.gui_settings import GuiSettingsStore
    from batch_login.login_manager_app import LoginManagerApp
    from batch_login.password_vault import WindowsDpapiProtector

    root = tk.Tk()
    repository = AccountRepository(
        default_account_db_path(),
        protector=WindowsDpapiProtector(),
    )
    service = AccountManagerService(repository)
    coordinator = AccountLoginCoordinator(repository, GuiSettingsStore())
    LoginManagerApp(root, service, coordinator)
    root.mainloop()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
