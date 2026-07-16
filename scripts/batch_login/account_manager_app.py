from __future__ import annotations

import os
import queue
import shutil
import asyncio
import threading
import tkinter as tk
from pathlib import Path
from tkinter import filedialog, messagebox, simpledialog, ttk
from uuid import uuid4

from .account_login_coordinator import LoginProgressEvent
from .account_manager_service import (
    AccountManagerService,
    AccountManagerServiceError,
    ImportPreview,
)
from .account_repository import CredentialStatus, LifecycleStatus, LoginStatus
from .gui_app import BatchLoginApp
from .gui_runtime import build_default_controller
from .models import LoginMode
from .redaction import redact_text
from .worker_events import WorkerEvent


def atomic_write_text(path: Path, text: str) -> None:
    path = Path(path)
    temporary = path.with_name(f".{path.name}.{uuid4().hex}.tmp")
    try:
        path.parent.mkdir(parents=True, exist_ok=True)
        with temporary.open("x", encoding="utf-8", newline="\n") as handle:
            handle.write(text)
            if not text.endswith("\n"):
                handle.write("\n")
            handle.flush()
            os.fsync(handle.fileno())
        os.replace(temporary, path)
    except Exception:
        temporary.unlink(missing_ok=True)
        raise


def clear_secret_vars(*variables) -> None:
    for variable in variables:
        variable.set("")


def password_cell_text(account) -> str:
    return "••••••" if account.has_current_password else "未设置"


def json_status_text(account, live_status: str | None = None) -> str:
    if live_status:
        return live_status
    if account.credential_status is CredentialStatus.VALID:
        return "成功"
    if account.login_status is LoginStatus.RUNNING:
        return "处理中"
    if account.login_status is LoginStatus.FAILED:
        code = redact_text(str(account.last_error_code or "未知错误"))
        return f"失败：{code}"
    return "未获取"


def format_worker_event(event: WorkerEvent) -> str:
    payload = event.payload
    if event.kind == "browser_stage":
        labels = {
            "oidc_register": "注册企业 OIDC 客户端",
            "portal_init": "初始化企业登录门户",
            "device_authorization": "确认设备授权码",
            "workflow_init": "初始化 AWS 登录工作流",
            "username": "提交用户名",
            "password": "提交一次性密码",
            "password_reset": "设置新密码",
            "sso_token": "获取企业 SSO Token",
            "complete": "登录流程完成",
        }
        stage = str(payload.get("stage") or "")
        return labels.get(stage, f"登录阶段：{redact_text(stage)}")
    if event.kind == "security_warning":
        return redact_text(str(payload.get("message") or "安全提示"))
    if event.kind == "account_started":
        return (
            f"开始登录 {payload.get('accountMasked', '***')} "
            f"({payload.get('index', 0)}/{payload.get('total', 0)})"
        )
    if event.kind == "account_finished":
        parts = [f"登录结果：{payload.get('status', 'unknown')}"]
        if payload.get("code"):
            parts.append(f"代码={redact_text(str(payload['code']))}")
        if payload.get("stage"):
            parts.append(f"阶段={redact_text(str(payload['stage']))}")
        return "，".join(parts)
    if event.kind == "batch_started":
        return f"登录批次开始，共 {int(payload.get('total', 0))} 个账号"
    if event.kind in {"batch_finished", "batch_cancelled"}:
        return (
            f"登录批次{'完成' if event.kind == 'batch_finished' else '取消'}："
            f"成功 {int(payload.get('succeeded', 0))}，"
            f"失败 {int(payload.get('failed', 0))}"
        )
    if event.kind == "import_event":
        return (
            f"RS 导入：状态={redact_text(str(payload.get('status', 'unknown')))}"
        )
    return f"运行事件：{redact_text(event.kind)}"


