from datetime import datetime
from decimal import Decimal

from fastapi import APIRouter, Depends, HTTPException
from pydantic import BaseModel, ConfigDict, Field
from sqlalchemy import select
from sqlalchemy.orm import Session

from app.adapters import ADAPTERS
from app.auth.deps import current_user
from app.db import get_db
from app.db.models import SavedSearch

router = APIRouter(prefix="/api/searches", tags=["searches"], dependencies=[Depends(current_user)])


class SearchIn(BaseModel):
    source: str
    name: str = Field(min_length=1, max_length=128)
    query: str = Field(min_length=1, max_length=256)
    category: str | None = None
    min_price: Decimal | None = None
    max_price: Decimal | None = None
    poll_interval_seconds: int = Field(default=300, ge=60)
    alert_threshold: float = Field(default=1.0, gt=0)
    active: bool = True


class SearchOut(SearchIn):
    model_config = ConfigDict(from_attributes=True)

    id: int
    last_polled_at: datetime | None
    created_at: datetime


def _validate_source(source: str) -> None:
    if source not in ADAPTERS:
        raise HTTPException(
            status_code=422, detail=f"Unknown source {source!r}; known: {sorted(ADAPTERS)}"
        )


@router.get("", response_model=list[SearchOut])
def list_searches(db: Session = Depends(get_db)):
    return db.scalars(select(SavedSearch).order_by(SavedSearch.id)).all()


@router.post("", response_model=SearchOut, status_code=201)
def create_search(body: SearchIn, db: Session = Depends(get_db)):
    _validate_source(body.source)
    search = SavedSearch(**body.model_dump())
    db.add(search)
    db.commit()
    return search


@router.put("/{search_id}", response_model=SearchOut)
def update_search(search_id: int, body: SearchIn, db: Session = Depends(get_db)):
    _validate_source(body.source)
    search = db.get(SavedSearch, search_id)
    if search is None:
        raise HTTPException(status_code=404, detail="Search not found")
    for key, value in body.model_dump().items():
        setattr(search, key, value)
    db.commit()
    return search


@router.delete("/{search_id}", status_code=204)
def delete_search(search_id: int, db: Session = Depends(get_db)):
    search = db.get(SavedSearch, search_id)
    if search is None:
        raise HTTPException(status_code=404, detail="Search not found")
    # soft-disable rather than delete: listings/deals reference searches
    search.active = False
    db.commit()
