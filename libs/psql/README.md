# `sako:psql`

PostgreSQL, spoken by a Rust client built into the runtime. Nothing to install:
[`crates/sako-postgres`](../../crates/sako-postgres) implements the v3 wire
protocol -- including SCRAM-SHA-256 -- and this library is the surface over it.

```ts
import { connect } from "sako:psql";

const db = await connect("postgres://sako@localhost/app");

const users = await db.query`select id, email from users where team = ${team}`;
for (const user of users) console.log(user.id, user.email);

await db.close();
```

## Parameters are never string-pasted

A tagged template turns every interpolation into a bound parameter:

```ts
await db.query`select * from users where email = ${email}`;
// select * from users where email = $1, with email bound
```

The value reaches the server as a parameter, so there is nothing for it to
escape out of. The extended protocol this uses also rejects several statements
in one call, which closes the other half of that door: a parameter cannot
smuggle a `; drop table` in behind the statement it was bound to.

The function form is the same thing written out:

```ts
await db.query("select * from users where email = $1", [email]);
```

## What comes back

`query` resolves to an array of row objects, with the result's own details
attached as non-enumerable properties:

```ts
const rows = await db.query`insert into notes (body) values (${body}) returning id`;
rows[0].id;      // 7
rows.count;      // 1        -- rows the command reported
rows.command;    // "INSERT 0 1"
rows.columns;    // [{ name: "id", type: 23 }]
```

| API | What it does |
| --- | --- |
| `db.query` | Runs one statement, as a template or with explicit parameters. |
| `db.first` | The first row, or `undefined`. |
| `db.transaction(body)` | `begin`, run `body`, `commit` -- or `rollback` if it throws. |
| `db.parameter(name)` | A server parameter, e.g. `server_version`. |
| `db.close()` | Ends the session. |

### Types

Values arrive as the text PostgreSQL itself prints, and the library converts
the types it can do faithfully:

| PostgreSQL | JavaScript |
| --- | --- |
| `bool` | `boolean` |
| `int2`, `int4`, `float4`, `float8` | `number` |
| `int8` | `number`, or `bigint` when it will not fit exactly |
| `json`, `jsonb` | the parsed value |
| `bytea` | `Uint8Array` |
| `numeric` | `string` |
| everything else | `string` |

`numeric` stays text on purpose: it is arbitrary precision, and a JavaScript
number cannot hold every value one can. Dates, times, arrays, and every other
type arrive as the server's text with the column's type OID in `rows.columns`,
so a caller that wants more can decode it without this library guessing.

## What this does not do yet

- **It blocks.** A query occupies the event loop until the server answers, the
  same way `fetch` does. A `sako:http` server sharing the process serves nobody
  while a query is in flight. This is the first thing to fix, and it means
  giving the driver to the IOCP reactor.
- **No TLS.** A URL asking for `sslmode=require` is refused rather than quietly
  connecting in the clear, so a managed database that requires TLS -- most of
  them -- is out of reach for now.
- **No pool.** One connection per `connect`.
- **No `LISTEN`/`NOTIFY`, no `COPY`, no prepared statement reuse, no cursors.**
  A result is read into memory whole, bounded at one million rows.
- **One statement per call**, which is a consequence of the extended protocol
  and also the reason parameters are safe.

## Types in an editor

The source is the types. Point TypeScript at it:

```json
{
  "compilerOptions": {
    "paths": { "sako:psql": ["./libs/psql/src/index.ts"] }
  }
}
```
