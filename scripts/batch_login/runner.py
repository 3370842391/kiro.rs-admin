from __future__ import annotations

import asyncio
from contextlib import suppress
from dataclasses import dataclass
from datetime import datetime, timezone
from typing import Any

from .browser_flows import BrowserFlowError
from .models import (
    AccountEntry,
    LoginMode,
    LoginOutcome,
    ResultStatus,
    RunRecord,
)
from .redaction import mask_account, redact_text
from .rs_client import RsApiError


@dataclass(slots=True, frozen=True)
class RunnerSettings:
    region: str
    start_url: str | None = None


def _invalid_response(stage: str, message: str = "RS 登录响应格式无效") -> RsApiError:
    return RsApiError(
        code="invalid_rs_response",
        stage=stage,
        retryable=False,
        status_code=0,
        message=message,
    )


def outcome_from_success(result: dict[str, Any]) -> LoginOutcome:
    if result.get("status") != "success":
        raise _invalid_response("login_complete")
    credential_id = result.get("credentialId")
    if not isinstance(credential_id, int) or isinstance(credential_id, bool):
        raise _invalid_response("login_complete")
    duplicate = result.get("duplicate", False) is True
    return LoginOutcome(
        status=ResultStatus.DUPLICATE if duplicate else ResultStatus.SUCCESS,
        credential_id=credential_id,
        duplicate=duplicate,
    )


def record_from_outcome(
    run_id: str,
    mode: LoginMode,
    entry: AccountEntry,
    outcome: LoginOutcome,
) -> RunRecord:
    return RunRecord(
        run_id=run_id,
        line_number=entry.line_number,
        account_hash=entry.account_hash,
        account_masked=mask_account(entry.account),
        mode=mode,
        status=outcome.status,
        stage=outcome.stage or "done",
        attempts=1,
        timestamp=datetime.now(timezone.utc).isoformat().replace("+00:00", "Z"),
        credential_id=outcome.credential_id,
        code=outcome.code,
        retryable=outcome.retryable,
        message=redact_text(outcome.message or "") or None,
    )


class BatchLoginRunner:
    def __init__(self, client, browser_factory, checkpoint):
        self.client = client
        self.browser_factory = browser_factory
        self.checkpoint = checkpoint

    async def _wait_idc(
        self,
        session_id: str,
        poll_interval: float,
    ) -> dict[str, Any]:
        while True:
            result = await self.client.poll_idc(session_id)
            status = result.get("status")
            if status == "pending":
                await asyncio.sleep(max(poll_interval, 0.2))
                continue
            if status == "expired":
                raise RsApiError(
                    "session_expired",
                    "idc_poll",
                    False,
                    410,
                    "IDC 会话已过期",
                )
            if status != "success":
                raise _invalid_response("idc_poll")
            return result

    async def _run_enterprise(
        self,
        entry: AccountEntry,
        settings: RunnerSettings,
    ) -> LoginOutcome:
        if not settings.start_url:
            raise RsApiError(
                "missing_start_url",
                "config",
                False,
                0,
                "企业登录缺少 start URL",
            )
        started = await self.client.start_idc(
            region=settings.region,
            start_url=settings.start_url,
            email=entry.account,
        )
        session_id = started.get("sessionId")
        verification_url = started.get("verificationUriComplete") or started.get(
            "verificationUri"
        )
        if not isinstance(session_id, str):
            raise _invalid_response("idc_start")

        try:
            if not isinstance(verification_url, str):
                raise _invalid_response("idc_start")
            try:
                poll_interval = float(started.get("pollInterval", 5))
            except (TypeError, ValueError) as error:
                raise _invalid_response("idc_start") from error
            if poll_interval < 0:
                raise _invalid_response("idc_start")

            async with self.browser_factory.account_context() as browser:
                browser_task = asyncio.create_task(
                    browser.complete_enterprise(
                        verification_url,
                        entry.account,
                        entry.password,
                    )
                )
                poll_task = asyncio.create_task(
                    self._wait_idc(
                        session_id,
                        poll_interval,
                    )
                )
                tasks = {browser_task, poll_task}
                try:
                    done, _pending = await asyncio.wait(
                        tasks,
                        return_when=asyncio.FIRST_EXCEPTION,
                    )
                    for task in done:
                        task.result()
                    result = await poll_task
                finally:
                    for task in tasks:
                        if not task.done():
                            task.cancel()
                    await asyncio.gather(*tasks, return_exceptions=True)
            return outcome_from_success(result)
        except BaseException:
            with suppress(Exception):
                await self.client.cancel_idc(session_id)
            raise

    async def _run_microsoft(
        self,
        entry: AccountEntry,
        _settings: RunnerSettings,
    ) -> LoginOutcome:
        started = await self.client.start_social(email=entry.account)
        session_id = started.get("sessionId")
        portal_url = started.get("portalUrl")
        if not isinstance(session_id, str):
            raise _invalid_response("social_start")

        try:
            if not isinstance(portal_url, str):
                raise _invalid_response("social_start")
            async with self.browser_factory.account_context() as browser:
                first = await browser.capture_callback(
                    portal_url,
                    entry.account,
                    entry.password,
                    expected_path="/signin/callback",
                )
                result = await self.client.complete_social(session_id, first)
                if result.get("status") == "continue":
                    next_url = result.get("nextUrl")
                    if not isinstance(next_url, str):
                        raise _invalid_response("social_descriptor")
                    final = await browser.capture_callback(
                        next_url,
                        entry.account,
                        entry.password,
                        expected_path="/oauth/callback",
                    )
                    result = await self.client.complete_social(session_id, final)
                return outcome_from_success(result)
        except BaseException:
            with suppress(Exception):
                await self.client.cancel_social(session_id)
            raise

    async def run_one(
        self,
        mode: LoginMode,
        entry: AccountEntry,
        settings: RunnerSettings,
    ) -> LoginOutcome:
        try:
            if mode is LoginMode.ENTERPRISE:
                return await self._run_enterprise(entry, settings)
            return await self._run_microsoft(entry, settings)
        except (BrowserFlowError, RsApiError) as error:
            status = (
                ResultStatus.MANUAL_REQUIRED
                if error.code in {"mfa_timeout", "captcha_required"}
                else ResultStatus.FAILED
            )
            return LoginOutcome(
                status=status,
                code=error.code,
                stage=error.stage,
                retryable=error.retryable,
                message=str(error),
            )

    async def run_batch(
        self,
        mode: LoginMode,
        entries,
        settings: RunnerSettings,
        *,
        resume: bool,
        run_id: str,
    ) -> list[LoginOutcome]:
        outcomes = []
        for entry in entries:
            if self.checkpoint and not self.checkpoint.should_run(
                entry.line_number,
                entry.account_hash,
                mode,
                resume,
            ):
                continue
            outcome = await self.run_one(mode, entry, settings)
            outcomes.append(outcome)
            if self.checkpoint:
                self.checkpoint.append(
                    record_from_outcome(run_id, mode, entry, outcome)
                )
        return outcomes