class AccountManagerApp:
    TABLE_COLUMNS = (
        "account",
        "password",
        "start_url",
        "login_status",
        "credential_status",
        "json_status",
        "lifecycle_status",
        "note",
        "updated_at",
    )
    DEFAULT_EXPORT_TEMPLATE = "{account}----{password}----{start_url}"
    PRIMARY_ACTION_LABEL = "一键登录导出 JSON"
    INPUT_TEMPLATE = "login = {account} / onetime password = {password}"
    INPUT_TEMPLATE_PRESETS = (
        INPUT_TEMPLATE,
        "{account}----{password}",
        "{account}|{password}|{start_url}",
    )
    PRIMARY_TOOLBAR_LABELS = (
        "粘贴并识别",
        "指定 URL",
        PRIMARY_ACTION_LABEL,
        "自动登录设置",
    )
    SELECTION_TOOLBAR_LABELS = (
        "全选",
        "反选",
        "取消选择",
        "查看密码",
        "更新密码",
        "导出账号密码",
        "标记已售",
        "恢复管理",
    )
    CONTEXT_MENU_LABELS = (
        "一键获取 JSON",
        "复制账号",
        "复制账号信息",
        "复制 Start URL",
        "查看密码",
        "更新密码",
        "导出账号密码",
        "标记已售",
        "恢复管理",
    )
    STATUS_VALUES = {
        "管理中": "managed",
        "全部": "all",
        "待登录": "pending",
        "可导出": "exportable",
        "登录失败": "failed",
        "已售出": "sold",
    }

    def __init__(self, root: tk.Tk, service: AccountManagerService, coordinator=None):
        self.root = root
        self.service = service
        self.coordinator = coordinator
        self.login_running = False
        self.query_var = tk.StringVar()
        self.filter_var = tk.StringVar(value="管理中")
        self.status_var = tk.StringVar(value="准备就绪")
        self.selected_count_var = tk.StringVar(value="已选择 0 个账号")
        self.login_progress_var = tk.DoubleVar(value=0)
        self.login_progress_text_var = tk.StringVar(value="JSON 进度：0/0")
        self.login_event_queue: queue.Queue = queue.Queue()
        self.json_status_by_id: dict[int, str] = {}
        self.active_login_ids: list[int] = []
        self.visible_ids: list[int] = []
        self._refreshing_tree = False
        self.root.title("Kiro 账号管理器")
        self.root.geometry("1420x820")
        self.root.minsize(1080, 640)
        self._build()
        self.refresh()
        self.root.after(80, self._poll_login_events)

    def _build(self) -> None:
        outer = ttk.Frame(self.root, padding=12)
        outer.pack(fill="both", expand=True)
        ttk.Label(outer, text="Kiro 账号管理器", font=("Microsoft YaHei UI", 16, "bold")).pack(anchor="w")
        ttk.Label(
            outer,
            text=(
                "单击单选；Ctrl+单击追加/取消；Shift+单击连续选择；"
                "右键对全部高亮账号操作"
            ),
            foreground="#475569",
        ).pack(anchor="w", pady=(2, 8))
        toolbar = ttk.Frame(outer)
        toolbar.pack(fill="x", pady=(0, 4))
        ttk.Entry(toolbar, textvariable=self.query_var, width=34).pack(side="left")
        self.query_var.trace_add("write", lambda *_args: self.refresh())
        status = ttk.Combobox(toolbar, state="readonly", textvariable=self.filter_var, values=list(self.STATUS_VALUES), width=10)
        status.pack(side="left", padx=6)
        status.bind("<<ComboboxSelected>>", lambda _event: self.refresh())
        primary_commands = (
            self.open_import_dialog,
            self.open_start_url_manager,
            self.start_login_export,
            self.open_legacy_login,
        )
        for text, command in zip(
            self.PRIMARY_TOOLBAR_LABELS, primary_commands, strict=True
        ):
            ttk.Button(toolbar, text=text, command=command).pack(side="left", padx=2)
        selection_toolbar = ttk.Frame(outer)
        selection_toolbar.pack(fill="x", pady=(0, 8))
        ttk.Label(selection_toolbar, text="批量操作：").pack(side="left")
        selection_commands = (
            self.select_all,
            self.invert_selection,
            self.clear_selection,
            self.open_password_viewer,
            self.update_password,
            self.open_export_dialog,
            self.mark_sold,
            self.restore_managed,
        )
        for text, command in zip(
            self.SELECTION_TOOLBAR_LABELS, selection_commands, strict=True
        ):
            ttk.Button(selection_toolbar, text=text, command=command).pack(
                side="left", padx=2
            )

        table_frame = ttk.Frame(outer)
        table_frame.pack(fill="both", expand=True)
        self.tree = ttk.Treeview(
            table_frame,
            columns=self.TABLE_COLUMNS,
            show="headings",
            selectmode="extended",
        )
        headings = {
            "account": "账号", "password": "当前密码",
            "start_url": "Start URL", "login_status": "登录状态",
            "credential_status": "凭据状态", "json_status": "JSON 状态",
            "lifecycle_status": "销售状态",
            "note": "备注", "updated_at": "更新时间",
        }
        widths = {"account": 190, "password": 110, "start_url": 280, "login_status": 90, "credential_status": 90, "json_status": 150, "lifecycle_status": 90, "note": 180, "updated_at": 165}
        for column in self.TABLE_COLUMNS:
            self.tree.heading(column, text=headings[column])
            self.tree.column(column, width=widths[column], anchor="w")
        vertical = ttk.Scrollbar(
            table_frame, orient="vertical", command=self.tree.yview
        )
        horizontal = ttk.Scrollbar(
            table_frame, orient="horizontal", command=self.tree.xview
        )
        self.tree.configure(
            yscrollcommand=vertical.set, xscrollcommand=horizontal.set
        )
        self.tree.grid(row=0, column=0, sticky="nsew")
        vertical.grid(row=0, column=1, sticky="ns")
        horizontal.grid(row=1, column=0, sticky="ew")
        table_frame.rowconfigure(0, weight=1)
        table_frame.columnconfigure(0, weight=1)
        self.tree.bind("<Button-3>", self._tree_context_menu)
        self.tree.bind("<<TreeviewSelect>>", self._tree_selection, add="+")
        self.context_menu = tk.Menu(self.root, tearoff=False)
        context_commands = (
            self.start_login_export,
            self.copy_selected_accounts,
            self.copy_selected_account_info,
            self.copy_selected_start_urls,
            self.open_password_viewer,
            self.update_password,
            self.open_export_dialog,
            self.mark_sold,
            self.restore_managed,
        )
        for index, (label, command) in enumerate(
            zip(self.CONTEXT_MENU_LABELS, context_commands, strict=True)
        ):
            if index in {4, 7}:
                self.context_menu.add_separator()
            self.context_menu.add_command(label=label, command=command)

        login_panel = ttk.LabelFrame(outer, text="JSON 获取进度", padding=8)
        login_panel.pack(fill="x", pady=(8, 0))
        progress_row = ttk.Frame(login_panel)
        progress_row.pack(fill="x")
        ttk.Progressbar(
            progress_row,
            variable=self.login_progress_var,
            maximum=100,
            mode="determinate",
        ).pack(side="left", fill="x", expand=True)
        ttk.Label(
            progress_row,
            textvariable=self.login_progress_text_var,
            width=18,
            anchor="e",
        ).pack(side="right", padx=(8, 0))
        log_frame = ttk.Frame(login_panel)
        log_frame.pack(fill="x", pady=(6, 0))
        self.login_log_text = tk.Text(
            log_frame,
            height=6,
            wrap="word",
            state="disabled",
            font=("Consolas", 9),
        )
        log_scroll = ttk.Scrollbar(
            log_frame, orient="vertical", command=self.login_log_text.yview
        )
        self.login_log_text.configure(yscrollcommand=log_scroll.set)
        self.login_log_text.pack(side="left", fill="both", expand=True)
        log_scroll.pack(side="right", fill="y")

        footer = ttk.Frame(outer)
        footer.pack(fill="x", pady=(6, 0))
        ttk.Label(footer, textvariable=self.selected_count_var).pack(side="left")
        ttk.Label(footer, textvariable=self.status_var, foreground="#475569").pack(side="right")

    def refresh(self) -> None:
        try:
            accounts = self.service.list_accounts(
                query=self.query_var.get(),
                status=self.STATUS_VALUES.get(self.filter_var.get(), "managed"),
            )
        except AccountManagerServiceError as error:
            self._error(error)
            return
        self.visible_ids = [item.id for item in accounts]
        selected = self.service.selected_ids
        self._refreshing_tree = True
        try:
            self.tree.delete(*self.tree.get_children())
            for item in accounts:
                self.tree.insert("", "end", iid=str(item.id), values=(
                    item.account, password_cell_text(item), item.start_url or "",
                    item.login_status.value, item.credential_status.value,
                    json_status_text(
                        item, self.json_status_by_id.get(item.id)
                    ),
                    "已售出" if item.lifecycle_status is LifecycleStatus.SOLD else "管理中",
                    item.note, item.updated_at,
                ))
                if item.id in selected:
                    self.tree.selection_add(str(item.id))
        finally:
            self._refreshing_tree = False
        self._update_selected_count()
        self.status_var.set(f"显示 {len(accounts)} 个账号")

    def _tree_selection(self, _event=None) -> None:
        if self._refreshing_tree:
            return
        self.service.set_selected(int(item) for item in self.tree.selection())
        self._update_selected_count()

    def _tree_context_menu(self, event):
        row = self.tree.identify_row(event.y)
        if not row:
            return "break"
        highlighted = {int(item) for item in self.tree.selection()}
        if int(row) not in highlighted:
            self.tree.selection_set(row)
            highlighted = {int(row)}
        self.service.set_selected(highlighted)
        self._update_selected_count()
        try:
            self.context_menu.tk_popup(event.x_root, event.y_root)
        finally:
            self.context_menu.grab_release()
        return "break"

    def _selected_action_ids(self) -> list[int]:
        return sorted(self.service.selected_ids)

    def _update_selected_count(self) -> None:
        self.selected_count_var.set(f"已选择 {len(self.service.selected_ids)} 个账号")

    def select_all(self) -> None:
        self.service.set_selected(self.visible_ids)
        self.refresh()

    def invert_selection(self) -> None:
        self.service.invert_visible(self.visible_ids)
        self.refresh()

    def clear_selection(self) -> None:
        self.service.clear_selected()
        self.refresh()

    def open_start_url_manager(self) -> None:
        try:
            catalog = self.service.load_start_url_catalog()
        except AccountManagerServiceError as error:
            self._error(error)
            return
        window = tk.Toplevel(self.root)
        window.title("指定企业 Start URL")
        window.geometry("760x360")
        window.transient(self.root)
        entry_var = tk.StringVar(value=catalog.default_url)
        default_var = tk.StringVar()
        ttk.Label(
            window,
            text="保存常用企业登录 URL，并指定粘贴账号时默认使用的地址。",
            foreground="#475569",
        ).pack(anchor="w", padx=12, pady=(12, 6))
        editor = ttk.Frame(window)
        editor.pack(fill="x", padx=12)
        ttk.Entry(editor, textvariable=entry_var).pack(
            side="left", fill="x", expand=True
        )
        saved_list = tk.Listbox(window, height=9, activestyle="dotbox")
        saved_list.pack(fill="both", expand=True, padx=12, pady=8)

        def refresh_catalog(updated=None) -> None:
            nonlocal catalog
            if updated is not None:
                catalog = updated
            saved_list.delete(0, "end")
            for item in catalog.urls:
                prefix = "★ " if item == catalog.default_url else "   "
                saved_list.insert("end", prefix + item)
            default_var.set(
                f"当前默认：{catalog.default_url or '未指定'}"
            )

        def selected_url() -> str:
            selected = saved_list.curselection()
            if not selected:
                return entry_var.get().strip()
            return catalog.urls[int(selected[0])]

        def save_url(*, make_default: bool) -> None:
            try:
                updated = self.service.save_start_url(
                    entry_var.get(), make_default=make_default
                )
            except AccountManagerServiceError as error:
                messagebox.showerror("URL 保存失败", str(error), parent=window)
                return
            refresh_catalog(updated)
            entry_var.set(updated.default_url if make_default else entry_var.get())

        def set_default(_event=None) -> None:
            value = selected_url()
            try:
                updated = self.service.set_default_start_url(value)
            except AccountManagerServiceError as error:
                messagebox.showerror("URL 设置失败", str(error), parent=window)
                return
            entry_var.set(updated.default_url)
            refresh_catalog(updated)

        def delete_selected() -> None:
            value = selected_url()
            if not value:
                messagebox.showinfo("删除 URL", "请先选择一个 URL", parent=window)
                return
            try:
                updated = self.service.delete_start_url(value)
            except AccountManagerServiceError as error:
                messagebox.showerror("URL 删除失败", str(error), parent=window)
                return
            entry_var.set(updated.default_url)
            refresh_catalog(updated)

        ttk.Button(
            editor,
            text="仅保存",
            command=lambda: save_url(make_default=False),
        ).pack(side="left", padx=(8, 0))
        ttk.Button(
            editor,
            text="保存并设为默认",
            command=lambda: save_url(make_default=True),
        ).pack(side="left", padx=(6, 0))
        saved_list.bind("<Double-1>", set_default)
        footer = ttk.Frame(window)
        footer.pack(fill="x", padx=12, pady=(0, 12))
        ttk.Label(footer, textvariable=default_var).pack(side="left")
        ttk.Button(footer, text="删除选中", command=delete_selected).pack(
            side="right"
        )
        ttk.Button(footer, text="设为默认", command=set_default).pack(
            side="right", padx=6
        )
        refresh_catalog()

    def open_import_dialog(self) -> None:
        try:
            catalog = self.service.load_start_url_catalog()
        except AccountManagerServiceError as error:
            self._error(error)
            return
        window = tk.Toplevel(self.root)
        window.title("粘贴并识别账号")
        window.geometry("980x700")
        template = tk.StringVar(value=self.INPUT_TEMPLATE)
        mode = tk.StringVar(value=LoginMode.ENTERPRISE.value)
        start_url = tk.StringVar(value=catalog.default_url)
        fields = ttk.Frame(window)
        fields.pack(fill="x", padx=10, pady=(10, 4))
        ttk.Label(fields, text="账号格式").grid(row=0, column=0, sticky="w")
        ttk.Combobox(
            fields,
            textvariable=template,
            values=self.INPUT_TEMPLATE_PRESETS,
        ).grid(row=0, column=1, sticky="ew", padx=(8, 0))
        ttk.Label(fields, text="登录方式").grid(row=1, column=0, sticky="w", pady=(6, 0))
        ttk.Combobox(
            fields,
            state="readonly",
            textvariable=mode,
            values=[item.value for item in LoginMode],
            width=18,
        ).grid(row=1, column=1, sticky="w", padx=(8, 0), pady=(6, 0))
        ttk.Label(fields, text="统一 Start URL").grid(row=2, column=0, sticky="w", pady=(6, 0))
        ttk.Combobox(
            fields,
            textvariable=start_url,
            values=catalog.urls,
        ).grid(
            row=2, column=1, sticky="ew", padx=(8, 0), pady=(6, 0)
        )
        ttk.Label(
            fields,
            text="每行没有 URL 时使用这里的地址；每行自带 URL 时以该行地址为准。",
            foreground="#475569",
        ).grid(row=3, column=1, sticky="w", padx=(8, 0), pady=(2, 0))
        fields.columnconfigure(1, weight=1)
        source = tk.Text(window, height=14)
        source.pack(fill="both", expand=True, padx=10, pady=6)
        summary = tk.StringVar(value="尚未解析")
        ttk.Label(window, textvariable=summary).pack(anchor="w", padx=10)
        preview_box = ttk.Treeview(window, columns=("line", "account", "url", "status"), show="headings", height=9)
        for column, title in (("line", "行"), ("account", "账号"), ("url", "Start URL"), ("status", "状态")):
            preview_box.heading(column, text=title)
        preview_box.pack(fill="both", expand=True, padx=10, pady=6)
        def parse_preview() -> ImportPreview | None:
            preview_box.delete(*preview_box.get_children())
            summary.set("正在解析")
            try:
                result = self.service.preview_import(
                    source.get("1.0", "end-1c"),
                    template.get(),
                    LoginMode(mode.get()),
                    default_start_url=start_url.get(),
                )
            except (ValueError, AccountManagerServiceError) as error:
                summary.set("解析失败")
                messagebox.showerror("解析失败", str(error), parent=window)
                return None
            for item in result.entries:
                preview_box.insert("", "end", values=(item.line_number, item.account, item.start_url or "", "有效"))
            for issue in result.issues:
                preview_box.insert("", "end", values=(issue.line_number, "", "", issue.message))
            summary.set(f"有效 {len(result.entries)} 个，提示/错误 {len(result.issues)} 个")
            return result

        def confirm():
            result = parse_preview()
            if result is None:
                return
            try:
                report = self.service.confirm_import(result)
            except AccountManagerServiceError as error:
                messagebox.showerror("保存失败", str(error), parent=window); return
            self.status_var.set(f"已保存 {report.saved} 个账号")
            window.destroy(); self.refresh()

        actions = ttk.Frame(window); actions.pack(fill="x", padx=10, pady=(0, 10))
        ttk.Button(actions, text="转换并预览", command=parse_preview).pack(side="left")
        ttk.Button(actions, text="保存到账号库", command=confirm).pack(side="right")

    def copy_selected_accounts(self) -> None:
        ids = self._selected_action_ids()
        if not ids:
            messagebox.showinfo("复制账号", "请先选择账号", parent=self.root)
            return
        try:
            accounts = [self.service.repository.get(item) for item in ids]
        except Exception as error:
            self._error(error)
            return
        self._copy("\n".join(item.account for item in accounts))
        self.status_var.set(f"已复制 {len(accounts)} 个账号")

    def copy_selected_account_info(self) -> None:
        ids = self._selected_action_ids()
        if not ids:
            messagebox.showinfo("复制账号信息", "请先选择账号", parent=self.root)
            return
        try:
            text = self.service.render_text(ids, self.DEFAULT_EXPORT_TEMPLATE)
        except AccountManagerServiceError as error:
            self._error(error)
            return
        self._copy(text)
        self.status_var.set(f"已复制 {len(ids)} 个账号信息")

    def copy_selected_start_urls(self) -> None:
        ids = self._selected_action_ids()
        if not ids:
            messagebox.showinfo("复制 Start URL", "请先选择账号", parent=self.root)
            return
        try:
            accounts = [self.service.repository.get(item) for item in ids]
        except Exception as error:
            self._error(error)
            return
        urls = list(
            dict.fromkeys(item.start_url for item in accounts if item.start_url)
        )
        if not urls:
            messagebox.showinfo(
                "复制 Start URL", "所选账号没有 Start URL", parent=self.root
            )
            return
        self._copy("\n".join(urls))
        self.status_var.set(f"已复制 {len(urls)} 个 Start URL")

    def open_password_viewer(self) -> None:
        ids = self._selected_action_ids()
        if len(ids) != 1:
            messagebox.showinfo("查看密码", "请选择一个账号", parent=self.root); return
        try:
            account = self._load_account_with_password_recovery(ids[0])
        except Exception as error:
            self._error(error); return
        self.refresh()
        window = tk.Toplevel(self.root); window.title(f"密码查看 - {account.account}")
        initial = tk.StringVar(value=account.initial_password or "")
        current = tk.StringVar(value=account.current_password or "")
        for row, (label, variable) in enumerate((("初始一次性密码", initial), ("当前登录密码", current))):
            ttk.Label(window, text=label).grid(row=row, column=0, padx=10, pady=8, sticky="w")
            entry = ttk.Entry(
                window, textvariable=variable, show="•", width=58,
                state="readonly",
            ); entry.grid(row=row, column=1, padx=6)
            ttk.Button(window, text="显示", command=lambda item=entry: item.configure(show="" if item.cget("show") else "•")).grid(row=row, column=2, padx=4)
            ttk.Button(window, text="复制", command=lambda var=variable: self._copy(var.get())).grid(row=row, column=3, padx=(0, 10))
        if not account.current_password:
            ttk.Label(
                window,
                text="未找到已确认的新密码，可使用“更新密码”手动录入。",
                foreground="#b45309",
            ).grid(row=2, column=0, columnspan=4, padx=10, pady=(2, 8))
        def close():
            clear_secret_vars(initial, current); window.destroy()
        window.protocol("WM_DELETE_WINDOW", close)

    def _load_account_with_password_recovery(self, account_id: int):
        account = self.service.repository.get(
            account_id, include_secrets=True
        )
        if account.current_password or self.coordinator is None:
            return account
        self.coordinator.sync_saved_passwords([account_id])
        return self.service.repository.get(
            account_id, include_secrets=True
        )

    def update_password(self) -> None:
        ids = self._selected_action_ids()
        if not ids:
            messagebox.showinfo("更新密码", "请先选择账号", parent=self.root); return
        password = simpledialog.askstring("更新当前密码", f"为 {len(ids)} 个账号设置最新密码", show="•", parent=self.root)
        if password is None:
            return
        try:
            count = self.service.update_password(ids, password)
        except AccountManagerServiceError as error:
            self._error(error); return
        self.status_var.set(f"已更新 {count} 个账号的当前密码"); self.refresh()

    def mark_sold(self) -> None:
        ids = self._selected_action_ids()
        note = simpledialog.askstring("标记已售", "客户/销售备注", parent=self.root)
        if note is None:
            return
        try:
            self.service.mark_sold(ids, note)
        except AccountManagerServiceError as error:
            self._error(error); return
        self.service.clear_selected(); self.refresh()

    def restore_managed(self) -> None:
        try:
            self.service.restore_managed(self._selected_action_ids())
        except AccountManagerServiceError as error:
            self._error(error); return
        self.service.clear_selected(); self.refresh()

    def open_export_dialog(self) -> None:
        ids = self._selected_action_ids()
        if not ids:
            messagebox.showinfo("导出", "请先选择账号", parent=self.root); return
        window = tk.Toplevel(self.root); window.title("导出账号密码")
        template = tk.StringVar(value=self.DEFAULT_EXPORT_TEMPLATE)
        note = tk.StringVar()
        sold = tk.BooleanVar(value=True)
        ttk.Label(window, text="导出模板").pack(anchor="w", padx=10, pady=(10, 0))
        ttk.Entry(window, textvariable=template, width=78).pack(fill="x", padx=10)
        preview = tk.Text(window, width=100, height=14, state="disabled"); preview.pack(fill="both", expand=True, padx=10, pady=8)
        ttk.Entry(window, textvariable=note).pack(fill="x", padx=10)
        ttk.Checkbutton(window, text="导出成功后标记为已售出", variable=sold).pack(anchor="w", padx=10, pady=6)
        def render():
            try: text = self.service.render_text(ids, template.get())
            except AccountManagerServiceError as error: self._error(error); return None
            preview.configure(state="normal"); preview.delete("1.0", "end"); preview.insert("1.0", text); preview.configure(state="disabled"); return text
        def finish(writer):
            try:
                self.service.export_text(ids, template=template.get(), writer=writer, note=note.get(), mark_sold=sold.get())
            except AccountManagerServiceError as error: self._error(error); return
            window.destroy(); self.service.clear_selected(); self.refresh()
        def copy(): finish(self._copy)
        def save():
            path = filedialog.asksaveasfilename(title="保存账号文本", defaultextension=".txt", filetypes=[("文本文件", "*.txt")], parent=window)
            if path: finish(lambda text: atomic_write_text(Path(path), text))
        actions = ttk.Frame(window); actions.pack(fill="x", padx=10, pady=(0, 10))
        ttk.Button(actions, text="刷新预览", command=render).pack(side="left")
        ttk.Button(actions, text="复制", command=copy).pack(side="right")
        ttk.Button(actions, text="保存 TXT", command=save).pack(side="right", padx=6)
        render()

    def open_legacy_login(self) -> None:
        window = tk.Toplevel(self.root)
        BatchLoginApp(window, build_default_controller(), ssh_available=shutil.which("ssh") is not None)

    def _append_login_log(self, message: str) -> None:
        safe_message = redact_text(str(message))
        self.login_log_text.configure(state="normal")
        self.login_log_text.insert("end", safe_message + "\n")
        self.login_log_text.see("end")
        self.login_log_text.configure(state="disabled")

    def _prepare_login_progress(self, ids: list[int]) -> None:
        self.active_login_ids = list(ids)
        for account_id in ids:
            self.json_status_by_id[account_id] = "等待中"
            if self.tree.exists(str(account_id)):
                self.tree.set(str(account_id), "json_status", "等待中")
        self.login_progress_var.set(0)
        self.login_progress_text_var.set(f"JSON 进度：0/{len(ids)}")
        self._append_login_log(f"开始获取 {len(ids)} 个账号的 JSON")

    def _apply_login_progress(self, event: LoginProgressEvent) -> None:
        labels = {
            "waiting": "等待中",
            "running": "处理中",
            "reused": "复用成功",
            "success": "成功",
        }
        if event.status == "failed":
            code = redact_text(str(event.code or "未知错误"))
            status = f"失败：{code}"
        else:
            status = labels.get(event.status, redact_text(event.status))
        self.json_status_by_id[event.account_id] = status
        if self.tree.exists(str(event.account_id)):
            self.tree.set(str(event.account_id), "json_status", status)
        self.login_progress_var.set(
            event.completed * 100 / max(event.total, 1)
        )
        self.login_progress_text_var.set(
            f"JSON 进度：{event.completed}/{event.total}"
        )
        if event.status != "waiting":
            self._append_login_log(
                f"[{event.index}/{event.total}] {event.account_masked}：{status}"
            )

    def _apply_worker_event(self, event: WorkerEvent) -> None:
        self._append_login_log(format_worker_event(event))

    def _poll_login_events(self) -> None:
        while True:
            try:
                event = self.login_event_queue.get_nowait()
            except queue.Empty:
                break
            if isinstance(event, LoginProgressEvent):
                self._apply_login_progress(event)
            elif isinstance(event, WorkerEvent):
                self._apply_worker_event(event)
            elif isinstance(event, tuple) and event[:1] == ("finished",):
                self._login_finished(report=event[1], error=event[2])
        try:
            self.root.after(80, self._poll_login_events)
        except tk.TclError:
            pass

    def start_login_export(self) -> None:
        if self.login_running:
            messagebox.showinfo("一键登录", "已有登录任务正在运行", parent=self.root)
            return
        ids = self._selected_action_ids()
        if not ids:
            messagebox.showinfo("一键登录", "请先选择账号", parent=self.root)
            return
        if self.coordinator is None:
            messagebox.showerror("一键登录", "登录协调器未初始化", parent=self.root)
            return
        choice = messagebox.askyesnocancel(
            "一键登录导出 JSON",
            "选择“是”将强制重新登录全部账号；选择“否”将复用已有有效凭据。",
            parent=self.root,
        )
        if choice is None:
            return
        self.login_running = True
        self._prepare_login_progress(ids)
        self.status_var.set(f"正在处理 {len(ids)} 个账号…")

        def worker():
            try:
                report = asyncio.run(
                    self.coordinator.run(
                        ids,
                        force_relogin=bool(choice),
                        progress=self.login_event_queue.put,
                        event_sink=self.login_event_queue.put,
                    )
                )
            except Exception as error:
                self.login_event_queue.put(("finished", None, error))
                return
            self.login_event_queue.put(("finished", report, None))

        threading.Thread(
            target=worker,
            name="kiro-account-manager-login",
            daemon=False,
        ).start()

    def _login_finished(self, *, report=None, error=None) -> None:
        self.login_running = False
        if error is not None:
            for account_id in self.active_login_ids:
                if self.json_status_by_id.get(account_id) in {
                    "等待中",
                    "处理中",
                }:
                    self.json_status_by_id[account_id] = "失败：任务异常"
            total = len(self.active_login_ids)
            self.login_progress_var.set(100 if total else 0)
            self.login_progress_text_var.set(f"JSON 进度：{total}/{total}")
            self._append_login_log("JSON 获取任务异常结束")
            self._error(error)
            self.refresh()
            self.status_var.set("一键登录导出失败")
            return
        self.service.clear_selected()
        self.refresh()
        self._append_login_log(
            f"任务完成：登录 {report.logged_in}，复用 {report.reused}，"
            f"失败 {report.failed}，导出 {report.exported}"
        )
        self.status_var.set(
            f"完成：登录 {report.logged_in}，复用 {report.reused}，失败 {report.failed}，导出 {report.exported}"
        )

    def _copy(self, text: str) -> None:
        self.root.clipboard_clear(); self.root.clipboard_append(text)

    def _error(self, error: BaseException) -> None:
        messagebox.showerror("操作失败", redact_text(str(error)), parent=self.root)
