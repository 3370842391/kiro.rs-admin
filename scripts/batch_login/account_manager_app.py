from __future__ import annotations

import os
import shutil
import asyncio
import threading
import tkinter as tk
from pathlib import Path
from tkinter import filedialog, messagebox, simpledialog, ttk
from uuid import uuid4

from .account_manager_service import (
    AccountManagerService,
    AccountManagerServiceError,
    ImportPreview,
)
from .account_repository import LifecycleStatus
from .gui_app import BatchLoginApp
from .gui_runtime import build_default_controller
from .models import LoginMode
from .redaction import redact_text


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


def select_range_ids(
    row_ids: list[str] | tuple[str, ...],
    anchor: str,
    current: str,
) -> set[int]:
    try:
        start = row_ids.index(anchor)
        end = row_ids.index(current)
    except ValueError:
        return set()
    low, high = sorted((start, end))
    return {int(item) for item in row_ids[low : high + 1]}


class AccountManagerApp:
    TABLE_COLUMNS = (
        "checked",
        "account",
        "password",
        "start_url",
        "login_status",
        "credential_status",
        "lifecycle_status",
        "note",
        "updated_at",
    )
    DEFAULT_EXPORT_TEMPLATE = "{account}----{password}----{start_url}"
    PRIMARY_ACTION_LABEL = "一键登录导出 JSON"
    INPUT_TEMPLATE = "{account}|{password}|{start_url}"
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
        self.visible_ids: list[int] = []
        self.drag_anchor = ""
        self.root.title("Kiro 账号管理器")
        self.root.geometry("1420x820")
        self.root.minsize(1080, 640)
        self._build()
        self.refresh()

    def _build(self) -> None:
        outer = ttk.Frame(self.root, padding=12)
        outer.pack(fill="both", expand=True)
        ttk.Label(outer, text="Kiro 账号管理器", font=("Microsoft YaHei UI", 16, "bold")).pack(anchor="w")
        ttk.Label(outer, text="粘贴入库、批量选择、密码查看、销售标记和导出", foreground="#475569").pack(anchor="w", pady=(2, 8))
        toolbar = ttk.Frame(outer)
        toolbar.pack(fill="x", pady=(0, 8))
        ttk.Entry(toolbar, textvariable=self.query_var, width=34).pack(side="left")
        self.query_var.trace_add("write", lambda *_args: self.refresh())
        status = ttk.Combobox(toolbar, state="readonly", textvariable=self.filter_var, values=list(self.STATUS_VALUES), width=10)
        status.pack(side="left", padx=6)
        status.bind("<<ComboboxSelected>>", lambda _event: self.refresh())
        for text, command in (
            ("粘贴并识别", self.open_import_dialog),
            (self.PRIMARY_ACTION_LABEL, self.start_login_export),
            ("全选", self.select_all),
            ("反选", self.invert_selection),
            ("取消选择", self.clear_selection),
            ("查看密码", self.open_password_viewer),
            ("更新密码", self.update_password),
            ("导出账号密码", self.open_export_dialog),
            ("标记已售", self.mark_sold),
            ("恢复管理", self.restore_managed),
            ("自动登录设置", self.open_legacy_login),
        ):
            ttk.Button(toolbar, text=text, command=command).pack(side="left", padx=2)

        self.tree = ttk.Treeview(outer, columns=self.TABLE_COLUMNS, show="headings", selectmode="extended")
        headings = {
            "checked": "选择", "account": "账号", "password": "当前密码",
            "start_url": "Start URL", "login_status": "登录状态",
            "credential_status": "凭据状态", "lifecycle_status": "销售状态",
            "note": "备注", "updated_at": "更新时间",
        }
        widths = {"checked": 55, "account": 170, "password": 100, "start_url": 260, "login_status": 85, "credential_status": 85, "lifecycle_status": 85, "note": 170, "updated_at": 155}
        for column in self.TABLE_COLUMNS:
            self.tree.heading(column, text=headings[column])
            self.tree.column(column, width=widths[column], anchor="w")
        self.tree.pack(fill="both", expand=True)
        self.tree.bind("<Button-1>", self._tree_click, add="+")
        self.tree.bind("<B1-Motion>", self._tree_drag, add="+")
        self.tree.bind("<<TreeviewSelect>>", self._tree_selection, add="+")
        self.tree.bind("<Double-1>", lambda _event: self.open_password_viewer())
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
        self.tree.delete(*self.tree.get_children())
        for item in accounts:
            self.tree.insert("", "end", iid=str(item.id), values=(
                "☑" if item.id in selected else "☐", item.account, "••••••" if item.credential_status.value != "missing" else "未设置",
                item.start_url or "", item.login_status.value, item.credential_status.value,
                "已售出" if item.lifecycle_status is LifecycleStatus.SOLD else "管理中",
                item.note, item.updated_at,
            ))
            if item.id in selected:
                self.tree.selection_add(str(item.id))
        self._update_selected_count()
        self.status_var.set(f"显示 {len(accounts)} 个账号")

    def _tree_click(self, event) -> None:
        row = self.tree.identify_row(event.y)
        if not row:
            return
        self.drag_anchor = row
        if self.tree.identify_column(event.x) == "#1":
            self.service.toggle_selected(int(row))
            self.refresh()

    def _tree_drag(self, event) -> None:
        current = self.tree.identify_row(event.y)
        if not self.drag_anchor or not current:
            return
        ids = select_range_ids(list(self.tree.get_children()), self.drag_anchor, current)
        self.service.select_visible(ids)
        self.refresh()

    def _tree_selection(self, _event=None) -> None:
        self.service.select_visible(int(item) for item in self.tree.selection())
        self._update_selected_count()

    def _update_selected_count(self) -> None:
        self.selected_count_var.set(f"已选择 {len(self.service.selected_ids)} 个账号")

    def select_all(self) -> None:
        self.service.select_visible(self.visible_ids)
        self.refresh()

    def invert_selection(self) -> None:
        self.service.invert_visible(self.visible_ids)
        self.refresh()

    def clear_selection(self) -> None:
        self.service.clear_selected()
        self.refresh()

    def open_import_dialog(self) -> None:
        window = tk.Toplevel(self.root)
        window.title("粘贴并识别账号")
        window.geometry("980x650")
        template = tk.StringVar(value=self.INPUT_TEMPLATE)
        mode = tk.StringVar(value=LoginMode.ENTERPRISE.value)
        ttk.Entry(window, textvariable=template).pack(fill="x", padx=10, pady=(10, 4))
        ttk.Combobox(window, state="readonly", textvariable=mode, values=[item.value for item in LoginMode]).pack(anchor="w", padx=10)
        source = tk.Text(window, height=14)
        source.pack(fill="both", expand=True, padx=10, pady=6)
        summary = tk.StringVar(value="尚未解析")
        ttk.Label(window, textvariable=summary).pack(anchor="w", padx=10)
        preview_box = ttk.Treeview(window, columns=("line", "account", "url", "status"), show="headings", height=9)
        for column, title in (("line", "行"), ("account", "账号"), ("url", "Start URL"), ("status", "状态")):
            preview_box.heading(column, text=title)
        preview_box.pack(fill="both", expand=True, padx=10, pady=6)
        state: dict[str, ImportPreview] = {}

        def parse_preview():
            try:
                result = self.service.preview_import(source.get("1.0", "end-1c"), template.get(), LoginMode(mode.get()))
            except (ValueError, AccountManagerServiceError) as error:
                messagebox.showerror("解析失败", str(error), parent=window)
                return
            state["preview"] = result
            preview_box.delete(*preview_box.get_children())
            for item in result.entries:
                preview_box.insert("", "end", values=(item.line_number, item.account, item.start_url or "", "有效"))
            for issue in result.issues:
                preview_box.insert("", "end", values=(issue.line_number, "", "", issue.message))
            summary.set(f"有效 {len(result.entries)} 个，提示/错误 {len(result.issues)} 个")

        def confirm():
            result = state.get("preview")
            if result is None:
                parse_preview(); result = state.get("preview")
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

    def open_password_viewer(self) -> None:
        ids = sorted(self.service.selected_ids)
        if len(ids) != 1:
            messagebox.showinfo("查看密码", "请选择一个账号", parent=self.root); return
        try:
            account = self.service.repository.get(ids[0], include_secrets=True)
        except Exception as error:
            self._error(error); return
        window = tk.Toplevel(self.root); window.title(f"密码查看 - {account.account}")
        initial = tk.StringVar(value=account.initial_password or "")
        current = tk.StringVar(value=account.current_password or "")
        for row, (label, variable) in enumerate((("初始一次性密码", initial), ("当前登录密码", current))):
            ttk.Label(window, text=label).grid(row=row, column=0, padx=10, pady=8, sticky="w")
            entry = ttk.Entry(window, textvariable=variable, show="•", width=58); entry.grid(row=row, column=1, padx=6)
            ttk.Button(window, text="显示", command=lambda item=entry: item.configure(show="" if item.cget("show") else "•")).grid(row=row, column=2, padx=4)
            ttk.Button(window, text="复制", command=lambda var=variable: self._copy(var.get())).grid(row=row, column=3, padx=(0, 10))
        def close():
            clear_secret_vars(initial, current); window.destroy()
        window.protocol("WM_DELETE_WINDOW", close)

    def update_password(self) -> None:
        ids = sorted(self.service.selected_ids)
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
        ids = sorted(self.service.selected_ids)
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
            self.service.restore_managed(sorted(self.service.selected_ids))
        except AccountManagerServiceError as error:
            self._error(error); return
        self.service.clear_selected(); self.refresh()

    def open_export_dialog(self) -> None:
        ids = sorted(self.service.selected_ids)
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

    def start_login_export(self) -> None:
        if self.login_running:
            messagebox.showinfo("一键登录", "已有登录任务正在运行", parent=self.root)
            return
        ids = sorted(self.service.selected_ids)
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
        self.status_var.set(f"正在处理 {len(ids)} 个账号…")

        def worker():
            try:
                report = asyncio.run(
                    self.coordinator.run(ids, force_relogin=bool(choice))
                )
            except Exception as error:
                self.root.after(
                    0, lambda captured=error: self._login_finished(error=captured)
                )
                return
            self.root.after(0, lambda: self._login_finished(report=report))

        threading.Thread(
            target=worker,
            name="kiro-account-manager-login",
            daemon=False,
        ).start()

    def _login_finished(self, *, report=None, error=None) -> None:
        self.login_running = False
        if error is not None:
            self._error(error)
            self.refresh()
            self.status_var.set("一键登录导出失败")
            return
        self.service.clear_selected()
        self.refresh()
        self.status_var.set(
            f"完成：登录 {report.logged_in}，复用 {report.reused}，失败 {report.failed}，导出 {report.exported}"
        )

    def _copy(self, text: str) -> None:
        self.root.clipboard_clear(); self.root.clipboard_append(text)

    def _error(self, error: BaseException) -> None:
        messagebox.showerror("操作失败", redact_text(str(error)), parent=self.root)
