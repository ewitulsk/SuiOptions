import os
import tempfile

# Must be set before any app import: config is cached and the engine is global.
_db_path = os.path.join(tempfile.mkdtemp(prefix="scraper-test-"), "test.db")
os.environ.setdefault("DATABASE_URL", f"sqlite:///{_db_path}")
os.environ.setdefault("SESSION_SECRET", "test-secret")
os.environ.setdefault("SEED_ADMIN_PASSWORD", "admin-password")
os.environ.setdefault("EBAY_CLIENT_ID", "test-client-id")
os.environ.setdefault("EBAY_CLIENT_SECRET", "test-client-secret")
os.environ.setdefault("DISCORD_WEBHOOK_URL", "https://discord.example/webhook")

import pytest
from fastapi.testclient import TestClient

from app.db import get_engine, session_factory
from app.db.models import Base
from app.main import app


@pytest.fixture(autouse=True)
def fresh_db():
    engine = get_engine()
    Base.metadata.drop_all(engine)
    Base.metadata.create_all(engine)
    yield


@pytest.fixture
def db():
    session = session_factory()()
    yield session
    session.close()


@pytest.fixture
def client():
    with TestClient(app) as c:
        yield c


@pytest.fixture
def auth_client(client):
    resp = client.post("/auth/login", json={"username": "admin", "password": "admin-password"})
    assert resp.status_code == 200, resp.text
    return client
