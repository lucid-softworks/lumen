// bun:sqlite oracle-comparison fixture.
//
// This script runs unmodified under BOTH runtimes and must print identical output:
//
//   bun run bun_sqlite_oracle.js               # the oracle (verified on bun v1.2.21)
//   lumen-cli bun_sqlite_oracle.js             # the implementation under test
//
// `bun_sqlite_oracle.expected.txt` is bun's captured output; the `bun_sqlite_matches_oracle`
// integration test replays this script in lumen and diffs against it. BigInt values are printed
// via typeof/String() (console.log formatting of BigInt differs across runtimes; the *values*
// are asserted identical).
const { Database, Statement, SQLiteError, constants } = require("bun:sqlite");
const log = (...a) => console.log(...a);

// -- CRUD + JS typing of INTEGER/TEXT/REAL/BLOB/NULL --
const db = new Database(":memory:");
db.run("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val REAL, data BLOB, n INTEGER)");
const ins = db.query("INSERT INTO t (name, val, data, n) VALUES ($name, $val, $data, $n)");
log("run:", JSON.stringify(ins.run({ $name: "alice", $val: 3.5, $data: new Uint8Array([1, 2, 3]), $n: 42 })));
const one = db.query("SELECT * FROM t WHERE id=?").get(1);
log("types:", typeof one.id, typeof one.name, typeof one.val, one.data instanceof Uint8Array, Array.from(one.data).join(","), typeof one.n);
log("missing:", db.query("SELECT * FROM t WHERE id=999").get());
log("values:", JSON.stringify(db.query("SELECT id,name FROM t").values()));
log("all:", JSON.stringify(db.query("SELECT id,name FROM t").all()));

// -- error shapes: syntax + constraint --
try { db.run("SELCT bad"); } catch (e) { log("syntax:", e.name, e instanceof SQLiteError, e instanceof Error, e.errno, "|", e.message); }
db.run("CREATE TABLE u (id INTEGER PRIMARY KEY, x NOT NULL)");
db.run("INSERT INTO u VALUES (1, 5)");
try { db.run("INSERT INTO u VALUES (1, 6)"); } catch (e) { log("unique:", e.name, e.code, e.errno, "|", e.message); }
try { db.run("INSERT INTO u (id) VALUES (2)"); } catch (e) { log("notnull:", e.name, e.code, e.errno, "|", e.message); }
try { db.run("INSERT INTO nope VALUES (1)"); } catch (e) { log("notable:", e.name, e.code === undefined, e.errno, "|", e.message); }

// -- transactions: commit, rollback, nested savepoints, variants --
db.run("CREATE TABLE tx (a)");
const t1 = db.transaction((n) => { db.run("INSERT INTO tx VALUES (?)", [n]); return n * 2; });
log("tx ret:", t1(21));
try { db.transaction(() => { db.run("INSERT INTO tx VALUES (99)"); throw new Error("boom"); })(); } catch (e) { log("tx threw:", e.message); }
log("tx rows:", JSON.stringify(db.query("SELECT a FROM tx ORDER BY a").values()));
const inner = db.transaction(() => { db.run("INSERT INTO tx VALUES (10)"); });
const outer = db.transaction(() => {
  db.run("INSERT INTO tx VALUES (20)");
  try { db.transaction(() => { db.run("INSERT INTO tx VALUES (30)"); throw new Error("x"); })(); } catch (e) {}
  inner();
});
outer();
log("nested:", JSON.stringify(db.query("SELECT a FROM tx WHERE a>=10 ORDER BY a").values()));
const tv = db.transaction(() => 7);
log("tx variants:", typeof tv.deferred, typeof tv.immediate, typeof tv.exclusive, tv.immediate());

// -- safeIntegers: per-statement + db-level; bigint round trip --
db.run("CREATE TABLE big (v INTEGER)");
db.run("INSERT INTO big VALUES (9007199254740993)");
log("default big:", db.query("SELECT v FROM big").get().v, typeof db.query("SELECT v FROM big").get().v);
const st = db.query("SELECT v FROM big");
log("safeIntegers chain:", st.safeIntegers(true) === st);
const bv = st.get().v;
log("safe big:", String(bv), typeof bv);
db.query("INSERT INTO big VALUES (?)").run(10n);
log("bigint bound:", db.query("SELECT v FROM big WHERE v=10").get().v);
const sidb = new Database(":memory:", { safeIntegers: true });
sidb.run("CREATE TABLE z (v)");
const rr = sidb.query("INSERT INTO z VALUES (5)").run();
log("db safeInts:", String(rr.lastInsertRowid), typeof rr.lastInsertRowid, typeof sidb.query("SELECT v FROM z").get().v);

