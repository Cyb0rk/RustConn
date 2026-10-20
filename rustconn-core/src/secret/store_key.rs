//! The vault keys a connection's credential is stored and looked up under, on
//! every backend except KeePass.
//!
//! These lived in the GUI crate until 0.23.17, which left `rustconn-cli` to
//! build its own keys — and it built different ones, so `secret get` never found
//! a password saved from the GUI and a connect never found one saved with
//! `secret set` (issue #353). One copy here, used by both.

use uuid::Uuid;

use crate::config::SecretBackendType;
use crate::models::{Connection, ConnectionGroup};
use crate::secret::{CredentialResolver, KeePassHierarchy};

/// Generates the correct store key for a connection based on the backend type.
///
/// LibSecret uses hierarchical `"RustConn/{group_path}/{name} ({protocol})"` format
/// when `group_path` is provided (matching
/// [`CredentialResolver::generate_keyring_key_with_hierarchy`]), or falls back to
/// `"{name} ({protocol})"` for backward compatibility when no group path is given.
/// All other backends use `"rustconn/{name}"` (matching
/// [`CredentialResolver::generate_lookup_key`]).
///
/// When `conn_name` is empty, falls back to `conn_host` for non-LibSecret
/// backends, matching the resolver's `generate_lookup_key` behavior.
///
/// This ensures that the key used at store time matches the primary key the
/// resolver tries at resolve time, eliminating the need for fallback lookups.
#[must_use]
pub fn generate_store_key(
    conn_name: &str,
    conn_host: &str,
    protocol_str: &str,
    backend_type: SecretBackendType,
) -> String {
    generate_store_key_with_group(conn_name, conn_host, protocol_str, backend_type, None)
}

/// Generates a store key that includes the group path for keyring backends.
///
/// Prefer [`generate_store_key_for_connection`] whenever a connection's group is
/// what decides the key. This function's `group_path` has three meanings, not
/// two, and getting the third wrong is issue
/// [#316](https://github.com/totoshko88/RustConn/issues/316): `Some("Prod")` is a
/// grouped connection, `Some("")` is an ungrouped *connection*, and `None` is the
/// pre-0.19.18 flat key that belongs to no connection at all. `Some("")` and
/// `None` differ by the `RustConn/` prefix, which is exactly what an ungrouped
/// connection's entry is written with.
///
/// `group_path` is the `/`-separated group hierarchy (e.g. `"Production/Web"`).
/// When provided and the backend is LibSecret or the macOS Keychain, the key is
/// `"RustConn/{group_path}/{name} ({protocol})"`.
///
/// The macOS Keychain is included because `CredentialResolver` resolves it
/// through the same hierarchical keyring path as LibSecret
/// (`resolve_from_keyring_hierarchical`). Storing it under the flat
/// `rustconn/{name}` key, as this function did until 0.19.19, meant the resolver
/// never looked where the credential had been written.
#[must_use]
pub fn generate_store_key_with_group(
    conn_name: &str,
    conn_host: &str,
    protocol_str: &str,
    backend_type: SecretBackendType,
    group_path: Option<&str>,
) -> String {
    if matches!(
        backend_type,
        SecretBackendType::LibSecret | SecretBackendType::MacOsKeychain
    ) {
        let name = conn_name.trim().replace('/', "-");
        let suffix = format!("{name} ({protocol_str})");
        match group_path {
            Some(path) if !path.is_empty() => format!("RustConn/{path}/{suffix}"),
            Some(_) => format!("RustConn/{suffix}"),
            None => suffix,
        }
    } else {
        // All other backends: "rustconn/{identifier}" — matches generate_lookup_key
        // Falls back to host when name is empty, same as CredentialResolver
        let identifier = if conn_name.trim().is_empty() {
            conn_host
        } else {
            conn_name
        };
        format!("rustconn/{identifier}")
    }
}

