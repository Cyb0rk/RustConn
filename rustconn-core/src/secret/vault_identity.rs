//! The account identity a vault entry supplies alongside its password.
//!
//! A vault entry carries a user name and, for some backends, a Windows domain.
//! The rule for combining them with what the connection itself stores is the
//! same for every backend and every protocol, so it lives here once:
//!
//! - a non-empty vault value **replaces** the connection's value — the vault is
//!   where the user keeps the account, so a stale copy on the connection must
//!   not win;
//! - an empty or missing vault value **never clears** the connection's value —
//!   most backends simply have no domain, and an entry with no user name means
//!   "use the connection's";
//! - a down-level vault user name, `DOMAIN\user`, is split into its domain and
//!   user parts. An explicit vault domain still takes precedence over the
//!   prefix. A UPN (`user@corp.example`) is left whole and does not touch the
//!   domain, because some services use an e-mail address as the login;
//! - a vault domain that is not a plausible NetBIOS or DNS name (a notes field
//!   with prose in it, a value with a newline) is ignored rather than handed to
//!   an RDP client.
//!
//! The merge applies to one session. Writing the result back into the
//! connection is the editor's job (**Load credentials from vault**).

/// The longest domain accepted: the DNS limit for a fully qualified name.
const MAX_DOMAIN_LEN: usize = 255;

/// The user name and domain a session should use.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VaultIdentity {
    /// The effective user name, if any.
    pub username: Option<String>,
    /// The effective Windows domain, if any.
    pub domain: Option<String>,
}

/// Returns `domain` trimmed if it is a plausible NetBIOS or DNS domain name.
///
/// Accepts ASCII letters, digits, `.`, `-` and `_`, up to 255 characters, with
/// no leading or trailing dot. Anything else — whitespace inside the value, a
/// newline, punctuation from a free-text notes field — returns `None`.
#[must_use]
pub fn sanitize_domain(domain: &str) -> Option<String> {
    let domain = domain.trim();
    let plausible = !domain.is_empty()
        && domain.len() <= MAX_DOMAIN_LEN
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && domain
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'));
    plausible.then(|| domain.to_string())
}

/// Splits a down-level logon name, `DOMAIN\user`, into `(domain, user)`.
///
/// Returns `None` for a name without a backslash, with an empty part on either
/// side, or with a domain part [`sanitize_domain`] rejects. A UPN
/// (`user@corp.example`) is not a down-level name and returns `None`.
#[must_use]
pub fn split_down_level(username: &str) -> Option<(String, String)> {
    let (domain, user) = username.trim().split_once('\\')?;
    let user = user.trim();
    if user.is_empty() || user.contains('\\') {
        return None;
    }
    Some((sanitize_domain(domain)?, user.to_string()))
}

/// Combines the connection's stored identity with the one a vault entry
/// supplies, following the rules in the module documentation.
#[must_use]
pub fn merge_vault_identity(
    connection_username: Option<&str>,
    connection_domain: Option<&str>,
    vault_username: Option<&str>,
    vault_domain: Option<&str>,
) -> VaultIdentity {
    let non_empty = |value: Option<&str>| {
        value
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
    };

    let (prefix_domain, vault_user) = match non_empty(vault_username) {
        Some(name) => match split_down_level(&name) {
            Some((domain, user)) => (Some(domain), Some(user)),
            None => (None, Some(name)),
        },
        None => (None, None),
    };

    let explicit_domain = vault_domain.and_then(|raw| {
        let sanitized = sanitize_domain(raw);
        if sanitized.is_none() && !raw.trim().is_empty() {
            // The value itself is not logged: it may be a notes field.
            tracing::warn!(
                len = raw.len(),
                "vault domain is not a valid domain name; keeping the connection's"
            );
        }
        sanitized
    });

    VaultIdentity {
        username: vault_user.or_else(|| non_empty(connection_username)),
        domain: explicit_domain
            .or(prefix_domain)
            .or_else(|| non_empty(connection_domain)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_vault_value_replaces_the_connection_value() {
        let id = merge_vault_identity(Some("old"), Some("OLDDOM"), Some("new"), Some("NEWDOM"));
        assert_eq!(id.username.as_deref(), Some("new"));
        assert_eq!(id.domain.as_deref(), Some("NEWDOM"));
    }

    #[test]
    fn an_empty_vault_value_never_clears_the_connection_value() {
        for (vu, vd) in [(None, None), (Some(""), Some("")), (Some("  "), Some(" "))] {
            let id = merge_vault_identity(Some("admin"), Some("CORP"), vu, vd);
            assert_eq!(id.username.as_deref(), Some("admin"), "{vu:?}");
            assert_eq!(id.domain.as_deref(), Some("CORP"), "{vd:?}");
        }
        assert_eq!(
            merge_vault_identity(None, None, None, None),
            VaultIdentity::default()
        );
    }

    #[test]
    fn a_down_level_vault_name_is_split_into_domain_and_user() {
        let id = merge_vault_identity(Some("admin"), Some("OLD"), Some(r"CORP\alice"), None);
        assert_eq!(id.username.as_deref(), Some("alice"));
        assert_eq!(id.domain.as_deref(), Some("CORP"));
    }

    #[test]
    fn an_explicit_vault_domain_beats_the_down_level_prefix() {
        let id = merge_vault_identity(None, None, Some(r"CORP\alice"), Some("corp.example"));
        assert_eq!(id.username.as_deref(), Some("alice"));
        assert_eq!(id.domain.as_deref(), Some("corp.example"));
    }

    #[test]
    fn a_upn_stays_whole_and_leaves_the_domain_alone() {
        let id = merge_vault_identity(Some("old"), Some("CORP"), Some("alice@corp.example"), None);
        assert_eq!(id.username.as_deref(), Some("alice@corp.example"));
        assert_eq!(id.domain.as_deref(), Some("CORP"));
    }

    #[test]
    fn an_implausible_vault_domain_is_ignored() {
        for bad in [
            "VPN via office, rotate quarterly",
            "CORP\nEVIL",
            ".corp",
            "corp.",
            "co rp",
        ] {
            let id = merge_vault_identity(None, Some("KEEP"), None, Some(bad));
            assert_eq!(id.domain.as_deref(), Some("KEEP"), "{bad:?}");
        }
        let long = "a".repeat(MAX_DOMAIN_LEN + 1);
        assert_eq!(sanitize_domain(&long), None);
    }

    #[test]
    fn split_down_level_rejects_malformed_names() {
        assert_eq!(
            split_down_level(r" CORP\alice "),
            Some(("CORP".to_string(), "alice".to_string()))
        );
        for bad in [
            r"\alice",
            r"CORP\",
            r"CO RP\alice",
            r"A\B\C",
            "alice",
            "a@b.c",
        ] {
            assert_eq!(split_down_level(bad), None, "{bad:?}");
        }
    }
}
