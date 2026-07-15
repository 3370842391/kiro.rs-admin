from __future__ import annotations

import asyncio
import re
from collections.abc import Callable
from contextlib import asynccontextmanager
from dataclasses import dataclass
from urllib.parse import urlsplit
from typing import Any

from playwright.async_api import (
    Browser,
    BrowserContext,
    Error as PlaywrightError,
    Page,
)


@dataclass(slots=True)
class BrowserFlowError(Exception):
    code: str
    stage: str
    retryable: bool
    message: str

    def __str__(self) -> str:
        return self.message


class BrowserFlows:
    def __init__(
        self,
        browser: Browser,
        *,
        timeout_seconds: float,
        mfa_timeout_seconds: float,
        event_sink: Callable[[dict[str, Any]], None] | None = None,
    ):
        if timeout_seconds <= 0 or mfa_timeout_seconds <= 0:
            raise ValueError("浏览器超时必须大于 0")
        self.browser = browser
        self.timeout_seconds = timeout_seconds
        self.mfa_timeout_seconds = mfa_timeout_seconds
        self.event_sink = event_sink

    @asynccontextmanager
    async def account_context(self):
        context = await self.browser.new_context()
        page = await context.new_page()
        try:
            yield AccountBrowserSession(
                context,
                page,
                timeout_seconds=self.timeout_seconds,
                mfa_timeout_seconds=self.mfa_timeout_seconds,
                event_sink=self.event_sink,
            )
        finally:
            await context.close()


