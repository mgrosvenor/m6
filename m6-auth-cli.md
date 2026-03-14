# m6-auth-cli

Administrative CLI for m6-auth. Manages users and groups by operating directly on the SQLite database. Works whether m6-auth is running or not — required for initial setup before the server starts.

---

## CLI

```
m6-auth-cli <config> <entity> <command> [args] [flags]
```

`<config>` — path to `configs/m6-auth.conf`. The CLI reads the database path from this file. No other config keys are used.

---

## Commands

### Users

```
m6-auth-cli <config> user ls
```
List all users. Output: table of `username`, `roles`, `created_at`.

```
m6-auth-cli <config> user add <username> [--role <role>]...
```
Create a user. Prompts for password interactively (hidden input). `--role` may be repeated. Common roles: `admin`, `user`. Roles are arbitrary strings — m6-auth-cli does not validate them.

```
m6-auth-cli <config> user del <username>
```
Delete a user and remove them from all groups. Revokes all their active refresh tokens.

```
m6-auth-cli <config> user passwd <username>
```
Set a new password. Prompts interactively.

```
m6-auth-cli <config> user roles <username> [--set <role>]... [--unset <role>]...
```
Add or remove roles. `--set` and `--unset` may be repeated and combined in one command.

### Groups

```
m6-auth-cli <config> group ls
```
List all groups with member count.

```
m6-auth-cli <config> group add <name>
```
Create a group.

```
m6-auth-cli <config> group del <name>
```
Delete a group and remove all memberships.

```
m6-auth-cli <config> group member ls <group>
```
List members of a group.

```
m6-auth-cli <config> group member add <group> <username>
```
Add a user to a group.

```
m6-auth-cli <config> group member del <group> <username>
```
Remove a user from a group.

---

## Flags

| Flag | Applies to | Notes |
|---|---|---|
| `--password <pw>` | `user add`, `user passwd` | Supply password non-interactively. Appears in shell history — use only in scripts where history is disabled. |
| `--json` | all `ls` commands | Output JSON instead of table. |

---

## Concurrent access

m6-auth-cli opens the SQLite database in WAL mode with a short busy timeout (5 seconds). If m6-auth is running and holds a write lock, the CLI waits briefly then exits 1 with a clear error rather than corrupting data.

m6-auth does not cache user or group data in memory — it queries SQLite on every login. Changes made by the CLI take effect on the next login attempt with no server restart or signal required.

---

## Exit codes

| Code | Meaning |
|---|---|
| `0` | Success |
| `1` | Runtime error (user not found, database locked, etc.) |
| `2` | Usage error (bad arguments, config not found) |

---

## Bootstrap workflow

Before starting m6-auth for the first time:

```bash
# Generate signing keys
mkdir -p keys
openssl ecparam -name prime256v1 -genkey -noout -out keys/auth.pem
openssl ec -in keys/auth.pem -pubout -out keys/auth.pub
chmod 600 keys/auth.pem

# Create admin user — database created automatically if absent
m6-auth-cli configs/m6-auth.conf user add admin --role admin

# Optionally create application groups
m6-auth-cli configs/m6-auth.conf group add editors
m6-auth-cli configs/m6-auth.conf group member add editors admin

# Start the server
systemctl start m6-auth
```

`setup.sh` in examples wraps this sequence.

---

## Cargo.toml

```toml
[package]
name    = "m6-auth-cli"
version = "0.1.0"
edition = "2021"

[[bin]]
name = "m6-auth-cli"

[dependencies]
m6-auth     = { path = "../m6-auth" }
toml        = "0.8"
anyhow      = "1"
rpassword   = "7"       # hidden password prompt
serde_json  = "1"
```