// -- binding forms: varargs, array, named ($ : @ and bare), strict mode --
db.run("CREATE TABLE p (a,b,c)");
db.run("INSERT INTO p VALUES (?, ?, ?)", 1, 2, 3);
db.run("INSERT INTO p VALUES (?, ?, ?)", [4, 5, 6]);
db.query("INSERT INTO p VALUES ($a, :b, @c)").run({ $a: 7, ":b": 8, "@c": 9 });
db.query("INSERT INTO p VALUES ($a, :b, @c)").run({ a: 10, b: 11, c: 12 });
log("forms:", JSON.stringify(db.query("SELECT * FROM p ORDER BY a").values()));
const sdb = new Database(":memory:", { strict: true });
sdb.run("CREATE TABLE s (a,b)");
sdb.query("INSERT INTO s VALUES ($a,$b)").run({ a: 10, b: 20 });
log("strict:", JSON.stringify(sdb.query("SELECT * FROM s").get()));
try { sdb.query("INSERT INTO s VALUES ($a,$b)").run({ a: 1 }); } catch (e) { log("strict missing:", e.name, "|", e.message); }
try { sdb.query("INSERT INTO s VALUES ($a,$b)").run({ $a: 1, $b: 2 }); } catch (e) { log("strict prefixed:", e.name, "|", e.message); }

// -- number storage classes (int52 boundary) + exotic binds --
db.run("CREATE TABLE nb (a)");
const qnb = db.query("INSERT INTO nb VALUES (?)");
const chk = db.query("SELECT a, typeof(a) t FROM nb ORDER BY rowid DESC LIMIT 1");
// (labels, not raw values: console rendering of -0 differs across runtimes; storage class is
// what's being asserted)
for (const [label, v] of [["42", 42], ["2^40", 2 ** 40], ["2^50", 2 ** 50], ["2^51", 2 ** 51], ["1.5", 1.5], ["-2^51", -(2 ** 51)], ["-2^51-1", -(2 ** 51) - 1], ["-0", -0]]) {
  qnb.run(v);
  log("bind", label, "->", chk.get().t);
}
try { qnb.run(() => {}); } catch (e) { log("fn bind:", e.name, "|", e.message); }
try { qnb.run(Symbol("x")); } catch (e) { log("symbol bind:", e.name, "|", e.message); }
qnb.run(null); log("null bind:", chk.get().t);
qnb.run(true); log("bool bind:", chk.get().a, chk.get().t);
qnb.run(NaN); log("nan bind:", chk.get().t);

// -- positional count mismatch --
try { db.query("INSERT INTO p VALUES (?, ?, ?)").run(1); } catch (e) { log("mismatch:", e.name, "|", e.message); }
log("0-param ignores args:", JSON.stringify(db.query("SELECT COUNT(*) c FROM p").get(123)));

// -- query cache vs prepare; statement surface --
log("cached:", db.query("SELECT 1") === db.query("SELECT 1"), db.prepare("SELECT 1") === db.prepare("SELECT 1"));
const q8 = db.query("SELECT id AS foo, name FROM t WHERE id=?");
q8.run(1);
log("toString:", q8.toString());
log("columnNames:", JSON.stringify(q8.columnNames), "paramsCount:", q8.paramsCount);

// -- iteration + class mapping --
db.run("CREATE TABLE it2 (id INTEGER, name TEXT)");
db.run("INSERT INTO it2 VALUES (1,'a'),(2,'b'),(3,'c')");
const seen = [];
for (const row of db.query("SELECT * FROM it2")) seen.push(row.id);
log("iterated:", JSON.stringify(seen));
const it = db.query("SELECT * FROM it2").iterate();
log("iterate next:", JSON.stringify(it.next()));
class Row { getName() { return this.name.toUpperCase(); } }
const m = db.query("SELECT * FROM it2 WHERE id=1").as(Row).get();
log("as:", m instanceof Row, m.getName());

// -- run/exec result objects; multi-statement exec --
log("exec:", JSON.stringify(db.exec("INSERT INTO nb VALUES (100); INSERT INTO nb VALUES (101)")));
log("select run changes:", db.query("SELECT * FROM it2").run().changes);

// -- close semantics --
const c = new Database(":memory:");
c.close();
c.close();
try { c.query("SELECT 1"); } catch (e) { log("closed query:", e.constructor.name, "|", e.message); }
try { c.run("SELECT 1"); } catch (e) { log("closed run:", e.constructor.name, "|", e.message); }

// -- finalize --
const f = db.query("SELECT 1 AS x");
f.finalize();
f.finalize();
try { f.get(); } catch (e) { log("finalized:", e.name, "|", e.message); }
log("recreated:", JSON.stringify(db.query("SELECT 1 AS x").get()));

// -- serialize / deserialize --
const buf = db.serialize();
log("serialize nonempty:", buf.length > 0);
log("deserialize:", Database.deserialize(buf).query("SELECT COUNT(*) c FROM it2").get().c);
log("ctor from bytes:", new Database(buf).query("SELECT COUNT(*) c FROM it2").get().c);

// -- SQLiteError construction guard; module surface --
try { new SQLiteError("x"); } catch (e) { log("SQLiteError ctor:", e.message); }
log("surface:", typeof Database.open, Database.MAX_QUERY_CACHE_SIZE, typeof Statement, constants.SQLITE_OPEN_READONLY, constants.SQLITE_FCNTL_RESET_CACHE);
log("done");
