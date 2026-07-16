from contextlib import asynccontextmanager

from fastapi import FastAPI
from fastapi.middleware.cors import CORSMiddleware

from app.api import deals, listings, searches
from app.auth import routes as auth_routes
from app.config import get_settings
from app.db import get_engine, session_factory
from app.db.models import Base


@asynccontextmanager
async def lifespan(app: FastAPI):
    Base.metadata.create_all(get_engine())
    db = session_factory()()
    try:
        auth_routes.seed_admin(db)
    finally:
        db.close()
    yield


app = FastAPI(title="scraper", lifespan=lifespan)

app.add_middleware(
    CORSMiddleware,
    allow_origins=get_settings().cors_origins,
    allow_credentials=True,
    allow_methods=["*"],
    allow_headers=["*"],
)

app.include_router(auth_routes.router)
app.include_router(searches.router)
app.include_router(listings.router)
app.include_router(deals.router)


@app.get("/health")
def health():
    return {"ok": True}