/// Generates the vault key for a connection, from the group it is in.
///
/// The form to use wherever a connection decides the key, and the reason it
/// exists is issue [#316](https://github.com/totoshko88/RustConn/issues/316).
/// Four call sites built the `Option<&str>` group path for
/// [`generate_store_key_with_group`] themselves, and they did not agree about the
/// ungrouped case: saving and deleting passed `Some("")`, while resolving and the
/// connection dialog's 📂 load and ✓ test buttons wrote
/// `connection.group_id.map(…)`, which is `None` when there is no group. With
/// LibSecret that meant a password on an ungrouped connection was written to
/// `RustConn/{name} ({protocol})` and looked for at `{name} ({protocol})`. The
/// Secret Service matches attributes exactly, so the two never met: connecting
/// prompted for a password that was sitting in the keyring, and both buttons
/// reported it missing. Putting the connection into a group fixed it, because
/// then the two keys differed *and* the resolver's second, legacy key happened to
/// cover the miss.
///
/// `group_id` of `None` means the connection is ungrouped — it does not mean the
/// key should drop the `RustConn/` prefix. That decision is made here now, once,
/// instead of at each caller.
#[must_use]
pub fn generate_store_key_for_connection(
    conn_name: &str,
    conn_host: &str,
    protocol_str: &str,
    backend_type: SecretBackendType,
    group_id: Option<Uuid>,
    groups: &[ConnectionGroup],
) -> String {
    let group_path = group_id
        .map(|gid| KeePassHierarchy::resolve_group_path(gid, groups).join("/"))
        .unwrap_or_default();
    generate_store_key_with_group(
        conn_name,
        conn_host,
        protocol_str,
        backend_type,
        Some(&group_path),
    )
}

/// Builds every vault lookup key a connection's credentials may live under on a
/// non-KeePass backend, most-current format first.
///
/// The first entry is the key the current code stores under; the rest are
/// formats written by earlier releases, kept so that cleanup and fallback
/// retrieval still find them.
///
/// For LibSecret and the macOS Keychain the current key embeds the group path
/// (`RustConn/{group}/{name} ({protocol})`) so same-named connections in
/// different groups no longer collide (issue #264). Deleting by the flat
/// `{name} ({protocol})` key alone therefore matched nothing and silently left
/// the keyring item behind (issue #263) — the attribute search in
/// `LibSecretBackend::delete_value` finds zero items and reports success.
///
/// The legacy name-keyed formats are deliberately included even though two
/// same-named connections in different groups map onto the same legacy key, so
/// cleaning up one can remove an entry the other still resolves through. That is
/// acceptable because a shared legacy key means the two connections were already
/// sharing a single stored password — which is precisely the collision reported
/// in issue #264 — so there is no distinct credential to lose. Once either
/// connection has been resolved on 0.19.18 or later, its credential lives under
/// its own group-scoped key and is unaffected.
#[must_use]
pub fn vault_keys_for_connection(
    groups: &[ConnectionGroup],
    connection: &Connection,
    protocol_str: &str,
    backend_type: SecretBackendType,
) -> Vec<String> {
    let mut keys = Vec::new();

    if matches!(
        backend_type,
        SecretBackendType::LibSecret | SecretBackendType::MacOsKeychain
    ) {
        // Primary: exactly what `save_password_to_vault` writes. The ungrouped
        // case — the `RustConn/` prefix with no group segment — is decided inside
        // `generate_store_key_for_connection`; this call site used to make it
        // itself, which is how the two sites that made it differently went
        // unnoticed (issue #316).
        keys.push(generate_store_key_for_connection(
            &connection.name,
            &connection.host,
            protocol_str,
            backend_type,
            connection.group_id,
            groups,
        ));
        // The resolver builds the same shape but additionally runs the name
        // through `sanitize_imported_value`, so an imported name with trailing
        // escape sequences can land under a second key. Read-time migration
        // writes that variant, so cleanup has to cover it too.
        keys.push(CredentialResolver::generate_keyring_key_with_hierarchy(
            connection, groups,
        ));
        // Pre-0.19.18 flat key, before the group path was part of the key.
        keys.push(CredentialResolver::generate_keyring_key(connection));
        // Pre-0.19.19 macOS Keychain key: the store path wrote the flat
        // `rustconn/{name}` format while the resolver looked for the
        // hierarchical one, so entries can still exist under it.
        let identifier = if connection.name.trim().is_empty() {
            &connection.host
        } else {
            &connection.name
        };
        keys.push(format!("rustconn/{identifier}"));
    } else {
        keys.push(generate_store_key(
            &connection.name,
            &connection.host,
            protocol_str,
            backend_type,
        ));
    }

    // Oldest format: the connection UUID.
    keys.push(connection.id.to_string());

    keys.dedup();
    keys
}
