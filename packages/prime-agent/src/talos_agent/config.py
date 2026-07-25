"""Configuration via environment variables and ~/.talos-agent/config.json."""

from __future__ import annotations

import json
from decimal import Decimal
from pathlib import Path

from pydantic import Field, PrivateAttr
from pydantic_settings import BaseSettings, SettingsConfigDict

APP_DIR = Path.home() / ".talos-agent"


def _json_config_source() -> dict:
    path = APP_DIR / "config.json"
    if path.exists():
        return json.loads(path.read_text())
    return {}


class Settings(BaseSettings):
    _secret_store: object | None = PrivateAttr(default=None)

    model_config = SettingsConfigDict(
        env_file=".env",
        env_file_encoding="utf-8",
        extra="ignore",
        populate_by_name=True,
    )

    # Talos Web API
    talos_api_url: str = "https://talos-stellar.vercel.app"
    talos_api_key: str = ""
    talos_id: str = ""

    # Multi-agent mode: comma-separated list of API keys
    # e.g. TALOS_API_KEYS=tak_aaa,tak_bbb,tak_ccc
    talos_api_keys: str = ""

    def get_all_api_keys(self) -> list[str]:
        """Return all agent API keys — multi-agent list if set, else single key."""
        if self.talos_api_keys:
            return [k.strip() for k in self.talos_api_keys.split(",") if k.strip()]
        if self.talos_api_key:
            return [self.talos_api_key]
        return []

    # LLM (Groq preferred — free, OpenAI-compatible)
    groq_api_key: str = ""
    groq_model: str = "llama-3.3-70b-versatile"

    # OpenAI (fallback if groq_api_key is not set)
    openai_api_key: str = ""
    openai_model: str = "gpt-4o-mini"

    @property
    def llm_api_key(self) -> str:
        groq_key = self.secret_value("groq_api_key")
        return groq_key or self.secret_value("openai_api_key")

    @property
    def llm_model(self) -> str:
        return self.groq_model if self.secret_value("groq_api_key") else self.openai_model

    @property
    def llm_base_url(self) -> str | None:
        return "https://api.groq.com/openai/v1" if self.secret_value("groq_api_key") else None

    # X (Twitter)
    x_username: str = ""
    x_password: str = ""
    x_email: str = ""

    # Discord
    # Webhook URL is sufficient for posting. Bot token + channel/guild IDs
    # unlock replies, mentions, and analytics via the REST API.
    discord_webhook_url: str = ""
    discord_bot_token: str = ""
    discord_channel_id: str = ""
    discord_guild_id: str = ""

    # Per-channel credential configs for additional adapters.
    # Set as JSON in env: CHANNEL_CONFIGS={"telegram": {"bot_token": "...", "chat_id": "@channel"}}
    channel_configs: dict = Field(default_factory=dict, description="Per-channel credentials map")
    telegram_bot_token: str = ""
    telegram_chat_id: str = ""

    # Versioned encrypted secret rotation (opt-in for backward compatibility).
    secret_rotation_enabled: bool = Field(
        default=False, validation_alias="TALOS_SECRET_ROTATION_ENABLED"
    )
    secret_keyring: str = Field(default="", validation_alias="TALOS_SECRET_KEYRING")
    secret_active_key_id: str = Field(
        default="", validation_alias="TALOS_SECRET_ACTIVE_KEY_ID"
    )
    secret_scope: str = Field(default="default", validation_alias="TALOS_SECRET_SCOPE")
    secret_dual_read: bool = Field(
        default=True, validation_alias="TALOS_SECRET_DUAL_READ"
    )
    secret_legacy_fallback: bool = Field(
        default=True, validation_alias="TALOS_SECRET_LEGACY_FALLBACK"
    )
    secret_max_bytes: int = Field(
        default=65536,
        ge=1,
        le=1048576,
        validation_alias="TALOS_SECRET_MAX_BYTES",
    )
    secret_db_timeout_ms: int = Field(
        default=5000,
        ge=1,
        le=60000,
        validation_alias="TALOS_SECRET_DB_TIMEOUT_MS",
    )

    # Third-party adapter capability sandbox (opt-in rollout).
    adapter_sandbox_enabled: bool = Field(
        default=False, validation_alias="TALOS_ADAPTER_SANDBOX_ENABLED"
    )
    adapter_capability_manifests: str = Field(
        default="", validation_alias="TALOS_ADAPTER_CAPABILITY_MANIFESTS"
    )
    adapter_timeout_seconds: int = Field(
        default=30,
        ge=1,
        le=120,
        validation_alias="TALOS_ADAPTER_TIMEOUT_SECONDS",
    )
    adapter_max_concurrency: int = Field(
        default=2,
        ge=1,
        le=16,
        validation_alias="TALOS_ADAPTER_MAX_CONCURRENCY",
    )
    adapter_max_input_bytes: int = Field(
        default=16384,
        ge=1,
        le=1048576,
        validation_alias="TALOS_ADAPTER_MAX_INPUT_BYTES",
    )
    adapter_max_output_bytes: int = Field(
        default=262144,
        ge=1,
        le=2097152,
        validation_alias="TALOS_ADAPTER_MAX_OUTPUT_BYTES",
    )
    adapter_max_network_requests: int = Field(
        default=8,
        ge=1,
        le=32,
        validation_alias="TALOS_ADAPTER_MAX_NETWORK_REQUESTS",
    )
    adapter_invocation_lease_seconds: int = Field(
        default=120,
        ge=5,
        le=900,
        validation_alias="TALOS_ADAPTER_INVOCATION_LEASE_SECONDS",
    )
    adapter_max_invocation_records: int = Field(
        default=100000,
        ge=100,
        le=1000000,
        validation_alias="TALOS_ADAPTER_MAX_INVOCATION_RECORDS",
    )

    # Agent behaviour
    agent_cycle_interval: int = Field(default=30, description="Seconds between agent cycles")
    polling_interval: int = Field(default=10, description="Seconds between API polls")
    heartbeat_interval: int = Field(default=60, description="Seconds between heartbeats")
    max_iterations: int = Field(default=20, description="Max tool-call iterations per cycle")
    approval_threshold: Decimal = Field(default=Decimal("10"), description="USD threshold for auto-approval")
    browser_headless: bool = Field(default=False, description="Run browser in headless mode")
    auto_repay_loans: bool = Field(default=False, description="Enable automatic loan repayment from treasury")

    # Dividend distribution
    dividend_distribution_interval: int = Field(default=3600, description="Seconds between dividend distribution checks")
    dividend_usdc_threshold: Decimal = Field(default=Decimal("100"), description="USDC threshold to trigger dividend distribution")

    def __init__(self, **kwargs):
        overrides = _json_config_source()
        overrides.update(kwargs)
        super().__init__(**overrides)

    def bind_secret_store(self, store: object) -> None:
        """Attach the runtime resolver after the local database is available."""
        self._secret_store = store

    def secret_value(self, name: str, legacy_value: str | None = None) -> str:
        """Resolve a credential at point of use, preserving legacy defaults."""
        legacy = legacy_value
        if legacy is None:
            value = getattr(self, name, "")
            legacy = value if isinstance(value, str) else ""
        if not self.secret_rotation_enabled or self._secret_store is None:
            return legacy or ""
        resolution = self._secret_store.resolve(name, legacy or "")
        return resolution.value


def ensure_app_dir() -> Path:
    APP_DIR.mkdir(parents=True, exist_ok=True)
    (APP_DIR / "logs").mkdir(exist_ok=True)
    return APP_DIR


def resolve_setting_secret(settings: object, name: str, legacy_value: str | None = None) -> str:
    """Resolve secrets on real Settings while remaining friendly to test doubles."""
    resolver = getattr(type(settings), "secret_value", None)
    if callable(resolver):
        return resolver(settings, name, legacy_value)
    if legacy_value is not None:
        return legacy_value
    value = getattr(settings, name, "")
    return value if isinstance(value, str) else ""
