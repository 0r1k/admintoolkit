//! MySQL/MariaDB user management. Connects either directly or through an
//! SSH tunnel (see `crate::ssh_tunnel`), then issues plain `CREATE
//! USER`/`DROP USER`/`ALTER USER`/`GRANT` statements — MySQL has no
//! parameter-binding for identifiers or for `IDENTIFIED BY` in these
//! statements, so values are escaped manually (see `quote_lit`/`quote_ident`)
//! rather than relying on driver-side parameterization.

use mysql::prelude::Queryable;
use mysql::{Conn, OptsBuilder};

use super::config::ConnectionWithSecrets;
use crate::ssh_exec::Credentials;

pub struct MysqlUser {
    pub user: String,
    pub host: String,
}

/// Privileges offered in the "Add User" picker. `"ALL PRIVILEGES"` must
/// stay first — it's treated as the picker's all-or-nothing sentinel.
pub const PRIVILEGES: &[&str] = &[
    "ALL PRIVILEGES",
    "SELECT",
    "INSERT",
    "UPDATE",
    "DELETE",
    "CREATE",
    "DROP",
    "ALTER",
    "INDEX",
    "REFERENCES",
    "EXECUTE",
];

fn connect_direct(host: &str, port: u16, user: &str, password: &str) -> Result<Conn, String> {
    let opts = OptsBuilder::new()
        .ip_or_hostname(Some(host))
        .tcp_port(port)
        .user(Some(user))
        .pass(if password.is_empty() { None } else { Some(password) });
    Conn::new(opts).map_err(|e| e.to_string())
}

/// Connects using a profile's settings, transparently opening an SSH
/// tunnel first if the profile asks for one.
pub fn connect(cfg: &ConnectionWithSecrets) -> Result<Conn, String> {
    let port: u16 = cfg.port.trim().parse().map_err(|_| format!("invalid port {:?}", cfg.port))?;

    if cfg.use_tunnel {
        let creds = Credentials {
            user: cfg.ssh_user.clone(),
            password: cfg.ssh_password.clone(),
            private_key_path: cfg.ssh_key_path.clone(),
        };
        let tunnel = crate::ssh_tunnel::open(&cfg.ssh_host, &cfg.ssh_port, &creds, &cfg.host, port)
            .map_err(|e| format!("SSH tunnel failed: {e}"))?;
        connect_direct("127.0.0.1", tunnel.local_port, &cfg.db_user, &cfg.db_password)
    } else {
        connect_direct(&cfg.host, port, &cfg.db_user, &cfg.db_password)
    }
}

pub fn list_users(conn: &mut Conn) -> Result<Vec<MysqlUser>, String> {
    conn.query_map("SELECT User, Host FROM mysql.user ORDER BY User, Host", |(user, host)| MysqlUser { user, host })
        .map_err(|e| e.to_string())
}

/// Creates a user and, if `grant` is `Some((target, privileges))`, grants
/// the selected privileges on `target` (an empty `privileges` slice means
/// `ALL PRIVILEGES`). `target` follows MySQL's own `GRANT ... ON`
/// scoping syntax — see `quote_grant_target` for the accepted forms.
pub fn create_user(
    conn: &mut Conn,
    user: &str,
    host: &str,
    password: &str,
    grant: Option<(&str, &[&str])>,
) -> Result<(), String> {
    let stmt = format!("CREATE USER {}@{} IDENTIFIED BY {}", quote_lit(user), quote_lit(host), quote_lit(password));
    conn.query_drop(&stmt).map_err(|e| e.to_string())?;

    if let Some((target, privileges)) = grant {
        grant_privileges(conn, user, host, target, privileges)?;
    }
    Ok(())
}

/// Grants `privileges` (empty = `ALL PRIVILEGES`) on `target` to an
/// existing `user@host` — the same scoping/escaping `create_user`'s
/// initial grant uses, but callable on its own so privileges can be added
/// after the user already exists.
pub fn grant_privileges(conn: &mut Conn, user: &str, host: &str, target: &str, privileges: &[&str]) -> Result<(), String> {
    let target_part = quote_grant_target(target);
    let priv_list = if privileges.is_empty() { "ALL PRIVILEGES".to_string() } else { privileges.join(", ") };
    let stmt = format!("GRANT {priv_list} ON {target_part} TO {}@{}", quote_lit(user), quote_lit(host));
    conn.query_drop(&stmt).map_err(|e| e.to_string())
}

/// Revokes `privileges` (empty = `ALL PRIVILEGES`) on `target` from
/// `user@host`.
pub fn revoke_privileges(conn: &mut Conn, user: &str, host: &str, target: &str, privileges: &[&str]) -> Result<(), String> {
    let target_part = quote_grant_target(target);
    let priv_list = if privileges.is_empty() { "ALL PRIVILEGES".to_string() } else { privileges.join(", ") };
    let stmt = format!("REVOKE {priv_list} ON {target_part} FROM {}@{}", quote_lit(user), quote_lit(host));
    conn.query_drop(&stmt).map_err(|e| e.to_string())
}

pub fn drop_user(conn: &mut Conn, user: &str, host: &str) -> Result<(), String> {
    let stmt = format!("DROP USER {}@{}", quote_lit(user), quote_lit(host));
    conn.query_drop(&stmt).map_err(|e| e.to_string())
}

pub fn change_password(conn: &mut Conn, user: &str, host: &str, new_password: &str) -> Result<(), String> {
    let stmt = format!(
        "ALTER USER {}@{} IDENTIFIED BY {}",
        quote_lit(user),
        quote_lit(host),
        quote_lit(new_password)
    );
    conn.query_drop(&stmt).map_err(|e| e.to_string())
}

/// Parses the "Grant DB" field into a `GRANT ... ON <target>` scope,
/// following MySQL's own privilege-target syntax: empty or `*` means
/// every database (`*.*`); `db` means every table in that database
/// (`db.*`); `db.*` and `db.table` pass through with each half quoted
/// (an unquoted `*` half stays a wildcard, never treated as an
/// identifier).
fn quote_grant_target(target: &str) -> String {
    let trimmed = target.trim();
    if trimmed.is_empty() || trimmed == "*" || trimmed == "*.*" {
        return "*.*".to_string();
    }
    let quote_part = |p: &str| if p == "*" { "*".to_string() } else { quote_ident(p) };
    match trimmed.split_once('.') {
        Some((db, table)) => format!("{}.{}", quote_part(db), quote_part(table)),
        None => format!("{}.*", quote_part(trimmed)),
    }
}

/// Escapes a MySQL string literal, e.g. for `'user'@'host'` or
/// `IDENTIFIED BY 'password'` — these can't be bound as query parameters
/// since `CREATE USER`/`ALTER USER` aren't preparable in older MySQL.
fn quote_lit(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            '\0' => out.push_str("\\0"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\x1a' => out.push_str("\\Z"),
            _ => out.push(c),
        }
    }
    out.push('\'');
    out
}

/// Escapes a MySQL identifier (database/table/column name).
fn quote_ident(s: &str) -> String {
    format!("`{}`", s.replace('`', "``"))
}
