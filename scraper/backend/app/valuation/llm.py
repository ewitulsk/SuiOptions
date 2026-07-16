import json
import logging

import litellm
from pydantic import ValidationError

from app.config import get_settings
from app.db.models import Listing
from app.valuation import prompts
from app.valuation.schemas import TriageResult, ValuationResult

logger = logging.getLogger(__name__)

# Providers differ in which params they accept; drop unsupported ones instead of erroring.
litellm.drop_params = True


class ValuationError(Exception):
    pass


def _complete_json(model: str, messages: list[dict]) -> dict:
    """Provider-agnostic JSON completion. Swap OpenAI/Anthropic/OpenRouter by
    changing the model string (TRIAGE_MODEL / FULL_MODEL env vars)."""
    response = litellm.completion(
        model=model,
        messages=messages,
        response_format={"type": "json_object"},
        timeout=120,
    )
    content = response.choices[0].message.content or ""
    return _extract_json(content)


def _extract_json(content: str) -> dict:
    try:
        return json.loads(content)
    except json.JSONDecodeError:
        pass
    # fall back to the outermost {...} block (some models wrap JSON in prose/fences)
    start, end = content.find("{"), content.rfind("}")
    if start != -1 and end > start:
        try:
            return json.loads(content[start : end + 1])
        except json.JSONDecodeError:
            pass
    raise ValuationError(f"model returned unparseable JSON: {content[:200]!r}")


def triage(listing: Listing) -> TriageResult:
    settings = get_settings()
    messages = [
        {"role": "system", "content": prompts.TRIAGE_SYSTEM},
        {
            "role": "user",
            "content": prompts.triage_user_message(
                listing.title,
                str(listing.price),
                listing.currency,
                listing.saved_search.category if listing.saved_search else None,
            ),
        },
    ]
    try:
        data = _complete_json(settings.triage_model, messages)
        return TriageResult.model_validate(data)
    except (ValuationError, ValidationError) as e:
        # on triage failure, err toward the full valuation rather than dropping a deal
        logger.warning("triage failed for listing %s: %s", listing.id, e)
        return TriageResult(promising=True, reason=f"triage error, passing through: {e}")


def full_valuation(listing: Listing) -> ValuationResult:
    settings = get_settings()
    content: list[dict] = [
        {
            "type": "text",
            "text": prompts.full_user_text(
                listing.title,
                str(listing.price),
                listing.currency,
                listing.description,
                listing.location,
                listing.source,
            ),
        }
    ]
    for url in (listing.photos or [])[: settings.max_photos_per_valuation]:
        content.append({"type": "image_url", "image_url": {"url": url}})

    messages = [
        {"role": "system", "content": prompts.FULL_SYSTEM},
        {"role": "user", "content": content},
    ]
    try:
        data = _complete_json(settings.full_model, messages)
        return ValuationResult.model_validate(data)
    except ValidationError as e:
        raise ValuationError(f"valuation response failed validation: {e}") from e
