"""Web API client — all communication between Local Agent and Talos Web."""

from __future__ import annotations

import logging
from typing import Any

import httpx

from talos_agent.config import Settings
from talos_agent.http import request_with_retry
from talos_agent.idempotency import (
    IdempotencyConflictError,
    generate_idempotency_key,
    is_payload_conflict,
    validate_idempotency_key,
)

log = logging.getLogger(__name__)

# Sentinel: caller explicitly opted out of idempotency
_NO_KEY = object()


def _inject_key(
    idempotency_key: Any,  # str | None | _NO_KEY
) -> str | None:
    """Resolve the effective idempotency key for a write call.

    - ``_NO_KEY`` sentinel  → auto-generate a fresh UUID v4.
    - ``None``             → opt-out; no key injected.
    - Any string           → use as-is after validation.
    """
    if idempotency_key is _NO_KEY:
        return generate_idempotency_key()
    if idempotency_key is None:
        return None
    return validate_idempotency_key(idempotency_key)


def _check_idempotency_conflict(
    response: httpx.Response,
    key: str | None,
    path: str,
) -> None:
    """Raise :class:`IdempotencyConflictError` if the 409 is a payload conflict."""
    if response.status_code != 409 or key is None:
        return
    try:
        body = response.text
    except Exception:
        body = ""
    if is_payload_conflict(body):
        raise IdempotencyConflictError(key=key, path=path, body=body)


