# m6-auth-cli

m6-auth-cli is the administrative command for an m6-auth database.
It creates, changes and deletes users, groups, roles and API (*Application Programming Interface*) tokens, and it can do so before m6-auth-server has started.

This page is the reference for every command, flag, config key and exit code.
Section 9 gives the order for a fresh install, and section 10 names the documents that own the material around the database.

**Contents**

1. [What m6-auth-cli is for](#1-what-m6-auth-cli-is-for)
2. [The command line](#2-the-command-line)
3. [The config file](#3-the-config-file)
4. [Users](#4-users)
5. [Groups](#5-groups)
6. [API tokens](#6-api-tokens)
7. [Flags](#7-flags)
8. [Exit codes](#8-exit-codes)
9. [Bootstrap sequence](#9-bootstrap-sequence)
10. [Where to read next](#10-where-to-read-next)

## 1. What m6-auth-cli is for

m6-auth-cli opens the SQLite file that m6-auth owns and edits its rows directly.
m6-auth-cli exists for two jobs: creating the first administrator before m6-auth-server starts, and managing users, groups and tokens while the site runs.

The direct access is what makes both jobs possible.
m6-auth-cli calls `m6_auth::Db::open`, the same function m6-auth-server calls.
That call creates the database file and its tables when they are absent, so a first install needs no separate schema step.
The same call sets SQLite's *WAL* (write-ahead log) mode and a 5 second busy timeout, so a write here does not block a read by m6-auth-server.
m6-auth-server reads users and groups from SQLite on each login, so a change made here takes effect at the next login and needs no restart or signal.

A second writer is the only case that waits.
A write that cannot take the lock within those 5 seconds fails with a `database is locked` error and exit code 1.
Each invocation opens the database, runs one command and exits.
The lock lasts for that command, and a retry after m6-auth-server's write finishes succeeds.
Section 2 gives the parts of a command.

## 2. The command line

Every command has the same four parts: a config file, an entity, a command and the arguments that command takes.

```
m6-auth-cli <config> <entity> <command> [args] [flags]
```

Table 1 gives the four parts.
Take from it that the first three are required and the last belongs to the command.

| part | meaning |
|---|---|
| `<config>` | the path to the `m6-auth.conf` that names the database and the signing keys |
| `<entity>` | `user`, `group` or `token` |
| `<command>` | the operation on that entity, such as `ls` or `add` |
| `[args] [flags]` | the command's arguments and flags, in that order |

**Table 1: the four parts of an m6-auth-cli command line.** The first three are required for every call.

Arguments are positional and are read from the left, so the name comes before the flags.
A flag written before the name is read as the name, and `user add --role admin` creates a user called `--role`.

`--version` or `-V` anywhere in the arguments prints `m6-auth-cli <version>` and exits 0, before the config file is read.
The argument count is checked next: a call with fewer than three arguments prints the usage block to stderr and exits 2.
An unknown entity or command also exits 2, after the config file is read.

The config file named by the first argument is the subject of section 3.

## 3. The config file

m6-auth-cli reads one TOML file, the same `m6-auth.conf` that m6-auth-server reads.
Table 2 gives the keys m6-auth-cli reads.
Take from it that the user and group commands need only `[storage]`.

| key | required | default | used for |
|---|---|---|---|
| `[storage] path` | yes | none | the SQLite file |
| `[tokens] issuer` | no | `localhost` | the `iss` claim of a minted token |
| `[keys] private_key` | `token create` only | none | signing the token |
| `[keys] public_key` | `token create` only | none | loading the key pair |

**Table 2: the config keys m6-auth-cli reads.** Only `token create` needs the two key entries.

Paths resolve differently for the two groups of keys.
A relative `[storage] path` resolves against the directory that holds the config file, and an absolute path is used as written.
m6-auth-cli creates the directory that holds the database when it is missing.

The `[keys]` paths resolve against the site root.
That root is the parent of the `configs` directory when the config file sits inside one, and the config file's own directory otherwise.
An absolute key path is used as written.
`--site-dir` on `token create` overrides the directory for the private key only, so the public key is always read from the inferred site root (section 7).

Every other key is ignored by m6-auth-cli.
The token lifetimes belong to m6-auth-server, and [`m6-auth.md`](m6-auth.md) owns the full config.
Section 4 starts the commands with users.

## 4. Users

Five commands create, list, change and delete users.
Table 3 gives them.
Take from it that `add` sets the roles at creation and `roles` changes them afterwards.

| command | what it does |
|---|---|
| `user ls [--json]` | lists every user as USERNAME, ROLES and CREATED |
| `user add <username> [--role <role>]... [--password <pw>]` | creates a user and prompts for the password twice |
| `user del <username>` | deletes a user and the rows that belong to them |
| `user passwd <username> [--password <pw>]` | sets a new password and prompts twice |
| `user roles <username> [--set <role>]... [--unset <role>]...` | prints the roles, or adds and removes them |

**Table 3: the five user commands.** `add` is the only command that takes roles at creation, and `passwd` is the only one that changes the password.

`user add` fails with exit 1 when the username is taken, and its message names that user.

Roles are free text.
Neither m6-auth-cli nor the m6-auth library validates a role, so a mistyped name creates a new role silently.
A route's `require = "role:admin"` in `site.toml` is where a role gains meaning, and [`m6-site-toml.md`](m6-site-toml.md) owns that form.

`user roles` reads and changes the list.
With no `--set` or `--unset` it prints the roles, comma separated, and exits 0.
`--set` adds a role that is absent and leaves one that is present in place, and `--unset` removes it.
Both flags may repeat and may be combined in one command.

`user del` removes the user row, and the schema removes the user's memberships, refresh tokens and API token records with it.
Refresh sessions end at once because m6-auth-server checks a refresh token against the database.
A JWT that was already issued keeps working until its `exp`, and section 6 explains why.

`user ls --json` prints one JSON array.
Each object carries `id`, `username`, `roles`, `groups` and `created_at`.

Group commands follow the same pattern, and section 5 gives them.

## 5. Groups

Six commands create groups, delete them and move users in and out.
Table 4 gives them.
Take from it that membership has its own `member` subcommand, and `group ls` counts the members.

| command | what it does |
|---|---|
| `group ls [--json]` | lists every group as GROUP and MEMBERS |
| `group add <name>` | creates a group |
| `group del <name>` | deletes a group and its membership rows |
| `group member ls <group> [--json]` | lists a group's members as USERNAME and ROLES |
| `group member add <group> <username>` | adds a user to a group |
| `group member del <group> <username>` | removes a user from a group |

**Table 4: the six group commands.** `member` is a subcommand of `group`, so it follows the entity and precedes its own command.

Group names are unique, and adding a name that exists fails with exit 1.
A membership row holds the user and the group, so it has no roles of its own: the ROLES column in `group member ls` is each user's global role list.

`group member del` exits 0 when the user is not a member, because the delete matches no row.
A script cannot read exit 0 as proof that a membership existed.

Deleting a group removes its membership rows and leaves the users in place.
`group ls --json` prints objects with `id`, `name` and `members`, where `members` is the list of usernames, and `group member ls --json` prints full user objects.

Section 6 gives the token commands, which read the same database through a different table.

## 6. API tokens

Three commands mint, list and revoke API tokens.
A token is a *JWT* (JSON Web Token) signed with the m6-auth keys, so m6-http checks it locally with the same `require` rules it uses for a session token.

Table 5 gives the three commands.
Take from it that `token create` is the only command that prints a token.

| command | what it does |
|---|---|
| `token create <username> [--name <name>] [--ttl-days <days>] [--site-dir <dir>]` | mints a token and prints it to stdout |
| `token ls <username> [--json]` | lists every token record for the user |
| `token revoke <token-id>` | deletes one token record |

**Table 5: the three API token commands.** Only `create` needs the `[keys]` entries in the config.

`token create` signs a token with the key pair from the config and prints the JWT to stdout.
Its messages go to stderr, so `TOKEN=$(m6-auth-cli ... token create ...)` captures the token alone.
The name defaults to `api-token`.
The *TTL* (time to live) defaults to 365 days.
`--ttl-days` takes a whole number of days, and a value that is not a number exits 1.

The claims in the token carry the user id, username, roles and groups as they stood at that moment.
A later role change does not alter an issued token, because m6-http reads the token and never the database.
The database stores a SHA-256 hash of the token, so the token text cannot be recovered after the command ends.

`token ls` lists every record the user has, ordered by `created_at`, newest first.
The list includes a token whose `exp` has passed.
The table columns are ID, NAME, CREATED and EXPIRES, and `--json` gives the fields `id`, `user_id`, `username`, `name`, `created_at` and `expires_at`.
An empty list prints the header and a note on stderr.

`token revoke` deletes the record, and the JWT keeps working until its `exp` because verification is local.
`--ttl-days 1` limits that window to a day, and the row that `token ls` shows is the only record of the token afterwards.

The sequence from minting a token to guarding a route with it is example 10 in [`m6-user-guide.md`](m6-user-guide.md).
Table 6 in section 7 gives the flags these commands share.

## 7. Flags

Table 6 gives every flag, the commands it changes and what it does.
Take from it that `--json` is the only flag that more than one kind of command accepts.

| flag | applies to | what it does |
|---|---|---|
| `--password <pw>` | `user add`, `user passwd` | supplies the password and skips both prompts |
| `--name <name>` | `token create` | names the token, default `api-token` |
| `--ttl-days <days>` | `token create` | sets the lifetime in whole days, default 365 |
| `--site-dir <dir>` | `token create` | reads the private key from `<dir>`, by file name |
| `--json` | `user ls`, `group ls`, `group member ls`, `token ls` | prints JSON in place of the table |
| `--version`, `-V` | any call | prints the version and exits 0 |

**Table 6: every flag, the commands it applies to and what it changes.** A flag that a command does not use is accepted and ignored.

`--password` puts the password in the process arguments, where the shell history and `ps` show it, and it removes the confirmation prompt.
Use it in a script whose history and process list are both controlled, and use the prompt everywhere else.

`--ttl-days` accepts an unsigned whole number, and zero is valid: it mints a token that has already expired.
The value is read as days and multiplied by 86400 seconds.

`--site-dir` changes the directory the private key is read from.
The public key is still read from the site root inferred from the config path.
The flag suits a private key that sits in a different directory from the public key.

`--json` is meaningful on the four listing commands, and every other command ignores it.

Section 8 gives the exit codes these commands return.

## 8. Exit codes

Every run ends with 0, 1 or 2, and the number tells a script whether any command ran.
Table 7 gives the three.
Take from it that 2 means no command ran.

| code | meaning | examples |
|---|---|---|
| `0` | the command succeeded | a user was created, a list was printed |
| `1` | the command ran and failed | user not found, duplicate name, password mismatch, database locked, key pair unreadable |
| `2` | the command line or config could not be used | fewer than three arguments, unknown entity or command, config missing or unparsable |

**Table 7: the three exit codes.** Code 1 means a matched command failed, and code 2 means no command ran.

A matched command that is missing its argument exits 1, because the check runs inside the command.
`user add` with no name is the example, and `group member` with no subcommand exits 2 because that check runs before a command is matched.

Section 9 puts the commands in the order a first install needs them.

## 9. Bootstrap sequence

A first install needs a signing key pair and one administrator user before m6-auth-server starts.
The block below creates both against a site whose config sits in `configs/`, and the comments give the reason for each step.

```sh
# An elliptic curve P-256 signing key pair.
# m6-auth reads the PKCS8 form ("BEGIN PRIVATE KEY") and fails on the SEC1
# form ("BEGIN EC PRIVATE KEY"), so convert the generated key.
mkdir -p keys
openssl ecparam -name prime256v1 -genkey -noout -out keys/auth-sec1.pem
openssl pkcs8 -topk8 -nocrypt -in keys/auth-sec1.pem -out keys/auth.pem
openssl ec -in keys/auth-sec1.pem -pubout -out keys/auth.pub
chmod 600 keys/auth.pem
rm keys/auth-sec1.pem

# The first open creates the database file and its tables.
m6-auth-cli configs/m6-auth.conf user add admin --role admin

# Optional: a group for the application's protected routes.
m6-auth-cli configs/m6-auth.conf group add editors
m6-auth-cli configs/m6-auth.conf group member add editors admin
```

`user add` prompts for the password twice, so the block runs interactively.
A script can pass `--password` instead, with the warning in section 7.

Start m6-auth-server after the database exists.
[`m6-auth.md`](m6-auth.md) owns the key paths and the rest of the m6-auth-server config.

Example 04 in [`m6-user-guide.md`](m6-user-guide.md) wraps the same steps in a setup script.
Its key commands write the SEC1 form, so add the `openssl pkcs8` step above.
Section 10 names the documents that own the rest.

## 10. Where to read next

This page covers the commands and stops at the edge of the database.
Table 8 gives the documents that own the material around it.
Take from it that every fact left out here has a named owner.

| document | owns |
|---|---|
| [`m6-auth.md`](m6-auth.md) | the m6-auth-server config, the token lifetimes and key rotation |
| [`m6-auth-lib.md`](m6-auth-lib.md) | the library API, and the argument for more than one process on one SQLite file |
| [`m6-user-guide.md`](m6-user-guide.md) | example 04's setup script and example 10, the API token walkthrough |

**Table 8: the documents that own what this page leaves out.** Start with the user guide for a working site, and use this page for the command in front of you.

Each command reports its result on stdout or stderr, and section 8 tells a script which exit code to expect.
That is the whole interface: the config file, the commands, the flags and the three exit codes.
