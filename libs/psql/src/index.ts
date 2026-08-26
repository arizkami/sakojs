// SPDX-License-Identifier: BSD-3-Clause

/**
 * `sako:psql` -- PostgreSQL, spoken by a Rust client built into the runtime.
 *
 * No driver to install and no wire protocol in JavaScript: `crates/sako-postgres`
 * implements the v3 protocol, including SCRAM-SHA-256, and this is the surface
 * over it.
 *
 * ```ts
 * import { connect } from "sako:psql";
 *
 * const db = await connect("postgres://sako@localhost/app");
 * const users = await db.query`select id, email from users where id = ${id}`;
 * await db.close();
 * ```
 *
 * A tagged template is the recommended form: every interpolation becomes a
 * bound parameter, so a value can never become part of the statement. The
 * function form takes the same parameters explicitly.
 *
 * **This blocks.** A query occupies the event loop until the server answers,
 * the same way `fetch` does. It is enough to build with and wrong to serve
 * production traffic with; see the README.
 */

declare const __sakoPostgresConnect: (url: string) => number;
declare const __sakoPostgresQuery: (
  id: number,
  sql: string,
  parameters: (string | null)[],
) => NativeResult;
declare const __sakoPostgresParameter: (id: number, name: string) => string | null;
declare const __sakoPostgresClose: (id: number) => void;

/** What the bridge hands back: column names, type OIDs, and rows of text. */
interface NativeResult {
  names: string[];
  types: number[];
  rows: (string | null)[][];
  command: string;
  affected: number;
}

/** A value a parameter may take. Anything else is rejected rather than guessed at. */
export type Parameter =
  | string
  | number
  | bigint
  | boolean
  | Date
  | Uint8Array
  | null
  | undefined;

/** One column of a result. */
export interface ResultColumn {
  readonly name: string;
  /** The PostgreSQL type OID, for a caller that wants to decode text itself. */
  readonly type: number;
}

/** What a statement produced. It is an array of rows, with the rest attached. */
export interface Result<Row = Record<string, unknown>> extends Array<Row> {
  readonly columns: ResultColumn[];
  /** The command tag, such as `SELECT 3` or `INSERT 0 1`. */
  readonly command: string;
  /** Rows the command reported affecting, which is not always `length`. */
  readonly count: number;
}

export interface Database {
  /** Runs one statement, as a tagged template or with explicit parameters. */
  query<Row = Record<string, unknown>>(
    strings: TemplateStringsArray,
    ...values: Parameter[]
  ): Promise<Result<Row>>;
  query<Row = Record<string, unknown>>(
    sql: string,
    parameters?: Parameter[],
  ): Promise<Result<Row>>;
  /** The first row, or undefined. */
  first<Row = Record<string, unknown>>(
    strings: TemplateStringsArray,
    ...values: Parameter[]
  ): Promise<Row | undefined>;
  first<Row = Record<string, unknown>>(
    sql: string,
    parameters?: Parameter[],
  ): Promise<Row | undefined>;
  /** Runs `body` inside a transaction, committing it or rolling it back. */
  transaction<T>(body: (db: Database) => Promise<T>): Promise<T>;
  /** A server parameter, such as `server_version`. */
  parameter(name: string): string | null;
  /** Ends the session. Every later query fails. */
  close(): Promise<void>;
  readonly closed: boolean;
}

/**
 * PostgreSQL type OIDs this library converts. Everything else stays the text
 * the server sent, which is always a faithful representation of the value --
 * guessing at a type this does not know would not be.
 */
const OID = {
  BOOL: 16,
  BYTEA: 17,
  INT8: 20,
  INT2: 21,
  INT4: 23,
  FLOAT4: 700,
  FLOAT8: 701,
  JSON: 114,
  JSONB: 3802,
  NUMERIC: 1700,
} as const;

/** How far an int8 can go before a JavaScript number stops being exact. */
const SAFE = BigInt(Number.MAX_SAFE_INTEGER);

const decodeHex = (text: string): Uint8Array => {
  // The text format for bytea is \x followed by hex digits.
  const digits = text.startsWith("\\x") ? text.slice(2) : text;
  const bytes = new Uint8Array(digits.length >> 1);
  for (let index = 0; index < bytes.length; index += 1) {
    bytes[index] = Number.parseInt(digits.substr(index * 2, 2), 16);
  }
  return bytes;
};

