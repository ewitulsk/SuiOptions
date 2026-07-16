def test_protected_route_requires_auth(client):
    assert client.get("/api/searches").status_code == 401
    assert client.get("/api/deals").status_code == 401


def test_login_wrong_password(client):
    resp = client.post("/auth/login", json={"username": "admin", "password": "nope"})
    assert resp.status_code == 401


def test_login_and_me(auth_client):
    resp = auth_client.get("/auth/me")
    assert resp.status_code == 200
    assert resp.json()["username"] == "admin"


def test_logout_clears_session(auth_client):
    auth_client.post("/auth/logout")
    assert auth_client.get("/auth/me").status_code == 401


def test_create_user_and_login(auth_client):
    resp = auth_client.post(
        "/auth/users", json={"username": "calvin", "password": "hunter2hunter2"}
    )
    assert resp.status_code == 201
    auth_client.post("/auth/logout")
    resp = auth_client.post(
        "/auth/login", json={"username": "calvin", "password": "hunter2hunter2"}
    )
    assert resp.status_code == 200


def test_duplicate_username_rejected(auth_client):
    body = {"username": "calvin", "password": "hunter2hunter2"}
    assert auth_client.post("/auth/users", json=body).status_code == 201
    assert auth_client.post("/auth/users", json=body).status_code == 409
