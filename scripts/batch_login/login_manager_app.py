from __future__ import annotations

import asyncio
import queue
import threading
import tkinter as tk
from tkinter import messagebox, simpledialog, ttk

from .account_login_coordinator import LoginProgressEvent
from .account_manager_service import AccountManagerServiceError
from .account_repository import CredentialStatus, LifecycleStatus, LoginStatus
from .models import LoginMode
from .redaction import redact_text
from .worker_events import WorkerEvent

# 独立「登录账号」窗口:多选右键登录(一次拿 json+apikey+最新密码)+ 手动刷新剩余额度。
# 与账号管理器共用同一账号库(AccountManagerService / AccountLoginCoordinator),
# 但 UI 独立;绝不改动 account_manager_app.py。


def _mask_password(account) -> str:
    return "••••••" if account.has_current_password else "未设置"


def format_login_event(event: WorkerEvent) -> str:
    payload = event.payload
    kind = event.kind
    if kind == "api_key_phase":
        if payload.get("phase") == "login":
            return f"第 1 步：登录取 JSON（{int(payload.get('count', 0))} 个缺凭据账号）"
        return f"第 2 步：提取 API Key（{int(payload.get('count', 0))} 个账号）"
    if kind == "api_key_created":
        return f"API Key 已创建：{payload.get('accountMasked', '***')}"
    if kind == "api_key_reused":
        return f"API Key 已存在：{payload.get('accountMasked', '***')}"
    if kind == "api_key_refreshed":
        return f"已刷新 token：{payload.get('accountMasked', '***')}"
    if kind == "api_key_failed":
        return (
            f"API Key 失败：{payload.get('accountMasked', '***')}"
            f"（{redact_text(str(payload.get('code', '')))}）"
        )
    if kind == "quota_updated":
        return f"额度已更新：{payload.get('accountMasked', '***')} → {payload.get('display', '')}"
    if kind == "quota_failed":
        return (
            f"额度查询失败：{payload.get('accountMasked', '***')}"
            f"（{redact_text(str(payload.get('code', '')))}）"
        )
    if kind == "security_warning":
        return redact_text(str(payload.get("message") or "提示"))
    if kind == "account_finished":
        return f"登录结果：{payload.get('status', 'unknown')}"
    if kind == "batch_started":
        return f"批次开始，共 {int(payload.get('total', 0))} 个账号"
    return f"运行事件：{redact_text(kind)}"


def quota_cell_text(quota: dict | None) -> str:
    if not quota:
        return "未查询"
    remaining = quota.get("remaining")
    total = quota.get("total")

    def fmt(value) -> str:
        if value is None:
            return "?"
        return str(int(value)) if float(value).is_integer() else f"{value:.2f}"

    return f"剩余 {fmt(remaining)} / 总 {fmt(total)}"


