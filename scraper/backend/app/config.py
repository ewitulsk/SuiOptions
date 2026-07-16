from functools import lru_cache

from pydantic_settings import BaseSettings, SettingsConfigDict


class Settings(BaseSettings):
    """All runtime configuration. Every field is overridable via env var of the
    same name (case-insensitive), e.g. DATABASE_URL, FULL_MODEL, EBAY_CLIENT_ID."""

    model_config = SettingsConfigDict(env_file=".env", extra="ignore")

    database_url: str = "sqlite:///./scraper.db"

    # --- auth ---
    session_secret: str = "dev-secret-change-me"
    session_max_age_seconds: int = 60 * 60 * 24 * 30
    cookie_secure: bool = False  # set true behind TLS (Caddy) in prod
    seed_admin_password: str | None = None  # creates user "admin" on startup if no users exist

    # --- AI valuation (LiteLLM model strings: provider/model) ---
    # Swap providers by changing the string: "openai/gpt-4o-mini",
    # "anthropic/claude-haiku-4-5", "openrouter/deepseek/deepseek-chat", ...
    # The matching provider API key must be set in the environment
    # (OPENAI_API_KEY / ANTHROPIC_API_KEY / OPENROUTER_API_KEY).
    triage_model: str = "anthropic/claude-haiku-4-5"
    full_model: str = "anthropic/claude-sonnet-5"
    max_photos_per_valuation: int = 6

    # --- scraping ---
    ebay_client_id: str | None = None
    ebay_client_secret: str | None = None
    ebay_env: str = "production"  # or "sandbox"
    scheduler_tick_seconds: int = 30

    # --- alerting ---
    discord_webhook_url: str | None = None

    cors_origins: list[str] = ["http://localhost:5173"]


@lru_cache
def get_settings() -> Settings:
    return Settings()
