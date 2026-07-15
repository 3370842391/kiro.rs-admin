from __future__ import annotations

import asyncio
import re
from contextlib import asynccontextmanager
from dataclasses import dataclass
from urllib.parse import urlsplit

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
    ):
        if timeout_seconds <= 0 or mfa_timeout_seconds <= 0:
            raise ValueError("浏览器超时必须大于 0")
        self.browser = browser
        self.timeout_seconds = timeout_seconds
        self.mfa_timeout_seconds = mfa_timeout_seconds

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
        r"密码不正确|账号或密码错误|incorrect password|invalid credentials",
        re.I,
    )
    LOCKED_TEXT = re.compile(r"账号.*锁定|account.*locked", re.I)

    def __init__(
        self,
        context: BrowserContext,
        page: Page,
        *,
        timeout_seconds: float,
        mfa_timeout_seconds: float,
    ):
        self.context = context
        self.page = page
        self.timeout_ms = int(timeout_seconds * 1000)
        self.mfa_timeout_ms = int(mfa_timeout_seconds * 1000)

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
        await locator.fill(password)
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
        print("检测到 MFA/验证码，请在当前浏览器窗口中完成人工验证。")
        try:
            await self.page.wait_for_function(
                "previous => document.body.innerText !== previous",
                body,
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
    ) -> None:
        deadline = asyncio.get_running_loop().time() + self.timeout_ms / 1000
        account_filled = False
        password_filled = False
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
                account_filled = True
                await self._click_primary(False)
                await asyncio.sleep(0.2)
                continue

            if not password_filled and await self._fill_password(password):
                password_filled = True
                await self._click_primary(True)
                await asyncio.sleep(0.2)
                continue

            if (
                callback_future is not None
                and await self._click_progress_without_credentials()
            ):
                await asyncio.sleep(0.2)
                continue

            if password_filled and callback_future is None:
                return
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
    ) -> None:
        try:
            await self.page.goto(
                url,
                wait_until="domcontentloaded",
                timeout=self.timeout_ms,
            )
            await self._drive_login(account, password)
        except BrowserFlowError:
            raise
        except PlaywrightError as error:
            raise BrowserFlowError(
                "browser_navigation_failed",
                "browser_login",
                True,
                "无法打开登录页面",
            ) from error

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
