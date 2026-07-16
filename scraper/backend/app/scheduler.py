import logging
import time
from datetime import UTC, timedelta

from sqlalchemy import select
from sqlalchemy.orm import Session

from app import pipeline
from app.adapters import get_adapter
from app.config import get_settings
from app.db.models import SavedSearch, utcnow

logger = logging.getLogger(__name__)


def poll_search(db: Session, search: SavedSearch) -> int:
    """Run one saved search end-to-end. Returns number of new listings found."""
    adapter = get_adapter(search.source)
    raw = adapter.search(search)
    new_listings = pipeline.ingest(db, search, raw)
    for listing in new_listings:
        pipeline.process_listing(db, listing, search)
    search.last_polled_at = utcnow()
    db.commit()
    return len(new_listings)


def due_searches(db: Session) -> list[SavedSearch]:
    now = utcnow()
    searches = db.scalars(select(SavedSearch).where(SavedSearch.active)).all()
    due = []
    for s in searches:
        last = s.last_polled_at
        if last is not None and last.tzinfo is None:
            # sqlite loses tzinfo on round-trip; stored values are always UTC

            last = last.replace(tzinfo=UTC)
        if last is None or now - last >= timedelta(seconds=s.poll_interval_seconds):
            due.append(s)
    return due


def run_forever() -> None:
    """Worker entrypoint: tick, poll every due search, sleep, repeat.
    Sequential on purpose — at this scale simplicity beats concurrency."""
    from app.db import get_engine, session_factory
    from app.db.models import Base

    Base.metadata.create_all(get_engine())
    tick = get_settings().scheduler_tick_seconds
    logger.info("scheduler started (tick=%ss)", tick)
    while True:
        db = session_factory()()
        try:
            for search in due_searches(db):
                try:
                    n = poll_search(db, search)
                    logger.info("search %s (%s): %s new listings", search.id, search.name, n)
                except Exception:
                    db.rollback()
                    logger.exception("search %s (%s) failed", search.id, search.name)
        finally:
            db.close()
        time.sleep(tick)
