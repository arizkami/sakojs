// Sako.SQL against a live server (SAKO_PG_TEST_URL); see tests/fixtures.rs.
const sql = new Sako.SQL(Sako.env.SAKO_PG_TEST_URL, { max: 4 });
const [row] = await sql`select ${42}::int4 as n, ${"it's"}::text as s, ${true} as b, ${3.5}::float8 as f,
  ${9007199254740993n}::int8 as big, ${{ a: [1, 2] }}::jsonb as j, ${new Uint8Array([104, 105])}::bytea as bin,
  ${new Date(Date.UTC(2020, 0, 2, 3, 4, 5))}::timestamptz as ts, ${[1, null, 3]}::int4[] as arr, null as nothing`;
console.log(row);
console.log(row.bin instanceof Uint8Array, row.ts instanceof Date, row.ts.toISOString());

await sql`drop table if exists people`;
await sql`create table people (id serial primary key, name text unique not null, age int4, tags text[])`;
const inserted = await sql`insert into people ${sql([{ name: "ada", age: 36, tags: ["math"] }, { name: "alan", age: 41, tags: [] }])} returning id, name`;
console.log(inserted, inserted.count, inserted.command);
const updated = await sql`update people set ${sql({ age: 37 })} where name = ${"ada"}`;
console.log("updated", updated.count, updated.command);
const cols = ["name", "age"];
console.log(await sql`select ${sql(cols)} from ${sql("people")} where id in ${sql([1, 2])} order by id`);
console.log(await sql`select name, age from people order by id`.values());
const byAge = (min) => sql`age >= ${min}`;
console.log(await sql`select name from people where ${byAge(40)}`);
console.log(await sql`select * from ${sql.unsafe("people")} where name = ${"alan"}`.then((r) => r.length));
console.log((await sql.unsafe("select 1 as a; select 2 as b"))[0]);
console.log((await sql.unsafe("select $1::int4 + $2::int4 as sum", [2, 3]))[0]);

try { await sql`insert into people (name) values (${"ada"})`; }
catch (e) { console.log(e instanceof Sako.SQL.PostgresError, e.name, e.code, e.constraint, e.detail); }
try { await sql`selec 1`; } catch (e) { console.log(e.code, e.message, e.position); }

const total = await sql.begin(async (tx) => {
  await tx`insert into people (name, age) values (${"grace"}, ${85})`;
  const [{ count }] = await tx`select count(*)::int4 from people`;
  return count;
});
console.log("committed", total, (await sql`select count(*)::int4 as c from people`)[0].c);
try {
  await sql.begin(async (tx) => {
    await tx`insert into people (name) values (${"temp"})`;
    throw new Error("abort!");
  });
} catch (e) { console.log("rolled back:", e.message, (await sql`select count(*)::int4 as c from people where name = 'temp'`)[0].c); }


const many = await Promise.all(Array.from({ length: 200 }, (_, i) => sql`select ${i}::int4 as i`));
console.log("200 concurrent:", many.every((r, i) => r[0].i === i));
await sql`drop table people`;
await sql.end();
try { await sql`select 1`; } catch (e) { console.log("after end:", e.code); }