class LoginManagerApp:
    TABLE_COLUMNS = (
        "account",
        "password",
        "login_status",
        "json_status",
        "api_key_status",
        "quota",
        "subscription",
        "updated_at",
    )
    TOOLBAR_LABELS = ("登录账号", "刷新额度", "全选", "反选")
    CONTEXT_MENU_LABELS = (
        "登录账号",
        "刷新额度",
        "复制 API Key",
        "复制 JSON",
        "查看密码",
    )

    def __init__(self, root, service, coordinator):
        self.root = root
        self.service = service
        self.coordinator = coordinator
        self.busy = False
        self.status_var = tk.StringVar(value="准备就绪")
        self.progress_var = tk.DoubleVar(value=0)
        self.progress_text_var = tk.StringVar(value="进度：0/0")
        self.progress_prefix = "进度"
        self.event_queue: queue.Queue = queue.Queue()
        self.live_status_by_id: dict[int, str] = {}
        self.visible_ids: list[int] = []
        self.quota_by_id: dict[int, dict] = {}
        self._refreshing = False
        self.root.title("Kiro 登录账号")
        self.root.geometry("1280x760")
        self.root.minsize(1000, 600)
        self._build()
        self.refresh()
        self.root.after(80, self._poll_events)

    def _build(self) -> None:
        outer = ttk.Frame(self.root, padding=12)
        outer.pack(fill="both", expand=True)
        ttk.Label(
            outer, text="Kiro 登录账号", font=("Microsoft YaHei UI", 16, "bold")
        ).pack(anchor="w")
        ttk.Label(
            outer,
            text="多选后右键「登录账号」一次拿 JSON+API Key+最新密码；额度需手动点「刷新额度」。",
            foreground="#475569",
        ).pack(anchor="w", pady=(2, 8))

        toolbar = ttk.Frame(outer)
        toolbar.pack(fill="x", pady=(0, 8))
        commands = (
            self.start_login,
            self.start_refresh_quota,
            self.select_all,
            self.invert_selection,
        )
        for text, command in zip(self.TOOLBAR_LABELS, commands, strict=True):
            ttk.Button(toolbar, text=text, command=command).pack(side="left", padx=2)

        table_frame = ttk.Frame(outer)
        table_frame.pack(fill="both", expand=True)
        self.tree = ttk.Treeview(
            table_frame, columns=self.TABLE_COLUMNS, show="headings", selectmode="extended"
        )
        headings = {
            "account": "账号", "password": "当前密码", "login_status": "登录状态",
            "json_status": "JSON", "api_key_status": "API Key", "quota": "剩余额度",
            "subscription": "订阅", "updated_at": "更新时间",
        }
        widths = {
            "account": 180, "password": 90, "login_status": 90, "json_status": 80,
            "api_key_status": 90, "quota": 160, "subscription": 120, "updated_at": 165,
        }
        for column in self.TABLE_COLUMNS:
            self.tree.heading(column, text=headings[column])
            self.tree.column(column, width=widths[column], anchor="w")
        vsb = ttk.Scrollbar(table_frame, orient="vertical", command=self.tree.yview)
        self.tree.configure(yscrollcommand=vsb.set)
        self.tree.grid(row=0, column=0, sticky="nsew")
        vsb.grid(row=0, column=1, sticky="ns")
        table_frame.rowconfigure(0, weight=1)
        table_frame.columnconfigure(0, weight=1)
        self.tree.bind("<Button-3>", self._context_menu)

        self.context_menu = tk.Menu(self.root, tearoff=False)
        menu_commands = (
            self.start_login,
            self.start_refresh_quota,
            self.copy_api_keys,
            self.copy_json,
            self.view_password,
        )
        for index, (label, command) in enumerate(
            zip(self.CONTEXT_MENU_LABELS, menu_commands, strict=True)
        ):
            if index == 2:
                self.context_menu.add_separator()
            self.context_menu.add_command(label=label, command=command)

        panel = ttk.LabelFrame(outer, text="进度", padding=8)
        panel.pack(fill="x", pady=(8, 0))
        row = ttk.Frame(panel)
        row.pack(fill="x")
        ttk.Progressbar(
            row, variable=self.progress_var, maximum=100, mode="determinate"
        ).pack(side="left", fill="x", expand=True)
        ttk.Label(row, textvariable=self.progress_text_var, width=18, anchor="e").pack(
            side="right", padx=(8, 0)
        )
        self.log_text = tk.Text(panel, height=6, wrap="word", state="disabled", font=("Consolas", 9))
        self.log_text.pack(fill="both", expand=True, pady=(6, 0))

        footer = ttk.Frame(outer)
        footer.pack(fill="x", pady=(6, 0))
        ttk.Label(footer, textvariable=self.status_var, foreground="#475569").pack(side="right")

    def refresh(self) -> None:
        try:
            accounts = self.service.list_accounts(status="managed")
        except AccountManagerServiceError as error:
            messagebox.showerror("操作失败", redact_text(str(error)), parent=self.root)
            return
        self.visible_ids = [item.id for item in accounts]
        try:
            self.quota_by_id = self.service.repository.load_quotas(self.visible_ids)
        except Exception:  # noqa: BLE001 - 额度读失败不阻断列表
            self.quota_by_id = {}
        self._refreshing = True
        try:
            self.tree.delete(*self.tree.get_children())
            for item in accounts:
                quota = self.quota_by_id.get(item.id)
                self.tree.insert(
                    "", "end", iid=str(item.id),
                    values=(
                        item.account,
                        _mask_password(item),
                        item.login_status.value,
                        self.live_status_by_id.get(item.id) or self._json_text(item),
                        "有" if self._has_api_key(item.id) else "无",
                        quota_cell_text(quota),
                        (quota or {}).get("subscription") or "",
                        item.updated_at,
                    ),
                )
        finally:
            self._refreshing = False
        self.status_var.set(f"显示 {len(accounts)} 个账号")

    def _json_text(self, account) -> str:
        if account.credential_status is CredentialStatus.VALID:
            return "有"
        if account.login_status is LoginStatus.FAILED:
            return "失败"
        return "无"

    def _has_api_key(self, account_id: int) -> bool:
        try:
            credential = self.service.repository.load_credential(account_id)
        except Exception:  # noqa: BLE001
            return False
        return bool(credential and (credential.kiro_api_key or "").strip())

    def _selected_ids(self) -> list[int]:
        return sorted(int(item) for item in self.tree.selection())

    def select_all(self) -> None:
        for account_id in self.visible_ids:
            self.tree.selection_add(str(account_id))

    def invert_selection(self) -> None:
        current = set(self.tree.selection())
        self.tree.selection_set(
            [str(i) for i in self.visible_ids if str(i) not in current]
        )

    def _context_menu(self, event):
        row = self.tree.identify_row(event.y)
        if not row:
            return "break"
        if row not in self.tree.selection():
            self.tree.selection_set(row)
        try:
            self.context_menu.tk_popup(event.x_root, event.y_root)
        finally:
            self.context_menu.grab_release()
        return "break"

    def _copy(self, text: str) -> None:
        self.root.clipboard_clear()
        self.root.clipboard_append(text)

    def copy_api_keys(self) -> None:
        ids = self._selected_ids()
        if not ids:
            messagebox.showinfo("复制 API Key", "请先选择账号", parent=self.root)
            return
        keys = []
        for account_id in ids:
            try:
                credential = self.service.repository.load_credential(account_id)
            except Exception as error:  # noqa: BLE001
                messagebox.showerror("操作失败", redact_text(str(error)), parent=self.root)
                return
            key = (credential.kiro_api_key or "").strip() if credential else ""
            if key:
                keys.append(key)
        if not keys:
            messagebox.showinfo("复制 API Key", "所选账号还没有 API Key", parent=self.root)
            return
        self._copy("\n".join(keys))
        self.status_var.set(f"已复制 {len(keys)} 个 API Key")

    def copy_json(self) -> None:
        import json

        ids = self._selected_ids()
        if not ids:
            messagebox.showinfo("复制 JSON", "请先选择账号", parent=self.root)
            return
        records = []
        for account_id in ids:
            try:
                credential = self.service.repository.load_credential(account_id)
            except Exception as error:  # noqa: BLE001
                messagebox.showerror("操作失败", redact_text(str(error)), parent=self.root)
                return
            if credential is not None:
                records.append(credential.as_add_request())
        if not records:
            messagebox.showinfo("复制 JSON", "所选账号还没有登录凭据", parent=self.root)
            return
        payload = records[0] if len(records) == 1 else records
        self._copy(json.dumps(payload, ensure_ascii=False, indent=2))
        self.status_var.set(f"已复制 {len(records)} 个账号的 JSON")

    def view_password(self) -> None:
        ids = self._selected_ids()
        if len(ids) != 1:
            messagebox.showinfo("查看密码", "请选择一个账号", parent=self.root)
            return
        try:
            account = self.service.repository.get(ids[0], include_secrets=True)
        except Exception as error:  # noqa: BLE001
            messagebox.showerror("操作失败", redact_text(str(error)), parent=self.root)
            return
        current = account.current_password or account.initial_password or ""
        if not current:
            messagebox.showinfo("查看密码", "该账号暂无可用密码", parent=self.root)
            return
        self._copy(current)
        self.status_var.set(f"已复制 {account.account} 的当前密码")

    def _append_log(self, message: str) -> None:
        self.log_text.configure(state="normal")
        self.log_text.insert("end", redact_text(str(message)) + "\n")
        self.log_text.see("end")
        self.log_text.configure(state="disabled")

    def _poll_events(self) -> None:
        while True:
            try:
                event = self.event_queue.get_nowait()
            except queue.Empty:
                break
            if isinstance(event, LoginProgressEvent):
                self._apply_progress(event)
            elif isinstance(event, WorkerEvent):
                self._apply_worker_event(event)
            elif isinstance(event, tuple) and event[:1] == ("finished",):
                self._finished(report=event[1], error=event[2])
        try:
            self.root.after(80, self._poll_events)
        except tk.TclError:
            pass

    def _apply_progress(self, event: LoginProgressEvent) -> None:
        labels = {"waiting": "等待中", "running": "处理中", "reused": "复用成功", "success": "成功"}
        status = (
            f"失败：{redact_text(str(event.code or '未知'))}"
            if event.status == "failed"
            else labels.get(event.status, redact_text(event.status))
        )
        self.live_status_by_id[event.account_id] = status
        if self.tree.exists(str(event.account_id)):
            self.tree.set(str(event.account_id), "login_status", status)
        self.progress_var.set(event.completed * 100 / max(event.total, 1))
        self.progress_text_var.set(f"{self.progress_prefix}：{event.completed}/{event.total}")

    def _apply_worker_event(self, event: WorkerEvent) -> None:
        if event.kind == "api_key_phase":
            count = int(event.payload.get("count", 0))
            self.progress_prefix = (
                "登录取 JSON 进度" if event.payload.get("phase") == "login" else "API Key 进度"
            )
            self.progress_var.set(0)
            self.progress_text_var.set(f"{self.progress_prefix}：0/{count}")
        self._append_log(format_login_event(event))

    def start_login(self) -> None:
        ids = self._guard_action("登录账号")
        if ids is None:
            return
        if not messagebox.askyesno(
            "登录账号",
            f"将为 {len(ids)} 个账号登录并获取 JSON + API Key + 最新密码。\n"
            "缺 JSON 的会自动登录，过期 token 会先刷新。是否继续？",
            parent=self.root,
        ):
            return
        self.busy = True
        self.progress_prefix = "进度"
        self.progress_var.set(0)
        self.progress_text_var.set(f"进度：0/{len(ids)}")
        self._append_log(f"开始登录 {len(ids)} 个账号")
        self.status_var.set(f"正在登录 {len(ids)} 个账号…")

        def worker():
            try:
                report = asyncio.run(
                    self.coordinator.login_and_extract_api_keys(
                        ids,
                        progress=self.event_queue.put,
                        event_sink=self.event_queue.put,
                    )
                )
            except Exception as error:  # noqa: BLE001
                self.event_queue.put(("finished", None, error))
                return
            self.event_queue.put(("finished", ("login", report), None))

        threading.Thread(target=worker, name="kiro-login-manager-login", daemon=False).start()

    def start_refresh_quota(self) -> None:
        ids = self._guard_action("刷新额度")
        if ids is None:
            return
        self.busy = True
        self.progress_prefix = "额度进度"
        self.progress_var.set(0)
        self.progress_text_var.set(f"额度进度：0/{len(ids)}")
        self._append_log(f"开始刷新 {len(ids)} 个账号的额度")
        self.status_var.set(f"正在刷新 {len(ids)} 个账号额度…")

        def worker():
            try:
                report = asyncio.run(
                    self.coordinator.refresh_quota(
                        ids,
                        progress=self.event_queue.put,
                        event_sink=self.event_queue.put,
                    )
                )
            except Exception as error:  # noqa: BLE001
                self.event_queue.put(("finished", None, error))
                return
            self.event_queue.put(("finished", ("quota", report), None))

        threading.Thread(target=worker, name="kiro-login-manager-quota", daemon=False).start()

    def _guard_action(self, title: str) -> list[int] | None:
        if self.busy:
            messagebox.showinfo(title, "已有任务正在运行", parent=self.root)
            return None
        ids = self._selected_ids()
        if not ids:
            messagebox.showinfo(title, "请先选择账号", parent=self.root)
            return None
        if self.coordinator is None:
            messagebox.showerror(title, "登录协调器未初始化", parent=self.root)
            return None
        return ids

    def _finished(self, *, report=None, error=None) -> None:
        self.busy = False
        if error is not None:
            self._append_log("任务异常结束")
            messagebox.showerror("操作失败", redact_text(str(error)), parent=self.root)
            self.refresh()
            self.status_var.set("任务失败")
            return
        kind, data = report
        self.live_status_by_id.clear()
        self.refresh()
        if kind == "login":
            summary = (
                f"完成：创建 {data.created}，刷新 {data.refreshed}，"
                f"复用 {data.reused}，失败 {data.failed}，跳过 {data.skipped}"
            )
        else:
            summary = (
                f"额度完成：更新 {data.updated}，刷新 token {data.refreshed}，"
                f"失败 {data.failed}，跳过 {data.skipped}"
            )
        self._append_log(summary)
        self.status_var.set(summary)