const decode = (value: string | null, type: number): unknown => {
  if (value === null) return null;
  switch (type) {
    case OID.BOOL:
      return value === "t";
    case OID.INT2:
    case OID.INT4:
    case OID.FLOAT4:
    case OID.FLOAT8:
      return Number(value);
    case OID.INT8: {
      // An int8 that fits stays a number, because that is what callers expect
      // of a count; one that does not becomes a bigint rather than silently
      // losing its low digits.
      const wide = BigInt(value);
      return wide <= SAFE && wide >= -SAFE ? Number(wide) : wide;
    }
    case OID.JSON:
    case OID.JSONB:
      return JSON.parse(value);
    case OID.BYTEA:
      return decodeHex(value);
    // NUMERIC is deliberately left as text: it is arbitrary precision, and a
    // JavaScript number cannot hold every value one can.
    case OID.NUMERIC:
    default:
      return value;
  }
};

const encode = (value: Parameter): string | null => {
  if (value === null || value === undefined) return null;
  if (typeof value === "string") return value;
  if (typeof value === "number" || typeof value === "bigint") return String(value);
  if (typeof value === "boolean") return value ? "t" : "f";
  if (value instanceof Date) return value.toISOString();
  if (value instanceof Uint8Array) {
    let hex = "\\x";
    for (const byte of value) hex += byte.toString(16).padStart(2, "0");
    return hex;
  }
  throw new TypeError(
    "a query parameter must be a string, number, bigint, boolean, Date, Uint8Array, or null",
  );
};

/**
 * Turns a tagged template into a statement and its parameters.
 *
 * `select * from t where id = ${id}` becomes `select * from t where id = $1`
 * with `id` bound. The value never reaches the parser, so there is nothing for
 * it to escape out of.
 */
const fromTemplate = (
  strings: TemplateStringsArray,
  values: Parameter[],
): [string, Parameter[]] => {
  let sql = strings[0];
  for (let index = 0; index < values.length; index += 1) {
    sql += `$${index + 1}${strings[index + 1]}`;
  }
  return [sql, values];
};

const isTemplate = (value: unknown): value is TemplateStringsArray =>
  Array.isArray(value) && Array.isArray((value as TemplateStringsArray).raw);

const toResult = <Row>(native: NativeResult): Result<Row> => {
  const columns: ResultColumn[] = native.names.map((name, index) => ({
    name,
    type: native.types[index],
  }));
  const rows = native.rows.map((values) => {
    const row: Record<string, unknown> = {};
    for (let index = 0; index < columns.length; index += 1) {
      row[columns[index].name] = decode(values[index], columns[index].type);
    }
    return row as Row;
  }) as Result<Row>;
  Object.defineProperties(rows, {
    columns: { value: columns, enumerable: false },
    command: { value: native.command, enumerable: false },
    count: { value: native.affected, enumerable: false },
  });
  return rows;
};

/**
 * Opens a connection.
 *
 * The URL is the ordinary one: `postgres://user:password@host:port/database`.
 * TLS is not implemented yet, so a URL asking for `sslmode=require` is refused
 * rather than quietly connecting in the clear.
 */
export async function connect(url: string): Promise<Database> {
  const id = __sakoPostgresConnect(String(url));
  let closed = false;

  const run = async <Row>(
    first: TemplateStringsArray | string,
    rest: Parameter[],
  ): Promise<Result<Row>> => {
    if (closed) throw new Error("the connection is closed");
    const [sql, values] = isTemplate(first)
      ? fromTemplate(first, rest)
      : [String(first), (rest[0] as Parameter[] | undefined) ?? []];
    return toResult<Row>(__sakoPostgresQuery(id, sql, values.map(encode)));
  };

  const database: Database = {
    query: (<Row>(first: TemplateStringsArray | string, ...rest: Parameter[]) =>
      run<Row>(first, rest)) as Database["query"],
    first: (<Row>(first: TemplateStringsArray | string, ...rest: Parameter[]) =>
      run<Row>(first, rest).then((rows) => rows[0])) as Database["first"],
    async transaction<T>(body: (db: Database) => Promise<T>): Promise<T> {
      await run("begin", []);
      try {
        const value = await body(database);
        await run("commit", []);
        return value;
      } catch (error) {
        // A rollback that itself fails must not replace the reason the
        // transaction was being rolled back.
        try {
          await run("rollback", []);
        } catch {
          // The connection is likely gone; the original error is the useful one.
        }
        throw error;
      }
    },
    parameter: (name: string) => (closed ? null : __sakoPostgresParameter(id, String(name))),
    async close() {
      if (closed) return;
      closed = true;
      __sakoPostgresClose(id);
    },
    get closed() {
      return closed;
    },
  };
  return database;
}

export default { connect };
