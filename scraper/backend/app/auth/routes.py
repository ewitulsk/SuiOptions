from fastapi import APIRouter, Depends, HTTPException, Request, Response
from pydantic import BaseModel, Field
from sqlalchemy import select
from sqlalchemy.orm import Session

from app.auth.deps import current_user
from app.auth.security import (
    SESSION_COOKIE,
    create_session_token,
    hash_password,
    login_limiter,
    verify_password,
)
from app.config import get_settings
from app.db import get_db
from app.db.models import User

router = APIRouter(prefix="/auth", tags=["auth"])


class LoginRequest(BaseModel):
    username: str
    password: str


class UserOut(BaseModel):
    id: int
    username: str


class CreateUserRequest(BaseModel):
    username: str = Field(min_length=1, max_length=64)
    password: str = Field(min_length=8)


def _set_session_cookie(response: Response, user_id: int) -> None:
    settings = get_settings()
    response.set_cookie(
        SESSION_COOKIE,
        create_session_token(user_id),
        max_age=settings.session_max_age_seconds,
        httponly=True,
        secure=settings.cookie_secure,
        samesite="lax",
    )


@router.post("/login", response_model=UserOut)
def login(body: LoginRequest, request: Request, response: Response, db: Session = Depends(get_db)):
    key = f"{request.client.host if request.client else '?'}:{body.username}"
    if login_limiter.blocked(key):
        raise HTTPException(status_code=429, detail="Too many failed attempts; try again later")
    user = db.scalar(select(User).where(User.username == body.username))
    if user is None or not verify_password(body.password, user.password_hash):
        login_limiter.record_failure(key)
        raise HTTPException(status_code=401, detail="Invalid username or password")
    login_limiter.reset(key)
    _set_session_cookie(response, user.id)
    return UserOut(id=user.id, username=user.username)


@router.post("/logout")
def logout(response: Response):
    response.delete_cookie(SESSION_COOKIE)
    return {"ok": True}


@router.get("/me", response_model=UserOut)
def me(user: User = Depends(current_user)):
    return UserOut(id=user.id, username=user.username)


@router.get("/users", response_model=list[UserOut])
def list_users(db: Session = Depends(get_db), user: User = Depends(current_user)):
    return [UserOut(id=u.id, username=u.username) for u in db.scalars(select(User)).all()]


@router.post("/users", response_model=UserOut, status_code=201)
def create_user(
    body: CreateUserRequest, db: Session = Depends(get_db), user: User = Depends(current_user)
):
    if db.scalar(select(User).where(User.username == body.username)):
        raise HTTPException(status_code=409, detail="Username already taken")
    new_user = User(username=body.username, password_hash=hash_password(body.password))
    db.add(new_user)
    db.commit()
    return UserOut(id=new_user.id, username=new_user.username)


def seed_admin(db: Session) -> None:
    """Create the first user from SEED_ADMIN_PASSWORD if the users table is empty."""
    settings = get_settings()
    if not settings.seed_admin_password:
        return
    if db.scalar(select(User).limit(1)) is not None:
        return
    db.add(User(username="admin", password_hash=hash_password(settings.seed_admin_password)))
    db.commit()
