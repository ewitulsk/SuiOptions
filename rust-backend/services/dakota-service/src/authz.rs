//! Role and scope enforcement.
//!
//! The rule this module exists to enforce: **scope comes only from the verified
//! JWT**, never from a path parameter, query string or request body. A business
//! session asking about customer X is answered only if X is genuinely beneath
//! that business; an individual session is confined to itself.
//!
//! `auth_client::require_auth` has already run and inserted [`VerifiedClaims`]
//! into the request extensions, so everything here is a pure function of those
//! claims plus the read model.

use std::sync::Arc;

use auth_client::VerifiedClaims;
use axum::http::StatusCode;
use tracing::warn;

use crate::state::AppState;

pub type AuthzError = (StatusCode, String);

/// Who is calling, reduced to the two things that decide access.
#[derive(Debug, Clone)]
pub enum Caller {
    /// Unscoped. Sees and does everything.
    Admin,
    /// A partner business, scoped to its own sub-client id.
    Business { sub_client_id: String },
    /// A single end customer.
    Individual { customer_id: String },
}

impl Caller {
    pub fn from_claims(claims: &VerifiedClaims) -> Result<Self, AuthzError> {
        match claims.role.as_str() {
            "admin" => Ok(Caller::Admin),
            "business" => claims
                .scope
                .clone()
                .map(|sub_client_id| Caller::Business { sub_client_id })
                .ok_or_else(|| unscoped("business")),
            "individual" => claims
                .scope
                .clone()
                .map(|customer_id| Caller::Individual { customer_id })
                .ok_or_else(|| unscoped("individual")),
            other => {
                warn!(role = other, "unknown role on a verified token");
                Err((StatusCode::FORBIDDEN, "unknown role".into()))
            }
        }
    }

    pub fn is_admin(&self) -> bool {
        matches!(self, Caller::Admin)
    }

    /// The sub-client filter to apply when listing. `None` means unfiltered,
    /// which only an admin ever gets.
    pub fn sub_client_filter(&self) -> Option<&str> {
        match self {
            Caller::Admin => None,
            Caller::Business { sub_client_id } => Some(sub_client_id),
            // An individual has no roster; list handlers must use
            // `visible_customer` instead of this.
            Caller::Individual { .. } => None,
        }
    }

    /// Admin-only gate for control-plane routes.
    pub fn require_admin(&self) -> Result<(), AuthzError> {
        if self.is_admin() {
            Ok(())
        } else {
            Err((StatusCode::FORBIDDEN, "admin only".into()))
        }
    }
}

/// A scope-unset token for a role that requires one is a bug upstream, not a
/// permission question — fail closed and loudly rather than defaulting to
/// "sees everything".
fn unscoped(role: &str) -> AuthzError {
    warn!(role, "token carries a scoped role but no scope");
    (
        StatusCode::FORBIDDEN,
        format!("{role} token is missing its scope"),
    )
}

