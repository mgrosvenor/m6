# m6-auth-cli

m6-auth-cli is the administrative command for an m6-auth database.
It creates, changes and deletes users, groups, roles and API (*Application Programming Interface*) tokens, and it can do so before m6-auth-server has started.
It also runs the session step of a key rotation.

This page is the reference for every command, flag, config key and exit code.
Section 9 gives the order for a fresh install.

**Contents**

1. [What m6-auth-cli is for](#1-what-m6-auth-cli-is-for)
2. [The command line](#2-the-command-line)
3. [The config file](#3-the-config-file)
4. [Users](#4-users)
5. [Groups](#5-groups)
6. [API tokens](#6-api-tokens)
7. [Sessions](#7-sessions)
8. [Exit codes](#8-exit-codes)
9. [Bootstrap sequence](#9-bootstrap-sequence)

## 1. What m6-auth-cli is for

m6-auth-cli opens the SQLite file that m6-auth owns and edits its rows directly.
It has two jobs: creating the first administrator before m6-auth-server starts, and managing users, groups and tokens while the site runs.

m6-auth-cli calls `m6_auth::Db::open`, the same function m6-auth-server calls.
That call creates the database file and its tables when they are absent, so a first install needs no separate schema step.
It also sets a 5 second busy timeout.
A write that cannot take the lock within that timeout fails with a `database is locked` error and exit code 1.
Each invocation opens the database, runs one command and exits, so a retry after the other writer finishes succeeds.
The Concurrent access section of [`m6-auth-lib.md`](m6-auth-lib.md) gives the cross-process rules.

## 2. The command line

Every call has the shape of the usage line below.

```
m6-auth-cli <config> <entity> <command> [args] [flags]
```

Table 1 gives the four parts.
Take from it that the entity selects which commands are available.

| part | meaning |
|---|---|
| `<config>` | the path to the `m6-auth.conf` that names the database and the signing keys |
| `<entity>` | `user`, `group`, `token` or `sessions` |
| `<command>` | the operation on that entity, such as `ls` or `add` |
| `[args] [flags]` | the command's arguments and flags, in that order |

**Table 1: the four parts of an m6-auth-cli command line.**
The first three are required, and `[args] [flags]` belongs to the command.

Arguments are positional and are read from the left, so the name comes before the flags.
`user add --role admin` creates a user called `--role`.

`--version` or `-V` anywhere in the arguments prints the tag form, `v1.16.0-rc.1`, and exits 0, before the config file is read.
`--version-full` prints one `<crate> - <full SemVer>` line per m6 crate in the binary, starting with `m6-auth-cli`, and exits 0 at the same point.
A call with fewer than three arguments after the program name prints the usage block to stderr and exits 2.
An unknown entity or command also exits 2, after the config file is read and the database is opened.

`--json` on an `ls` command prints JSON in place of the table.

## 3. The config file

m6-auth-cli reads one TOML file, the same `m6-auth.conf` that m6-auth-server reads.
Table 2 gives the keys it reads.
Take from it that the user and group commands need `[storage]` and `[tokens] issuer`, and only `token create` needs `[keys]`.

| key | required | default | used for |
|---|---|---|---|
| `[storage] path` | yes | none | the SQLite file |
| `[tokens] issuer` | yes | none | the `iss` claim of a minted token. This tool reads no `site.toml`, so it exits 2 when the config omits it |
| `[tokens] audience` | no | the issuer | the `aud` claim of a minted token |
| `[keys] private_key` | `token create` only | none | signing the token |
| `[keys] public_key` | `token create` only | none | loading the key pair |

**Table 2: the config keys m6-auth-cli reads.**
Without `[keys]`, `token create` exits 1 with `config missing [keys] section`.

Every path in the file is absolute, and a relative one exits 2 naming the setting.
m6-auth-cli creates the directory that holds the database when it is missing.
Run it as the service's user, for example `sudo -u m6`, so every file it creates beside the database stays the service's.
`token create` reads the private key the config names, so with a key delivered by `LoadCredential` it runs while m6-auth-server runs.

Every other key is ignored.
[`m6-auth.md`](m6-auth.md) owns the full config and the token lifetimes.

## 4. Users

Five commands create, list, change and delete users.
Table 3 gives them.
Take from it that `add` sets the roles at creation and `roles` reads and changes them afterwards.

| command | what it does |
|---|---|
| `user ls [--json]` | lists every user as USERNAME, ROLES and CREATED |
| `user add <username> [--role <role>]... [--password <pw>]` | creates a user and prompts for the password twice |
| `user del <username>` | deletes a user and the rows that belong to them |
| `user passwd <username> [--password <pw>]` | sets a new password and prompts twice |
| `user roles <username> [--set <role>]... [--unset <role>]...` | prints the roles, or adds and removes them |

**Table 3: the five user commands and what each one does.**

`user add` fails with exit 1 when the username is taken, and its message names that user.

`user add` and `user passwd` take `--password`, which supplies the password and skips both prompts.
It puts the password in the process arguments, where the shell history and `ps` show it, so use it in a script whose history and process list are both controlled and use the prompt everywhere else.

Roles are free text.
Neither m6-auth-cli nor the m6-auth library validates a role, so a mistyped name creates a new role silently.
A route's `require = "role:admin"` in `site.toml` is where a role gains meaning, and [`m6-site-toml.md`](m6-site-toml.md) owns that form.

`user roles` with no `--set` or `--unset` prints the roles, comma separated, and exits 0.
`--set` adds a role that is absent and leaves one that is present in place, and `--unset` removes it.
Both flags may repeat and may be combined in one command.

`user del` removes the user row, and the schema removes the user's memberships, refresh tokens and API token records with it.
m6-auth-server answers `POST /check` 401 for a deleted user, so a JWT issued to that user is refused at once, before its `exp`.

`user ls --json` prints one JSON array.
Each object carries `id`, `username`, `roles`, `groups` and `created_at`.

## 5. Groups

Six commands create groups, delete them and move users in and out.
Table 4 gives them.
Take from it that `group ls` counts the members, and only `group member add` and `group member del` take a user.

| command | what it does |
|---|---|
| `group ls [--json]` | lists every group as GROUP and MEMBERS |
| `group add <name>` | creates a group |
| `group del <name>` | deletes a group and its membership rows |
| `group member ls <group> [--json]` | lists a group's members as USERNAME and ROLES |
| `group member add <group> <username>` | adds a user to a group |
| `group member del <group> <username>` | removes a user from a group |

**Table 4: the six group commands.**
`member` is a subcommand of `group`, so it follows the entity and precedes its own command.

Group names are unique, and adding a name that exists fails with exit 1.
A membership row holds the user and the group, so the ROLES column in `group member ls` is each user's global role list.

`group member del` exits 0 when the user is not a member, because the delete matches no row.
A script cannot read exit 0 as proof that a membership existed.

Deleting a group removes its membership rows and leaves the users in place.
`group ls --json` prints objects with `id`, `name` and `members`, where `members` is the list of usernames, and `group member ls --json` prints full user objects.

## 6. API tokens

Three commands mint, list and revoke API tokens.
A token is a *JWT* (JSON Web Token) signed with the m6-auth keys.
m6-http asks the check service about it with the same `require` rules it uses for a session token.

Table 5 gives the three commands.
Take from it that `token create` is the only command that prints a token.

| command | what it does |
|---|---|
| `token create <username> [--name <name>] [--ttl-days <days>]` | mints a token and prints it to stdout |
| `token ls <username> [--json]` | lists every token record for the user |
| `token revoke <token-id>` | deletes one token record |

**Table 5: the three API token commands.**
Only `create` needs the `[keys]` entries in the config.

`token create` signs a token with the key pair from the config and prints the JWT to stdout.
Its messages go to stderr, so `TOKEN=$(m6-auth-cli ... token create ...)` captures the token alone.
The name defaults to `api-token`.
The *TTL* (time to live) defaults to 365 days.
`--ttl-days` takes a whole number of days, and any other value exits 1.
`--ttl-days 0` sets `exp` to the second of minting, so the token is refused from the next second, because m6-auth-server allows zero leeway past `exp`.

The claims carry the user id, username, roles and groups as they stood at that moment, and the configured issuer and audience.
`POST /check` answers the database's current roles and groups, so a later role change takes effect without a new token.
The token's `jti` is a fresh identifier for this mint and names no row.
The database stores a SHA-256 hash of the token, so the token text cannot be recovered after the command ends.

`token ls` lists every record the user has, newest `created_at` first, including a token whose `exp` has passed.
The table columns are ID, NAME, CREATED and EXPIRES, and `--json` gives the fields `id`, `user_id`, `username`, `name`, `created_at` and `expires_at`.
An empty list prints the header and a note on stderr.

`token revoke` deletes the record, and the JWT keeps working until its `exp`, because `POST /check` verifies the token's signature, kind, issuer, audience and expiry and never reads the token record.
`--ttl-days 1` limits that window to a day.
To end a token at once, delete its user with `user del`.

The sequence from minting a token to guarding a route with it is example 10 in [`m6-user-guide.md`](m6-user-guide.md).

## 7. Sessions

`sessions revoke-all` deletes every refresh-token row in one transaction.
It takes no argument and reports the number of rows it deleted.

The command is the second step of a key rotation, after the new key files are written and before the server restarts.
It ends every refresh token, so a stolen one cannot renew access.
A login after the command creates a new row.
[`m6-auth.md` section 4.2](m6-auth.md#42-key-rotation) gives the three steps of the procedure.

## 8. Exit codes

Every run ends with 0, 1 or 2.
Table 6 gives the three.
Take from it that code 2 is the only code that guarantees no command ran.

| code | meaning | examples |
|---|---|---|
| `0` | the call succeeded | a command finished, or `--version` or `--version-full` printed before dispatch |
| `1` | the call failed after the config file was read | user not found, duplicate name, password mismatch, database locked, key pair unreadable, database that cannot be created or opened |
| `2` | no command ran | fewer than three arguments, unknown entity or command, config file missing or unparsable |

**Table 6: the three exit codes, their meanings and examples.**

`--version` and `--version-full` return 0 before the config file or the database is read, so code 0 does not prove that a command ran.
A database that cannot be created or opened fails with code 1 before an entity or command is matched, so code 1 does not prove that a command ran either.
An unknown entity or command exits 2 after the database is opened, so a mistyped command can still create the database file and its directory.

A matched command that is missing its argument exits 1, because the check runs inside the command: `user add` with no name is the example.
`group member` with no subcommand exits 2, because that check runs before a command is matched.

## 9. Bootstrap sequence

A first install needs a signing key pair and one administrator user before m6-auth-server starts.
Generate the key pair as [`m6-auth.md` section 4.3](m6-auth.md#43-key-generation) describes, which also covers the PKCS#8 conversion.
The block below then creates the administrator against a site whose config sits in `configs/`.

```sh
# The first open creates the database file and its tables.
m6-auth-cli configs/m6-auth.conf user add admin --role admin

# Optional: a group for the application's protected routes.
m6-auth-cli configs/m6-auth.conf group add editors
m6-auth-cli configs/m6-auth.conf group member add editors admin
```

`user add` prompts for the password twice, so the block runs interactively.
A script can pass `--password` instead, with the warning in section 4.
Start m6-auth-server after the database exists.
Example 04 in [`m6-user-guide.md`](m6-user-guide.md) wraps the same steps in a setup script.