class AccountBrowserSession:
    ACCOUNT_NAMES = re.compile(
        r"用户名|账号|电子邮件|邮箱|email|username|user name",
        re.I,
    )
    PASSWORD_NAMES = re.compile(r"密码|password", re.I)
    NEXT_NAMES = re.compile(r"下一步|继续|打开|next|continue|open", re.I)
    SIGNIN_NAMES = re.compile(r"登录|登入|sign in|log in|submit", re.I)
    CONSENT_NAMES = re.compile(r"同意|允许|接受|accept|allow", re.I)
    DECLINE_PERSIST_NAMES = re.compile(
        r"否|不保持登录|no|do not stay signed in",
        re.I,
    )
    MFA_TEXT = re.compile(
        r"验证码|验证身份|多重身份|批准登录|authenticator|verification code|two.factor|mfa",
        re.I,
    )
    CAPTCHA_TEXT = re.compile(
        r"验证码图片|人机验证|captcha|verify you are human",
        re.I,
    )
    INVALID_TEXT = re.compile(
        r"密码不正确|账号或密码错误|无法验证.*登录凭证|"
        r"incorrect password|invalid credentials|"
        r"couldn.t verify.*sign-in credentials",
        re.I,
    )
    LOCKED_TEXT = re.compile(r"账号.*锁定|account.*locked", re.I)
    PASSWORD_RESET_TEXT = re.compile(
        r"设置新密码|新密码|set new password|new password|change password",
        re.I,
    )
    AUTHORIZATION_TEXT = re.compile(
        r"authorization requested|confirm this code matches|授权请求|确认此代码",
        re.I,
    )
    DEVICE_CODE_TEXT = re.compile(
        r"\b[A-Z0-9]{4,}(?:-[A-Z0-9]{4,})+\b",
        re.I,
    )
    SUCCESS_TEXT = re.compile(
        r"授权成功|请求已批准|可以关闭此窗口|"
        r"authorization (?:was )?successful|request approved|"
        r"you (?:can|may) close (?:this )?window",
        re.I,
    )
    PASSWORD_KEY_DELAY_MS = 35
    PASSWORD_SUBMIT_PAUSE_SECONDS = 0.8

    def __init__(
        self,
        context: BrowserContext,
        page: Page,
        *,
        timeout_seconds: float,
        mfa_timeout_seconds: float,
        event_sink: Callable[[dict[str, Any]], None] | None = None,
    ):
        self.context = context
        self.page = page
        self.timeout_ms = int(timeout_seconds * 1000)
        self.mfa_timeout_ms = int(mfa_timeout_seconds * 1000)
        self.event_sink = event_sink

    def _emit(self, kind: str, **payload: Any) -> None:
        if self.event_sink is not None:
            self.event_sink({"kind": kind, **payload})

    def _emit_stage(self, stage: str) -> None:
        """Emit a safe workflow stage without exposing authentication data."""
        self._emit("browser_stage", stage=stage)

    async def _first_visible(self, locators):
        for locator in locators:
            try:
                if await locator.first.is_visible(timeout=150):
                    return locator.first
            except PlaywrightError:
                continue
        return None

    async def _fill_account(self, account: str) -> bool:
        locator = await self._first_visible(
            [
                self.page.get_by_label(self.ACCOUNT_NAMES),
                self.page.get_by_role("textbox", name=self.ACCOUNT_NAMES),
                self.page.locator(
                    "input[name='loginfmt'], input[name='username'], "
                    "input[name='email'], input[type='email']"
                ),
            ]
        )
        if locator is None:
            return False
        await locator.fill(account)
        return True

    async def _fill_password(self, password: str) -> bool:
        locator = await self._first_visible(
            [
                self.page.get_by_label(self.PASSWORD_NAMES),
                self.page.locator(
                    "input[name='passwd'], input[name='password'], "
                    "input[type='password']"
                ),
            ]
        )
        if locator is None:
            return False
        await self._type_value(locator, password)
        return True

    async def _type_value(self, locator, value: str) -> None:
        await locator.fill("")
        await locator.press_sequentially(
            value,
            delay=self.PASSWORD_KEY_DELAY_MS,
        )

    async def _fill_password_reset(self, new_password: str) -> bool:
        self._emit_stage("password_reset")
        new_locator = await self._first_visible(
            [
                self.page.get_by_label(
                    re.compile(r"新密码|new password", re.I)
                ),
                self.page.locator(
                    "input[name='newPassword'], input[name='new_password']"
                ),
            ]
        )
        confirm_locator = await self._first_visible(
            [
                self.page.get_by_label(
                    re.compile(r"确认密码|confirm password|re-enter", re.I)
                ),
                self.page.locator(
                    "input[name='confirmPassword'], "
                    "input[name='confirmation'], "
                    "input[name='confirm_password']"
                ),
            ]
        )
        if new_locator is None or confirm_locator is None:
            password_inputs = self.page.locator("input[type='password']")
            if await password_inputs.count() < 2:
                return False
            new_locator = password_inputs.nth(0)
            confirm_locator = password_inputs.nth(1)
        await self._type_value(new_locator, new_password)
        await self._type_value(confirm_locator, new_password)
        return True

    async def _click_password_reset(self) -> bool:
        locator = await self._first_visible(
            [
                self.page.get_by_role(
                    "button",
                    name=re.compile(
                        r"设置新密码|set new password|change password|"
                        r"继续|continue",
                        re.I,
                    ),
                ),
                self.page.locator("button[type='submit'], input[type='submit']"),
            ]
        )
        if locator is None:
            return False
        await locator.click()
        return True

    async def _confirm_device_authorization(self, user_code: str) -> bool:
        body = await self._body_text()
        if not self.AUTHORIZATION_TEXT.search(body):
            return False
        self._emit_stage("device_authorization")
        displayed = self.DEVICE_CODE_TEXT.search(body)
        if displayed is None:
            raise BrowserFlowError(
                "device_code_missing",
                "device_authorization",
                False,
                "授权确认页未显示设备码",
            )
        expected_code = re.sub(r"[-\s]", "", user_code).upper()
        actual_code = re.sub(r"[-\s]", "", displayed.group(0)).upper()
        if expected_code != actual_code:
            raise BrowserFlowError(
                "device_code_mismatch",
                "device_authorization",
                False,
                "授权确认页设备码不匹配",
            )
        locator = await self._first_visible(
            [
                self.page.get_by_role(
                    "button",
                    name=re.compile(
                        r"Confirm and continue|确认并继续|确认",
                        re.I,
                    ),
                ),
                self.page.locator("button[type='submit'], input[type='submit']"),
            ]
        )
        if locator is None:
            raise BrowserFlowError(
                "authorization_confirm_missing",
                "device_authorization",
                False,
                "授权确认页缺少继续按钮",
            )
        await locator.click()
        await asyncio.sleep(0.2)
        return True

    async def _click_primary(self, password_stage: bool) -> bool:
        names = self.SIGNIN_NAMES if password_stage else self.NEXT_NAMES
        locator = await self._first_visible(
            [
                self.page.get_by_role("button", name=names),
                self.page.get_by_role("link", name=names),
                self.page.locator("button[type='submit'], input[type='submit']"),
            ]
        )
        if locator is None:
            return False
        await locator.click()
        return True

    async def _click_progress_without_credentials(self) -> bool:
        locator = await self._first_visible(
            [
                self.page.get_by_role("button", name=self.DECLINE_PERSIST_NAMES),
                self.page.get_by_role("button", name=self.CONSENT_NAMES),
                self.page.get_by_role("button", name=self.NEXT_NAMES),
                self.page.get_by_role("link", name=self.NEXT_NAMES),
            ]
        )
        if locator is None:
            return False
        await locator.click()
        return True

    async def _body_text(self) -> str:
        try:
            return await self.page.locator("body").inner_text(timeout=1000)
        except PlaywrightError:
            return ""

    async def _wait_for_manual_step(self, body: str, code: str) -> None:
        self._emit(
            "manual_action_required",
            manualKind="captcha" if code == "captcha_required" else "mfa",
            message="请在当前浏览器窗口完成验证",
        )
        try:
            await self.page.wait_for_function(
                "previous => document.body.innerText !== previous",
                arg=body,
                timeout=self.mfa_timeout_ms,
            )
        except PlaywrightError as error:
            message = (
                "等待人工完成验证码超时"
                if code == "captcha_required"
                else "等待人工完成 MFA 超时"
            )
            raise BrowserFlowError(code, "mfa", False, message) from error

    async def _drive_login(
        self,
        account: str,
        password: str,
        callback_future=None,
        new_password: str | None = None,
    ) -> None:
        deadline = asyncio.get_running_loop().time() + self.timeout_ms / 1000
        account_filled = False
        password_filled = False
        password_reset_handled = False
        while asyncio.get_running_loop().time() < deadline:
            if callback_future is not None and callback_future.done():
                return

            body = await self._body_text()
            if self.INVALID_TEXT.search(body):
                raise BrowserFlowError(
                    "invalid_credentials",
                    "browser_login",
                    False,
                    "账号或密码错误",
                )
            if self.LOCKED_TEXT.search(body):
                raise BrowserFlowError(
                    "account_locked",
                    "browser_login",
                    False,
                    "账号已锁定",
                )
            if self.PASSWORD_RESET_TEXT.search(body) and not password_reset_handled:
                if not new_password:
                    raise BrowserFlowError(
                        "new_password_required",
                        "password_reset",
                        False,
                        "账号要求设置新密码，但未配置新密码",
                    )
                if not await self._fill_password_reset(new_password):
                    raise BrowserFlowError(
                        "password_reset_page_unknown",
                        "password_reset",
                        False,
                        "无法识别设置新密码页面",
                    )
                password_reset_handled = True
                await asyncio.sleep(self.PASSWORD_SUBMIT_PAUSE_SECONDS)
                if not await self._click_password_reset():
                    raise BrowserFlowError(
                        "password_reset_submit_missing",
                        "password_reset",
                        False,
                        "设置新密码页面缺少提交按钮",
                    )
                await asyncio.sleep(0.2)
                continue
            if (
                (password_filled or password_reset_handled)
                and callback_future is None
                and self.SUCCESS_TEXT.search(body)
            ):
                self._emit_stage("complete")
                return

            manual_code = (
                "captcha_required"
                if self.CAPTCHA_TEXT.search(body)
                else "mfa_timeout"
                if self.MFA_TEXT.search(body)
                else None
            )
            if manual_code:
                await self._wait_for_manual_step(body, manual_code)
                continue

            if not account_filled and await self._fill_account(account):
                self._emit_stage("username")
                account_filled = True
                await self._click_primary(False)
                await asyncio.sleep(0.2)
                continue

            if not password_filled and await self._fill_password(password):
                self._emit_stage("password")
                password_filled = True
                await asyncio.sleep(self.PASSWORD_SUBMIT_PAUSE_SECONDS)
                await self._click_primary(True)
                await asyncio.sleep(0.2)
                continue

            if (
                callback_future is not None
                and await self._click_progress_without_credentials()
            ):
                await asyncio.sleep(0.2)
                continue

            await asyncio.sleep(0.2)

        raise BrowserFlowError(
            "unknown_page",
            "browser_login",
            False,
            "登录页面在超时前未完成",
        )

    async def complete_enterprise(
        self,
        url: str,
        account: str,
        password: str,
        user_code: str | None = None,
        new_password: str | None = None,
    ) -> None:
        try:
            self._emit_stage("portal_init")
            await self.page.goto(
                url,
                wait_until="domcontentloaded",
                timeout=self.timeout_ms,
            )
            if user_code:
                if not await self._fill_device_code(user_code):
                    await self._confirm_device_authorization(user_code)
            await self._drive_login(
                account,
                password,
                new_password=new_password,
            )
        except BrowserFlowError:
            raise
        except PlaywrightError as error:
            raise BrowserFlowError(
                "browser_navigation_failed",
                "browser_login",
                True,
                "无法打开登录页面",
            ) from error

    async def _fill_device_code(self, user_code: str) -> bool:
        locator = await self._first_visible(
            [
                self.page.get_by_label(
                    re.compile(r"设备码|用户码|device code|user code", re.I)
                ),
                self.page.locator(
                    "input[name='userCode'], input[name='user_code'], "
                    "input[autocomplete='one-time-code']"
                ),
            ]
        )
        if locator is None:
            return False
        self._emit_stage("device_authorization")
        await locator.fill(user_code)
        await self._click_primary(False)
        await asyncio.sleep(0.2)
        return True

    async def capture_callback(
        self,
        url: str,
        account: str,
        password: str,
        *,
        expected_path: str,
    ) -> str:
        if not expected_path.startswith("/"):
            raise ValueError("expected_path 必须是绝对 URL path")
        loop = asyncio.get_running_loop()
        callback = loop.create_future()

        def observe(request):
            if urlsplit(request.url).path == expected_path and not callback.done():
                callback.set_result(request.url)

        self.context.on("request", observe)
        try:
            try:
                await self.page.goto(
                    url,
                    wait_until="domcontentloaded",
                    timeout=self.timeout_ms,
                )
            except PlaywrightError as error:
                if not callback.done():
                    raise BrowserFlowError(
                        "browser_navigation_failed",
                        "browser_login",
                        True,
                        "无法打开登录页面",
                    ) from error
            await self._drive_login(account, password, callback)
            try:
                return await asyncio.wait_for(
                    callback,
                    timeout=self.timeout_ms / 1000,
                )
            except TimeoutError as error:
                raise BrowserFlowError(
                    "callback_timeout",
                    "browser_callback",
                    False,
                    "登录回调等待超时",
                ) from error
        finally:
            self.context.remove_listener("request", observe)