/// Authorize access to one customer, returning it.
///
/// Admin: anything. Individual: only itself. Business: only customers whose
/// `sub_client_id` is the business — checked against the read model, because
/// the caller could otherwise name any id it liked.
pub fn authorize_customer(
    state: &Arc<AppState>,
    caller: &Caller,
    customer_id: &str,
) -> Result<crate::db::models::Customer, AuthzError> {
    let customer = state
        .repo
        .get_customer(customer_id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        // 404, not 403: telling a caller that an id exists but is off-limits
        // lets them enumerate the customer base.
        .ok_or((StatusCode::NOT_FOUND, "unknown customer".to_string()))?;

    let permitted = match caller {
        Caller::Admin => true,
        Caller::Individual { customer_id: own } => own == customer_id,
        Caller::Business { sub_client_id } => {
            customer.sub_client_id.as_deref() == Some(sub_client_id.as_str())
                // A business can also see its own record.
                || customer.dakota_customer_id == *sub_client_id
        }
    };

    if !permitted {
        warn!(caller = ?caller, customer_id, "cross-scope access refused");
        return Err((StatusCode::NOT_FOUND, "unknown customer".into()));
    }
    Ok(customer)
}

/// The `sub_client_id` a newly created customer must be filed under.
///
/// A business may only create customers beneath itself — the value is taken
/// from its token, so a forged body cannot place a customer under someone
/// else. An admin may place a customer anywhere, including nowhere.
pub fn creation_sub_client(
    caller: &Caller,
    requested: Option<&str>,
) -> Result<Option<String>, AuthzError> {
    match caller {
        Caller::Admin => Ok(requested.map(|s| s.to_string())),
        Caller::Business { sub_client_id } => {
            if let Some(req) = requested {
                if req != sub_client_id {
                    return Err((
                        StatusCode::FORBIDDEN,
                        "a business may only create customers beneath itself".into(),
                    ));
                }
            }
            Ok(Some(sub_client_id.clone()))
        }
        Caller::Individual { .. } => Err((
            StatusCode::FORBIDDEN,
            "individuals cannot create customers".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claims(role: &str, scope: Option<&str>) -> VerifiedClaims {
        VerifiedClaims {
            address: String::new(),
            user_id: "u1".into(),
            role: role.into(),
            scope: scope.map(|s| s.into()),
            exp: 0,
        }
    }

    #[test]
    fn roles_map_to_callers() {
        assert!(Caller::from_claims(&claims("admin", None)).unwrap().is_admin());
        assert!(matches!(
            Caller::from_claims(&claims("business", Some("sub1"))).unwrap(),
            Caller::Business { .. }
        ));
        assert!(matches!(
            Caller::from_claims(&claims("individual", Some("cus1"))).unwrap(),
            Caller::Individual { .. }
        ));
    }

    #[test]
    fn a_scoped_role_without_a_scope_is_refused() {
        // Failing closed matters: defaulting to "no filter" would hand a
        // business the whole platform.
        assert!(Caller::from_claims(&claims("business", None)).is_err());
        assert!(Caller::from_claims(&claims("individual", None)).is_err());
    }

    #[test]
    fn unknown_role_is_refused() {
        assert!(Caller::from_claims(&claims("superuser", None)).is_err());
    }

    #[test]
    fn only_admin_lists_unfiltered() {
        assert_eq!(Caller::Admin.sub_client_filter(), None);
        assert_eq!(
            Caller::Business { sub_client_id: "sub1".into() }.sub_client_filter(),
            Some("sub1")
        );
    }

    #[test]
    fn business_cannot_file_a_customer_under_another_business() {
        let biz = Caller::Business { sub_client_id: "sub1".into() };
        // Ignoring the requested value and using the token's is the safe move.
        assert_eq!(creation_sub_client(&biz, None).unwrap().as_deref(), Some("sub1"));
        assert_eq!(creation_sub_client(&biz, Some("sub1")).unwrap().as_deref(), Some("sub1"));
        assert!(creation_sub_client(&biz, Some("sub2")).is_err());
    }

    #[test]
    fn admin_may_place_a_customer_anywhere() {
        assert_eq!(creation_sub_client(&Caller::Admin, None).unwrap(), None);
        assert_eq!(
            creation_sub_client(&Caller::Admin, Some("sub9")).unwrap().as_deref(),
            Some("sub9")
        );
    }

    #[test]
    fn individuals_cannot_create_customers() {
        let ind = Caller::Individual { customer_id: "cus1".into() };
        assert!(creation_sub_client(&ind, None).is_err());
    }

    #[test]
    fn require_admin_gates_control_plane() {
        assert!(Caller::Admin.require_admin().is_ok());
        assert!(Caller::Business { sub_client_id: "s".into() }.require_admin().is_err());
        assert!(Caller::Individual { customer_id: "c".into() }.require_admin().is_err());
    }
}
