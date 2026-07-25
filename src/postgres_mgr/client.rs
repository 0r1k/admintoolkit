//! PostgreSQL role (user) management. Connects either directly or through
//! an SSH tunnel (see `crate::ssh_tunnel`), then issues plain `CREATE
//! USER`/`DROP USER`/`ALTER USER`/`GRANT` statements — role/database names
//! can't be bound as query parameters (only values can), so identifiers are
//! escaped manually (see `quote_ident`/`quote_lit`).
//!
//! Table-level privileges (`SELECT`/`INSERT`/...) are a Postgres wrinkle:
//! `GRANT ... ON ALL TABLES IN SCHEMA` only affects the schema of the
//! *current connection's* database, not an arbitrarily named one, so
//! granting them on `grant_db` requires a second connection to that
//! specific database (see `grant_table_privileges`) — the maintenance
//! connection used for `CREATE USER` stays on the `postgres` database.

use postgres::{Client, NoTls};

use super::config::ConnectionWithSecrets;
use crate::ssh_exec::Credentials;

pub struct PgRole {
    pub name: String,
    pub superuser: bool,
    pub can_login: bool,
    pub create_db: bool,
    pub create_role: bool,
}

/// Table-level privileges offered in the "Add User" picker, granted via
/// `GRANT ... ON ALL TABLES IN SCHEMA public` (plus matching default
/// privileges for tables created later). `"ALL"` must stay first — it's
/// treated as the picker's all-or-nothing sentinel.
pub const PRIVILEGES: &[&str] = &["ALL", "SELECT", "INSERT", "UPDATE", "DELETE", "TRUNCATE", "REFERENCES", "TRIGGER"];

fn connect_direct(host: &str, port: u16, user: &str, password: &str, dbname: &str) -> Result<Client, String> {
    let mut config = postgres::Config::new();
    config.host(host).port(port).user(user).dbname(dbname);
    if !password.is_empty() {
        config.password(password);
    }
    config.connect(NoTls).map_err(|e| e.to_string())
}

/// Connects using a profile's settings (transparently opening an SSH
/// tunnel first if the profile asks for one) to `dbname` specifically.
pub fn connect_db(cfg: &ConnectionWithSecrets, dbname: &str) -> Result<Client, String> {
    let port: u16 = cfg.port.trim().parse().map_err(|_| format!("invalid port {:?}", cfg.port))?;

    if cfg.use_tunnel {
        let creds = Credentials {
            user: cfg.ssh_user.clone(),
            password: cfg.ssh_password.clone(),
            private_key_path: cfg.ssh_key_path.clone(),
        };
        let tunnel = crate::ssh_tunnel::open(&cfg.ssh_host, &cfg.ssh_port, &creds, &cfg.host, port)
            .map_err(|e| format!("SSH tunnel failed: {e}"))?;
        connect_direct("127.0.0.1", tunnel.local_port, &cfg.db_user, &cfg.db_password, dbname)
    } else {
        connect_direct(&cfg.host, port, &cfg.db_user, &cfg.db_password, dbname)
    }
}

/// Connects to the `postgres` maintenance database, which always exists
/// and is enough for role management (`CREATE ROLE`/`DROP ROLE` aren't
/// per-database).
pub fn connect(cfg: &ConnectionWithSecrets) -> Result<Client, String> {
    connect_db(cfg, "postgres")
}

pub fn list_users(client: &mut Client) -> Result<Vec<PgRole>, String> {
    let rows = client
        .query(
            "SELECT rolname, rolsuper, rolcanlogin, rolcreatedb, rolcreaterole \
             FROM pg_roles ORDER BY rolname",
            &[],
        )
        .map_err(|e| e.to_string())?;
    Ok(rows
        .iter()
        .map(|r| PgRole {
            name: r.get(0),
            superuser: r.get(1),
            can_login: r.get(2),
            create_db: r.get(3),
            create_role: r.get(4),
        })
        .collect())
}

pub fn create_user(client: &mut Client, name: &str, password: &str) -> Result<(), String> {
    let stmt = format!("CREATE USER {} WITH LOGIN PASSWORD {}", quote_ident(name), quote_lit(password));
    client.execute(&stmt, &[]).map_err(|e| e.to_string())?;
    Ok(())
}

/// Grants `privileges` (empty = `ALL`) on every table in `db`'s `public`
/// schema to `role`, plus matching default privileges so tables created
/// later automatically get them too. Opens its own connection *to `db`*
/// (see module docs for why) — `cfg` must be the same profile used to
/// reach the server in the first place.
pub fn grant_table_privileges(cfg: &ConnectionWithSecrets, db: &str, role: &str, privileges: &[&str]) -> Result<(), String> {
    let mut client = connect_db(cfg, db)?;
    let priv_list = if privileges.is_empty() { "ALL".to_string() } else { privileges.join(", ") };
    let role_ident = quote_ident(role);

    client
        .execute(&format!("GRANT CONNECT ON DATABASE {} TO {role_ident}", quote_ident(db)), &[])
        .map_err(|e| e.to_string())?;
    client
        .execute(&format!("GRANT USAGE ON SCHEMA public TO {role_ident}"), &[])
        .map_err(|e| e.to_string())?;
    client
        .execute(&format!("GRANT {priv_list} ON ALL TABLES IN SCHEMA public TO {role_ident}"), &[])
        .map_err(|e| e.to_string())?;
    client
        .execute(
            &format!("ALTER DEFAULT PRIVILEGES IN SCHEMA public GRANT {priv_list} ON TABLES TO {role_ident}"),
            &[],
        )
        .map_err(|e| e.to_string())?;
    Ok(())
}

pub fn drop_user(client: &mut Client, name: &str) -> Result<(), String> {
    let stmt = format!("DROP USER {}", quote_ident(name));
    client.execute(&stmt, &[]).map_err(|e| e.to_string())?;
    Ok(())
}

pub fn change_password(client: &mut Client, name: &str, new_password: &str) -> Result<(), String> {
    let stmt = format!("ALTER USER {} WITH PASSWORD {}", quote_ident(name), quote_lit(new_password));
    client.execute(&stmt, &[]).map_err(|e| e.to_string())?;
    Ok(())
}

/// Escapes a Postgres identifier (role/database/table name).
fn quote_ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// Escapes a Postgres string literal, e.g. for `PASSWORD '...'` — this
/// can't be bound as a query parameter since `CREATE`/`ALTER USER` don't
/// accept parameter placeholders in that position.
fn quote_lit(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}
