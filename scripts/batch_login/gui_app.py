from __future__ import annotations

import json
import os
import tkinter as tk
from pathlib import Path
from tkinter import filedialog, messagebox, ttk

from .gui_controller import GuiController, GuiFormState
from .gui_settings import GuiSavedSettings, GuiSettingsError, GuiSettingsStore
from .input_parser import parse_accounts, render_accounts
from .models import LoginMode, ParseResult
from .oidc_exporter import OidcExportMode
from .redaction import redact_text
from .worker_events import ResultMode, WorkerEvent


class BatchLoginApp:
    POLL_MS = 100
    INPUT_FORMAT_PRESETS = (
        "login = {account} / onetime password = {password}",
        "{account}----{password}",
        "{account}|{password}|{start_url}",
    )
    OIDC_EXPORT_LABELS = {
        OidcExportMode.MERGED: "合并 JSON",
        OidcExportMode.PER_ACCOUNT: "逐账号 JSON",
        OidcExportMode.BOTH: "两种同时",
    }
    OIDC_EXPORT_VALUES = {
        label: mode for mode, label in OIDC_EXPORT_LABELS.items()
    }

    def __init__(
        self,
        root: tk.Tk,
        controller: GuiController,
        *,
        ssh_available: bool = True,
        settings_store: GuiSettingsStore | None = None,
    ):
        self.root = root
        self.controller = controller
        self.ssh_available = ssh_available
        self.settings_store = settings_store or GuiSettingsStore()
        self.settings_warning = ""
        saved_settings = self._load_saved_settings()
        self.form = GuiFormState(
            admin_key=os.environ.get("KIRO_RS_ADMIN_KEY", "")
        )
        self.entries = []
        self.input_path = ""
        self._build_variables()
        if saved_settings is not None:
            self._apply_saved_settings(saved_settings)
        self._configure_style()
        self._build_layout()
        self._apply_mode_visibility()
        if self.settings_warning:
            self.status_var.set("本地配置未加载")
            self._append_log(self.settings_warning)
        self.root.protocol("WM_DELETE_WINDOW", self._on_close)
        self.root.bind("<Control-o>", lambda _event: self._open_input_file())
        self.root.bind("<Control-Return>", lambda _event: self._start())
        self.root.bind("<Escape>", lambda _event: self.controller.cancel())
        self.root.after(self.POLL_MS, self._poll_events)

    def _build_variables(self) -> None:
        self.input_template_var = tk.StringVar(value=self.form.input_template)
        self.output_template_var = tk.StringVar(value=self.form.output_template)
        self.mode_var = tk.StringVar(value=self.form.mode.value)
        self.start_url_var = tk.StringVar()
        self.password_vault_path_var = tk.StringVar()
        self.region_var = tk.StringVar(value="us-east-1")
        self.headless_var = tk.BooleanVar(value=False)
        self.timeout_var = tk.DoubleVar(value=180)
        self.mfa_timeout_var = tk.DoubleVar(value=300)
        self.result_mode_var = tk.StringVar(value=ResultMode.SAVE_ONLY.value)
        self.credential_path_var = tk.StringVar()
        self.oidc_export_mode_var = tk.StringVar(
            value=self.OIDC_EXPORT_LABELS[OidcExportMode.MERGED]
        )
        self.oidc_export_directory_var = tk.StringVar()
        self.checkpoint_path_var = tk.StringVar()
        self.resume_var = tk.BooleanVar(value=False)
        self.create_api_key_var = tk.BooleanVar(value=False)
        self.api_key_skip_if_exists_var = tk.BooleanVar(value=False)
        self.rs_url_var = tk.StringVar()
        self.admin_key_var = tk.StringVar(value=self.form.admin_key)
        self.use_ssh_var = tk.BooleanVar(value=False)
        self.ssh_host_var = tk.StringVar()
        self.ssh_user_var = tk.StringVar()
        self.ssh_port_var = tk.StringVar(value="22")
        self.identity_file_var = tk.StringVar()
        self.remote_host_var = tk.StringVar(value="127.0.0.1")
        self.remote_port_var = tk.StringVar(value="8990")
        self.local_port_var = tk.StringVar()
        self.status_var = tk.StringVar(value="准备就绪")
        self.progress_var = tk.DoubleVar(value=0)
        self.show_password_var = tk.BooleanVar(value=False)
        self.last_result = ParseResult([], [])

    def _configure_style(self) -> None:
        style = ttk.Style(self.root)
        style.configure("Title.TLabel", font=("Microsoft YaHei UI", 16, "bold"))
        style.configure("Muted.TLabel", foreground="#475569")
        style.configure("Accent.TButton", font=("Microsoft YaHei UI", 10, "bold"))
        style.configure("Treeview", rowheight=25)
        style.configure("Treeview.Heading", font=("Microsoft YaHei UI", 9, "bold"))

    def _build_layout(self) -> None:
        self.run_sensitive: list[tk.Widget] = []
        self.root.title("Kiro 批量登录助手")
        self.root.minsize(1050, 720)
        self.root.geometry("1180x860")
        outer = ttk.Frame(self.root, padding=12)
        outer.pack(fill="both", expand=True)
        ttk.Label(outer, text="Kiro 批量登录助手", style="Title.TLabel").pack(
            anchor="w"
        )
        ttk.Label(
            outer,
            text="本机完成登录，先保存完整凭据 JSON，再按需导入 RS。",
            style="Muted.TLabel",
        ).pack(anchor="w", pady=(2, 10))
        self._build_rule_bar(outer)
        self._build_input_preview(outer)
        settings = ttk.PanedWindow(outer, orient="horizontal")
        settings.pack(fill="x", pady=(0, 8))
        login_host = ttk.Frame(settings)
        rs_host = ttk.Frame(settings)
        settings.add(login_host, weight=1)
        settings.add(rs_host, weight=1)
        self._build_login_settings(login_host)
        self._build_rs_settings(rs_host)
        self._build_log_progress(outer)
        self._build_actions(outer)

    def _build_rule_bar(self, parent) -> None:
        frame = ttk.LabelFrame(parent, text="解析规则", padding=8)
        frame.pack(fill="x", pady=(0, 8))
        input_format = ttk.Combobox(
            frame,
            textvariable=self.input_template_var,
            values=self.INPUT_FORMAT_PRESETS,
        )
        input_format.grid(row=0, column=0, sticky="ew", padx=(0, 6))
        output_format = ttk.Entry(
            frame,
            textvariable=self.output_template_var,
            width=28,
        )
        output_format.grid(row=0, column=1, sticky="ew", padx=6)
        open_button = ttk.Button(
            frame,
            text="打开文件",
            command=self._open_input_file,
        )
        open_button.grid(row=0, column=2, padx=6)
        preview_button = ttk.Button(
            frame,
            text="转换并预览",
            command=self._convert_preview,
        )
        preview_button.grid(row=0, column=3)
        frame.columnconfigure(0, weight=3)
        frame.columnconfigure(1, weight=2)
        self.run_sensitive.extend(
            [input_format, output_format, open_button, preview_button]
        )

    def _build_input_preview(self, parent) -> None:
        pane = ttk.PanedWindow(parent, orient="horizontal")
        pane.pack(fill="both", expand=True, pady=(0, 8))
        left = ttk.LabelFrame(pane, text="原始账号文本", padding=6)
        right = ttk.LabelFrame(pane, text="转换预览", padding=6)
        pane.add(left, weight=1)
        pane.add(right, weight=1)
        self.input_text = tk.Text(
            left,
            wrap="none",
            undo=True,
            height=12,
            font=("Cascadia Mono", 10),
        )
        self.input_text.pack(fill="both", expand=True)
        self.run_sensitive.append(self.input_text)
        columns = (
            "line",
            "account",
            "password",
            "start_url",
            "status",
            "reason",
        )
        self.preview = ttk.Treeview(
            right,
            columns=columns,
            show="headings",
            height=11,
        )
        for name, title, width in (
            ("line", "行", 48),
            ("account", "账号", 180),
            ("password", "密码", 150),
            ("start_url", "企业门户", 260),
            ("status", "状态", 85),
            ("reason", "原因", 180),
        ):
            self.preview.heading(name, text=title)
            self.preview.column(name, width=width, anchor="w")
        self.preview.pack(fill="both", expand=True)
        actions = ttk.Frame(right)
        actions.pack(fill="x", pady=(6, 0))
        ttk.Checkbutton(
            actions,
            text="显示密码",
            variable=self.show_password_var,
            command=self._render_last_preview,
        ).pack(side="left")
        copy_button = ttk.Button(
            actions,
            text="复制统一格式",
            command=self._copy_output,
        )
        copy_button.pack(side="right")
        save_button = ttk.Button(
            actions,
            text="保存账号 TXT",
            command=self._save_output,
        )
        save_button.pack(side="right", padx=6)
        self.run_sensitive.extend([copy_button, save_button])

    def _entry_row(
        self,
        frame,
        row,
        label,
        variable,
        *,
        show=None,
        browse=None,
    ):
        label_widget = ttk.Label(frame, text=label)
        label_widget.grid(
            row=row,
            column=0,
            sticky="w",
            padx=(0, 6),
            pady=3,
        )
        entry = ttk.Entry(frame, textvariable=variable, show=show)
        entry.grid(row=row, column=1, sticky="ew", pady=3)
        frame.columnconfigure(1, weight=1)
        self.run_sensitive.append(entry)
        row_widgets = [label_widget, entry]
        if browse is not None:
            button = ttk.Button(frame, text="浏览", command=browse, width=7)
            button.grid(row=row, column=2, padx=(6, 0), pady=3)
            self.run_sensitive.append(button)
            row_widgets.append(button)
        entry._batch_login_row_widgets = row_widgets
        return entry

    def _build_login_settings(self, parent) -> None:
        frame = ttk.LabelFrame(parent, text="登录与结果", padding=8)
        frame.pack(fill="both", expand=True, padx=(0, 4))
        modes = ttk.Frame(frame)
        modes.grid(row=0, column=0, columnspan=3, sticky="w")
        for text, value in (("企业账号", "enterprise"), ("Microsoft", "microsoft")):
            button = ttk.Radiobutton(
                modes,
                text=text,
                value=value,
                variable=self.mode_var,
                command=self._apply_mode_visibility,
            )
            button.pack(side="left", padx=(0, 8))
            self.run_sensitive.append(button)
        self.start_url_entry = self._entry_row(
            frame, 1, "Start URL", self.start_url_var
        )
        self.password_vault_entry = self._entry_row(
            frame,
            2,
            "密码保险库（自动生成密码）",
            self.password_vault_path_var,
        )
        self._entry_row(frame, 3, "Region", self.region_var)
        self._entry_row(
            frame,
            4,
            "完整凭据 JSON",
            self.credential_path_var,
            browse=self._choose_credential_path,
        )
        self._entry_row(
            frame,
            5,
            "Checkpoint",
            self.checkpoint_path_var,
            browse=self._choose_checkpoint_path,
        )
        ttk.Label(frame, text="OIDC 导出方式").grid(
            row=6, column=0, sticky="w", pady=3
        )
        oidc_mode = ttk.Combobox(
            frame,
            state="readonly",
            textvariable=self.oidc_export_mode_var,
            values=list(self.OIDC_EXPORT_VALUES),
        )
        oidc_mode.grid(row=6, column=1, sticky="ew", pady=3)
        self.run_sensitive.append(oidc_mode)
        self._entry_row(
            frame,
            7,
            "OIDC 导出目录",
            self.oidc_export_directory_var,
            browse=self._choose_oidc_export_directory,
        )
        self.headless_toggle = ttk.Checkbutton(
            frame,
            text="无头浏览器",
            variable=self.headless_var,
        )
        self.headless_toggle.grid(row=8, column=0, sticky="w", pady=(5, 0))
        resume = ttk.Checkbutton(
            frame,
            text="恢复运行",
            variable=self.resume_var,
        )
        resume.grid(row=8, column=1, sticky="w", pady=(5, 0))
        create_key = ttk.Checkbutton(
            frame,
            text="登录后自动创建 API Key",
            variable=self.create_api_key_var,
        )
        create_key.grid(row=8, column=2, sticky="w", pady=(5, 0))
        skip_key = ttk.Checkbutton(
            frame,
            text="已存在同名则跳过",
            variable=self.api_key_skip_if_exists_var,
        )
        skip_key.grid(row=8, column=3, sticky="w", pady=(5, 0))
        self.run_sensitive.extend([self.headless_toggle, resume, create_key, skip_key])
        ttk.Label(frame, text="结果方式").grid(
            row=9, column=0, sticky="w", pady=(5, 0)
        )
        result = ttk.Combobox(
            frame,
            state="readonly",
            textvariable=self.result_mode_var,
            values=[
                ResultMode.SAVE_ONLY.value,
                ResultMode.SAVE_AND_IMPORT.value,
            ],
        )
        result.grid(row=9, column=1, sticky="ew", pady=(5, 0))
        result.bind(
            "<<ComboboxSelected>>",
            lambda _event: self._apply_mode_visibility(),
        )
        self.run_sensitive.append(result)

    def _build_rs_settings(self, parent) -> None:
        self.rs_frame = ttk.LabelFrame(parent, text="RS 连接", padding=8)
        self.rs_frame.pack(fill="both", expand=True, padx=(4, 0))
        self.ssh_toggle = ttk.Checkbutton(
            self.rs_frame,
            text="使用 SSH 隧道",
            variable=self.use_ssh_var,
            command=self._apply_mode_visibility,
        )
        self.ssh_toggle.grid(row=0, column=0, columnspan=3, sticky="w")
        if not self.ssh_available:
            self.ssh_toggle.configure(state="disabled")
            self.use_ssh_var.set(False)
        else:
            self.run_sensitive.append(self.ssh_toggle)
        self.direct_widgets = [
            self._entry_row(self.rs_frame, 1, "RS URL", self.rs_url_var)
        ]
        self._entry_row(
            self.rs_frame,
            2,
            "Admin Key",
            self.admin_key_var,
            show="•",
        )
        self.ssh_widgets = [
            self._entry_row(self.rs_frame, 3, "SSH 主机", self.ssh_host_var),
            self._entry_row(self.rs_frame, 4, "SSH 用户", self.ssh_user_var),
            self._entry_row(self.rs_frame, 5, "SSH 端口", self.ssh_port_var),
            self._entry_row(
                self.rs_frame,
                6,
                "私钥路径",
                self.identity_file_var,
                browse=self._choose_identity_file,
            ),
            self._entry_row(self.rs_frame, 7, "远端主机", self.remote_host_var),
            self._entry_row(self.rs_frame, 8, "远端端口", self.remote_port_var),
            self._entry_row(self.rs_frame, 9, "本地端口", self.local_port_var),
        ]

    def _build_log_progress(self, parent) -> None:
        self.log_frame = ttk.LabelFrame(parent, text="运行日志", padding=6)
        self.log_frame.pack(fill="both", pady=(0, 8))
        self.log_text = tk.Text(
            self.log_frame,
            height=5,
            state="disabled",
            wrap="word",
            font=("Cascadia Mono", 9),
        )
        self.log_text.pack(fill="both", expand=True)
        ttk.Progressbar(
            self.log_frame,
            variable=self.progress_var,
            maximum=100,
        ).pack(fill="x", pady=(6, 0))
        ttk.Label(
            self.log_frame,
            textvariable=self.status_var,
            style="Muted.TLabel",
        ).pack(anchor="w", pady=(4, 0))

    def _build_actions(self, parent) -> None:
        frame = ttk.Frame(parent)
        frame.pack(fill="x")
        import_button = ttk.Button(
            frame,
            text="导入已有 JSON",
            command=self._import_existing,
        )
        import_button.pack(side="left")
        export_button = ttk.Button(
            frame,
            text="转换已有完整 JSON",
            command=self._export_existing,
        )
        export_button.pack(side="left", padx=(8, 0))
        save_config_button = ttk.Button(
            frame,
            text="保存配置",
            command=self._save_configuration,
        )
        save_config_button.pack(side="left", padx=(8, 0))
        clear_config_button = ttk.Button(
            frame,
            text="清除配置",
            command=self._clear_configuration,
        )
        clear_config_button.pack(side="left", padx=(6, 0))
        self.stop_button = ttk.Button(
            frame,
            text="停止",
            command=self.controller.cancel,
            state="disabled",
        )
        self.stop_button.pack(side="right")
        start_button = ttk.Button(
            frame,
            text="开始批量登录",
            command=self._start,
            style="Accent.TButton",
        )
        start_button.pack(side="right", padx=8)
        self.run_sensitive.extend(
            [
                import_button,
                export_button,
                save_config_button,
                clear_config_button,
                start_button,
            ]
        )

    def _load_saved_settings(self) -> GuiSavedSettings | None:
        try:
            return self.settings_store.load()
        except GuiSettingsError as error:
            self.settings_warning = redact_text(str(error))
            return None

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
            "oidc_export_directory": self.oidc_export_directory_var,
            "create_api_key": self.create_api_key_var,
            "api_key_skip_if_exists": self.api_key_skip_if_exists_var,
        }
        for name, variable in bindings.items():
            value = getattr(settings, name)
            if name == "admin_key" and not value:
                continue
            variable.set(value)
        mode = OidcExportMode(settings.oidc_export_mode)
        self.oidc_export_mode_var.set(self.OIDC_EXPORT_LABELS[mode])

    def _snapshot_settings(self) -> GuiSavedSettings:
        return GuiSavedSettings(
            input_template=self.input_template_var.get(),
            output_template=self.output_template_var.get(),
            mode=self.mode_var.get(),
            start_url=self.start_url_var.get(),
            password_vault_path=self.password_vault_path_var.get(),
            region=self.region_var.get(),
            headless=bool(self.headless_var.get()),
            timeout_seconds=float(self.timeout_var.get()),
            mfa_timeout_seconds=float(self.mfa_timeout_var.get()),
            result_mode=self.result_mode_var.get(),
            credential_path=self.credential_path_var.get(),
            checkpoint_path=self.checkpoint_path_var.get(),
            resume=bool(self.resume_var.get()),
            rs_url=self.rs_url_var.get(),
            admin_key=self.admin_key_var.get(),
            use_ssh=bool(self.use_ssh_var.get()),
            ssh_host=self.ssh_host_var.get(),
            ssh_user=self.ssh_user_var.get(),
            ssh_port=self.ssh_port_var.get(),
            identity_file=self.identity_file_var.get(),
            remote_host=self.remote_host_var.get(),
            remote_port=self.remote_port_var.get(),
            local_port=self.local_port_var.get(),
            oidc_export_mode=self._selected_oidc_export_mode().value,
            oidc_export_directory=self.oidc_export_directory_var.get(),
            create_api_key=bool(self.create_api_key_var.get()),
            api_key_skip_if_exists=bool(self.api_key_skip_if_exists_var.get()),
        )

    def _save_configuration(self) -> None:
        try:
            path = self.settings_store.save(self._snapshot_settings())
        except (GuiSettingsError, TypeError, ValueError, tk.TclError) as error:
            messagebox.showerror(
                "保存配置失败",
                redact_text(str(error)),
                parent=self.root,
            )
            return
        message = f"配置已保存到 {path}（包含明文 Admin Key）"
        self.status_var.set(message)
        self._append_log(message)

    def _clear_configuration(self) -> None:
        if not messagebox.askyesno(
            "清除配置",
            "删除本地保存配置？当前表单不会清空。",
            parent=self.root,
        ):
            return
        try:
            self.settings_store.clear()
        except GuiSettingsError as error:
            messagebox.showerror(
                "清除配置失败",
                redact_text(str(error)),
                parent=self.root,
            )
            return
        message = "配置已清除，下次启动使用默认值"
        self.status_var.set(message)
        self._append_log(message)

    def _apply_mode_visibility(self) -> None:
        if self.mode_var.get() == LoginMode.ENTERPRISE.value:
            self.start_url_entry.grid()
            self._set_row_visible(self.password_vault_entry, True)
            self.headless_toggle.grid_remove()
        else:
            self.start_url_entry.grid_remove()
            self._set_row_visible(self.password_vault_entry, False)
            self.headless_toggle.grid()
        import_mode = (
            self.result_mode_var.get() == ResultMode.SAVE_AND_IMPORT.value
        )
        for child in self.rs_frame.winfo_children():
            if child is self.ssh_toggle and not self.ssh_available:
                continue
            child.configure(state="normal" if import_mode else "disabled")
        use_ssh = self.use_ssh_var.get() and import_mode
        for widget in self.direct_widgets:
            self._set_row_visible(widget, not use_ssh)
        for widget in self.ssh_widgets:
            self._set_row_visible(widget, use_ssh)

    @staticmethod
    def _set_row_visible(entry, visible: bool) -> None:
        for widget in entry._batch_login_row_widgets:
            widget.grid() if visible else widget.grid_remove()

    def _open_input_file(self) -> None:
        selected = filedialog.askopenfilename(
            title="打开账号文本",
            filetypes=[("文本文件", "*.txt"), ("所有文件", "*.*")],
            parent=self.root,
        )
        if not selected:
            return
        try:
            text = Path(selected).read_text(encoding="utf-8-sig")
        except OSError as error:
            messagebox.showerror(
                "读取失败", redact_text(str(error)), parent=self.root
            )
            return
        self.input_path = selected
        self.input_text.delete("1.0", "end")
        self.input_text.insert("1.0", text)

    def _choose_credential_path(self) -> None:
        selected = filedialog.asksaveasfilename(
            title="选择完整凭据 JSON",
            defaultextension=".json",
            filetypes=[("JSON", "*.json")],
            parent=self.root,
        )
        if selected:
            self.credential_path_var.set(selected)

    def _choose_oidc_export_directory(self) -> None:
        selected = filedialog.askdirectory(
            title="选择 OIDC JSON 导出目录",
            parent=self.root,
        )
        if selected:
            self.oidc_export_directory_var.set(selected)

    def _selected_oidc_export_mode(self) -> OidcExportMode:
        label = self.oidc_export_mode_var.get()
        try:
            return self.OIDC_EXPORT_VALUES[label]
        except KeyError as error:
            raise ValueError("OIDC 导出方式无效") from error

    def _choose_checkpoint_path(self) -> None:
        selected = filedialog.asksaveasfilename(
            title="选择 Checkpoint JSONL",
            defaultextension=".jsonl",
            filetypes=[("JSONL", "*.jsonl")],
            parent=self.root,
        )
        if selected:
            self.checkpoint_path_var.set(selected)

    def _choose_identity_file(self) -> None:
        selected = filedialog.askopenfilename(
            title="选择 SSH 私钥",
            filetypes=[("所有文件", "*.*")],
            parent=self.root,
        )
        if selected:
            self.identity_file_var.set(selected)

    def _render_preview(self, result: ParseResult) -> None:
        self.preview.delete(*self.preview.get_children())
        issues = {issue.line_number: issue for issue in result.issues}
        by_line = {entry.line_number: entry for entry in result.entries}
        for line_number in sorted(set(issues) | set(by_line)):
            entry = by_line.get(line_number)
            issue = issues.get(line_number)
            password = ""
            if entry is not None:
                password = (
                    entry.password
                    if self.show_password_var.get()
                    else "•" * min(max(len(entry.password), 6), 16)
                )
            self.preview.insert(
                "",
                "end",
                values=(
                    line_number,
                    entry.account if entry else "",
                    password,
                    entry.start_url if entry else "",
                    "有效" if entry else issue.code,
                    "" if issue is None else issue.message,
                ),
            )

    def _convert_preview(self) -> None:
        try:
            result = parse_accounts(
                self.input_text.get("1.0", "end-1c"),
                self.input_template_var.get(),
                LoginMode(self.mode_var.get()),
            )
        except ValueError as error:
            messagebox.showerror(
                "解析规则无效", str(error), parent=self.root
            )
            return
        self.last_result = result
        self.entries = result.entries
        self._render_preview(result)
        per_entry_urls = {
            entry.start_url for entry in result.entries if entry.start_url
        }
        portal_status = ""
        if len(per_entry_urls) == 1:
            self.start_url_var.set(next(iter(per_entry_urls)))
        elif len(per_entry_urls) > 1:
            portal_status = "，按每行企业门户登录"
        self.status_var.set(
            f"已解析 {len(result.entries)} 个账号，{len(result.issues)} 个提示"
            f"{portal_status}"
        )

    def _render_last_preview(self) -> None:
        self._render_preview(self.last_result)

    def _copy_output(self) -> None:
        try:
            text = render_accounts(
                self.entries,
                self.output_template_var.get(),
            )
        except ValueError as error:
            messagebox.showerror("输出规则无效", str(error), parent=self.root)
            return
        self.root.clipboard_clear()
        self.root.clipboard_append(text)
        self.status_var.set("统一格式已复制到剪贴板")

    def _save_output(self) -> None:
        selected = filedialog.asksaveasfilename(
            title="保存统一账号文本",
            defaultextension=".txt",
            filetypes=[("文本文件", "*.txt")],
            parent=self.root,
        )
        if not selected:
            return
        try:
            rendered = render_accounts(
                self.entries,
                self.output_template_var.get(),
            )
            Path(selected).write_text(
                rendered + "\n",
                encoding="utf-8",
                newline="\n",
            )
        except (OSError, ValueError) as error:
            messagebox.showerror(
                "保存失败", redact_text(str(error)), parent=self.root
            )

    @staticmethod
    def _port(value: str, *, optional: bool = False) -> int | None:
        stripped = value.strip()
        if optional and not stripped:
            return None
        try:
            return int(stripped)
        except ValueError as error:
            raise ValueError("端口必须是整数") from error

    def _collect_form(self) -> GuiFormState:
        return GuiFormState(
            mode=LoginMode(self.mode_var.get()),
            input_template=self.input_template_var.get(),
            output_template=self.output_template_var.get(),
            start_url=self.start_url_var.get(),
            password_vault_path=self.password_vault_path_var.get(),
            region=self.region_var.get(),
            headless=self.headless_var.get(),
            timeout_seconds=self.timeout_var.get(),
            mfa_timeout_seconds=self.mfa_timeout_var.get(),
            result_mode=ResultMode(self.result_mode_var.get()),
            input_path=self.input_path,
            credential_path=self.credential_path_var.get(),
            checkpoint_path=self.checkpoint_path_var.get(),
            resume=self.resume_var.get(),
            rs_url=self.rs_url_var.get(),
            admin_key=self.admin_key_var.get(),
            use_ssh=self.use_ssh_var.get(),
            ssh_host=self.ssh_host_var.get(),
            ssh_user=self.ssh_user_var.get(),
            ssh_port=self._port(self.ssh_port_var.get()),
            identity_file=self.identity_file_var.get(),
            remote_host=self.remote_host_var.get(),
            remote_port=self._port(self.remote_port_var.get()),
            local_port=self._port(
                self.local_port_var.get(),
                optional=True,
            ),
            oidc_export_mode=self._selected_oidc_export_mode(),
            oidc_export_directory=self.oidc_export_directory_var.get(),
            create_api_key=self.create_api_key_var.get(),
            api_key_skip_if_exists=self.api_key_skip_if_exists_var.get(),
        )

    def _start(self) -> None:
        self._convert_preview()
        fatal = [
            issue
            for issue in self.last_result.issues
            if issue.code != "duplicate_input"
        ]
        if fatal or not self.entries:
            messagebox.showerror(
                "无法开始",
                "请先修正账号解析错误",
                parent=self.root,
            )
            return
        raw_path = self.credential_path_var.get().strip()
        if raw_path and Path(raw_path).exists():
            choice = messagebox.askyesnocancel(
                "凭据文件已存在",
                "选择“是”将读取现有文件并去重追加；选择“否”可另存新文件。不会静默覆盖。",
                parent=self.root,
            )
            if choice is None:
                return
            if choice is False:
                self._choose_credential_path()
                if not self.credential_path_var.get().strip():
                    return
        if not messagebox.askokcancel(
            "敏感文件提示",
            "完整凭据 JSON 将包含 access/refresh token。请勿上传、截图或提交 Git。",
            parent=self.root,
        ):
            return
        try:
            self.controller.start(self.entries, self._collect_form())
        except (ValueError, RuntimeError, tk.TclError) as error:
            messagebox.showerror("无法开始", str(error), parent=self.root)
            return
        self.progress_var.set(0)
        self._set_running(True)

    def _import_existing(self) -> None:
        self.result_mode_var.set(ResultMode.SAVE_AND_IMPORT.value)
        self._apply_mode_visibility()
        selected = filedialog.askopenfilename(
            title="选择已有完整凭据 JSON",
            filetypes=[("JSON", "*.json"), ("所有文件", "*.*")],
            parent=self.root,
        )
        if not selected:
            return
        self.credential_path_var.set(selected)
        if not messagebox.askokcancel(
            "敏感文件提示",
            "将读取包含 access/refresh token 的完整凭据 JSON 并导入 RS。",
            parent=self.root,
        ):
            return
        try:
            self.controller.import_existing(self._collect_form())
        except (ValueError, RuntimeError, tk.TclError) as error:
            messagebox.showerror("无法导入", str(error), parent=self.root)
            return
        self.progress_var.set(0)
        self._set_running(True)

    def _export_existing(self) -> None:
        selected = filedialog.askopenfilename(
            title="选择已有完整凭据 JSON",
            filetypes=[("JSON", "*.json"), ("所有文件", "*.*")],
            parent=self.root,
        )
        if not selected:
            return
        self.credential_path_var.set(selected)
        if not self.oidc_export_directory_var.get().strip():
            self._choose_oidc_export_directory()
            if not self.oidc_export_directory_var.get().strip():
                return
        if not messagebox.askokcancel(
            "敏感文件提示",
            "OIDC JSON 将包含 refresh token 和可能存在的 client secret。请勿上传、截图或提交 Git。",
            parent=self.root,
        ):
            return
        try:
            self.controller.export_existing(self._collect_form())
        except (ValueError, RuntimeError, tk.TclError) as error:
            messagebox.showerror(
                "无法转换", redact_text(str(error)), parent=self.root
            )
            return
        self.progress_var.set(0)
        self._set_running(True)

    def _poll_events(self) -> None:
        for event in self.controller.drain_events():
            self._handle_event(event)
        self.root.after(self.POLL_MS, self._poll_events)

    def _handle_event(self, event: WorkerEvent) -> None:
        payload = event.payload
        if event.kind == "account_started":
            index = int(payload["index"])
            total = int(payload["total"])
            self.progress_var.set(index * 100 / max(total, 1))
            self.status_var.set(
                f"正在处理 {payload['accountMasked']}（{index}/{total}）"
            )
        elif event.kind == "browser_stage":
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
            message = labels.get(stage, f"登录阶段：{stage}")
            self.status_var.set(message)
            self._append_log(message)
        elif event.kind == "manual_action_required":
            self.status_var.set(
                str(payload.get("message") or "等待人工验证")
            )
        elif event.kind == "security_warning":
            self._append_log(
                str(payload.get("message") or "敏感文件权限需要检查")
            )
        elif event.kind in {"account_finished", "import_event"}:
            self._append_log(json.dumps(payload, ensure_ascii=False))
        elif event.kind in {"batch_finished", "batch_cancelled"}:
            self._append_log(json.dumps(payload, ensure_ascii=False))
            self.progress_var.set(100 if event.kind == "batch_finished" else 0)
            self.status_var.set(
                "任务完成"
                if event.kind == "batch_finished"
                else "任务已取消"
            )
            self._set_running(False)
        elif event.kind == "oidc_exported":
            message = (
                f"OIDC 导出完成：{payload.get('count', 0)} 个账号，"
                f"{payload.get('fileCount', 0)} 个文件，目录 {payload.get('directory', '')}"
            )
            self._append_log(message)
            self.status_var.set("OIDC 导出完成")
            self.progress_var.set(100)
            self._set_running(False)
        elif event.kind == "fatal_error":
            self._append_log(str(payload.get("message") or "任务失败"))
            self.status_var.set("任务失败")
            self._set_running(False)

    def _append_log(self, message: str) -> None:
        self.log_text.configure(state="normal")
        self.log_text.insert("end", redact_text(message) + "\n")
        self.log_text.see("end")
        self.log_text.configure(state="disabled")

    def _set_running(self, running: bool) -> None:
        state = "disabled" if running else "normal"
        for widget in self.run_sensitive:
            try:
                widget.configure(state=state)
            except tk.TclError:
                continue
        self.stop_button.configure(state="normal" if running else "disabled")
        if not running:
            self._apply_mode_visibility()

    def _on_close(self) -> None:
        if self.controller.thread is not None and self.controller.thread.is_alive():
            if not messagebox.askyesno(
                "任务仍在运行",
                "停止任务并退出？",
                parent=self.root,
            ):
                return
            self.controller.cancel()
            self._wait_worker_then_close()
            return
        self.root.destroy()

    def _wait_worker_then_close(self) -> None:
        thread = self.controller.thread
        if thread is not None and thread.is_alive():
            self.root.after(100, self._wait_worker_then_close)
        else:
            self.root.destroy()
