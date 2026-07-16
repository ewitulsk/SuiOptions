from fastapi import Depends, HTTPException, Request
from sqlalchemy.orm import Session

from app.auth.security import SESSION_COOKIE, read_session_token
from app.db import get_db
from app.db.models import User


def current_user(request: Request, db: Session = Depends(get_db)) -> User:
    token = request.cookies.get(SESSION_COOKIE)
    if not token:
        raise HTTPException(status_code=401, detail="Not authenticated")
    user_id = read_session_token(token)
    if user_id is None:
        raise HTTPException(status_code=401, detail="Invalid or expired session")
    user = db.get(User, user_id)
    if user is None:
        raise HTTPException(status_code=401, detail="User no longer exists")
    return user