class TalosAPIClient:
    def __init__(self, settings: Settings):
        self._base = settings.talos_api_url.rstrip("/")
        self._talos_id = settings.talos_id
        self._client = httpx.AsyncClient(
            base_url=self._base,
            headers={
                "Authorization": f"Bearer {settings.talos_api_key}",
                "Content-Type": "application/json",
            },
            timeout=30.0,
        )

    # ── Retry-wrapped HTTP verbs ──────────────────────────

    async def _get(self, url: str, **kwargs: Any) -> httpx.Response:
        return await request_with_retry(lambda: self._client.get(url, **kwargs))

    async def _post(
        self,
        url: str,
        *,
        idempotency_key: Any = _NO_KEY,
        **kwargs: Any,
    ) -> httpx.Response:
        """POST with automatic idempotency key injection.

        Parameters
        ----------
        idempotency_key:
            - Omitted / ``_NO_KEY`` → a fresh UUID v4 is generated and injected.
            - ``None`` → no key is injected (opt-out).
            - Any string → used directly after validation.
        """
        key = _inject_key(idempotency_key)
        headers: dict[str, str] = dict(kwargs.pop("headers", {}) or {})
        if key is not None:
            headers["Idempotency-Key"] = key
            log.debug("idempotency_key_injected key=%s method=POST url=%s", key, url)

        response = await request_with_retry(
            lambda: self._client.post(url, headers=headers, **kwargs)
        )
        _check_idempotency_conflict(response, key, url)
        return response

    async def _put(self, url: str, **kwargs: Any) -> httpx.Response:
        return await request_with_retry(lambda: self._client.put(url, **kwargs))

    async def _patch(
        self,
        url: str,
        *,
        idempotency_key: Any = _NO_KEY,
        **kwargs: Any,
    ) -> httpx.Response:
        """PATCH with automatic idempotency key injection (same semantics as _post)."""
        key = _inject_key(idempotency_key)
        headers: dict[str, str] = dict(kwargs.pop("headers", {}) or {})
        if key is not None:
            headers["Idempotency-Key"] = key
            log.debug("idempotency_key_injected key=%s method=PATCH url=%s", key, url)

        response = await request_with_retry(
            lambda: self._client.patch(url, headers=headers, **kwargs)
        )
        _check_idempotency_conflict(response, key, url)
        return response

    # ── Talos Config ──────────────────────────────────────

    async def get_talos(self, talos_id: str) -> dict | None:
        r = await self._get(f"/api/talos/{talos_id}")
        if r.status_code == 200:
            return r.json()
        return None

    async def get_talos_me(self) -> dict | None:
        """Resolve Talos from API key — no Talos ID needed."""
        r = await self._get("/api/talos/me")
        if r.status_code == 200:
            return r.json()
        return None

    # ── Activity Reporting ─────────────────────────────────

    async def report_activity(
        self,
        talos_id: str,
        *,
        type_: str,
        content: str,
        channel: str,
        idempotency_key: str | None = _NO_KEY,  # type: ignore[assignment]
    ) -> dict | None:
        r = await self._post(
            f"/api/talos/{talos_id}/activity",
            json={"type": type_, "content": content, "channel": channel},
            idempotency_key=idempotency_key,
        )
        if r.status_code in (200, 201):
            return r.json()
        return None

    # ── Status ─────────────────────────────────────────────

    async def update_status(self, talos_id: str, *, online: bool) -> None:
        # Status updates are fire-and-forget; no idempotency key needed.
        await self._patch(
            f"/api/talos/{talos_id}/status",
            json={"agentOnline": online},
            idempotency_key=None,
        )

    # ── Revenue ────────────────────────────────────────────

    async def report_revenue(
        self,
        talos_id: str,
        *,
        amount: float,
        source: str,
        tx_hash: str | None = None,
        idempotency_key: str | None = _NO_KEY,  # type: ignore[assignment]
    ) -> dict | None:
        r = await self._post(
            f"/api/talos/{talos_id}/revenue",
            json={"amount": amount, "currency": "USDC", "source": source, "txHash": tx_hash},
            idempotency_key=idempotency_key,
        )
        if r.status_code in (200, 201):
            return r.json()
        return None

    # ── Approvals ──────────────────────────────────────────

    async def create_approval(
        self,
        talos_id: str,
        *,
        type_: str,
        title: str,
        description: str | None = None,
        amount: float | None = None,
        idempotency_key: str | None = _NO_KEY,  # type: ignore[assignment]
    ) -> dict | None:
        r = await self._post(
            f"/api/talos/{talos_id}/approvals",
            json={"type": type_, "title": title, "description": description, "amount": amount},
            idempotency_key=idempotency_key,
        )
        if r.status_code in (200, 201):
            return r.json()
        return None

    async def get_approvals(self, talos_id: str, status: str | None = None) -> list[dict]:
        params: dict[str, Any] = {}
        if status:
            params["status"] = status
        r = await self._get(f"/api/talos/{talos_id}/approvals", params=params)
        if r.status_code == 200:
            data = r.json()
            return data if isinstance(data, list) else data.get("approvals", [])
        return []

    async def get_approval(self, talos_id: str, approval_id: str) -> dict | None:
        r = await self._get(f"/api/talos/{talos_id}/approvals/{approval_id}")
        if r.status_code == 200:
            return r.json()
        return None

    # ── Agent Wallet (Circle MPC) ────────────────────────────

    async def get_agent_wallet(self) -> dict | None:
        """Fetch agent wallet info (walletId, address) from Web."""
        r = await self._get(f"/api/talos/{self._talos_id}/wallet")
        if r.status_code == 200:
            return r.json()
        return None

    async def create_agent_wallet(self) -> dict | None:
        """Create a Circle MPC wallet for this Talos if one doesn't exist."""
        r = await self._post(f"/api/talos/{self._talos_id}/wallet", idempotency_key=None)
        if r.status_code in (200, 201):
            return r.json()
        return None

    async def sign_payment(
        self,
        *,
        payee: str,
        amount: int,
        asset_code: str = "USDC",
        asset_issuer: str | None = None,
    ) -> dict | None:
        """Request x402 payment signature from Web's Stellar proxy."""
        if not self._talos_id:
            return {"error": "talos_id not set"}
        payload: dict[str, Any] = {"payee": payee, "amount": amount, "assetCode": asset_code}
        if asset_issuer:
            payload["assetIssuer"] = asset_issuer
        # sign_payment is not a state-mutating write; opt out of idempotency.
        r = await self._post(f"/api/talos/{self._talos_id}/sign", json=payload, idempotency_key=None)
        if r.status_code == 200:
            return r.json()
        # Return error details
        try:
            return r.json()
        except Exception:
            return {"error": f"Sign request failed with status {r.status_code}"}

    # ── Commerce / x402 ────────────────────────────────────

    async def get_service(self, talos_id: str, service_type: str | None = None) -> httpx.Response:
        """GET service endpoint — expects 402 response with payment details."""
        params = {}
        if service_type:
            params["type"] = service_type
        return await self._get(f"/api/talos/{talos_id}/service", params=params)

    async def submit_commerce(
        self,
        talos_id: str,
        *,
        payment_header: str,
        payload: dict | None = None,
        idempotency_key: str | None = _NO_KEY,  # type: ignore[assignment]
    ) -> dict | None:
        """POST with x402 payment signature to purchase service."""
        r = await self._post(
            f"/api/talos/{talos_id}/service",
            json={"payload": payload},
            headers={"X-PAYMENT": payment_header},
            idempotency_key=idempotency_key,
        )
        if r.status_code in (200, 201):
            return r.json()
        # Return error details instead of None
        try:
            return r.json()
        except Exception:
            return {"error": f"Commerce submission failed with status {r.status_code}"}

    async def discover_services(
        self, category: str | None = None, target: str | None = None
    ) -> list[dict]:
        params: dict[str, Any] = {"self": self._talos_id}
        if category:
            params["category"] = category
        if target:
            params["target"] = target
        r = await self._get("/api/services", params=params)
        if r.status_code == 200:
            data = r.json()
            return data if isinstance(data, list) else data.get("data", [])
        return []

    async def register_service(
        self,
        talos_id: str,
        *,
        service_name: str,
        description: str,
        price: float,
        wallet_address: str | None = None,
    ) -> dict | None:
        """Register or update this Talos's x402 service on the marketplace."""
        payload: dict[str, Any] = {
            "serviceName": service_name,
            "description": description,
            "price": price,
        }
        if wallet_address:
            payload["walletAddress"] = wallet_address
        r = await self._put(f"/api/talos/{talos_id}/service", json=payload)
        if r.status_code in (200, 201):
            return r.json()
        return None

    # ── Transfers (Stellar) ────────────────────────────────

    async def request_transfer(
        self,
        *,
        to_account: str,
        amount: float,
        currency: str = "XLM",
        token_id: str | None = None,
        idempotency_key: str | None = _NO_KEY,  # type: ignore[assignment]
    ) -> dict | None:
        """Execute XLM or Stellar asset transfer via Web API."""
        payload: dict[str, Any] = {
            "to": to_account,
            "amount": amount,
            "currency": currency,
        }
        if token_id:
            payload["tokenId"] = token_id
        r = await self._post(
            f"/api/talos/{self._talos_id}/transfer",
            json=payload,
            idempotency_key=idempotency_key,
        )
        if r.status_code in (200, 201):
            return r.json()
        try:
            return r.json()
        except Exception:
            return {"error": f"Transfer failed with status {r.status_code}"}

    # ── Jobs ───────────────────────────────────────────────

    async def get_pending_jobs(self) -> list[dict]:
        r = await self._get("/api/jobs/pending")
        if r.status_code == 200:
            data = r.json()
            return data if isinstance(data, list) else data.get("jobs", [])
        return []

    async def claim_job(self, job_id: str, ttl_seconds: int = 300) -> dict | None:
        """Acquire a lease on a pending job. Returns the fencing token on success."""
        # claim_job is idempotent by the server's lease model; auto-inject a key.
        r = await self._post(
            f"/api/jobs/{job_id}/claim",
            json={"ttlSeconds": ttl_seconds},
        )
        if r.status_code == 200:
            return r.json()
        return None

    async def heartbeat_job(self, job_id: str, fencing_token: int) -> dict | None:
        """Extend the lease on a claimed job. Returns renewed expiry on success."""
        r = await self._post(
            f"/api/jobs/{job_id}/heartbeat",
            json={"fencingToken": fencing_token},
        )
        if r.status_code == 200:
            return r.json()
        return None

    async def release_job(self, job_id: str, fencing_token: int) -> dict | None:
        """Release a lease on a claimed job."""
        r = await self._post(
            f"/api/jobs/{job_id}/release",
            json={"fencingToken": fencing_token},
        )
        if r.status_code == 200:
            return r.json()
        return None

    async def submit_job_result(
        self,
        job_id: str,
        result: dict,
        fencing_token: int = 0,
        idempotency_key: str | None = _NO_KEY,  # type: ignore[assignment]
    ) -> dict | None:
        r = await self._post(
            f"/api/jobs/{job_id}/result",
            json={"result": result, "fencingToken": fencing_token},
            idempotency_key=idempotency_key,
        )
        if r.status_code in (200, 201):
            return r.json()
        return None

    async def get_job_result(self, job_id: str) -> dict | None:
        r = await self._get(f"/api/jobs/{job_id}/result")
        if r.status_code == 200:
            return r.json()
        return None

    # ── Playbooks ──────────────────────────────────────────

    async def publish_playbook(
        self,
        *,
        title: str,
        category: str,
        channel: str,
        description: str,
        price: float,
        tags: list[str] | None = None,
        content: dict | None = None,
        impressions: int = 0,
        engagement_rate: float = 0,
        conversions: int = 0,
        period_days: int = 30,
        idempotency_key: str | None = _NO_KEY,  # type: ignore[assignment]
    ) -> dict | None:
        """Publish a Playbook to the marketplace."""
        r = await self._post(
            "/api/playbooks",
            json={
                "title": title,
                "category": category,
                "channel": channel,
                "description": description,
                "price": price,
                "tags": tags or [],
                "content": content,
                "impressions": impressions,
                "engagementRate": engagement_rate,
                "conversions": conversions,
                "periodDays": period_days,
            },
            idempotency_key=idempotency_key,
        )
        if r.status_code in (200, 201):
            return r.json()
        return None

    # ── Dividend Distribution ─────────────────────────────

    async def get_distribution_preview(self, talos_id: str) -> dict | None:
        """Preview dividend distribution without executing."""
        r = await self._client.get(f"/api/talos/{talos_id}/revenue/distribute")
        if r.status_code == 200:
            return r.json()
        return None

    async def distribute_dividends(
        self, talos_id: str, *, requester_public_key: str
    ) -> dict | None:
        """Execute dividend distribution to patrons."""
        r = await self._client.post(
            f"/api/talos/{talos_id}/revenue/distribute",
            json={"requesterPublicKey": requester_public_key},
        )
        if r.status_code in (200, 201):
            return r.json()
        try:
            return r.json()
        except Exception:
            return {"error": f"Distribution failed with status {r.status_code}"}

    # ── Lifecycle ──────────────────────────────────────────

    async def close(self) -> None:
        await self._client.aclose()

    def set_request_id(self, request_id: str) -> None:
        """Propagate cycle_id as X-Request-Id to web API calls."""
        self._client.headers["x-request-id"] = request_id
